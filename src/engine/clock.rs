// The deployment's clock, as types that cannot be assembled wrongly.
//
// The defect this module exists to make unwritable: `skip_rules::Inputs`
// carried an instant (`now_epoch`) and the UTC offset in force at that
// instant (`utc_offset_seconds`) as two independent fields. One writer
// filled both honestly. Another filled the instant and left the offset at
// its `Default`, which is zero, which reads as UTC. Nothing in the type
// system objected, because a pair of integers is a pair of integers.
//
// The 7-day watering strip was the second writer. It also handed the
// evaluator a forecast provider's DAY MARKER as if it were an instant.
// A marker is a label, and its convention is per-provider: Open-Meteo
// stamps local midnight, NWS the 06:00 daytime period start, met.no local
// noon, OpenWeather a midday value. Read in UTC, an NWS marker for a US
// Eastern yard is hour 10, which is the first hour of a great many
// jurisdictions' midday watering bans. Every day of the week came back
// blocked, including the days the operator was allowed to water.
//
// So there are three ideas here, and the whole point is that they are
// three DIFFERENT types:
//
//   * `CivilDay` is a day. It knows its weekday. It has no instant, no
//     epoch conversion, and no arithmetic against one.
//   * `DayMarker` is what a provider stamped on a row. It cannot be asked
//     what hour it is, cannot be compared to an instant, and cannot be
//     added to anything. The only useful thing you can do with one is ask
//     the deployment `Calendar` which `CivilDay` it labels.
//   * `Zoned` is an instant together with the offset in force AT that
//     instant. It is the only type in the engine that answers `.hour()`,
//     and only `Calendar::at` can mint one.
//
// `DecisionTime` then replaces the pair outright. Its three states are the
// three honest answers to "when is this decision about", and none of them
// is a zero that means four things at once.

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, Timelike, Weekday};
use serde::{Deserialize, Serialize};

/// A civil calendar day in the deployment's frame. A LABEL, never an
/// instant.
///
/// Deliberately absent, and each omission is a bug that used to be
/// writable: `Timelike` (asking a day what hour it is), `From<CivilDay>
/// for i64` and any `Add`/`Sub` (doing epoch arithmetic on a day), and
/// `PartialOrd` against an instant (comparing a day label to a clock
/// reading). Ordering against another `CivilDay` is fine and is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CivilDay(NaiveDate);

impl CivilDay {
    pub const fn from_naive(d: NaiveDate) -> Self {
        Self(d)
    }

    pub fn naive(self) -> NaiveDate {
        self.0
    }

    /// A genuine day fact, which is why weekday gating stays exact even
    /// on a cell whose dispatch instant is unknowable.
    pub fn weekday(self) -> Weekday {
        self.0.weekday()
    }

    pub fn year(self) -> i32 {
        self.0.year()
    }

    /// Day of year. The solar declination term wants this.
    pub fn ordinal(self) -> u32 {
        self.0.ordinal()
    }

    pub fn month(self) -> u32 {
        self.0.month()
    }

    pub fn succ(self) -> Option<Self> {
        self.0.succ_opt().map(Self)
    }

    pub fn pred(self) -> Option<Self> {
        self.0.pred_opt().map(Self)
    }

    /// Whole days from `self` to `other`, negative when `other` is
    /// earlier. Day arithmetic on days is legitimate; it is epoch
    /// arithmetic on days that is not.
    pub fn days_until(self, other: Self) -> i64 {
        (other.0 - self.0).num_days()
    }
}

/// What a forecast provider stamped on a daily row.
///
/// Wire-identical to the `time_epoch: i64` it replaces, via the
/// [`marker_epoch`] serde adapter, which maps `0` to and from "unknown".
/// Zero is already every adapter's own sentinel for a row whose time did
/// not parse, so `Default` meaning "no day" is the honest reading rather
/// than a new convention.
///
/// The API surface is deliberately tiny. There is no `.hour()`, no
/// `Datelike`, no `Timelike`, no comparison against an instant and no
/// arithmetic, because every one of those is a way to mistake a label for
/// a time. Ask the [`Calendar`](crate::engine::calendar::Calendar) which
/// day it means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct DayMarker(Option<i64>);

