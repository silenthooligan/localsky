// Notification fan-out: every PushEvent the product emits reaches every
// enabled sink, not only the browser.
//
// This module was seven lines of comments describing sinks that did not
// exist, while the wizard collected ntfy and Slack URLs, the schema
// carried their config, and the docs admitted only Web Push worked. A
// field the product asks for and never reads is a promise it breaks
// quietly. ntfy and Slack are one HTTP POST each; they are wired here,
// through the same channel Web Push already drains, and email (which
// would have needed an SMTP dependency this image does not carry) is
// removed from the schema, the settings surface and the docs in the
// same change rather than left as a dead field.

#[cfg(feature = "ssr")]
pub mod sinks;

#[cfg(feature = "ssr")]
pub use fanout::{from_push_event, headline as fanout_headline, Fanout};

#[cfg(feature = "ssr")]
mod fanout {
    use std::sync::Arc;

    use crate::config::schema::Notifications;
    use crate::ports::notification_sink::{NotificationEvent, NotificationSink};
    use crate::push::PushEvent;

    /// The sinks a config enables, ready to receive.
    pub struct Fanout {
        sinks: Vec<Arc<dyn NotificationSink>>,
    }

    impl Fanout {
        /// Every sink the config configures. `web_push` is drained by the
        /// push dispatcher itself and `mqtt` by the Home Assistant
        /// discovery publisher, so they are not sinks here.
        pub fn from_config(cfg: &Notifications) -> Self {
            let mut sinks: Vec<Arc<dyn NotificationSink>> = Vec::new();
            if let Some(n) = cfg.ntfy.as_ref().filter(|n| !n.topic.trim().is_empty()) {
                sinks.push(Arc::new(super::sinks::ntfy::Ntfy::new(n.clone())));
            }
            if let Some(s) = cfg
                .slack
                .as_ref()
                .filter(|s| !s.webhook_url.trim().is_empty())
            {
                sinks.push(Arc::new(super::sinks::slack::Slack::new(s.clone())));
            }
            Self { sinks }
        }

        pub fn is_empty(&self) -> bool {
            self.sinks.is_empty()
        }

        pub fn sink_ids(&self) -> Vec<String> {
            self.sinks.iter().map(|s| s.id().to_string()).collect()
        }

        /// Deliver to every sink that handles the event. A sink that fails
        /// is logged and does not stop the others.
        pub async fn deliver(&self, event: &NotificationEvent) {
            for sink in &self.sinks {
                if !sink.handles(event) {
                    continue;
                }
                if let Err(e) = sink.emit(event).await {
                    tracing::warn!(sink = sink.id(), error = %e, "notification sink failed");
                }
            }
        }
    }

