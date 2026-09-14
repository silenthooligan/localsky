// The two wrappers every registered controller is put behind: the one
// place a run's length is capped, and the one place a status readback
// is throttled.
//
// Every path that opened a valve carried its own copy of the two-hour
// cap: the API clamped, the manual scheduler clamped, the MQTT and HTTP
// adapters clamped defensively, and smart morning and OpenSprinkler
// direct did not. A cycle-and-soak segment could ask OpenSprinkler for
// a six-hour station run and get it. A cap that lives in four places is
// a cap that is missing from the fifth, so it lives here.
//
// The tick awaited every controller's status() every ten seconds. A
// cloud adapter (B-hyve, Hydrawise, Rain Bird) hit its vendor 8,640
// times a day for a value that changes when a valve does, and Rachio
// grew a private throttle to survive its request budget. The interval an
// adapter declares in `status_poll_interval_s` is honored here, for
// every reader, with the cache dropped on any command through the same
// wrapper so a dispatch is visible on the very next read.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::ports::irrigation_controller::{
    ControllerCaps, ControllerResult, ControllerStatus, DiscoveredZone, IrrigationController,
    RunHandle, RunRecord,
};

/// Longest single valve command LocalSky will ever send, in seconds:
/// two hours. Nothing a lawn needs takes longer in one pass, and a
/// duration past this is a unit mistake (minutes typed as seconds, a
/// seasonal dial applied twice) rather than a decision.
pub const RUN_SECONDS_MAX: u32 = 7200;

/// How often a cloud controller's status is read when the adapter has no
/// tighter figure of its own: once a minute keeps B-hyve, Hydrawise (30
/// calls per five minutes) and Rain Bird inside their budgets with room
/// for the commands, and a valve change still shows within the window
/// the action endpoint tells the UI to wait for.
pub const CLOUD_STATUS_POLL_S: u32 = 60;

/// A controller whose status readback is served from a cache inside the
/// interval it declares. An adapter that declares `None` (a LAN device
/// read on demand) is returned unwrapped and read live every time.
///
/// Every reading it hands out carries the time it was taken
/// (`ControllerStatus::observed_epoch`), because the whole point of the
/// wrapper is that the answer can be older than the question.
pub struct Throttled {
    inner: Arc<dyn IrrigationController>,
    interval: Duration,
    /// Locked only to read or write the entry, never across the fetch: a
    /// command's `forget` must not queue behind a vendor round trip that
    /// can take fifteen seconds, with the dispatcher's per-zone lock held
    /// the whole time.
    cache: tokio::sync::Mutex<Option<Cached>>,
    /// One fetch at a time. The others wait here and then find the answer
    /// in the cache instead of making their own call, which is what the
    /// old lock-across-the-fetch bought at the cost of blocking commands.
    fetching: tokio::sync::Mutex<()>,
    /// Bumped by every command. A fetch that started before a valve moved
    /// and returned after it describes the valve as it was, so it is
    /// handed to its caller but never cached.
    generation: AtomicU64,
}

struct Cached {
    at: Instant,
    status: ControllerStatus,
}

impl Throttled {
    pub fn wrap(inner: Arc<dyn IrrigationController>) -> Arc<dyn IrrigationController> {
        match inner.status_poll_interval_s() {
            None => inner,
            Some(s) => Arc::new(Self {
                inner,
                interval: Duration::from_secs(u64::from(s)),
                cache: tokio::sync::Mutex::new(None),
                fetching: tokio::sync::Mutex::new(()),
                generation: AtomicU64::new(0),
            }),
        }
    }

    /// A command may have changed what the next status will say: drop the
    /// entry, and mark any fetch already in flight as describing the
    /// world before the command.
    async fn forget(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.cache.lock().await.take();
    }

    /// The cached reading while it is inside the declared interval.
    async fn cached(&self) -> Option<ControllerStatus> {
        let cache = self.cache.lock().await;
        let entry = cache.as_ref()?;
        (entry.at.elapsed() < self.interval).then(|| entry.status.clone())
    }
}

