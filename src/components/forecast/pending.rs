// Skeleton that stops pretending. The forecast panels shimmer while the
// snapshot is empty, which is right for the seconds a normal boot takes,
// but during a provider outage on a fresh store the shimmer ran FOREVER
// and read as a broken page (an actual field report guessed "hydration
// bug"). This wrapper renders the normal skeleton for a grace period,
// then upgrades to an honest "provider hasn't answered, retrying" note.
// The parent only mounts it while the data is empty, so real data still
// replaces it the instant a fetch lands.

use leptos::prelude::*;

/// Seconds of empty-snapshot shimmer before the honest note. A healthy
/// boot completes its first forecast fetch well inside this window.
/// (Referenced from the hydrate-only timer, so the ssr build sees it
/// as dead code without the cfg.)
#[cfg(feature = "hydrate")]
const GRACE_SECS: u64 = 12;

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

    move || {
        if waited.get() {
            view! {
                <div class="forecast-pending" role="status">
                    <span class="forecast-pending__title">{format!("No {what} yet")}</span>
                    <span class="forecast-pending__body">
                        "The forecast provider hasn't answered since LocalSky started. It retries automatically and this panel fills in as soon as data arrives."
                    </span>
                    <a class="forecast-pending__link" href="/settings/devices">"Check sources"</a>
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
