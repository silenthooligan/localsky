// The centralized one-time-notice popup. Nothing sits on the page:
// each notice pops once, the operator decides, and the page is theirs
// again.
//
// Queue, in priority order: the soil-model opt-in offer, the Home
// Assistant helper migration notice, the inferred-watering-target
// notice. ONE notice per page load: the head of the queue pops once,
// its own buttons record the decision and put the overlay away, and
// closing it instead (Escape, backdrop, the X) records nothing, so an
// undecided notice may return next session. Whatever else is queued
// waits for the next full page load rather than taking over the open
// overlay, which is what keeps a second click from landing on a notice
// nobody has read yet.
//
// Decisions persist where each notice keeps its record: the soil offer
// server-side (POST /soil-invite/dismiss, so a dismissal survives
// restarts, other browsers, and other devices, and a snooze returns it
// after 30 days), the two migrated notices in localStorage under their
// original keys and set-keyed semantics, so a dismissal recorded when
// they were page strips still holds.
//
// SSR and the first hydrate frame render nothing: eligibility arrives
// in hydrate-only effects (the snapshot signal, the localStorage
// reads, the soil fetch), so the server DOM and the first client DOM
// match, the sibling-notice hydration discipline. A demo instance
// never pops: the server refuses the soil offer there, and the client
// checks the demo attribute before opening, so screenshot surfaces
// stay clear.

use leptos::prelude::*;

use crate::components::irrigation::default_budget_banner;
use crate::components::irrigation::ha_adoption_banner;
use crate::components::ui::{Button, Sheet};
use crate::components::units_fmt::{use_unit_prefs, UnitPrefs};
use crate::ha::snapshot::IrrigationSnapshot;

/// One popup appearance per page load, across every page that mounts
/// the center. wasm is single-threaded; the atomic is just a static
/// that survives route changes and resets on a full reload.
#[cfg(feature = "hydrate")]
static POPPED_THIS_LOAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What the soil offer says about this yard, from GET /soil-invite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SoilInviteFacts {
    pub deficit_zones: u32,
    pub differs_today: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoticeKind {
    SoilInvite,
    HaAdoption,
    DefaultBudget,
}

/// One queued notice: identity, copy, and its affordances. The key is
/// the same identity each notice's dismissal stores, so silencing one
/// set never silences a different one.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Notice {
    pub kind: NoticeKind,
    pub key: String,
    pub title: &'static str,
    pub lines: Vec<String>,
    pub link_href: &'static str,
    pub link_label: &'static str,
    /// Only the soil offer snoozes; the migrated notices keep their
    /// original dismiss-only semantics.
    pub can_snooze: bool,
}

const SOIL_TITLE: &str = "Water each zone by its own soil";
const ADOPTION_TITLE: &str = "Home Assistant helpers";
const BUDGET_TITLE: &str = "Watering targets";

/// What opting in changes, in one sentence, action first.
const SOIL_OPT_IN_LINE: &str = "Opting in waters each zone when its own soil runs dry and refills \
     what that zone can hold, in place of the fixed weekly split.";

/// What the shadow shows on this yard: the zone count carrying a live
/// deficit right now, zero included (a yard reading full is still an
/// answer, not an omission).
fn soil_deficit_line(deficit_zones: u32) -> String {
    match deficit_zones {
        0 => "The soil model has been tracking this yard and reads every zone full or close to \
              it right now."
            .to_string(),
        1 => "The soil model has been tracking this yard and shows a live soil deficit on 1 zone \
              right now."
            .to_string(),
        n => format!(
            "The soil model has been tracking this yard and shows a live soil deficit on {n} \
             zones right now."
        ),
    }
}

/// The comparison, where the yard has one to show: zones where the
/// soil model and the weekly schedule disagree about watering today.
fn soil_differ_line(differs_today: u32) -> Option<String> {
    match differs_today {
        0 => None,
        1 => Some(
            "It would have watered differently than the weekly schedule on 1 zone today."
                .to_string(),
        ),
        n => Some(format!(
            "It would have watered differently than the weekly schedule on {n} zones today."
        )),
    }
}

fn soil_lines(f: SoilInviteFacts) -> Vec<String> {
    let mut lines = vec![soil_deficit_line(f.deficit_zones)];
    lines.extend(soil_differ_line(f.differs_today));
    lines.push(SOIL_OPT_IN_LINE.to_string());
    lines
}

