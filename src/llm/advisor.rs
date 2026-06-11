// Advisor entry points: explain_today + detect_anomalies. Both lazy
// + cached: no background tasks, no eager LLM calls. The dashboard
// hits the endpoints, which check cache then call the configured LLM
// provider on miss. Failures (provider down, parse errors, timeouts)
// cache an `AdvisorError::Offline` for a short TTL so we don't hammer
// the provider during an outage.

use crate::ha::snapshot::IrrigationSnapshot;
use crate::llm::cache::TtlCache;
use crate::llm::client::{ClientError, LlmClient};
use crate::llm::prompts::{ANOMALY_SYSTEM, ANOMALY_VERSION, EXPLAINER_SYSTEM, EXPLAINER_VERSION};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Cache TTLs. Explanations refresh every 5 min unless the snapshot
/// changes; anomalies refresh every hour. Offline state is cached for
/// 60s so we re-probe quickly when the provider comes back.
const EXPLANATION_TTL_SECS: i64 = 300;
const ANOMALY_TTL_SECS: i64 = 3600;
const OFFLINE_TTL_SECS: i64 = 60;

/// Tag-like discriminator for the advisor's response. Returned to the
/// dashboard so a thin badge can render "advisor offline" without
/// tearing down the explanation tile every time the provider hiccups.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdvisorError {
    /// LLM_ADVISOR_DISABLED=1 in the container env. Permanent until restart.
    Disabled,
    /// The configured LLM provider or its upstream is unreachable.
    Offline,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Anomaly {
    pub severity: String, // "info" / "warn" / "alert"
    #[serde(rename = "type")]
    pub kind: String,
    pub description: String,
}

#[derive(Clone)]
pub struct AdvisorState {
    inner: Arc<Inner>,
}

struct Inner {
    client: LlmClient,
    explanations: TtlCache<Result<String, AdvisorError>>,
    anomalies: TtlCache<Result<Vec<Anomaly>, AdvisorError>>,
}

impl AdvisorState {
    pub fn from_env() -> Self {
        let client = LlmClient::from_env().unwrap_or_else(|e| {
            tracing::error!("llm client init failed (will run disabled): {e:#}");
            // Construct a "disabled" client by setting the env var and
            // re-trying, but simpler: re-run with the disabled flag set.
            // If from_env fails twice, we rebuild manually.
            std::env::set_var("LLM_ADVISOR_DISABLED", "1");
            LlmClient::from_env().expect("disabled client never errors")
        });
        Self {
            inner: Arc::new(Inner {
                client,
                explanations: TtlCache::new(),
                anomalies: TtlCache::new(),
            }),
        }
    }

    pub fn disabled(&self) -> bool {
        self.inner.client.disabled()
    }

    /// Generate or fetch a cached 1-2 sentence explanation for the
    /// current snapshot. Returns Err(AdvisorError) when the LLM is
    /// unreachable or disabled, the dashboard hides the tile in that
    /// case rather than showing a stale value.
    pub async fn explain_today(&self, snap: &IrrigationSnapshot) -> Result<String, AdvisorError> {
        let key = explain_cache_key(snap);
        if let Some(cached) = self.inner.explanations.get(&key) {
            return cached;
        }
        if self.inner.client.disabled() {
            let err = Err(AdvisorError::Disabled);
            self.inner
                .explanations
                .put(key, err.clone(), OFFLINE_TTL_SECS);
            return err;
        }
        let prompt = build_explainer_prompt(snap);
        let result = self
            .inner
            .client
            .chat(EXPLAINER_SYSTEM, &prompt, Some(180), Some(0.4))
            .await;
        match result {
            Ok(text) => {
                let trimmed = text.trim().trim_matches('"').to_string();
                self.inner
                    .explanations
                    .put(key, Ok(trimmed.clone()), EXPLANATION_TTL_SECS);
                Ok(trimmed)
            }
            Err(e) => {
                tracing::warn!("advisor explain failed: {e}");
                let err = match e {
                    ClientError::Disabled => Err(AdvisorError::Disabled),
                    _ => Err(AdvisorError::Offline),
                };
                self.inner
                    .explanations
                    .put(key, err.clone(), OFFLINE_TTL_SECS);
                err
            }
        }
    }

