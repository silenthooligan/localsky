#![allow(unused_imports)]
use super::observers::*;
use super::*;
use crate::model::{IrrigationSnapshot, SoilProbeFault, ZoneState};
use crate::push::PushEvent;
use crate::refresher::store::IrrigationStore;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn deps(push: crate::push::PushDispatcher) -> ObserverDeps {
    // A fresh forecast, so the stale-forecast edge stays out of the way.
    let forecast_store = crate::forecast::ForecastStore::new();
    forecast_store.store(crate::forecast::snapshot::ForecastSnapshot {
        last_refresh_epoch: chrono::Utc::now().timestamp(),
        ..Default::default()
    });
    ObserverDeps {
        history_conn: None,
        push,
        forecast_store: Arc::new(forecast_store),
        tempest_store: Arc::new(crate::tempest::state::TempestStore::new()),
        controllers: crate::controllers::registry::ControllerRegistry::new(),
        source: SnapshotSource::Native,
        balance_dirty: Arc::new(AtomicBool::new(false)),
    }
}

/// Each snapshot is a later tick than the one before.
static TICK: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1_000);

fn snap(zones: &[(&str, bool)]) -> IrrigationSnapshot {
    let mut s = IrrigationSnapshot::default();
    s.last_refresh_epoch = TICK.fetch_add(10, Ordering::SeqCst);
    s.skip_check
        .decide("run", "Dry enough".into(), "run".into());
    s.zones = zones
        .iter()
        .map(|(slug, running)| ZoneState {
            slug: slug.to_string(),
            name: slug.to_uppercase(),
            running: *running,
            running_known: true,
            ..Default::default()
        })
        .collect();
    s
}

async fn drain(rx: &mut tokio::sync::mpsc::Receiver<PushEvent>) -> Vec<PushEvent> {
    let mut out = Vec::new();
    while let Ok(Some(ev)) =
        tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
    {
        out.push(ev);
    }
    out
}

fn kinds(events: &[PushEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|e| match e {
            PushEvent::ZoneStarted { .. } => "started",
            PushEvent::ZoneStopped { .. } => "stopped",
            PushEvent::DailyVerdict { .. } => "verdict",
            PushEvent::SoilProbeFault { .. } => "fault",
            PushEvent::SoilProbeSuspect { .. } => "suspect",
            PushEvent::SourceOffline { .. } => "offline",
            PushEvent::InferredTargetsPlanned { .. } => "inferred",
            _ => "other",
        })
        .collect()
}

/// Storing a snapshot is what drives the observers: a zone going from
/// idle to running pushes ZoneStarted, back to idle pushes ZoneStopped
/// with the run's length, and the day's verdict pushes once.
#[tokio::test]
async fn a_stored_snapshot_drives_the_push_edges() {
    let store = Arc::new(IrrigationStore::new());
    let (push, mut rx) = crate::push::PushDispatcher::capturing();
    let _task = spawn_observers(store.clone(), deps(push));
    tokio::task::yield_now().await;

    store.store(snap(&[("front", false)]));
    let first = drain(&mut rx).await;
    assert_eq!(kinds(&first), vec!["verdict"], "{first:?}");

    store.store(snap(&[("front", true)]));
    let started = drain(&mut rx).await;
    assert_eq!(kinds(&started), vec!["started"], "{started:?}");

    // The same state again: no edge, nothing pushed, even though the
    // store broadcast the tick.
    store.store(snap(&[("front", true)]));
    assert!(drain(&mut rx).await.is_empty());

    store.store(snap(&[("front", false)]));
    let stopped = drain(&mut rx).await;
    assert_eq!(kinds(&stopped), vec!["stopped"], "{stopped:?}");
    match &stopped[0] {
        PushEvent::ZoneStopped {
            slug, duration_min, ..
        } => {
            assert_eq!(slug, "front");
            assert_eq!(
                *duration_min, 0,
                "a run a few ms long rounds to zero minutes"
            );
        }
        other => panic!("{other:?}"),
    }
}

/// A snapshot the loop re-stored after a failed read keeps the previous
/// refresh instant and carries no decision: no edges, no verdict.
#[tokio::test]
async fn a_restored_snapshot_from_a_failed_read_triggers_nothing() {
    let store = Arc::new(IrrigationStore::new());
    let (push, mut rx) = crate::push::PushDispatcher::capturing();
    let _task = spawn_observers(store.clone(), deps(push));
    tokio::task::yield_now().await;
    let good = snap(&[("front", false)]);
    store.store(good.clone());
    drain(&mut rx).await;
    // The loop's failure path: the same snapshot again, same instant.
    let mut again = good.clone();
    again.zones[0].running = true;
    store.store(again);
    assert!(drain(&mut rx).await.is_empty());
}

/// A probe fault pushes once for the life of the process; a quarantine
/// pushes once per episode and re-arms when the zone leaves it.
#[tokio::test]
async fn faults_push_once_and_quarantines_once_per_episode() {
    let store = Arc::new(IrrigationStore::new());
    let (push, mut rx) = crate::push::PushDispatcher::capturing();
    let _task = spawn_observers(store.clone(), deps(push));
    tokio::task::yield_now().await;
    store.store(snap(&[("front", false)]));
    drain(&mut rx).await;

    let faulted = || {
        let mut s = snap(&[("front", false)]);
        s.soil_probe_faults = vec![SoilProbeFault {
            zone_slug: "front".into(),
            zone_name: "FRONT".into(),
            since_epoch: Some(1_000),
            ..Default::default()
        }];
        s
    };
    store.store(faulted());
    assert_eq!(kinds(&drain(&mut rx).await), vec!["fault"]);
    store.store(faulted());
    assert!(drain(&mut rx).await.is_empty(), "a fault pushes once");

    let quarantined = || {
        let mut s = snap(&[("front", false)]);
        s.zones[0].verdict = Some(crate::model::ZoneVerdict {
            source: "soil_quarantine".into(),
            reason: "Soil probe suspect (12% vs yard 41%)".into(),
            ..Default::default()
        });
        s
    };
    store.store(quarantined());
    let q = drain(&mut rx).await;
    assert_eq!(kinds(&q), vec!["suspect"], "{q:?}");
    store.store(quarantined());
    assert!(drain(&mut rx).await.is_empty(), "latched for the episode");
    store.store(snap(&[("front", false)]));
    drain(&mut rx).await;
    store.store(quarantined());
    assert_eq!(kinds(&drain(&mut rx).await), vec!["suspect"], "re-armed");
}

/// The loop is read, prefetch, decide, store: nothing in its body pushes,
/// writes a ledger row or bumps a metric.
#[test]
fn the_loop_body_only_stores() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/refresher/shell.rs"),
    )
    .unwrap();
    let start = src.find("pub fn spawn_refresher(").unwrap();
    let end = src[start..].find("\n}\n").unwrap() + start;
    let body = crate::engine::clock::code_only(&src[start..end]);
    for banned in [
        ".emit(",
        "emit_push_events(",
        "ledger_observation(",
        "ledger_et0_emission(",
        "metrics::",
        "ingest.observe(",
        "IngestState",
        ".upsert(",
    ] {
        assert!(
            !body.contains(banned),
            "spawn_refresher calls {banned}; that belongs to an observer"
        );
    }
    assert_eq!(
        body.matches("store.store(").count(),
        2,
        "one store per outcome"
    );
}
