// Per-field weather-source override helpers.
//
// `Config.field_source_overrides` maps a WeatherField snake_case name to the
// `id` of the source the user wants to OWN that field's live merge. This module
// is the single source of truth for:
//
//   * the canonical snake_case name of each user-relevant WeatherField
//     (`field_name`), used as the config map key AND the JSON the settings UI
//     reads/writes, and
//   * the small, user-facing set of fields the override picker offers
//     (`USER_FIELDS`): the headline readings an operator actually thinks in
//     terms of (temperature, humidity, wind, rain, pressure, solar/UV).
//
// Keeping the name <-> field mapping here (rather than scattering string
// literals) is what makes the config key, the settings picker, and the
// merge-install translation agree. The merge layer (TempestStore) keys its
// owner map by snapshot-field key, not by WeatherField name, so the install
// path (`overrides_to_owner_labels`) translates name -> owner-key before
// handing the map to the store; that translation lives next to the field list
// it depends on.

use crate::ports::weather_source::WeatherField;

/// Canonical snake_case name for a `WeatherField`, used as the
/// `field_source_overrides` config-map key and the settings-UI field id. These
/// match the documented MQTT field-name convention
/// (`sources::mqtt_subscribe::parse_weather_field`), so an operator hand-editing
/// the TOML uses the same spellings everywhere. Returns `None` for the
/// structured-forecast variants (a forecast snapshot is arbitrated whole by the
/// forecast bridge, not per-field, so it is not override-able here).
pub fn field_name(f: WeatherField) -> Option<&'static str> {
    use WeatherField::*;
    match f {
        RainTypeStr | ForecastDaily | ForecastHourly => None,
        other => Some(other.name()),
    }
}

/// Parse a `field_source_overrides` config-map key back to a `WeatherField`.
/// Inverse of [`field_name`]; unknown keys (typos, removed fields) return
/// `None` and are ignored by the install path (never an error).
pub fn parse_field_name(name: &str) -> Option<WeatherField> {
    WeatherField::parse(name).filter(|f| field_name(*f).is_some())
}

/// The user-relevant headline fields the override picker offers, in display
/// order: each is `(field_name, label)`. Deliberately the small set an operator
/// reasons about ("which station owns my wind?"), not every internal channel.
/// The settings UI renders one picker per entry.
pub const USER_FIELDS: &[(&str, &str)] = &[
    ("air_temp_f", "Temperature"),
    ("rh_pct", "Humidity"),
    ("wind_mph", "Wind"),
    ("rain_today_in", "Rain"),
    ("pressure_in_hg", "Pressure"),
    ("solar_w_m2", "Solar / UV"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_roundtrips() {
        for (name, _) in USER_FIELDS {
            let f = parse_field_name(name).expect("user field parses");
            assert_eq!(field_name(f), Some(*name), "{name} round-trips");
        }
    }

    #[test]
    fn structured_forecast_has_no_name() {
        assert_eq!(field_name(WeatherField::ForecastDaily), None);
        assert_eq!(field_name(WeatherField::RainTypeStr), None);
    }
}
