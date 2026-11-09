// GeoSearch. The one address box both the setup wizard's Location step
// and Settings -> Location render. Type a place and press Enter (or
// Search), pick a hit, or share the device's own position; the host gets
// a GeoPick and decides what to do with the coordinates (the wizard
// persists them into the draft, Settings waits for Save).
//
// Explicit search, not type-ahead, on purpose: the server relays to the
// public Nominatim service, whose usage policy forbids autocomplete and
// caps callers at one request per second. What the box owes the operator
// instead is to never go quiet. An empty answer, a rate limit, a service
// failure and a refused permission each say so here, in words, and stale
// results clear the moment the query changes. The first version parsed
// every non-success to an empty list, so after one 429 the box looked
// dead for a minute with nothing to explain why.

use leptos::prelude::*;

use crate::components::ui::{Button, FormField};

/// What the host receives when the operator picks a hit or shares the
/// device position.
#[derive(Clone, Debug, PartialEq)]
pub struct GeoPick {
    pub lat: f64,
    pub lon: f64,
    /// The hit's label: a Nominatim display name, a Census matched
    /// address, or [`DEVICE_LOCATION_LABEL`].
    pub label: String,
}

/// Label a device-position pick carries.
pub const DEVICE_LOCATION_LABEL: &str = "This device's location";
/// Label a map-pin pick carries.
pub const MAP_PIN_LABEL: &str = "Pinned on the map";
pub const PINNED: &str =
    "Pin placed. Check the coordinates below; drag the pin or tap the map again to adjust.";

// Status copy. STRAIGHT voice (see `voice`): someone reading these is
// trying to tell LocalSky where their yard is and has just been refused.
pub const NO_MATCHES: &str = "No matches. Try the street or landmark on its own, add the ZIP or \
     postal code, or type the coordinates below. For a brand-new street in the US, include a \
     house number.";
pub const RATE_LIMITED: &str = "Too many searches in the last minute. Wait a moment and try again.";
pub const SEARCH_FORBIDDEN: &str =
    "Address search works from your own network or after signing in.";
pub const SERVICE_DOWN: &str = "The address service did not answer. Try again in a moment, or \
     type the coordinates below.";
pub const UNREACHABLE: &str = "Could not reach LocalSky. Check the connection and try again.";
pub const LOCATING: &str = "Asking your browser for this device's location…";
pub const LOCATED: &str = "Using this device's location. Check the coordinates below and adjust \
     them if the fix looks off.";
pub const NEEDS_SECURE_CONTEXT: &str = "Browsers only share location over HTTPS or on localhost. \
     Open LocalSky that way, or search for the address instead.";
pub const GEO_DENIED: &str = "Location permission was denied. Allow it in the browser, or search \
     for the address instead. Inside Home Assistant, open LocalSky in its own tab first.";
pub const GEO_UNAVAILABLE: &str =
    "This device could not work out where it is. Search for the address instead.";
pub const GEO_TIMEOUT: &str =
    "Finding this device's location took too long. Try again, or search for the address.";
pub const GEO_UNSUPPORTED: &str =
    "This browser cannot share its location. Search for the address instead.";

/// One candidate as the relay returns it. `source` is `osm` or `census`.
#[derive(Clone, Debug, PartialEq)]
struct GeoHit {
    label: String,
    lat: f64,
    lon: f64,
    source: String,
}

#[derive(Clone, Debug, PartialEq)]
struct GeoStatus {
    text: String,
    error: bool,
}

fn source_label(source: &str) -> &'static str {
    match source {
        "census" => "US Census",
        _ => "OpenStreetMap",
    }
}

/// How one search ended, from the browser's side of the relay.
#[cfg(feature = "hydrate")]
enum SearchOutcome {
    Hits(Vec<GeoHit>),
    RateLimited,
    Forbidden,
    Down,
    Unreachable,
}

