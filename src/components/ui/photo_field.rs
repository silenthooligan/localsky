// <PhotoField/>, drop-zone + browse button + URL fallback for the
// Zones form's photo_url field. Replaces a bare text input.
//
// Browse: hidden <input type="file"> is clicked from a styled button.
// Drag-and-drop: any image dropped onto the surface is uploaded.
// Both paths POST multipart to /api/zones/photo (see src/api/photos.rs)
// and write the returned URL into the shared `value` signal so the
// rest of the form already does the right thing on save.
//
// A URL text input below the drop zone keeps off-site URLs working
// for users who already host their photos somewhere else.

use leptos::prelude::*;

#[component]
pub fn PhotoField(value: RwSignal<String>) -> impl IntoView {
    let uploading = RwSignal::new(false);
    let upload_err = RwSignal::new(String::new());
    let drag_active = RwSignal::new(false);

    let file_input_ref = NodeRef::<leptos::html::Input>::new();

    let trigger_browse = move |_| {
        #[cfg(feature = "hydrate")]
        {
            use wasm_bindgen::JsCast;
            if let Some(input) = file_input_ref.get() {
                let elt: &web_sys::HtmlInputElement = (*input).unchecked_ref();
                elt.click();
            }
        }
    };

    let on_file_change = move |_ev| {
        #[cfg(feature = "hydrate")]
        {
            use wasm_bindgen::JsCast;
            if let Some(input) = file_input_ref.get() {
                let elt: &web_sys::HtmlInputElement = (*input).unchecked_ref();
                if let Some(files) = elt.files() {
                    if let Some(file) = files.item(0) {
                        upload_one(file, value, uploading, upload_err);
                    }
                }
            }
        }
        #[cfg(not(feature = "hydrate"))]
        let _ = _ev;
    };

    let on_clear = move |_| {
        value.set(String::new());
        upload_err.set(String::new());
    };

    let drop_class = move || {
        if drag_active.get() {
            "photo-field__drop is-drag"
        } else if !value.get().is_empty() {
            "photo-field__drop has-image"
        } else {
            "photo-field__drop"
        }
    };

    view! {
        <div class="photo-field">
            <div
                class=drop_class
                on:dragover=move |ev| {
                    ev.prevent_default();
                    drag_active.set(true);
                }
                on:dragleave=move |ev| {
                    ev.prevent_default();
                    drag_active.set(false);
                }
                on:drop=move |ev| {
                    ev.prevent_default();
                    drag_active.set(false);
                    #[cfg(feature = "hydrate")]
                    {
                        if let Some(dt) = ev.data_transfer() {
                            if let Some(files) = dt.files() {
                                if let Some(file) = files.item(0) {
                                    upload_one(file, value, uploading, upload_err);
                                }
                            }
                        }
                    }
                }
            >
                {move || if !value.get().is_empty() {
                    view! {
                        <img class="photo-field__preview" src=move || value.get() alt="zone photo"/>
                        <button
                            type="button"
                            class="photo-field__clear"
                            aria-label="Remove photo"
                            on:click=on_clear
                        >
                            "×"
                        </button>
                    }.into_any()
                } else {
                    view! {
                        <div class="photo-field__hint">
                            <span class="photo-field__hint-icon" aria-hidden="true">"+"</span>
                            <span class="photo-field__hint-text">
                                "Drop an image here or "
                                <button type="button" class="photo-field__browse-link" on:click=trigger_browse>
                                    "browse"
                                </button>
                            </span>
                            <span class="photo-field__hint-meta">"JPG, PNG, GIF, WebP up to 10 MB"</span>
                        </div>
                    }.into_any()
                }}
            </div>
            <input
                type="file"
                accept="image/jpeg,image/png,image/gif,image/webp"
                style="display:none"
                node_ref=file_input_ref
                on:change=on_file_change
            />
            <Show when=move || uploading.get()>
                <p class="photo-field__status" role="status">"Uploading…"</p>
            </Show>
            <Show when=move || !upload_err.get().is_empty()>
                <p class="photo-field__error" role="alert">{move || upload_err.get()}</p>
            </Show>
            <input
                type="text"
                class="ui-input photo-field__url"
                placeholder="…or paste a URL"
                prop:value=move || value.get()
                on:input=move |ev| value.set(event_target_value(&ev))
            />
        </div>
    }
}

/// Multipart-upload the selected file to /api/zones/photo. On success
/// writes the server-returned URL into `value`; on failure pushes a
/// human-readable message into `upload_err`. Best-effort: every error
/// path resets `uploading` so the spinner doesn't stick.
#[cfg(feature = "hydrate")]
fn upload_one(
    file: web_sys::File,
    value: RwSignal<String>,
    uploading: RwSignal<bool>,
    upload_err: RwSignal<String>,
) {
    use wasm_bindgen::JsCast;

    upload_err.set(String::new());
    uploading.set(true);

    leptos::task::spawn_local(async move {
        let finish_err = |msg: String| {
            upload_err.set(msg);
            uploading.set(false);
        };

        let form = match web_sys::FormData::new() {
            Ok(f) => f,
            Err(_) => return finish_err("could not create FormData".into()),
        };
        let name = file.name();
        if form
            .append_with_blob_and_filename("file", &file, &name)
            .is_err()
        {
            return finish_err("could not attach file to form".into());
        }

        let opts = web_sys::RequestInit::new();
        opts.set_method("POST");
        opts.set_body(&form.into());

        let req = match web_sys::Request::new_with_str_and_init("/api/zones/photo", &opts) {
            Ok(r) => r,
            Err(_) => return finish_err("could not build request".into()),
        };

        let window = match web_sys::window() {
            Some(w) => w,
            None => return finish_err("no window".into()),
        };

        let resp_js =
            match wasm_bindgen_futures::JsFuture::from(window.fetch_with_request(&req)).await {
                Ok(j) => j,
                Err(_) => return finish_err("network error".into()),
            };
        let resp: web_sys::Response = match resp_js.dyn_into() {
            Ok(r) => r,
            Err(_) => return finish_err("malformed response".into()),
        };

        if !resp.ok() {
            let status = resp.status();
            let body = match resp.text() {
                Ok(p) => wasm_bindgen_futures::JsFuture::from(p)
                    .await
                    .ok()
                    .and_then(|j| j.as_string())
                    .unwrap_or_default(),
                Err(_) => String::new(),
            };
            return finish_err(format!("upload failed ({status}): {body}"));
        }

        let json_promise = match resp.json() {
            Ok(p) => p,
            Err(_) => return finish_err("response not JSON".into()),
        };
        let json = match wasm_bindgen_futures::JsFuture::from(json_promise).await {
            Ok(j) => j,
            Err(_) => return finish_err("could not parse response JSON".into()),
        };
        let url = js_sys::Reflect::get(&json, &"url".into())
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default();
        if url.is_empty() {
            return finish_err("server returned no URL".into());
        }
        value.set(url);
        uploading.set(false);
    });
}
