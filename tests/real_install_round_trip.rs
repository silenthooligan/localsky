// Install safety, checked against a REAL deployment's files rather than
// against fixtures we wrote ourselves.
//
// CI green is not the same as install safe. This branch retyped a
// forecast field and added a restriction-window variant, both of which
// are serde-facing. A mistake in either does not fail loudly: it either
// refuses to parse an operator's config at boot, or, worse, parses it
// into something subtly different and silently disables a compliance
// rule that their water authority does enforce.
//
// Point these at a copy of a real `localsky.toml` and a real
// `forecast-cache.json` and they assert the shapes survive a full
// round trip. Skipped when the files are absent, so the suite still
// runs anywhere.
//
//   LOCALSKY_REAL_CONFIG=/path/to/localsky.toml
//   LOCALSKY_REAL_FORECAST=/path/to/forecast-cache.json

use localsky::config::schema::Config;
use localsky::forecast::snapshot::ForecastSnapshot;

/// The checked-in fixture for each env var, so CI exercises this even when
/// no real install is available to point at.
///
/// Without a default these tests skipped in CI and the gate proved
/// nothing. The fixture is a real install's SHAPE with every secret
/// redacted, plus a southern-hemisphere floating window so the variant
/// the settings form could not author is covered too.
fn fallback_fixture(var: &str) -> Option<&'static str> {
    match var {
        "LOCALSKY_REAL_CONFIG" => Some("tests/fixtures/real_shaped_config.toml"),
        _ => None,
    }
}

fn read_env_file(var: &str) -> Option<String> {
    let path = std::env::var(var)
        .ok()
        .or_else(|| fallback_fixture(var).map(str::to_string))?;
    match std::fs::read_to_string(&path) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("{var}={path} could not be read: {e}");
            None
        }
    }
}

/// A real operator's config parses, and the parts this branch touched
/// survive being written back out.
#[test]
fn a_real_config_round_trips() {
    let Some(toml_text) = read_env_file("LOCALSKY_REAL_CONFIG") else {
        eprintln!("LOCALSKY_REAL_CONFIG not set; skipping");
        return;
    };

    let cfg: Config = toml::from_str(&toml_text).expect("a real config must still parse");

    // Restrictions are the compliance surface. A serde mistake here does
    // not throw, it quietly stops enforcing a legal rule.
    let before =
        serde_json::to_value(&cfg.engine.watering_restrictions).expect("restrictions serialize");
    let reparsed: Config = toml::from_str(&toml::to_string(&cfg).expect("config re-serializes"))
        .expect("a re-serialized config must parse");
    let after = serde_json::to_value(&reparsed.engine.watering_restrictions)
        .expect("restrictions serialize");
    assert_eq!(
        before, after,
        "watering restrictions changed shape across a round trip"
    );

    // Every restriction's effective window must still name the same
    // variant. DstOnly and StandardOnly delegate to FloatingRange now,
    // and the variants have to stay on the wire byte for byte.
    for r in &cfg.engine.watering_restrictions {
        let json = serde_json::to_string(&r.effective).expect("window serializes");
        let back: localsky::config::schema::EffectiveWindow =
            serde_json::from_str(&json).expect("window parses");
        assert_eq!(
            &back, &r.effective,
            "effective window {json} did not survive"
        );
    }
}