impl DayMarker {
    /// Build a marker from a provider stamp.
    ///
    /// The name states the one property every shipped provider actually
    /// satisfies: whatever hour the stamp encodes, it falls INSIDE the
    /// local day the row is about. Midnight, 06:00, noon, 18:00 and a
    /// midday value all satisfy it, which is why resolving the day
    /// through the deployment calendar is correct for all of them and
    /// reading the hour off the stamp is correct for none of them.
    pub const fn inside_local_day(epoch: i64) -> Self {
        if epoch == 0 {
            Self(None)
        } else {
            Self(Some(epoch))
        }
    }

    pub const fn unknown() -> Self {
        Self(None)
    }

    pub const fn is_known(self) -> bool {
        self.0.is_some()
    }

    /// The raw stamp, for logging and for checking a provider against its
    /// declared convention.
    ///
    /// Named at length on purpose. This is the one hole the type wall
    /// cannot close, because something has to be able to see the original
    /// number, so it is spelled so that it cannot appear in a diff by
    /// accident and so the engine's guard test can find it.
    pub const fn provenance_epoch_not_an_instant(self) -> Option<i64> {
        self.0
    }
}

/// Serde adapter keeping [`DayMarker`] on the wire exactly as the `i64`
/// it replaced: a plain integer, with `0` for unknown. No nulls, no
/// nested objects, so stored snapshots and any client reading
/// `time_epoch` keep working across the upgrade.
pub mod marker_epoch {
    use super::DayMarker;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(m: &DayMarker, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i64(m.provenance_epoch_not_an_instant().unwrap_or(0))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DayMarker, D::Error> {
        Ok(DayMarker::inside_local_day(i64::deserialize(d)?))
    }
}

/// An instant together with the UTC offset in force AT that instant.
///
/// The offset is not a deployment-wide constant. A yard on a zone that
/// observes daylight saving has two of them in a year, and a 7-day
/// forecast can straddle the change. Sampling the offset once and
/// applying it to seven forward days is how a strip ends up judging a
/// November morning with an August offset.
///
/// The inner value is private and there is no public constructor:
/// [`Calendar::at`](crate::engine::calendar::Calendar::at) is the only
/// producer, so an offset can never be invented next to an instant that
/// does not have it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zoned(DateTime<FixedOffset>);

impl Zoned {
    /// The only constructor, private to this module. `Calendar::at` is
    /// implemented here so that it, and nothing else, can call this.
    pub(super) fn from_parts(epoch: i64, offset_s: i32) -> Option<Self> {
        let offset = FixedOffset::east_opt(offset_s)?;
        Some(Self(
            DateTime::from_timestamp(epoch, 0)?.with_timezone(&offset),
        ))
    }

    pub fn epoch(self) -> i64 {
        self.0.timestamp()
    }

    pub fn offset_seconds(self) -> i32 {
        self.0.offset().local_minus_utc()
    }

    pub fn day(self) -> CivilDay {
        CivilDay(self.0.date_naive())
    }

    pub fn weekday(self) -> Weekday {
        self.0.weekday()
    }

    /// Local hour, 0..=23. The wall-clock question, answerable only here.
    pub fn hour(self) -> u32 {
        self.0.hour()
    }

    pub fn minute(self) -> u32 {
        self.0.minute()
    }

    /// Rendering in the deployment's frame, so a timestamp shown to the
    /// operator and the decision made about it cannot disagree.
    pub fn format(self, fmt: &str) -> String {
        self.0.format(fmt).to_string()
    }

