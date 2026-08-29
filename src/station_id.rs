// The controller-station shapes, in ONE place.
//
// A zone's `controller_station` is the controller's own id for the valve
// that zone fires, and every controller kind spells its ids differently. The
// shape gate is not cosmetic: it is what stopped issue #8's mis-actuation,
// where a station NUMBER typed into a Rachio zone was sent to the cloud as a
// zone id and every zone failed to start.
//
// Three places have to agree about those shapes:
//
//   * `runtime::build_controllers` decides with them whether a zone entry
//     overrides the controller's own zone map (dispatch),
//   * `config::validate` warns with them about a station value that will
//     never bind (the config check),
//   * the zone editor reads them for the card badge and the in-form line
//     (the UI).
//
// A validator that disagrees with dispatch is worse than no validator, so
// all three call these functions instead of restating the shapes. This
// module is deliberately ungated (no `ssr` / `hydrate` cfg) and depends on
// nothing, so the browser build and the server build share one copy.

/// Parse a zone entry's `controller_station` as a Rachio zone id.
///
/// Rachio addresses zones by UUID and never by station number, and the only
/// place a correct one comes from is the controller's own zone scan (or the
/// wizard's import of that scan). This parser used to be an infallible
/// `Some(station.to_string())`, which left `overlay_zone_entries`' warn-skip
/// arm unreachable for Rachio alone: ANY non-empty station value overwrote
/// the scanned uuid, and dispatch then sent that value to the cloud as a
/// zone id. Because the zone editor's station help text taught station
/// NUMBERS, a user could put `1`..`7` there in good faith and every zone
/// failed to start with an opaque upstream error. Accepting only the uuid
/// shape puts Rachio on the same footing as the other cloud kinds, whose
/// numeric parsers already reject junk and leave the scanned mapping alone.
pub fn rachio_zone_id(station: &str) -> Option<String> {
    let s = station.trim();
    if is_uuid_shaped(s) {
        Some(s.to_string())
    } else {
        None
    }
}

/// Parse a zone entry's `controller_station` as a Hydrawise relay id.
/// Hydrawise addresses zones by a numeric relay id, so unlike Rachio a
/// bare number IS a valid id here and legitimately overrides the map.
pub fn hydrawise_relay_id(station: &str) -> Option<i64> {
    station.trim().parse::<i64>().ok()
}

/// Parse a zone entry's `controller_station` as a station number. B-hyve
/// and Rain Bird both address zones by station index, so as with
/// Hydrawise a bare number is a valid id and overrides the map.
pub fn station_number(station: &str) -> Option<u32> {
    station.trim().parse::<u32>().ok()
}

/// Parse a zone entry's `controller_station` as a Home Assistant entity id.
///
/// HA addresses a valve as `domain.object_id` and never as a station number.
/// The shape gate matters for the same reason Rachio's does: the v0.1
/// upgrade path (`env_compat::seed_legacy_zones`) stamps `"1"`..`"4"` into
/// `controller_station` on an ha_service_call controller, and those numbers
/// were inert only because nothing read the field. Now that the overlay
/// exists they must be warn-skipped, not handed to HA as an entity id.
pub fn ha_entity_id(station: &str) -> Option<String> {
    let s = station.trim();
    let (domain, object_id) = s.split_once('.')?;
    let ok = |part: &str| {
        !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    };
    if ok(domain) && ok(object_id) && !object_id.contains('.') {
        Some(s.to_string())
    } else {
        None
    }
}

/// Canonical 8-4-4-4-12 hyphenated-hex UUID shape. Shape only: no version
/// or variant bits are checked, so a legitimate vendor id is never rejected
/// for being an unusual UUID version.
pub fn is_uuid_shaped(s: &str) -> bool {
    let mut groups = s.split('-');
    for want in [8usize, 4, 4, 4, 12] {
        let Some(g) = groups.next() else {
            return false;
        };
        if g.len() != want || !g.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
    }
    groups.next().is_none()
}

/// Parse a zone entry's `controller_station` as an OpenSprinkler station.
///
/// Stations are 1-BASED, and the guard is a mis-actuation guard rather than
/// a tidiness one: the box's own `sid` is 0-based at the wire, so a stored
/// `0` would alias onto station 1 and water the WRONG zone. Rejecting it
/// leaves the zone unbound, which is loud and harmless.
///
/// The single implementation of that rule. `runtime::build_controllers`
/// derives its zone map with it and `station_is_dispatchable` judges with
/// it, so the config check cannot call a value bindable that dispatch drops.
pub fn opensprinkler_station(station: &str) -> Option<u32> {
    station_number(station).filter(|n| *n >= 1)
}

