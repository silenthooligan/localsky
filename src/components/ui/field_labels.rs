// Field-local accessible names and validation associations. FormField owns
// the observer and disconnects it when its reactive owner is cleaned up.
// Dynamic controls (the controller station picker, for example) can appear
// after mount, so observing that field's children is intentional. There is
// no document-wide scan and no leaked observer closure.

#[cfg(feature = "hydrate")]
static CONTROL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "hydrate")]
pub(super) fn bind(field: &web_sys::Element, label_id: &str, error_id: &str, invalid: bool) {
    use wasm_bindgen::JsCast;

    let Ok(Some(label)) = field.query_selector("label.ui-form-field__label") else {
        return;
    };
    let _ = label.set_attribute("id", label_id);
    let Ok(controls) =
        field.query_selector_all("input:not([type=hidden]), select, textarea, [role=radiogroup]")
    else {
        return;
    };
    for i in 0..controls.length() {
        let Some(control) = controls
            .item(i)
            .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
        else {
            continue;
        };
        // A nested FormField owns its own controls.
        if !control
            .closest(".ui-form-field")
            .ok()
            .flatten()
            .is_some_and(|owner| owner.is_same_node(Some(field)))
        {
            continue;
        }
        let explicitly_named = ["aria-label", "aria-labelledby"].iter().any(|attr| {
            control
                .get_attribute(attr)
                .is_some_and(|value| !value.trim().is_empty())
        });
        if !explicitly_named {
            let _ = control.set_attribute("aria-labelledby", label_id);
        }
        // An id alone is not an accessible name. Reuse a caller's id when
        // connecting the visible label; never rename its control.
        if i == 0 && control.get_attribute("role").as_deref() != Some("radiogroup") {
            let id = control
                .get_attribute("id")
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| {
                    format!(
                        "{label_id}-control-{}",
                        CONTROL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    )
                });
            let _ = control.set_attribute("id", &id);
            let _ = label.set_attribute("for", &id);
        }
        let mut descriptions: Vec<String> = control
            .get_attribute("aria-describedby")
            .unwrap_or_default()
            .split_whitespace()
            .filter(|id| *id != error_id)
            .map(str::to_string)
            .collect();
        if invalid {
            descriptions.push(error_id.to_string());
            let _ = control.set_attribute("aria-invalid", "true");
        } else {
            let _ = control.remove_attribute("aria-invalid");
        }
        if descriptions.is_empty() {
            let _ = control.remove_attribute("aria-describedby");
        } else {
            let _ = control.set_attribute("aria-describedby", &descriptions.join(" "));
        }
    }
}
