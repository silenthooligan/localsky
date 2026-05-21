// Per-zone soil water balance. Single-bucket model after FAO-56 §8.
//
// State: depletion_mm = how much water below field capacity the bucket
// currently is. depletion = 0 means saturated to FC; depletion = TAW
// means at wilting point (PWP).
//
// Each engine tick steps the bucket: ETc drives it down, effective rain
// + applied irrigation refill it. Trigger irrigation when depletion >= RAW.

use crate::config::schema::{SoilTexture, GrassSpecies};
use crate::engine::soil_catalog::{raw_mm as raw_for, taw_mm as taw_for};
use crate::engine::species_catalog::{kc_at_doy_lat, lookup as species_lookup};

#[derive(Debug, Clone, Copy)]
pub struct ZoneWaterState {
    /// Current depletion below field capacity (mm). Clamped to [0, TAW].
    pub depletion_mm: f64,
}

impl Default for ZoneWaterState {
    fn default() -> Self {
        Self { depletion_mm: 0.0 }
    }
}

/// Crop ET (mm/day) = ET0 * Kc * heat_multiplier.
pub fn etc_mm(et0_mm: f64, kc: f64, heat_multiplier: f64) -> f64 {
    (et0_mm * kc * heat_multiplier).max(0.0)
}

/// Step the per-zone bucket by one period (typically one day for daily
/// ET0 ticks). Returns the updated depletion.
///
/// effective_rain = gross_rain * capture_efficiency; capture_efficiency
/// accounts for runoff, canopy interception, and evaporation losses
/// before water enters the root zone. Default 0.70 per
/// EngineParams::capture_efficiency.
///
/// applied_mm is the depth of irrigation water that reached the soil
/// during this period (already capture-corrected for application
/// efficiency, since irrigation hoses have similar losses).
pub fn step(
    state: &mut ZoneWaterState,
    et_c_mm: f64,
    gross_rain_mm: f64,
    applied_mm: f64,
    capture_efficiency: f64,
    taw_mm: f64,
) -> f64 {
    let effective_rain = gross_rain_mm * capture_efficiency.clamp(0.0, 1.0);
    let next = state.depletion_mm + et_c_mm - effective_rain - applied_mm;
    state.depletion_mm = next.clamp(0.0, taw_mm.max(0.0));
    state.depletion_mm
}

/// True when the bucket has depleted past the readily-available band
/// and irrigation is recommended.
pub fn should_irrigate(depletion_mm: f64, raw_mm: f64) -> bool {
    depletion_mm >= raw_mm
}

/// Translate a depletion target into runtime seconds for a sprinkler
/// with the given precipitation rate. capture_efficiency accounts for
/// the wet-loss between hose and root zone; runtime is capped at
/// max_duration_s to honor controller safety ceilings.
pub fn refill_runtime_seconds(
    depletion_mm: f64,
    precip_rate_mm_hr: f64,
    capture_efficiency: f64,
    max_duration_s: u32,
) -> u32 {
    if depletion_mm <= 0.0 || precip_rate_mm_hr <= 0.01 {
        return 0;
    }
    let eff = capture_efficiency.clamp(0.05, 1.0);
    let gross_mm = depletion_mm / eff;
    let hours = gross_mm / precip_rate_mm_hr;
    let s = (hours * 3600.0).round() as i64;
    s.clamp(0, max_duration_s as i64) as u32
}

/// Per-zone summary the engine produces on each tick: depletion, TAW,
/// RAW, kc, predicted ETc, and whether irrigation is currently warranted.
#[derive(Debug, Clone, Copy)]
pub struct ZoneBalanceSummary {
    pub depletion_mm: f64,
    pub taw_mm: f64,
    pub raw_mm: f64,
    pub kc: f64,
    pub etc_today_mm: f64,
    pub needs_irrigation: bool,
    pub planned_runtime_s: u32,
}

