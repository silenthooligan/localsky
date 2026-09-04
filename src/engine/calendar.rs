// The deployment's calendar, as something the engine is HANDED rather
// than something it reaches out and reads.
//
// Four engine modules used to call `crate::timeutil` directly for "what
// local day is this instant" and "what is the yard's UTC offset". That is
// ambient state: it resolves against a process-wide timezone the engine
// does not own, which made the engine's answers depend on where the
// process runs. Two consequences, both of which bit:
//
//   * Tests inherited the runner's clock. Fixtures that pinned a
//     midnight-to-sunrise window passed on a machine whose zone matched
//     the fixture's coordinates and failed in the UTC build container,
//     twice, on unrelated changes.
//   * The modules could not compile for the browser, because resolving a
//     named zone needs the full timezone database.
//
// A `Calendar` is a pair of plain function pointers. The server passes
// the timezone-aware implementations from `timeutil`; a test passes
// `Calendar::utc()` or `Calendar::fixed_offset(secs)` and gets the same
// answer on every machine. No trait objects, no lifetimes, and the
// struct stays `Copy`, so threading it costs nothing.

use chrono::{DateTime, NaiveDate, Utc};

/// How instants map to calendar days for THIS deployment.
#[derive(Debug, Clone, Copy)]
pub struct Calendar {
    /// The local calendar day an instant falls in. `None` when the epoch
    /// is not representable.
    pub local_date: fn(i64) -> Option<NaiveDate>,
    /// The `[start, end)` UTC instants of a local calendar day. `None`
    /// when local midnight is ambiguous or does not exist, which is what
    /// a DST transition produces, so callers keep failing safe there.
    pub day_bounds_utc: fn(NaiveDate) -> Option<(DateTime<Utc>, DateTime<Utc>)>,
}

impl Calendar {
    /// A calendar with no offset. Every instant maps to its UTC day and
    /// every day runs midnight to midnight UTC. The default for tests,
    /// because it is the one answer that cannot vary by machine.
    pub fn utc() -> Self {
        Self {
            local_date: |epoch| Utc.timestamp_opt(epoch, 0).single().map(|d| d.date_naive()),
            day_bounds_utc: |day| {
                let start = day.and_hms_opt(0, 0, 0)?;
                let end = day.succ_opt()?.and_hms_opt(0, 0, 0)?;
                Some((start.and_utc(), end.and_utc()))
            },
        }
    }
}

impl Default for Calendar {
    /// UTC. A caller that forgets to supply the deployment's calendar
    /// gets a deterministic answer rather than whatever zone the process
    /// happens to be running in.
    fn default() -> Self {
        Self::utc()
    }
}

use chrono::TimeZone;

#[cfg(test)]
mod tests {
    use super::*;

    /// No engine module reaches for the process's timezone. Every
    /// calendar question is answered by a `Calendar` the caller supplies,
    /// which is what lets the whole suite pass under any host zone. This
    /// reads the engine's own source: a new `crate::timeutil::` call in
    /// there would reintroduce ambient state and fail here rather than in
    /// a fixture six months later, in a timezone nobody runs.
    #[test]
    fn the_engine_asks_no_process_for_the_time() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("engine source directory") {
            let path = entry.expect("readable entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            // This module names the helpers it wraps, in prose.
            if path.file_name().and_then(|f| f.to_str()) == Some("calendar.rs") {
                continue;
            }
            let body = std::fs::read_to_string(&path).expect("readable source");
            for (n, line) in body.lines().enumerate() {
                if line.contains("crate::timeutil::") {
                    offenders.push(format!(
                        "{}:{}: {}",
                        path.file_name().and_then(|f| f.to_str()).unwrap_or("?"),
                        n + 1,
                        line.trim()
                    ));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "the engine must be HANDED its calendar, not read one:
  {}",
            offenders.join(
                "
  "
            )
        );
    }

    #[test]
    fn the_utc_calendar_is_the_same_everywhere() {
        let c = Calendar::utc();
        // 03:00 UTC on a known day. In UTC that instant belongs to that
        // day; on a machine west of Greenwich an ambient calendar would
        // call it the day before, which is the whole point of pinning it.
        let day = NaiveDate::from_ymd_opt(2026, 5, 26).unwrap();
        let epoch = day.and_hms_opt(3, 0, 0).unwrap().and_utc().timestamp();
        let d = (c.local_date)(epoch).expect("representable");
        assert_eq!(d, day);
        let (start, end) = (c.day_bounds_utc)(d).expect("representable day");
        assert_eq!(end.timestamp() - start.timestamp(), 86_400);
        assert!(start.timestamp() <= epoch && epoch < end.timestamp());
    }
}
