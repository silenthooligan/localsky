//! Pure queries over canonical hour-start forecasts, shared by every track.
use super::snapshot::{ForecastSnapshot, HourlyEntry};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForecastWindow {
    pub track: String,
    pub model: Option<String>,
    pub provider_label: String,
    pub fetched_at: Option<i64>,
    pub age_s: Option<i64>,
    pub from: i64,
    pub to: i64,
    pub hours: usize,
    pub expected_hours: usize,
    pub complete: bool,
    pub precipitation_hours: usize,
    pub probability_hours: usize,
    pub temperature_hours: usize,
    pub pop_max_pct: Option<u32>,
    pub precip_max_in: Option<f64>,
    pub precip_sum_in: Option<f64>,
    pub temp_max_f: Option<f64>,
    pub temp_min_f: Option<f64>,
    pub hourly: Vec<WindowHour>,
}

/// Only fields whose presence is preserved by every forecast adapter.
/// Legacy advisory entries use zero placeholders for unsupported series;
/// those are not observations and must not escape through this new API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowHour {
    pub time_epoch: i64,
    pub temp_f: Option<f64>,
    pub precip_in: Option<f64>,
    pub precip_probability: Option<u32>,
}

impl From<&HourlyEntry> for WindowHour {
    fn from(hour: &HourlyEntry) -> Self {
        Self {
            time_epoch: hour.time_epoch,
            temp_f: hour.temp_f.filter(|v| v.is_finite()),
            precip_in: hour.precip_in.filter(|v| v.is_finite() && *v >= 0.0),
            precip_probability: hour.precip_probability.filter(|v| *v <= 100),
        }
    }
}

pub fn validate_range(from: i64, to: i64, max_seconds: i64) -> Result<(), &'static str> {
    if from < 0 || to > 253_402_300_799 || to < from {
        return Err("from and to must be ordered UTC epoch seconds between 1970 and 9999");
    }
    if to - from > max_seconds {
        return Err("requested forecast range exceeds the endpoint limit");
    }
    Ok(())
}

