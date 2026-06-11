// In-page nav debug log. A small ring buffer of recent step strings
// rendered in a fixed strip at the bottom of every page, so a user
// on a phone (where dev tools is awkward) can see exactly what
// happens when they tap a route tab, handler fire, prevent_default,
// navigate call, return, route render, without leaving the page.
//
// Wire-up: app.rs creates a single (read, write) signal at top
// level, provides the ReadSignal as context for the rendered
// strip, and stashes the WriteSignal in a thread-local that
// `log_nav()` here writes to. Components anywhere in the tree
// call `log_nav("step description")`.
//
// Runtime cost: a Vec<String> push + truncate to 5 + a signal
// notification per call. Cheap. SSR build is a no-op (the cell
// is never set on the server).

use leptos::prelude::*;
use std::cell::RefCell;

const MAX_LINES: usize = 8;

thread_local! {
    static SINK: RefCell<Option<WriteSignal<Vec<String>>>> = const { RefCell::new(None) };
}

/// Wire up the global sink. Called once from App::() during setup,
/// passing the WriteSignal half of the debug log signal.
pub fn install_sink(setter: WriteSignal<Vec<String>>) {
    SINK.with(|s| *s.borrow_mut() = Some(setter));
}

/// Push a debug line into the visible strip. No-op on SSR (the sink
/// hasn't been installed) so adding calls in component code is safe
/// regardless of where it runs.
pub fn log_nav(msg: impl Into<String>) {
    let msg = msg.into();
    SINK.with(|s| {
        if let Some(setter) = s.borrow().as_ref() {
            let stamped = format!("{} · {}", short_now(), msg);
            setter.update(|v| {
                v.push(stamped);
                let len = v.len();
                if len > MAX_LINES {
                    v.drain(0..len - MAX_LINES);
                }
            });
        }
    });
}

#[cfg(feature = "hydrate")]
fn short_now() -> String {
    let ms = js_sys::Date::now();
    let total_secs = (ms as i64) / 1000;
    let h = (total_secs / 3600) % 24;
    let m = (total_secs / 60) % 60;
    let s = total_secs % 60;
    let ms_part = (ms as i64) % 1000;
    format!("{:02}:{:02}:{:02}.{:03}", h, m, s, ms_part)
}

#[cfg(not(feature = "hydrate"))]
fn short_now() -> String {
    String::new()
}

/// Strip rendered at the bottom of the page. Reads from the
/// ReadSignal half (provide_context'd from App::()).
#[component]
pub fn NavLogStrip() -> impl IntoView {
    let log = use_context::<ReadSignal<Vec<String>>>();
    view! {
        <div class="nav-log-strip" aria-live="polite">
            {move || log.map(|l| l.get()).unwrap_or_default()
                .into_iter()
                .map(|line| view! { <div class="nav-log-line">{line}</div> })
                .collect::<Vec<_>>()}
        </div>
    }
}
