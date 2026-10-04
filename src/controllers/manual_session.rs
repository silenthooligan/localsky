//! A manual sequence owns dispatch until its last stop has been acknowledged.
//! The command-order barrier protects acquisition and every dispatch check.
use crate::scheduler::dispatch_gate::StopToken;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

#[derive(Clone)]
pub struct Session {
    pub id: String,
    cancelled: Arc<AtomicBool>,
    stop: StopToken,
}

impl Session {
    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst) || self.stop.requested()
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

#[derive(Clone, Default)]
pub struct ManualSession(Arc<Mutex<Option<Session>>>);

impl ManualSession {
    pub fn owns(&self, id: &str) -> bool {
        self.0.lock().unwrap().as_ref().is_some_and(|s| s.id == id)
    }

    /// Caller holds the command-order write barrier through idle validation.
    pub fn reserve(&self, id: String) -> Option<Reservation> {
        let mut slot = self.0.lock().unwrap();
        if slot.is_some() {
            return None;
        }
        let session = Session {
            id,
            cancelled: Arc::new(AtomicBool::new(false)),
            stop: StopToken::capture(),
        };
        *slot = Some(session.clone());
        Some(Reservation {
            owner: self.clone(),
            session,
        })
    }

    pub fn refusal(&self, session_id: &str) -> Option<&'static str> {
        let slot = self.0.lock().unwrap();
        slot.as_ref().and_then(|session| {
            if session.id != session_id {
                Some("Quick Run is active. Stop it before starting another run.")
            } else if session.cancelled() {
                Some("Quick Run was stopped. Remaining zones will not start.")
            } else {
                None
            }
        })
    }

    pub fn cancel(&self, id: &str) -> bool {
        let slot = self.0.lock().unwrap();
        if let Some(session) = slot.as_ref().filter(|s| s.id == id) {
            session.cancel();
            true
        } else {
            false
        }
    }
}

pub struct Reservation {
    owner: ManualSession,
    pub session: Session,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut slot = self.owner.0.lock().unwrap();
        if slot.as_ref().is_some_and(|s| s.id == self.session.id) {
            *slot = None;
        }
    }
}
