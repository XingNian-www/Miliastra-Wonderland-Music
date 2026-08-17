use super::*;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::time::Duration;

use crate::composition::application::http_facade::ApplicationHttpCommandFacade;
use crate::features::hall::HallRuntimeState;
use crate::features::playback::{PlaybackRuntimeState, PlayerStatus, test_candidate, test_track};
use crate::features::startup::{StartupSource, StartupTaskKind};
use crate::features::turtle_soup::{TurtleSoupAppendReceipt, TurtleSoupSnapshot};
use crate::features::undercover::UndercoverSnapshot;
use crate::interfaces::http::{
    HttpAiPort, HttpHallPort, HttpLoginPort, HttpLoginStatus, HttpPlayerPort, HttpProviderView,
    HttpQueryPort, HttpTaskPort,
};
use crate::runtime::chat_listener::ChatListenerState;
use crate::runtime::player_io::SearchCandidate;
use crate::runtime::scheduler::{DiagnosticTaskSnapshot, FormalTaskReceipt};
use miliastra_playback::{
    AudioCacheStats, AudioCacheTrackStatus, CachedTrackInfo, CachedTrackPage, KugouAccountStatus,
    KugouListenReport, LoginSession, TrackKey,
};

fn custom_workflow_service_from_config_parts(
    config: &CustomWorkflowConfig,
    timing: &TimingConfig,
    ocr: &OcrConfig,
) -> CustomWorkflowService {
    CustomWorkflowService::new(
        config.clone(),
        WorkflowDefaults {
            default_timeout_ms: timing.workflow.default_timeout_ms,
            default_poll_ms: timing.workflow.default_poll_ms,
            default_step_wait_ms: timing.workflow.default_step_wait_ms,
            decision_timeout_ms: timing.decision.timeout_ms,
            decision_poll_ms: timing.decision.poll_ms,
            after_activate_ms: timing.input.after_activate_ms,
            clipboard_hold_ms: timing.input.text_ms,
            stability_mean_threshold: ocr.change_mean_threshold,
            stability_changed_ratio_threshold: ocr.change_pixel_threshold,
        },
    )
}

#[derive(Clone, Debug)]
struct RecordedFormalTask {
    id: u64,
    kind: RecordedFormalTaskKind,
    queued: bool,
    running: bool,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
enum RecordedFormalTaskKind {
    Command(Box<PendingCommand>),
    Startup(StartupTask),
    ConsoleChat { text: String, prefix: String },
    ListenerMode(ChatListenerMode),
    ClearIdleExit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RecordedMutation {
    PlaybackPush(Box<QueueItem>),
    PlaybackRemove(QueueRemoval),
    PlaybackClear,
    HallPatch(HallStatePatch),
    TurtleSoupStart(Option<String>),
    TurtleSoupEnd,
    TurtleSoupAppend {
        title: String,
        surface: String,
        bottom: String,
        adjudication_notes: String,
        enabled: bool,
    },
    ChatListenerModeRequest(ChatListenerMode),
    ChatListenerModeCancel(ChatListenerMode),
}

struct RecordingHttpState {
    next_id: u64,
    next_queue_id: u64,
    formal_tasks: Vec<RecordedFormalTask>,
    diagnostic_tasks: HashMap<u64, DiagnosticTaskSnapshot>,
    diagnostic_requests: Vec<(u64, WebToolRequest)>,
    cancellation_requests: Vec<u64>,
    decisions: Vec<(u64, DecisionAction)>,
    mutations: Vec<RecordedMutation>,
    queue: Vec<QueueItem>,
    playback: PlaybackRuntimeState,
    hall: HallRuntimeState,
    hall_screenshot_requests: usize,
    hall_screenshot_error: bool,
    listener_enqueue_error: bool,
    startup_enqueue_error: bool,
    turtle_soup: TurtleSoupSnapshot,
    turtle_soup_submissions: Vec<TurtleSoupSubmission>,
    undercover: UndercoverSnapshot,
    listener: ChatListenerState,
}

impl RecordingHttpState {
    fn new() -> Self {
        Self {
            next_id: 1,
            next_queue_id: 1,
            formal_tasks: Vec::new(),
            diagnostic_tasks: HashMap::new(),
            diagnostic_requests: Vec::new(),
            cancellation_requests: Vec::new(),
            decisions: Vec::new(),
            mutations: Vec::new(),
            queue: Vec::new(),
            playback: PlaybackRuntimeState::default(),
            hall: HallRuntimeState::default(),
            hall_screenshot_requests: 0,
            hall_screenshot_error: false,
            listener_enqueue_error: false,
            startup_enqueue_error: false,
            turtle_soup: TurtleSoupSnapshot::default(),
            turtle_soup_submissions: Vec::new(),
            undercover: UndercoverSnapshot::default(),
            listener: ChatListenerState::new(),
        }
    }

    fn allocate_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }
}

struct RecordingHttpPort {
    state: Mutex<RecordingHttpState>,
}

impl RecordingHttpPort {
    fn new() -> Self {
        Self {
            state: Mutex::new(RecordingHttpState::new()),
        }
    }

    fn enqueue_kind(&self, kind: RecordedFormalTaskKind) -> Result<FormalTaskEnqueueOutcome> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?;
        let id = state.allocate_id();
        let position = state.formal_tasks.iter().filter(|task| task.queued).count() + 1;
        state.formal_tasks.push(RecordedFormalTask {
            id,
            kind,
            queued: true,
            running: false,
        });
        Ok(FormalTaskEnqueueOutcome::Queued(FormalTaskReceipt {
            task_id: id,
            position,
        }))
    }

    fn formal_tasks(&self) -> Vec<RecordedFormalTask> {
        self.state
            .lock()
            .expect("recording port")
            .formal_tasks
            .clone()
    }

    fn mutations(&self) -> Vec<RecordedMutation> {
        self.state.lock().expect("recording port").mutations.clone()
    }

    fn diagnostic_requests(&self) -> Vec<(u64, WebToolRequest)> {
        self.state
            .lock()
            .expect("recording port")
            .diagnostic_requests
            .clone()
    }

    fn decisions(&self) -> Vec<(u64, DecisionAction)> {
        self.state.lock().expect("recording port").decisions.clone()
    }

    fn cancellation_requests(&self) -> Vec<u64> {
        self.state
            .lock()
            .expect("recording port")
            .cancellation_requests
            .clone()
    }

    fn mark_started(&self, id: u64) {
        let mut state = self.state.lock().expect("recording port");
        let task = state
            .formal_tasks
            .iter_mut()
            .find(|task| task.id == id)
            .expect("recorded task");
        task.queued = false;
        task.running = true;
    }

    fn hall_screenshot_requests(&self) -> usize {
        self.state
            .lock()
            .expect("recording port")
            .hall_screenshot_requests
    }

    fn fail_hall_screenshot(&self) {
        self.state
            .lock()
            .expect("recording port")
            .hall_screenshot_error = true;
    }

    fn fail_listener_enqueue(&self) {
        self.state
            .lock()
            .expect("recording port")
            .listener_enqueue_error = true;
    }

    fn fail_startup_enqueue(&self) {
        self.state
            .lock()
            .expect("recording port")
            .startup_enqueue_error = true;
    }
}

impl HttpTaskPort for RecordingHttpPort {
    /// 与真实端口一致：复用 QueueItem 统一去重策略（含结构化/待解析交叉形态）。
    fn playback_queue_contains(&self, item: QueueItem) -> Result<bool> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?;
        Ok(state
            .queue
            .iter()
            .any(|existing| existing.duplicates_with(&item)))
    }

    fn apply_mutation(&self, intent: BusinessMutationIntent) -> Result<BusinessMutationOutcome> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?;
        Ok(match intent {
            BusinessMutationIntent::Administration(
                AdministrationMutationIntent::RequestChatListenerMode(mode),
            ) => {
                state
                    .mutations
                    .push(RecordedMutation::ChatListenerModeRequest(mode));
                let queued = state.listener.request_mode(mode);
                BusinessMutationOutcome::Administration(
                    AdministrationMutationOutcome::ChatListenerModeRequested {
                        queued,
                        snapshot: state.listener.snapshot(),
                    },
                )
            }
            BusinessMutationIntent::Administration(
                AdministrationMutationIntent::CancelChatListenerModeRequest(mode),
            ) => {
                state
                    .mutations
                    .push(RecordedMutation::ChatListenerModeCancel(mode));
                state.listener.cancel_mode_request(mode);
                BusinessMutationOutcome::Administration(
                    AdministrationMutationOutcome::ChatListenerModeRequestCancelled,
                )
            }
            BusinessMutationIntent::Playback(PlaybackMutationIntent::Push(mut item)) => {
                if item.id == 0 {
                    item.id = state.next_queue_id;
                    state.next_queue_id = state.next_queue_id.wrapping_add(1).max(1);
                }
                state
                    .mutations
                    .push(RecordedMutation::PlaybackPush(item.clone()));
                state.queue.push(*item);
                BusinessMutationOutcome::Playback(PlaybackMutationOutcome::Pushed(
                    crate::features::playback::QueuePushOutcome {
                        accepted: true,
                        size: state.queue.len(),
                    },
                ))
            }
            BusinessMutationIntent::Playback(PlaybackMutationIntent::Remove(removal)) => {
                state
                    .mutations
                    .push(RecordedMutation::PlaybackRemove(removal));
                let index = match removal {
                    QueueRemoval::Id(id) => state.queue.iter().position(|item| item.id == id),
                    QueueRemoval::Index(index) => {
                        Some(index).filter(|index| *index < state.queue.len())
                    }
                    QueueRemoval::Front => (!state.queue.is_empty()).then_some(0),
                };
                let removed = match index {
                    Some(index) => {
                        let item = state.queue.remove(index);
                        QueueRemoveOutcome::Removed {
                            index,
                            item: Box::new(item),
                            size: state.queue.len(),
                        }
                    }
                    None => match removal {
                        QueueRemoval::Id(_) => QueueRemoveOutcome::MissingId,
                        QueueRemoval::Index(_) => QueueRemoveOutcome::InvalidIndex,
                        QueueRemoval::Front => QueueRemoveOutcome::Empty,
                    },
                };
                BusinessMutationOutcome::Playback(PlaybackMutationOutcome::Removed(removed))
            }
            BusinessMutationIntent::Playback(PlaybackMutationIntent::Clear) => {
                state.mutations.push(RecordedMutation::PlaybackClear);
                state.queue.clear();
                BusinessMutationOutcome::Playback(PlaybackMutationOutcome::Cleared)
            }
            BusinessMutationIntent::Playback(PlaybackMutationIntent::RemovePoolTrack(_key)) => {
                state.mutations.push(RecordedMutation::PlaybackClear);
                BusinessMutationOutcome::Playback(PlaybackMutationOutcome::PoolTrackRemoved(false))
            }
            BusinessMutationIntent::Hall(HallMutationIntent::PatchState(patch)) => {
                state
                    .mutations
                    .push(RecordedMutation::HallPatch(patch.clone()));
                if let Some(value) = patch.remaining_minutes {
                    state.hall.remaining_minutes = value;
                }
                if let Some(value) = patch.remaining_updated_at {
                    state.hall.remaining_updated_at = value;
                }
                if let Some(value) = patch.expiring_warning_sent {
                    state.hall.expiring_warning_sent = value;
                }
                BusinessMutationOutcome::Hall(HallMutationOutcome::StatePatched)
            }
            BusinessMutationIntent::TurtleSoup(TurtleSoupMutationIntent::Start { puzzle_id }) => {
                state
                    .mutations
                    .push(RecordedMutation::TurtleSoupStart(puzzle_id));
                state.turtle_soup.enabled = true;
                BusinessMutationOutcome::TurtleSoup(Box::new(TurtleSoupMutationOutcome::Started(
                    state.turtle_soup.clone(),
                )))
            }
            BusinessMutationIntent::TurtleSoup(TurtleSoupMutationIntent::End) => {
                state.mutations.push(RecordedMutation::TurtleSoupEnd);
                let ended = state.turtle_soup.enabled;
                state.turtle_soup = TurtleSoupSnapshot::default();
                BusinessMutationOutcome::TurtleSoup(Box::new(TurtleSoupMutationOutcome::Ended {
                    ended,
                    snapshot: state.turtle_soup.clone(),
                }))
            }
            BusinessMutationIntent::TurtleSoup(TurtleSoupMutationIntent::AppendPuzzle(
                submission,
            )) => {
                state.mutations.push(RecordedMutation::TurtleSoupAppend {
                    title: submission.title.clone(),
                    surface: submission.surface.clone(),
                    bottom: submission.bottom.clone(),
                    adjudication_notes: submission.adjudication_notes.clone(),
                    enabled: submission.enabled,
                });
                state.turtle_soup_submissions.push(submission);
                let position = state.turtle_soup_submissions.len();
                BusinessMutationOutcome::TurtleSoup(Box::new(
                    TurtleSoupMutationOutcome::PuzzleAppended(TurtleSoupAppendReceipt {
                        id: format!("soup-{position:04}"),
                        position,
                        total: position,
                    }),
                ))
            }
        })
    }

    fn enqueue_command(&self, pending: PendingCommand) -> Result<FormalTaskEnqueueOutcome> {
        {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?;
            if state.formal_tasks.iter().any(|task| {
                task.queued
                    && matches!(
                        &task.kind,
                        RecordedFormalTaskKind::Command(command)
                            if command.lock_key == pending.lock_key
                    )
            }) {
                return Ok(FormalTaskEnqueueOutcome::Duplicate);
            }
        }
        self.enqueue_kind(RecordedFormalTaskKind::Command(Box::new(pending)))
    }

    fn enqueue_startup(&self, task: StartupTask) -> Result<FormalTaskEnqueueOutcome> {
        // 注入点：fail_startup_enqueue 时只拒绝「进入千星」任务，模拟多任务入队部分失败。
        let fail = {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?;
            state.startup_enqueue_error
                && task
                    == StartupTask::new(
                        StartupTaskKind::EnterWonderland,
                        StartupSource::REMOTE_CONSOLE,
                    )
        };
        if fail {
            return Err(anyhow!("startup enqueue failed"));
        }
        self.enqueue_kind(RecordedFormalTaskKind::Startup(task))
    }

    fn enqueue_console_chat(
        &self,
        text: String,
        prefix: String,
    ) -> Result<FormalTaskEnqueueOutcome> {
        self.enqueue_kind(RecordedFormalTaskKind::ConsoleChat { text, prefix })
    }

    fn enqueue_listener_mode(&self, target: ChatListenerMode) -> Result<FormalTaskEnqueueOutcome> {
        if self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?
            .listener_enqueue_error
        {
            return Err(anyhow!("listener enqueue failed"));
        }
        self.enqueue_kind(RecordedFormalTaskKind::ListenerMode(target))
    }

    fn enqueue_clear_idle_exit(&self) -> Result<FormalTaskEnqueueOutcome> {
        self.enqueue_kind(RecordedFormalTaskKind::ClearIdleExit)
    }

    fn enqueue_diagnostic(&self, request: WebToolRequest) -> Result<DiagnosticTaskSnapshot> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?;
        let id = state.allocate_id();
        let snapshot = DiagnosticTaskSnapshot {
            id,
            label: request.label(),
            status: "queued".to_string(),
            result: None,
        };
        state.diagnostic_requests.push((id, request));
        state.diagnostic_tasks.insert(id, snapshot.clone());
        Ok(snapshot)
    }

    fn cancel_task(&self, task_id: u64) -> Result<FormalTaskCancelOutcome> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?;
        state.cancellation_requests.push(task_id);
        let Some(task) = state
            .formal_tasks
            .iter_mut()
            .find(|task| task.id == task_id)
        else {
            return Ok(FormalTaskCancelOutcome::NotFound);
        };
        if task.running {
            return Ok(FormalTaskCancelOutcome::CancellationRequested);
        }
        if !task.queued {
            return Ok(FormalTaskCancelOutcome::AlreadyFinished);
        }
        task.queued = false;
        Ok(FormalTaskCancelOutcome::CanceledBeforeStart)
    }

    fn submit_decision(&self, id: u64, action: DecisionAction) -> Result<()> {
        self.state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?
            .decisions
            .push((id, action));
        Ok(())
    }
}

