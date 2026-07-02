// Catalog defaults for sprinkler precipitation rate, mm/hr. Mid-band
// typical residential values: operators with measurements override per
// zone via ZoneConfig.precip_rate_mm_hr from the settings UI.

use crate::config::schema::SprinklerType;

/// Resolve the precipitation rate (mm/hr) for a sprinkler type when the operator
/// has not measured it. The DATA lives in the shared, slug-keyed `crate::agronomy`
/// catalog (read by the wasm UI too); this delegates via the enum's serde slug,
/// pinned by `sprinkler_slug_matches_serde`.
pub fn catalog_precip_rate_mm_hr(t: SprinklerType) -> f64 {
    crate::agronomy::sprinkler_precip_mm_hr(sprinkler_slug(t))
}

/// Enum -> snake_case slug used by the agronomy catalog + the config wire format.
fn sprinkler_slug(t: SprinklerType) -> &'static str {
    use SprinklerType::*;
    match t {
        Rotor => "rotor",
        Spray => "spray",
        MpRotator => "mp_rotator",
        Drip => "drip",
        Bubbler => "bubbler",
        Other => "other",
    }
}

/// Effective precipitation rate (mm/hr) for a zone: explicit
/// `precip_rate_mm_hr` override when present, otherwise catalog default
/// keyed off `sprinkler_type`.
pub fn effective_precip_rate_mm_hr(
    sprinkler_type: SprinklerType,
    override_value: Option<f64>,
) -> f64 {
    override_value
        .filter(|v| *v > 0.0)
        .unwrap_or_else(|| catalog_precip_rate_mm_hr(sprinkler_type))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sprinkler_slug` must equal each variant's serde wire name (the agronomy
    /// catalog + config file key off it). A drift would silently return the
    /// generic "other" rate.
    #[test]
    fn sprinkler_slug_matches_serde() {
        let variants = [
            SprinklerType::Rotor,
            SprinklerType::Spray,
            SprinklerType::MpRotator,
            SprinklerType::Drip,
            SprinklerType::Bubbler,
            SprinklerType::Other,
        ];
        for v in variants {
            assert_eq!(
                serde_json::Value::String(sprinkler_slug(v).to_string()),
                serde_json::to_value(v).unwrap(),
                "sprinkler_slug drifted from serde for {v:?}"
            );
        }
    }

    #[test]
    fn effective_prefers_positive_override() {
        assert_eq!(
            effective_precip_rate_mm_hr(SprinklerType::Rotor, Some(20.0)),
            20.0
        );
        assert_eq!(
            effective_precip_rate_mm_hr(SprinklerType::Rotor, None),
            10.0
        );
        // A non-positive override falls back to the catalog default.
        assert_eq!(
            effective_precip_rate_mm_hr(SprinklerType::Spray, Some(0.0)),
            38.0
        );
    }
}
