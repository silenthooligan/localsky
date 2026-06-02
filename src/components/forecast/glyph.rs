// WMO weather code → emoji + label. Open-Meteo emits these per
// daily window and per hour; we use the same mapping in both views
// so a thunderstorm tile reads the same way in the 7-day strip and
// the hourly chart. Codes from
// https://open-meteo.com/en/docs (look for "Weather code" appendix).

pub fn weather_code_glyph(code: u32, is_day: bool) -> (&'static str, &'static str) {
    match code {
        0 => {
            if is_day {
                ("☀️", "Clear")
            } else {
                ("🌙", "Clear")
            }
        }
        1 => {
            if is_day {
                ("🌤", "Mostly clear")
            } else {
                ("🌙", "Mostly clear")
            }
        }
        2 => ("⛅", "Partly cloudy"),
        3 => ("☁️", "Overcast"),
        45 | 48 => ("🌫", "Fog"),
        51 | 53 | 55 => ("🌦", "Drizzle"),
        56 | 57 => ("🌨", "Freezing drizzle"),
        61 => ("🌧", "Light rain"),
        63 => ("🌧", "Rain"),
        65 => ("🌧", "Heavy rain"),
        66 | 67 => ("🌧", "Freezing rain"),
        71 => ("🌨", "Light snow"),
        73 => ("🌨", "Snow"),
        75 => ("🌨", "Heavy snow"),
        77 => ("🌨", "Snow grains"),
        80 => ("🌦", "Light showers"),
        81 => ("🌧", "Showers"),
        82 => ("⛈", "Heavy showers"),
        85 | 86 => ("🌨", "Snow showers"),
        95 => ("⛈", "Thunderstorm"),
        96 | 99 => ("⛈", "Thunder + hail"),
        _ => ("·", ""),
    }
}