    pub fn inner(self) -> DateTime<FixedOffset> {
        self.0
    }
}

/// WHEN a decision is about.
///
/// This replaces the `now_epoch` + `utc_offset_seconds` pair. The pair
/// had a fourth state nobody meant: both zero, which read as "midnight
/// UTC on 1 January 1970, in a deployment running UTC" and was
/// indistinguishable from "unset". Each state below is one fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecisionTime {
    /// No clock supplied. Day gates and hour gates both abstain. This is
    /// the honest spelling of the rule editor's preview, which
    /// deliberately neutralizes the control gates so an operator sees
    /// what their condition does rather than what today happens to be.
    #[default]
    Unknown,
    /// A whole civil day whose dispatch instant is not knowable: polar
    /// latitudes where the sun does not rise, or an install with no
    /// location set. Day facts bind; hour facts abstain.
    Day(CivilDay),
    /// A real instant in the deployment's frame. Every gate binds.
    At(Zoned),
}

impl DecisionTime {
    /// Resolve an instant in a calendar. `Unknown` when the epoch is not
    /// representable there, which is the honest answer and not a zero.
    pub fn at(cal: crate::engine::calendar::Calendar, epoch: i64) -> Self {
        cal.at(epoch).map(Self::At).unwrap_or(Self::Unknown)
    }

    /// The civil day this decision is about, when there is one.
    pub fn day(self) -> Option<CivilDay> {
        match self {
            Self::Unknown => None,
            Self::Day(d) => Some(d),
            Self::At(z) => Some(z.day()),
        }
    }

    /// The instant, when one is known. `None` means hour gates must
    /// abstain rather than guess.
    pub fn zoned(self) -> Option<Zoned> {
        match self {
            Self::At(z) => Some(z),
            _ => None,
        }
    }

    pub fn is_known(self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

/// One instant, minted once, that a whole pass of work is about.
///
/// A snapshot build reads the clock a dozen times with database awaits
/// between the reads, and every read is correct on its own. They are
/// still different instants. Across local midnight the restriction cap
/// can be judged on Sunday while the verdict is judged on Monday, the
/// manual-override weekday can disagree with the weekday the restriction
/// used, and the day-of-year the crop coefficient uses can differ from
/// the one the soil projection uses.
///
/// This is the same shape as the defect that prompted this module, one
/// level up: several values that must describe one moment, obtained
/// separately. So the moment is obtained once and handed down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tick {
    at: Zoned,
    calendar: crate::engine::calendar::Calendar,
}

impl Tick {
    /// Mint a tick at a known instant.
    pub fn at(cal: crate::engine::calendar::Calendar, epoch: i64) -> Option<Self> {
        cal.at(epoch).map(|at| Self { at, calendar: cal })
    }

    /// The instant everything in this pass is about.
    pub fn zoned(&self) -> Zoned {
        self.at
    }

    pub fn calendar(&self) -> crate::engine::calendar::Calendar {
        self.calendar
    }

    pub fn epoch(&self) -> i64 {
        self.at.epoch()
    }

    /// The deployment's civil day for this tick.
    pub fn day(&self) -> CivilDay {
        self.at.day()
    }

    pub fn weekday(&self) -> Weekday {
        self.at.weekday()
    }

    /// Day of year, for the seasonal terms.
    pub fn ordinal(&self) -> u16 {
        self.at.day().ordinal() as u16
    }

    pub fn month(&self) -> u32 {
        self.at.day().month()
    }

    /// When this decision is about, for the engine.
    pub fn decision_time(&self) -> DecisionTime {
        DecisionTime::At(self.at)
    }
}

/// Why a local calendar day might not begin cleanly.
///
/// The old shape collapsed all of this into `Option`, and the two
/// production readers of that `None` disagreed about what it meant. One
/// treated it as "no constraint" and dropped a safety clamp. The other
/// treated it as "no record" and re-armed a dispatch that had already
/// run, which on a midnight-transition zone is a second full irrigation
/// cycle after a restart. Naming the outcomes forces each caller to say
/// which case it is handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalStart {
    /// Local midnight exists exactly once.
    At(i64),
    /// Local midnight does not exist: the clock sprang forward across
    /// it. The day still happens, starting at `resumes_at`.
    Skipped { resumes_at: i64 },
    /// Local midnight happens twice: the clock fell back across it. The
    /// day still happens, starting at `first`.
    Twice { first: i64, second: i64 },
    /// The date itself is out of range.
    Unrepresentable,
}

