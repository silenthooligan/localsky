// Cross-source device reconciliation (Phase F3). The same physical gateway
// can appear twice: once native (LocalSky reads it directly) and once via
// the HA import. They often share NO stable identity (a user's Ecowitt is
// keyed by MAC natively but by the vendor passkey in HA, and MD5(MAC) is not
// the passkey), so a strict id match can't collapse them. This does a
// conservative reconcile: an HA device is folded into a native device when
// they're the same coarse kind AND share a hardware identity OR the native
// device's vendor token appears in the HA device, AND the match is
// unambiguous (exactly one candidate). The native device wins (LocalSky owns
// it) and is flagged `also_in_ha`; the HA duplicate is dropped. Unmatched HA
// devices pass through unchanged.

use super::Device;

/// Merge native (config-derived) + HA-imported devices, collapsing confident
/// duplicates. Returns the reconciled set, sorted by id.
pub fn reconcile(config: &[Device], ha: &[Device]) -> Vec<Device> {
    let mut out: Vec<Device> = config.to_vec();
    let mut ha_used = vec![false; ha.len()];

    // Every native -> HA match, plus how many natives each HA device matches.
    let mut native_matches: Vec<Vec<usize>> = vec![Vec::new(); out.len()];
    let mut ha_match_count = vec![0usize; ha.len()];
    for (ni, nd) in out.iter().enumerate() {
        for (hj, hd) in ha.iter().enumerate() {
            if same_device(nd, hd) {
                native_matches[ni].push(hj);
                ha_match_count[hj] += 1;
            }
        }
    }

    // Collapse only a bidirectionally unambiguous 1:1 pair: the native matches
    // exactly one HA device AND that HA device matches exactly one native.
    // Either-side ambiguity (two gateways of the same vendor) is left alone.
    for (ni, matches) in native_matches.iter().enumerate() {
        if matches.len() == 1 && ha_match_count[matches[0]] == 1 {
            ha_used[matches[0]] = true;
            out[ni].also_in_ha = true;
        }
    }

    for (i, hd) in ha.iter().enumerate() {
        if !ha_used[i] {
            out.push(hd.clone());
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Whether a native device and an HA-imported device are the same physical
/// hardware.
fn same_device(native: &Device, ha: &Device) -> bool {
    if native.kind != ha.kind {
        return false;
    }
    // Strong signal: a shared hardware identity (e.g. a MAC), separator- and
    // case-normalized.
    if let (Some(n), Some(h)) = (&native.identity, &ha.identity) {
        let (n, h) = (norm_id(n), norm_id(h));
        if !n.is_empty() && n == h {
            return true;
        }
    }
    // Heuristic: the native device's vendor token (first word of its model,
    // e.g. "Ecowitt", "Tempest") appears anywhere in the HA device's model /
    // identity / name. Covers native model "Ecowitt" vs HA model "GW1100B"
    // with identity "ecowitt:<passkey>".
    if let Some(model) = &native.model {
        let vendor = model
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if vendor.len() >= 4 {
            let hay = format!(
                "{} {} {}",
                ha.model.as_deref().unwrap_or("").to_ascii_lowercase(),
                ha.identity.as_deref().unwrap_or("").to_ascii_lowercase(),
                ha.name.to_ascii_lowercase()
            );
            if hay.contains(&vendor) {
                return true;
            }
        }
    }
    false
}

/// Alphanumerics only, lowercased: makes "EC:64:C9" and "ec64c9" comparable.
fn norm_id(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{DeviceKind, DeviceOrigin};

    fn d(
        id: &str,
        kind: DeviceKind,
        origin: DeviceOrigin,
        model: Option<&str>,
        identity: Option<&str>,
    ) -> Device {
        Device {
            id: id.into(),
            kind,
            name: id.into(),
            model: model.map(String::from),
            identity: identity.map(String::from),
            origin,
            source_id: None,
            online: None,
            last_seen_epoch: None,
            also_in_ha: false,
            enabled: None,
            source_kind: None,
            children: Vec::new(),
        }
    }

    #[test]
    fn collapses_ecowitt_native_and_ha() {
        // Real shape: native model "Ecowitt" + host identity; HA model
        // "GW1100B" + "ecowitt:<passkey>" identity. No shared id, vendor match.
        let native = vec![d(
            "source:ecowitt_gw",
            DeviceKind::WeatherGateway,
            DeviceOrigin::Native,
            Some("Ecowitt"),
            Some("192.0.2.61"),
        )];
        let ha = vec![d(
            "ha:abc",
            DeviceKind::WeatherGateway,
            DeviceOrigin::HomeAssistant,
            Some("GW1100B"),
            Some("ecowitt:741EB29E"),
        )];
        let out = reconcile(&native, &ha);
        assert_eq!(out.len(), 1, "the HA duplicate is folded in");
        assert!(out[0].also_in_ha);
        assert_eq!(out[0].id, "source:ecowitt_gw");
    }

    #[test]
    fn keeps_unrelated_ha_device() {
        let native = vec![d(
            "source:tempest_lan",
            DeviceKind::WeatherGateway,
            DeviceOrigin::Native,
            Some("Tempest"),
            None,
        )];
        let ha = vec![d(
            "ha:soil",
            DeviceKind::WeatherGateway,
            DeviceOrigin::HomeAssistant,
            Some("Ecowitt GW1100B"),
            Some("ecowitt:x"),
        )];
        let out = reconcile(&native, &ha);
        assert_eq!(out.len(), 2, "Tempest != Ecowitt, both kept");
        assert!(out.iter().all(|x| !x.also_in_ha));
    }

    #[test]
    fn does_not_collapse_ambiguous_two_gateways() {
        // Two native Ecowitt-ish gateways + one HA Ecowitt: ambiguous, so the
        // HA one is NOT collapsed into either (avoids a wrong merge).
        let native = vec![
            d(
                "source:gw1",
                DeviceKind::WeatherGateway,
                DeviceOrigin::Native,
                Some("Ecowitt"),
                None,
            ),
            d(
                "source:gw2",
                DeviceKind::WeatherGateway,
                DeviceOrigin::Native,
                Some("Ecowitt"),
                None,
            ),
        ];
        let ha = vec![d(
            "ha:e",
            DeviceKind::WeatherGateway,
            DeviceOrigin::HomeAssistant,
            Some("Ecowitt"),
            None,
        )];
        let out = reconcile(&native, &ha);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|x| !x.also_in_ha));
    }

    #[test]
    fn different_kinds_never_match() {
        let native = vec![d(
            "source:mqtt",
            DeviceKind::Virtual,
            DeviceOrigin::Native,
            Some("MQTT"),
            None,
        )];
        let ha = vec![d(
            "ha:m",
            DeviceKind::WeatherGateway,
            DeviceOrigin::HomeAssistant,
            Some("MQTT thing"),
            None,
        )];
        let out = reconcile(&native, &ha);
        assert_eq!(out.len(), 2);
    }
}