/// The queue, in priority order, minus everything decided. Pure so the
/// ordering and the drop-on-decision rules are pinnable.
pub(crate) fn build_queue(
    soil: Option<SoilInviteFacts>,
    soil_decided: bool,
    snap: &IrrigationSnapshot,
    prefs: UnitPrefs,
    adoption_dismissed: &str,
    budget_dismissed: &str,
) -> Vec<Notice> {
    let mut q = Vec::new();
    if let Some(f) = soil {
        if !soil_decided {
            q.push(Notice {
                kind: NoticeKind::SoilInvite,
                key: "soil_invite".to_string(),
                title: SOIL_TITLE,
                lines: soil_lines(f),
                link_href: "/settings/engine",
                link_label: "Open Engine settings",
                can_snooze: true,
            });
        }
    }
    if ha_adoption_banner::worth_showing(snap) {
        let key = ha_adoption_banner::adopted_key(snap);
        if key != adoption_dismissed {
            q.push(Notice {
                kind: NoticeKind::HaAdoption,
                key,
                title: ADOPTION_TITLE,
                lines: ha_adoption_banner::lines(snap),
                link_href: "/settings/skip-rules",
                link_label: "Open Settings",
                can_snooze: false,
            });
        }
    }
    let budget_lines = default_budget_banner::lines(snap, prefs);
    if !budget_lines.is_empty() {
        let key = default_budget_banner::inferred_key(snap);
        if key != budget_dismissed {
            q.push(Notice {
                kind: NoticeKind::DefaultBudget,
                key,
                title: BUDGET_TITLE,
                lines: budget_lines,
                link_href: "/settings/zones",
                link_label: "Set targets",
                can_snooze: false,
            });
        }
    }
    q
}

/// The demo attribute the app root stamps on <html>. Checked right
/// before the popup opens; the server refuses the soil offer on a demo
/// instance as well, so this is the second gate, not the only one.
#[cfg(feature = "hydrate")]
fn demo_mode() -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
        .map(|e| e.get_attribute("data-demo").as_deref() == Some("true"))
        .unwrap_or(false)
}

