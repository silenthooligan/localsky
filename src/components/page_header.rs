// Global page header. Rendered once in app.rs above <Routes/>, so it
// appears on every route. Currently a single right-aligned segmented
// control: the Simple/Nerd mode toggle.
//
// v2: dual segments instead of a flip-toggle. A single pill that
// flipped between "Simple" and "Nerd" hid the fact that there were
// two states; making both labels visible at once communicates that
// the user is choosing between modes, with the active mode lit and
// the other dimmed. Clicking either segment switches.

use crate::app::NerdMode;
use leptos::prelude::*;

#[component]
pub fn PageHeader() -> impl IntoView {
    let nerd_mode = use_context::<NerdMode>()
        .map(|n| n.0)
        .unwrap_or_else(|| RwSignal::new(false));

    let set_simple = move |_| nerd_mode.set(false);
    let set_nerd = move |_| nerd_mode.set(true);

    let simple_class = move || {
        if !nerd_mode.get() {
            "mode-toggle__seg is-active"
        } else {
            "mode-toggle__seg"
        }
    };
    let nerd_class = move || {
        if nerd_mode.get() {
            "mode-toggle__seg is-active"
        } else {
            "mode-toggle__seg"
        }
    };

    view! {
        <div class="page-header" aria-label="Page header">
            <div class="mode-toggle" role="group" aria-label="Display mode">
                <span class="mode-toggle__label">"Mode"</span>
                <button
                    type="button"
                    class=simple_class
                    on:click=set_simple
                    aria-pressed=move || if !nerd_mode.get() { "true" } else { "false" }
                    title="Simple mode: show only the verdict and the rules currently blocking, hide engine math"
                >
                    "Simple"
                </button>
                <button
                    type="button"
                    class=nerd_class
                    on:click=set_nerd
                    aria-pressed=move || if nerd_mode.get() { "true" } else { "false" }
                    title="Nerd mode: surface ET0, ETc, Kc, bucket depth, and every skip-check input"
                >
                    "Nerd"
                </button>
            </div>
        </div>
    }
}
