// Persistent soil-anomaly banner. Sibling of RunningBanner: where that one
// surfaces zones actively WATERING, this surfaces zones whose soil data is
// untrustworthy, the two conditions the operator most wants to know about at
// a glance without opening the Sensors page.
//
// Surfaces two anomaly classes, one concise line each:
//   (a) SUSPECT probe: the quarantine logic distrusted this zone's probe (a
//       wild outlier vs its siblings, or offline) REGARDLESS of the final
//       verdict. Read from the verdict-INDEPENDENT `zone.soil_suspect`
//       indicator (set by the engine's `suspect_probes`), so a genuinely bad
//       probe shows even when a global gate (forecast rain, freeze, ...)
//       ultimately decided the zone and masked `verdict.source` to "global".
//       The reason string carries the numbers.
//   (b) OFFLINE probe: a configured probe that has stopped producing valid
//       readings entirely (snap.soil_probe_faults). The saturation gate is
//       running without it.
//
// Quiet by default: returns ().into_any() when there are no anomalies so the
// page shows nothing in the healthy case. Display/notification only, it never
// changes a watering decision (the engine already made those).
//
// Data: reads the same IrrigationSnapshot signal everything else uses. Links
// to /sensors so a tap goes straight to the probe inventory.

use crate::ha::snapshot::{IrrigationSnapshot, SoilProbeFault};
use leptos::prelude::*;

#[component]
pub fn AnomalyBanner(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    move || {
        let s = snap.get();

        // (a) Suspect zones: the quarantine logic distrusted the probe,
        // independent of the final verdict. Read from `zone.soil_suspect`
        // (set by the engine's verdict-independent `suspect_probes`), so a
        // genuinely bad probe shows even when a global gate decided the zone.
        // The stored reason carries the canonical "Soil probe suspect (28% vs
        // yard 73%)" shape; trim to the human line "Back Yard probe suspect:
        // 28% vs yard 73%".
        let mut lines: Vec<String> = s
            .zones
            .iter()
            .filter_map(|z| {
                z.soil_suspect
                    .as_deref()
                    .map(|reason| suspect_line(&z.name, Some(reason)))
            })
            .collect();

        // (b) Offline probes: configured but producing no valid reading.
        // One calm line per fault that names the ZONE, the SENSOR (device
        // family parsed from the spec, e.g. "WH51" for an Ecowitt soil
        // channel, else a generic "soil probe"), the SINCE date (24h local
        // via crate::timefmt), and a remediation verb, so the operator knows
        // which physical probe to touch and what to do, not just which zone.
        let tz = s.timezone.clone();
        lines.extend(s.soil_probe_faults.iter().map(|f| fault_line(f, &tz)));

        if lines.is_empty() {
            return ().into_any();
        }

        view! {
            <div class="anomaly-banner" role="status" aria-live="polite">
                <div class="anomaly-banner-icon" aria-hidden="true">"!"</div>
                <div class="anomaly-banner-text">
                    {lines
                        .into_iter()
                        .map(|l| view! { <div class="anomaly-banner-line">{l}</div> })
                        .collect_view()}
                </div>
                <a class="anomaly-banner-link" href="/sensors">"Sensors"</a>
            </div>
        }
        .into_any()
    }
}

/// Build the one-line suspect summary for a quarantined zone. Prefers the
/// engine's canonical reason (parsed to the pre-"inferred" half so it reads
/// "Back Yard probe suspect: 28% vs yard 73%"); falls back to a generic line
/// when the reason is missing or doesn't match the expected shape.
fn suspect_line(name: &str, reason: Option<&str>) -> String {
    if let Some(r) = reason {
        // Canonical engine form: "Soil probe suspect (28% vs yard 73%); inferred ..."
        if let Some(rest) = r.strip_prefix("Soil probe suspect (") {
            if let Some((inner, _)) = rest.split_once(')') {
                // inner = "28% vs yard 73%" (or "offline vs yard 73%")
                return format!("{name} probe suspect: {inner}");
            }
        }
    }
    format!("{name} probe suspect: watering decided from neighbors")
}