#[component]
pub fn NoticeCenter(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    let soil: RwSignal<Option<SoilInviteFacts>> = RwSignal::new(None);
    let soil_decided = RwSignal::new(false);
    let adoption_dismissed: RwSignal<String> = RwSignal::new(String::new());
    let budget_dismissed: RwSignal<String> = RwSignal::new(String::new());
    let open = RwSignal::new(false);
    let busy = RwSignal::new(false);
    // This mount already opened the overlay once; content may advance
    // inside it but a closed overlay stays closed.
    let popped_this_mount = StoredValue::new(false);

    // Hydrate-only arm: the stored dismissals, then the soil offer's one
    // read. Nothing here reads a reactive signal, so it runs once.
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        adoption_dismissed.set(ha_adoption_banner::read_dismissed());
        budget_dismissed.set(default_budget_banner::read_dismissed());
        leptos::task::spawn_local(async move {
            let Ok(resp) = gloo_net::http::Request::get("/api/v1/irrigation/soil-invite")
                .send()
                .await
            else {
                return;
            };
            // 404 = no history database (routes unregistered): the offer
            // cannot keep a dismissal, so it is not made.
            if !resp.ok() {
                return;
            }
            let Ok(v) = resp.json::<serde_json::Value>().await else {
                return;
            };
            if v.get("eligible").and_then(|b| b.as_bool()) != Some(true)
                || v.get("state").and_then(|s| s.as_str()) != Some("open")
            {
                return;
            }
            let facts = SoilInviteFacts {
                deficit_zones: v.get("deficit_zones").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
                differs_today: v.get("differs_today").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
            };
            let _ = soil.try_set(Some(facts));
        });
    });

    let queue = Memo::new(move |_| {
        build_queue(
            soil.get(),
            soil_decided.get(),
            &snap.get(),
            prefs.get(),
            &adoption_dismissed.get(),
            &budget_dismissed.get(),
        )
    });

    // The whole notice is COPIED when it pops, not looked up again while
    // the overlay is open: a later snapshot poll, an offer whose fetch
    // resolves late, or an identity key that drifts under a live update
    // can never swap the words under someone mid-read, and a click can
    // never land on a notice that took the button's place.
    let active: RwSignal<Option<Notice>> = RwSignal::new(None);

    // Pop control. Opens once per page load (the static) and once per
    // mount (the StoredValue), showing the head of the queue and nothing
    // after it: whatever else is queued waits for the next full load.
    Effect::new(move |_| {
        let head = queue.get().into_iter().next();
        if active.with_untracked(|a| a.is_some()) {
            return;
        }
        let Some(n) = head else {
            return;
        };
        #[cfg(feature = "hydrate")]
        {
            if !popped_this_mount.get_value()
                && !demo_mode()
                && !POPPED_THIS_LOAD.load(std::sync::atomic::Ordering::Relaxed)
            {
                POPPED_THIS_LOAD.store(true, std::sync::atomic::Ordering::Relaxed);
                popped_this_mount.set_value(true);
                active.set(Some(n));
                open.set(true);
            }
        }
        #[cfg(not(feature = "hydrate"))]
        {
            let _ = (popped_this_mount, n);
        }
    });

    // One decision path for all three notices. The migrated two keep
    // their original localStorage keying; the soil offer posts its
    // choice server-side and only closes on a confirmed write, so a
    // failed save cannot look like a dismissal that later refires.
    // Every decision puts the overlay away: the page is the operator's
    // again, and anything still queued waits for the next page load.
    let decide = Callback::new(
        move |(kind, dismiss_kind): (NoticeKind, &'static str)| match kind {
            NoticeKind::SoilInvite => {
                #[cfg(not(feature = "hydrate"))]
                let _ = (dismiss_kind, busy, soil_decided);
                #[cfg(feature = "hydrate")]
                {
                    if busy.get_untracked() {
                        return;
                    }
                    busy.set(true);
                    let body = serde_json::json!({ "kind": dismiss_kind });
                    leptos::task::spawn_local(async move {
                        let outcome: Result<(), (u16, String)> = async {
                            let resp = gloo_net::http::Request::post(
                                "/api/v1/irrigation/soil-invite/dismiss",
                            )
                            .json(&body)
                            .map_err(|e| (0u16, e.to_string()))?
                            .send()
                            .await
                            .map_err(|e| (0u16, e.to_string()))?;
                            if !resp.ok() {
                                let status = resp.status();
                                let text = resp.text().await.unwrap_or_default();
                                return Err((status, text));
                            }
                            Ok(())
                        }
                        .await;
                        let _ = busy.try_set(false);
                        match outcome {
                            Ok(()) => {
                                if dismiss_kind == "snooze" {
                                    crate::components::ui::use_toast()
                                        .success("Snoozed for 30 days.");
                                } else {
                                    crate::components::ui::use_toast()
                                        .success("Dismissed. This offer will not return.");
                                }
                                let _ = soil_decided.try_set(true);
                                let _ = open.try_set(false);
                            }
                            Err((status, text)) => {
                                crate::components::ui::use_toast().error(
                                    crate::components::settings_ui::save_error_message(
                                        status, &text,
                                    ),
                                );
                            }
                        }
                    });
                }
            }
            NoticeKind::HaAdoption => {
                let key = ha_adoption_banner::adopted_key(&snap.get_untracked());
                ha_adoption_banner::store_dismissed(&key);
                adoption_dismissed.set(key);
                open.set(false);
            }
            NoticeKind::DefaultBudget => {
                let key = default_budget_banner::inferred_key(&snap.get_untracked());
                default_budget_banner::store_dismissed(&key);
                budget_dismissed.set(key);
                open.set(false);
            }
        },
    );

    view! {
        {move || {
            active
                .get()
                .map(|n| {
                    let kind = n.kind;
                    let on_dismiss = move |_| decide.run((kind, "permanent"));
                    let on_snooze = move |_| decide.run((kind, "snooze"));
                    view! {
                        <Sheet open=open title=n.title aria_label=n.title>
                            {n
                                .lines
                                .iter()
                                .map(|l| view! { <p class="notice-popup__line">{l.clone()}</p> })
                                .collect_view()}
                            <div class="notice-popup__actions">
                                <Button variant="primary" href=n.link_href>{n.link_label}</Button>
                                {n
                                    .can_snooze
                                    .then(|| {
                                        view! {
                                            <Button
                                                variant="secondary"
                                                loading=Signal::derive(move || busy.get())
                                                on_click=Callback::new(on_snooze)
                                            >"Snooze 30 days"</Button>
                                        }
                                    })}
                                <Button
                                    variant="secondary"
                                    loading=Signal::derive(move || busy.get())
                                    on_click=Callback::new(on_dismiss)
                                >"Dismiss"</Button>
                            </div>
                        </Sheet>
                    }
                })
        }}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::snapshot::{HaAdoptedHelper, WaterBudget};

    // ---- The soil offer's copy, pinned verbatim ----

    #[test]
    fn the_deficit_line_names_the_count_and_zero_is_an_answer() {
        assert_eq!(
            soil_deficit_line(3),
            "The soil model has been tracking this yard and shows a live soil deficit on 3 \
             zones right now."
        );
        assert_eq!(
            soil_deficit_line(1),
            "The soil model has been tracking this yard and shows a live soil deficit on 1 zone \
             right now."
        );
        assert_eq!(
            soil_deficit_line(0),
            "The soil model has been tracking this yard and reads every zone full or close to \
             it right now."
        );
    }

    #[test]
    fn the_comparison_line_shows_only_where_the_yard_has_one() {
        assert_eq!(soil_differ_line(0), None);
        assert_eq!(
            soil_differ_line(1).unwrap(),
            "It would have watered differently than the weekly schedule on 1 zone today."
        );
        assert_eq!(
            soil_differ_line(2).unwrap(),
            "It would have watered differently than the weekly schedule on 2 zones today."
        );
    }

    #[test]
    fn the_opt_in_sentence_is_one_sentence_action_first() {
        assert_eq!(
            SOIL_OPT_IN_LINE,
            "Opting in waters each zone when its own soil runs dry and refills what that zone \
             can hold, in place of the fixed weekly split."
        );
        let lines = soil_lines(SoilInviteFacts {
            deficit_zones: 2,
            differs_today: 1,
        });
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("2 zones"));
        assert!(lines[1].contains("1 zone today"));
        assert_eq!(lines[2], SOIL_OPT_IN_LINE);
    }

    // ---- The queue ----

    fn snap_with_both_legacy_notices() -> IrrigationSnapshot {
        let mut s = IrrigationSnapshot::default();
        s.water_budgets = vec![WaterBudget {
            zone_slug: "back_yard".into(),
            zone_name: "Back Yard".into(),
            weekly_budget_in: 1.0,
            sessions_per_week: 2,
            target_inferred: true,
            ..Default::default()
        }];
        s.ha_adoption = vec![HaAdoptedHelper {
            entity: "input_number.irrigation_max_wind_mph".into(),
            outcome: "adopted".into(),
            target: String::new(),
            adopted_value: Some("12".into()),
            observed_value: None,
            previous_value: Some("10".into()),
            epoch: 1,
        }];
        s
    }

    /// Priority order: the soil offer first, then the migration notice,
    /// then the target notice; each queues under its own identity key.
    #[test]
    fn the_queue_orders_soil_then_adoption_then_budget() {
        let s = snap_with_both_legacy_notices();
        let q = build_queue(
            Some(SoilInviteFacts::default()),
            false,
            &s,
            UnitPrefs::default(),
            "",
            "",
        );
        assert_eq!(
            q.iter().map(|n| n.kind).collect::<Vec<_>>(),
            vec![
                NoticeKind::SoilInvite,
                NoticeKind::HaAdoption,
                NoticeKind::DefaultBudget
            ]
        );
        assert_eq!(q[0].key, "soil_invite");
        assert!(q[0].can_snooze);
        assert!(!q[1].can_snooze && !q[2].can_snooze);
        assert_eq!(q[0].link_href, "/settings/engine");
        assert_eq!(q[1].link_href, "/settings/skip-rules");
        assert_eq!(q[2].link_href, "/settings/zones");
    }

    /// Deciding drops exactly the decided notice: the soil offer on its
    /// decided flag, the migrated two on their original stored keys.
    #[test]
    fn a_decision_drops_only_the_decided_notice() {
        let s = snap_with_both_legacy_notices();
        let adoption_key = ha_adoption_banner::adopted_key(&s);
        let budget_key = default_budget_banner::inferred_key(&s);

        let q = build_queue(
            Some(SoilInviteFacts::default()),
            true,
            &s,
            UnitPrefs::default(),
            "",
            "",
        );
        assert_eq!(
            q.iter().map(|n| n.kind).collect::<Vec<_>>(),
            vec![NoticeKind::HaAdoption, NoticeKind::DefaultBudget]
        );

        let q = build_queue(
            None,
            false,
            &s,
            UnitPrefs::default(),
            &adoption_key,
            &budget_key,
        );
        assert!(q.is_empty(), "{q:?}");

        // A stale stored key (a different set was dismissed) silences
        // nothing, the original strip semantics.
        let q = build_queue(None, false, &s, UnitPrefs::default(), "other", "other");
        assert_eq!(q.len(), 2);
    }

    /// No soil offer fetched (ineligible, snoozed, dismissed, or no
    /// history database) and a quiet snapshot: nothing queues, nothing
    /// pops.
    #[test]
    fn a_quiet_install_queues_nothing() {
        let s = IrrigationSnapshot::default();
        assert!(build_queue(None, false, &s, UnitPrefs::default(), "", "").is_empty());
    }
}