impl HttpQueryPort for RecordingHttpPort {
    fn turtle_soup_snapshot(&self) -> Result<TurtleSoupSnapshot> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?
            .turtle_soup
            .clone())
    }

    fn undercover_snapshot(&self) -> Result<UndercoverSnapshot> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?
            .undercover
            .clone())
    }

    fn diagnostic_task_snapshot(&self, id: u64) -> Result<Option<DiagnosticTaskSnapshot>> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?
            .diagnostic_tasks
            .get(&id)
            .cloned())
    }

    fn playback_queue_snapshot(&self) -> Result<Vec<QueueItem>> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?
            .queue
            .clone())
    }

    fn playback_state_snapshot(&self) -> Result<PlaybackRuntimeState> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?
            .playback
            .clone())
    }

    fn hall_state_snapshot(&self) -> Result<HallRuntimeState> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?
            .hall
            .clone())
    }
}

impl HttpHallPort for RecordingHttpPort {
    fn capture_hall_screenshot(&self) -> Result<Arc<DynamicImage>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?;
        state.hall_screenshot_requests += 1;
        if state.hall_screenshot_error {
            return Err(anyhow!("目标游戏窗口不可用"));
        }
        Ok(Arc::new(DynamicImage::new_rgb8(2, 2)))
    }
}

struct HttpTestPlayerPort {
    fail: bool,
    status: PlayerStatus,
}

impl HttpTestPlayerPort {
    fn successful() -> Self {
        Self {
            fail: false,
            status: PlayerStatus {
                status: String::new(),
                current_uri: String::new(),
                name: String::new(),
                singer: String::new(),
                album_name: String::new(),
                lyric_line_text: String::new(),
                duration: 0.0,
                progress: 0.0,
                playback_rate: 1.0,
                volume: 0,
                requester: String::new(),
                ..PlayerStatus::default()
            },
        }
    }

    fn failing() -> Self {
        Self {
            fail: true,
            ..Self::successful()
        }
    }

    fn with_status(status: PlayerStatus) -> Self {
        Self {
            fail: false,
            status,
        }
    }

    fn fail_if_requested(&self) -> std::result::Result<(), HttpPlayerSearchError> {
        if self.fail {
            Err(HttpPlayerSearchError::new("backend failed"))
        } else {
            Ok(())
        }
    }
}

impl HttpPlayerPort for HttpTestPlayerPort {
    fn status(&self) -> Result<crate::features::playback::PlayerStatus> {
        Ok(self.status.clone())
    }

    fn cache_stats(
        &self,
        keys: &[TrackKey],
    ) -> Result<(Option<AudioCacheStats>, Vec<AudioCacheTrackStatus>)> {
        Ok((
            Some(AudioCacheStats {
                enabled: true,
                directory: "deps/cache/audio".to_string(),
                complete_entries: 3,
                complete_bytes: 1024,
                max_bytes: 2048,
                active_downloads: 1,
                lyrics_entries: 2,
                lyrics_bytes: 128,
                play_count: 12,
                requested_play_count: 9,
                pool_play_count: 3,
                cache_hit_count: 8,
                failure_count: 2,
            }),
            keys.iter()
                .map(|key| AudioCacheTrackStatus {
                    source: key.provider.to_string(),
                    id: key.id.clone(),
                    cached: true,
                    bytes: Some(512),
                    play_count: 4,
                    requested_play_count: 3,
                    pool_play_count: 1,
                    cache_hit_count: 3,
                    failure_count: 1,
                    last_played_at_ms: Some(1_700_000_100_000),
                    last_failure_code: Some("decode_failure".to_string()),
                })
                .collect(),
        ))
    }

    fn cached_tracks(&self, offset: usize, limit: usize) -> Result<CachedTrackPage> {
        // 模拟两条已知曲目与一条未知孤儿（无身份信息）的混合分页数据。
        let tracks = vec![
            CachedTrackInfo {
                hash: "hash-known-a".to_string(),
                source: Some("qqmusic".to_string()),
                id: Some("song-a".to_string()),
                title: Some("晴天".to_string()),
                artists: Some(vec!["周杰伦".to_string()]),
                album: Some("叶惠美".to_string()),
                duration_ms: Some(269_000),
                bytes: 8_388_608,
                complete: true,
                cached_at_ms: 1_700_000_000_000,
                last_used_at_ms: 1_700_000_100_000,
                play_count: 5,
                requested_play_count: 4,
                pool_play_count: 1,
                cache_hit_count: 3,
                failure_count: 1,
                last_played_at_ms: Some(1_700_000_090_000),
                last_failure_code: Some("decode_failure".to_string()),
                downloaded_at_ms: Some(1_700_000_000_000),
            },
            CachedTrackInfo {
                hash: "hash-known-b".to_string(),
                source: Some("netease".to_string()),
                id: Some("song-b".to_string()),
                title: Some("夜曲".to_string()),
                artists: Some(vec!["周杰伦".to_string()]),
                album: None,
                duration_ms: None,
                bytes: 4_194_304,
                complete: true,
                cached_at_ms: 1_700_000_200_000,
                last_used_at_ms: 1_700_000_300_000,
                play_count: 2,
                requested_play_count: 1,
                pool_play_count: 1,
                cache_hit_count: 1,
                failure_count: 0,
                last_played_at_ms: Some(1_700_000_290_000),
                last_failure_code: None,
                downloaded_at_ms: Some(1_700_000_200_000),
            },
            CachedTrackInfo {
                hash: "hash-orphan".to_string(),
                source: None,
                id: None,
                title: None,
                artists: None,
                album: None,
                duration_ms: None,
                bytes: 1_048_576,
                complete: true,
                cached_at_ms: 1_700_000_400_000,
                last_used_at_ms: 1_700_000_400_000,
                play_count: 0,
                requested_play_count: 0,
                pool_play_count: 0,
                cache_hit_count: 0,
                failure_count: 0,
                last_played_at_ms: None,
                last_failure_code: None,
                downloaded_at_ms: None,
            },
        ];
        Ok(CachedTrackPage {
            total: tracks.len(),
            offset,
            limit,
            tracks: tracks.into_iter().skip(offset).take(limit).collect(),
        })
    }

    fn search_text(
        &self,
        keyword: &str,
        source: &str,
    ) -> std::result::Result<String, HttpPlayerSearchError> {
        self.fail_if_requested()?;
        Ok(format!("raw search: {keyword} [{source}]"))
    }

    fn search_candidates(
        &self,
        keyword: &str,
        source: &str,
    ) -> std::result::Result<Vec<SearchCandidate>, HttpPlayerSearchError> {
        self.fail_if_requested()?;
        Ok(vec![test_candidate(
            &format!("{keyword} result"),
            &format!("miliastra://track/{source}/1"),
        )])
    }
}

#[derive(Default)]
struct HttpTestLoginPort {
    active: Mutex<Option<LoginSession>>,
}

impl HttpLoginPort for HttpTestLoginPort {
    fn providers(&self) -> Result<Vec<HttpProviderView>, HttpLoginError> {
        Ok(ProviderId::ALL
            .into_iter()
            .map(|provider| {
                let status = miliastra_playback::CredentialStatus::empty(provider.as_str());
                HttpProviderView {
                    provider,
                    configured: status.configured,
                    fields: status
                        .fields
                        .into_iter()
                        .map(|(name, present)| (name.to_owned(), present))
                        .collect(),
                    refresh_supported: status.refresh_supported,
                    manual_refresh_supported: status.manual_refresh_supported,
                    refresh_ready: status.refresh_ready,
                    refresh_state: status.refresh_state,
                    last_refresh_at_ms: status.last_refresh_at_ms,
                    next_refresh_check_at_ms: status.next_refresh_check_at_ms,
                    last_refresh_error: status.last_refresh_error,
                }
            })
            .collect())
    }

    fn status(&self) -> HttpLoginStatus {
        let active = self.active.lock().unwrap().clone();
        HttpLoginStatus {
            active: active.is_some(),
            session_id: active.as_ref().map(|session| session.session_id),
            provider: active.map(|session| session.provider),
            last_error: None,
        }
    }

    fn start(&self, provider: ProviderId) -> Result<LoginSession, HttpLoginError> {
        let mut active = self.active.lock().unwrap();
        if active.is_some() {
            return Err(HttpLoginError {
                code: "login_in_progress".to_owned(),
                message: "已有登录任务正在进行".to_owned(),
            });
        }
        let session = LoginSession {
            session_id: Uuid::new_v4(),
            provider,
        };
        *active = Some(session.clone());
        Ok(session)
    }

    fn cancel(&self, session_id: Uuid) -> Result<(), HttpLoginError> {
        let mut active = self.active.lock().unwrap();
        if active.as_ref().map(|session| session.session_id) != Some(session_id) {
            return Err(HttpLoginError {
                code: "login_session_invalid".to_owned(),
                message: "登录会话无效".to_owned(),
            });
        }
        *active = None;
        Ok(())
    }

    fn logout(
        &self,
        provider: ProviderId,
    ) -> Result<miliastra_playback::CredentialStatus, HttpLoginError> {
        Ok(miliastra_playback::CredentialStatus::empty(
            provider.as_str(),
        ))
    }

    fn refresh(
        &self,
        provider: ProviderId,
    ) -> Result<miliastra_playback::CredentialStatus, HttpLoginError> {
        Ok(miliastra_playback::CredentialStatus::empty(
            provider.as_str(),
        ))
    }

    fn kugou_status(&self) -> Result<KugouAccountStatus, HttpLoginError> {
        Ok(KugouAccountStatus {
            logged_in: true,
            user_id: Some("123".to_owned()),
            nickname: Some("测试账号".to_owned()),
            vip: true,
            vip_type: Some("年费".to_owned()),
            vip_expire_at_ms: Some(1_735_689_600_000),
            listen_report_available: true,
        })
    }

    fn account_status(
        &self,
        provider: ProviderId,
    ) -> Result<Option<miliastra_playback::ProviderAccountStatus>, HttpLoginError> {
        if matches!(provider, ProviderId::Kugou | ProviderId::Netease) {
            return Ok(Some(miliastra_playback::ProviderAccountStatus {
                provider: provider.to_string(),
                logged_in: true,
                user_id: Some("123".to_owned()),
                ..miliastra_playback::ProviderAccountStatus::default()
            }));
        }
        Ok(None)
    }

    fn refresh_account_status(
        &self,
        provider: ProviderId,
    ) -> Result<Option<miliastra_playback::ProviderAccountStatus>, HttpLoginError> {
        if provider == ProviderId::Kugou {
            return Ok(Some(miliastra_playback::ProviderAccountStatus {
                provider: "kugou".to_owned(),
                logged_in: true,
                user_id: Some("123".to_owned()),
                ..miliastra_playback::ProviderAccountStatus::default()
            }));
        }
        Ok(None)
    }

    fn kugou_claim_vip(&self) -> Result<KugouListenReport, HttpLoginError> {
        Ok(KugouListenReport {
            accepted: true,
            vip_days: 30,
            message: "领取成功".to_owned(),
        })
    }

    fn kugou_upgrade_vip(&self) -> Result<KugouListenReport, HttpLoginError> {
        Ok(KugouListenReport {
            accepted: true,
            vip_days: 7,
            message: "升级成功".to_owned(),
        })
    }
}

struct HttpTestAiPort;

impl HttpAiPort for HttpTestAiPort {
    fn recognize(&self, _query: &[(String, String)]) -> Result<String> {
        Ok("test recognition".to_string())
    }

    fn match_song(&self, _query: &[(String, String)]) -> Result<String> {
        Ok("test match".to_string())
    }

    fn pick(&self, _query: &[(String, String)]) -> Result<String> {
        Ok("test pick".to_string())
    }
}

struct HttpTestState {
    state: HttpSharedState,
    recording: Arc<RecordingHttpPort>,
    /// 配置中心临时数据库根目录；Drop 时清理。
    config_root: Option<PathBuf>,
}

impl Drop for HttpTestState {
    fn drop(&mut self) {
        if let Some(root) = self.config_root.take() {
            let _ = fs::remove_dir_all(root);
        }
    }
}

