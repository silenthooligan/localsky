// Declarative per-kind scalar field schema for the controller editor, the
// controller-side mirror of sources_form::field_schema. It renders the
// connection settings (host/url/tokens/poll cadence) a beginner fills by hand
// as labeled inputs, two-way-synced to the raw `config_text` JSON, while nested
// zone maps (zone_command_map, zone_*_map) stay in the advanced JSON editor and
// are populated by "Scan zones". Reuses the proven field_row renderer + flush
// pattern so there's one rendering/sync path, not two.

use leptos::prelude::*;

use crate::components::sources_form::field_schema::{field_row, FieldSpec};

/// Scalar connection fields offered as a guided form for each controller kind.
/// Nested maps are intentionally omitted here (they live in the JSON editor).
pub fn controller_fields(kind: &str) -> Vec<FieldSpec> {
    match kind {
        "opensprinkler_direct" => vec![
            FieldSpec::text("host", "Host / IP", "The controller's LAN address.", true, "192.0.2.50"),
            FieldSpec::int("port", "Port", "HTTP port (default 80).", 80.0),
            FieldSpec::secret(
                "password_md5",
                "Password (MD5)",
                "md5(password), lowercased. The setup wizard computes this for you.",
                true,
                "",
            ),
            FieldSpec::int("poll_interval_s", "Poll interval (s)", "How often to read controller status.", 10.0),
        ],
        "http_generic" => vec![
            FieldSpec::text(
                "base_url",
                "Base URL",
                "The board's address, e.g. http://192.0.2.50",
                true,
                "http://192.0.2.50",
            ),
            FieldSpec::secret(
                "bearer_token",
                "Bearer token",
                "Optional. Sent as Authorization: Bearer <token>. Leave blank for an open board.",
                false,
                "",
            ),
            FieldSpec::int("poll_interval_s", "Poll interval (s)", "How often to read board status.", 10.0),
        ],
        "mqtt_command" => vec![
            FieldSpec::text("broker_host", "Broker host", "MQTT broker LAN address.", true, "192.0.2.10"),
            FieldSpec::int("broker_port", "Broker port", "Default 1883.", 1883.0),
            FieldSpec::text("username", "Username", "Optional broker username.", false, ""),
            FieldSpec::secret("password", "Password", "Optional broker password.", false, ""),
            FieldSpec::text(
                "availability_topic",
                "Availability topic",
                "Optional. The board's online/offline (LWT) topic.",
                false,
                "localsky-irrig/status",
            ),
            FieldSpec::text(
                "flow_topic",
                "Flow topic",
                "Optional. A topic publishing a numeric flow rate (gal/min).",
                false,
                "",
            ),
        ],
        "ha_service_call" => vec![
            FieldSpec::text(
                "base_url",
                "Home Assistant URL",
                "e.g. http://homeassistant.local:8123",
                true,
                "http://homeassistant.local:8123",
            ),
            FieldSpec::secret("bearer_token", "Long-lived token", "A Home Assistant long-lived access token.", true, ""),
            FieldSpec::text_default(
                "start_service",
                "Start service",
                "domain.service to start a zone.",
                "script.os_zone_toggle",
                "",
            ),
            FieldSpec::text_default(
                "stop_service",
                "Stop service",
                "domain.service to stop a zone.",
                "opensprinkler.stop",
                "",
            ),
        ],
        "rachio" => vec![
            FieldSpec::secret("api_token", "API token", "Rachio API token.", true, ""),
            FieldSpec::text("device_id", "Device ID", "Rachio device id.", true, ""),
        ],
        "hydrawise" => vec![
            FieldSpec::secret("api_key", "API key", "Hydrawise account API key.", true, ""),
            FieldSpec::int_required("controller_id", "Controller ID", "Hydrawise controller id.", ""),
        ],
        "bhyve" => vec![
            FieldSpec::text("email", "Email", "Orbit B-hyve account email.", true, ""),
            FieldSpec::secret("password", "Password", "B-hyve account password.", true, ""),
            FieldSpec::text("device_id", "Device ID", "B-hyve device id.", true, ""),
        ],
        "rainbird" => vec![
            FieldSpec::text("email", "Email", "Rain Bird account email.", true, ""),
            FieldSpec::secret("password", "Password", "Rain Bird account password.", true, ""),
            FieldSpec::text("controller_id", "Controller ID", "Rain Bird controller serial / id.", true, ""),
            FieldSpec::text_default(
                "base_url",
                "API base URL",
                "Override only if Rain Bird rotates hosts.",
                "https://rdz-rest.rainbird.com",
                "",
            ),
        ],
        "dry_run" => vec![FieldSpec::boolean(
            "simulate_runs",
            "Simulate runs",
            "Write synthetic run rows so the dashboard shows activity (demo).",
            false,
        )],
        _ => vec![],
    }
}

