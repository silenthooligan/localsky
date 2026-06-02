// MergedSnapshot. Per-field aggregation across all enabled sources with
// provenance recorded so the UI can show "ET0 5.2 mm via tempest_lan
// (Penman-Monteith)".
//
// Per-field aggregation rules are configurable; defaults preserve the
// v0.1 behavior: rain_today = max, temp_min_24h = min, etc.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::ports::weather_source::WeatherField;

/// One field's current value plus where it came from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldValue {
    pub value: f64,
    pub source_id: String,
    pub observed_at: i64,
    /// Optional method/note from the producer (e.g. "penman_monteith").
    pub method: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MergedSnapshot {
    pub fields: HashMap<String, FieldValue>,
}

impl MergedSnapshot {
    pub fn get(&self, key: &str) -> Option<&FieldValue> {
        self.fields.get(key)
    }

    pub fn value(&self, key: &str) -> Option<f64> {
        self.fields.get(key).map(|v| v.value)
    }

    pub fn insert(&mut self, key: impl Into<String>, value: FieldValue) {
        self.fields.insert(key.into(), value);
    }
}

/// Per-field merge policy. Determines what "winning" means when multiple
/// reachable sources have a value for the same field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergePolicy {
    /// Highest source priority wins. Ties broken by most recent observation.
    HighestPriority,
    /// Numeric max across sources. Used by rain_today so a single dry
    /// gauge can't mask actual rain.
    Max,
    /// Numeric min across sources. Used by temp_min_24h.
    Min,
}

pub fn default_policy(field: WeatherField) -> MergePolicy {
    use WeatherField::*;
    match field {
        RainTodayIn | RainIntensityInHr | LightningCount | WindGustMph => MergePolicy::Max,
        // For overnight low + wind floor a min across sources is safer.
        // Daily/hourly forecasts use "highest priority" as the default
        // since they're aggregate structures, not scalar values.
        _ => MergePolicy::HighestPriority,
    }
}

/// Apply `policy` to a slice of candidate FieldValues and pick a winner.
/// Caller passes priorities alongside each candidate so HighestPriority
/// can break ties.
pub fn merge_field(candidates: &[(FieldValue, i32)], policy: MergePolicy) -> Option<FieldValue> {
    if candidates.is_empty() {
        return None;
    }
    match policy {
        MergePolicy::Max => candidates
            .iter()
            .max_by(|a, b| {
                a.0.value
                    .partial_cmp(&b.0.value)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(v, _)| v.clone()),
        MergePolicy::Min => candidates
            .iter()
            .min_by(|a, b| {
                a.0.value
                    .partial_cmp(&b.0.value)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(v, _)| v.clone()),
        MergePolicy::HighestPriority => candidates
            .iter()
            .max_by(|a, b| {
                a.1.cmp(&b.1)
                    .then_with(|| a.0.observed_at.cmp(&b.0.observed_at))
            })
            .map(|(v, _)| v.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fv(src: &str, val: f64, at: i64) -> FieldValue {
        FieldValue {
            value: val,
            source_id: src.into(),
            observed_at: at,
            method: None,
        }
    }

    #[test]
    fn highest_priority_wins() {
        let candidates = vec![
            (fv("tempest", 21.0, 100), 100),
            (fv("openmeteo", 22.5, 100), 50),
        ];
        let w = merge_field(&candidates, MergePolicy::HighestPriority).unwrap();
        assert_eq!(w.source_id, "tempest");
    }

    #[test]
    fn priority_tie_breaks_by_recency() {
        let candidates = vec![(fv("a", 21.0, 100), 50), (fv("b", 22.0, 200), 50)];
        let w = merge_field(&candidates, MergePolicy::HighestPriority).unwrap();
        assert_eq!(w.source_id, "b");
    }

    #[test]
    fn max_policy_picks_largest() {
        let candidates = vec![
            (fv("tempest", 0.05, 100), 100),
            (fv("openmeteo", 0.15, 100), 50),
        ];
        let w = merge_field(&candidates, MergePolicy::Max).unwrap();
        assert_eq!(w.source_id, "openmeteo");
        assert!((w.value - 0.15).abs() < 1e-9);
    }

    #[test]
    fn min_policy_picks_smallest() {
        let candidates = vec![(fv("a", 38.0, 100), 100), (fv("b", 32.0, 100), 50)];
        let w = merge_field(&candidates, MergePolicy::Min).unwrap();
        assert!((w.value - 32.0).abs() < 1e-9);
    }

    #[test]
    fn empty_candidates_returns_none() {
        let r = merge_field(&[], MergePolicy::Max);
        assert!(r.is_none());
    }

    #[test]
    fn default_policies_match_v01_intent() {
        assert!(matches!(
            default_policy(WeatherField::RainTodayIn),
            MergePolicy::Max
        ));
        assert!(matches!(
            default_policy(WeatherField::LightningCount),
            MergePolicy::Max
        ));
        // Most other fields default to HighestPriority.
        assert!(matches!(
            default_policy(WeatherField::AirTempF),
            MergePolicy::HighestPriority
        ));
    }
}
