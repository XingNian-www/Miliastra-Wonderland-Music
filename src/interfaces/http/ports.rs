use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use image::DynamicImage;
use miliastra_playback::{CredentialStatus, LoginSession, ProviderId, TrackMetadata, TrackRef};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::protocol::WebToolRequest;
use crate::features::hall::{HallCommand, HallRuntimeState};
use crate::features::playback::{PlaybackCommand, PlaybackRuntimeState, PlayerStatus, QueueItem};
use crate::features::startup::StartupTask;
use crate::features::turtle_soup::TurtleSoupSnapshot;
use crate::features::undercover::UndercoverSnapshot;
use crate::interfaces::chat::PendingCommand;
use crate::runtime::business::{BusinessMutationIntent, BusinessMutationOutcome};
use crate::runtime::chat_listener::ChatListenerMode;
use crate::runtime::decision::DecisionAction;
use crate::runtime::player_io::SearchCandidate;
use crate::runtime::scheduler::{
    DiagnosticTaskSnapshot, FormalTaskCancelOutcome, FormalTaskEnqueueOutcome,
};

pub(crate) trait HttpTaskPort: Send + Sync {
    fn apply_mutation(&self, intent: BusinessMutationIntent) -> Result<BusinessMutationOutcome>;
    fn enqueue_command(&self, pending: PendingCommand) -> Result<FormalTaskEnqueueOutcome>;
    fn enqueue_startup(&self, task: StartupTask) -> Result<FormalTaskEnqueueOutcome>;
    fn enqueue_console_chat(
        &self,
        text: String,
        prefix: String,
    ) -> Result<FormalTaskEnqueueOutcome>;
    fn enqueue_listener_mode(&self, target: ChatListenerMode) -> Result<FormalTaskEnqueueOutcome>;
    fn enqueue_clear_idle_exit(&self) -> Result<FormalTaskEnqueueOutcome>;
    fn enqueue_diagnostic(&self, request: WebToolRequest) -> Result<DiagnosticTaskSnapshot>;
    fn cancel_task(&self, task_id: u64) -> Result<FormalTaskCancelOutcome>;
    fn submit_decision(&self, id: u64, action: DecisionAction) -> Result<()>;
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PlayTrackRequest {
    pub track_ref: TrackRef,
    pub metadata: TrackMetadata,
    #[serde(default)]
    pub requester: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HttpCommandError {
    pub status: u16,
    pub message: String,
}

impl HttpCommandError {
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            message: message.into(),
        }
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self {
            status: 500,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for HttpCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HttpCommandError {}

pub(crate) trait HttpCommandPort: Send + Sync {
    fn playback_control(&self, command: PlaybackCommand) -> Result<String, HttpCommandError>;
    fn hall_control(&self, command: HallCommand) -> Result<String, HttpCommandError>;
    fn undercover_control(&self, start: bool) -> Result<String, HttpCommandError>;
    fn remote_song(
        &self,
        keyword: String,
        source: String,
        prefer_accompaniment: bool,
        ai_assisted: bool,
    ) -> Result<String, HttpCommandError>;
    fn play_track(&self, request: PlayTrackRequest) -> Result<String, HttpCommandError>;
    fn request_chat_listener_mode(
        &self,
        mode: ChatListenerMode,
    ) -> Result<String, HttpCommandError>;
    fn set_operator_commands(&self, enabled: bool) -> Result<String, HttpCommandError>;
    fn set_idle_exit(&self, minutes: u32) -> Result<String, HttpCommandError>;
    fn clear_idle_exit(&self) -> Result<String, HttpCommandError>;
    fn list_workflows(&self) -> Result<String, HttpCommandError>;
    fn run_workflow(&self, name: String, args: String) -> Result<String, HttpCommandError>;
}

pub(crate) trait HttpQueryPort: Send + Sync {
    fn turtle_soup_snapshot(&self) -> Result<TurtleSoupSnapshot>;
    fn undercover_snapshot(&self) -> Result<UndercoverSnapshot>;
    fn diagnostic_task_snapshot(&self, id: u64) -> Result<Option<DiagnosticTaskSnapshot>>;
    fn playback_queue_snapshot(&self) -> Result<Vec<QueueItem>>;
    fn playback_state_snapshot(&self) -> Result<PlaybackRuntimeState>;
    fn hall_state_snapshot(&self) -> Result<HallRuntimeState>;
}

pub(crate) trait HttpHallPort: Send + Sync {
    fn capture_hall_screenshot(&self) -> Result<Arc<DynamicImage>>;
}

pub(crate) trait HttpPlayerPort: Send + Sync {
    fn status(&self) -> Result<PlayerStatus>;
    fn search_text(&self, keyword: &str, source: &str) -> Result<String, HttpPlayerSearchError>;
    fn search_candidates(
        &self,
        keyword: &str,
        source: &str,
    ) -> Result<Vec<SearchCandidate>, HttpPlayerSearchError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HttpPlayerSearchError {
    message: String,
}

impl HttpPlayerSearchError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for HttpPlayerSearchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HttpPlayerSearchError {}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HttpProviderView {
    pub provider: ProviderId,
    pub configured: bool,
    pub fields: BTreeMap<String, bool>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HttpLoginErrorView {
    pub code: String,
    pub message: String,
    pub provider: Option<ProviderId>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HttpLoginStatus {
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<HttpLoginErrorView>,
}

#[derive(Clone, Debug)]
pub(crate) struct HttpLoginError {
    pub code: &'static str,
    pub message: &'static str,
}

impl std::fmt::Display for HttpLoginError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for HttpLoginError {}

pub(crate) trait HttpLoginPort: Send + Sync {
    fn providers(&self) -> Result<Vec<HttpProviderView>, HttpLoginError>;
    fn status(&self) -> HttpLoginStatus;
    fn start(&self, provider: ProviderId) -> Result<LoginSession, HttpLoginError>;
    fn cancel(&self, session_id: Uuid) -> Result<(), HttpLoginError>;
    fn logout(&self, provider: ProviderId) -> Result<CredentialStatus, HttpLoginError>;
}

pub(crate) trait HttpAiPort: Send + Sync {
    fn recognize(&self, query: &[(String, String)]) -> Result<String>;
    fn match_song(&self, query: &[(String, String)]) -> Result<String>;
    fn pick(&self, query: &[(String, String)]) -> Result<String>;
}

#[derive(Clone)]
pub(crate) struct HttpApplicationPorts {
    pub commands: Arc<dyn HttpCommandPort>,
    pub tasks: Arc<dyn HttpTaskPort>,
    pub queries: Arc<dyn HttpQueryPort>,
    pub hall: Arc<dyn HttpHallPort>,
    pub player: Arc<dyn HttpPlayerPort>,
    pub login: Arc<dyn HttpLoginPort>,
    pub ai: Arc<dyn HttpAiPort>,
}

impl HttpApplicationPorts {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        commands: Arc<dyn HttpCommandPort>,
        tasks: Arc<dyn HttpTaskPort>,
        queries: Arc<dyn HttpQueryPort>,
        hall: Arc<dyn HttpHallPort>,
        player: Arc<dyn HttpPlayerPort>,
        login: Arc<dyn HttpLoginPort>,
        ai: Arc<dyn HttpAiPort>,
    ) -> Self {
        Self {
            commands,
            tasks,
            queries,
            hall,
            player,
            login,
            ai,
        }
    }
}