/// Convenience: assemble a `ZoneBalanceSummary` from species + soil +
/// current state. Used by the scheduler to render dashboard tiles and
/// to feed the controller dispatch logic.
pub fn summarize(
    species: GrassSpecies,
    soil: SoilTexture,
    root_depth_mm_override: Option<f64>,
    mad_pct_override: Option<f64>,
    state: ZoneWaterState,
    et0_today_mm: f64,
    heat_multiplier: f64,
    doy: u16,
    latitude_deg: f64,
    precip_rate_mm_hr: f64,
    capture_efficiency: f64,
    max_duration_s: u32,
) -> ZoneBalanceSummary {
    let profile = species_lookup(species);
    let root_depth = root_depth_mm_override.unwrap_or(profile.root_depth_mm);
    let mad = mad_pct_override.unwrap_or(profile.mad_pct);
    let taw = taw_for(soil, root_depth);
    let raw = raw_for(soil, root_depth, mad);
    let kc = kc_at_doy_lat(species, doy, latitude_deg);
    let etc = etc_mm(et0_today_mm, kc, heat_multiplier);
    let needs = should_irrigate(state.depletion_mm, raw);
    let runtime = if needs {
        // Refill back to field capacity (depletion = 0).
        refill_runtime_seconds(state.depletion_mm, precip_rate_mm_hr, capture_efficiency, max_duration_s)
    } else {
        0
    };
    ZoneBalanceSummary {
        depletion_mm: state.depletion_mm,
        taw_mm: taw,
        raw_mm: raw,
        kc,
        etc_today_mm: etc,
        needs_irrigation: needs,
        planned_runtime_s: runtime,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etc_scales_by_kc_and_heat() {
        let baseline = etc_mm(5.0, 0.90, 1.0);
        let hot = etc_mm(5.0, 0.90, 1.20);
        assert!((baseline - 4.5).abs() < 1e-9);
        assert!((hot - 5.4).abs() < 1e-9);
    }

    #[test]
    fn step_drains_with_et_refills_with_rain() {
        let mut s = ZoneWaterState { depletion_mm: 10.0 };
        step(&mut s, 5.0, 0.0, 0.0, 0.70, 60.0);
        assert!((s.depletion_mm - 15.0).abs() < 1e-9);
        step(&mut s, 0.0, 20.0, 0.0, 0.70, 60.0); // 20mm rain * 0.7 capture = 14mm effective
        assert!((s.depletion_mm - 1.0).abs() < 1e-9);
    }

    #[test]
    fn step_clamps_below_zero_at_field_capacity() {
        let mut s = ZoneWaterState { depletion_mm: 2.0 };
        step(&mut s, 0.0, 50.0, 0.0, 0.70, 60.0); // big rain
        assert_eq!(s.depletion_mm, 0.0, "should not go negative (above FC = runoff)");
    }

    #[test]
    fn step_clamps_above_taw_at_wilting_point() {
        let mut s = ZoneWaterState { depletion_mm: 55.0 };
        step(&mut s, 30.0, 0.0, 0.0, 0.70, 60.0); // would push to 85; TAW=60 caps
        assert_eq!(s.depletion_mm, 60.0);
    }

    #[test]
    fn should_irrigate_at_raw_threshold() {
        assert!(!should_irrigate(20.0, 30.0));
        assert!(should_irrigate(30.0, 30.0));
        assert!(should_irrigate(45.0, 30.0));
    }

    #[test]
    fn refill_runtime_cap_honored() {
        // 20 mm depletion, 15 mm/hr precip rate, 70% capture -> 28.57 mm gross
        // /15 mm/hr = 1.905 hr = 6858 s. Cap at 3600 -> 3600.
        let s = refill_runtime_seconds(20.0, 15.0, 0.70, 3600);
        assert_eq!(s, 3600);
    }

    #[test]
    fn refill_runtime_proportional_to_deficit() {
        // 5 mm depletion, 15 mm/hr precip, 70% eff -> 7.14 mm gross / 15 = 0.476hr = 1714s
        let s = refill_runtime_seconds(5.0, 15.0, 0.70, 7200);
        assert!(s > 1700 && s < 1800, "got {s}");
    }

    #[test]
    fn summarize_st_augustine_summer_day() {
        let summary = summarize(
            GrassSpecies::StAugustine,
            SoilTexture::SandyLoam,
            None,
            None,
            ZoneWaterState { depletion_mm: 25.0 },
            6.0,  // mid-summer FL ET0
            1.10, // mild heat advisory
            196,  // mid-July
            28.5, // Florida latitude (Northern hemisphere)
            14.0, // 14 mm/hr rotor
            0.70,
            3600,
        );
        // Sandy loam at 150mm depth: TAW = (0.23-0.10)*150 = 19.5 mm
        assert!((summary.taw_mm - 19.5).abs() < 0.1);
        // RAW = TAW * 0.50 (St Aug MAD) = 9.75 mm
        assert!((summary.raw_mm - 9.75).abs() < 0.1);
        // Kc(July) ~ 1.00 for St. Aug
        assert!((summary.kc - 1.00).abs() < 0.05);
        // ETc = 6 * 1.0 * 1.10 = 6.6
        assert!((summary.etc_today_mm - 6.6).abs() < 0.1);
        // Depletion 25mm > RAW 9.75 mm -> irrigate
        assert!(summary.needs_irrigation);
        assert!(summary.planned_runtime_s > 0);
    }

    use crate::config::schema::SoilTexture;
}