#[cfg(feature = "hydrate")]
fn parse_hits(v: &serde_json::Value) -> Vec<GeoHit> {
    // Nominatim sends coordinates as strings; accept numbers too so a
    // future relay change cannot blank the list.
    fn coord(v: &serde_json::Value) -> Option<f64> {
        v.as_f64().or_else(|| v.as_str()?.trim().parse().ok())
    }
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    Some(GeoHit {
                        label: r.get("display_name")?.as_str()?.to_string(),
                        lat: coord(r.get("lat")?)?,
                        lon: coord(r.get("lon")?)?,
                        source: r
                            .get("source")
                            .and_then(|s| s.as_str())
                            .unwrap_or("osm")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(feature = "hydrate")]
async fn run_search(q: &str) -> SearchOutcome {
    let url = format!("/api/wizard/geocode?q={}", crate::text::query_value(q));
    let Ok(resp) = gloo_net::http::Request::get(&url).send().await else {
        return SearchOutcome::Unreachable;
    };
    if resp.ok() {
        return match resp.json::<serde_json::Value>().await {
            Ok(v) => SearchOutcome::Hits(parse_hits(&v)),
            Err(_) => SearchOutcome::Down,
        };
    }
    match resp.status() {
        429 => SearchOutcome::RateLimited,
        401 | 403 => SearchOutcome::Forbidden,
        _ => SearchOutcome::Down,
    }
}

#[component]
pub fn GeoSearch(
    /// Hold the box closed until the host's own data has loaded, so a pick
    /// cannot land before the load overwrites it.
    #[prop(into, optional)]
    disabled: Signal<bool>,
    /// The host's current coordinates, so the map pin follows a search hit,
    /// the device position or a typed value. None while unset.
    #[prop(into, optional)]
    position: Signal<Option<(f64, f64)>>,
    /// Fired when the operator picks a hit, the device reports where it is,
    /// or the map pin lands.
    on_pick: Callback<GeoPick>,
) -> impl IntoView {
    let query = RwSignal::new(String::new());
    let searching = RwSignal::new(false);
    let locating = RwSignal::new(false);
    let results: RwSignal<Vec<GeoHit>> = RwSignal::new(Vec::new());
    let status: RwSignal<Option<GeoStatus>> = RwSignal::new(None);
    // "Pick on map": closed until asked, so no tile is fetched for anyone
    // who never opens it. The canvas is recreated on each open.
    let map_open = RwSignal::new(false);
    let map_ref = NodeRef::<leptos::html::Div>::new();

    let say = move |text: &str, error: bool| {
        status.set(Some(GeoStatus {
            text: text.to_string(),
            error,
        }));
    };

    // The map's pin, when the operator moves it. geo-picker.js dispatches
    // a bubbling `geo-pick` CustomEvent on the canvas; the listener is
    // attached once per canvas (the effect re-runs on the NodeRef, which
    // changes each time the map opens).
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        use wasm_bindgen::{closure::Closure, JsCast, JsValue};
        let Some(canvas) = map_ref.get() else {
            return;
        };
        let on_geo_pick =
            Closure::<dyn FnMut(web_sys::CustomEvent)>::new(move |ev: web_sys::CustomEvent| {
                let detail = ev.detail();
                let read = |key: &str| {
                    js_sys::Reflect::get(&detail, &JsValue::from_str(key))
                        .ok()
                        .and_then(|v| v.as_f64())
                };
                let (Some(lat), Some(lon)) = (read("lat"), read("lon")) else {
                    return;
                };
                // A pin after the step unmounted has nowhere to go; the
                // signal's disposal is the gate (a disposed Callback panics).
                if status
                    .try_set(Some(GeoStatus {
                        text: PINNED.to_string(),
                        error: false,
                    }))
                    .is_some()
                {
                    return;
                }
                let _ = results.try_set(Vec::new());
                on_pick.run(GeoPick {
                    lat,
                    lon,
                    label: MAP_PIN_LABEL.to_string(),
                });
            });
        let _ = canvas
            .add_event_listener_with_callback("geo-pick", on_geo_pick.as_ref().unchecked_ref());
        // Lives as long as the canvas; retired with it when the map closes.
        on_geo_pick.forget();
    });

    let on_search = move || {
        let q = query.get_untracked().trim().to_string();
        if q.is_empty() || searching.get_untracked() || disabled.get_untracked() {
            return;
        }
        searching.set(true);
        results.set(Vec::new());
        status.set(None);
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let outcome = run_search(&q).await;
            // The answer belongs to the query that was in the box when the
            // search started. If the box moved on, the operator has too.
            let still_wanted = query.try_get_untracked().is_some_and(|now| now.trim() == q);
            if still_wanted {
                match outcome {
                    SearchOutcome::Hits(hits) if hits.is_empty() => say(NO_MATCHES, false),
                    SearchOutcome::Hits(hits) => {
                        let _ = results.try_set(hits);
                    }
                    SearchOutcome::RateLimited => say(RATE_LIMITED, true),
                    SearchOutcome::Forbidden => say(SEARCH_FORBIDDEN, true),
                    SearchOutcome::Down => say(SERVICE_DOWN, true),
                    SearchOutcome::Unreachable => say(UNREACHABLE, true),
                }
            }
            let _ = searching.try_set(false);
        });
        #[cfg(not(feature = "hydrate"))]
        {
            let _ = q;
            searching.set(false);
        }
    };

    let on_locate = move || {
        if locating.get_untracked() || disabled.get_untracked() {
            return;
        }
        // No browser on the server; the click never fires there, but the
        // closure still has to be whole.
        #[cfg(not(feature = "hydrate"))]
        say(GEO_UNSUPPORTED, true);
        #[cfg(feature = "hydrate")]
        {
            use wasm_bindgen::closure::Closure;
            use wasm_bindgen::JsCast;

            let Some(window) = web_sys::window() else {
                say(GEO_UNSUPPORTED, true);
                return;
            };
            // Geolocation is a secure-context API. A LAN http:// install
            // gets a plain explanation instead of a silent no-op.
            if !window.is_secure_context() {
                say(NEEDS_SECURE_CONTEXT, true);
                return;
            }
            let Ok(geolocation) = window.navigator().geolocation() else {
                say(GEO_UNSUPPORTED, true);
                return;
            };
            locating.set(true);
            results.set(Vec::new());
            say(LOCATING, false);
            // once_into_js: the browser calls exactly one of the pair and
            // that one frees itself; the other is a few bytes per click.
            let on_ok = Closure::once_into_js(move |pos: web_sys::Position| {
                let coords = pos.coords();
                // Five decimals is about a meter; more is GPS noise
                // dressed up as precision.
                let lat = (coords.latitude() * 1e5).round() / 1e5;
                let lon = (coords.longitude() * 1e5).round() / 1e5;
                // The fix can arrive after the step unmounted (navigation
                // mid-prompt); a disposed callback panics when run, so
                // the signal's own disposal is the gate.
                if locating.try_set(false).is_some() {
                    return;
                }
                let _ = status.try_set(Some(GeoStatus {
                    text: LOCATED.to_string(),
                    error: false,
                }));
                on_pick.run(GeoPick {
                    lat,
                    lon,
                    label: DEVICE_LOCATION_LABEL.to_string(),
                });
            });
            let on_err = Closure::once_into_js(move |err: web_sys::PositionError| {
                let _ = locating.try_set(false);
                // PERMISSION_DENIED = 1, POSITION_UNAVAILABLE = 2, TIMEOUT = 3.
                let text = match err.code() {
                    1 => GEO_DENIED,
                    3 => GEO_TIMEOUT,
                    _ => GEO_UNAVAILABLE,
                };
                let _ = status.try_set(Some(GeoStatus {
                    text: text.to_string(),
                    error: true,
                }));
            });
            let options = web_sys::PositionOptions::new();
            options.set_enable_high_accuracy(true);
            options.set_timeout(20_000);
            options.set_maximum_age(60_000);
            if geolocation
                .get_current_position_with_error_callback_and_options(
                    on_ok.unchecked_ref(),
                    Some(on_err.unchecked_ref()),
                    &options,
                )
                .is_err()
            {
                locating.set(false);
                say(GEO_UNSUPPORTED, true);
            }
        }
    };

    let results_view = move || {
        results
            .get()
            .into_iter()
            .map(|hit| {
                let GeoHit {
                    label,
                    lat,
                    lon,
                    source,
                } = hit;
                let pick_label = label.clone();
                view! {
                    <button
                        type="button"
                        class="geo-result"
                        on:click=move |_| {
                            results.set(Vec::new());
                            status.set(None);
                            on_pick.run(GeoPick { lat, lon, label: pick_label.clone() });
                        }
                    >
                        <span class="geo-result__name">{label}</span>
                        <span class="geo-result__source">{source_label(&source)}</span>
                        <span class="geo-result__coords">{format!("{lat:.4}, {lon:.4}")}</span>
                    </button>
                }
            })
            .collect_view()
    };

    view! {
        <FormField
            label="Find by address".to_string()
            helptext="City, street, or landmark (OpenStreetMap, with a US Census fallback for new streets). Press Enter or Search.".to_string()
            error=Signal::derive(|| None::<String>)
        >
            <div class="geo-search">
                <input
                    type="text"
                    class="ui-input"
                    autocomplete="off"
                    enterkeyhint="search"
                    spellcheck="false"
                    placeholder=crate::voice::LOCATION_SEARCH_EXAMPLE
                    disabled=move || disabled.get()
                    prop:value=move || query.get()
                    on:input=move |ev| {
                        query.set(event_target_value(&ev));
                        // A changed question retires the old answer.
                        if !results.get_untracked().is_empty() || status.get_untracked().is_some() {
                            results.set(Vec::new());
                            status.set(None);
                        }
                    }
                    on:keydown=move |ev| {
                        if ev.key() == "Enter" {
                            ev.prevent_default();
                            on_search();
                        }
                    }
                />
                <Button
                    variant="ghost"
                    icon="search"
                    disabled=Signal::derive(move || disabled.get() || searching.get())
                    on_click=Callback::new(move |_| on_search())
                >
                    {move || if searching.get() { "Searching…" } else { "Search" }}
                </Button>
                <Button
                    variant="ghost"
                    icon="location"
                    disabled=Signal::derive(move || disabled.get() || locating.get())
                    on_click=Callback::new(move |_| on_locate())
                >
                    {move || if locating.get() { "Locating…" } else { "Use my location" }}
                </Button>
                <Button
                    variant="ghost"
                    disabled=disabled
                    aria_expanded=Signal::derive(move || map_open.get().to_string())
                    on_click=Callback::new(move |_| map_open.update(|open| *open = !*open))
                >
                    {move || if map_open.get() { "Hide map" } else { "Pick on map" }}
                </Button>
            </div>
        </FormField>
        // Always in the DOM so assistive tech has a live region to watch
        // before the first message arrives; empty until there is one.
        <p
            class="geo-status"
            class:geo-status--error=move || status.get().is_some_and(|s| s.error)
            role="status"
            aria-live="polite"
        >
            {move || status.get().map(|s| view! { <span class="geo-status__text">{s.text}</span> })}
        </p>
        <div class="geo-results">{results_view}</div>
        // The pin map (public/geo-picker.js mounts Leaflet on the canvas
        // when it appears and follows data-lat/data-lon). An interactive
        // map, so `application`, not `img`: an image cannot hold controls.
        <Show when=move || map_open.get()>
            <div class="geo-map">
                <div
                    class="geo-map__canvas"
                    node_ref=map_ref
                    data-geo-picker=""
                    role="application"
                    aria-label="Map. Tap or click to drop the pin on your location; drag the pin to adjust."
                    data-lat=move || position.get().map(|(lat, _)| lat.to_string())
                    data-lon=move || position.get().map(|(_, lon)| lon.to_string())
                ></div>
                <p class="geo-map__hint">
                    "Tap the map to drop the pin, drag it to fine-tune, then check the coordinates below. "
                    "Map tiles come from OpenStreetMap only while the map is open."
                </p>
            </div>
        </Show>
    }
}
