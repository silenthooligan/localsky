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
        Self::from_config_or_env(None)
    }

    /// Build the advisor from the UI/wizard-configured `[llm]` block when it
    /// names a concrete single-endpoint provider (Ollama / OpenAI-compat /
    /// llama.cpp), else fall back to env (LLM_BASE_URL etc). This is what makes a
    /// provider set in Settings actually drive the live advisor: previously the
    /// live advisor used the env-only client, so a UI-configured provider (whose
    /// wizard Test button passes via a DIFFERENT code path) was silently ignored
    /// and the advisor stayed offline. `Auto` still resolves via env here (its
    /// runtime probing is the separate LlmProvider path).
    pub fn from_config_or_env(cfg: Option<&crate::config::schema::LlmConfig>) -> Self {
        let disabled_client = || {
            LlmClient::from_env().unwrap_or_else(|e| {
                tracing::error!("llm client init failed (will run disabled): {e:#}");
                // Force the disabled flag and rebuild; a disabled client never errors.
                std::env::set_var("LLM_ADVISOR_DISABLED", "1");
                LlmClient::from_env().expect("disabled client never errors")
            })
        };
        let client = cfg
            .and_then(LlmClient::from_config)
            .and_then(|r| match r {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::error!("llm client from config failed (falling back to env): {e:#}");
                    None
                }
            })
            .unwrap_or_else(disabled_client);
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
        // Unknown probability keys as "na", distinct from any real percent.
        tp = sk
            .rain_tomorrow_prob_pct
            .map(|v| v.to_string())
            .unwrap_or_else(|| "na".into()),
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
    // No probability reported: no confidence claim (never "at 0%").
    let tomorrow_line = match sk.rain_tomorrow_prob_pct {
        Some(prob) => format!(
            "- rain_tomorrow: {:.2}\" at {prob}% confidence\n",
            sk.forecast_in
        ),
        None => format!(
            "- rain_tomorrow: {:.2}\" (no confidence reported)\n",
            sk.forecast_in
        ),
    };
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
         {tomorrow_line}\
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
    // Absent forecast values are OMITTED, not printed as 0: prompting the
    // model with "temp max today: 0°F" as ground truth invited bogus
    // freeze-anomaly advisories on every install without those values.
    let tomorrow_line = match sk.rain_tomorrow_prob_pct {
        Some(prob) => format!("- rain tomorrow: {:.2}\" ({prob}% prob)\n", sk.forecast_in),
        None => format!(
            "- rain tomorrow: {:.2}\" (no probability reported)\n",
            sk.forecast_in
        ),
    };
    let temp_range_lines = match (f.temp_max_today_f, f.temp_min_today_f) {
        (Some(tmax), Some(tmin)) => {
            format!("- temp max today: {tmax:.0}°F\n- temp min today: {tmin:.0}°F\n")
        }
        (Some(tmax), None) => format!("- temp max today: {tmax:.0}°F\n"),
        (None, Some(tmin)) => format!("- temp min today: {tmin:.0}°F\n"),
        (None, None) => String::new(),
    };
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
         {tomorrow_line}\
         - rain 3-day weighted: {w3:.2}\"\n\
         {temp_range}\
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
        w3 = sk.rain_3day_weighted_in,
        temp_range = temp_range_lines,
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

/// Byte-budgeted truncation that never splits a UTF-8 codepoint: when `max`
/// lands mid-character, walk back to the nearest char boundary before slicing.
/// LLM bodies routinely carry multibyte text (the prompts themselves embed
/// "°F"), and `&s[..max]` on a non-boundary index panics inside the axum
/// handler task, 500ing the advisor request. Log-trimming only, so losing a
/// few bytes to the boundary walk is fine.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn truncate_never_splits_a_multibyte_codepoint() {
        // "°" is two bytes (0xC2 0xB0): a cut at byte 2 of "7°F" lands in the
        // middle of the codepoint and must walk back instead of panicking.
        let s = "7°F";
        assert_eq!(truncate(s, 2), "7…");
        // A budget that lands exactly on a boundary keeps the full prefix.
        assert_eq!(truncate(s, 3), "7°…");
        // Short-enough input is returned untouched.
        assert_eq!(truncate(s, s.len()), s);
        // Plain ASCII behavior is unchanged.
        assert_eq!(truncate("abcdef", 3), "abc…");
        // All-multibyte input cut mid-codepoint (each "…" is three bytes):
        // the cut walks back to the first ellipsis, then the marker appends.
        assert_eq!(truncate("……", 4), "……");
        assert_eq!(truncate("……", 2), "…");
    }
}
