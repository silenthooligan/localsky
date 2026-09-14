// Config migrations: ordered, recorded, applied once.
//
// A migration rewrites the raw TOML document (never the parsed Config,
// so `${VAR}` references stay references and no secret is baked into the
// file) and may move records into the ledger. Each runs exactly once per
// install: it is skipped when the ledger records its id, and when the
// document already carries a schema_version at or past its target. The
// store runs this on load and writes both files back when anything
// changed; every later load finds nothing to do.

use crate::config::ledger::Ledger;
use crate::config::schema::CURRENT_SCHEMA_VERSION;

pub struct Migration {
    pub id: &'static str,
    /// The schema_version the document carries once this has run.
    pub target: u32,
    pub apply: fn(&mut toml::Table, &mut Ledger),
}

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        id: "2026-09-ledger-sidecar",
        target: 2,
        apply: ledger_sidecar,
    },
    Migration {
        id: "2026-09-open-meteo-past-days",
        target: 2,
        apply: open_meteo_past_days,
    },
];

/// Apply every migration this document still needs, in order. Returns the
/// ids applied; empty means neither file needs writing.
pub fn migrate(doc: &mut toml::Table, ledger: &mut Ledger, now_epoch: i64) -> Vec<&'static str> {
    let mut applied = Vec::new();
    let version = document_version(doc);
    // A rollback binary cannot interpret a future schema. The file store
    // reports the incompatibility; this pure helper must never downgrade it.
    if version > CURRENT_SCHEMA_VERSION {
        return applied;
    }
    for m in MIGRATIONS {
        if version >= m.target || ledger.has_migration(m.id) {
            continue;
        }
        (m.apply)(doc, ledger);
        ledger.record_migration(m.id, now_epoch);
        applied.push(m.id);
    }
    if version != CURRENT_SCHEMA_VERSION {
        doc.insert(
            "schema_version".into(),
            toml::Value::Integer(i64::from(CURRENT_SCHEMA_VERSION)),
        );
        if applied.is_empty() {
            applied.push("schema-version");
        }
    }
    applied
}

pub fn document_version(doc: &toml::Table) -> u32 {
    doc.get("schema_version")
        .and_then(toml::Value::as_integer)
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(1)
}

/// v1 kept three server-owned records inside the document. Move them to
/// the ledger (unioned, so a ledger that already exists loses nothing)
/// and drop them from the document.
fn ledger_sidecar(doc: &mut toml::Table, ledger: &mut Ledger) {
    if let Some(v) = doc.remove("seeded_source_ids") {
        if let Ok(ids) = v.try_into::<Vec<String>>() {
            ledger.absorb_seeded(ids);
        }
    }
    if let Some(v) = doc.remove("priority_repaired_ids") {
        if let Ok(ids) = v.try_into::<Vec<String>>() {
            for id in ids {
                if !ledger.priority_repaired_ids.contains(&id) {
                    ledger.priority_repaired_ids.push(id);
                }
            }
        }
    }
    if let Some(v) = doc.remove("ha_adoption") {
        if let Ok(recs) = v.try_into::<Vec<crate::model::HaAdoptedHelper>>() {
            for r in recs {
                if !ledger.ha_adoption.iter().any(|h| h.entity == r.entity) {
                    ledger.ha_adoption.push(r);
                }
            }
        }
    }
}

/// Open-Meteo `past_days == 1` rewrites to 3. Before 1.21.0 the fetch
/// hardcoded 3 past days and ignored the field, while the old default and
/// both UI templates stamped an explicit 1 into nearly every persisted
/// config; honoring 1 as written would drop those installs to a one-day
/// archive. A stored 1 was never a value anyone chose against observed
/// behavior. Once, at migration: a 1 typed after this ran is the owner's.
fn open_meteo_past_days(doc: &mut toml::Table, _ledger: &mut Ledger) {
    let Some(sources) = doc.get_mut("sources").and_then(toml::Value::as_array_mut) else {
        return;
    };
    for src in sources.iter_mut() {
        let Some(t) = src.as_table_mut() else {
            continue;
        };
        if t.get("kind").and_then(toml::Value::as_str) != Some("open_meteo") {
            continue;
        }
        let Some(c) = t.get_mut("config").and_then(toml::Value::as_table_mut) else {
            continue;
        };
        if c.get("past_days").and_then(toml::Value::as_integer) == Some(1) {
            c.insert("past_days".into(), toml::Value::Integer(3));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_never_downgrades_a_future_document() {
        let mut doc = toml::Table::new();
        doc.insert(
            "schema_version".into(),
            toml::Value::Integer(i64::from(CURRENT_SCHEMA_VERSION + 1)),
        );
        let original = doc.clone();
        let mut ledger = Ledger::default();
        assert!(migrate(&mut doc, &mut ledger, 100).is_empty());
        assert_eq!(doc, original);
        assert_eq!(ledger, Ledger::default());
    }

    const V1: &str = r#"
schema_version = 1
seeded_source_ids = ["nws"]
priority_repaired_ids = ["open_meteo"]

[deployment.location]
lat = 29.65
lon = -82.32

[[sources]]
id = "open_meteo"
kind = "open_meteo"
[sources.config]
past_days = 1

[[ha_adoption]]
entity = "input_boolean.irrigation_pause"
outcome = "adopted"
target = "irrigation_control.is_paused"
epoch = 5
"#;

    #[test]
    fn a_v1_document_moves_its_records_out_and_bumps_once() {
        let mut doc: toml::Table = toml::from_str(V1).unwrap();
        let mut ledger = Ledger::default();
        let applied = migrate(&mut doc, &mut ledger, 100);
        assert_eq!(
            applied,
            vec!["2026-09-ledger-sidecar", "2026-09-open-meteo-past-days"]
        );
        assert_eq!(document_version(&doc), CURRENT_SCHEMA_VERSION);
        for gone in ["seeded_source_ids", "priority_repaired_ids", "ha_adoption"] {
            assert!(!doc.contains_key(gone), "{gone} left in the document");
        }
        assert_eq!(ledger.seeded_source_ids, vec!["nws"]);
        assert_eq!(ledger.priority_repaired_ids, vec!["open_meteo"]);
        assert_eq!(ledger.ha_adoption.len(), 1);
        assert_eq!(ledger.migrations.len(), 2);
        let past = doc["sources"][0]["config"]["past_days"].as_integer();
        assert_eq!(past, Some(3));

        // Second pass: nothing to do, nothing changes.
        let before = doc.clone();
        assert!(migrate(&mut doc, &mut ledger, 200).is_empty());
        assert_eq!(doc, before);
    }

    /// A ledger that already records a migration keeps a later document
    /// edit alone, even if the document forgot its version.
    #[test]
    fn a_recorded_migration_never_runs_twice() {
        let mut doc: toml::Table = toml::from_str(V1).unwrap();
        let mut ledger = Ledger::default();
        ledger.record_migration("2026-09-open-meteo-past-days", 1);
        migrate(&mut doc, &mut ledger, 100);
        assert_eq!(
            doc["sources"][0]["config"]["past_days"].as_integer(),
            Some(1),
            "the owner's 1 stands"
        );
    }

    #[test]
    fn the_current_version_is_the_last_target() {
        assert_eq!(
            MIGRATIONS.iter().map(|m| m.target).max(),
            Some(CURRENT_SCHEMA_VERSION)
        );
        let mut ids: Vec<&str> = MIGRATIONS.iter().map(|m| m.id).collect();
        ids.dedup();
        assert_eq!(ids.len(), MIGRATIONS.len());
    }
}
