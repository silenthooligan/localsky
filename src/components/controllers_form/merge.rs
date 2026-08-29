// Pure scan-result merge for the controller editor's "Scan zones" button.
//
// Issue #8: the editor's scan used to format the discovered zones into a
// status message and DISCARD them, while the surrounding copy claimed the
// zone map in the JSON had been filled. This module is the merge that was
// missing: given the controller kind, the draft config JSON, and the scan
// results, it writes the kind's zone-map key in place and reports what it
// did so the status message can tell the truth.
//
// Pure JSON-in/JSON-out on purpose: the same function runs in the browser
// (hydrate, called from `ControllerEditorPanel::on_scan`) and under the
// native test target, so the regression test issue #8 lacked runs in the
// ordinary `cargo test --lib` gate with no browser involved.

use serde::{Deserialize, Serialize};

/// One zone/station reported by POST /api/v1/wizard/scan_zones. Client-side
/// mirror of `ports::irrigation_controller::DiscoveredZone` (that module is
/// ssr-only); the `discovered_zone_wire_shape_matches_ports` test pins the
/// field parity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredZone {
    pub station_id: String,
    pub name: String,
}

/// slug for a zone imported from a controller scan: lowercase, runs of
/// non-alphanumerics collapse to single underscores. Shared by the setup
/// wizard's zone import and the editor's scan merge so both paths bind the
/// same name to the same slug (dispatch resolves zone slugs with the same
/// underscore normalization).
pub fn zone_slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_us = true;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_us = false;
        } else if !last_us {
            out.push('_');
            last_us = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        "zone".into()
    } else {
        out
    }
}

/// Zones from ONE scan whose names collapse onto the same slug. Their
/// mappings are all written (suffixed keys, the wizard's rule) but the
/// user must verify which controller-native id belongs to which zone,
/// so the merge message surfaces each group.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeCollision {
    /// The slug the colliding names collapse to.
    pub base_slug: String,
    /// The zone names, in scan order.
    pub names: Vec<String>,
    /// The map keys written for them, in the same order.
    pub slugs: Vec<String>,
}

/// What `merge_scanned_zones` did to the config JSON.
#[derive(Debug, Clone, PartialEq)]
pub enum MergeOutcome {
    /// Scan results were written into the config's `map_key` object:
    /// `added` new slugs, `updated` slugs the PRE-EXISTING map already
    /// held (a rescan refreshing an entry). `skipped` lists zones whose
    /// station id failed the kind's numeric parse (relay/station maps
    /// hold numbers); those are left out rather than written corrupt.
    /// `collisions` lists same-scan duplicate names (see MergeCollision).
    Merged {
        map_key: &'static str,
        added: usize,
        updated: usize,
        skipped: Vec<String>,
        collisions: Vec<MergeCollision>,
    },
    /// This kind holds no zone map in its controller config: its zones bind
    /// through zone entries (controller_station on the Zones page / setup
    /// wizard import). The config is left untouched.
    NoMapKind,
}