    /// The sink-facing event for a push event, when there is one. The
    /// tuning and inferred-target nudges are dashboard business and stay
    /// with Web Push; everything about water, valves and controllers
    /// goes everywhere.
    pub fn from_push_event(ev: &PushEvent, now_epoch: i64) -> Option<NotificationEvent> {
        let date_local = crate::timeutil::now_local().format("%Y-%m-%d").to_string();
        Some(match ev {
            PushEvent::ZoneStarted { slug, .. } => NotificationEvent::ZoneStarted {
                zone_slug: slug.clone(),
                controller_id: String::new(),
                planned_duration_s: 0,
                at_epoch: now_epoch,
            },
            PushEvent::ZoneStopped {
                slug, duration_min, ..
            } => NotificationEvent::ZoneStopped {
                zone_slug: slug.clone(),
                controller_id: String::new(),
                actual_duration_s: duration_min * 60,
                at_epoch: now_epoch,
            },
            PushEvent::DailyVerdict { verdict, reason } => NotificationEvent::DailyVerdict {
                date_local,
                verdict: verdict.clone(),
                reason: reason.clone(),
                at_epoch: now_epoch,
            },
            PushEvent::ControllerOffline {
                controller_id,
                error,
            } => NotificationEvent::AnomalyDetected {
                severity: "error".into(),
                description: format!("Controller {controller_id} is not answering: {error}"),
                at_epoch: now_epoch,
            },
            PushEvent::SourceOffline { source_id, silent_s } => NotificationEvent::SourceOffline {
                source_id: format!("{source_id} (silent for {} min)", silent_s / 60),
                at_epoch: now_epoch,
            },
            PushEvent::DispatchFailed {
                zone_name,
                controller_id,
                error,
                ..
            } => NotificationEvent::AnomalyDetected {
                severity: "error".into(),
                description: format!(
                    "{zone_name} did not start: {controller_id} refused the command ({error}). Nothing watered."
                ),
                at_epoch: now_epoch,
            },
            PushEvent::ValveUnclosed {
                zone_name,
                controller_id,
                overdue_s,
                ..
            } => NotificationEvent::AnomalyDetected {
                severity: "critical".into(),
                description: format!(
                    "{zone_name} may still be open: its shutoff was due {} min ago and {controller_id} has not confirmed closing it. Check the valve.",
                    (overdue_s / 60).max(1)
                ),
                at_epoch: now_epoch,
            },
            PushEvent::FlowWithoutCommand { gpm } => NotificationEvent::AnomalyDetected {
                severity: "warning".into(),
                description: format!(
                    "The flow meter reads {gpm:.1} gal/min while no zone is commanded on. A stuck valve or a leak."
                ),
                at_epoch: now_epoch,
            },
            PushEvent::SoilProbeFault {
                zone_name,
                zone_slug,
                ..
            } => NotificationEvent::AnomalyDetected {
                severity: "warning".into(),
                description: format!("The soil probe on {zone_name} ({zone_slug}) has stopped reporting."),
                at_epoch: now_epoch,
            },
            PushEvent::SoilProbeSuspect { .. }
            | PushEvent::TuningReportReady { .. }
            | PushEvent::RunCapRaised { .. }
            | PushEvent::InferredTargetsPlanned { .. } => return None,
        })
    }

    /// The sentence a sink sends for an event. One place, so ntfy and
    /// Slack say the same thing.
    pub fn headline(event: &NotificationEvent) -> (String, String) {
        match event {
            NotificationEvent::ZoneStarted { zone_slug, .. } => (
                format!("{zone_slug} started"),
                "Watering in progress.".into(),
            ),
            NotificationEvent::ZoneStopped {
                zone_slug,
                actual_duration_s,
                ..
            } => (
                format!("{zone_slug} finished"),
                format!("Ran for {} min.", actual_duration_s / 60),
            ),
            NotificationEvent::DailyVerdict {
                verdict, reason, ..
            } => (format!("Today: {verdict}"), reason.clone()),
            NotificationEvent::SkipExplained { reason, .. } => {
                ("Skipped today".into(), reason.clone())
            }
            NotificationEvent::SourceOffline { source_id, .. } => (
                "A weather source went quiet".into(),
                format!("{source_id} has stopped reporting."),
            ),
            NotificationEvent::ControllerOffline { controller_id, .. } => (
                format!("{controller_id} is not answering"),
                "Watering that needs it is on hold until it answers.".into(),
            ),
            NotificationEvent::AnomalyDetected {
                severity,
                description,
                ..
            } => (format!("LocalSky {severity}"), description.clone()),
        }
    }
}

#[cfg(all(test, feature = "ssr"))]
mod tests {
    use super::*;
    use crate::config::schema::{Notifications, NtfyConfig, SlackConfig};
    use crate::ports::notification_sink::NotificationEvent;
    use crate::push::PushEvent;
    use std::sync::{Arc, Mutex};

