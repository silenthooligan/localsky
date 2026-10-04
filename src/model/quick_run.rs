//! One-time manual watering, shared by the API and browser.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickRunChoice {
    pub zone: String,
    pub seconds: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QuickRunRequest {
    pub request_id: String,
    pub zones: Vec<QuickRunChoice>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QuickRunZone {
    pub zone: String,
    pub name: String,
    pub max_seconds: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuickRunPhase {
    Starting,
    Running,
    Finishing,
    Stopping,
    Finished,
    Stopped,
    Failed,
    Interrupted,
}

impl QuickRunPhase {
    pub fn active(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Running | Self::Finishing | Self::Stopping
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "Starting Quick Run",
            Self::Running => "Quick Run in progress",
            Self::Finishing => "Finishing this zone",
            Self::Stopping => "Stopping Quick Run",
            Self::Finished => "Quick Run finished",
            Self::Stopped => "Quick Run stopped",
            Self::Failed => "Quick Run needs attention",
            Self::Interrupted => "Quick Run interrupted",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QuickRunStatus {
    pub id: String,
    pub request_id: String,
    pub phase: QuickRunPhase,
    pub zones: Vec<QuickRunChoice>,
    pub names: Vec<String>,
    pub completed: usize,
    pub current: Option<usize>,
    pub current_ends_epoch: Option<i64>,
    pub started_epoch: i64,
    pub message: String,
    #[serde(default)]
    pub stop_unconfirmed: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QuickRunView {
    pub available: bool,
    pub reason: Option<String>,
    pub zones: Vec<QuickRunZone>,
    pub run: Option<QuickRunStatus>,
}