/// Merge scanned zones into the controller-config JSON for `kind`.
///
/// Map keys per kind:
/// - `rachio` -> `zone_uuid_map` (slug -> zone uuid, String)
/// - `hydrawise` -> `zone_relay_map` (slug -> relay id, i64)
/// - `bhyve` / `rainbird` -> `zone_station_map` (slug -> station number, u32)
/// - everything else -> `NoMapKind` (zones bind via zone entries)
///
/// Merge semantics: an existing entry for the same slug is updated; entries
/// for slugs the scan did not report are preserved, so a hand-edit survives
/// a rescan. Two zones in the SAME scan that collapse to one slug (duplicate
/// names, or all-symbol names that fall back to "zone") never overwrite each
/// other: the later one gets underscore-suffixed keys (the wizard's import
/// rule) and the group is reported in `collisions` for the user to verify.
/// A `map_key` that exists but is not a JSON object (a typo'd scalar) is
/// replaced by a fresh map.
pub fn merge_scanned_zones(
    kind: &str,
    config: &mut serde_json::Value,
    zones: &[DiscoveredZone],
) -> MergeOutcome {
    let map_key: &'static str = match kind {
        "rachio" => "zone_uuid_map",
        "hydrawise" => "zone_relay_map",
        "bhyve" | "rainbird" => "zone_station_map",
        _ => return MergeOutcome::NoMapKind,
    };
    if !config.is_object() {
        *config = serde_json::json!({});
    }
    let Some(obj) = config.as_object_mut() else {
        // Unreachable after the object coercion above; stay defensive.
        return MergeOutcome::NoMapKind;
    };
    let mut map = match obj.get(map_key) {
        Some(serde_json::Value::Object(m)) => m.clone(),
        _ => serde_json::Map::new(),
    };
    let preexisting: std::collections::BTreeSet<String> = map.keys().cloned().collect();
    let mut written: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // base slug -> (names, written slugs) in scan order, to surface
    // same-scan collisions.
    let mut by_base: std::collections::BTreeMap<String, (Vec<String>, Vec<String>)> =
        std::collections::BTreeMap::new();
    let mut added = 0usize;
    let mut updated = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    for z in zones {
        let value = match kind {
            "rachio" => serde_json::Value::String(z.station_id.clone()),
            "hydrawise" => match z.station_id.trim().parse::<i64>() {
                Ok(n) => serde_json::Value::from(n),
                Err(_) => {
                    skipped.push(format!("{} (station \"{}\")", z.name, z.station_id));
                    continue;
                }
            },
            // bhyve | rainbird (the match above admits nothing else here)
            _ => match z.station_id.trim().parse::<u32>() {
                Ok(n) => serde_json::Value::from(n),
                Err(_) => {
                    skipped.push(format!("{} (station \"{}\")", z.name, z.station_id));
                    continue;
                }
            },
        };
        let base = zone_slug(&z.name);
        let mut slug = base.clone();
        if written.contains(&slug) {
            // Same-scan duplicate: never overwrite the earlier zone's
            // mapping. Suffix underscores (the wizard's import rule) until
            // the key is free of both this scan's writes and pre-existing
            // entries, so no binding is silently lost or clobbered.
            while written.contains(&slug) || map.contains_key(&slug) {
                slug.push('_');
            }
        }
        if preexisting.contains(&slug) {
            updated += 1;
        } else {
            added += 1;
        }
        map.insert(slug.clone(), value);
        written.insert(slug.clone());
        let group = by_base.entry(base).or_default();
        group.0.push(z.name.clone());
        group.1.push(slug);
    }
    let collisions: Vec<MergeCollision> = by_base
        .into_iter()
        .filter(|(_, (names, _))| names.len() > 1)
        .map(|(base_slug, (names, slugs))| MergeCollision {
            base_slug,
            names,
            slugs,
        })
        .collect();
    obj.insert(map_key.to_string(), serde_json::Value::Object(map));
    MergeOutcome::Merged {
        map_key,
        added,
        updated,
        skipped,
        collisions,
    }
}

/// The status message the editor shows after a successful scan, describing
/// what ACTUALLY happened to the config JSON.
pub fn merge_message(found: usize, outcome: &MergeOutcome) -> String {
    let plural = if found == 1 { "" } else { "s" };
    match outcome {
        MergeOutcome::Merged {
            map_key,
            skipped,
            collisions,
            ..
        } => {
            let mut msg =
                format!("Found {found} zone{plural} and filled {map_key}. Review and save.");
            if !skipped.is_empty() {
                msg.push_str(&format!(
                    " Skipped (station id not numeric): {}.",
                    skipped.join(", ")
                ));
            }
            for c in collisions {
                msg.push_str(&format!(
                    " {} zones share the slug \"{}\" ({}); wrote {}. Verify which id belongs to which zone before saving.",
                    c.names.len(),
                    c.base_slug,
                    c.names.join(", "),
                    c.slugs.join(", ")
                ));
            }
            msg
        }
        MergeOutcome::NoMapKind => format!(
            "Found {found} zone{plural}. This controller binds zones in the Zones page, not here."
        ),
    }
}

#[cfg(all(test, feature = "ssr"))]
mod tests {
    use super::*;
    use serde_json::json;

    fn zones(pairs: &[(&str, &str)]) -> Vec<DiscoveredZone> {
        pairs
            .iter()
            .map(|(station_id, name)| DiscoveredZone {
                station_id: station_id.to_string(),
                name: name.to_string(),
            })
            .collect()
    }

    // The client-side DiscoveredZone must decode exactly what the ssr-side
    // ports type serializes onto the wire ({station_id, name}).
    #[test]
    fn discovered_zone_wire_shape_matches_ports() {
        let wire = serde_json::to_value(crate::ports::irrigation_controller::DiscoveredZone {
            station_id: "1f00aa00-0000-4000-8000-000000000001".into(),
            name: "Front Lawn".into(),
        })
        .unwrap();
        let mirrored: DiscoveredZone = serde_json::from_value(wire).unwrap();
        assert_eq!(mirrored.station_id, "1f00aa00-0000-4000-8000-000000000001");
        assert_eq!(mirrored.name, "Front Lawn");
    }