impl Deref for HttpTestState {
    type Target = HttpSharedState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl DerefMut for HttpTestState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

struct TestHttpResponse {
    status_line: String,
    headers: HashMap<String, String>,
    body: String,
}

fn start_test_http_server(state: &mut HttpTestState, access_token: &str) -> HttpServer {
    state.config.http.host = "127.0.0.1".to_string();
    state.config.http.port = 0;
    state.config.http.access_token = access_token.to_string();
    start(state.state.clone()).expect("start HTTP server")
}

fn http_get(address: SocketAddr, target: &str, access_token: Option<&str>) -> TestHttpResponse {
    http_request(address, "GET", target, access_token)
}

fn http_post(address: SocketAddr, target: &str, access_token: Option<&str>) -> TestHttpResponse {
    http_request(address, "POST", target, access_token)
}

fn http_post_json(
    address: SocketAddr,
    target: &str,
    body: &str,
    access_token: Option<&str>,
) -> TestHttpResponse {
    http_request_with_body(address, "POST", target, access_token, body)
}

fn http_request(
    address: SocketAddr,
    method: &str,
    target: &str,
    access_token: Option<&str>,
) -> TestHttpResponse {
    http_request_with_body(address, method, target, access_token, "")
}

fn http_request_with_body(
    address: SocketAddr,
    method: &str,
    target: &str,
    access_token: Option<&str>,
    body: &str,
) -> TestHttpResponse {
    let mut stream = TcpStream::connect(address).expect("connect to HTTP server");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    let token_header = access_token
        .map(|token| format!("X-Miliastra-Token: {token}\r\n"))
        .unwrap_or_default();
    let content_type = if body.is_empty() {
        String::new()
    } else {
        "Content-Type: application/json\r\n".to_string()
    };
    let request = format!(
        "{method} {target} HTTP/1.1\r\nHost: localhost\r\n{token_header}{content_type}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .expect("write HTTP request");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read HTTP response");
    let (head, body) = raw.split_once("\r\n\r\n").expect("HTTP response head");
    let mut lines = head.split("\r\n");
    let status_line = lines.next().expect("HTTP status line").to_string();
    let headers = lines
        .map(|line| line.split_once(':').expect("HTTP response header"))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    TestHttpResponse {
        status_line,
        headers,
        body: body.to_string(),
    }
}

#[test]
fn owned_http_server_serves_and_stops_on_an_ephemeral_port() {
    let mut state = test_state();
    let server = start_test_http_server(&mut state, "");
    let address = server.local_addr();
    let response = http_get(address, "/", None);

    assert_eq!(response.status_line, "HTTP/1.1 200 OK");
    server.shutdown().expect("shutdown HTTP server");
    assert!(TcpStream::connect(address).is_err());
}

#[test]
fn screenshot_rejects_an_invalidated_frame_cache() {
    let state = test_state();
    {
        let mut cache = state.latest_frame.lock().expect("frame cache");
        cache.store(Arc::new(image::DynamicImage::new_rgb8(2, 2)));
        cache.invalidate();
    }
    let request = Request {
        method: "GET".to_string(),
        path: "/screenshot".to_string(),
        query: Vec::new(),
        headers: HeaderMap::new(),
        body: Vec::new(),
    };

    let error = match screenshot_response(&request, &state) {
        Ok(_) => panic!("invalidated frame must not be served"),
        Err(error) => error,
    };
    assert_eq!(error.status, 503);
}

#[test]
fn hall_screenshot_triggers_detection_and_serves_captured_frame() {
    let state = test_state();
    let request = Request {
        method: "GET".to_string(),
        path: "/hall-screenshot".to_string(),
        query: vec![("quality".to_string(), "88".to_string())],
        headers: HeaderMap::new(),
        body: Vec::new(),
    };

    let response = hall_screenshot_response(&request, &state).expect("hall screenshot");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "image/jpeg");
    assert_eq!(state.recording.hall_screenshot_requests(), 1);
}

#[test]
fn hall_screenshot_maps_detection_failure_to_service_unavailable() {
    let state = test_state();
    state.recording.fail_hall_screenshot();
    let request = Request {
        method: "GET".to_string(),
        path: "/hall-screenshot".to_string(),
        query: Vec::new(),
        headers: HeaderMap::new(),
        body: Vec::new(),
    };

    let error = hall_screenshot_response(&request, &state.state).unwrap_err();

    assert_eq!(error.status, 503);
}

#[test]
fn hall_screenshot_rejects_invalid_quality_before_touching_the_game() {
    let state = test_state();
    let request = Request {
        method: "GET".to_string(),
        path: "/hall-screenshot".to_string(),
        query: vec![("quality".to_string(), "79".to_string())],
        headers: HeaderMap::new(),
        body: Vec::new(),
    };

    let error = hall_screenshot_response(&request, &state.state).unwrap_err();

    assert_eq!(error.status, 400);
    assert_eq!(state.recording.hall_screenshot_requests(), 0);
}

#[test]
fn search_routes_preserve_their_contract_over_real_http() {
    let mut state = test_state();
    let server = start_test_http_server(&mut state, "");
    let address = server.local_addr();

    let text = http_get(address, "/search?keyword=song&source=netease", None);
    assert_eq!(text.status_line, "HTTP/1.1 200 OK");
    assert_eq!(
        text.headers.get("content-type").map(String::as_str),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(text.body, "raw search: song [netease]");

    let candidates = http_get(
        address,
        "/search/candidates?keyword=song&source=netease",
        None,
    );
    assert_eq!(candidates.status_line, "HTTP/1.1 200 OK");
    assert_eq!(
        candidates.headers.get("content-type").map(String::as_str),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&candidates.body).expect("candidate JSON"),
        json!([{
            "trackRef": {"key": {"provider": "netease", "id": "1"}},
            "metadata": {
                "title": "song result",
                "artists": ["测试歌手"],
                "durationMs": 180000
            },
            "eligibility": "unknown",
            "text": "song result",
        }])
    );

    server.shutdown().expect("shutdown HTTP server");
}

#[test]
fn native_player_and_login_routes_use_structured_json_without_secrets() {
    let mut state = test_state();
    let server = start_test_http_server(&mut state, "");
    let address = server.local_addr();

    let providers = http_get(address, "/player/providers", None);
    assert_eq!(providers.status_line, "HTTP/1.1 200 OK");
    let providers: Value = serde_json::from_str(&providers.body).expect("providers JSON");
    assert_eq!(providers.as_array().map(Vec::len), Some(4));
    assert_eq!(providers[0]["configured"], false);
    assert!(providers[0].get("cookies").is_none());

    let track = json!({
        "trackRef": {
            "key": {"provider": "netease", "id": "123"},
            "resolverLocator": "netease:v1:123"
        },
        "metadata": {
            "title": "结构化歌曲",
            "artists": ["测试歌手"],
            "album": "测试专辑",
            "durationMs": 180000
        },
        "requester": "Alice"
    });
    let played = http_post_json(address, "/player/play-track", &track.to_string(), None);
    assert_eq!(played.status_line, "HTTP/1.1 200 OK");
    let played: Value = serde_json::from_str(&played.body).expect("play response JSON");
    assert_eq!(played["currentUri"], "miliastra://track/netease/123");
    assert_eq!(played["queued"], true);
    assert_eq!(played["size"], 1);
    let queue = state
        .application
        .queries
        .playback_queue_snapshot()
        .expect("playback queue snapshot");
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].keyword, "结构化歌曲");
    assert_eq!(queue[0].source, "netease");
    assert_eq!(queue[0].requester, "Alice");
    assert!(!queue[0].dedup_bypass);
    assert_eq!(
        queue[0]
            .track
            .as_ref()
            .map(|track| track.track_ref.key.to_string())
            .as_deref(),
        Some("miliastra://track/netease/123")
    );

    let mismatch = json!({
        "trackRef": {
            "key": {"provider": "netease", "id": "123"},
            "resolverLocator": "qqmusic:v2:123:media123"
        },
        "metadata": {"title": "坏曲目", "artists": []}
    });
    let mismatch = http_post_json(address, "/player/play-track", &mismatch.to_string(), None);
    assert_eq!(mismatch.status_line, "HTTP/1.1 400 Bad Request");
    assert!(mismatch.body.contains("resolverLocator"));
    assert_eq!(
        state
            .application
            .queries
            .playback_queue_snapshot()
            .unwrap()
            .len(),
        1
    );

    let empty_kugou_locator = json!({
        "trackRef": {
            "key": {"provider": "kugou", "id": "ABCDEF0123456789"},
            "resolverLocator": "kugou:ABCDEF0123456789::314159"
        },
        "metadata": {"title": "空专辑歌曲", "artists": []}
    });
    let empty_kugou_locator = http_post_json(
        address,
        "/player/play-track",
        &empty_kugou_locator.to_string(),
        None,
    );
    assert_eq!(empty_kugou_locator.status_line, "HTTP/1.1 200 OK");
    assert_eq!(
        state
            .application
            .queries
            .playback_queue_snapshot()
            .unwrap()
            .len(),
        2
    );

    let malformed_kugou_locator = json!({
        "trackRef": {
            "key": {"provider": "kugou", "id": "ABCDEF0123456789"},
            "resolverLocator": "kugou:ABCDEF0123456789:pending:314159"
        },
        "metadata": {"title": "坏曲目", "artists": []}
    });
    let malformed_kugou_locator = http_post_json(
        address,
        "/player/play-track",
        &malformed_kugou_locator.to_string(),
        None,
    );
    assert_eq!(
        malformed_kugou_locator.status_line,
        "HTTP/1.1 400 Bad Request"
    );
    assert!(malformed_kugou_locator.body.contains("resolverLocator"));
    assert_eq!(
        state
            .application
            .queries
            .playback_queue_snapshot()
            .unwrap()
            .len(),
        2
    );

    let unsupported_version = json!({
        "trackRef": {
            "key": {"provider": "netease", "id": "123"},
            "resolverLocator": "netease:v2:123"
        },
        "metadata": {"title": "坏曲目", "artists": []}
    });
    let unsupported_version = http_post_json(
        address,
        "/player/play-track",
        &unsupported_version.to_string(),
        None,
    );
    assert_eq!(unsupported_version.status_line, "HTTP/1.1 400 Bad Request");
    assert!(unsupported_version.body.contains("resolverLocator"));
    assert_eq!(
        state
            .application
            .queries
            .playback_queue_snapshot()
            .unwrap()
            .len(),
        2
    );

    let session = http_post_json(
        address,
        "/player/login/start",
        r#"{"provider":"qqmusic"}"#,
        None,
    );
    assert_eq!(session.status_line, "HTTP/1.1 200 OK");
    let session: Value = serde_json::from_str(&session.body).expect("login response JSON");
    let session_id = session["sessionId"].as_str().expect("session id");
    let status = http_get(address, "/player/login/status", None);
    let status: Value = serde_json::from_str(&status.body).expect("login status JSON");
    assert_eq!(status["active"], true);
    assert_eq!(status["provider"], "qqmusic");
    let cancel = http_post_json(
        address,
        "/player/login/cancel",
        &json!({"sessionId": session_id}).to_string(),
        None,
    );
    assert_eq!(cancel.status_line, "HTTP/1.1 200 OK");
    let status = http_get(address, "/player/login/status", None);
    let status: Value = serde_json::from_str(&status.body).expect("inactive login status JSON");
    assert_eq!(status["active"], false);

    let refresh = http_post_json(address, "/player/login/refresh?provider=kugou", "{}", None);
    assert_eq!(refresh.status_line, "HTTP/1.1 200 OK");
    let refresh: Value = serde_json::from_str(&refresh.body).expect("refresh JSON");
    assert_eq!(refresh["provider"], "kugou");

    let kugou_status = http_get(address, "/player/kugou/status", None);
    assert_eq!(kugou_status.status_line, "HTTP/1.1 200 OK");
    let kugou_status: Value = serde_json::from_str(&kugou_status.body).expect("Kugou status JSON");
    assert_eq!(kugou_status["loggedIn"], true);
    assert_eq!(kugou_status["userId"], "123");
    assert_eq!(kugou_status["vip"], true);
    assert!(kugou_status.get("token").is_none());

    let account_status = http_get(address, "/player/account/status?provider=kugou", None);
    assert_eq!(account_status.status_line, "HTTP/1.1 200 OK");
    let account_status: Value =
        serde_json::from_str(&account_status.body).expect("account status JSON");
    assert_eq!(account_status["provider"], "kugou");

    for target in ["/player/account/status", "/player/account/status?provider="] {
        let account_status = http_get(address, target, None);
        assert_eq!(account_status.status_line, "HTTP/1.1 200 OK");
        let account_status: Value =
            serde_json::from_str(&account_status.body).expect("default account status JSON");
        assert_eq!(account_status["provider"], "netease");
    }

    let account_refresh = http_post_json(
        address,
        "/player/account/refresh?provider=kugou",
        "{}",
        None,
    );
    assert_eq!(account_refresh.status_line, "HTTP/1.1 200 OK");
    let account_refresh: Value =
        serde_json::from_str(&account_refresh.body).expect("account refresh JSON");
    assert_eq!(account_refresh["provider"], "kugou");
    assert_eq!(account_refresh["loggedIn"], true);

    let account_refresh = http_post_json(
        address,
        "/player/account/refresh?provider=%20kugou%20",
        "{}",
        None,
    );
    assert_eq!(account_refresh.status_line, "HTTP/1.1 200 OK");

    let report = http_post_json(address, "/player/kugou/claim-vip", "{}", None);
    assert_eq!(report.status_line, "HTTP/1.1 200 OK");
    let report: Value = serde_json::from_str(&report.body).expect("Kugou claim JSON");
    assert_eq!(report["accepted"], true);
    assert_eq!(report["vipDays"], 30);

    let upgrade = http_post_json(address, "/player/kugou/upgrade-vip", "{}", None);
    assert_eq!(upgrade.status_line, "HTTP/1.1 200 OK");
    let upgrade: Value = serde_json::from_str(&upgrade.body).expect("Kugou upgrade JSON");
    assert_eq!(upgrade["accepted"], true);
    assert_eq!(upgrade["vipDays"], 7);

    server.shutdown().expect("shutdown HTTP server");
}

#[test]
fn native_play_track_rejects_unknown_fields() {
    let mut state = test_state();
    let server = start_test_http_server(&mut state, "");
    let body = json!({
        "trackRef": {"key": {"provider": "qqmusic", "id": "1"}},
        "metadata": {"title": "歌曲", "artists": []},
        "credential": "must not be accepted"
    });
    let response = http_post_json(
        server.local_addr(),
        "/player/play-track",
        &body.to_string(),
        None,
    );
    assert_eq!(response.status_line, "HTTP/1.1 400 Bad Request");
    assert!(
        state
            .application
            .queries
            .playback_queue_snapshot()
            .unwrap()
            .is_empty()
    );
    server.shutdown().expect("shutdown HTTP server");
}