/// Labeled connection form for a controller kind, two-way-synced to
/// `config_text`. Mirrors sources_form::SourceConfigForm (including its
/// self-edit guard + fresh-read flush) so only this kind's scalar keys are
/// written and any nested map keys in the JSON are structurally preserved.
#[component]
pub fn ControllerConfigForm(
    config_text: RwSignal<String>,
    #[prop(into)] kind: Signal<String>,
) -> impl IntoView {
    let cfg = RwSignal::new(
        serde_json::from_str::<serde_json::Value>(&config_text.get_untracked())
            .unwrap_or(serde_json::Value::Null),
    );
    // See sources_form: a value-based guard would loop because flush()
    // normalizes the JSON, so a flag marks writes WE caused.
    let self_edit = RwSignal::new(false);

    let flush = move || {
        let mut fresh: serde_json::Value =
            serde_json::from_str(&config_text.get_untracked()).unwrap_or(serde_json::json!({}));
        let mine = cfg.get_untracked();
        for spec in controller_fields(&kind.get_untracked()) {
            match mine.get(spec.key) {
                Some(v) => {
                    if let Some(obj) = fresh.as_object_mut() {
                        obj.insert(spec.key.to_string(), v.clone());
                    } else {
                        fresh = serde_json::json!({ spec.key: v.clone() });
                    }
                }
                None => {
                    if let Some(obj) = fresh.as_object_mut() {
                        obj.remove(spec.key);
                    }
                }
            }
        }
        self_edit.set(true);
        config_text.set(serde_json::to_string_pretty(&fresh).unwrap_or_else(|_| "{}".into()));
    };

    // Re-seed `cfg` when config_text changes from OUTSIDE this form (the JSON
    // textarea, or the kind-swap template reset in the parent).
    Effect::new(move |_| {
        let text = config_text.get();
        if self_edit.get_untracked() {
            self_edit.set(false);
            return;
        }
        let parsed =
            serde_json::from_str::<serde_json::Value>(&text).unwrap_or(serde_json::Value::Null);
        if parsed != cfg.get_untracked() {
            cfg.set(parsed);
        }
    });

    view! {
        <div class="controller-config-form">
            {move || {
                controller_fields(&kind.get())
                    .iter()
                    .map(|spec| field_row(spec, cfg, flush))
                    .collect_view()
            }}
        </div>
    }
}

#[cfg(all(test, feature = "ssr"))]
mod tests {
    use super::*;

    #[test]
    fn every_buildable_kind_has_connection_fields() {
        // The kinds offered in the picker should each render at least one
        // guided field (dry_run has its single toggle). Mirrors the parity
        // test in mod.rs but for the structured form.
        for kind in [
            "opensprinkler_direct",
            "http_generic",
            "mqtt_command",
            "ha_service_call",
            "rachio",
            "hydrawise",
            "bhyve",
            "rainbird",
            "dry_run",
        ] {
            assert!(
                !controller_fields(kind).is_empty(),
                "controller_fields(`{kind}`) should expose at least one field"
            );
        }
        // An unknown/hidden kind yields no guided fields (JSON-only).
        assert!(controller_fields("esphome_native").is_empty());
    }

    #[test]
    fn field_keys_match_schema_serde_keys() {
        // Guard against a typo'd key that would silently never persist. Spot-check
        // the DIY kinds whose fields this iteration introduced.
        let http: Vec<_> = controller_fields("http_generic")
            .iter()
            .map(|f| f.key)
            .collect();
        assert_eq!(http, vec!["base_url", "bearer_token", "poll_interval_s"]);
        let mqtt: Vec<_> = controller_fields("mqtt_command")
            .iter()
            .map(|f| f.key)
            .collect();
        assert!(mqtt.contains(&"broker_host"));
        assert!(mqtt.contains(&"availability_topic"));
        assert!(mqtt.contains(&"flow_topic"));
    }
}