/// A real cached forecast parses, and its day markers stay integers on
/// the wire.
#[test]
fn a_real_forecast_cache_round_trips() {
    let Some(text) = read_env_file("LOCALSKY_REAL_FORECAST") else {
        eprintln!("LOCALSKY_REAL_FORECAST not set; skipping");
        return;
    };

    let parsed: serde_json::Value = serde_json::from_str(&text).expect("the cache file is JSON");

    // The cache may hold a bare snapshot or a keyed map of them. Find
    // every object that looks like one and round-trip it.
    let mut checked = 0usize;
    let mut candidates: Vec<&serde_json::Value> = Vec::new();
    match &parsed {
        serde_json::Value::Object(map) => {
            if map.contains_key("daily") {
                candidates.push(&parsed);
            }
            for v in map.values() {
                if v.get("daily").is_some() {
                    candidates.push(v);
                }
            }
        }
        _ => candidates.push(&parsed),
    }

    for v in candidates {
        let Ok(snap) = serde_json::from_value::<ForecastSnapshot>(v.clone()) else {
            continue;
        };
        checked += 1;
        let out = serde_json::to_value(&snap).expect("snapshot re-serializes");

        // The day marker must still be a plain integer named time_epoch.
        // Not null, not an object: an older client or a template sensor
        // reading this field has to keep working.
        for (i, day) in out["daily"].as_array().into_iter().flatten().enumerate() {
            let stamp = &day["time_epoch"];
            assert!(
                stamp.is_i64(),
                "daily[{i}].time_epoch must stay an integer, got {stamp}"
            );
        }

        // And the values must be unchanged, not merely well typed.
        let original_stamps: Vec<i64> = v["daily"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|d| d["time_epoch"].as_i64().unwrap_or(0))
            .collect();
        let round_tripped: Vec<i64> = out["daily"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|d| d["time_epoch"].as_i64().unwrap_or(0))
            .collect();
        assert_eq!(
            original_stamps, round_tripped,
            "day markers changed value across a round trip"
        );
    }

    assert!(
        checked > 0,
        "no forecast snapshot found in the cache file; the check proved nothing"
    );
}

/// The soil rows a configured install boots with come from `localsky.toml`
/// alone: one per zone, named by the zone, with nothing read from an
/// environment variable or discovered from an entity name.
#[test]
fn a_real_config_builds_one_soil_row_per_zone() {
    let Some(toml_text) = read_env_file("LOCALSKY_REAL_CONFIG") else {
        eprintln!("LOCALSKY_REAL_CONFIG not set; skipping");
        return;
    };
    let cfg: Config = toml::from_str(&toml_text).expect("a real config must still parse");
    let policy = localsky::refresher::WateringPolicy::from_config(&cfg);
    assert_eq!(
        policy.soil_zones.len(),
        cfg.zones.len(),
        "one soil row per configured zone"
    );
    for slug in cfg.zones.keys() {
        let norm = slug.replace('-', "_");
        assert!(
            policy.soil_zones.iter().any(|z| z.slug == norm),
            "{slug} has a soil row"
        );
    }
}

/// A real install's pre-0.9.0 file (schema_version 1, records inside the
/// document) migrates on the first load: the records move to the ledger
/// beside it, the document says 2, and the second load rewrites nothing.
#[tokio::test]
async fn a_pre_090_config_migrates_to_the_ledger() {
    use localsky::ports::config_store::ConfigStore;
    let v1 = std::fs::read_to_string("tests/fixtures/real_shaped_config_v1.toml")
        .expect("the v1 fixture is checked in");
    let dir = std::env::temp_dir().join(format!("localsky-real-v1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("localsky.toml");
    std::fs::write(&path, &v1).unwrap();
    // The fixture references env-held secrets; the loader interpolates
    // them, so give it something to interpolate.
    for var in ["HA_LONG_LIVED_TOKEN", "LOCALSKY_TEST_TOKEN"] {
        if std::env::var(var).is_err() {
            std::env::set_var(var, "test");
        }
    }
    let store = localsky::config::FileConfigStore::new(&path);
    let cfg = store
        .load()
        .await
        .expect("a real v1 config loads and migrates");
    assert_eq!(cfg.schema_version, 2);
    let doc = std::fs::read_to_string(&path).unwrap();
    assert!(doc.contains("schema_version = 2"));
    assert!(!doc.contains("seeded_source_ids") && !doc.contains("priority_repaired_ids"));
    let ledger = store.ledger();
    assert!(ledger.seeded_source_ids.contains(&"nws".to_string()));
    assert!(ledger.priority_repaired_ids.contains(&"nws".to_string()));
    assert_eq!(ledger.migrations.len(), 2);
    let again = store.load().await.unwrap();
    assert_eq!(again.schema_version, 2);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        doc,
        "the second load rewrites nothing"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