#[test]
fn search_route_requires_the_configured_token_over_real_http() {
    let mut state = test_state();
    let server = start_test_http_server(&mut state, "secret");

    let response = http_get(
        server.local_addr(),
        "/search?keyword=song&source=netease",
        None,
    );

    assert_eq!(response.status_line, "HTTP/1.1 401 Unauthorized");
    assert_eq!(
        response.headers.get("content-type").map(String::as_str),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(response.body, "错误: 需要有效的 Web 访问令牌");
    server.shutdown().expect("shutdown HTTP server");
}

#[test]
fn search_backend_failure_keeps_the_http_error_contract() {
    let mut state = test_state_with_player_port(HttpTestPlayerPort::failing());
    let server = start_test_http_server(&mut state, "");

    let response = http_get(
        server.local_addr(),
        "/search?keyword=song&source=netease",
        None,
    );

    assert_eq!(response.status_line, "HTTP/1.1 500 Internal Server Error");
    assert_eq!(
        response.headers.get("content-type").map(String::as_str),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(response.body, "错误: backend failed");
    server.shutdown().expect("shutdown HTTP server");
}

#[test]
fn chat_send_requires_post() {
    assert!(is_mutating_route("/chat/send"));
}

#[test]
fn turtle_soup_routes_have_expected_methods_and_monitor_snapshot() {
    assert!(!is_mutating_route("/turtle-soup"));
    assert!(is_json_route("/turtle-soup"));
    for route in ["/turtle-soup/start", "/turtle-soup/end"] {
        assert!(is_mutating_route(route));
        assert!(is_json_route(route));
    }
    assert!(is_mutating_route("/turtle-soup/questions"));
    assert!(is_json_route("/turtle-soup/questions"));

    let state = test_state();
    let monitor: Value = serde_json::from_str(&monitor_json(&state).unwrap()).unwrap();
    assert_eq!(monitor["turtleSoup"]["enabled"], false);
    assert_eq!(monitor["turtleSoup"]["phase"], "idle");
}

#[test]
fn undercover_routes_are_redacted_json_and_controls_require_post() {
    assert!(!is_mutating_route("/undercover"));
    assert!(is_json_route("/undercover"));
    for route in ["/undercover/start", "/undercover/end"] {
        assert!(is_mutating_route(route));
        assert!(is_json_route(route));
    }

    let state = test_state();
    let monitor: Value = serde_json::from_str(&monitor_json(&state).unwrap()).unwrap();
    assert_eq!(monitor["undercover"]["enabled"], false);
    assert_eq!(monitor["undercover"]["phase"], "idle");
    assert!(monitor["undercover"].get("words").is_none());
    assert!(monitor["undercover"].get("roles").is_none());
}

#[test]
fn custom_workflow_routes_keep_their_list_and_enqueue_contracts() {
    let mut state = test_state();
    state.config.custom_workflows = serde_yaml::from_str(
        r#"
enabled: true
default_threshold: 0.9
wait_template_absent_stable_default: true
max_hold_key_seconds: 10
templates: {}
workflows:
  - enabled: true
    name: example
    commands: ["入口", "别名"]
    allow_args: true
    message_types: [blue]
    confirm_before_run: false
    confirm_message: ""
    confirm_message_types: [blue]
    confirm_timeout_ms: null
    confirm_poll_ms: null
    steps:
      - type: press_key
        key: F
    success_message: ""
"#,
    )
    .expect("custom workflow config");
    let default_config: AppConfig =
        serde_yaml::from_str(include_str!("../../../../tests/fixtures/config.full.yaml"))
            .expect("default config");
    state.application.commands = Arc::new(ApplicationHttpCommandFacade::new(
        state.recording.clone(),
        custom_workflow_service_from_config_parts(
            &state.config.custom_workflows,
            &state.config.timing,
            &default_config.ocr,
        ),
    ));

    let listed: Value =
        serde_json::from_str(&operator_workflows_route(&[], &state).unwrap()).unwrap();
    assert_eq!(listed[0]["name"], "example");
    assert_eq!(listed[0]["commands"], json!(["入口", "别名"]));
    assert_eq!(listed[0]["allowArgs"], true);
    assert_eq!(listed[0]["confirmBeforeRun"], false);

    let response: Value = serde_json::from_str(
        &operator_workflow_run_route(
            &[
                ("name".to_string(), "example".to_string()),
                ("args".to_string(), "5".to_string()),
            ],
            &state,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(response["queued"], true);
    assert_eq!(response["duplicate"], false);
    assert_eq!(response["position"], 1);
    assert_eq!(response["command"], "入口 5");
    assert!(response["taskId"].as_u64().is_some());
    let tasks = state.recording.formal_tasks();
    assert_eq!(tasks.len(), 1);
    assert!(matches!(
        tasks[0].kind,
        RecordedFormalTaskKind::Command(ref pending)
            if pending.routed.raw == "入口 5"
                && matches!(
                    pending.routed.command,
                    ModuleCommand::CustomWorkflow(ref command)
                        if command.workflow == "example" && command.args == "5"
                )
    ));
}

#[test]
fn turtle_soup_question_submission_appends_in_request_order() {
    let state = test_state();
    let first = r#"{"title":"第一题","surface":"第一面","bottom":"第一底"}"#;
    let second =
        r#"{"title":"第二题","surface":"第二面","bottom":"第二底","adjudicationNotes":"备注"}"#;

    let first: Value = serde_json::from_str(
        &turtle_soup_questions_route(first.as_bytes(), &state).expect("first submission"),
    )
    .expect("first receipt");
    let second: Value = serde_json::from_str(
        &turtle_soup_questions_route(second.as_bytes(), &state).expect("second submission"),
    )
    .expect("second receipt");

    assert_eq!(first["id"], "soup-0001");
    assert_eq!(first["position"], 1);
    assert_eq!(second["id"], "soup-0002");
    assert_eq!(second["position"], 2);
}

#[test]
fn turtle_soup_question_submission_rejects_invalid_json() {
    let state = test_state();
    let error = turtle_soup_questions_route(r#"{"title":"缺少内容"}"#.as_bytes(), &state)
        .expect_err("invalid submission");

    assert_eq!(error.status, 400);
    assert!(state.recording.mutations().is_empty());
}

#[test]
fn web_tool_routes_are_queued_json_post_routes() {
    for route in [
        "/tools/ocr",
        "/tools/scan-chat",
        "/tools/ui-state",
        "/tools/hall-name",
        "/tools/template",
        "/tools/click",
        "/tools/key",
        "/tools/chat-change-samples",
        "/tools/panel-benchmark",
        "/tools/ocr-backends",
        "/tools/ai-preview",
    ] {
        assert!(is_mutating_route(route), "{route} should require POST");
        assert!(is_json_route(route), "{route} should return JSON");
    }
    assert!(!is_mutating_route("/tools/task"));
    assert!(is_json_route("/tools/task"));
    assert!(TOOLS_PAGE.contains("Miliastra 高级控制"));
}

#[test]
fn web_tools_wait_outside_the_formal_pending_queue() {
    let state = test_state();
    let body = tool_ui_state_route(&[], &state).expect("tool route succeeds");
    let ticket: Value = serde_json::from_str(&body).expect("tool ticket");
    let id = ticket["id"].as_u64().expect("tool id");

    assert_eq!(ticket["status"], "queued");
    assert!(state.recording.formal_tasks().is_empty());
    let requests = state.recording.diagnostic_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, id);
    assert!(matches!(requests[0].1, WebToolRequest::UiState));
    assert_eq!(
        state
            .application
            .queries
            .diagnostic_task_snapshot(id)
            .expect("tool snapshot")
            .expect("queued tool")
            .label,
        "UI 状态检测"
    );
}

#[test]
fn web_tool_ocr_rejects_malformed_rect_as_client_error() {
    let state = test_state();
    let error = tool_ocr_route(&[("rect".to_string(), "invalid".to_string())], &state)
        .expect_err("invalid rect rejected");

    assert_eq!(error.status, 400);
    assert!(error.message.contains("rect参数无效"));
}

#[test]
fn web_tool_templates_expose_configured_fixed_regions() {
    let state = test_state();
    let body = tool_templates_route(&[], &state).expect("template list");
    let templates: Vec<Value> = serde_json::from_str(&body).expect("template list json");
    let marker_threshold = state.config.templates.marker_threshold;
    let expected = [
        (
            "blue-marker",
            state.config.screen.chat_rect,
            marker_threshold,
        ),
        (
            "yellow-marker",
            state.config.screen.chat_rect,
            marker_threshold,
        ),
        (
            "pink-marker",
            state.config.screen.chat_rect,
            marker_threshold,
        ),
        ("friend", state.config.screen.friend_rect, marker_threshold),
        (
            "secondary-back",
            state.config.screen.secondary_back_rect,
            marker_threshold,
        ),
        (
            "secondary-hall",
            state.config.screen.secondary_hall_rect,
            marker_threshold,
        ),
        (
            "invite-view-star",
            state.config.invite.view_star_region,
            marker_threshold,
        ),
        (
            "invite-goto-hall",
            state.config.invite.goto_hall_region,
            marker_threshold,
        ),
        (
            "invite-enter-hall",
            state.config.invite.enter_hall_region,
            marker_threshold,
        ),
        (
            "friend-panel",
            state.config.moderation.friend_panel_region,
            marker_threshold,
        ),
        (
            "friend-search-panel",
            state.config.moderation.search_panel_region,
            marker_threshold,
        ),
        (
            "friend-more-settings",
            state.config.moderation.more_settings_region,
            marker_threshold,
        ),
        (
            "friend-block-chat",
            state.config.moderation.block_chat_region,
            marker_threshold,
        ),
        (
            "friend-blacklist",
            state.config.moderation.blacklist_region,
            marker_threshold,
        ),
        (
            "friend-confirm",
            state.config.moderation.confirm_region,
            marker_threshold,
        ),
        (
            "wonderland-confirm",
            state.config.startup.wonderland_confirm_region,
            state.config.startup.wonderland_confirm_threshold,
        ),
        (
            "paimon-menu",
            state.config.startup.main_ui_region,
            state.config.startup.template_threshold,
        ),
        (
            "wonderland-map-star",
            state.config.startup.wonderland_map_star_region,
            state.config.startup.template_threshold,
        ),
    ];
    for (name, region, threshold) in expected {
        let template = templates
            .iter()
            .find(|template| template["name"] == name)
            .unwrap_or_else(|| panic!("missing template {name}"));
        assert_eq!(
            template["region"],
            serde_json::to_value(region).expect("template region json"),
            "template region mismatch: {name}"
        );
        let actual_threshold = template["threshold"].as_f64().expect("template threshold") as f32;
        assert!(
            (actual_threshold - threshold).abs() < f32::EPSILON,
            "template threshold mismatch: {name}"
        );
    }
    assert!(TOOLS_PAGE.contains("useConfiguredTemplateRegion"));
}

#[test]
fn remote_http_api_requires_token_when_configured() {
    let mut config: AppConfig =
        serde_yaml::from_str(include_str!("../../../../tests/fixtures/config.full.yaml"))
            .expect("default config");
    config.http.host = "0.0.0.0".to_string();
    config.http.access_token = "secret".to_string();
    let request = Request {
        method: "GET".to_string(),
        path: "/monitor".to_string(),
        query: Vec::new(),
        headers: HeaderMap::new(),
        body: Vec::new(),
    };

    assert!(requires_access_token(&config.http, &request.path));
    assert!(!has_valid_access_token(&request, &config.http.access_token));
    assert!(!requires_access_token(&config.http, "/"));
}

#[test]
fn status_route_includes_requester_for_the_matching_active_song() {
    let status = PlayerStatus {
        status: "playing".to_string(),
        current_track: Some(test_track("miliastra://track/qqmusic/1", "晴天 - 周杰伦")),
        current_uri: "miliastra://track/qqmusic/1".to_string(),
        name: "晴天".to_string(),
        ..PlayerStatus::default()
    };
    let state = test_state_with_player_port(HttpTestPlayerPort::with_status(status));
    state
        .recording
        .state
        .lock()
        .expect("recording state")
        .playback
        .active_request = Some(ActivePlaybackRequest {
        track: Some(test_track("miliastra://track/qqmusic/1", "晴天 - 周杰伦")),
        requester: "Alice".to_string(),
        ..ActivePlaybackRequest::default()
    });

    let value: Value = serde_json::from_str(&status_route(&[], &state).expect("status route"))
        .expect("status JSON");
    assert_eq!(value["requester"], "Alice");
}

#[test]
fn playback_insights_returns_history_and_cache_status() {
    let state = test_state_with_player_port(HttpTestPlayerPort::successful());
    state
        .recording
        .state
        .lock()
        .expect("recording state")
        .playback
        .previous_requests = vec![ActivePlaybackRequest {
        keyword: "晴天 - 周杰伦".to_string(),
        source: "qqmusic".to_string(),
        title: "晴天".to_string(),
        artist: "周杰伦".to_string(),
        requester: "Alice".to_string(),
        started_at_ms: 1_700_000_000_000,
        track: Some(test_track("miliastra://track/qqmusic/1", "晴天 - 周杰伦")),
        ..ActivePlaybackRequest::default()
    }];

    let value: Value = serde_json::from_str(
        &playback_insights_route(&[], &state).expect("playback insights route"),
    )
    .expect("insights JSON");
    assert_eq!(value["history"][0]["title"], "晴天");
    assert_eq!(value["history"][0]["cached"], true);
    assert_eq!(value["history"][0]["cacheBytes"], 512);
    assert_eq!(value["cache"]["completeEntries"], 3);
    assert_eq!(value["cache"]["activeDownloads"], 1);
    assert!(is_json_route("/playback/insights"));
    assert!(!is_mutating_route("/playback/insights"));
}

#[test]
fn playback_statistics_reset_is_controlled_and_preserves_cache_assets() {
    let state = test_state_with_player_port(HttpTestPlayerPort::successful());
    let value: Value = serde_json::from_str(
        &playback_statistics_reset_route(
            &[
                ("provider".to_string(), "qqmusic".to_string()),
                ("id".to_string(), "song-a".to_string()),
            ],
            &state,
        )
        .expect("statistics reset route"),
    )
    .expect("statistics reset JSON");
    assert_eq!(value["ok"], true);
    assert_eq!(value["cachePreserved"], true);
    assert_eq!(value["metadataPreserved"], true);
    assert!(is_json_route("/playback/statistics/reset"));
    assert!(is_mutating_route("/playback/statistics/reset"));

    let error =
        playback_statistics_reset_route(&[("provider".to_string(), "qqmusic".to_string())], &state)
            .expect_err("missing id rejected");
    assert_eq!(error.status, 400);
}

#[test]
fn playback_cache_tracks_route_returns_paginated_camel_case_list() {
    let state = test_state_with_player_port(HttpTestPlayerPort::successful());

    let value: Value = serde_json::from_str(
        &playback_cache_tracks_route(
            &[
                ("offset".to_string(), "1".to_string()),
                ("limit".to_string(), "2".to_string()),
            ],
            &state,
        )
        .expect("cache tracks route"),
    )
    .expect("cache tracks JSON");

    assert_eq!(value["total"], 3);
    assert_eq!(value["offset"], 1);
    assert_eq!(value["limit"], 2);
    let tracks = value["tracks"].as_array().expect("tracks array");
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0]["hash"], "hash-known-b");
    assert_eq!(tracks[0]["source"], "netease");
    assert_eq!(tracks[0]["id"], "song-b");
    assert_eq!(tracks[0]["title"], "夜曲");
    assert_eq!(tracks[0]["artists"], json!(["周杰伦"]));
    assert_eq!(tracks[0]["bytes"], 4_194_304);
    assert_eq!(tracks[0]["complete"], true);
    assert_eq!(tracks[0]["cachedAtMs"], 1_700_000_200_000_i64);
    assert_eq!(tracks[0]["lastUsedAtMs"], 1_700_000_300_000_i64);
    // 未知孤儿：身份与元数据字段为 null，前端展示为「未知缓存」。
    let orphan = tracks[1].as_object().expect("orphan object");
    assert_eq!(orphan["hash"], "hash-orphan");
    assert!(orphan["source"].is_null());
    assert!(orphan["id"].is_null());
    assert!(orphan["title"].is_null());
    assert!(orphan["album"].is_null());
    assert!(orphan["durationMs"].is_null());

    // 缺省参数：offset=0、limit=100。
    let default_page: Value = serde_json::from_str(
        &playback_cache_tracks_route(&[], &state).expect("default cache tracks route"),
    )
    .expect("default page JSON");
    assert_eq!(default_page["offset"], 0);
    assert_eq!(default_page["limit"], 100);
    assert_eq!(default_page["tracks"].as_array().map(Vec::len), Some(3));

    assert!(is_json_route("/playback/cache/tracks"));
    assert!(!is_mutating_route("/playback/cache/tracks"));
}

#[test]
fn playback_cache_tracks_route_rejects_invalid_pagination() {
    let state = test_state();

    let error = playback_cache_tracks_route(&[("limit".to_string(), "abc".to_string())], &state)
        .expect_err("non-numeric limit rejected");
    assert_eq!(error.status, 400);
    assert!(error.message.contains("limit"));

    let error = playback_cache_tracks_route(&[("offset".to_string(), "-1".to_string())], &state)
        .expect_err("negative offset rejected");
    assert_eq!(error.status, 400);
    assert!(error.message.contains("offset"));

    let error = playback_cache_tracks_route(&[("limit".to_string(), "0".to_string())], &state)
        .expect_err("zero limit rejected");
    assert_eq!(error.status, 400);
    assert!(error.message.contains("limit"));
}

#[test]
fn playback_cache_tracks_contract_over_real_http_and_clamps_limit() {
    let mut state = test_state();
    let server = start_test_http_server(&mut state, "");
    let address = server.local_addr();

    let page = http_get(address, "/playback/cache/tracks?offset=1&limit=2", None);
    assert_eq!(page.status_line, "HTTP/1.1 200 OK");
    assert_eq!(
        page.headers.get("content-type").map(String::as_str),
        Some("application/json; charset=utf-8")
    );
    let value: Value = serde_json::from_str(&page.body).expect("page JSON");
    assert_eq!(value["total"], 3);
    assert_eq!(value["offset"], 1);
    assert_eq!(value["limit"], 2);
    assert_eq!(value["tracks"][0]["title"], "夜曲");

    // 超过最大页大小 500 时收敛到 500，不报错。
    let clamped = http_get(address, "/playback/cache/tracks?limit=9999", None);
    assert_eq!(clamped.status_line, "HTTP/1.1 200 OK");
    let value: Value = serde_json::from_str(&clamped.body).expect("clamped page JSON");
    assert_eq!(value["limit"], 500);

    // 非法参数返回 400。
    let invalid = http_get(address, "/playback/cache/tracks?limit=abc", None);
    assert_eq!(invalid.status_line, "HTTP/1.1 400 Bad Request");
    assert!(invalid.body.contains("limit"));

    server.shutdown().expect("shutdown HTTP server");
}

#[test]
fn remote_song_routes_are_queued_json_post_routes() {
    assert!(is_mutating_route("/searchPlay"));
    assert!(is_mutating_route("/ai/search"));
    assert!(is_json_route("/searchPlay"));
    assert!(is_json_route("/ai/search"));
}

#[test]
fn search_routes_keep_their_existing_response_contracts() {
    let state = test_state();
    let query = [
        ("keyword".to_string(), "晴天".to_string()),
        ("source".to_string(), "netease".to_string()),
    ];

    assert_eq!(
        search_route(&query, &state).unwrap(),
        "raw search: 晴天 [netease]"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&search_candidates_route(&query, &state).unwrap()).unwrap(),
        json!([{
            "trackRef": {"key": {"provider": "netease", "id": "1"}},
            "metadata": {
                "title": "晴天 result",
                "artists": ["测试歌手"],
                "durationMs": 180000
            },
            "eligibility": "unknown",
            "text": "晴天 result",
        }])
    );
}

#[test]
fn search_route_preserves_backend_error_text() {
    let error = player_search_error(HttpPlayerSearchError::new("backend failed"));

    assert_eq!(error.status, 500);
    assert_eq!(error.message, "backend failed");
    assert_eq!(
        player_search_error(HttpPlayerSearchError::new(
            "player search lane queue is full"
        ))
        .message,
        "player search lane queue is full"
    );
}

#[test]
fn playback_control_routes_are_queued_json_post_routes() {
    for route in ["/play", "/pause", "/skip-next", "/skip-prev", "/volume"] {
        assert!(is_mutating_route(route), "{route} should require POST");
        assert!(is_json_route(route), "{route} should return queued JSON");
    }
}

#[test]
fn startup_routes_are_queued_json_post_routes() {
    assert!(is_mutating_route("/startup/game"));
    assert!(is_mutating_route("/startup/wonderland"));
    assert!(is_mutating_route("/startup/enter-wonderland"));
    assert!(is_json_route("/startup/game"));
    assert!(is_json_route("/startup/wonderland"));
    assert!(is_json_route("/startup/enter-wonderland"));
    assert!(PAGE.contains("call('/startup/game','POST')"));
    assert!(PAGE.contains("call('/startup/wonderland','POST')"));
    assert!(PAGE.contains("call('/startup/enter-wonderland','POST')"));
}

#[test]
fn queue_add_defaults_api_requester_when_not_provided() {
    let state = test_state();
    queue_add(
        &[
            ("keyword".to_string(), "测试歌曲".to_string()),
            ("source".to_string(), "qqmusic".to_string()),
        ],
        &state,
    )
    .expect("queue add succeeds");

    let queue = state
        .application
        .queries
        .playback_queue_snapshot()
        .expect("playback queue snapshot");
    assert_eq!(queue[0].requester, "WEB/API");
    // HTTP 默认不绕过去重。
    assert!(!queue[0].dedup_bypass);
}

#[test]
fn queue_add_rejects_obvious_duplicate_without_bypass() {
    let state = test_state();
    queue_add(
        &[
            ("keyword".to_string(), "晴天".to_string()),
            ("source".to_string(), "netease".to_string()),
        ],
        &state,
    )
    .expect("first queue add succeeds");

    let error = queue_add(
        &[
            ("keyword".to_string(), "晴天".to_string()),
            ("source".to_string(), "netease".to_string()),
        ],
        &state,
    )
    .expect_err("duplicate queue add is rejected");
    assert_eq!(error.status, 409);
    assert!(error.message.contains("队列已有"));

    let queue = state
        .application
        .queries
        .playback_queue_snapshot()
        .expect("playback queue snapshot");
    assert_eq!(queue.len(), 1);
}

#[test]
fn queue_add_cross_detects_structured_track_duplicate() {
    let state = test_state();
    // 先入队结构化曲目（标题“晴天”）。
    let track = test_track("miliastra://track/netease/42", "晴天 - 周杰伦");
    state
        .application
        .commands
        .play_track(crate::interfaces::http::PlayTrackRequest {
            track_ref: track.track_ref,
            metadata: track.metadata,
            requester: String::new(),
        })
        .expect("play track succeeds");

    // 待解析项命中结构化曲目：409，不再直接允许明显重复。
    let error = queue_add(
        &[
            ("keyword".to_string(), "晴天".to_string()),
            ("source".to_string(), "netease".to_string()),
        ],
        &state,
    )
    .expect_err("cross-form duplicate queue add is rejected");
    assert_eq!(error.status, 409);

    let queue = state
        .application
        .queries
        .playback_queue_snapshot()
        .expect("playback queue snapshot");
    assert_eq!(queue.len(), 1);
    assert!(queue[0].track.is_some());
}

#[test]
fn play_track_rejects_duplicate_track_key() {
    let state = test_state();
    let request = || {
        let track = test_track("miliastra://track/netease/42", "晴天 - 周杰伦");
        crate::interfaces::http::PlayTrackRequest {
            track_ref: track.track_ref,
            metadata: track.metadata,
            requester: String::new(),
        }
    };
    state
        .application
        .commands
        .play_track(request())
        .expect("first play track succeeds");

    let error = state
        .application
        .commands
        .play_track(request())
        .expect_err("duplicate play track is rejected");
    assert_eq!(error.status, 409);
    assert!(error.message.contains("队列已有"));

    let queue = state
        .application
        .queries
        .playback_queue_snapshot()
        .expect("playback queue snapshot");
    assert_eq!(queue.len(), 1);
}

#[test]
fn page_displays_song_requester() {
    assert!(PAGE.contains("点歌人"));
    assert!(PAGE.contains("it.requester||it.friendUsername"));
    assert!(PAGE.contains("pc.requester"));
}

#[test]
fn page_displays_disk_cache_track_list() {
    assert!(PAGE.contains("磁盘缓存歌曲"));
    assert!(PAGE.contains("id=\"cacheTracks\""));
    assert!(PAGE.contains("loadCacheTracks()"));
    assert!(PAGE.contains("id=\"cacheTracksPrev\""));
    assert!(PAGE.contains("id=\"cacheTracksNext\""));
    assert!(PAGE.contains("changeCacheTracksPage(-1)"));
    assert!(PAGE.contains("changeCacheTracksPage(1)"));
    assert!(PAGE.contains("offset='+cacheTracksOffset+'&limit='+cacheTracksPageSize"));
    assert!(PAGE.contains("共 '+total+' 首 · 第 '"));
    // 未知孤儿曲目展示为「未知缓存」。
    assert!(PAGE.contains("未知缓存"));
    // 缓存接口使用 source 字段，结构化播放接口要求 trackRef.key.provider。
    assert!(PAGE.contains("trackRef:{key:{provider:t.source,id:t.id}"));
    assert!(!PAGE.contains("trackRef:{key:{source:t.source,id:t.id}"));
}

#[test]
fn page_contains_config_center() {
    // 内嵌页面必须与配置中心后端接口配套：导航、页面容器与全部接口调用点。
    assert!(PAGE.contains("配置中心"));
    assert!(PAGE.contains("data-route=\"config\""));
    assert!(PAGE.contains("id=\"page-config\""));
    assert!(PAGE.contains("function loadConfigPage()"));
    assert!(PAGE.contains("function saveConfig()"));
    assert!(PAGE.contains("function restoreDefaults()"));
    assert!(PAGE.contains("function rollbackTo(rev)"));
    assert!(PAGE.contains("function loadConfigRevisions()"));
    assert!(PAGE.contains("request('/config/schema')"));
    assert!(PAGE.contains("request('/config')"));
    assert!(PAGE.contains("request('/config/revisions')"));
    assert!(PAGE.contains("requestJson('/config/validate'"));
    assert!(PAGE.contains("requestJson('/config/save'"));
    assert!(PAGE.contains("requestJson('/config/rollback'"));
    // secret 清除标记与保留语义必须体现在页面中。
    assert!(PAGE.contains("__clear__"));
    assert!(PAGE.contains("留空表示不修改"));
    // 配置表单重绘前必须先同步编辑缓冲；Object 收集需支持数组。
    assert!(PAGE.contains("function syncConfigFormToData()"));
    assert!(PAGE.contains("if(value&&typeof value==='object')return {value}"));
    // 保存成功后的只读刷新失败不能被误报为保存失败。
    assert!(PAGE.contains("配置已保存，但页面刷新失败"));
    // 路由表必须包含 config（导航可达）。
    assert!(PAGE.contains("'config'"));
}

#[test]
fn remote_next_builds_console_game_command() {
    let pending = ApplicationHttpCommandFacade::remote_control_command(
        "下一首".to_string(),
        "下一首",
        ModuleCommand::Playback(PlaybackCommand::Next),
    )
    .into_pending();

    assert_eq!(pending.routed.message_type, "控制台");
    assert_eq!(pending.routed.username, "控制台");
    assert_eq!(pending.routed.raw, "下一首");
    assert_eq!(pending.routed.user_command, "@下一首");
    assert!(matches!(
        pending.routed.command,
        ModuleCommand::Playback(PlaybackCommand::Next)
    ));
}

#[test]
fn remote_volume_builds_console_game_command() {
    let pending = ApplicationHttpCommandFacade::remote_control_command(
        "音量 60".to_string(),
        "音量",
        ModuleCommand::Playback(PlaybackCommand::Volume("60".to_string())),
    )
    .into_pending();

    assert_eq!(pending.routed.raw, "音量 60");
    assert_eq!(pending.routed.user_command, "@音量 60");
    assert!(matches!(
        pending.routed.command,
        ModuleCommand::Playback(PlaybackCommand::Volume(ref volume)) if volume == "60"
    ));
}

#[test]
fn refresh_toggle_runs_full_uncached_refresh_when_resumed() {
    assert!(PAGE.contains("id=\"refreshToggle\""));
    assert!(PAGE.contains("onclick=\"toggleRefresh()\""));
    assert!(PAGE.contains("function toggleRefresh()"));
    assert!(PAGE.contains("if(!refreshPaused)refreshAll()"));
    assert!(PAGE.contains("async function refreshAll()"));
    assert!(PAGE.contains(
        "Promise.allSettled([loadMonitor(),loadHistory(),refreshPlayer(),loadPlaybackInsights(),loadPlayMode()])"
    ));
    assert!(PAGE.contains("async function refreshLoginProgress()"));
    assert!(PAGE.contains("loginRuntimeStatus&&loginRuntimeStatus.active"));
    assert!(!PAGE.contains("setInterval(()=>{if(!refreshPaused)loadLoginState()},2000)"));
    assert!(PAGE.contains("cache:'no-store'"));
    assert!(!PAGE.contains("onclick=\"loadMonitor()\""));
}

#[test]
fn web_inputs_support_enter_submit() {
    assert!(PAGE.contains("function isPlainEnter(e)"));
    assert!(PAGE.contains("!e.isComposing"));
    assert!(PAGE.contains("bindEnter('consoleChatText',sendConsoleChat)"));
    assert!(PAGE.contains("bindEnter('consoleChatPrefix',sendConsoleChat)"));
    assert!(PAGE.contains("bindEnter('keyword',()=>remoteSong(false))"));
    assert!(PAGE.contains("bindEnter('volumeInput',setVolume)"));
    assert!(PAGE.contains("bindEnter('workflowArgs',runWorkflow)"));
    assert!(PAGE.contains("function removeQueueId(id)"));
}

#[test]
fn remote_control_response_includes_trackable_task_id() {
    let state = test_state();
    let body = play_route(&[], &state).expect("play route");
    let response: Value = serde_json::from_str(&body).expect("play response json");
    let task_id = response["taskId"].as_u64().expect("task id");

    assert_eq!(response["queued"], true);
    assert_eq!(response["position"], 1);
    let tasks = state.recording.formal_tasks();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, task_id);
    assert!(matches!(
        tasks[0].kind,
        RecordedFormalTaskKind::Command(ref pending)
            if matches!(
                pending.routed.command,
                ModuleCommand::Playback(PlaybackCommand::Resume)
            )
    ));
}

