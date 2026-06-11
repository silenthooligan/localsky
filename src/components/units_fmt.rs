// Display-unit plumbing for the persisted Units preference
// (Settings -> Units, localStorage keys units_temp / units_rain).
// Engine math and the snapshot wire format stay imperial-as-stored;
// these helpers convert at the display boundary only. Wired to the
// primary temperature and precipitation surfaces (weather hero, stat
// tiles, forecast hourly/daily); wind and area conversions are not
// implemented yet and their selectors are not shown in Settings.
//
// SSR and the first hydrate frame always render the imperial default so
// the DOM trees match; the Effect inside use_unit_prefs then reads
// localStorage and updates the signal, re-rendering consumers
// client-side only.

use leptos::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct UnitPrefs {
    /// Show temperatures in Celsius (source values are Fahrenheit).
    pub temp_c: bool,
    /// Show precipitation in millimeters (source values are inches).
    pub rain_mm: bool,
}

/// Reactive per-device unit preferences. Call once at component scope;
/// read the returned signal inside render closures so a preference
/// change (or the post-hydration localStorage load) re-renders.
pub fn use_unit_prefs() -> Signal<UnitPrefs> {
    let prefs = RwSignal::new(UnitPrefs::default());
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        let Some(win) = web_sys::window() else {
            return;
        };
        let Ok(Some(storage)) = win.local_storage() else {
            return;
        };
        let read = |k: &str| storage.get_item(k).ok().flatten();
        let next = UnitPrefs {
            temp_c: read("units_temp").as_deref() == Some("c"),
            rain_mm: read("units_rain").as_deref() == Some("mm"),
        };
        if next != prefs.get_untracked() {
            prefs.set(next);
        }
    });
    prefs.into()
}

pub fn f_to_c(f: f64) -> f64 {
    (f - 32.0) * 5.0 / 9.0
}

pub fn in_to_mm(inches: f64) -> f64 {
    inches * 25.4
}

/// Degree-glyph form without the scale letter: "72°" or "22°".
pub fn fmt_temp_short(temp_f: f64, p: UnitPrefs) -> String {
    if p.temp_c {
        format!("{:.0}°", f_to_c(temp_f))
    } else {
        format!("{temp_f:.0}°")
    }
}

/// Bare numeric value for StatTile-style value/unit splits.
pub fn temp_value(temp_f: f64, p: UnitPrefs) -> String {
    if p.temp_c {
        format!("{:.0}", f_to_c(temp_f))
    } else {
        format!("{temp_f:.0}")
    }
}

pub fn temp_unit(p: UnitPrefs) -> &'static str {
    if p.temp_c {
        "°C"
    } else {
        "°F"
    }
}

/// Rain rate: "0.02in/h" or "0.5mm/h".
pub fn fmt_rain_rate(in_per_hr: f64, p: UnitPrefs) -> String {
    if p.rain_mm {
        format!("{:.1}mm/h", in_to_mm(in_per_hr))
    } else {
        format!("{in_per_hr:.2}in/h")
    }
}

/// Rain amount: "0.25\"" or "6.4mm".
pub fn fmt_rain_amount(inches: f64, p: UnitPrefs) -> String {
    if p.rain_mm {
        format!("{:.1}mm", in_to_mm(inches))
    } else {
        format!("{inches:.2}\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMPERIAL: UnitPrefs = UnitPrefs {
        temp_c: false,
        rain_mm: false,
    };
    const METRIC: UnitPrefs = UnitPrefs {
        temp_c: true,
        rain_mm: true,
    };

    #[test]
    fn conversions() {
        assert!((f_to_c(32.0)).abs() < 1e-9);
        assert!((f_to_c(212.0) - 100.0).abs() < 1e-9);
        assert!((in_to_mm(1.0) - 25.4).abs() < 1e-9);
    }

    #[test]
    fn temp_formatting_both_scales() {
        assert_eq!(fmt_temp_short(72.0, IMPERIAL), "72°");
        assert_eq!(fmt_temp_short(72.0, METRIC), "22°");
        assert_eq!(temp_value(50.0, METRIC), "10");
        assert_eq!(temp_unit(IMPERIAL), "°F");
        assert_eq!(temp_unit(METRIC), "°C");
    }

    #[test]
    fn rain_formatting_both_scales() {
        assert_eq!(fmt_rain_rate(0.5, IMPERIAL), "0.50in/h");
        assert_eq!(fmt_rain_rate(0.5, METRIC), "12.7mm/h");
        assert_eq!(fmt_rain_amount(0.5, IMPERIAL), "0.50\"");
        assert_eq!(fmt_rain_amount(0.5, METRIC), "12.7mm");
    }
}