    #[test]
    fn zone_slug_matches_wizard_rules() {
        assert_eq!(zone_slug("Front Lawn"), "front_lawn");
        assert_eq!(zone_slug("  Side -- Beds  "), "side_beds");
        assert_eq!(zone_slug("Zone 7"), "zone_7");
        assert_eq!(zone_slug("!!!"), "zone");
    }

    #[test]
    fn rachio_fills_zone_uuid_map_with_string_uuids() {
        let mut cfg = json!({ "api_token": "t", "device_id": "d", "zone_uuid_map": {} });
        let zs = zones(&[
            ("1f00aa00-0000-4000-8000-000000000001", "Front Lawn"),
            ("1f00aa00-0000-4000-8000-000000000002", "Back Lawn"),
        ]);
        let out = merge_scanned_zones("rachio", &mut cfg, &zs);
        assert_eq!(
            out,
            MergeOutcome::Merged {
                map_key: "zone_uuid_map",
                added: 2,
                updated: 0,
                skipped: vec![],
                collisions: vec![],
            }
        );
        assert_eq!(
            cfg["zone_uuid_map"]["front_lawn"],
            "1f00aa00-0000-4000-8000-000000000001"
        );
        assert_eq!(
            cfg["zone_uuid_map"]["back_lawn"],
            "1f00aa00-0000-4000-8000-000000000002"
        );
    }

    #[test]
    fn hydrawise_fills_zone_relay_map_with_numbers() {
        let mut cfg = json!({ "api_key": "k", "controller_id": 7 });
        let out = merge_scanned_zones("hydrawise", &mut cfg, &zones(&[("5551234", "Beds")]));
        assert_eq!(
            out,
            MergeOutcome::Merged {
                map_key: "zone_relay_map",
                added: 1,
                updated: 0,
                skipped: vec![],
                collisions: vec![],
            }
        );
        // A NUMBER, not a string: HydrawiseConfig.zone_relay_map is
        // BTreeMap<String, i64> and a string here would fail the typed PUT.
        assert_eq!(cfg["zone_relay_map"]["beds"], json!(5551234));
    }

    #[test]
    fn bhyve_and_rainbird_fill_zone_station_map_and_skip_unparseable() {
        for kind in ["bhyve", "rainbird"] {
            let mut cfg = json!({});
            let zs = zones(&[("1", "Front"), ("not-a-number", "Broken Station")]);
            let out = merge_scanned_zones(kind, &mut cfg, &zs);
            match out {
                MergeOutcome::Merged {
                    map_key,
                    added,
                    updated,
                    skipped,
                    collisions,
                } => {
                    assert_eq!(map_key, "zone_station_map");
                    assert_eq!(added, 1);
                    assert_eq!(updated, 0);
                    assert_eq!(skipped, vec!["Broken Station (station \"not-a-number\")"]);
                    assert!(collisions.is_empty());
                }
                other => panic!("expected Merged for {kind}, got {other:?}"),
            }
            assert_eq!(cfg["zone_station_map"]["front"], json!(1));
            assert!(cfg["zone_station_map"].get("broken_station").is_none());
        }
    }

    #[test]
    fn zone_entry_bound_kinds_are_no_map_kinds_and_config_is_untouched() {
        for kind in [
            "opensprinkler_direct",
            "http_generic",
            "mqtt_command",
            "ha_service_call",
            "dry_run",
        ] {
            let mut cfg = json!({ "host": "192.0.2.10" });
            let before = cfg.clone();
            let out = merge_scanned_zones(kind, &mut cfg, &zones(&[("1", "Front")]));
            assert_eq!(out, MergeOutcome::NoMapKind, "kind {kind}");
            assert_eq!(cfg, before, "NoMapKind must not touch the config ({kind})");
        }
    }

    #[test]
    fn rescan_updates_existing_slug_and_preserves_hand_edits() {
        // A hand-added mapping for a zone the scan does not report survives;
        // a slug the scan reports again is updated in place.
        let mut cfg = json!({
            "zone_uuid_map": {
                "front_lawn": "old-uuid",
                "hand_edited": "keep-me",
            }
        });
        let out = merge_scanned_zones(
            "rachio",
            &mut cfg,
            &zones(&[("new-uuid", "Front Lawn"), ("side-uuid", "Side Beds")]),
        );
        assert_eq!(
            out,
            MergeOutcome::Merged {
                map_key: "zone_uuid_map",
                added: 1,
                updated: 1,
                skipped: vec![],
                collisions: vec![],
            }
        );
        assert_eq!(cfg["zone_uuid_map"]["front_lawn"], "new-uuid");
        assert_eq!(cfg["zone_uuid_map"]["hand_edited"], "keep-me");
        assert_eq!(cfg["zone_uuid_map"]["side_beds"], "side-uuid");
    }