/// Build the one-line offline summary for a faulted soil probe. Names the
/// ZONE, the SENSOR by its device family (parsed from `f.sensor_id`, e.g.
/// "WH51" for an Ecowitt soil channel, otherwise a generic "soil probe"),
/// the SINCE moment (the channel's last good reading as a 24h-local short
/// date via `crate::timefmt`, in the deployment `tz`; "no reading yet" when
/// the channel never produced a valid value), and a remediation VERB so the
/// operator knows which physical probe to touch and what to do.
///
/// Examples:
///   "Back Yard WH51 soil probe offline since Jun 28. Reseat or replace it."
///   "Side Bed soil probe offline, no reading yet. Reseat or replace it."
fn fault_line(f: &SoilProbeFault, tz: &str) -> String {
    let device = device_family(&f.sensor_id);
    let since = match f.since_epoch {
        Some(epoch) if epoch > 0 => {
            let md = crate::timefmt::format_md(epoch, tz);
            if md.is_empty() {
                "offline".to_string()
            } else {
                format!("offline since {md}")
            }
        }
        // None / never-valid: the channel has produced nothing above 0%.
        _ => "offline, no reading yet".to_string(),
    };
    format!("{} {device} {since}. Reseat or replace it.", f.zone_name)
}

/// Friendly device hint for a configured soil-sensor spec. The known Ecowitt
/// soil channel (`source:<id>:soilmoisture<N>`) is a WH51; everything else
/// (an `ha:` entity, a legacy bare id, an unrecognized channel key) falls
/// back to a generic "soil probe" so the line never reads awkwardly.
fn device_family(sensor_id: &str) -> &'static str {
    // `source:<id>:<key>` -> inspect the trailing channel key. An Ecowitt
    // soil channel keys as `soilmoisture<N>`, which is a WH51 in the field.
    if let Some(rest) = sensor_id.strip_prefix("source:") {
        if let Some((_, key)) = rest.split_once(':') {
            if key.starts_with("soilmoisture") {
                return "WH51 soil probe";
            }
        }
    }
    "soil probe"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fault(zone: &str, sensor: &str, since: Option<i64>) -> SoilProbeFault {
        SoilProbeFault {
            zone_slug: String::new(),
            zone_name: zone.to_string(),
            sensor_id: sensor.to_string(),
            since_epoch: since,
        }
    }

    #[test]
    fn device_family_names_ecowitt_soil_channel_as_wh51() {
        // The canonical Ecowitt soil channel spec resolves to a WH51.
        assert_eq!(
            device_family("source:ecowitt_gw:soilmoisture2"),
            "WH51 soil probe"
        );
        assert_eq!(
            device_family("source:gw:soilmoisture_back_yard"),
            "WH51 soil probe"
        );
    }

    #[test]
    fn device_family_falls_back_to_generic_for_unknown_specs() {
        // ha: entity, legacy bare id, and a non-soil source channel all read
        // as a generic "soil probe" so the line never names the wrong part.
        assert_eq!(device_family("ha:sensor.back_yard_moisture"), "soil probe");
        assert_eq!(device_family("sensor.back_yard_moisture"), "soil probe");
        assert_eq!(device_family("source:gw:temperature"), "soil probe");
        assert_eq!(device_family(""), "soil probe");
    }

    #[test]
    fn fault_line_with_no_reading_yet_omits_a_date_and_keeps_remediation() {
        // None since_epoch (channel never produced a valid value): no date,
        // an explicit "no reading yet", still names the device and the verb.
        let line = fault_line(
            &fault("Side Bed", "source:gw:soilmoisture3", None),
            "America/New_York",
        );
        assert_eq!(
            line,
            "Side Bed WH51 soil probe offline, no reading yet. Reseat or replace it."
        );
    }

    #[test]
    fn fault_line_with_unknown_family_reads_as_generic_soil_probe() {
        let line = fault_line(
            &fault("Front", "ha:sensor.front_moisture", Some(0)),
            "America/New_York",
        );
        // since_epoch 0 is treated as never-valid, so the generic device line
        // still resolves cleanly without naming a WH51.
        assert_eq!(
            line,
            "Front soil probe offline, no reading yet. Reseat or replace it."
        );
    }

    // The dated branch routes through crate::timefmt, whose named-zone path is
    // chrono-tz (ssr) only; assert the exact "since Jun 28" copy under ssr.
    #[cfg(all(feature = "ssr", not(feature = "hydrate")))]
    #[test]
    fn fault_line_with_since_date_names_zone_device_date_and_verb() {
        // 2026-06-28 18:05:00 UTC -> Jun 28 in America/New_York (matches the
        // timefmt crate's own anchor).
        const EPOCH: i64 = 1_782_669_900;
        let line = fault_line(
            &fault("Back Yard", "source:ecowitt_gw:soilmoisture2", Some(EPOCH)),
            "America/New_York",
        );
        assert_eq!(
            line,
            "Back Yard WH51 soil probe offline since Jun 28. Reseat or replace it."
        );
    }
}
