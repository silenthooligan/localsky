//! Extra models share the app's config client and form primitives.
use crate::components::ui::{Button, FormField, Icon, ToastHub};
use crate::config::ForecastTrack;
use crate::forecast::window::TrackStatus;
use leptos::prelude::*;

#[component]
pub fn ForecastTracks() -> impl IntoView {
    let tracks = RwSignal::new(Vec::<ForecastTrack>::new());
    let statuses = RwSignal::new(Vec::<TrackStatus>::new());
    let status_unavailable = RwSignal::new(false);
    let loaded = RwSignal::new(false);
    let saving = RwSignal::new(false);
    let error = RwSignal::new(String::new());
    let reload = RwSignal::new(0_u32);
    let name = RwSignal::new(String::new());
    let model = RwSignal::new(crate::forecast::model_catalog::DEFAULT_MODEL.to_string());
    let toast = use_context::<ToastHub>();

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            reload.track();
            leptos::task::spawn_local(async move {
                match crate::components::config_client::get_config().await {
                    Ok(config) => match serde_json::from_value::<Vec<ForecastTrack>>(
                        config
                            .get("forecast_tracks")
                            .cloned()
                            .unwrap_or(serde_json::json!([])),
                    ) {
                        Ok(values) => {
                            tracks.set(values);
                            loaded.set(true);
                        }
                        Err(_) => error.set("Could not read the extra forecast models.".into()),
                    },
                    Err(message) => error.set(message),
                }
            });
        });
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cleanup = cancelled.clone();
        on_cleanup(move || cleanup.store(true, std::sync::atomic::Ordering::Relaxed));
        leptos::task::spawn_local(async move {
            while !cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                let mut received = false;
                if let Ok(response) = gloo_net::http::Request::get("/api/v1/forecast/tracks")
                    .send()
                    .await
                {
                    if response.ok() {
                        if let Ok(values) = response.json::<Vec<TrackStatus>>().await {
                            if !cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                                statuses.set(values);
                                received = true;
                            }
                        }
                    }
                }
                if !cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                    status_unavailable.set(!received);
                }
                gloo_timers::future::TimeoutFuture::new(15_000).await;
            }
        });
    }

    let save = Callback::new(move |next: Vec<ForecastTrack>| {
        if saving.get_untracked() {
            return;
        }
        if let Err(message) = ForecastTrack::validate_all(&next) {
            error.set(message);
            return;
        }
        saving.set(true);
        error.set(String::new());
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let result = async {
                let mut config = crate::components::config_client::get_config().await?;
                config["forecast_tracks"] =
                    serde_json::to_value(&next).map_err(|e| e.to_string())?;
                crate::components::config_client::put_config(&config).await
            }
            .await;
            match result {
                Ok(outcome) => {
                    tracks.set(next);
                    name.set(String::new());
                    reload.update(|value| *value = value.wrapping_add(1));
                    if let Some(toast) = toast {
                        toast.success(outcome.confirmation("Forecast models saved."));
                    }
                }
                Err(message) => error.set(message),
            }
            saving.set(false);
        });
        #[cfg(not(feature = "hydrate"))]
        {
            let _ = (next, toast, reload);
            saving.set(false);
        }
    });
    let add = Callback::new(move |_| {
        let mut next = tracks.get_untracked();
        next.push(ForecastTrack {
            id: name.get_untracked().trim().to_string(),
            model: model.get_untracked(),
        });
        save.run(next);
    });

    view! {
        <section class="forecast-tracks" aria-labelledby="forecast-tracks-title">
            <div class="forecast-tracks__heading">
                <Icon name="sources" size=20/>
                <h3 id="forecast-tracks-title">"Extra forecast models"</h3>
            </div>
            <p class="forecast-tracks__intro">"Keep a separate forecast for Home Assistant or another app."
                " "<a href="/docs/forecast.html#forecast-tracks">"Learn more"</a>
            </p>
            <ul class="forecast-tracks__list">
                <For each=move || tracks.get() key=|track| track.id.clone() children=move |track| {
                    let id = track.id.clone();
                    let status_id = track.id.clone();
                    let label = crate::forecast::model_catalog::model_by_id(&track.model).map(|m| m.label.to_string()).unwrap_or(track.model);
                    let remove = Callback::new(move |_| {
                        save.run(tracks.get_untracked().into_iter().filter(|track| track.id != id).collect());
                    });
                    view! {
                        <li class="forecast-tracks__row">
                            <div class="forecast-tracks__identity"><strong>{track.id}</strong><span>{label}</span></div>
                            <span class="forecast-tracks__status">{move || statuses.with(|rows| {
                                if status_unavailable.get() { return "Status unavailable".to_string(); }
                                match rows.iter().find(|row| row.id == status_id) {
                                    Some(row) => match row.age_s {
                                        Some(age) => format!("{} · {} min ago", if row.degraded { "Needs attention" } else { "Ready" }, age / 60),
                                        None => row.last_error.clone().unwrap_or_else(|| "Waiting for its first forecast".into()),
                                    },
                                    None => "Waiting for its first forecast".into(),
                                }
                            })}</span>
                            <Button variant="ghost" size="sm" disabled=Signal::derive(move || saving.get()) on_click=remove>"Remove"</Button>
                        </li>
                    }
                }/>
            </ul>
            <Show when=move || tracks.get().len() < 4>
                <div class="forecast-tracks__form">
                    <FormField label="Short name" helptext="Use this name in your automations.">
                        <input class="ui-input" id="forecast-track-name" type="text" maxlength="40" placeholder="nbm"
                            prop:value=move || name.get() on:input=move |event| name.set(event_target_value(&event)) />
                    </FormField>
                    <FormField label="Weather model">
                        <select class="ui-input" id="forecast-track-model" prop:value=move || model.get()
                            on:change=move |event| model.set(event_target_value(&event))>
                            {crate::forecast::model_catalog::models().iter().map(|entry| view! {
                                <option value=entry.id>{entry.label}</option>
                            }).collect_view()}
                        </select>
                    </FormField>
                    <Button variant="secondary" on_click=add loading=Signal::derive(move || saving.get())
                        disabled=Signal::derive(move || !loaded.get() || name.get().trim().is_empty())>"Add model"</Button>
                </div>
                <p class="forecast-tracks__coverage">{move || {
                    let id = model.get();
                    crate::forecast::model_catalog::model_by_id(&id).map(|entry| format!("Coverage: {}", entry.region)).unwrap_or_default()
                }}</p>
            </Show>
            <Show when=move || !error.get().is_empty()><p class="setup-result setup-result--err" role="alert">{move || error.get()}</p></Show>
        </section>
    }
}