impl LocalStart {
    /// The instant the day begins, for every case in which the day
    /// exists at all. Only `Unrepresentable` yields `None`, so a caller
    /// that wants "does this day happen" gets a straight answer and
    /// cannot accidentally read a DST transition as a missing day.
    pub fn instant(self) -> Option<i64> {
        match self {
            Self::At(e) => Some(e),
            Self::Skipped { resumes_at } => Some(resumes_at),
            Self::Twice { first, .. } => Some(first),
            Self::Unrepresentable => None,
        }
    }

    pub fn exists(self) -> bool {
        self.instant().is_some()
    }
}

/// Strip comments and string literals, so a guard test matches CODE and
/// not prose about code. The previous guard matched raw lines, which
/// meant the moment a module explained the rule in a comment it started
/// tripping over its own explanation.
#[cfg(test)]
#[cfg(test)]
pub fn code_only(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let (mut i, n) = (0usize, b.len());
    while i < n {
        // Line comment.
        if b[i] == '/' && i + 1 < n && b[i + 1] == '/' {
            while i < n && b[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // Block comment.
        if b[i] == '/' && i + 1 < n && b[i + 1] == '*' {
            i += 2;
            while i + 1 < n && !(b[i] == '*' && b[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        // String literal, including escapes.
        if b[i] == '"' {
            i += 1;
            while i < n && b[i] != '"' {
                if b[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(n);
            out.push(' ');
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// The text a person reads: string literal contents only, everything
/// else blanked, newlines kept so line numbers survive. The complement
/// of `code_only`. Comments and the `#[cfg(test)]` region are dropped,
/// since a test may quote the very words a guard bans.
#[cfg(test)]
pub fn strings_only(src: &str) -> String {
    let prose = src.split("#[cfg(test)]").next().unwrap_or("");
    let b: Vec<char> = prose.chars().collect();
    let mut out = String::with_capacity(prose.len());
    let (mut i, n) = (0usize, b.len());
    while i < n {
        if b[i] == '/' && i + 1 < n && b[i + 1] == '/' {
            while i < n && b[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if b[i] == '"' {
            i += 1;
            while i < n && b[i] != '"' {
                if b[i] == '\\' && i + 1 < n {
                    out.push(b[i + 1]);
                    i += 2;
                    continue;
                }
                out.push(b[i]);
                i += 1;
            }
            i = (i + 1).min(n);
            out.push(' ');
            continue;
        }
        if b[i] == '\n' {
            out.push('\n');
        }
        i += 1;
    }
    out
}

/// Every `.rs` file under `dir`, recursively.
#[cfg(test)]
pub fn rust_sources(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(rust_sources(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NY_EDT: i32 = -4 * 3600;
    const KOLKATA: i32 = 5 * 3600 + 1800;

    fn day(y: i32, m: u32, d: u32) -> CivilDay {
        CivilDay::from_naive(NaiveDate::from_ymd_opt(y, m, d).expect("valid date"))
    }

    /// The production stamp from the reported defect: the NWS daytime
    /// period start for Saturday 5 September 2026 in US Eastern, which
    /// is 10:00 UTC. Read in UTC it is hour 10; read in the yard's own
    /// frame it is 06:00, a legal watering hour nearly everywhere.
    const NWS_SEPT_5: i64 = 1_788_602_400;

    /// The engine asks no process for the time.
    ///
    /// Every calendar question is answered by a `Calendar` the caller
    /// supplies, which is what lets the suite pass under any host zone.
    ///
    /// This replaces a guard that walked one directory level, matched a
    /// single literal substring, matched inside comments, and exempted a
    /// whole file by name. It could not see a future subdirectory, it
    /// could not see `chrono::Local`, and its own module's tests built
    /// `DateTime<Local>` for all fourteen of their fixtures while it
    /// stayed silent. It would not have caught the defect that prompted
    /// this module.
    #[test]
    fn the_engine_asks_no_process_for_the_time() {
        const BANNED: &[(&str, &str)] = &[
            (
                "crate::timeutil::",
                "the deployment clock is handed in, not read",
            ),
            (
                "chrono::Local",
                "that is the machine's zone, not the yard's",
            ),
            ("Local::now", "that is the machine's zone, not the yard's"),
            (
                "Local.timestamp",
                "that is the machine's zone, not the yard's",
            ),
            (
                "Local.with_ymd",
                "that is the machine's zone, not the yard's",
            ),
            ("Utc::now", "the engine is given its instants"),
            ("SystemTime::now", "the engine is given its instants"),
            (
                "FixedOffset::east_opt",
                "only Calendar::at may pair an offset with an instant",
            ),
            (
                "provenance_epoch_not_an_instant",
                "a day marker's raw stamp is not an instant",
            ),
        ];
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine");
        let mut offenders = Vec::new();
        for path in rust_sources(&dir) {
            let name = path
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or_default()
                .to_string();
            // This module IS the seam. It is the one place allowed to
            // pair an offset with an instant and to read a raw stamp.
            if name == "clock.rs" {
                continue;
            }
            let body = code_only(&std::fs::read_to_string(&path).expect("readable source"));
            for (n, line) in body.lines().enumerate() {
                for (pat, why) in BANNED {
                    // Calendar::day_of is the sanctioned resolver, and
                    // resolving is exactly what it does with the stamp.
                    if *pat == "provenance_epoch_not_an_instant" && name == "calendar.rs" {
                        continue;
                    }
                    if line.contains(pat) {
                        offenders.push(format!("{name}:{}: {pat} ({why})", n + 1));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "the engine must be HANDED its calendar, not read one:\n  {}",
            offenders.join("\n  ")
        );
    }

    /// The forecast adapters do not read the container's clock either.
    ///
    /// A separate scope because the engine-only guard is blind to it, and
    /// this is where it actually bit: the NWS adapter bucketed rain into
    /// day rows using the container's zone, so a UTC container serving a
    /// US Eastern yard credited evening rain to the next day, and that
    /// total feeds the engine's rain gates directly.
    #[test]
    fn the_forecast_adapters_do_not_read_the_container_clock() {
        const BANNED: &[&str] = &["chrono::Local", "Local::now", "Local.timestamp"];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offenders = Vec::new();
        for sub in ["src/sources", "src/forecast"] {
            for path in rust_sources(&root.join(sub)) {
                let body = code_only(&std::fs::read_to_string(&path).expect("readable source"));
                for (n, line) in body.lines().enumerate() {
                    for pat in BANNED {
                        if line.contains(pat) {
                            offenders.push(format!(
                                "{}:{}: {pat}",
                                path.file_name().and_then(|f| f.to_str()).unwrap_or("?"),
                                n + 1
                            ));
                        }
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "forecast adapters must bucket by the deployment calendar:\n  {}",
            offenders.join("\n  ")
        );
    }

    /// The guard reads code, not prose. Without this, any module that
    /// explains the rule in a comment trips over its own explanation,
    /// which is how the previous guard ended up exempting a whole file.
    #[test]
    fn the_guard_ignores_comments_and_strings() {
        let stripped =
            code_only("// mentions chrono::Local here\nlet m = \"Utc::now\";\nlet real = 1;\n");
        assert!(!stripped.contains("chrono::Local"));
        assert!(!stripped.contains("Utc::now"));
        assert!(stripped.contains("let real = 1;"));
    }

    /// One tick answers every question about its own moment, so the
    /// answers cannot disagree with each other.
    #[test]
    fn a_tick_is_one_moment_asked_many_ways() {
        let cal = crate::engine::calendar::Calendar::fixed_offset(NY_EDT).expect("EDT");
        // Saturday 5 September 2026, 23:59 EDT: one minute before the
        // day rolls, which is exactly where separate reads diverge.
        let t = Tick::at(cal, 1_788_667_140).expect("representable");
        assert_eq!(t.day(), day(2026, 9, 5));
        assert_eq!(t.weekday(), Weekday::Sat);
        assert_eq!(t.month(), 9);
        assert_eq!(t.ordinal(), 248);
        assert_eq!(t.epoch(), 1_788_667_140);
        assert_eq!(t.decision_time().day(), Some(day(2026, 9, 5)));
        // The instant the engine judges and the day the seasonal terms
        // use are the same moment, by construction.
        assert_eq!(t.decision_time().zoned().map(|z| z.day()), Some(t.day()));
    }

    #[test]
    fn a_tick_is_minted_in_the_deployment_frame_not_utc() {
        let cal = crate::engine::calendar::Calendar::fixed_offset(NY_EDT).expect("EDT");
        // 03:00 UTC is still the previous evening in New York.
        let epoch = 1_788_663_600;
        let ny = Tick::at(cal, epoch).expect("representable");
        let utc = Tick::at(crate::engine::calendar::Calendar::utc(), epoch).expect("representable");
        assert_ne!(ny.day(), utc.day(), "the frame decides the day");
    }

    #[test]
    fn a_marker_read_in_the_wrong_frame_is_the_whole_bug() {
        let utc = Zoned::from_parts(NWS_SEPT_5, 0).expect("representable");
        let yard = Zoned::from_parts(NWS_SEPT_5, NY_EDT).expect("representable");
        assert_eq!(utc.hour(), 10, "hour 10 is the first banned hour");
        assert_eq!(yard.hour(), 6, "the same instant is 06:00 in the yard");
        // Same instant, same civil day here, different legal answer.
        assert_eq!(utc.epoch(), yard.epoch());
        assert_eq!(yard.day(), day(2026, 9, 5));
    }

    #[test]
    fn a_zoned_carries_the_offset_it_was_minted_with() {
        let z = Zoned::from_parts(NWS_SEPT_5, NY_EDT).expect("representable");
        assert_eq!(z.offset_seconds(), NY_EDT);
        // Round-trips: the offset is not recoverable from the epoch, which
        // is exactly why it has to travel with it.
        assert_eq!(Zoned::from_parts(z.epoch(), z.offset_seconds()), Some(z));
    }

    #[test]
    fn sub_hour_offsets_survive_as_seconds() {
        // India is +05:30. An offset stored in hours would round this to
        // +05:00 or +06:00 and move every gate by half an hour.
        let z = Zoned::from_parts(NWS_SEPT_5, KOLKATA).expect("representable");
        assert_eq!(z.offset_seconds(), KOLKATA);
        assert_eq!(z.hour(), 15);
        assert_eq!(z.minute(), 30);
    }

    #[test]
    fn an_out_of_range_offset_yields_no_zoned_rather_than_a_wrong_one() {
        assert_eq!(Zoned::from_parts(NWS_SEPT_5, 86_400), None);
        assert_eq!(Zoned::from_parts(NWS_SEPT_5, -86_400), None);
        // Chatham, the widest real offset, must still work.
        assert!(Zoned::from_parts(NWS_SEPT_5, 12 * 3600 + 2700).is_some());
    }

    #[test]
    fn a_marker_of_zero_is_unknown_not_1970() {
        assert!(!DayMarker::inside_local_day(0).is_known());
        assert_eq!(DayMarker::default(), DayMarker::unknown());
        assert!(!DayMarker::default().is_known());
        assert!(DayMarker::inside_local_day(NWS_SEPT_5).is_known());
    }

    #[test]
    fn the_marker_wire_shape_is_still_a_bare_integer() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Row {
            #[serde(rename = "time_epoch", with = "marker_epoch", default)]
            day_marker: DayMarker,
        }
        let known = Row {
            day_marker: DayMarker::inside_local_day(NWS_SEPT_5),
        };
        assert_eq!(
            serde_json::to_string(&known).expect("serializes"),
            format!("{{\"time_epoch\":{NWS_SEPT_5}}}")
        );
        let unknown = Row {
            day_marker: DayMarker::unknown(),
        };
        assert_eq!(
            serde_json::to_string(&unknown).expect("serializes"),
            "{\"time_epoch\":0}"
        );
        // Old payloads keep parsing, including ones that omit the field.
        assert_eq!(
            serde_json::from_str::<Row>(&format!("{{\"time_epoch\":{NWS_SEPT_5}}}"))
                .expect("parses"),
            known
        );
        assert_eq!(serde_json::from_str::<Row>("{}").expect("parses"), unknown);
    }

    #[test]
    fn decision_time_has_no_state_that_means_two_things() {
        assert_eq!(DecisionTime::default(), DecisionTime::Unknown);
        assert!(!DecisionTime::Unknown.is_known());
        assert_eq!(DecisionTime::Unknown.day(), None);
        assert_eq!(DecisionTime::Unknown.zoned(), None);

        let d = day(2026, 9, 6);
        let day_only = DecisionTime::Day(d);
        assert_eq!(day_only.day(), Some(d), "day facts still bind");
        assert_eq!(day_only.zoned(), None, "hour facts must abstain");

        let z = Zoned::from_parts(NWS_SEPT_5, NY_EDT).expect("representable");
        let at = DecisionTime::At(z);
        assert_eq!(at.day(), Some(day(2026, 9, 5)));
        assert_eq!(at.zoned(), Some(z));
    }

    #[test]
    fn a_civil_day_knows_its_weekday_without_any_instant() {
        // 5 September 2026 is a Saturday. This is a property of the day
        // itself and needs no clock, which is why weekday gating stays
        // exact for a cell whose dispatch instant is unknowable.
        assert_eq!(day(2026, 9, 5).weekday(), Weekday::Sat);
        assert_eq!(day(2026, 9, 6).weekday(), Weekday::Sun);
        assert_eq!(day(2026, 9, 5).succ(), Some(day(2026, 9, 6)));
        assert_eq!(day(2026, 9, 5).pred(), Some(day(2026, 9, 4)));
        assert_eq!(day(2026, 9, 5).days_until(day(2026, 9, 10)), 5);
        assert_eq!(day(2026, 9, 10).days_until(day(2026, 9, 5)), -5);
    }

    #[test]
    fn a_dst_transition_day_still_exists() {
        // The old shape returned None here and two callers read that
        // None in opposite directions. Every arm but Unrepresentable now
        // answers "yes, the day happens, and here is when it starts".
        assert_eq!(LocalStart::At(10).instant(), Some(10));
        assert_eq!(
            LocalStart::Skipped { resumes_at: 3600 }.instant(),
            Some(3600)
        );
        assert_eq!(
            LocalStart::Twice {
                first: 100,
                second: 3700
            }
            .instant(),
            Some(100)
        );
        assert!(LocalStart::Skipped { resumes_at: 3600 }.exists());
        assert!(LocalStart::Twice {
            first: 100,
            second: 3700
        }
        .exists());
        assert_eq!(LocalStart::Unrepresentable.instant(), None);
        assert!(!LocalStart::Unrepresentable.exists());
    }

    #[test]
    fn a_civil_day_round_trips_on_the_wire_as_a_plain_date() {
        let d = day(2026, 9, 5);
        assert_eq!(
            serde_json::to_string(&d).expect("serializes"),
            "\"2026-09-05\""
        );
        assert_eq!(
            serde_json::from_str::<CivilDay>("\"2026-09-05\"").expect("parses"),
            d
        );
    }
}
