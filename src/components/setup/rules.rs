// The wizard's watering-rules step. Most yards in the United States
// water under a district or HOA rule, and the wizard never asked: the
// Settings page had the parity control and three starters, and a new
// install found them only after a morning that watered on a day it
// should not have. This step asks, in a homeowner's words, and writes
// the same shapes the Settings page writes.

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, prev_step_href, SetupFooter};
use crate::components::ui::{HelpHint, Panel, SegmentedControl};

/// The three starters the Settings page offers, as the wizard's own
/// choices. Each is the exact rule the Settings page would add.
pub fn starter(kind: &str) -> Option<serde_json::Value> {
    match kind {
        "no_midday" => Some(serde_json::json!({
            "id": "starter_no_midday", "name": "No midday watering", "enabled": true,
            "effective": { "kind": "all_year" },
            "forbidden_hour_start": 10, "forbidden_hour_end": 16,
        })),
        "two_days" => Some(serde_json::json!({
            "id": "starter_two_days", "name": "Two days a week", "enabled": true,
            "effective": { "kind": "all_year" },
            "allowed_weekdays": [3, 6],
            "forbidden_hour_start": 10, "forbidden_hour_end": 16,
        })),
        "odd_even" => Some(serde_json::json!({
            "id": "starter_odd_even", "name": "Odd/even address days", "enabled": true,
            "effective": { "kind": "all_year" },
            "allowed_weekdays_odd": [3, 6], "allowed_weekdays_even": [4, 0],
        })),
        _ => None,
    }
}

/// What the wizard wrote, read back for the Review page.
pub fn rules_summary(cfg: &serde_json::Value) -> String {
    let rules: Vec<&serde_json::Value> = cfg
        .get("engine")
        .and_then(|e| e.get("watering_restrictions"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter(|r| r.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true))
                .collect()
        })
        .unwrap_or_default();
    if rules.is_empty() {
        return "None. LocalSky waters any day the weather allows.".to_string();
    }
    let names: Vec<&str> = rules
        .iter()
        .filter_map(|r| r.get("name").and_then(|v| v.as_str()))
        .collect();
    let parity = cfg
        .get("deployment")
        .and_then(|d| d.get("address_parity"))
        .and_then(|v| v.as_str())
        .unwrap_or("not_applicable");
    let parity_note = match parity {
        "odd" => " Your house number is odd.",
        "even" => " Your house number is even.",
        _ => "",
    };
    format!("{}.{parity_note}", names.join(", "))
}