#[test]
fn waiting_formal_task_can_be_canceled() {
    let state = test_state();
    let body = pause_route(&[], &state).expect("pause route");
    let response: Value = serde_json::from_str(&body).expect("pause response json");
    let task_id = response["taskId"].as_u64().expect("task id");

    let cancel_body = task_cancel_route(&[("id".to_string(), task_id.to_string())], &state)
        .expect("cancel route");
    let canceled: Value = serde_json::from_str(&cancel_body).expect("cancel response json");

    assert_eq!(canceled["canceled"], true);
    assert_eq!(state.recording.cancellation_requests(), [task_id]);
    let tasks = state.recording.formal_tasks();
    assert_eq!(tasks.len(), 1);
    assert!(!tasks[0].queued);
}

#[test]
fn started_formal_task_accepts_a_cancellation_request() {
    let state = test_state();
    let body = pause_route(&[], &state).expect("pause route");
    let response: Value = serde_json::from_str(&body).expect("pause response json");
    let task_id = response["taskId"].as_u64().expect("task id");
    state.recording.mark_started(task_id);

    let body = task_cancel_route(&[("id".to_string(), task_id.to_string())], &state)
        .expect("running task cancellation request");
    let response: Value = serde_json::from_str(&body).expect("cancel response json");

    assert_eq!(response["canceled"], false);
    assert_eq!(response["cancellationRequested"], true);
    assert_eq!(state.recording.cancellation_requests(), [task_id]);
}