pub fn query(
    snapshot: &ForecastSnapshot,
    track: &str,
    model: Option<&str>,
    from: i64,
    to: i64,
    now: i64,
) -> Result<ForecastWindow, &'static str> {
    validate_range(from, to, 48 * 3600)?;
    let mut hourly: Vec<_> = snapshot
        .hourly
        .iter()
        .filter(|h| h.time_epoch >= from && h.time_epoch <= to)
        .map(WindowHour::from)
        .collect();
    hourly.sort_by_key(|h| h.time_epoch);
    hourly.dedup_by_key(|h| h.time_epoch);
    let offset = snapshot
        .hourly
        .first()
        .map(|h| h.time_epoch.rem_euclid(3600))
        .unwrap_or(0);
    let expected_hours =
        ((to - offset).div_euclid(3600) - (from - 1 - offset).div_euclid(3600)) as usize;
    let hours = hourly.len();
    let complete = hours > 0
        && hours == expected_hours
        && hourly
            .windows(2)
            .all(|h| h[1].time_epoch - h[0].time_epoch == 3600);
    let precipitation: Vec<f64> = hourly
        .iter()
        .filter_map(|h| h.precip_in.filter(|v| v.is_finite() && *v >= 0.0))
        .collect();
    let probability: Vec<u32> = hourly
        .iter()
        .filter_map(|h| h.precip_probability.filter(|v| *v <= 100))
        .collect();
    let temperature: Vec<f64> = hourly
        .iter()
        .filter_map(|h| h.temp_f.filter(|v| v.is_finite()))
        .collect();
    let rain_complete = complete && precipitation.len() == hours;
    let pop_complete = complete && probability.len() == hours;
    let temp_complete = complete && temperature.len() == hours;
    let fetched_at = (snapshot.last_refresh_epoch > 0).then_some(snapshot.last_refresh_epoch);
    Ok(ForecastWindow {
        track: track.into(),
        model: model.map(str::to_owned),
        provider_label: snapshot.source_label.clone(),
        fetched_at,
        age_s: fetched_at.map(|at| now.saturating_sub(at).max(0)),
        from,
        to,
        hours,
        expected_hours,
        complete,
        precipitation_hours: precipitation.len(),
        probability_hours: probability.len(),
        temperature_hours: temperature.len(),
        pop_max_pct: pop_complete
            .then(|| probability.iter().copied().max())
            .flatten(),
        precip_max_in: rain_complete
            .then(|| precipitation.iter().copied().reduce(f64::max))
            .flatten(),
        precip_sum_in: rain_complete.then(|| precipitation.iter().sum()),
        temp_max_f: temp_complete
            .then(|| temperature.iter().copied().reduce(f64::max))
            .flatten(),
        temp_min_f: temp_complete
            .then(|| temperature.iter().copied().reduce(f64::min))
            .flatten(),
        hourly,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn forecast() -> ForecastSnapshot {
        ForecastSnapshot {
            last_refresh_epoch: 3600,
            hourly: (1..=4)
                .map(|n| HourlyEntry {
                    time_epoch: n * 3600,
                    precip_in: Some(0.1),
                    precip_probability: Some(30),
                    temp_f: Some(60.0 + n as f64),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }
    #[test]
    fn inclusive_window_and_original_age() {
        let result = query(&forecast(), "merged", None, 7200, 10800, 72000).unwrap();
        assert_eq!(result.hours, 2);
        assert_eq!(result.age_s, Some(68400));
        assert_eq!(result.precip_sum_in, Some(0.2));
        assert_eq!(result.temp_min_f, Some(62.0));
    }
    #[test]
    fn missing_rows_and_values_never_become_a_dry_window() {
        let mut f = forecast();
        f.hourly[1].precip_in = None;
        let result = query(&f, "nbm", Some("ncep_nbm_conus"), 3600, 10800, 7200).unwrap();
        assert_eq!(result.precip_sum_in, None);
        assert_eq!(result.pop_max_pct, Some(30));
        f.hourly.remove(1);
        let result = query(&f, "merged", None, 3600, 10800, 7200).unwrap();
        assert!(!result.complete);
        assert_eq!(result.pop_max_pct, None);
        assert_eq!(result.temp_max_f, None);
        let empty = query(&f, "merged", None, 72000, 75600, 7200).unwrap();
        assert_eq!(empty.hours, 0);
        assert_eq!(empty.precip_sum_in, None);
    }
    #[test]
    fn boundaries_are_checked_before_arithmetic() {
        for (from, to) in [(4, 3), (0, 172801), (i64::MIN, i64::MAX)] {
            assert!(query(&forecast(), "merged", None, from, to, 7200).is_err());
        }
    }

    #[test]
    fn hourly_response_preserves_unknowns_without_legacy_advisory_placeholders() {
        let mut f = forecast();
        f.hourly[0].temp_f = None;
        f.hourly[0].precip_in = Some(0.0);
        f.hourly[0].precip_probability = Some(0);
        let result = query(&f, "nbm", None, 3600, 3600, 7200).unwrap();
        assert_eq!(
            serde_json::to_value(&result.hourly[0]).unwrap(),
            serde_json::json!({
                "time_epoch": 3600,
                "temp_f": null,
                "precip_in": 0.0,
                "precip_probability": 0,
            })
        );
        assert_eq!(result.temp_max_f, None);
        assert_eq!(result.precip_sum_in, Some(0.0));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackStatus {
    pub id: String,
    pub model: String,
    pub provider_label: String,
    pub fetched_at: Option<i64>,
    pub age_s: Option<i64>,
    pub tier: String,
    pub degraded: bool,
    pub last_error: Option<String>,
    /// Serialized shared failure record; retained as owned JSON in the browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<serde_json::Value>,
}
