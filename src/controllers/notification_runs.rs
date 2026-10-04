//! Short-lived identities for notification Stop. These are NOT authorization:
//! the API still requires the normal owner/LAN access and Origin checks.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct NotificationRun {
    pub zone: String,
    pub run_id: String,
}

#[derive(Clone)]
struct Entry {
    run_id: String,
    controller: String,
    issued: i64,
}

#[derive(Clone, Default)]
pub struct NotificationRuns(Arc<Mutex<Entries>>);

#[derive(Default)]
struct Entries {
    runs: HashMap<String, Entry>,
    commands: HashMap<String, i64>,
    reset_epoch: i64,
}

impl NotificationRuns {
    pub fn issue(
        &self,
        zone: &str,
        controller: &str,
        observed: i64,
        now: i64,
    ) -> Option<NotificationRun> {
        let run_id = format!("notice-{:032x}", rand::random::<u128>());
        let mut entries = self.0.lock().unwrap();
        if observed <= entries.reset_epoch
            || entries
                .commands
                .get(controller)
                .is_some_and(|epoch| observed <= *epoch)
        {
            return None;
        }
        entries.runs.retain(|_, e| now - e.issued <= 6 * 3600);
        if entries.runs.contains_key(zone) {
            return None;
        }
        entries.runs.insert(
            zone.into(),
            Entry {
                run_id: run_id.clone(),
                controller: controller.into(),
                issued: now,
            },
        );
        Some(NotificationRun {
            zone: zone.into(),
            run_id,
        })
    }

    /// Called under the dispatch command-order barrier. A later command,
    /// completed run, controller replacement, restart or expiry invalidates it.
    pub fn controller(&self, request: &NotificationRun, now: i64) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .runs
            .get(&request.zone)
            .filter(|e| e.run_id == request.run_id && (0..=6 * 3600).contains(&(now - e.issued)))
            .map(|e| e.controller.clone())
    }

    pub fn clear_zone(&self, zone: &str) {
        self.0.lock().unwrap().runs.remove(zone);
    }
    pub fn clear_controller(&self, controller: &str) {
        let mut entries = self.0.lock().unwrap();
        entries.runs.retain(|_, e| e.controller != controller);
        entries
            .commands
            .insert(controller.into(), chrono::Utc::now().timestamp());
    }
    pub fn clear(&self) {
        let mut entries = self.0.lock().unwrap();
        entries.runs.clear();
        entries.reset_epoch = chrono::Utc::now().timestamp();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacement_completion_restart_and_expiry_reject_old_notifications() {
        let notices = NotificationRuns::default();
        let old = notices.issue("lawn", "controller", 100, 100).unwrap();
        assert_eq!(notices.controller(&old, 101).as_deref(), Some("controller"));
        notices.clear_zone("lawn");
        let current = notices.issue("lawn", "controller", 102, 102).unwrap();
        assert!(notices.controller(&old, 103).is_none());
        assert!(notices.controller(&current, 102 + 6 * 3600 + 1).is_none());
        assert!(notices.controller(&current, 101).is_none());
        notices.clear_zone("lawn");
        assert!(notices.controller(&current, 104).is_none());
        assert!(NotificationRuns::default()
            .controller(&current, 104)
            .is_none());
    }
    #[test]
    fn new_device_command_invalidates_sibling_actions_only_on_that_controller() {
        let notices = NotificationRuns::default();
        let a = notices.issue("front", "a", 100, 100).unwrap();
        let b = notices.issue("back", "a", 100, 100).unwrap();
        let c = notices.issue("beds", "b", 100, 100).unwrap();
        notices.clear_controller("a");
        assert!(notices.controller(&a, 101).is_none());
        assert!(notices.controller(&b, 101).is_none());
        assert!(notices.controller(&c, 101).is_some());
        assert!(notices.issue("front", "a", 100, 102).is_none());
        notices.clear();
        assert!(notices.controller(&c, 101).is_none());
    }
}
