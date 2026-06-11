// /about page. Product identity, the living facts of this instance
// (version + update state, uptime, engine), a links grid, and the
// acknowledgements tucked behind a disclosure. Pulls /api/v1/health
// and /api/v1/updates after hydration.

use leptos::prelude::*;

use crate::components::ui::Icon;
use crate::docs::{doc_url, ISSUES_URL, REPO_URL, SITE_BASE};

fn uptime_label(s: i64) -> String {
    match s {
        s if s < 120 => format!("{s} seconds"),
        s if s < 7_200 => format!("{} minutes", s / 60),
        s if s < 172_800 => format!("{} hours", s / 3_600),
        s => format!("{} days", s / 86_400),
    }
}

#[component]
pub fn AboutPage() -> impl IntoView {
    let version = env!("CARGO_PKG_VERSION");
    let health: RwSignal<Option<serde_json::Value>> = RwSignal::new(None);
    let updates: RwSignal<Option<serde_json::Value>> = RwSignal::new(None);

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Ok(resp) = gloo_net::http::Request::get("/api/v1/health").send().await {
                if let Ok(v) = resp.json::<serde_json::Value>().await {
                    health.set(Some(v));
                }
            }
            if let Ok(resp) = gloo_net::http::Request::get("/api/v1/updates").send().await {
                if let Ok(v) = resp.json::<serde_json::Value>().await {
                    updates.set(Some(v));
                }
            }
        });
    });
    #[cfg(not(feature = "hydrate"))]
    let _ = (health, updates);

    view! {
        <main id="main-content" class="about-page">
            <div class="about-hero">
                <span class="about-hero__mark">
                    <img src="/brand-mark.svg" alt="" width="56" height="56"/>
                </span>
                <h1 class="about-hero__name">"LOCAL"<span class="about-hero__accent">"SKY"</span></h1>
                <p class="about-hero__tag">
                    "Hyperlocal weather and irrigation intelligence that lives on "
                    "your hardware and answers to no cloud."
                </p>
                <div class="about-hero__badges">
                    <span class="ha-chip ha-chip--on">
                        <span class="ha-chip__dot" aria-hidden="true"></span>
                        {format!("v{version}")}
                    </span>
                    {move || {
                        let u = updates.get()?;
                        let available = u.get("update_available").and_then(|v| v.as_bool()).unwrap_or(false);
                        let enabled = u.get("check_enabled").and_then(|v| v.as_bool()).unwrap_or(false);
                        Some(if available {
                            let latest = u.get("latest").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                            let url = u.get("release_url").and_then(|v| v.as_str()).unwrap_or(REPO_URL).to_string();
                            view! {
                                <a class="ha-chip ha-chip--warn" href=url target="_blank" rel="noopener">
                                    <span class="ha-chip__dot" aria-hidden="true"></span>
                                    {format!("{latest} available")}
                                </a>
                            }.into_any()
                        } else if enabled {
                            view! {
                                <span class="ha-chip">
                                    <span class="ha-chip__dot" aria-hidden="true"></span>
                                    "Up to date"
                                </span>
                            }.into_any()
                        } else {
                            ().into_any()
                        })
                    }}
                </div>
            </div>

            {move || health.get().map(|h| {
                let uptime = h.get("uptime_s").and_then(|v| v.as_i64()).unwrap_or(0);
                let status = h.get("status").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                let brain = h
                    .pointer("/ha/snapshot_source")
                    .and_then(|v| v.as_str())
                    .map(|s| if s == "standalone" { "LocalSky native engine" } else { "Home Assistant (migration)" })
                    .unwrap_or("LocalSky native engine")
                    .to_string();
                let sources = h.get("sources").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
                view! {
                    <div class="about-facts">
                        <div class="about-fact">
                            <span class="about-fact__k">"Health"</span>
                            <span class="about-fact__v">{status}</span>
                        </div>
                        <div class="about-fact">
                            <span class="about-fact__k">"Up for"</span>
                            <span class="about-fact__v">{uptime_label(uptime)}</span>
                        </div>
                        <div class="about-fact">
                            <span class="about-fact__k">"Watering brain"</span>
                            <span class="about-fact__v">{brain}</span>
                        </div>
                        <div class="about-fact">
                            <span class="about-fact__k">"Weather sources"</span>
                            <span class="about-fact__v">{sources.to_string()}</span>
                        </div>
                    </div>
                }
            })}

            <div class="about-links">
                <a class="about-link" href=doc_url("getting-started") target="_blank" rel="noopener">
                    <Icon name="download" size=18/>
                    <strong>"Installation guide"</strong>
                    <span>"Docker, first boot, the wizard"</span>
                </a>
                <a class="about-link" href=SITE_BASE target="_blank" rel="noopener">
                    <Icon name="info" size=18/>
                    <strong>"Documentation"</strong>
                    <span>"The full manual at localsky.io"</span>
                </a>
                <a class="about-link" href=REPO_URL target="_blank" rel="noopener">
                    <Icon name="external" size=18/>
                    <strong>"Source code"</strong>
                    <span>"Apache-2.0 on GitHub"</span>
                </a>
                <a class="about-link" href=ISSUES_URL target="_blank" rel="noopener">
                    <Icon name="alert-triangle" size=18/>
                    <strong>"Report a problem"</strong>
                    <span>"Bugs and feature requests"</span>
                </a>
                <a class="about-link" href=doc_url("migrating-from-ha") target="_blank" rel="noopener">
                    <Icon name="home" size=18/>
                    <strong>"Coming from Home Assistant?"</strong>
                    <span>"The migration walkthrough"</span>
                </a>
                <a class="about-link" href="/setup">
                    <Icon name="wizard" size=18/>
                    <strong>"Setup wizard"</strong>
                    <span>"Re-run guided setup any time"</span>
                </a>
            </div>

            <section class="about-credits">
                <h2 class="about-credits__title">"Standing on shoulders: science and software credits"</h2>
                <ul class="setup-source-list">
                    <li>
                        "FAO Irrigation and Drainage Paper No. 56 (Allen et al., 1998) "
                        "for the Penman-Monteith reference ET."
                    </li>
                    <li>"ASCE-EWRI Standardized Reference Evapotranspiration Equation (2005)."</li>
                    <li>
                        "UF/IFAS Extension turfgrass publications (ENH6, ENH8, ENH11, "
                        "ENH19, ENH62, ENH1115)."
                    </li>
                    <li>
                        "USDA NRCS National Irrigation Guide (Part 652) for soil "
                        "infiltration and available-water tables."
                    </li>
                    <li>
                        "Home Assistant's Smart Irrigation and Irrigation Unlimited "
                        "integrations: the prior art that informed this clean-room rewrite."
                    </li>
                    <li>
                        "Open-Meteo, RainViewer, Leaflet, Leptos, rumqttc, rusqlite, "
                        "tokio, reqwest, and the broader Rust + WASM ecosystem."
                    </li>
                </ul>
            </section>

            <p class="about-footer">
                {format!("LOCALSKY v{version} · Apache-2.0 · made for yards everywhere")}
            </p>
        </main>
    }
}
