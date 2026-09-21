//! Immutable hourly forecast issuances, independent of observed rain history.
use crate::forecast::snapshot::ForecastSnapshot;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

pub const RETENTION_DAYS: i64 = 400;
pub const MAX_PAGE_SIZE: usize = 5000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArchiveRow {
    pub track: String,
    pub provider: String,
    pub model: Option<String>,
    pub target_epoch: i64,
    pub lead_h: u32,
    pub pop_pct: Option<u32>,
    pub precip_in: Option<f64>,
    pub fetched_at: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArchivePage {
    pub rows: Vec<ArchiveRow>,
    pub next_cursor: Option<String>,
}

#[derive(Clone)]
pub struct ForecastArchiveStore {
    conn: Arc<Mutex<Connection>>,
}

pub struct ArchiveQuery {
    pub track: String,
    pub from: i64,
    pub to: i64,
    pub lead_h: Option<u32>,
    pub after: Option<(i64, i64)>,
    pub limit: usize,
}

impl ForecastArchiveStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    pub async fn record(
        &self,
        track: &str,
        model: Option<&str>,
        snapshot: &ForecastSnapshot,
    ) -> anyhow::Result<usize> {
        let fetched_at = snapshot.last_refresh_epoch;
        if fetched_at <= 0 {
            return Ok(0);
        }
        // Canonical rows describe [T,T+1h). Include the current partial hour
        // as lead zero, and at most 48 hours per issuance.
        let hour = fetched_at.div_euclid(3600) * 3600;
        let rows: Vec<ArchiveRow> = snapshot
            .hourly
            .iter()
            .filter(|h| h.time_epoch >= hour && h.time_epoch < hour + 48 * 3600)
            .map(|h| ArchiveRow {
                track: track.into(),
                model: model.map(str::to_owned),
                provider: snapshot.source_label.clone(),
                target_epoch: h.time_epoch,
                lead_h: ((h.time_epoch - hour) / 3600) as u32,
                pop_pct: h.precip_probability.filter(|v| *v <= 100),
                precip_in: h.precip_in.filter(|v| v.is_finite() && *v >= 0.0),
                fetched_at,
            })
            .collect();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<usize> {
            let mut conn = conn.blocking_lock();
            let tx = conn.transaction()?;
            let mut count = 0;
            {
                let mut insert = tx.prepare_cached(
                    "INSERT INTO forecast_hourly_archive
                    (track, provider, model, target_epoch, lead_h, pop_pct, precip_in, fetched_at)
                    VALUES (?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT DO NOTHING",
                )?;
                for row in rows {
                    count += insert.execute(params![
                        row.track,
                        row.provider,
                        row.model,
                        row.target_epoch,
                        row.lead_h,
                        row.pop_pct,
                        row.precip_in,
                        row.fetched_at
                    ])?;
                }
            }
            tx.commit()?;
            Ok(count)
        })
        .await?
        .map_err(Into::into)
    }

    pub async fn query(&self, query: ArchiveQuery) -> anyhow::Result<ArchivePage> {
        crate::forecast::window::validate_range(query.from, query.to, RETENTION_DAYS * 86400)
            .map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            query.limit > 0 && query.limit <= MAX_PAGE_SIZE,
            "invalid archive page size"
        );
        anyhow::ensure!(
            query.lead_h.is_none_or(|lead| lead < 48),
            "lead_h must be between 0 and 47"
        );
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<ArchivePage> {
            let conn = conn.blocking_lock();
            let mut statement = conn.prepare_cached(
                "SELECT track,provider,model,target_epoch,lead_h,pop_pct,precip_in,fetched_at
                FROM forecast_hourly_archive WHERE track=?1 AND target_epoch BETWEEN ?2 AND ?3
                AND (?4 IS NULL OR lead_h=?4)
                AND (target_epoch>?5 OR (target_epoch=?5 AND fetched_at>?6))
                ORDER BY target_epoch,fetched_at LIMIT ?7",
            )?;
            let (after_target, after_fetch) = query.after.unwrap_or((-1, -1));
            let mut rows: Vec<ArchiveRow> = statement
                .query_map(
                    params![
                        query.track,
                        query.from,
                        query.to,
                        query.lead_h,
                        after_target,
                        after_fetch,
                        (query.limit + 1) as i64
                    ],
                    |r| {
                        Ok(ArchiveRow {
                            track: r.get(0)?,
                            provider: r.get(1)?,
                            model: r.get(2)?,
                            target_epoch: r.get(3)?,
                            lead_h: r.get(4)?,
                            pop_pct: r.get(5)?,
                            precip_in: r.get(6)?,
                            fetched_at: r.get(7)?,
                        })
                    },
                )?
                .collect::<rusqlite::Result<_>>()?;
            let more = rows.len() > query.limit;
            rows.truncate(query.limit);
            let next_cursor = more
                .then(|| {
                    rows.last()
                        .map(|r| format!("{}:{}", r.target_epoch, r.fetched_at))
                })
                .flatten();
            Ok(ArchivePage { rows, next_cursor })
        })
        .await?
        .map_err(Into::into)
    }

    pub async fn prune_older_than(&self, cutoff: i64) -> anyhow::Result<usize> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            conn.blocking_lock().execute(
                "DELETE FROM forecast_hourly_archive WHERE target_epoch < ?1",
                [cutoff],
            )
        })
        .await?
        .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn store() -> ForecastArchiveStore {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut conn).unwrap();
        ForecastArchiveStore::new(Arc::new(Mutex::new(conn)))
    }
    fn forecast(at: i64, amount: Option<f64>) -> ForecastSnapshot {
        ForecastSnapshot {
            last_refresh_epoch: at,
            source_label: "NWS".into(),
            hourly: vec![crate::forecast::snapshot::HourlyEntry {
                time_epoch: 7200,
                precip_in: amount,
                ..Default::default()
            }],
            ..Default::default()
        }
    }
    fn query(after: Option<(i64, i64)>) -> ArchiveQuery {
        ArchiveQuery {
            track: "merged".into(),
            from: 0,
            to: 10000,
            lead_h: None,
            after,
            limit: 1,
        }
    }
    #[tokio::test]
    async fn newer_issuance_does_not_rewrite_old_forecast_and_paging_is_lossless() {
        let store = store().await;
        assert_eq!(
            store
                .record("merged", None, &forecast(4000, None))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .record("merged", None, &forecast(4000, Some(0.9)))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .record("merged", None, &forecast(5800, Some(0.3)))
                .await
                .unwrap(),
            1
        );
        let first = store.query(query(None)).await.unwrap();
        assert_eq!(first.rows[0].precip_in, None);
        assert_eq!(first.next_cursor.as_deref(), Some("7200:4000"));
        let second = store.query(query(Some((7200, 4000)))).await.unwrap();
        assert_eq!(second.rows[0].precip_in, Some(0.3));
        assert!(second.next_cursor.is_none());
        assert_eq!(store.prune_older_than(7200).await.unwrap(), 0);
        assert_eq!(store.prune_older_than(7201).await.unwrap(), 2);
    }
}
