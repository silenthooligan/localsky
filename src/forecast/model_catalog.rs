// Open-Meteo forecast model catalog: every weather model the forecast
// fetch can pin via `&models=<id>`, as plain static data with no
// ssr-only dependencies so both the server (refresher URL builder,
// /api/v1/sources/openmeteo/models) and a future WASM settings UI
// compile it (the radar_catalog / gates_catalog precedent). Every id
// here was verified live on 2026-06-12 (HTTP 200 with full non-null
// hourly temp+precip at Munich, plus extra probes for the
// region-limited entries); do not add models without the same
// verification. Notably there is NO `ecmwf_seamless`: that id returns
// HTTP 400 (verified), `ecmwf_ifs025` is the correct deterministic id.

use serde::Serialize;

/// The Open-Meteo default model selection. When a source is configured
/// with this id the fetch URL carries no `models=` parameter at all,
/// keeping it byte-identical to the pre-model-selection URL.
pub const DEFAULT_MODEL: &str = "best_match";

/// One selectable forecast model. Serialized verbatim into the
/// /api/v1/sources/openmeteo/models response a future settings UI
/// reads (id, label, agency, region).
#[derive(Debug, Clone, Serialize)]
pub struct ForecastModel {
    /// Stable Open-Meteo model id; what `sources[].config.model` and
    /// the `&models=` query parameter speak.
    pub id: &'static str,
    /// Picker display label.
    pub label: &'static str,
    /// Issuing agency, for the settings UI.
    pub agency: &'static str,
    /// Human coverage note. "global" models work anywhere; the
    /// seamless variants nest a high-res regional model inside a
    /// global fallback; the two hard-regional entries return HTTP 400
    /// outside their domain (noted inline).
    pub region: &'static str,
}

/// Catalog of every verified model, in picker order (the default
/// first, then by rough global reach).
pub fn models() -> &'static [ForecastModel] {
    &[
        ForecastModel {
            // Open-Meteo's automatic composite: picks the best model
            // per location. The default; carries no models= parameter.
            id: "best_match",
            label: "Best Match (auto composite)",
            agency: "Open-Meteo default",
            region: "global",
        },
        ForecastModel {
            id: "icon_seamless",
            label: "DWD ICON (Germany)",
            agency: "DWD Germany",
            region: "global (ICON Global + ICON-EU + ICON-D2 nests)",
        },
        ForecastModel {
            id: "gfs_seamless",
            label: "NOAA GFS (US)",
            agency: "NOAA US",
            region: "global (GFS + HRRR US nest)",
        },
        ForecastModel {
            id: "meteofrance_seamless",
            label: "Meteo-France ARPEGE/AROME",
            agency: "Meteo-France",
            region: "global (ARPEGE global + AROME France high-res nest)",
        },
        ForecastModel {
            // Deterministic 0.25 degree IFS. There is no ecmwf_seamless;
            // that id is rejected upstream with HTTP 400 (verified).
            id: "ecmwf_ifs025",
            label: "ECMWF IFS 0.25",
            agency: "ECMWF",
            region: "global",
        },
        ForecastModel {
            id: "ukmo_seamless",
            label: "UK Met Office UKMO",
            agency: "UK Met Office",
            region: "global (UKMO Global 10km + UK 2km UKV nest)",
        },
        ForecastModel {
            id: "jma_seamless",
            label: "JMA GSM/MSM (Japan)",
            agency: "JMA Japan",
            region: "global (GSM) + Japan high-res (MSM) nest",
        },
        ForecastModel {
            id: "gem_seamless",
            label: "GEM (Canada)",
            agency: "ECCC Canada",
            region: "global (GEM Global + HRDPS Canada nest)",
        },
        ForecastModel {
            id: "cma_grapes_global",
            label: "CMA GRAPES (China)",
            agency: "CMA China",
            region: "global",
        },
        ForecastModel {
            id: "metno_seamless",
            label: "MET Norway Nordic",
            agency: "MET Norway",
            region: "Nordic high-res + global fallback",
        },
        ForecastModel {
            id: "knmi_seamless",
            label: "KNMI HARMONIE (Netherlands)",
            agency: "KNMI Netherlands",
            region: "Benelux/Europe high-res + global fallback",
        },
        ForecastModel {
            id: "dmi_seamless",
            label: "DMI HARMONIE (Denmark)",
            agency: "DMI Denmark",
            region: "Northern/central Europe high-res + global fallback",
        },
        ForecastModel {
            id: "geosphere_seamless",
            label: "GeoSphere AROME (Austria)",
            agency: "GeoSphere Austria",
            region: "Austria/Alps high-res + global fallback",
        },
        ForecastModel {
            // Hard-regional: HTTP 400 "No data is available for this
            // location" outside the CH1/CH2 domain (verified at NYC;
            // Munich is inside, the CH2 domain covers the wider Alps).
            id: "meteoswiss_icon_seamless",
            label: "MeteoSwiss ICON CH1/CH2",
            agency: "MeteoSwiss",
            region: "Switzerland/Alps + central Europe only (HTTP 400 outside domain)",
        },
        ForecastModel {
            // Hard-regional, same 400-outside-domain behavior. No
            // seamless variant exists; this is the only id.
            id: "italia_meteo_arpae_icon_2i",
            label: "ItaliaMeteo ICON-2I (Italy)",
            agency: "ItaliaMeteo/ARPAE",
            region: "Italy + surrounding only (HTTP 400 outside domain)",
        },
    ]
}

pub fn model_by_id(id: &str) -> Option<&'static ForecastModel> {
    models().iter().find(|m| m.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for m in models() {
            assert!(seen.insert(m.id), "duplicate model id '{}'", m.id);
        }
    }

    #[test]
    fn best_match_is_present_default_and_first() {
        assert_eq!(models()[0].id, DEFAULT_MODEL);
        assert!(model_by_id(DEFAULT_MODEL).is_some());
        assert_eq!(
            model_by_id("best_match").unwrap().agency,
            "Open-Meteo default"
        );
    }

    #[test]
    fn ecmwf_seamless_is_deliberately_absent() {
        // Upstream rejects ecmwf_seamless with HTTP 400; the catalog
        // must carry the working deterministic id instead.
        assert!(model_by_id("ecmwf_seamless").is_none());
        assert!(model_by_id("ecmwf_ifs025").is_some());
    }

    #[test]
    fn every_entry_has_display_metadata() {
        for m in models() {
            assert!(!m.label.is_empty(), "model '{}' has no label", m.id);
            assert!(!m.agency.is_empty(), "model '{}' has no agency", m.id);
            assert!(!m.region.is_empty(), "model '{}' has no region", m.id);
        }
    }
}
