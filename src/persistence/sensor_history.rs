// Sensor history time-series store. One row per (epoch, source_id, key)
// triple via the composite PK in M0004. Idempotent inserts via INSERT
// OR IGNORE. Used by the daily ET0 integrator + the merge layer's
// last-seen tracker + dashboard sparkline ranges.

use std::sync::Arc;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;

#[derive(Debug, Error)]
pub enum SensorHistoryError {
    #[error("sqlite: {0}")]
    Sqlite(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reading {
    pub epoch: i64,
    pub source_id: String,
    pub key: String,
    pub value: f64,
}

#[derive(Clone)]
pub struct SensorHistoryStore {
    conn: Arc<Mutex<Connection>>,
}

impl SensorHistoryStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    pub async fn insert(&self, r: Reading) -> Result<(), SensorHistoryError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = c.blocking_lock();
            conn.execute(
                "INSERT OR IGNORE INTO sensor_history(epoch, source_id, key, value)
                 VALUES (?, ?, ?, ?)",
                params![r.epoch, r.source_id, r.key, r.value],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| SensorHistoryError::Sqlite(format!("join: {e}")))?
        .map_err(|e| SensorHistoryError::Sqlite(e.to_string()))
    }

    /// Batch insert. INSERT OR IGNORE on each row; one transaction.
    pub async fn insert_many(&self, rs: Vec<Reading>) -> Result<usize, SensorHistoryError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<usize> {
            let mut conn = c.blocking_lock();
            let mut inserted = 0usize;
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "INSERT OR IGNORE INTO sensor_history(epoch, source_id, key, value)
                     VALUES (?, ?, ?, ?)",
                )?;
                for r in rs {
                    let n = stmt.execute(params![r.epoch, r.source_id, r.key, r.value])?;
                    inserted += n;
                }
            }
            tx.commit()?;
            Ok(inserted)
        })
        .await
        .map_err(|e| SensorHistoryError::Sqlite(format!("join: {e}")))?
        .map_err(|e| SensorHistoryError::Sqlite(e.to_string()))
    }

    /// All readings for a given key in [from, to). Most-recent first.
    pub async fn series(
        &self,
        key: String,
        from_epoch: i64,
        to_epoch: i64,
        limit: usize,
    ) -> Result<Vec<Reading>, SensorHistoryError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<Vec<Reading>> {
            let conn = c.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT epoch, source_id, key, value FROM sensor_history
                 WHERE key = ? AND epoch >= ? AND epoch < ?
                 ORDER BY epoch DESC LIMIT ?",
            )?;
            let rows = stmt
                .query_map(params![key, from_epoch, to_epoch, limit as i64], |r| {
                    Ok(Reading {
                        epoch: r.get(0)?,
                        source_id: r.get(1)?,
                        key: r.get(2)?,
                        value: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| SensorHistoryError::Sqlite(format!("join: {e}")))?
        .map_err(|e| SensorHistoryError::Sqlite(e.to_string()))
    }

    /// Most-recent observation epoch per source_id. Used by /api/health
    /// to report which configured sources have produced data recently
    /// vs. which are stale. Returns None for source_ids that have never
    /// emitted a row.
    pub async fn last_seen_per_source(
        &self,
        source_ids: Vec<String>,
    ) -> Result<std::collections::HashMap<String, i64>, SensorHistoryError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(
            move || -> rusqlite::Result<std::collections::HashMap<String, i64>> {
                let conn = c.blocking_lock();
                let mut out = std::collections::HashMap::new();
                // One prepared statement; iterate ids. Cheap on a few-source
                // setup; SQLite's per-statement overhead is tiny.
                let mut stmt =
                    conn.prepare("SELECT MAX(epoch) FROM sensor_history WHERE source_id = ?")?;
                for id in source_ids {
                    let row: Option<i64> = stmt
                        .query_row(rusqlite::params![&id], |r| r.get(0))
                        .ok()
                        .flatten();
                    if let Some(epoch) = row {
                        out.insert(id, epoch);
                    }
                }
                Ok(out)
            },
        )
        .await
        .map_err(|e| SensorHistoryError::Sqlite(format!("join: {e}")))?
        .map_err(|e| SensorHistoryError::Sqlite(e.to_string()))
    }

    /// The last value for a key from a specific source. Used by the
    /// merge layer to surface "what did Tempest report 30 min ago" when
    /// the live snapshot doesn't have it.
    pub async fn last_value(
        &self,
        source_id: String,
        key: String,
    ) -> Result<Option<Reading>, SensorHistoryError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<Option<Reading>> {
            let conn = c.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT epoch, source_id, key, value FROM sensor_history
                 WHERE source_id = ? AND key = ?
                 ORDER BY epoch DESC LIMIT 1",
            )?;
            let mut rows = stmt.query_map(params![source_id, key], |r| {
                Ok(Reading {
                    epoch: r.get(0)?,
                    source_id: r.get(1)?,
                    key: r.get(2)?,
                    value: r.get(3)?,
                })
            })?;
            match rows.next() {
                Some(Ok(r)) => Ok(Some(r)),
                Some(Err(e)) => Err(e),
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| SensorHistoryError::Sqlite(format!("join: {e}")))?
        .map_err(|e| SensorHistoryError::Sqlite(e.to_string()))
    }

    /// Latest reading for every (source_id, key) whose key looks like a
    /// soil-moisture channel (e.g. Ecowitt `soilmoisture1..8`, or an
    /// `*_soil_moisture` mirror). Powers the zone soil-sensor picker so a
    /// user can assign any local channel to a zone. Ordered by source then
    /// key.
    pub async fn soil_channels(&self) -> Result<Vec<Reading>, SensorHistoryError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<Vec<Reading>> {
            let conn = c.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT epoch, source_id, key, value FROM sensor_history s
                 WHERE (key LIKE 'soilmoisture%' OR key LIKE '%soil_moisture%')
                   AND epoch = (SELECT MAX(epoch) FROM sensor_history
                                WHERE source_id = s.source_id AND key = s.key)
                 GROUP BY source_id, key
                 ORDER BY source_id, key",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(Reading {
                        epoch: r.get(0)?,
                        source_id: r.get(1)?,
                        key: r.get(2)?,
                        value: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| SensorHistoryError::Sqlite(format!("join: {e}")))?
        .map_err(|e| SensorHistoryError::Sqlite(e.to_string()))
    }

    /// Latest value for every distinct key a source has reported, newest
    /// first. Powers the Sensors page "what is this integration actually
    /// reporting right now?" view.
    pub async fn latest_for_source(
        &self,
        source_id: String,
    ) -> Result<Vec<Reading>, SensorHistoryError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<Vec<Reading>> {
            let conn = c.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT epoch, source_id, key, value FROM sensor_history s
                 WHERE source_id = ?1
                   AND epoch = (SELECT MAX(epoch) FROM sensor_history
                                WHERE source_id = ?1 AND key = s.key)
                 GROUP BY key
                 ORDER BY key",
            )?;
            let rows = stmt
                .query_map(params![source_id], |r| {
                    Ok(Reading {
                        epoch: r.get(0)?,
                        source_id: r.get(1)?,
                        key: r.get(2)?,
                        value: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| SensorHistoryError::Sqlite(format!("join: {e}")))?
        .map_err(|e| SensorHistoryError::Sqlite(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::runner;

    async fn fresh_store() -> SensorHistoryStore {
        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        SensorHistoryStore::new(Arc::new(Mutex::new(c)))
    }

    #[tokio::test]
    async fn insert_then_series_roundtrip() {
        let s = fresh_store().await;
        for i in 0..5i64 {
            s.insert(Reading {
                epoch: 1000 + i * 60,
                source_id: "tempest_lan".into(),
                key: "air_temp_c".into(),
                value: 20.0 + (i as f64),
            })
            .await
            .unwrap();
        }
        let series = s
            .series("air_temp_c".into(), 1000, 2000, 100)
            .await
            .unwrap();
        assert_eq!(series.len(), 5);
        // Newest first.
        assert!(series[0].epoch > series[4].epoch);
    }

    #[tokio::test]
    async fn insert_many_in_one_tx() {
        let s = fresh_store().await;
        let rs = (0..50i64)
            .map(|i| Reading {
                epoch: 1000 + i,
                source_id: "s".into(),
                key: "k".into(),
                value: i as f64,
            })
            .collect();
        let n = s.insert_many(rs).await.unwrap();
        assert_eq!(n, 50);
    }

    #[tokio::test]
    async fn duplicate_pk_silently_ignored() {
        let s = fresh_store().await;
        let r = Reading {
            epoch: 1000,
            source_id: "s".into(),
            key: "k".into(),
            value: 1.0,
        };
        s.insert(r.clone()).await.unwrap();
        s.insert(r.clone()).await.unwrap();
        let series = s.series("k".into(), 0, 9999, 10).await.unwrap();
        assert_eq!(series.len(), 1, "pk should dedupe");
    }

    #[tokio::test]
    async fn last_seen_per_source_reports_per_id() {
        let s = fresh_store().await;
        for (src, key, epoch) in [
            ("tempest_lan", "air_temp_c", 1000i64),
            ("tempest_lan", "humidity_pct", 1100),
            ("open_meteo", "et0_today", 950),
            ("ecowitt", "soil_back_yard", 1200),
        ] {
            s.insert(Reading {
                epoch,
                source_id: src.into(),
                key: key.into(),
                value: 0.0,
            })
            .await
            .unwrap();
        }
        let res = s
            .last_seen_per_source(vec![
                "tempest_lan".into(),
                "open_meteo".into(),
                "ecowitt".into(),
                "never_emitted".into(),
            ])
            .await
            .unwrap();
        assert_eq!(
            res.get("tempest_lan"),
            Some(&1100),
            "should pick MAX(epoch) per source"
        );
        assert_eq!(res.get("open_meteo"), Some(&950));
        assert_eq!(res.get("ecowitt"), Some(&1200));
        assert_eq!(
            res.get("never_emitted"),
            None,
            "unseen sources omitted from map"
        );
    }

    #[tokio::test]
    async fn soil_channels_finds_soil_keys_only() {
        let s = fresh_store().await;
        for (src, key, epoch, val) in [
            ("ecowitt", "soilmoisture1", 1000i64, 40.0),
            ("ecowitt", "soilmoisture1", 1100, 42.0), // newer wins
            ("ecowitt", "soilmoisture2", 1050, 55.0),
            ("ecowitt", "tempf", 1100, 70.0), // not soil -> excluded
            ("zigbee", "back_yard_soil_moisture", 900, 31.0),
        ] {
            s.insert(Reading {
                epoch,
                source_id: src.into(),
                key: key.into(),
                value: val,
            })
            .await
            .unwrap();
        }
        let rows = s.soil_channels().await.unwrap();
        // soilmoisture1, soilmoisture2, back_yard_soil_moisture (3 channels).
        assert_eq!(rows.len(), 3, "only soil channels, deduped per key");
        let ch1 = rows
            .iter()
            .find(|r| r.key == "soilmoisture1")
            .expect("ch1 present");
        assert_eq!(ch1.value, 42.0, "newest reading per channel");
        assert!(
            !rows.iter().any(|r| r.key == "tempf"),
            "non-soil keys excluded"
        );
    }

    #[tokio::test]
    async fn last_value_returns_most_recent() {
        let s = fresh_store().await;
        for i in 0..3i64 {
            s.insert(Reading {
                epoch: 1000 + i * 60,
                source_id: "src".into(),
                key: "x".into(),
                value: i as f64 * 10.0,
            })
            .await
            .unwrap();
        }
        let last = s
            .last_value("src".into(), "x".into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(last.value, 20.0);
        assert_eq!(last.epoch, 1120);
    }
}