/// Whether a non-empty `controller_station` is a shape this controller kind
/// can actually dispatch, keyed by the serde kind tag.
///
/// `None` means the kind does not read `controller_station` at all, so there
/// is no shape to judge: `mqtt_command` (its per-zone value is a command
/// struct), `esphome_native` (the adapter is never built), `dry_run` (any
/// slug runs), and any kind this build does not know. `Some(false)` is the
/// case worth reporting: a value is set, it LOOKS like a binding, and the
/// controller will never accept it.
///
/// Callers handle the empty string themselves. An empty station means
/// unbound, which is a different diagnosis with a different remedy.
pub fn station_is_dispatchable(kind: &str, station: &str) -> Option<bool> {
    match kind {
        "rachio" => Some(rachio_zone_id(station).is_some()),
        "hydrawise" => Some(hydrawise_relay_id(station).is_some()),
        // The same function build_controllers derives its map with, so a 0
        // (which aliases onto station 1 at the wire) is judged unbindable
        // here exactly because it is dropped there.
        "opensprinkler_direct" => Some(opensprinkler_station(station).is_some()),
        "bhyve" | "rainbird" => Some(station_number(station).is_some()),
        "ha_service_call" => Some(ha_entity_id(station).is_some()),
        // The DIY board's station is an opaque string it chose itself, so
        // any non-empty value is a shape LocalSky cannot second-guess.
        "http_generic" => Some(!station.trim().is_empty()),
        _ => None,
    }
}

/// What a kind's station value looks like, in the words a person needs in
/// order to fix one. Pairs with [`station_is_dispatchable`]; `None` for the
/// kinds that do not read the field.
pub fn station_expectation(kind: &str) -> Option<&'static str> {
    match kind {
        "rachio" => Some("a zone UUID, which only the controller's own zone list supplies"),
        "hydrawise" => Some("a numeric relay id"),
        "opensprinkler_direct" => Some("a station number counting from 1"),
        "bhyve" | "rainbird" => Some("a station number"),
        "ha_service_call" => Some("an entity id such as switch.back_yard_zone"),
        "http_generic" => Some("the board's own zone id"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "1f00aa00-0000-4000-8000-0000000000a1";

    #[test]
    fn each_kind_accepts_its_own_ids_and_rejects_the_others() {
        // The exact issue #8 shape, and its mirror: a UUID is not a relay id
        // and a relay id is not a UUID.
        assert_eq!(station_is_dispatchable("rachio", UUID), Some(true));
        assert_eq!(station_is_dispatchable("rachio", "3"), Some(false));
        assert_eq!(station_is_dispatchable("hydrawise", "3"), Some(true));
        assert_eq!(station_is_dispatchable("hydrawise", UUID), Some(false));
        assert_eq!(station_is_dispatchable("bhyve", "2"), Some(true));
        assert_eq!(station_is_dispatchable("rainbird", UUID), Some(false));
        assert_eq!(
            station_is_dispatchable("ha_service_call", "switch.back_yard"),
            Some(true)
        );
        assert_eq!(station_is_dispatchable("ha_service_call", "1"), Some(false));
        // OpenSprinkler counts from 1; a 0 aliases onto station 1 at the
        // wire and build_controllers drops it, so it is not dispatchable.
        assert_eq!(
            station_is_dispatchable("opensprinkler_direct", "1"),
            Some(true)
        );
        assert_eq!(
            station_is_dispatchable("opensprinkler_direct", "0"),
            Some(false)
        );
        assert_eq!(
            station_is_dispatchable("opensprinkler_direct", UUID),
            Some(false)
        );
        // A DIY board's ids are its own; any non-empty string is plausible.
        assert_eq!(
            station_is_dispatchable("http_generic", "back_yard"),
            Some(true)
        );
    }

    #[test]
    fn the_kinds_that_ignore_the_station_field_have_no_shape_to_judge() {
        for kind in [
            "mqtt_command",
            "esphome_native",
            "dry_run",
            "",
            "future_kind",
        ] {
            assert_eq!(
                station_is_dispatchable(kind, "anything"),
                None,
                "{kind} does not dispatch on controller_station"
            );
            assert_eq!(station_expectation(kind), None, "{kind}");
        }
    }

    #[test]
    fn every_kind_that_judges_a_shape_can_also_describe_it() {
        // A warning that says a value is wrong without saying what is right
        // is not worth emitting, so the two tables cover the same set.
        for kind in [
            "rachio",
            "hydrawise",
            "opensprinkler_direct",
            "bhyve",
            "rainbird",
            "ha_service_call",
            "http_generic",
        ] {
            assert!(station_is_dispatchable(kind, "x").is_some(), "{kind}");
            assert!(
                station_expectation(kind).is_some_and(|e| !e.is_empty()),
                "{kind} must say what it expects"
            );
        }
    }

    #[test]
    fn whitespace_is_trimmed_the_way_dispatch_trims_it() {
        assert_eq!(hydrawise_relay_id("  7 "), Some(7));
        assert_eq!(station_number(" 2"), Some(2));
        assert_eq!(
            ha_entity_id(" switch.back_yard "),
            Some("switch.back_yard".to_string())
        );
        assert_eq!(station_is_dispatchable("bhyve", "  2  "), Some(true));
    }

    #[test]
    fn uuid_shape_is_shape_only() {
        assert!(is_uuid_shaped(UUID));
        // Any version nibble is accepted: a legitimate vendor id must never
        // be rejected for being an unusual UUID version.
        assert!(is_uuid_shaped("1f00aa00-0000-9000-0000-0000000000a1"));
        assert!(!is_uuid_shaped("1f00aa00-0000-4000-8000-0000000000a"));
        assert!(!is_uuid_shaped("1f00aa00-0000-4000-8000-0000000000a1-x"));
        assert!(!is_uuid_shaped("zzzzzzzz-0000-4000-8000-0000000000a1"));
        assert!(!is_uuid_shaped(""));
    }
}
