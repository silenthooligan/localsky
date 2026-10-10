// WMO weather code → Icon registry name + label. Open-Meteo emits these
// per daily window and per hour; we use the same mapping in both views
// so a thunderstorm tile reads the same way in the 7-day strip and the
// hourly chart. Codes from https://open-meteo.com/en/docs ("Weather
// code" appendix). Names resolve through ui::Icon, so the glyphs theme
// with currentColor across dark / light / high-contrast (emoji didn't).

pub fn weather_code_glyph(code: u32, is_day: bool) -> (&'static str, &'static str) {
    match code {
        0 => {
            if is_day {
                ("sun", "Clear")
            } else {
                ("moon", "Clear")
            }
        }
        1 => {
            if is_day {
                ("cloud-sun", "Mostly clear")
            } else {
                ("cloud-moon", "Mostly clear")
            }
        }
        2 => (
            if is_day { "cloud-sun" } else { "cloud-moon" },
            "Partly cloudy",
        ),
        3 => ("cloud", "Overcast"),
        45 | 48 => ("cloud-fog", "Fog"),
        51 | 53 | 55 => ("cloud-drizzle", "Drizzle"),
        56 | 57 => ("cloud-snow", "Freezing drizzle"),
        61 => ("cloud-rain", "Light rain"),
        63 => ("cloud-rain", "Rain"),
        65 => ("cloud-rain", "Heavy rain"),
        66 | 67 => ("cloud-rain", "Freezing rain"),
        71 => ("cloud-snow", "Light snow"),
        73 => ("cloud-snow", "Snow"),
        75 => ("cloud-snow", "Heavy snow"),
        77 => ("cloud-snow", "Snow grains"),
        80 => ("cloud-drizzle", "Light showers"),
        81 => ("cloud-rain", "Showers"),
        82 => ("cloud-rain", "Heavy showers"),
        85 | 86 => ("cloud-snow", "Snow showers"),
        95 => ("cloud-lightning", "Thunderstorm"),
        96 | 99 => ("cloud-lightning", "Thunder + hail"),
        _ => ("cloud", "Conditions unavailable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn night_clouds_and_heavy_showers_keep_their_actual_meaning() {
        assert_eq!(
            weather_code_glyph(2, false),
            ("cloud-moon", "Partly cloudy")
        );
        assert_eq!(weather_code_glyph(1, false), ("cloud-moon", "Mostly clear"));
        assert_eq!(
            weather_code_glyph(82, true),
            ("cloud-rain", "Heavy showers")
        );
        assert_eq!(
            weather_code_glyph(u32::MAX, true),
            ("cloud", "Conditions unavailable")
        );
    }
}