    /// A tiny HTTP server that records what was posted to it.
    async fn recorder() -> (String, Arc<Mutex<Vec<(String, String)>>>) {
        use axum::{extract::Path, routing::post, Router};
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let app = Router::new().route(
            "/{*path}",
            post(move |Path(path): Path<String>, body: String| {
                let seen = seen2.clone();
                async move {
                    seen.lock().unwrap().push((path, body));
                    "ok"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), seen)
    }

    /// A failed 05:30 dispatch reaches an ntfy double and a Slack double.
    #[tokio::test]
    async fn a_failed_dispatch_reaches_ntfy_and_slack() {
        let (base, seen) = recorder().await;
        let cfg = Notifications {
            ntfy: Some(NtfyConfig {
                base_url: base.clone(),
                topic: "lawn".into(),
                auth_token: None,
            }),
            slack: Some(SlackConfig {
                webhook_url: format!("{base}/services/hook"),
            }),
            ..Default::default()
        };
        let fanout = Fanout::from_config(&cfg);
        assert_eq!(
            fanout.sink_ids(),
            vec!["ntfy".to_string(), "slack".to_string()]
        );
        let ev = from_push_event(
            &PushEvent::DispatchFailed {
                zone_name: "Front".into(),
                zone_slug: "front".into(),
                controller_id: "os_main".into(),
                error: "controller offline".into(),
            },
            1_788_600_000,
        )
        .expect("a sink event");
        assert!(
            matches!(ev, NotificationEvent::AnomalyDetected { ref severity, .. } if severity == "error")
        );
        fanout.deliver(&ev).await;
        let posts = seen.lock().unwrap().clone();
        assert_eq!(posts.len(), 2, "{posts:?}");
        let ntfy = posts.iter().find(|(p, _)| p == "lawn").expect("ntfy post");
        assert!(ntfy.1.contains("Front did not start"), "{ntfy:?}");
        let slack = posts
            .iter()
            .find(|(p, _)| p == "services/hook")
            .expect("slack post");
        assert!(
            slack.1.contains("\"text\"") && slack.1.contains("os_main refused"),
            "{slack:?}"
        );
    }

    /// Nothing configured, nothing sent, and the dashboard-only nudges
    /// never become sink events.
    #[test]
    fn unconfigured_sinks_and_dashboard_nudges() {
        assert!(Fanout::from_config(&Notifications::default()).is_empty());
        assert!(from_push_event(
            &PushEvent::TuningReportReady {
                recommendation_count: 2
            },
            0
        )
        .is_none());
        assert!(from_push_event(
            &PushEvent::ValveUnclosed {
                zone_name: "Front".into(),
                zone_slug: "front".into(),
                controller_id: "os".into(),
                overdue_s: 120,
            },
            0,
        )
        .is_some());
    }

    /// Every field of the Notifications block has a reader somewhere:
    /// web_push in the push dispatcher, mqtt in the discovery publisher,
    /// ntfy and slack here. A field nothing reads is a promise the
    /// product breaks quietly.
    #[test]
    fn every_notification_field_has_a_reader() {
        let schema = std::fs::read_to_string("src/config/schema.rs").unwrap();
        let start = schema.find("pub struct Notifications {").unwrap();
        let end = schema[start..].find("\n}\n").unwrap() + start;
        let fields: Vec<String> = schema[start..end]
            .lines()
            .skip(1)
            .filter_map(|l| l.trim().strip_prefix("pub "))
            .map(|l| l.split(':').next().unwrap().trim().to_string())
            .collect();
        assert!(!fields.is_empty());
        let readers = [
            std::fs::read_to_string("src/notifications/mod.rs").unwrap(),
            std::fs::read_to_string("src/push/dispatcher.rs").unwrap(),
            std::fs::read_to_string("src/integrations/home_assistant/mqtt_publish.rs").unwrap(),
        ]
        .join("\n");
        for f in fields {
            assert!(
                readers.contains(&format!(".{f}")) || readers.contains(&format!("notifications.{f}")),
                "notifications.{f} has no reader in the sinks, the push dispatcher or the MQTT publisher"
            );
        }
    }
}
