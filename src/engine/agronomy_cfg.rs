// Per-zone agronomy, as the engine sees it.
//
// Species, soil texture, slope, root depth and the management-allowed
// depletion are the inputs the soil model and the cycle-and-soak planner
// reason about, so this is engine data. It lived in the refresher, which
// is server-only, and that is what stopped the engine naming the type it
// needed: sequence planning could not move in until this did.

/// structures (main.rs zone_runtime + smart_morning's boot cfg Arc) and
/// silently required a restart.
#[derive(Debug, Clone, Copy)]
pub struct ZoneAgronomyCfg {
    pub sprinkler_type: crate::config::schema::SprinklerType,
    pub precip_rate_mm_hr: Option<f64>,
    pub soil_texture: crate::config::schema::SoilTexture,
    pub slope_pct: f64,
    /// Configured grass/planting species. Feeds `kc_at_doy_lat` for the
    /// zone's crop coefficient, which is where Kc comes from now that the
    /// Smart Irrigation entity's `multiplier` attribute is gone.
    pub species: crate::config::schema::GrassSpecies,
    /// Root-depth override (mm); `None` = the species profile default.
    /// The soil model's TAW/RAW derivation reads it here so an applied
    /// zone edit reshapes the bucket on the next tick.
    pub root_depth_mm: Option<f64>,
    /// MAD override; `None` = the species default. Same hot-reload
    /// contract as `root_depth_mm`.
    pub mad_pct_override: Option<f64>,
    /// Per-zone scheduling-model pin from `ZoneConfig::scheduling_model`;
    /// `None` = the engine default (`WateringPolicy::scheduling_model`).
    pub scheduling_model: Option<crate::config::schema::SchedulingModel>,
}
