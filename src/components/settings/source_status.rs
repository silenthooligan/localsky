//! Shared presentation of the health API's source states.

pub struct SourceStatus {
    pub label: &'static str,
    pub tone: &'static str,
    pub meaning: &'static str,
}

pub fn presentation(status: &str, enabled: bool, unlocated: bool) -> SourceStatus {
    let (label, tone, meaning) = if !enabled {
        ("Off", "waiting", "Disabled in Settings")
    } else if unlocated {
        (
            "Needs location",
            "waiting",
            "Set your location to connect this source",
        )
    } else {
        match status {
            "active" | "fresh" => ("Active", "fresh", "Providing readings"),
            "watching" => (
                "Watching",
                "fresh",
                "Connected; waiting for relevant readings",
            ),
            "standby" => (
                "Standby",
                "waiting",
                "Connected; another source has priority",
            ),
            "falling_through" => (
                "Backup in use",
                "waiting",
                "Another source is covering its readings",
            ),
            "stale" => ("Delayed", "stale", "Readings are older than expected"),
            "offline" => ("Offline", "offline", "No recent connection or readings"),
            _ => ("Unknown", "waiting", "Status unavailable"),
        }
    };
    SourceStatus {
        label,
        tone,
        meaning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_taxonomy_never_falls_back_to_offline() {
        for status in ["active", "watching", "standby", "falling_through", "fresh"] {
            assert_ne!(presentation(status, true, false).tone, "offline");
        }
        assert_eq!(presentation("offline", true, false).label, "Offline");
        assert_eq!(presentation("", true, false).label, "Unknown");
        assert_eq!(
            presentation("new_server_state", true, false).label,
            "Unknown"
        );
        assert_eq!(presentation("offline", false, false).label, "Off");
        assert_eq!(presentation("offline", true, true).label, "Needs location");
    }
}