#[component]
pub fn RulesStep() -> impl IntoView {
    let parity = RwSignal::new("not_applicable".to_string());
    let choice = RwSignal::new("none".to_string());
    let draft = RwSignal::new(serde_json::Value::Null);
    let loaded = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Some(d) = crate::components::setup::draft::fetch().await {
                let cfg = d.get("config").cloned().unwrap_or_default();
                parity.set(
                    cfg.get("deployment")
                        .and_then(|x| x.get("address_parity"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("not_applicable")
                        .to_string(),
                );
                // Which starter is present, if one is.
                let ids: Vec<String> = cfg
                    .get("engine")
                    .and_then(|e| e.get("watering_restrictions"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|r| {
                                r.get("id").and_then(|v| v.as_str()).map(str::to_string)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                choice.set(
                    if ids.iter().any(|i| i == "starter_odd_even") {
                        "odd_even"
                    } else if ids.iter().any(|i| i == "starter_two_days") {
                        "two_days"
                    } else if ids.iter().any(|i| i == "starter_no_midday") {
                        "no_midday"
                    } else if ids.is_empty() {
                        "none"
                    } else {
                        "custom"
                    }
                    .to_string(),
                );
                draft.set(d);
                loaded.set(true);
            }
        });
    });

    // Every change lands in the draft. The starters replace each other;
    // a rule the operator wrote elsewhere ("custom") is left alone.
    Effect::new(move |_| {
        let p = parity.get();
        let c = choice.get();
        if !loaded.get_untracked() {
            return;
        }
        let mut changed = false;
        draft.update(|d| {
            let Some(cfg) = d.get_mut("config").and_then(|c| c.as_object_mut()) else {
                return;
            };
            let dep = cfg.entry("deployment").or_insert(serde_json::json!({}));
            if let Some(obj) = dep.as_object_mut() {
                if obj.get("address_parity").and_then(|v| v.as_str()) != Some(p.as_str()) {
                    obj.insert("address_parity".into(), serde_json::json!(p));
                    changed = true;
                }
            }
            if c == "custom" {
                return;
            }
            let engine = cfg.entry("engine").or_insert(serde_json::json!({}));
            let Some(engine) = engine.as_object_mut() else {
                return;
            };
            let arr = engine
                .entry("watering_restrictions")
                .or_insert(serde_json::json!([]));
            let Some(arr) = arr.as_array_mut() else {
                return;
            };
            let before = arr.clone();
            arr.retain(|r| {
                !r.get("id")
                    .and_then(|v| v.as_str())
                    .is_some_and(|id| id.starts_with("starter_"))
            });
            if let Some(rule) = starter(&c) {
                arr.push(rule);
            }
            if *arr != before {
                changed = true;
            }
        });
        if !changed {
            return;
        }
        let candidate = draft.get_untracked();
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let _ = crate::components::setup::draft::save(&candidate).await;
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = candidate;
    });

    view! {
        <div class="setup-step">
            <h2 class="setup-step__title">"Watering rules "<span class="setup-step__optional">"optional"</span><HelpHint topic="restrictions"/></h2>
            <p class="setup-step__body">
                "Does your water district, city or HOA limit when you can water? "
                "Most do, and LocalSky will keep to it: it never waters on a day or "
                "at an hour the rule forbids, and the week on your dashboard shows "
                "which days are yours. Pick the closest match; you can tune the "
                "exact days and hours later under Settings."
            </p>

            <Panel title="Your house number".to_string()>
                <p class="sensors-section__hint">
                    "Many rules give odd-numbered and even-numbered addresses different days."
                </p>
                <SegmentedControl
                    value=parity
                    options=vec![
                        ("not_applicable".into(), "Not used here".into()),
                        ("odd".into(), "Odd".into()),
                        ("even".into(), "Even".into()),
                    ]
                    aria_label="House number parity".to_string()
                />
            </Panel>

            <Panel title="The rule".to_string()>
                <SegmentedControl
                    value=choice
                    options=vec![
                        ("none".into(), "No rule".into()),
                        ("no_midday".into(), "No midday watering".into()),
                        ("two_days".into(), "Two days a week".into()),
                        ("odd_even".into(), "Odd/even days".into()),
                    ]
                    aria_label="Watering rule".to_string()
                />
                <p class="sensors-section__hint">
                    {move || match choice.get().as_str() {
                        "none" => "LocalSky waters any day the weather allows.".to_string(),
                        "no_midday" => "Any day, never between 10 in the morning and 4 in the afternoon.".to_string(),
                        "two_days" => "Wednesday and Saturday, never between 10 and 4. Change the days later if yours differ.".to_string(),
                        "odd_even" => "Odd addresses water Wednesday and Saturday; even addresses Thursday and Sunday. Pick your house number above.".to_string(),
                        _ => "A rule you wrote yourself is in place and stays as it is.".to_string(),
                    }}
                </p>
            </Panel>

            <SetupFooter
                prev=prev_step_href("rules")
                next=next_step_href("rules")
            />
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_review_row_reads_the_draft() {
        let empty = serde_json::json!({});
        assert_eq!(
            rules_summary(&empty),
            "None. LocalSky waters any day the weather allows."
        );
        let cfg = serde_json::json!({
            "deployment": { "address_parity": "even" },
            "engine": { "watering_restrictions": [starter("odd_even").unwrap()] }
        });
        assert_eq!(
            rules_summary(&cfg),
            "Odd/even address days. Your house number is even."
        );
    }

    /// The starters are the Settings page's own rules.
    #[test]
    fn the_starters_match_settings() {
        let two = starter("two_days").unwrap();
        assert_eq!(two["allowed_weekdays"], serde_json::json!([3, 6]));
        assert!(starter("nope").is_none());
    }
}