    /// Look for inconsistencies in the snapshot. Returns Ok(empty) when
    /// the data is consistent, the prompt is explicit about not
    /// fabricating false positives.
    pub async fn detect_anomalies(
        &self,
        snap: &IrrigationSnapshot,
    ) -> Result<Vec<Anomaly>, AdvisorError> {
        let key = anomaly_cache_key(snap);
        if let Some(cached) = self.inner.anomalies.get(&key) {
            return cached;
        }
        if self.inner.client.disabled() {
            let err = Err(AdvisorError::Disabled);
            self.inner.anomalies.put(key, err.clone(), OFFLINE_TTL_SECS);
            return err;
        }
        let prompt = build_anomaly_prompt(snap);
        let result = self
            .inner
            .client
            .chat(ANOMALY_SYSTEM, &prompt, Some(400), Some(0.2))
            .await;
        match result {
            Ok(text) => {
                let trimmed = strip_json_fence(text.trim());
                match serde_json::from_str::<Vec<Anomaly>>(trimmed) {
                    Ok(a) => {
                        self.inner
                            .anomalies
                            .put(key, Ok(a.clone()), ANOMALY_TTL_SECS);
                        Ok(a)
                    }
                    Err(e) => {
                        tracing::warn!(
                            "advisor anomalies parse failed: {e} body={}",
                            truncate(trimmed, 160)
                        );
                        let err = Err(AdvisorError::Offline);
                        self.inner.anomalies.put(key, err.clone(), OFFLINE_TTL_SECS);
                        err
                    }
                }
            }
            Err(e) => {
                tracing::warn!("advisor anomalies failed: {e}");
                let err = match e {
                    ClientError::Disabled => Err(AdvisorError::Disabled),
                    _ => Err(AdvisorError::Offline),
                };
                self.inner.anomalies.put(key, err.clone(), OFFLINE_TTL_SECS);
                err
            }
        }
    }
}

/// Cache key for explanations: prompt version + verdict + reason +
/// rounded forecast inputs. Coarse on purpose, we don't want a 0.01
/// drift in heat index to invalidate the explanation.
fn explain_cache_key(s: &IrrigationSnapshot) -> String {
    let sk = &s.skip_check;
    let f = &s.forecast;
    format!(
        "{ver}|{verdict}|{reason}|t{tnow:.0}|w{wnow:.0}|r{rt:.2}|n4h{n4h:.2}|tom{tom:.2}p{tp}|3d{w3:.2}|hi3{hi3:.0}|days{days}",
        ver = EXPLAINER_VERSION,
        verdict = sk.verdict,
        reason = sk.reason,
        tnow = sk.temp_now_f,
        wnow = sk.wind_now_mph,
        rt = sk.rain_today_in,
        n4h = sk.rain_next_4h_in,
        tom = sk.forecast_in,
        tp = sk.rain_tomorrow_prob_pct,
        w3 = sk.rain_3day_weighted_in,
        hi3 = sk.heat_index_max_3day_f,
        days = f.days_since_significant_rain,
    )
}

/// Anomaly cache key: epoch hour + zone count + ha_reachable. Coarser
/// than the explanation key, anomalies are about cross-signal
/// consistency, hourly granularity is plenty.
fn anomaly_cache_key(s: &IrrigationSnapshot) -> String {
    let hour_bucket = s.last_refresh_epoch / 3600;
    format!(
        "{ver}|h{hour}|n{n}|reach{reach}",
        ver = ANOMALY_VERSION,
        hour = hour_bucket,
        n = s.zones.len(),
        reach = s.ha_reachable,
    )
}