#[test]
fn task_cancel_contract_requires_authentication_and_post_before_submitting_id() {
    let mut state = test_state();
    let queued: Value = serde_json::from_str(&pause_route(&[], &state).expect("pause route"))
        .expect("pause response");
    let task_id = queued["taskId"].as_u64().expect("task id");
    let server = start_test_http_server(&mut state, "secret");
    let target = format!("/tasks/cancel?id={task_id}");

    let unauthenticated = http_post(server.local_addr(), &target, None);
    assert_eq!(unauthenticated.status_line, "HTTP/1.1 401 Unauthorized");
    assert!(state.recording.cancellation_requests().is_empty());

    let wrong_method = http_get(server.local_addr(), &target, Some("secret"));
    assert_eq!(wrong_method.status_line, "HTTP/1.1 405 Method Not Allowed");
    assert!(state.recording.cancellation_requests().is_empty());

    let canceled = http_post(server.local_addr(), &target, Some("secret"));
    assert_eq!(canceled.status_line, "HTTP/1.1 200 OK");
    assert_eq!(
        canceled.headers.get("content-type").map(String::as_str),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&canceled.body).expect("cancel response JSON")["ok"],
        true
    );
    assert_eq!(state.recording.cancellation_requests(), [task_id]);

    server.shutdown().expect("shutdown HTTP server");
}

#[test]
fn web_decision_submission_reaches_active_song_decision() {
    let state = test_state();
    let id = 42;

    let body = decision_submit_route(
        &[
            ("id".to_string(), id.to_string()),
            ("action".to_string(), "switch_source".to_string()),
        ],
        &state,
    )
    .expect("decision route");

    let response: Value = serde_json::from_str(&body).expect("decision response");
    assert_eq!(response["decisionId"], id);
    assert_eq!(response["submitted"], "switch_source");
    assert_eq!(
        state.recording.decisions(),
        [(id, DecisionAction::SwitchSource)]
    );
}

