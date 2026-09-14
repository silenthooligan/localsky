// Skeleton that stops pretending. The forecast panels shimmer while the
// snapshot is empty, which is right for the seconds a normal boot takes,
// but during a provider outage on a fresh store the shimmer ran FOREVER
// and read as a broken page (an actual field report guessed "hydration
// bug"). This wrapper renders the normal skeleton for a grace period,
// then upgrades to an honest "provider hasn't answered, retrying" note.
// The parent only mounts it while the data is empty, so real data still
// replaces it the instant a fetch lands.
//
// With no location configured there is no provider to wait on: the note
// says so at once and points at setup, and never blames a provider that
// was never asked.

use leptos::prelude::*;

/// Seconds of empty-snapshot shimmer before the honest note. A healthy
/// boot completes its first forecast fetch well inside this window.
/// (Referenced from the hydrate-only timer, so the ssr build sees it
/// as dead code without the cfg.)
#[cfg(feature = "hydrate")]
const GRACE_SECS: u64 = 12;

/// The note the panel shows once it stops shimmering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingNote {
    pub title: String,
    pub body: &'static str,
    pub link_href: &'static str,
    pub link_label: &'static str,
}

/// What to render for an empty forecast panel. `None` is the skeleton.
/// `located` is `/api/v1/info`'s answer (None while it is in flight).
pub fn pending_note(what: &str, located: Option<bool>, waited: bool) -> Option<PendingNote> {
    let title = format!("No {what} yet");
    match located {
        Some(false) => Some(PendingNote {
            title,
            body: "LocalSky does not know where the yard is yet, so there is nothing to \
                   forecast. Set a location and this panel fills in about a minute later.",
            link_href: "/setup",
            link_label: "Set a location",
        }),
        _ if waited => Some(PendingNote {
            title,
            body: "The forecast provider hasn't answered since LocalSky started. It retries \
                   automatically and this panel fills in as soon as data arrives.",
            link_href: "/settings?section=devices",
            link_label: "Check sources",
        }),
        _ => None,
    }
}

#[component]
pub fn ForecastPending(
    /// Skeleton shape during the grace period: "chart" (one chart ghost),
    /// "blocks" (7 block cards), or "tiles" (7 grid tiles).
    #[prop(into)]
    variant: String,
    /// What the panel is waiting for, e.g. "hourly forecast". Rendered as
    /// "No hourly forecast yet".
    #[prop(into)]
    what: String,
) -> impl IntoView {
    let (waited, set_waited) = signal(false);
    // Client-only: effects never run during SSR, so the server and first
    // paint always agree on the skeleton (no hydration mismatch).
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        set_timeout(
            move || set_waited.set(true),
            std::time::Duration::from_secs(GRACE_SECS),
        );
    });
    #[cfg(not(feature = "hydrate"))]
    let _ = &set_waited;
    // None on SSR and on the first client frame, so both render the
    // skeleton; the deferred info fetch settles it afterwards.
    let located = use_context::<crate::app::Located>().map(|l| l.0);

    move || {
        let located = located.and_then(|l| l.get());
        if let Some(note) = pending_note(&what, located, waited.get()) {
            view! {
                <div class="forecast-pending" role="status">
                    <span class="forecast-pending__title">{note.title}</span>
                    <span class="forecast-pending__body">{note.body}</span>
                    <a class="forecast-pending__link" href=note.link_href>{note.link_label}</a>
                </div>
            }
            .into_any()
        } else {
            // display:contents on the wrapper promotes the ghosts to
            // children of the surrounding grid/flex rail, so the 7-col
            // layouts size them exactly like the real cells.
            let ghosts: Vec<_> = match variant.as_str() {
                "chart" => vec![view! { <crate::components::ui::Skeleton variant="chart"/> }],
                "tiles" => (0..7)
                    .map(|_| view! { <crate::components::ui::Skeleton variant="tile"/> })
                    .collect(),
                _ => (0..7)
                    .map(|_| view! { <crate::components::ui::Skeleton variant="block"/> })
                    .collect(),
            };
            view! { <div class="forecast-pending-ghosts">{ghosts}</div> }.into_any()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no location there is no provider to blame, before or after
    /// the grace period.
    #[test]
    fn an_unlocated_install_never_blames_the_provider() {
        for waited in [false, true] {
            let note = pending_note("hourly forecast", Some(false), waited)
                .expect("the note shows at once, no grace period");
            let text = format!("{} {} {}", note.title, note.body, note.link_label);
            assert!(!text.to_lowercase().contains("provider"), "{text}");
            assert_eq!(note.link_href, "/setup");
            assert_eq!(note.title, "No hourly forecast yet");
        }
    }

    /// A located install keeps the skeleton through the grace period and
    /// then names the provider.
    #[test]
    fn a_located_install_waits_then_says_so() {
        assert_eq!(pending_note("7-day forecast", Some(true), false), None);
        assert_eq!(pending_note("7-day forecast", None, false), None);
        let note = pending_note("7-day forecast", Some(true), true).unwrap();
        assert!(note.body.contains("provider"));
        assert_eq!(note.link_href, "/settings?section=devices");
    }
}