    #[test]
    fn non_object_map_key_is_replaced_with_a_fresh_map() {
        let mut cfg = json!({ "zone_uuid_map": "oops" });
        let out = merge_scanned_zones("rachio", &mut cfg, &zones(&[("u1", "Front")]));
        assert!(matches!(out, MergeOutcome::Merged { added: 1, .. }));
        assert_eq!(cfg["zone_uuid_map"]["front"], "u1");
    }

    #[test]
    fn merge_message_states_what_happened() {
        let merged = MergeOutcome::Merged {
            map_key: "zone_uuid_map",
            added: 7,
            updated: 0,
            skipped: vec![],
            collisions: vec![],
        };
        assert_eq!(
            merge_message(7, &merged),
            "Found 7 zones and filled zone_uuid_map. Review and save."
        );
        assert_eq!(
            merge_message(3, &MergeOutcome::NoMapKind),
            "Found 3 zones. This controller binds zones in the Zones page, not here."
        );
    }

    // Two zones with the SAME name in one scan must both keep a binding:
    // the later one gets the wizard's underscore suffix, both are counted
    // added, and the collision is reported so the user verifies which
    // uuid belongs to which physical zone.
    #[test]
    fn same_scan_duplicate_names_decollide_and_are_reported() {
        let mut cfg = json!({ "zone_uuid_map": {} });
        let zs = zones(&[("uuid-a", "Side Yard"), ("uuid-b", "Side Yard")]);
        let out = merge_scanned_zones("rachio", &mut cfg, &zs);
        assert_eq!(cfg["zone_uuid_map"]["side_yard"], "uuid-a");
        assert_eq!(cfg["zone_uuid_map"]["side_yard_"], "uuid-b");
        match &out {
            MergeOutcome::Merged {
                added,
                updated,
                collisions,
                ..
            } => {
                assert_eq!(*added, 2, "both bindings are new");
                assert_eq!(*updated, 0);
                assert_eq!(collisions.len(), 1);
                assert_eq!(collisions[0].base_slug, "side_yard");
                assert_eq!(collisions[0].slugs, vec!["side_yard", "side_yard_"]);
            }
            other => panic!("expected Merged, got {other:?}"),
        }
        let msg = merge_message(2, &out);
        assert!(
            msg.contains("2 zones share the slug \"side_yard\""),
            "message must surface the collision: {msg}"
        );
        assert!(msg.contains("side_yard, side_yard_"));
    }

    // All-symbol names all collapse to the fallback slug "zone"; every
    // one of them must still land on its own key.
    #[test]
    fn all_symbol_fallback_slugs_decollide() {
        let mut cfg = json!({});
        let zs = zones(&[("uuid-1", "!!!"), ("uuid-2", "###"), ("uuid-3", "???")]);
        let out = merge_scanned_zones("rachio", &mut cfg, &zs);
        assert_eq!(cfg["zone_uuid_map"]["zone"], "uuid-1");
        assert_eq!(cfg["zone_uuid_map"]["zone_"], "uuid-2");
        assert_eq!(cfg["zone_uuid_map"]["zone__"], "uuid-3");
        match &out {
            MergeOutcome::Merged {
                added, collisions, ..
            } => {
                assert_eq!(*added, 3);
                assert_eq!(collisions.len(), 1);
                assert_eq!(collisions[0].base_slug, "zone");
                assert_eq!(collisions[0].names, vec!["!!!", "###", "???"]);
            }
            other => panic!("expected Merged, got {other:?}"),
        }
    }

    // A duplicate whose suffixed key would hit a PRE-EXISTING map entry
    // skips past it (never clobbers a hand-edit), and `updated` counts
    // only slugs the pre-existing map held.
    #[test]
    fn decollision_never_clobbers_a_preexisting_suffixed_entry() {
        let mut cfg = json!({ "zone_uuid_map": { "side_yard_": "hand-edit" } });
        let zs = zones(&[("uuid-a", "Side Yard"), ("uuid-b", "Side Yard")]);
        let out = merge_scanned_zones("rachio", &mut cfg, &zs);
        assert_eq!(cfg["zone_uuid_map"]["side_yard"], "uuid-a");
        assert_eq!(cfg["zone_uuid_map"]["side_yard_"], "hand-edit");
        assert_eq!(cfg["zone_uuid_map"]["side_yard__"], "uuid-b");
        match out {
            MergeOutcome::Merged { added, updated, .. } => {
                assert_eq!(added, 2);
                assert_eq!(updated, 0, "the hand-edit was preserved, not updated");
            }
            other => panic!("expected Merged, got {other:?}"),
        }
    }
}
