// DemoReplay weather source. Generates plausible synthetic readings on
// a sine wave so demo mode boots with a fully populated dashboard
// without any external dependencies (no LAN, no cloud, no recorded
// JSONL).
//
// Plausible defaults (Florida summer day):
//   air_temp:    swings 24-32 C across the day
//   humidity:    swings 55-85 %
//   wind:        steady 5 mph + gusts
//   solar:       half-sine, peaks ~1000 W/m^2 at solar noon
//   rain:        rare-event probabilistic (~1 in 20 ticks)
//
// Configurable replay rate scales the wave so a 10x rate cycles through
// a "day" in ~2.4h wall-clock.

use std::sync::Arc;

use async_trait::async_trait;
use std::collections::HashSet;
use tokio::time::{interval, Duration};
use tracing::info;

use crate::config::schema::DemoReplayConfig;
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};

pub struct DemoReplay {
    id: String,
    config: DemoReplayConfig,
}

impl DemoReplay {
    pub fn new(id: impl Into<String>, config: DemoReplayConfig) -> Self {
        Self {
            id: id.into(),
            config,
        }
    }

    fn synthesize(&self, t_sim: f64) -> Vec<(WeatherField, f64)> {
        // t_sim in [0, 86400) seconds = one simulated day.
        let day_phase = (t_sim / 86400.0) * std::f64::consts::TAU;
        let solar = (day_phase - std::f64::consts::FRAC_PI_2).sin();
        let solar_w_m2 = if solar > 0.0 { solar * 1000.0 } else { 0.0 };
        let temp_c = 28.0 + 4.0 * (day_phase - 0.4 * std::f64::consts::TAU).sin();
        let temp_f = temp_c * 9.0 / 5.0 + 32.0;
        let humidity = 70.0 + 15.0 * (day_phase - 1.0).sin();
        let wind_mph = 5.0 + 2.5 * (day_phase * 3.0).sin().abs();
        let wind_gust_mph = wind_mph + 4.0;
        // Rare probabilistic rain: 5% of ticks at >0.
        let rain_intensity = if (t_sim as i64) % 2000 < 100 {
            0.05 * ((t_sim as i64 % 2000) as f64 / 100.0).min(1.0)
        } else {
            0.0
        };

        vec![
            (WeatherField::AirTempF, temp_f),
            (WeatherField::RhPct, humidity.clamp(0.0, 100.0)),
            (WeatherField::WindMph, wind_mph),
            (WeatherField::WindGustMph, wind_gust_mph),
            (WeatherField::SolarWm2, solar_w_m2),
            (WeatherField::RainIntensityInHr, rain_intensity),
            (WeatherField::PressureInHg, 30.05),
            (WeatherField::UvIndex, (solar_w_m2 / 100.0).clamp(0.0, 12.0)),
        ]
    }
}

#[async_trait]
impl WeatherSource for DemoReplay {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        let mut fields = HashSet::new();
        fields.insert(WeatherField::AirTempF);
        fields.insert(WeatherField::RhPct);
        fields.insert(WeatherField::WindMph);
        fields.insert(WeatherField::WindGustMph);
        fields.insert(WeatherField::SolarWm2);
        fields.insert(WeatherField::RainIntensityInHr);
        fields.insert(WeatherField::PressureInHg);
        fields.insert(WeatherField::UvIndex);
        SourceCaps {
            live_current: true,
            hourly_forecast_hours: 0,
            daily_forecast_days: 0,
            radar_tiles: false,
            et0_native: false,
            fields,
        }
    }

    fn priority(&self, _field: WeatherField) -> i32 {
        // High priority so demo mode dominates the merge.
        100
    }

    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        info!(
            source = self.id,
            rate = self.config.rate,
            "demo_replay starting"
        );
        let mut ticker = interval(Duration::from_secs(3));
        let started_real = now_epoch();
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let elapsed_real = now_epoch() - started_real;
                    let t_sim = (elapsed_real as f64 * self.config.rate) % 86400.0;
                    let fields = self.synthesize(t_sim);
                    let _ = bus.send(SourceEvent::Observation {
                        source_id: self.id.clone(),
                        fields,
                        at_epoch: now_epoch(),
                    });
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!(source = self.id, "demo_replay shutting down");
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrl() -> DemoReplay {
        DemoReplay::new(
            "demo",
            DemoReplayConfig {
                rate: 10.0,
                replay_path: None,
            },
        )
    }

    #[test]
    fn synthesize_at_midday_has_high_solar() {
        let s = ctrl();
        // Day phase ~ pi (midday in radians, since we offset by pi/2).
        let f = s.synthesize(43200.0); // 12:00 sim time
        let solar = f
            .iter()
            .find(|(k, _)| *k == WeatherField::SolarWm2)
            .map(|(_, v)| *v)
            .unwrap();
        assert!(solar > 500.0, "midday solar should be high, got {solar}");
    }

    #[test]
    fn synthesize_at_midnight_has_no_solar() {
        let s = ctrl();
        let f = s.synthesize(0.0);
        let solar = f
            .iter()
            .find(|(k, _)| *k == WeatherField::SolarWm2)
            .map(|(_, v)| *v)
            .unwrap();
        assert!(solar <= 0.001, "midnight solar should be ~0, got {solar}");
    }

    #[test]
    fn humidity_stays_in_range() {
        let s = ctrl();
        for t in (0..86400).step_by(3600) {
            let f = s.synthesize(t as f64);
            let h = f
                .iter()
                .find(|(k, _)| *k == WeatherField::RhPct)
                .map(|(_, v)| *v)
                .unwrap();
            assert!(
                h >= 0.0 && h <= 100.0,
                "humidity out of range: {h} at t={t}"
            );
        }
    }

    #[test]
    fn capabilities_advertises_live_only() {
        let s = ctrl();
        let caps = s.capabilities();
        assert!(caps.live_current);
        assert_eq!(caps.daily_forecast_days, 0);
        assert_eq!(caps.hourly_forecast_hours, 0);
        assert!(!caps.radar_tiles);
    }

    #[test]
    fn priority_is_high() {
        let s = ctrl();
        assert_eq!(s.priority(WeatherField::AirTempF), 100);
    }
}