#[async_trait]
impl IrrigationController for Throttled {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn supports(&self) -> ControllerCaps {
        self.inner.supports()
    }
    fn simulated(&self) -> bool {
        self.inner.simulated()
    }
    fn rate_limit_remaining(&self) -> Option<String> {
        self.inner.rate_limit_remaining()
    }
    fn mapped_zone_slugs(&self) -> Vec<String> {
        self.inner.mapped_zone_slugs()
    }
    fn status_poll_interval_s(&self) -> Option<u32> {
        self.inner.status_poll_interval_s()
    }
    async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
        let out = self.inner.run_zone(slug, duration_s).await;
        self.forget().await;
        out
    }
    async fn stop_zone(&self, slug: &str) -> ControllerResult<()> {
        let out = self.inner.stop_zone(slug).await;
        self.forget().await;
        out
    }
    async fn stop_all(&self) -> ControllerResult<()> {
        let out = self.inner.stop_all().await;
        self.forget().await;
        out
    }
    async fn status(&self) -> ControllerResult<ControllerStatus> {
        if let Some(st) = self.cached().await {
            return Ok(st);
        }
        let _one_at_a_time = self.fetching.lock().await;
        // Another caller may have filled the cache while this one queued.
        if let Some(st) = self.cached().await {
            return Ok(st);
        }
        let before = self.generation.load(Ordering::SeqCst);
        let mut st = self.inner.status().await?;
        let fetched_at = Instant::now();
        // Two answers are handed to the caller but never cached. One the
        // adapter served from its OWN last-known state (`reachable: false`,
        // the cloud adapters' fallback): re-stamping it as fresh would hold
        // a pre-stop reading for another whole interval, which is how a
        // finished zone stays "running" long enough for the reaper to stop
        // the whole device. And one whose fetch straddled a command: it may
        // predate the valve moving.
        if st.reachable {
            // A reading served later has to say when it was taken. An
            // adapter that stamps its own keeps it, so its fallback still
            // carries the original reading's time.
            st.observed_epoch.get_or_insert(crate::timefmt::now_epoch());
            // The generation test happens while HOLDING the cache lock,
            // because `forget` takes that same lock. Testing first and
            // locking afterwards left a window: a command could retire
            // this reading in between, and it was cached anyway. That is
            // how a pre-stop "running" survives a stop for a whole poll
            // interval, which is the thing the comment above warns about.
            let mut cache = self.cache.lock().await;
            if self.generation.load(Ordering::SeqCst) == before {
                *cache = Some(Cached {
                    at: fetched_at,
                    status: st.clone(),
                });
            }
        }
        Ok(st)
    }
    /// A reading taken now. `forget` clears THIS wrapper's cache, and the
    /// call goes to the adapter's own fresh path, because clearing only
    /// the outer cache still let an adapter with its own throttle (Rachio)
    /// answer from a snapshot up to its poll interval old. Deliberately
    /// NOT cached afterwards: this is a reading for one decision, and
    /// re-stamping it as the shared current state is what the generation
    /// check above exists to prevent.
    async fn status_fresh(&self) -> ControllerResult<ControllerStatus> {
        self.forget().await;
        self.inner.status_fresh().await
    }
    async fn run_history(&self, since_epoch: i64) -> ControllerResult<Vec<RunRecord>> {
        self.inner.run_history(since_epoch).await
    }
    async fn discover_zones(&self) -> ControllerResult<Vec<DiscoveredZone>> {
        self.inner.discover_zones().await
    }
}

/// A controller behind the cap. Every method delegates; `run_zone` clamps.
pub struct Capped {
    inner: Arc<dyn IrrigationController>,
}

impl Capped {
    pub fn wrap(inner: Arc<dyn IrrigationController>) -> Arc<dyn IrrigationController> {
        Arc::new(Self { inner })
    }
}