#[test]
fn console_chat_prefix_can_be_configured() {
    let state = test_state();

    let default_body = chat_send_route(&[("text".to_string(), "你好".to_string())], &state)
        .expect("default prefix");
    let default_value: Value = serde_json::from_str(&default_body).expect("json response");
    assert_eq!(default_value["message"], "[控制台]: 你好");

    let custom_body = chat_send_route(
        &[
            ("text".to_string(), "你好".to_string()),
            ("prefix".to_string(), "[远程] ".to_string()),
        ],
        &state,
    )
    .expect("custom prefix");
    let custom_value: Value = serde_json::from_str(&custom_body).expect("json response");
    assert_eq!(custom_value["message"], "[远程] 你好");

    let raw_body = chat_send_route(
        &[
            ("text".to_string(), "你好".to_string()),
            ("usePrefix".to_string(), "0".to_string()),
            ("prefix".to_string(), "[远程] ".to_string()),
        ],
        &state,
    )
    .expect("no prefix");
    let raw_value: Value = serde_json::from_str(&raw_body).expect("json response");
    assert_eq!(raw_value["message"], "你好");

    let tasks = state.recording.formal_tasks();
    let messages = tasks
        .iter()
        .map(|task| match &task.kind {
            RecordedFormalTaskKind::ConsoleChat { text, prefix } => {
                format!("{prefix}{text}")
            }
            other => panic!("unexpected task: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(messages, ["[控制台]: 你好", "[远程] 你好", "你好"]);
}

#[test]
fn chat_send_requires_non_empty_text() {
    let error = normalize_required_text(Some("  "), "text").unwrap_err();

    assert_eq!(error.status, 400);
    assert!(error.message.contains("缺少text参数"));
}

#[test]
fn remote_song_command_builds_console_plain_song() {
    let state = test_state();
    state
        .application
        .commands
        .remote_song("晴天 伴奏".to_string(), "qqmusic".to_string(), false, false)
        .expect("remote song command");
    let tasks = state.recording.formal_tasks();
    let RecordedFormalTaskKind::Command(pending) = &tasks[0].kind else {
        panic!("expected song command");
    };

    assert_eq!(pending.routed.message_type, "控制台");
    assert_eq!(pending.routed.username, "控制台");
    assert_eq!(pending.routed.raw, "点歌 晴天 伴奏");
    match &pending.routed.command {
        ModuleCommand::SongRequest(song) => {
            assert_eq!(song.keyword, "晴天");
            assert_eq!(song.source, SongSource::QqMusic);
            assert!(song.prefer_accompaniment);
            assert!(!song.ai_assisted);
            assert!(song.friend_username.is_empty());
        }
        _ => panic!("expected song command"),
    }
}

#[test]
fn remote_song_command_builds_console_ai_song() {
    let state = test_state();
    state
        .application
        .commands
        .remote_song("晴天".to_string(), "qqmusic".to_string(), false, true)
        .expect("remote ai song command");
    let tasks = state.recording.formal_tasks();
    let RecordedFormalTaskKind::Command(pending) = &tasks[0].kind else {
        panic!("expected song command");
    };

    assert_eq!(pending.routed.raw, "AI点歌 晴天");
    match &pending.routed.command {
        ModuleCommand::SongRequest(song) => {
            assert_eq!(song.source, SongSource::All);
            assert!(song.ai_assisted);
        }
        _ => panic!("expected song command"),
    }
}

#[test]
fn remote_song_command_supports_bilibili_source() {
    let state = test_state();
    state
        .application
        .commands
        .remote_song(
            "耀斑 HOYO-MiX".to_string(),
            "bilibili".to_string(),
            false,
            false,
        )
        .expect("remote bilibili song command");
    let tasks = state.recording.formal_tasks();
    let RecordedFormalTaskKind::Command(pending) = &tasks[0].kind else {
        panic!("expected song command");
    };

    assert_eq!(pending.routed.raw, "B站点歌 耀斑 HOYO-MiX");
    match &pending.routed.command {
        ModuleCommand::SongRequest(song) => {
            assert_eq!(song.source, SongSource::Bilibili);
            assert_eq!(song.prefix, "B站点歌");
        }
        _ => panic!("expected song command"),
    }
}

#[test]
fn queue_removal_by_id_survives_automatic_front_shift() {
    let state = test_state();
    for keyword in ["第一首", "第二首", "第三首"] {
        queue_add(
            &[
                ("keyword".to_string(), keyword.to_string()),
                ("source".to_string(), "qqmusic".to_string()),
            ],
            &state,
        )
        .expect("queue push");
    }
    let third_id = state
        .application
        .queries
        .playback_queue_snapshot()
        .expect("queue snapshot")[2]
        .id;
    let removed = queue_remove(&[], &state).expect("queue shift");
    let removed: Value = serde_json::from_str(&removed).expect("remove response");
    assert_eq!(removed["removed"]["keyword"], "第一首");

    let body =
        queue_remove(&[("id".to_string(), third_id.to_string())], &state).expect("remove by id");
    let response: Value = serde_json::from_str(&body).expect("remove response json");

    assert_eq!(response["removed"]["id"], third_id);
    assert_eq!(response["removed"]["keyword"], "第三首");
    let queue = state
        .application
        .queries
        .playback_queue_snapshot()
        .expect("queue snapshot");
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].keyword, "第二首");
}

#[test]
fn runtime_state_routes_preserve_patch_and_response_contracts() {
    let state = test_state();
    let saved = state_save(
            &[(
                "json".to_string(),
                r#"{"hallRemainingMinutes":42,"hallRemainingUpdatedAt":1234,"hallExpiringWarningSent":true}"#
                    .to_string(),
            )],
            &state,
        )
        .expect("state patch succeeds");
    let saved: Value = serde_json::from_str(&saved).expect("save response json");
    let snapshot: Value = serde_json::from_str(&state_json(&state).expect("state json"))
        .expect("state snapshot json");

    assert_eq!(saved, json!({ "ok": true }));
    assert_eq!(snapshot["hallRemainingMinutes"], 42);
    assert_eq!(snapshot["hallRemainingUpdatedAt"], 1234);
    assert_eq!(snapshot["hallExpiringWarningSent"], true);
    assert!(snapshot.get("hallRemainingMinutesNow").is_some());
    assert!(snapshot["playback"].get("previousRequests").is_none());

    state_save(
        &[(
            "json".to_string(),
            r#"{"hallRemainingMinutes":null,"hallRemainingUpdatedAt":null}"#.to_string(),
        )],
        &state,
    )
    .expect("state clear succeeds");
    let cleared: Value =
        serde_json::from_str(&state_json(&state).expect("state json")).expect("cleared state json");
    assert!(cleared["hallRemainingMinutes"].is_null());
    assert!(cleared["hallRemainingUpdatedAt"].is_null());
}

#[test]
fn remote_plain_song_rejects_multi_source() {
    let state = test_state();
    let error = state
        .application
        .commands
        .remote_song(
            "晴天".to_string(),
            "qqmusic,netease".to_string(),
            false,
            false,
        )
        .unwrap_err();

    assert_eq!(error.status, 400);
    assert!(error.message.contains("source只允许"));
}

#[test]
fn screenshot_quality_is_bounded() {
    assert_eq!(parse_jpeg_quality(None).unwrap(), 88);
    assert_eq!(parse_jpeg_quality(Some("95")).unwrap(), 95);
    assert!(parse_jpeg_quality(Some("79")).is_err());
    assert!(parse_jpeg_quality(Some("96")).is_err());
}

#[test]
fn chat_listener_mode_route_enqueues_secondary_switch() {
    let state = test_state();
    let query = vec![("mode".to_string(), "secondary".to_string())];

    let response = chat_listener_mode_route(&query, &state).expect("listener route");
    let response: Value = serde_json::from_str(&response).expect("listener response json");

    assert_eq!(response["queued"], true);
    assert_eq!(response["mode"], "primary");
    assert_eq!(response["pendingMode"], "secondary");
    assert_eq!(
        state.recording.mutations(),
        [RecordedMutation::ChatListenerModeRequest(
            ChatListenerMode::Secondary
        )]
    );
    let tasks = state.recording.formal_tasks();
    assert_eq!(tasks.len(), 1);
    assert!(matches!(
        tasks[0].kind,
        RecordedFormalTaskKind::ListenerMode(ChatListenerMode::Secondary)
    ));
}

#[test]
fn chat_listener_mode_enqueue_failure_rolls_back_request() {
    let state = test_state();
    state.recording.fail_listener_enqueue();
    let query = vec![("mode".to_string(), "secondary".to_string())];

    let error = chat_listener_mode_route(&query, &state).unwrap_err();

    assert_eq!(error.status, 500);
    assert_eq!(
        state.recording.mutations(),
        [
            RecordedMutation::ChatListenerModeRequest(ChatListenerMode::Secondary),
            RecordedMutation::ChatListenerModeCancel(ChatListenerMode::Secondary),
        ]
    );
    assert!(state.recording.formal_tasks().is_empty());
}

fn test_state() -> HttpTestState {
    test_state_with_player_port(HttpTestPlayerPort::successful())
}

/// 构造 bootstrap.http.access_token 为非空值的测试状态，用于验证配置中心
/// 路由对启动引导注入令牌的脱敏（http 段未入库，脱敏必须覆盖注入值）。
fn test_state_with_bootstrap_access_token(access_token: &str) -> HttpTestState {
    test_state_with_player_port_and_bootstrap_token(HttpTestPlayerPort::successful(), access_token)
}

#[test]
fn internal_errors_use_a_generic_message_instead_of_leaking_details() {
    let error = internal_error(anyhow!("读取配置失败: C:\\secret\\path\\config.yaml"));
    assert_eq!(error.status, 500);
    assert_eq!(error.message, "内部错误");
}

#[test]
fn access_token_matching_is_exact_and_length_safe() {
    let make = |token: &str| Request {
        method: "GET".to_string(),
        path: "/status".to_string(),
        query: Vec::new(),
        headers: {
            let mut headers = HeaderMap::new();
            headers.insert(
                "x-miliastra-token",
                HeaderValue::from_str(token).expect("token header"),
            );
            headers
        },
        body: Vec::new(),
    };
    assert!(has_valid_access_token(&make("secret"), "secret"));
    assert!(!has_valid_access_token(&make("secret2"), "secret"));
    assert!(!has_valid_access_token(&make("sec"), "secret"));
    assert!(!has_valid_access_token(&make(""), "secret"));
    assert!(!has_valid_access_token(&make("SECRET"), "secret"));
}

#[test]
fn hall_state_patch_rejects_invalid_values_instead_of_silently_ignoring() {
    let mut patch = HashMap::new();
    patch.insert("hallRemainingMinutes".to_string(), serde_json::json!("abc"));
    assert_eq!(hall_state_patch(&patch).unwrap_err().status, 400);

    let mut patch = HashMap::new();
    patch.insert(
        "hallRemainingMinutes".to_string(),
        serde_json::json!(u64::MAX),
    );
    assert_eq!(hall_state_patch(&patch).unwrap_err().status, 400);

    let mut patch = HashMap::new();
    patch.insert(
        "hallRemainingUpdatedAt".to_string(),
        serde_json::json!("now"),
    );
    assert_eq!(hall_state_patch(&patch).unwrap_err().status, 400);

    let mut patch = HashMap::new();
    patch.insert(
        "hallExpiringWarningSent".to_string(),
        serde_json::json!("yes"),
    );
    assert_eq!(hall_state_patch(&patch).unwrap_err().status, 400);

    // 合法值与 null 清空保持可用。
    let mut patch = HashMap::new();
    patch.insert("hallRemainingMinutes".to_string(), serde_json::json!(null));
    patch.insert(
        "hallRemainingUpdatedAt".to_string(),
        serde_json::json!(123456),
    );
    patch.insert(
        "hallExpiringWarningSent".to_string(),
        serde_json::json!(true),
    );
    let result = hall_state_patch(&patch).expect("valid patch");
    assert_eq!(result.remaining_minutes, Some(None));
    assert_eq!(result.remaining_updated_at, Some(Some(123456)));
    assert_eq!(result.expiring_warning_sent, Some(true));
}

#[test]
fn search_source_route_requires_an_explicit_source() {
    let state = test_state();
    let error = search_source_route(&[], &state).unwrap_err();
    assert_eq!(error.status, 400);
    assert!(error.message.contains("source"));

    let query = [("source".to_string(), "qqmusic".to_string())];
    // 有 source 时不再报“必须提供 source”；缺 keyword 等其余参数属正常业务错误。
    let error = search_source_route(&query, &state).unwrap_err();
    assert!(!error.message.contains("必须提供 source"));
}

#[test]
fn startup_wonderland_partial_enqueue_reports_which_tasks_queued() {
    let state = test_state();
    state.recording.fail_startup_enqueue();
    let value: Value =
        serde_json::from_str(&enqueue_startup_wonderland(&state).expect("partial response"))
            .expect("response JSON");

    assert_eq!(value["ok"], true);
    assert_eq!(value["queued"], true);
    assert_eq!(value["allQueued"], false);
    assert_eq!(value["taskIds"].as_array().unwrap().len(), 1);
    assert_eq!(value["failed"][0]["index"], 1);
    assert!(
        value["failed"][0]["error"]
            .as_str()
            .unwrap()
            .contains("内部错误")
    );
}

#[test]
fn startup_wonderland_full_success_keeps_existing_contract() {
    let state = test_state();
    let value: Value =
        serde_json::from_str(&enqueue_startup_wonderland(&state).expect("full response"))
            .expect("response JSON");

    assert_eq!(value["ok"], true);
    assert_eq!(value["queued"], true);
    // 全部成功走原版响应契约（无 allQueued 字段）。
    assert!(value.get("allQueued").is_none());
    assert_eq!(value["taskIds"].as_array().unwrap().len(), 2);
    assert!(value.get("failed").is_none());
}

fn test_state_with_player_port(player: impl HttpPlayerPort + 'static) -> HttpTestState {
    test_state_with_player_port_and_bootstrap_token(player, "")
}

fn test_state_with_player_port_and_bootstrap_token(
    player: impl HttpPlayerPort + 'static,
    access_token: &str,
) -> HttpTestState {
    let mut config: AppConfig =
        serde_yaml::from_str(include_str!("../../../../tests/fixtures/config.full.yaml"))
            .expect("default config");
    // 配置中心的 bootstrap 注入值：非空时用于验证 http.access_token 脱敏。
    config.http.access_token = access_token.to_string();
    let monitor = MonitorShared::new(20);
    let custom_workflow = custom_workflow_service_from_config_parts(
        &config.custom_workflows,
        &config.timing,
        &config.ocr,
    );
    let recording = Arc::new(RecordingHttpPort::new());
    let login = Arc::new(HttpTestLoginPort::default());
    // 配置中心：临时目录新建 ConfigStore，供 /config 系列路由读写。
    let config_root = std::env::temp_dir().join(format!("http-config-store-{}", Uuid::new_v4()));
    let database_path = config_root.join("deps/data/playback.sqlite3");
    fs::create_dir_all(database_path.parent().unwrap()).unwrap();
    let config_store: crate::config::SharedConfigStore = Arc::new(Mutex::new(
        crate::config::ConfigStore::open(
            &database_path,
            &config_root,
            crate::config::BootstrapConfig {
                database_path: database_path.clone(),
                http: config.http.clone(),
                logging: config.logging.clone(),
            },
        )
        .expect("open config store"),
    ));
    HttpTestState {
        state: HttpSharedState::new(
            HttpInterfaceConfig::new(
                config.http.clone(),
                config.screen.clone(),
                config.templates.clone(),
                config.moderation.clone(),
                config.startup.clone(),
                config.invite.clone(),
                config.timing.clone(),
                config.custom_workflows.clone(),
            ),
            monitor,
            Arc::new(Mutex::new(LatestFrameCache::default())),
            config_store,
            crate::config::LiveConfigs::from_config(&config),
            HttpApplicationPorts::new(
                Arc::new(ApplicationHttpCommandFacade::new(
                    recording.clone(),
                    custom_workflow,
                )),
                recording.clone(),
                recording.clone(),
                recording.clone(),
                Arc::new(player),
                login.clone(),
                Arc::new(HttpTestAiPort),
            ),
        ),
        recording,
        config_root: Some(config_root),
    }
}

/// 从配置中心读取当前完整配置 JSON（load_full 序列化，含注入段，未脱敏），
/// 作为保存/校验请求的候选段基底（与 ConfigStore::save 的整段替换语义一致）。
fn full_config_sections(state: &HttpSharedState) -> Value {
    let store = state.config_store.lock().expect("config store");
    serde_json::to_value(store.load_full().expect("load full config")).expect("serialize config")
}

#[test]
fn config_routes_have_expected_methods() {
    for route in [
        "/config",
        "/config/schema",
        "/config/section",
        "/config/revisions",
    ] {
        assert!(!is_mutating_route(route), "{route} should allow GET");
        assert!(is_json_route(route), "{route} should return JSON");
    }
    for route in ["/config/validate", "/config/save", "/config/rollback"] {
        assert!(is_mutating_route(route), "{route} should require POST");
        assert!(is_json_route(route), "{route} should return JSON");
    }
}

#[test]
fn config_route_masks_secrets_and_reports_revision() {
    let state = test_state();
    // 保存含 api_key 的配置 → revision 2。
    let mut sections = full_config_sections(&state);
    sections["ai"]["api_key"] = json!("sk-test-http-123");
    let body = serde_json::to_string(&json!({ "baseRevision": 1, "sections": sections })).unwrap();
    let saved: Value =
        serde_json::from_str(&config_save_route(body.as_bytes(), &state).unwrap()).unwrap();
    assert_eq!(saved["ok"], true);
    assert_eq!(saved["revision"], 2);

    let response: Value = serde_json::from_str(&config_route(&[], &state).unwrap()).unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(response["revision"], 2);
    assert_eq!(response["schemaVersion"], 1);
    assert_eq!(response["sections"]["ai"]["api_key"], "••••••");
    // 数据库中的真实密钥必须保留（脱敏只影响展示）。
    let store = state.config_store.lock().unwrap();
    assert_eq!(store.load_full().unwrap().ai.api_key, "sk-test-http-123");
}

#[test]
fn config_route_keeps_relative_paths_unresolved() {
    // 显示值必须保持库中相对路径（deps/assets/...），不能返回解析后的绝对路径，
    // 否则 Web 表单回填绝对路径并在保存时把绝对路径写回配置库。
    let state = test_state();
    let response: Value = serde_json::from_str(&config_route(&[], &state).unwrap()).unwrap();
    let blue = response["sections"]["templates"]["blue_marker"]
        .as_str()
        .expect("blue_marker 必须是字符串");
    assert!(
        blue.starts_with("deps/assets/"),
        "显示值应保持相对路径，实际: {blue}"
    );
    let idioms = response["sections"]["idiom_chain"]["lexicon_path"]
        .as_str()
        .expect("lexicon_path 必须是字符串");
    assert!(
        idioms.starts_with("deps/assets/"),
        "成语词库显示值应保持相对路径，实际: {idioms}"
    );
    // http/logging 段不落库，必须由 bootstrap 注入且保持可显示。
    assert_eq!(response["sections"]["http"]["port"], 18888);
    assert!(response["sections"]["logging"]["dir"].is_string());
    // 与 load_full（运行态解析后）不同：显示值保持相对，运行值仍为绝对。
    let store = state.config_store.lock().unwrap();
    let runtime = store.load_full().unwrap();
    assert!(runtime.templates.blue_marker.is_absolute());
}

#[test]
fn config_schema_route_lists_all_sections_with_defaults() {
    let state = test_state();
    let response: Value = serde_json::from_str(&config_schema_route(&[], &state).unwrap()).unwrap();
    assert_eq!(response["ok"], true);
    let sections = response["sections"].as_array().expect("sections array");
    assert!(!sections.is_empty());
    let queue = sections
        .iter()
        .find(|section| section["name"] == "queue")
        .expect("queue section");
    let max_size = queue["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|field| field["path"] == "queue.max_size")
        .expect("queue.max_size field");
    assert_eq!(max_size["kind"]["type"], "int");
    assert_eq!(max_size["kind"]["min"], 1);
    assert_eq!(max_size["default"], 5);
    assert_eq!(max_size["effect"], "restart");
    assert_eq!(max_size["source"], "db");
    assert_eq!(max_size["nullable"], false);
    // secret 字段的类型。
    let ai = sections
        .iter()
        .find(|section| section["name"] == "ai")
        .expect("ai section");
    let api_key = ai["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|field| field["path"] == "ai.api_key")
        .expect("ai.api_key field");
    assert_eq!(api_key["kind"]["type"], "secret");
    // audio_cache 默认 null，但其子字段 default 用"启用后默认对象"提供
    // （Web「启用」开关预填来源，避免用户手填 JSON）。
    let playback = sections
        .iter()
        .find(|section| section["name"] == "playback")
        .expect("playback section");
    let cache_enabled = playback["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|field| field["path"] == "playback.audio_cache.enabled")
        .expect("audio_cache.enabled field");
    assert_eq!(cache_enabled["default"], json!(true));
    assert_eq!(cache_enabled["optionalParent"], true);
    let cache_dir = playback["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|field| field["path"] == "playback.audio_cache.directory")
        .expect("audio_cache.directory field");
    assert_eq!(cache_dir["default"], json!("deps/cache/audio"));
    let cache_max = playback["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|field| field["path"] == "playback.audio_cache.max_bytes_mb")
        .expect("audio_cache.max_bytes_mb field");
    assert_eq!(cache_max["default"], json!(20 * 1024));
}

#[test]
fn config_section_route_returns_values_and_revision() {
    let state = test_state();
    let response: Value = serde_json::from_str(
        &config_section_route(&[("name".to_string(), "queue".to_string())], &state).unwrap(),
    )
    .unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(response["name"], "queue");
    assert_eq!(response["label"], "点歌队列");
    assert_eq!(response["values"]["max_size"], 5);
    assert_eq!(response["revision"], 1);
    let fields = response["fields"].as_array().unwrap();
    assert!(fields.iter().any(|field| field["path"] == "queue.max_size"));
}

#[test]
fn config_section_route_unknown_section_fails() {
    let state = test_state();
    let error = config_section_route(&[("name".to_string(), "不存在".to_string())], &state)
        .expect_err("unknown section rejected");
    assert_eq!(error.status, 400);
    assert!(error.message.contains("配置段不存在"));
}

#[test]
fn config_section_route_masks_secret_fields() {
    let state = test_state();
    // 保存 ai.api_key 与 song_review.provider.api_key 真实值 → revision 2。
    let mut sections = full_config_sections(&state);
    sections["ai"]["api_key"] = json!("sk-ai-real-1");
    sections["song_review"]["provider"]["api_key"] = json!("sk-song-real-1");
    let body = serde_json::to_string(&json!({ "baseRevision": 1, "sections": sections })).unwrap();
    let saved: Value =
        serde_json::from_str(&config_save_route(body.as_bytes(), &state).unwrap()).unwrap();
    assert_eq!(saved["ok"], true);
    assert_eq!(saved["revision"], 2);
    // 再保存 turtle_soup.ai.api_key 真实值 → revision 3。
    let mut sections = full_config_sections(&state);
    sections["turtle_soup"]["ai"]["api_key"] = json!("sk-soup-real-1");
    let body = serde_json::to_string(&json!({ "baseRevision": 2, "sections": sections })).unwrap();
    let saved: Value =
        serde_json::from_str(&config_save_route(body.as_bytes(), &state).unwrap()).unwrap();
    assert_eq!(saved["ok"], true);
    assert_eq!(saved["revision"], 3);

    // ai 段：api_key 必须掩码且响应不含真实值。
    let ai: Value = serde_json::from_str(
        &config_section_route(&[("name".to_string(), "ai".to_string())], &state).unwrap(),
    )
    .unwrap();
    assert_eq!(ai["values"]["api_key"], crate::config::SECRET_MASK);
    assert!(
        !serde_json::to_string(&ai["values"])
            .unwrap()
            .contains("sk-ai-real-1"),
        "ai 段不得泄漏真实 api_key"
    );

    // song_review 段：provider.api_key 必须掩码。
    let song: Value = serde_json::from_str(
        &config_section_route(&[("name".to_string(), "song_review".to_string())], &state).unwrap(),
    )
    .unwrap();
    assert_eq!(
        song["values"]["provider"]["api_key"],
        crate::config::SECRET_MASK
    );
    assert!(
        !serde_json::to_string(&song["values"])
            .unwrap()
            .contains("sk-song-real-1"),
        "song_review 段不得泄漏真实 api_key"
    );

    // turtle_soup 段：ai.api_key 必须掩码。
    let soup: Value = serde_json::from_str(
        &config_section_route(&[("name".to_string(), "turtle_soup".to_string())], &state).unwrap(),
    )
    .unwrap();
    assert_eq!(soup["values"]["ai"]["api_key"], crate::config::SECRET_MASK);
    assert!(
        !serde_json::to_string(&soup["values"])
            .unwrap()
            .contains("sk-soup-real-1"),
        "turtle_soup 段不得泄漏真实 api_key"
    );

    // http 段：bootstrap 注入的 access_token 即使未保存也必须掩码。
    let http_token = state
        .config_store
        .lock()
        .unwrap()
        .bootstrap()
        .http
        .access_token
        .clone();
    let http: Value = serde_json::from_str(
        &config_section_route(&[("name".to_string(), "http".to_string())], &state).unwrap(),
    )
    .unwrap();
    assert_eq!(http["values"]["access_token"], crate::config::SECRET_MASK);
    if !http_token.is_empty() {
        assert!(
            !serde_json::to_string(&http["values"])
                .unwrap()
                .contains(&http_token),
            "http 段不得泄漏 bootstrap access_token"
        );
    }

    // 数据库中真实密钥必须保留（脱敏只影响展示）。
    let store = state.config_store.lock().unwrap();
    let full = store.load_full().unwrap();
    assert_eq!(full.ai.api_key, "sk-ai-real-1");
    assert_eq!(full.song_review.provider.api_key, "sk-song-real-1");
    assert_eq!(full.turtle_soup.ai.api_key, "sk-soup-real-1");
}

#[test]
fn config_routes_mask_non_empty_bootstrap_access_token() {
    // bootstrap http.access_token 非空时（未入库、由启动引导注入），
    // /config 与 /config/section?name=http 的响应都不得包含明文。
    let state = test_state_with_bootstrap_access_token("bootstrap-secret-token-123");

    let full: Value = serde_json::from_str(&config_route(&[], &state).unwrap()).unwrap();
    assert_eq!(
        full["sections"]["http"]["access_token"],
        crate::config::SECRET_MASK,
        "/config 中 http.access_token 必须掩码"
    );
    let serialized = serde_json::to_string(&full).unwrap();
    assert!(
        !serialized.contains("bootstrap-secret-token-123"),
        "/config 响应不得包含 bootstrap access_token 明文"
    );

    let section: Value = serde_json::from_str(
        &config_section_route(&[("name".to_string(), "http".to_string())], &state).unwrap(),
    )
    .unwrap();
    assert_eq!(
        section["values"]["access_token"],
        crate::config::SECRET_MASK,
        "/config/section 中 http.access_token 必须掩码"
    );
    assert!(
        !serde_json::to_string(&section["values"])
            .unwrap()
            .contains("bootstrap-secret-token-123"),
        "/config/section 响应不得包含 bootstrap access_token 明文"
    );

    // 脱敏只影响展示：bootstrap 中的真实令牌必须原样保留。
    assert_eq!(
        state
            .config_store
            .lock()
            .unwrap()
            .bootstrap()
            .http
            .access_token,
        "bootstrap-secret-token-123",
        "bootstrap 中的真实 access_token 必须保留"
    );
}

#[test]
fn config_revisions_route_lists_history() {
    let state = test_state();
    let response: Value =
        serde_json::from_str(&config_revisions_route(&[], &state).unwrap()).unwrap();
    assert_eq!(response["ok"], true);
    let revisions = response["revisions"].as_array().unwrap();
    assert_eq!(revisions.len(), 1);
    assert_eq!(revisions[0]["revision"], 1);
    assert!(revisions[0]["createdAtMs"].as_u64().is_some());
}

#[test]
fn config_validate_route_reports_field_errors() {
    let state = test_state();
    let mut sections = full_config_sections(&state);
    sections["window"]["content_width"] = json!(0);
    let body = serde_json::to_string(&json!({ "sections": sections })).unwrap();
    let response: Value =
        serde_json::from_str(&config_validate_route(body.as_bytes(), &state).unwrap()).unwrap();
    assert_eq!(response["ok"], true);
    let errors = response["errors"].as_array().expect("errors array");
    assert!(!errors.is_empty());
    assert!(
        errors
            .iter()
            .any(|error| error["field"] == "window.content_width")
    );
    assert!(errors.iter().any(|error| error["section"] == "window"));
}

#[test]
fn config_validate_route_rejects_missing_sections() {
    let state = test_state();
    let error = config_validate_route(b"{}", &state).expect_err("missing sections rejected");
    assert_eq!(error.status, 400);
    assert!(error.message.contains("sections"));
    // 段值不是对象。
    let error = config_validate_route(r#"{"sections":{"queue":[1,2]}}"#.as_bytes(), &state)
        .expect_err("non-object section rejected");
    assert_eq!(error.status, 400);
    // 非 JSON 请求体。
    let error = config_validate_route(b"not-json", &state).expect_err("invalid JSON rejected");
    assert_eq!(error.status, 400);
}

#[test]
fn config_save_route_persists_and_reports_changed_fields() {
    let state = test_state();
    let mut sections = full_config_sections(&state);
    sections["queue"]["max_size"] = json!(20);
    let body = serde_json::to_string(&json!({ "baseRevision": 1, "sections": sections })).unwrap();
    let response: Value =
        serde_json::from_str(&config_save_route(body.as_bytes(), &state).unwrap()).unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(response["revision"], 2);
    let changed = response["changedFields"].as_array().unwrap();
    assert!(changed.contains(&json!("queue.max_size")));
    assert_eq!(response["restartRequired"], true);
    assert!(
        response["restartFields"]
            .as_array()
            .unwrap()
            .contains(&json!("queue.max_size"))
    );
    assert!(response["appliedLiveFields"].as_array().unwrap().is_empty());
    let store = state.config_store.lock().unwrap();
    assert_eq!(store.load_full().unwrap().queue.max_size, 20);
}

#[test]
fn config_save_route_applies_live_fields_to_shared_values() {
    let state = test_state();
    // 预热：load_full 返回的是绝对路径，先按原值提交一次，使库中值与候选
    // 基底一致（避免路径解析差异污染后续变更集合），revision 2。
    let sections = full_config_sections(&state);
    let body = serde_json::to_string(&json!({ "baseRevision": 1, "sections": sections })).unwrap();
    let warm_up: Value =
        serde_json::from_str(&config_save_route(body.as_bytes(), &state).unwrap()).unwrap();
    assert_eq!(warm_up["ok"], true);
    assert_eq!(warm_up["revision"], 2);

    // 仅变更 Live 字段：queue.protect_current_song_until_finished → 无需重启。
    let mut sections = full_config_sections(&state);
    sections["queue"]["protect_current_song_until_finished"] = json!(false);
    let body = serde_json::to_string(&json!({ "baseRevision": 2, "sections": sections })).unwrap();
    let response: Value =
        serde_json::from_str(&config_save_route(body.as_bytes(), &state).unwrap()).unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(response["revision"], 3);
    assert!(
        response["appliedLiveFields"]
            .as_array()
            .unwrap()
            .contains(&json!("queue.protect_current_song_until_finished")),
        "live 字段必须进入 appliedLiveFields: {}",
        response
    );
    assert_eq!(
        response["restartRequired"], false,
        "仅 live 变更时不得要求重启: {}",
        response
    );
    assert!(response["restartFields"].as_array().unwrap().is_empty());
    // 共享热更新值必须已覆盖：运行态读取点立即看到新值（不虚报已生效）。
    assert!(
        !*state
            .live_configs
            .queue_protect_current_song
            .read()
            .unwrap()
    );
    assert!(
        !state
            .config_store
            .lock()
            .unwrap()
            .load_full()
            .unwrap()
            .queue
            .protect_current_song_until_finished,
        "库中值必须同步落盘"
    );

    // 混合变更（Live + Restart）：restartRequired=true，Live 字段仍进 appliedLiveFields。
    let mut sections = full_config_sections(&state);
    sections["queue"]["protect_current_song_until_finished"] = json!(true);
    sections["queue"]["max_size"] = json!(30);
    let body = serde_json::to_string(&json!({ "baseRevision": 3, "sections": sections })).unwrap();
    let response: Value =
        serde_json::from_str(&config_save_route(body.as_bytes(), &state).unwrap()).unwrap();
    assert_eq!(response["restartRequired"], true);
    assert!(
        response["appliedLiveFields"]
            .as_array()
            .unwrap()
            .contains(&json!("queue.protect_current_song_until_finished"))
    );
    assert!(
        response["restartFields"]
            .as_array()
            .unwrap()
            .contains(&json!("queue.max_size"))
    );
    assert!(
        *state
            .live_configs
            .queue_protect_current_song
            .read()
            .unwrap()
    );
}

#[test]
fn config_rollback_route_applies_live_fields_to_shared_values() {
    let state = test_state();
    // 预热：先按原值提交一次使库中值与候选基底一致，revision 2。
    let sections = full_config_sections(&state);
    let body = serde_json::to_string(&json!({ "baseRevision": 1, "sections": sections })).unwrap();
    let warm_up: Value =
        serde_json::from_str(&config_save_route(body.as_bytes(), &state).unwrap()).unwrap();
    assert_eq!(warm_up["revision"], 2);
    // 保存 live 字段（protect=false）→ revision 3，共享值立即变化。
    let mut sections = full_config_sections(&state);
    sections["queue"]["protect_current_song_until_finished"] = json!(false);
    let body = serde_json::to_string(&json!({ "baseRevision": 2, "sections": sections })).unwrap();
    config_save_route(body.as_bytes(), &state).unwrap();
    assert!(
        !*state
            .live_configs
            .queue_protect_current_song
            .read()
            .unwrap()
    );
    // 回滚到 revision 2（默认 protect=true）→ 共享值恢复为 true。
    let body = serde_json::to_string(&json!({ "revision": 2, "baseRevision": 3 })).unwrap();
    let response: Value =
        serde_json::from_str(&config_rollback_route(body.as_bytes(), &state).unwrap()).unwrap();
    assert_eq!(response["ok"], true);
    assert!(
        response["appliedLiveFields"]
            .as_array()
            .unwrap()
            .contains(&json!("queue.protect_current_song_until_finished"))
    );
    assert!(
        *state
            .live_configs
            .queue_protect_current_song
            .read()
            .unwrap()
    );
}

#[test]
fn config_save_succeeds_even_when_live_apply_fails() {
    let state = test_state();
    // 落库成功：手动走 store.save（等价于 config_save_route 的保存成功分支）。
    let sections = full_config_sections(&state);
    let mut store = state.config_store.lock().unwrap();
    let outcome = store
        .save(1, sections.as_object().expect("sections 对象").clone())
        .unwrap();
    assert_eq!(outcome.revision, 2);

    // 模拟「落库成功后热更新应用阶段读取失败」：从另一连接破坏 config_sections 表
    // （真实场景：保存提交后存储异常/被外部工具修改，apply 阶段 load_full 失败）。
    let database_path = state
        .config_root
        .as_ref()
        .expect("config root")
        .join("deps/data/playback.sqlite3");
    let connection = rusqlite::Connection::open(&database_path).expect("open sqlite");
    connection
        .execute_batch("DROP TABLE config_sections")
        .expect("drop config_sections");
    drop(connection);
    // 模拟必须使 apply 阶段读取失败，否则测试失去意义。
    assert!(store.load_full().is_err(), "破坏表后 load_full 必须失败");

    // 保存成功分支的收尾：apply 失败被吞掉，仍返回保存成功响应（revision 等）。
    let response: Value = serde_json::from_str(
        &save_outcome_response_after_apply(&state, &store, outcome, "配置已保存")
            .expect("save response"),
    )
    .expect("response JSON");
    assert_eq!(response["ok"], true);
    assert_eq!(response["revision"], 2);
}

#[test]
fn config_save_route_stale_base_revision_conflicts() {
    let state = test_state();
    let sections = full_config_sections(&state);
    let body = serde_json::to_string(&json!({ "baseRevision": 0, "sections": sections })).unwrap();
    let response: Value =
        serde_json::from_str(&config_save_route(body.as_bytes(), &state).unwrap()).unwrap();
    assert_eq!(response["ok"], false);
    assert_eq!(response["code"], "config_conflict");
    assert_eq!(response["message"], "配置已被其他修改，请刷新后重试");
    assert_eq!(
        state
            .config_store
            .lock()
            .unwrap()
            .current_revision()
            .unwrap(),
        1,
        "冲突时不得写库"
    );
}

#[test]
fn config_save_route_requires_base_revision() {
    let state = test_state();
    let sections = full_config_sections(&state);
    let body = serde_json::to_string(&json!({ "sections": sections })).unwrap();
    let error = config_save_route(body.as_bytes(), &state).expect_err("missing baseRevision");
    assert_eq!(error.status, 400);
    assert!(error.message.contains("baseRevision"));
}

#[test]
fn config_save_route_reports_validation_failure_without_writing() {
    let state = test_state();
    let mut sections = full_config_sections(&state);
    sections["window"]["content_width"] = json!(0);
    let body = serde_json::to_string(&json!({ "baseRevision": 1, "sections": sections })).unwrap();
    let response: Value =
        serde_json::from_str(&config_save_route(body.as_bytes(), &state).unwrap()).unwrap();
    assert_eq!(response["ok"], false);
    assert_eq!(response["code"], "config_validation_failed");
    assert_eq!(response["message"], "配置校验失败");
    assert!(
        response["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error["field"] == "window.content_width")
    );
    assert_eq!(
        state
            .config_store
            .lock()
            .unwrap()
            .current_revision()
            .unwrap(),
        1,
        "校验失败时不得写库"
    );
}

#[test]
fn config_save_route_secret_mask_submission_keeps_previous_value() {
    let state = test_state();
    // 先保存真实密钥 → revision 2。
    let mut sections = full_config_sections(&state);
    sections["ai"]["api_key"] = json!("sk-http-secret");
    let body = serde_json::to_string(&json!({ "baseRevision": 1, "sections": sections })).unwrap();
    let saved: Value =
        serde_json::from_str(&config_save_route(body.as_bytes(), &state).unwrap()).unwrap();
    assert_eq!(saved["ok"], true);
    assert_eq!(saved["revision"], 2);

    // Web 表单回传掩码重新提交 → 密钥保留 → revision 3，changedFields 不含密钥。
    let mut sections = full_config_sections(&state);
    sections["ai"]["api_key"] = json!(crate::config::SECRET_MASK);
    let body = serde_json::to_string(&json!({ "baseRevision": 2, "sections": sections })).unwrap();
    let response: Value =
        serde_json::from_str(&config_save_route(body.as_bytes(), &state).unwrap()).unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(response["revision"], 3);
    assert!(
        !response["changedFields"]
            .as_array()
            .unwrap()
            .contains(&json!("ai.api_key"))
    );
    assert_eq!(
        state
            .config_store
            .lock()
            .unwrap()
            .load_full()
            .unwrap()
            .ai
            .api_key,
        "sk-http-secret",
        "掩码提交不得覆盖已保存的密钥"
    );
}

#[test]
fn config_rollback_route_restores_previous_revision() {
    let state = test_state();
    // queue.max_size = 20 → revision 2。
    let mut sections = full_config_sections(&state);
    sections["queue"]["max_size"] = json!(20);
    let body = serde_json::to_string(&json!({ "baseRevision": 1, "sections": sections })).unwrap();
    let saved: Value =
        serde_json::from_str(&config_save_route(body.as_bytes(), &state).unwrap()).unwrap();
    assert_eq!(saved["revision"], 2);
    // queue.max_size = 30 → revision 3。
    let mut sections = full_config_sections(&state);
    sections["queue"]["max_size"] = json!(30);
    let body = serde_json::to_string(&json!({ "baseRevision": 2, "sections": sections })).unwrap();
    config_save_route(body.as_bytes(), &state).unwrap();

    // 回滚到 revision 2 → 新版本 4，queue.max_size 恢复 20。
    let body = serde_json::to_string(&json!({ "revision": 2, "baseRevision": 3 })).unwrap();
    let response: Value =
        serde_json::from_str(&config_rollback_route(body.as_bytes(), &state).unwrap()).unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(response["revision"], 4);
    assert_eq!(
        state
            .config_store
            .lock()
            .unwrap()
            .load_full()
            .unwrap()
            .queue
            .max_size,
        20,
        "回滚后必须恢复目标版本的值"
    );

    // 不存在的目标版本 → config_revision_not_found。
    let body = serde_json::to_string(&json!({ "revision": 99, "baseRevision": 4 })).unwrap();
    let response: Value =
        serde_json::from_str(&config_rollback_route(body.as_bytes(), &state).unwrap()).unwrap();
    assert_eq!(response["ok"], false);
    assert_eq!(response["code"], "config_revision_not_found");

    // 旧页面基于过期 revision 发起回滚时必须冲突，不能覆盖新配置。
    let body = serde_json::to_string(&json!({ "revision": 2, "baseRevision": 3 })).unwrap();
    let response: Value =
        serde_json::from_str(&config_rollback_route(body.as_bytes(), &state).unwrap()).unwrap();
    assert_eq!(response["ok"], false);
    assert_eq!(response["code"], "config_conflict");
}

#[test]
fn config_endpoints_require_access_token() {
    let mut state = test_state();
    let server = start_test_http_server(&mut state, "secret");
    let address = server.local_addr();

    for target in [
        "/config",
        "/config/schema",
        "/config/section?name=queue",
        "/config/revisions",
        "/config/validate",
        "/config/save",
        "/config/rollback",
    ] {
        let response = http_get(address, target, None);
        assert_eq!(
            response.status_line, "HTTP/1.1 401 Unauthorized",
            "{target}"
        );
    }
    // 携带正确 token 后可正常访问。
    let response = http_get(address, "/config", Some("secret"));
    assert_eq!(response.status_line, "HTTP/1.1 200 OK");
    let value: Value = serde_json::from_str(&response.body).expect("config JSON");
    assert_eq!(value["ok"], true);
    server.shutdown().expect("shutdown HTTP server");
}
