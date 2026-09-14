// The one place a physical unit turns into another. Every adapter, the
// engine, the assembly and the views convert through these, so a factor
// is written once and a grep for `25.4` finds this file and nothing else.
//
// Conventions inside LocalSky: temperature in F, depth in inches, speed
// in mph, pressure in inHg, distance in miles; the engine's soil math is
// in millimeters. A source that reports in other units converts at its
// edge; a metric viewer is converted at the display edge.

/// Millimeters per inch, exactly.
pub const MM_PER_IN: f64 = 25.4;
/// Kilometers per statute mile, exactly.
pub const KM_PER_MI: f64 = 1.609_344;
/// Meters per foot, exactly.
pub const M_PER_FT: f64 = 0.304_8;
/// Meters per second per mile per hour, exactly (1609.344 m / 3600 s).
pub const MS_PER_MPH: f64 = 0.447_04;
/// Hectopascals per inch of mercury (conventional, at 0 C).
pub const HPA_PER_INHG: f64 = 33.863_886_67;

pub fn c_to_f(c: f64) -> f64 {
    c * 9.0 / 5.0 + 32.0
}

pub fn f_to_c(f: f64) -> f64 {
    (f - 32.0) * 5.0 / 9.0
}

pub fn mm_to_in(mm: f64) -> f64 {
    mm / MM_PER_IN
}

pub fn in_to_mm(inches: f64) -> f64 {
    inches * MM_PER_IN
}

pub fn kph_to_mph(kph: f64) -> f64 {
    kph / KM_PER_MI
}

pub fn mph_to_kph(mph: f64) -> f64 {
    mph * KM_PER_MI
}

pub fn km_to_mi(km: f64) -> f64 {
    km / KM_PER_MI
}

pub fn mi_to_km(mi: f64) -> f64 {
    mi * KM_PER_MI
}

pub fn ms_to_mph(ms: f64) -> f64 {
    ms / MS_PER_MPH
}

pub fn mph_to_ms(mph: f64) -> f64 {
    mph * MS_PER_MPH
}

pub fn hpa_to_inhg(hpa: f64) -> f64 {
    hpa / HPA_PER_INHG
}

pub fn inhg_to_hpa(inhg: f64) -> f64 {
    inhg * HPA_PER_INHG
}

pub fn m_to_ft(m: f64) -> f64 {
    m / M_PER_FT
}

pub fn ft_to_m(ft: f64) -> f64 {
    ft * M_PER_FT
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn the_anchors_hold() {
        assert!(close(c_to_f(100.0), 212.0));
        assert!(close(f_to_c(32.0), 0.0));
        assert!(close(in_to_mm(1.0), 25.4));
        assert!(close(mm_to_in(25.4), 1.0));
        assert!(close(mi_to_km(1.0), 1.609_344));
        assert!(close(mph_to_ms(1.0), 0.447_04));
        assert!((hpa_to_inhg(1013.25) - 29.92).abs() < 0.005);
        assert!(close(ft_to_m(1.0), 0.304_8));
    }

    #[test]
    fn every_pair_round_trips() {
        for x in [0.0, 0.37, 12.5, 1013.25, -40.0] {
            assert!(close(f_to_c(c_to_f(x)), x));
            assert!(close(mm_to_in(in_to_mm(x)), x));
            assert!(close(kph_to_mph(mph_to_kph(x)), x));
            assert!(close(km_to_mi(mi_to_km(x)), x));
            assert!(close(ms_to_mph(mph_to_ms(x)), x));
            assert!(close(inhg_to_hpa(hpa_to_inhg(x)), x));
            assert!(close(m_to_ft(ft_to_m(x)), x));
        }
        assert!(
            close(c_to_f(-40.0), -40.0),
            "the one point both scales share"
        );
    }

    fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                rust_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    fn production_code(path: &std::path::Path) -> String {
        let src = std::fs::read_to_string(path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name.ends_with("_tests.rs") {
            return String::new();
        }
        crate::engine::clock::code_only(src.split("#[cfg(test)]").next().unwrap())
    }

    /// One definition per conversion: no adapter, engine module or view
    /// carries its own `fn c_to_f`.
    #[test]
    fn no_module_redefines_a_conversion() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&root, &mut files);
        let mut offenders = Vec::new();
        for f in files {
            if f.ends_with("units.rs") && f.parent().is_some_and(|p| p.ends_with("src")) {
                continue;
            }
            let code = production_code(&f);
            for name in [
                "c_to_f",
                "f_to_c",
                "mm_to_in",
                "in_to_mm",
                "kph_to_mph",
                "mph_to_kph",
                "km_to_mi",
                "mi_to_km",
                "ms_to_mph",
                "mph_to_ms",
                "hpa_to_inhg",
                "inhg_to_hpa",
                "m_to_ft",
                "ft_to_m",
            ] {
                if code.contains(&format!("fn {name}(")) {
                    offenders.push(format!("{}: fn {name}", f.display()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "conversions live in crate::units:\n{}",
            offenders.join("\n")
        );
    }

    /// The factor is written here and nowhere else in the pure half and
    /// the adapters; the views read the clock through timefmt.
    #[test]
    fn no_bare_factor_and_no_local_clock_outside_their_owners() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        for dir in ["assembly", "refresher", "engine", "sources", "components"] {
            let mut files = Vec::new();
            rust_files(&root.join(dir), &mut files);
            for f in files {
                let code = production_code(&f);
                for (n, line) in code.lines().enumerate() {
                    if line.contains("25.4") || line.contains("1.609") || line.contains("0.621") {
                        offenders.push(format!("{}:{}: {}", f.display(), n + 1, line.trim()));
                    }
                    if dir == "components" && line.contains("Local::now(") {
                        offenders.push(format!("{}:{}: {}", f.display(), n + 1, line.trim()));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "use crate::units for the factor and timefmt::now_epoch for the clock:\n{}",
            offenders.join("\n")
        );
    }
}
