//! Shared browser notification choices. Persisted per subscription, not in
//! localStorage, so the server applies them while the PWA is closed.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ssr", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    WateringStarted,
    WateringFinished,
    WateringProblem,
    UrgentEquipment,
    SoilSensor,
    WeatherSource,
    Rain,
    Wind,
    Freeze,
    Heat,
    Lightning,
    Tuning,
    Configuration,
}

impl EventKind {
    pub const ALL: [Self; 13] = [
        Self::WateringStarted,
        Self::WateringFinished,
        Self::WateringProblem,
        Self::UrgentEquipment,
        Self::SoilSensor,
        Self::WeatherSource,
        Self::Rain,
        Self::Wind,
        Self::Freeze,
        Self::Heat,
        Self::Lightning,
        Self::Tuning,
        Self::Configuration,
    ];

    pub fn group(self) -> &'static str {
        match self {
            Self::WateringStarted | Self::WateringFinished => "Irrigation",
            Self::WateringProblem
            | Self::UrgentEquipment
            | Self::SoilSensor
            | Self::WeatherSource => "Equipment and sensors",
            Self::Rain | Self::Wind | Self::Freeze | Self::Heat | Self::Lightning => "Weather",
            Self::Tuning | Self::Configuration => "Updates",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::WateringStarted => "Watering starts",
            Self::WateringFinished => "Watering finishes",
            Self::WateringProblem => "Watering problems",
            Self::UrgentEquipment => "Urgent equipment alerts",
            Self::SoilSensor => "Soil sensor problems",
            Self::WeatherSource => "Forecast source offline",
            Self::Rain => "Rain starts",
            Self::Wind => "High wind",
            Self::Freeze => "Freezing temperature",
            Self::Heat => "High temperature",
            Self::Lightning => "Nearby lightning",
            Self::Tuning => "Weekly tuning suggestions",
            Self::Configuration => "Watering configuration changes",
        }
    }

    pub fn trigger(self) -> &'static str {
        match self {
            Self::WateringStarted => "When a zone starts. Includes Stop on supported devices.",
            Self::WateringFinished => "When a zone finishes, with its run time.",
            Self::WateringProblem => {
                "When a run fails to start or its controller cannot be reached."
            }
            Self::UrgentEquipment => {
                "When a valve may still be open or water flows with no zone running."
            }
            Self::SoilSensor => "When a probe stops reporting or its readings become unreliable.",
            Self::WeatherSource => "When the forecast feed stops updating.",
            Self::Rain => "When fresh station readings detect rain.",
            Self::Wind => "When measured sustained wind reaches 25 mph (40 km/h).",
            Self::Freeze => "When measured air temperature drops to 32°F (0°C).",
            Self::Heat => "When measured air temperature reaches 95°F (35°C).",
            Self::Lightning => "When a detector reports lightning within 10 miles (16 km).",
            Self::Tuning => "When the weekly report has suggestions to review.",
            Self::Configuration => {
                "When a run limit increases or inferred watering targets are first used."
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ssr", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
pub struct QuietHours {
    pub enabled: bool,
    pub start: String,
    pub end: String,
    pub allow_urgent: bool,
}

impl Default for QuietHours {
    fn default() -> Self {
        Self {
            enabled: true,
            start: "22:00".into(),
            end: "07:00".into(),
            allow_urgent: true,
        }
    }
}

pub fn minute(time: &str) -> Option<u32> {
    crate::config::schema::DailyOutlook {
        enabled: true,
        time: time.into(),
    }
    .minute_of_day()
}

impl QuietHours {
    pub fn contains(&self, now: u32) -> bool {
        if !self.enabled {
            return false;
        }
        let (Some(start), Some(end)) = (minute(&self.start), minute(&self.end)) else {
            return true;
        };
        if start < end {
            now >= start && now < end
        } else {
            now >= start || now < end
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ssr", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
pub struct PushPreferences {
    pub enabled: bool,
    pub events: BTreeSet<EventKind>,
    pub quiet_hours: QuietHours,
    pub daily_outlook: crate::config::schema::DailyOutlook,
}

impl Default for PushPreferences {
    fn default() -> Self {
        Self {
            enabled: true,
            events: [
                EventKind::WateringStarted,
                EventKind::WateringFinished,
                EventKind::WateringProblem,
                EventKind::UrgentEquipment,
                EventKind::SoilSensor,
                EventKind::WeatherSource,
                EventKind::Configuration,
            ]
            .into_iter()
            .collect(),
            quiet_hours: QuietHours::default(),
            daily_outlook: Default::default(),
        }
    }
}

impl PushPreferences {
    pub fn validate(&self) -> Result<(), &'static str> {
        if minute(&self.quiet_hours.start).is_none()
            || minute(&self.quiet_hours.end).is_none()
            || self.daily_outlook.minute_of_day().is_none()
        {
            return Err("Use a valid time for each notification setting.");
        }
        if self.quiet_hours.enabled && self.quiet_hours.start == self.quiet_hours.end {
            return Err("Choose different start and end times for quiet hours.");
        }
        if self.daily_outlook.enabled
            && self
                .quiet_hours
                .contains(minute(&self.daily_outlook.time).unwrap())
        {
            return Err("Choose a daily outlook time outside quiet hours.");
        }
        Ok(())
    }

    pub fn allows(&self, kind: EventKind, local_minute: u32) -> bool {
        self.enabled
            && self.events.contains(&kind)
            && (!self.quiet_hours.contains(local_minute)
                || (kind == EventKind::UrgentEquipment && self.quiet_hours.allow_urgent))
    }

    pub fn outlook_due(&self, local_minute: u32) -> bool {
        self.enabled
            && self.daily_outlook.due_at(local_minute)
            && !self.quiet_hours.contains(local_minute)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overnight_and_same_day_quiet_hours_have_clear_boundaries() {
        let mut prefs = PushPreferences::default();
        for time in [1320, 1439, 0, 419] {
            assert!(!prefs.allows(EventKind::WateringStarted, time));
            assert!(prefs.allows(EventKind::UrgentEquipment, time));
        }
        for time in [420, 1319] {
            assert!(prefs.allows(EventKind::WateringStarted, time));
        }
        prefs.quiet_hours.start = "12:00".into();
        prefs.quiet_hours.end = "14:00".into();
        assert!(prefs.allows(EventKind::WateringStarted, 719));
        assert!(!prefs.allows(EventKind::WateringStarted, 720));
        assert!(prefs.allows(EventKind::WateringStarted, 840));
    }

    #[test]
    fn explicit_choices_override_urgent_bypass_and_defaults_are_quiet() {
        let mut prefs: PushPreferences = serde_json::from_str("{}").unwrap();
        assert!(!prefs.outlook_due(540));
        assert!(!prefs.allows(EventKind::Rain, 540));
        prefs.quiet_hours.allow_urgent = false;
        assert!(!prefs.allows(EventKind::UrgentEquipment, 0));
        prefs.quiet_hours.allow_urgent = true;
        prefs.events.remove(&EventKind::UrgentEquipment);
        assert!(!prefs.allows(EventKind::UrgentEquipment, 0));
        prefs.enabled = false;
        assert!(!prefs.allows(EventKind::WateringStarted, 540));
    }

    #[test]
    fn rejects_bad_times_and_summaries_inside_quiet_hours() {
        let mut prefs = PushPreferences::default();
        prefs.daily_outlook.enabled = true;
        prefs.daily_outlook.time = "23:00".into();
        assert!(prefs.validate().is_err());
        prefs.daily_outlook.time = "09:00".into();
        assert!(prefs.validate().is_ok());
        prefs.quiet_hours.end = "22:00".into();
        assert!(prefs.validate().is_err());
        prefs.quiet_hours.end = "25:00".into();
        assert!(prefs.validate().is_err());
    }
}
