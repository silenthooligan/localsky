//! Resolved meter readings. Missing evidence stays missing, including zero.

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FlowReadout {
    pub rate_gpm: Option<f64>,
    pub rate_source_id: Option<String>,
    pub total_gal_today: Option<f64>,
    pub total_source_id: Option<String>,
}

impl FlowReadout {
    /// A real controller meter is closest to the valves. A type capability or
    /// unavailable reading never displaces the bus's selected meter. Cumulative
    /// water is independent: a rate cannot invent a since-midnight total.
    pub fn prefer_controller(&mut self, source_id: &str, gpm: Option<f64>) {
        if let Some(value) = gpm.filter(|v| v.is_finite() && *v >= 0.0) {
            self.rate_gpm = Some(value);
            self.rate_source_id = Some(format!("controller:{source_id}"));
        }
    }
}

/// A timestamped scalar already accepted by the source arbiter.
#[cfg(feature = "ssr")]
#[derive(Debug, Clone)]
pub(crate) struct FlowSample {
    pub value: f64,
    pub source_id: String,
    pub observed_epoch: i64,
}

#[cfg(feature = "ssr")]
impl FlowSample {
    pub fn fresh_value(&self, now: i64, max_age_s: i64) -> Option<f64> {
        (self.observed_epoch > 0
            && self.observed_epoch <= now
            && now.saturating_sub(self.observed_epoch) <= max_age_s
            && self.value.is_finite()
            && self.value >= 0.0)
            .then_some(self.value)
    }
}
