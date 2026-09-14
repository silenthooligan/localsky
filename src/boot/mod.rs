// Boot, one function per phase; main() calls them in order and serves.
//
// Each phase returns a typed struct the later phases take by reference,
// so what a task needs is visible in a signature rather than captured
// from the locals of a 1,600-line function, and nothing reaches a
// router through a process global: the state a handler reads is the
// state its router was built with. The order is the data dependency:
//
//   logging  -> the subscriber and the log ring
//   storage  -> the demo switch, the history database, the instance id
//   config   -> the config store, the boot config, the policy handles
//   stores   -> the three live stores and the push dispatcher
//   control  -> the controller registry, the refresher, the schedulers
//   sources  -> the bus, the adapters, the bridges, the recorder
//   api      -> every router, mounted once at /api and /api/v1
//
// Two boot-resolved constants are written here and read as statics
// rather than threaded: the deployment timezone (`timeutil`) and the
// instance id (`instance`). Both are resolved before any task spawns
// and never change for the life of the process.

pub mod api;
pub mod config;
pub mod control;
pub mod logging;
pub mod sources;
pub mod storage;
pub mod stores;

#[cfg(test)]
mod tests {
    /// The handles a router reads travel in its state. The only statics
    /// left are the two boot constants named above, the refresher
    /// heartbeat the watchdog reads, the Tempest listener's status, the
    /// stop gate, the metrics registry and the timezone table; a new
    /// `OnceLock` or a `set_*` writer anywhere else is the wiring diagram
    /// growing back.
    #[test]
    fn no_router_reads_a_process_global() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let allowed_once_locks = [
            ("src/timeutil.rs", "CONFIGURED_TZ"),
            ("src/instance.rs", "INSTANCE_ID"),
            ("src/tempest/listener.rs", "CELL"),
        ];
        let allowed_setters = [
            "src/timeutil.rs:set_configured_tz",
            // The writer for the listener-status cell allowed above. The
            // socket lives in a source task and the status is read by
            // /api/v1/health and the wizard, so the two ends never meet in
            // a router's state.
            "src/tempest/listener.rs:set_status",
        ];
        let mut offenders = Vec::new();
        for path in walk(&root) {
            let rel = path
                .strip_prefix(root.parent().unwrap())
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel.ends_with("_tests.rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).unwrap();
            let code = src.split("#[cfg(test)]").next().unwrap();
            for line in code.lines() {
                let t = line.trim();
                if t.starts_with("//") {
                    continue;
                }
                if t.contains("OnceLock<") && t.contains("static ") {
                    let name = t
                        .split("static ")
                        .nth(1)
                        .and_then(|r| r.split(':').next())
                        .unwrap_or("")
                        .trim();
                    if !allowed_once_locks
                        .iter()
                        .any(|(f, n)| rel == *f && name == *n)
                    {
                        offenders.push(format!("{rel}: static {name} is a OnceLock"));
                    }
                }
                // A free function at column 0; a method inside an impl is
                // indented and sets a field of the value it is called on.
                if let Some(rest) = line.strip_prefix("pub fn set_") {
                    let name = format!("set_{}", rest.split('(').next().unwrap_or(""));
                    let key = format!("{rel}:{name}");
                    if !allowed_setters.contains(&key.as_str()) {
                        offenders.push(format!("{rel}: {name} writes a process global"));
                    }
                }
            }
        }
        // `timefmt::set_clock_12h` is a thread-local viewer preference and
        // `metrics::set_gauge` records a value; neither is boot wiring.
        offenders.retain(|o| {
            !o.contains("timefmt.rs: set_clock_12h") && !o.contains("metrics.rs: set_gauge")
        });
        assert!(offenders.is_empty(), "{}", offenders.join("\n"));
    }

    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
        out
    }
}
