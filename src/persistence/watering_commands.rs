//! Durable command provenance. These rows never supply watering credit:
//! readback/history still owns the evidence that a valve was actually open.

use super::runs::{NewRun, RunsError};
use rusqlite::{params, Connection};
use std::sync::Arc;
use tokio::sync::Mutex;

pub fn new_session_id() -> String {
    format!("run-{:032x}", rand::random::<u128>())
}

#[derive(Clone)]
pub struct WateringCommands {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribution {
    /// None means overlapping/unconfirmed command evidence is ambiguous.
    pub session_id: Option<String>,
    pub cycle_index: Option<u32>,
    pub cycle_count: Option<u32>,
}

impl WateringCommands {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Written before dispatch. A crash or timeout leaves requested provenance,
    /// which cannot be mistaken for a confirmed command or applied water.
    pub async fn request(&self, n: NewRun) -> Result<i64, RunsError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = c.blocking_lock();
            conn.execute(
                "INSERT INTO watering_commands
                (session_id, zone_slug, controller_id, start_epoch, end_epoch,
                 cycle_index, cycle_count, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'requested')",
                params![
                    n.session_id,
                    n.zone_slug,
                    n.controller_id,
                    n.start_epoch,
                    n.start_epoch
                        .saturating_add(i64::from(n.planned_duration_s)),
                    n.cycle_index,
                    n.cycle_count
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
        .map_err(|e| {
            RunsError::Sqlite(Box::new(crate::diagnostics::from_error(
                &e,
                "watering_commands.request",
            )))
        })?
        .map_err(|e: rusqlite::Error| {
            RunsError::Sqlite(Box::new(crate::diagnostics::from_error(
                &e,
                "watering_commands.request",
            )))
        })
    }

    pub async fn finish(&self, id: i64, confirmed: Option<(i64, u32)>) -> Result<(), RunsError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = c.blocking_lock();
            if let Some((start, duration)) = confirmed {
                conn.execute(
                    "UPDATE watering_commands SET state='confirmed', start_epoch=?2,
                    end_epoch=?3 WHERE id=?1 AND state='requested'",
                    params![id, start, start.saturating_add(i64::from(duration))],
                )?;
            } else {
                conn.execute(
                    "UPDATE watering_commands SET state='failed' WHERE id=?1 AND state='requested'",
                    [id],
                )?;
            }
            Ok(())
        })
        .await
        .map_err(|e| {
            RunsError::Sqlite(Box::new(crate::diagnostics::from_error(
                &e,
                "watering_commands.finish",
            )))
        })?
        .map_err(|e: rusqlite::Error| {
            RunsError::Sqlite(Box::new(crate::diagnostics::from_error(
                &e,
                "watering_commands.finish",
            )))
        })
    }

    /// Attribute only a fully contained observation with exactly one matching
    /// confirmed segment. Overlapping jobs are ambiguous, never guessed by gap.
    pub async fn attribution(
        &self,
        zone: &str,
        controller: &str,
        start: i64,
        end: i64,
    ) -> Result<Option<Attribution>, RunsError> {
        let c = self.conn.clone();
        let zone = zone.to_string();
        let controller = controller.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = c.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT session_id, cycle_index, cycle_count, state, start_epoch, end_epoch
                FROM watering_commands WHERE zone_slug=?1 AND controller_id=?2
                  AND state!='failed' AND start_epoch<=?4 AND end_epoch>?3 LIMIT 2",
            )?;
            let rows = stmt
                .query_map(params![zone, controller, start, end], |r| {
                    let contained = r.get::<_, String>(3)? == "confirmed"
                        && r.get::<_, i64>(4)? <= start
                        && r.get::<_, i64>(5)? >= end;
                    Ok(Attribution {
                        session_id: if contained { Some(r.get(0)?) } else { None },
                        cycle_index: if contained { r.get(1)? } else { None },
                        cycle_count: if contained { r.get(2)? } else { None },
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(match rows.len() {
                0 => None,
                1 => rows.into_iter().next(),
                _ => Some(Attribution {
                    session_id: None,
                    cycle_index: None,
                    cycle_count: None,
                }),
            })
        })
        .await
        .map_err(|e| {
            RunsError::Sqlite(Box::new(crate::diagnostics::from_error(
                &e,
                "watering_commands.attribution",
            )))
        })?
        .map_err(|e: rusqlite::Error| {
            RunsError::Sqlite(Box::new(crate::diagnostics::from_error(
                &e,
                "watering_commands.attribution",
            )))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: &str, start: i64) -> NewRun {
        NewRun {
            session_id: Some(id.into()),
            zone_slug: "bed".into(),
            controller_id: "native".into(),
            start_epoch: start,
            planned_duration_s: 60,
            source: "smart_morning".into(),
            skip_reason: None,
            et0_mm: None,
            etc_mm: None,
            cycle_index: Some(0),
            cycle_count: Some(2),
        }
    }

    #[tokio::test]
    async fn provenance_reopens_and_requires_one_confirmed_containing_segment() {
        let path = std::env::temp_dir().join(format!("localsky-commands-{}.db", new_session_id()));
        {
            let mut conn = Connection::open(&path).unwrap();
            crate::persistence::runner::run(&mut conn).unwrap();
            let commands = WateringCommands::new(Arc::new(Mutex::new(conn)));
            let id = commands.request(run("morning", 100)).await.unwrap();
            assert_eq!(
                commands
                    .attribution("bed", "native", 110, 150)
                    .await
                    .unwrap()
                    .unwrap()
                    .session_id,
                None
            );
            commands.finish(id, Some((100, 60))).await.unwrap();
        }
        {
            let commands =
                WateringCommands::new(Arc::new(Mutex::new(Connection::open(&path).unwrap())));
            let found = commands
                .attribution("bed", "native", 110, 150)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(found.session_id.as_deref(), Some("morning"));
            assert_eq!(found.cycle_count, Some(2));
            assert!(commands
                .attribution("other", "native", 110, 150)
                .await
                .unwrap()
                .is_none());
            assert!(commands
                .attribution("bed", "other", 110, 150)
                .await
                .unwrap()
                .is_none());
            assert_eq!(
                commands
                    .attribution("bed", "native", 110, 180)
                    .await
                    .unwrap()
                    .unwrap()
                    .session_id,
                None
            );
            let id = commands.request(run("manual", 130)).await.unwrap();
            commands.finish(id, Some((130, 60))).await.unwrap();
            assert_eq!(
                commands
                    .attribution("bed", "native", 140, 150)
                    .await
                    .unwrap()
                    .unwrap()
                    .session_id,
                None
            );
        }
        std::fs::remove_file(path).unwrap();
    }
}