/// Build the user-message body the explainer reads. We hand it the
/// JSON-y inputs the rule ladder evaluated; the system prompt tells
/// the model to add color, not just echo the reason.
fn build_explainer_prompt(s: &IrrigationSnapshot) -> String {
    let sk = &s.skip_check;
    let f = &s.forecast;
    format!(
        "Verdict: {verdict}\n\
         Reason from rule engine: {reason}\n\
         \n\
         Live (Tempest):\n\
         - temp_now: {tnow:.0}°F\n\
         - wind_now: {wnow:.1} mph (forecast peak today: {wmax:.0} mph)\n\
         - humidity_now: {hnow:.0}%\n\
         - rain_today: {rt:.2}\" (Tempest+OM merged)\n\
         - rain_intensity_now: {ri:.2} in/hr\n\
         \n\
         Forecast (Open-Meteo):\n\
         - rain_next_4h: {n4h:.2}\"\n\
         - rain_tomorrow: {tom:.2}\" at {tp}% confidence\n\
         - rain_3day_weighted: {w3:.2}\" (Σ daily × prob/100)\n\
         - rain_7day_weighted: {w7:.2}\"\n\
         - overnight low next 24h: {tlo:.0}°F\n\
         - 3-day high temp: {thi:.0}°F\n\
         - heat index now: {hin:.0}°F (peak 3d: {hi3:.0}°F)\n\
         - days since significant rain: {days}\n\
         \n\
         Thresholds:\n\
         - rain_skip: {rs:.2}\"\n\
         - max_wind: {mw:.0} mph\n\
         - min_temp: {mt:.0}°F\n\
         \n\
         Write a 1-2 sentence explanation a homeowner would find useful. \
         Don't repeat 'Reason' verbatim, add concrete context from the data.",
        verdict = sk.verdict,
        reason = if sk.reason.is_empty() {
            "running normally"
        } else {
            &sk.reason
        },
        tnow = sk.temp_now_f,
        wnow = sk.wind_now_mph,
        wmax = sk.wind_max_today_mph,
        hnow = sk.humidity_now_pct,
        rt = sk.rain_today_in,
        ri = sk.rain_intensity_now_in_hr,
        n4h = sk.rain_next_4h_in,
        tom = sk.forecast_in,
        tp = sk.rain_tomorrow_prob_pct,
        w3 = sk.rain_3day_weighted_in,
        w7 = sk.rain_7day_weighted_in,
        tlo = sk.temp_min_24h_f,
        thi = sk.temp_max_3day_f,
        hin = sk.heat_index_now_f,
        hi3 = sk.heat_index_max_3day_f,
        days = f.days_since_significant_rain,
        rs = sk.rain_skip_in,
        mw = sk.max_wind_mph,
        mt = sk.min_temp_f,
    )
}

/// Anomaly user-message body. We include both Tempest live + Open-Meteo
/// + the verdict so the model can flag cross-signal mismatches.
fn build_anomaly_prompt(s: &IrrigationSnapshot) -> String {
    let sk = &s.skip_check;
    let f = &s.forecast;
    format!(
        "Snapshot at epoch {epoch} (HA reachable: {reach}, {n_zones} zones tracked):\n\
         \n\
         Tempest (live):\n\
         - temp: {tnow:.1}°F\n\
         - wind avg: {wnow:.1} mph\n\
         - humidity: {hnow:.0}%\n\
         - rain today (Tempest gauge): {rtemp:.2}\"\n\
         - rain intensity now: {ri:.2} in/hr\n\
         - rain type: {rtype}\n\
         \n\
         Open-Meteo (regional forecast):\n\
         - rain today: {rom:.2}\"\n\
         - rain tomorrow: {rtom:.2}\" ({tp}% prob)\n\
         - rain 3-day weighted: {w3:.2}\"\n\
         - temp max today: {tmax:.0}°F\n\
         - temp min today: {tmin:.0}°F\n\
         - heat index 3d peak: {hi3:.0}°F\n\
         - days since significant rain: {days}\n\
         \n\
         Engine verdict: {verdict}, {reason}\n\
         \n\
         Return [] if everything is consistent.",
        epoch = s.last_refresh_epoch,
        reach = s.ha_reachable,
        n_zones = s.zones.len(),
        tnow = sk.temp_now_f,
        wnow = sk.wind_now_mph,
        hnow = sk.humidity_now_pct,
        rtemp = f.rain_today_tempest_in,
        ri = sk.rain_intensity_now_in_hr,
        rtype = f.rain_type,
        rom = f.rain_today_om_in,
        rtom = sk.forecast_in,
        tp = sk.rain_tomorrow_prob_pct,
        w3 = sk.rain_3day_weighted_in,
        tmax = f.temp_max_today_f,
        tmin = f.temp_min_today_f,
        hi3 = sk.heat_index_max_3day_f,
        days = f.days_since_significant_rain,
        verdict = sk.verdict,
        reason = if sk.reason.is_empty() {
            "running normally"
        } else {
            &sk.reason
        },
    )
}

/// Strip a fenced code block (```json ... ```) if the model wrapped
/// its JSON in one. The system prompt tells it not to, but defense
/// in depth, single-shot models still wrap occasionally.
fn strip_json_fence(s: &str) -> &str {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("```json") {
        return rest.trim_start().trim_end_matches('`').trim();
    }
    if let Some(rest) = s.strip_prefix("```") {
        return rest.trim_start().trim_end_matches('`').trim();
    }
    s
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}