#[async_trait]
impl IrrigationController for Capped {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn supports(&self) -> ControllerCaps {
        self.inner.supports()
    }
    fn simulated(&self) -> bool {
        self.inner.simulated()
    }
    fn rate_limit_remaining(&self) -> Option<String> {
        self.inner.rate_limit_remaining()
    }
    fn mapped_zone_slugs(&self) -> Vec<String> {
        self.inner.mapped_zone_slugs()
    }
    fn status_poll_interval_s(&self) -> Option<u32> {
        self.inner.status_poll_interval_s()
    }
    async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
        let capped = duration_s.min(RUN_SECONDS_MAX);
        if capped != duration_s {
            tracing::warn!(
                controller = %self.inner.id(),
                zone = slug,
                requested_s = duration_s,
                capped_s = capped,
                "run clamped to RUN_SECONDS_MAX at the controller boundary"
            );
        }
        self.inner.run_zone(slug, capped).await
    }
    async fn stop_zone(&self, slug: &str) -> ControllerResult<()> {
        self.inner.stop_zone(slug).await
    }
    async fn stop_all(&self) -> ControllerResult<()> {
        self.inner.stop_all().await
    }
    async fn status(&self) -> ControllerResult<ControllerStatus> {
        self.inner.status().await
    }
    async fn status_fresh(&self) -> ControllerResult<ControllerStatus> {
        self.inner.status_fresh().await
    }
    async fn run_history(&self, since_epoch: i64) -> ControllerResult<Vec<RunRecord>> {
        self.inner.run_history(since_epoch).await
    }
    async fn discover_zones(&self) -> ControllerResult<Vec<DiscoveredZone>> {
        self.inner.discover_zones().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::irrigation_controller::ControllerError;
    use std::sync::Mutex;

    /// Records what it was told, like OpenSprinkler direct would send.
    struct Echo(Mutex<Vec<u32>>);

    #[async_trait]
    impl IrrigationController for Echo {
        fn id(&self) -> &str {
            "os"
        }
        fn supports(&self) -> ControllerCaps {
            ControllerCaps {
                flow_meter: false,
                rain_sensor: false,
                master_valve: false,
                multi_zone_parallel: false,
                history_query: false,
                remote_program_upload: false,
                water_level: false,
                per_zone_stop: true,
                duration_quantum_s: 1,
            }
        }
        async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
            self.0.lock().unwrap().push(duration_s);
            Ok(RunHandle {
                controller_id: "os".into(),
                zone_slug: slug.into(),
                started_epoch: 0,
                planned_duration_s: duration_s,
                provider_ref: None,
            })
        }
        async fn stop_zone(&self, _slug: &str) -> ControllerResult<()> {
            Ok(())
        }
        async fn stop_all(&self) -> ControllerResult<()> {
            Ok(())
        }
        async fn status(&self) -> ControllerResult<ControllerStatus> {
            Err(ControllerError::Offline)
        }
        async fn run_history(&self, _since_epoch: i64) -> ControllerResult<Vec<RunRecord>> {
            Ok(vec![])
        }
    }

    /// Counts status reads, declaring the interval it is built with.
    struct Counter {
        interval: Option<u32>,
        reads: std::sync::atomic::AtomicU32,
        /// From this read on, answer the way a cloud adapter does when its
        /// own fetch fails: the last known state, `reachable: false`.
        /// `u32::MAX` (the default) never fails.
        fallback_from: std::sync::atomic::AtomicU32,
        /// When set, the next read parks until the test releases it, so a
        /// command can be sent while a fetch is in flight.
        gated: std::sync::atomic::AtomicBool,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    impl Counter {
        fn reads(&self) -> u32 {
            self.reads.load(std::sync::atomic::Ordering::SeqCst)
        }
        /// Park the next read until `release`.
        fn gate(&self) {
            self.gated.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl IrrigationController for Counter {
        fn id(&self) -> &str {
            "cloud"
        }
        fn supports(&self) -> ControllerCaps {
            ControllerCaps {
                flow_meter: false,
                rain_sensor: false,
                master_valve: false,
                multi_zone_parallel: false,
                history_query: false,
                remote_program_upload: false,
                water_level: false,
                per_zone_stop: false,
                duration_quantum_s: 60,
            }
        }
        fn status_poll_interval_s(&self) -> Option<u32> {
            self.interval
        }
        async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
            Ok(RunHandle {
                controller_id: "cloud".into(),
                zone_slug: slug.into(),
                started_epoch: 0,
                planned_duration_s: duration_s,
                provider_ref: None,
            })
        }
        async fn stop_zone(&self, _slug: &str) -> ControllerResult<()> {
            Ok(())
        }
        async fn stop_all(&self) -> ControllerResult<()> {
            Ok(())
        }
        async fn status(&self) -> ControllerResult<ControllerStatus> {
            let n = self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if self.gated.swap(false, std::sync::atomic::Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            let reachable = n < self.fallback_from.load(std::sync::atomic::Ordering::SeqCst);
            Ok(ControllerStatus {
                observed_epoch: None,
                reachable,
                master_enabled: Some(n % 2 == 1),
                water_level_pct: None,
                rain_sensor_tripped: None,
                current_program: None,
                zone_states: vec![],
                flow_gpm: None,
                flow_connected: false,
                firmware: None,
            })
        }
        async fn run_history(&self, _since_epoch: i64) -> ControllerResult<Vec<RunRecord>> {
            Ok(vec![])
        }
    }

    fn counter(interval: Option<u32>) -> Arc<Counter> {
        Arc::new(Counter {
            interval,
            reads: std::sync::atomic::AtomicU32::new(0),
            fallback_from: std::sync::atomic::AtomicU32::new(u32::MAX),
            gated: std::sync::atomic::AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        })
    }

    /// The reading a cloud adapter serves from its OWN last-known state
    /// when a fetch fails is handed to the caller and thrown away, not
    /// re-stamped as fresh. Caching it would hold a pre-stop reading for
    /// another whole interval, which is how a zone that finished on its
    /// own timer stays "running" long enough for the reaper to stop the
    /// entire device and cut a sibling zone short.
    #[tokio::test]
    async fn a_fault_answered_from_the_adapters_last_reading_is_never_cached() {
        let probe = counter(Some(60));
        probe
            .fallback_from
            .store(2, std::sync::atomic::Ordering::SeqCst);
        let inner: Arc<dyn IrrigationController> = probe.clone();
        let c = Throttled::wrap(inner);
        assert!(c.status().await.unwrap().reachable);
        c.run_zone("front", 600).await.unwrap();
        let fallback = c.status().await.unwrap();
        assert!(!fallback.reachable, "the adapter reported the fault");
        assert_eq!(probe.reads(), 2);
        c.status().await.unwrap();
        assert_eq!(
            probe.reads(),
            3,
            "a fault must not be served as a fresh reading for the interval"
        );
    }

    /// Every reading the wrapper serves says when it was taken, so a
    /// reader crediting water counts from the moment the valve was seen
    /// open rather than the moment it happened to ask.
    #[tokio::test]
    async fn a_served_reading_carries_the_time_it_was_taken() {
        let probe = counter(Some(60));
        let inner: Arc<dyn IrrigationController> = probe.clone();
        let c = Throttled::wrap(inner);
        let first = c.status().await.unwrap();
        let taken_at = first
            .observed_epoch
            .expect("a reading that can be served later carries its time");
        let second = c.status().await.unwrap();
        assert_eq!(probe.reads(), 1, "the second read is the cached first");
        assert_eq!(
            second.observed_epoch,
            Some(taken_at),
            "the cached copy keeps the original reading's time, not the time it was asked for"
        );
    }

    /// A command sent while a status fetch is in flight neither waits for
    /// it (the dispatcher holds the per-zone lock across the command, and
    /// a vendor cloud can take fifteen seconds) nor is undone by it: the
    /// straddling fetch describes the valve before the command, so it is
    /// returned to its own caller and never cached.
    #[tokio::test]
    async fn a_command_during_a_fetch_neither_waits_for_it_nor_is_undone_by_it() {
        let probe = counter(Some(60));
        probe.gate();
        let inner: Arc<dyn IrrigationController> = probe.clone();
        let c = Throttled::wrap(inner);
        let reader = {
            let c = c.clone();
            tokio::spawn(async move { c.status().await })
        };
        probe.entered.notified().await;
        tokio::time::timeout(Duration::from_secs(5), c.run_zone("front", 600))
            .await
            .expect("a command must not queue behind a status fetch")
            .unwrap();
        probe.release.notify_one();
        reader.await.unwrap().unwrap();
        assert_eq!(probe.reads(), 1);
        c.status().await.unwrap();
        assert_eq!(
            probe.reads(),
            2,
            "the fetch that straddled the command described the valve before it"
        );
    }

    /// Inside the declared interval the second read is the cached first;
    /// a command through the wrapper makes the next read live.
    #[tokio::test]
    async fn a_cloud_controller_is_read_once_per_interval_and_after_a_command() {
        let probe = counter(Some(60));
        let inner: Arc<dyn IrrigationController> = probe.clone();
        let c = Throttled::wrap(inner);
        let first = c.status().await.unwrap();
        let second = c.status().await.unwrap();
        assert_eq!(probe.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(first.master_enabled, second.master_enabled);
        c.run_zone("front", 600).await.unwrap();
        let third = c.status().await.unwrap();
        assert_eq!(probe.reads.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_ne!(third.master_enabled, second.master_enabled);
        c.stop_all().await.unwrap();
        c.status().await.unwrap();
        assert_eq!(probe.reads.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    /// A controller that reads on demand is not wrapped at all.
    #[tokio::test]
    async fn an_on_demand_controller_is_read_live_every_time() {
        let probe = counter(None);
        let inner: Arc<dyn IrrigationController> = probe.clone();
        let c = Throttled::wrap(inner);
        c.status().await.unwrap();
        c.status().await.unwrap();
        assert_eq!(probe.reads.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// A 21600 s segment reaches the hardware as 7200 s; an ordinary
    /// segment reaches it untouched.
    #[tokio::test]
    async fn a_six_hour_segment_is_clamped_at_the_boundary() {
        let echo = Arc::new(Echo(Mutex::new(vec![])));
        let capped = Capped::wrap(echo.clone());
        let h = capped.run_zone("front", 21_600).await.unwrap();
        assert_eq!(h.planned_duration_s, RUN_SECONDS_MAX);
        capped.run_zone("front", 900).await.unwrap();
        assert_eq!(*echo.0.lock().unwrap(), vec![RUN_SECONDS_MAX, 900]);
        assert_eq!(capped.id(), "os");
    }
}
