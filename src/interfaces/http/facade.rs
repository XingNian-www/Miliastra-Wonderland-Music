use std::sync::Arc;

use anyhow::Result;
use miliastra_playback::{
    AudioCacheStats, AudioCacheTrackStatus, CachedTrackPage, CredentialStatus, LoginSession,
    PlayableTrack, PlaybackHandle, ProviderId, TrackKey, bilibili_is_bvid,
};
use serde_json::json;
use uuid::Uuid;

use crate::adapters::login_helper::{
    LoginHelperFailure, LoginHelperManager, LoginManagerStatus, ProviderView,
};
use crate::adapters::player::PlayerRuntimeBackend;
use crate::features::administration::{
    AdministrationCommand, AdministrationMutationIntent, AdministrationMutationOutcome,
};
use crate::features::command::ModuleCommand;
use crate::features::custom_workflow::CustomWorkflowService;
use crate::features::hall::HallCommand;
use crate::features::playback::{
    MusicPlayerBackend, PlaybackCommand, PlaybackMutationIntent, PlaybackMutationOutcome,
    PlayerStatus, QueueItem,
};
use crate::features::song_request::{AiClient, SongCommand, SongSource};
use crate::features::undercover::UndercoverCommand;
use crate::interfaces::chat::ConsoleCommandIntent;
use crate::interfaces::http::{
    HttpAiPort, HttpCommandError, HttpCommandPort, HttpLoginError, HttpLoginErrorView,
    HttpLoginPort, HttpLoginQrCodeView, HttpLoginStatus, HttpPlayerPort, HttpPlayerSearchError,
    HttpProviderView, HttpTaskPort, PlayTrackRequest,
};
use crate::runtime::business::{BusinessMutationIntent, BusinessMutationOutcome};
use crate::runtime::chat_listener::ChatListenerMode;
use crate::runtime::player_io::{PlayerSearchClient, PlayerSearchClientError, SearchCandidate};
use crate::runtime::scheduler::FormalTaskEnqueueOutcome;

#[derive(Clone)]
pub(crate) struct ApplicationHttpCommandFacade {
    tasks: Arc<dyn HttpTaskPort>,
    custom_workflow: CustomWorkflowService,
    /// 调试用模拟聊天注入所需的链路句柄。
    ///
    /// 只有生产装配点会填；测试里为 None，simulate_chat 会明确报错而不是静默无效。
    simulation: Option<ChatSimulation>,
}

/// 模拟聊天注入所需的共享句柄。
///
/// 只持有下层句柄（observation / features）。「外部源是否在接管」用闭包传入，
/// 避免 interfaces 反向依赖 composition —— 那个方向由分层策略禁止。
#[derive(Clone)]
pub(crate) struct ChatSimulation {
    pub(crate) timeline: crate::observation::chat::ChatTimeline,
    pub(crate) identity: crate::features::identity::IdentityAccess,
    /// 外部源此刻是否在接管。决策窗口要靠它选快路径，模拟时也要如实反映。
    pub(crate) external_active: std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
}

impl ApplicationHttpCommandFacade {
    pub(crate) fn new(
        tasks: Arc<dyn HttpTaskPort>,
        custom_workflow: CustomWorkflowService,
    ) -> Self {
        Self {
            tasks,
            custom_workflow,
            simulation: None,
        }
    }

    /// 生产装配：额外带上模拟聊天所需的链路句柄。
    pub(crate) fn with_simulation(
        tasks: Arc<dyn HttpTaskPort>,
        custom_workflow: CustomWorkflowService,
        simulation: ChatSimulation,
    ) -> Self {
        Self {
            tasks,
            custom_workflow,
            simulation: Some(simulation),
        }
    }

    pub(crate) fn remote_control_command(
        raw: String,
        matched: &str,
        command: ModuleCommand,
    ) -> ConsoleCommandIntent {
        ConsoleCommandIntent::new(raw, matched, command)
    }

    fn enqueue_remote_command(
        &self,
        intent: ConsoleCommandIntent,
    ) -> Result<String, HttpCommandError> {
        let pending = intent.into_pending();
        let command = pending.routed.raw.clone();
        let outcome = self
            .tasks
            .enqueue_command(pending)
            .map_err(internal_command_error)?;
        let receipt = match outcome {
            FormalTaskEnqueueOutcome::Queued(receipt) => Some(receipt),
            FormalTaskEnqueueOutcome::Duplicate => None,
        };
        Ok(json!({
            "ok": true,
            "queued": receipt.is_some(),
            "duplicate": receipt.is_none(),
            "taskId": receipt.as_ref().map(|receipt| receipt.task_id),
            "position": receipt.as_ref().map_or(0, |receipt| receipt.position),
            "command": command,
        })
        .to_string())
    }

    fn required_receipt(
        outcome: Result<FormalTaskEnqueueOutcome>,
    ) -> Result<crate::runtime::scheduler::FormalTaskReceipt, HttpCommandError> {
        match outcome.map_err(internal_command_error)? {
            FormalTaskEnqueueOutcome::Queued(receipt) => Ok(receipt),
            FormalTaskEnqueueOutcome::Duplicate => Err(HttpCommandError {
                status: 409,
                message: "任务已在待执行范围内".to_string(),
            }),
        }
    }
}

impl HttpCommandPort for ApplicationHttpCommandFacade {
    /// 调试用：把一条模拟聊天消息注入观察链路，**只入队不等待**。
    ///
    /// 不阻塞是刻意的：点歌/歌单这类命令会在后台正式任务里打开决策窗口并等 20s，
    /// HTTP 请求若跟着等，接口会长时间挂住且无法观察中间状态。这里只负责注入，
    /// 命令执行与窗口等待都发生在原有线程里，符合真实链路。
    fn simulate_chat(&self, role: String, text: String) -> Result<String, HttpCommandError> {
        use miliastra_kernel::chat_source::{
            ChatChannel, ExternalChatMessage, ExternalSender, GameUid,
        };

        let Some(simulation) = self.simulation.as_ref() else {
            return Err(HttpCommandError::internal(
                "模拟聊天未装配（仅生产运行时可用）",
            ));
        };

        let text = text.trim().to_string();
        if text.is_empty() {
            return Err(HttpCommandError {
                status: 400,
                message: "text 不能为空".to_string(),
            });
        }

        // 每种角色一个固定假身份。用 __sim_ 前缀避免与真实玩家昵称/UID 撞车。
        let (nickname, uid, identity_role, private) = match role.as_str() {
            // 大厅普通成员：没有身份映射，权限落到 User。
            "user" => ("__sim_user__", 990_001u32, None, false),
            "friend" => (
                "__sim_friend__",
                990_002,
                Some(crate::features::identity::IdentityRole::Friend),
                false,
            ),
            "admin" => (
                "__sim_admin__",
                990_003,
                Some(crate::features::identity::IdentityRole::Admin),
                false,
            ),
            "owner" => (
                "__sim_owner__",
                990_004,
                Some(crate::features::identity::IdentityRole::Owner),
                false,
            ),
            // 私聊走的也是同一套身份判定，只是容器不同。
            "friend_private" => (
                "__sim_friend__",
                990_002,
                Some(crate::features::identity::IdentityRole::Friend),
                true,
            ),
            "admin_private" => (
                "__sim_admin__",
                990_003,
                Some(crate::features::identity::IdentityRole::Admin),
                true,
            ),
            other => {
                return Err(HttpCommandError {
                    status: 400,
                    message: format!(
                        "未知角色 {other:?}；可用：user / friend / admin / owner / friend_private / admin_private"
                    ),
                });
            }
        };

        // 幂等登记假身份。配置重载会用库里的值覆盖 IdentityAccess，
        // 所以每次调用都要补一次——只碰 __sim_ 保留名，不影响真实映射。
        if let Some(role) = identity_role {
            simulation
                .identity
                .ensure_simulation_mapping(nickname, uid, role);
        }

        let channel = if private {
            ChatChannel::new(ChatChannel::PRIVATE)
        } else {
            ChatChannel::current_hall()
        };

        // 序号只需在同一来源内单调，用于去重与命令锁锚点。
        static SIM_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let sequence = SIM_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let message = ExternalChatMessage {
            sequence,
            sent_at: None,
            channel,
            hall_id: None,
            sender: ExternalSender::new(Some(nickname.to_string()), Some(GameUid::from_u32(uid))),
            // 私聊必须有接收者，否则 scope 推导失败、消息会被整条丢弃。
            to_uid: private.then(|| GameUid::from_u32(990_999)),
            text: text.clone(),
        };

        let accepted = simulation
            .timeline
            .inject_external(
                std::time::Instant::now(),
                crate::observation::chat::VisualSessionId::new(u64::MAX),
                "simulate",
                // 调试注入不带本机账号语义，不需要排除 self。
                None,
                vec![message],
            )
            .map_err(internal_command_error)?;

        Ok(json!({
            "ok": true,
            "injected": accepted.len(),
            "role": role,
            "nickname": nickname,
            "uid": uid,
            "channel": if private { "private" } else { "current_hall" },
            "text": text,
            "note": "已注入观察链路；命令在后台任务里执行，本请求不等待决策窗口",
        })
        .to_string())
    }
    fn playback_control(&self, command: PlaybackCommand) -> Result<String, HttpCommandError> {
        let (raw, matched) = match &command {
            PlaybackCommand::Resume => ("继续".to_string(), "继续"),
            PlaybackCommand::Pause => ("暂停".to_string(), "暂停"),
            PlaybackCommand::Next => ("下一首".to_string(), "下一首"),
            PlaybackCommand::Previous => ("上一首".to_string(), "上一首"),
            PlaybackCommand::Volume(volume) => (format!("音量 {volume}"), "音量"),
            PlaybackCommand::Lyrics => ("歌词".to_string(), "歌词"),
            _ => return Err(HttpCommandError::internal("HTTP 不支持该播放控制命令")),
        };
        self.enqueue_remote_command(Self::remote_control_command(
            raw,
            matched,
            ModuleCommand::Playback(command),
        ))
    }

    fn startup_task(&self, kind: String) -> Result<String, HttpCommandError> {
        use crate::features::startup::{StartupSource, StartupTask};
        let task = match kind.as_str() {
            "start_game" => StartupTask::start_game(StartupSource::DEBUG_SANDBOX),
            "enter_wonderland" => StartupTask::enter_wonderland(StartupSource::DEBUG_SANDBOX),
            _ => {
                return Err(HttpCommandError::bad_request(
                    "启动类型必须是 start_game 或 enter_wonderland",
                ));
            }
        };
        let receipt = Self::required_receipt(self.tasks.enqueue_startup(task))?;
        Ok(json!({
            "ok": true,
            "queued": true,
            "taskId": receipt.task_id,
            "position": receipt.position,
        })
        .to_string())
    }

    fn chat_send(&self, text: String, prefix: String) -> Result<String, HttpCommandError> {
        let receipt = Self::required_receipt(self.tasks.enqueue_console_chat(text, prefix))?;
        Ok(json!({
            "ok": true,
            "queued": true,
            "taskId": receipt.task_id,
            "position": receipt.position,
        })
        .to_string())
    }

    fn hall_control(&self, command: HallCommand) -> Result<String, HttpCommandError> {
        let (raw, matched) = match &command {
            HallCommand::Detect => ("大厅检测".to_string(), "大厅检测"),
            HallCommand::Time => ("大厅时间".to_string(), "大厅时间"),
            HallCommand::ToggleMicrophone { .. } => ("麦克风".to_string(), "麦克风"),
        };
        self.enqueue_remote_command(Self::remote_control_command(
            raw,
            matched,
            ModuleCommand::Hall(command),
        ))
    }

    fn undercover_control(&self, start: bool) -> Result<String, HttpCommandError> {
        let (raw, command) = if start {
            ("卧底开局", UndercoverCommand::Start)
        } else {
            ("卧底结束", UndercoverCommand::End)
        };
        self.enqueue_remote_command(Self::remote_control_command(
            raw.to_string(),
            "卧底",
            ModuleCommand::Undercover(command),
        ))
    }

    fn remote_song(
        &self,
        keyword: String,
        source: String,
        prefer_accompaniment: bool,
        ai_assisted: bool,
    ) -> Result<String, HttpCommandError> {
        let contains_accompaniment = keyword.contains("伴奏");
        let keyword = keyword
            .replace("伴奏", " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if keyword.trim().is_empty() {
            return Err(HttpCommandError::bad_request("缺少keyword参数"));
        }
        let prefer_accompaniment = prefer_accompaniment || contains_accompaniment;
        let (prefix, song_source) = if ai_assisted {
            ("AI点歌", SongSource::All)
        } else {
            match source.as_str() {
                "qqmusic" => ("点歌", SongSource::QqMusic),
                "netease" => ("网易点歌", SongSource::Netease),
                "bilibili" => ("B站点歌", SongSource::Bilibili),
                "kugou" => ("酷狗点歌", SongSource::Kugou),
                _ => {
                    return Err(HttpCommandError::bad_request(
                        "远程点歌source只允许qqmusic、netease、bilibili或kugou",
                    ));
                }
            }
        };
        let raw = if prefer_accompaniment {
            format!("{prefix} {keyword} 伴奏")
        } else {
            format!("{prefix} {keyword}")
        };
        self.enqueue_remote_command(ConsoleCommandIntent::new(
            raw,
            prefix,
            ModuleCommand::SongRequest(SongCommand {
                keyword,
                source: song_source,
                prefix: prefix.to_string(),
                prefer_accompaniment,
                ai_assisted,
                friend_username: String::new(),
            }),
        ))
    }

    fn play_track(&self, request: PlayTrackRequest) -> Result<String, HttpCommandError> {
        let requester = if request.requester.is_empty() {
            "WEB/API".to_owned()
        } else {
            request.requester
        };
        let track = PlayableTrack {
            track_ref: request.track_ref,
            metadata: request.metadata,
        };
        validate_playable_track(&track)?;
        let current_uri = track.track_ref.key.to_string();
        let item = QueueItem {
            id: 0,
            keyword: track.metadata.title.clone(),
            source: track.track_ref.key.provider.to_string(),
            prefer_accompaniment: false,
            ai_original_text: String::new(),
            track: Some(track.clone()),
            friend_username: String::new(),
            requester,
            // 与聊天路径一致：HTTP 默认不绕过去重；协议无权限/参数，不提供显式 bypass。
            dedup_bypass: false,
            candidate_snapshot: Vec::new(),
            playlist_auto: false,
        };
        // 入队前复用播放队列统一去重策略（结构化/待解析交叉形态同样生效）。
        if self
            .tasks
            .playback_queue_contains(item.clone())
            .map_err(internal_command_error)?
        {
            return Err(HttpCommandError {
                status: 409,
                message: format!("队列已有: {}", track.metadata.title),
            });
        }
        let BusinessMutationOutcome::Playback(PlaybackMutationOutcome::Pushed(pushed)) = self
            .tasks
            .apply_mutation(BusinessMutationIntent::Playback(
                PlaybackMutationIntent::Push(Box::new(item)),
            ))
            .map_err(internal_command_error)?
        else {
            unreachable!("playback queue push intent returned a different outcome")
        };
        if !pushed.accepted {
            return Err(HttpCommandError::bad_request("音乐播放队列已满"));
        }
        Ok(json!({
            "ok": true,
            "queued": true,
            "size": pushed.size,
            "currentUri": current_uri,
            "track": track,
        })
        .to_string())
    }

    fn request_chat_listener_mode(
        &self,
        mode: ChatListenerMode,
    ) -> Result<String, HttpCommandError> {
        let BusinessMutationOutcome::Administration(
            AdministrationMutationOutcome::ChatListenerModeRequested { queued, snapshot },
        ) = self
            .tasks
            .apply_mutation(BusinessMutationIntent::Administration(
                AdministrationMutationIntent::RequestChatListenerMode(mode),
            ))
            .map_err(internal_command_error)?
        else {
            unreachable!("chat listener mode intent returned a different outcome")
        };
        if !queued {
            return Ok(json!({
                "ok": true,
                "queued": false,
                "mode": snapshot.mode,
                "pendingMode": snapshot.pending_mode,
            })
            .to_string());
        }
        let receipt = match Self::required_receipt(self.tasks.enqueue_listener_mode(mode)) {
            Ok(receipt) => receipt,
            Err(error) => {
                let _ = self
                    .tasks
                    .apply_mutation(BusinessMutationIntent::Administration(
                        AdministrationMutationIntent::CancelChatListenerModeRequest(mode),
                    ));
                return Err(error);
            }
        };
        Ok(json!({
            "ok": true,
            "queued": true,
            "taskId": receipt.task_id,
            "position": receipt.position,
            "mode": snapshot.mode,
            "pendingMode": mode,
        })
        .to_string())
    }

    fn set_operator_commands(&self, enabled: bool) -> Result<String, HttpCommandError> {
        let raw = if enabled { "启用" } else { "禁用" }.to_string();
        self.enqueue_remote_command(Self::remote_control_command(
            raw.clone(),
            &raw,
            ModuleCommand::Administration(AdministrationCommand::SetCommandsEnabled {
                enabled,
                username: "控制台".to_string(),
            }),
        ))
    }

    fn set_idle_exit(&self, minutes: u32) -> Result<String, HttpCommandError> {
        self.enqueue_remote_command(Self::remote_control_command(
            format!("闲置退出 {minutes}"),
            "闲置退出",
            ModuleCommand::Administration(AdministrationCommand::IdleExit { minutes }),
        ))
    }

    fn clear_idle_exit(&self) -> Result<String, HttpCommandError> {
        let receipt = Self::required_receipt(self.tasks.enqueue_clear_idle_exit())?;
        Ok(json!({
            "ok": true,
            "queued": true,
            "taskId": receipt.task_id,
            "position": receipt.position,
            "command": "取消闲置退出"
        })
        .to_string())
    }

    fn list_workflows(&self) -> Result<String, HttpCommandError> {
        let workflows = self
            .custom_workflow
            .list()
            .into_iter()
            .map(|workflow| {
                json!({
                    "name": workflow.name,
                    "commands": workflow.commands,
                    "allowArgs": workflow.allow_args,
                    "confirmBeforeRun": workflow.confirm_before_run,
                })
            })
            .collect::<Vec<_>>();
        serde_json::to_string(&workflows).map_err(internal_command_error)
    }

    fn run_workflow(&self, name: String, args: String) -> Result<String, HttpCommandError> {
        let prepared = self
            .custom_workflow
            .prepare_remote(&name, &args)
            .map_err(|error| HttpCommandError::bad_request(error.to_string()))?;
        self.enqueue_remote_command(Self::remote_control_command(
            prepared.raw,
            &prepared.matched,
            ModuleCommand::CustomWorkflow(prepared.command),
        ))
    }
}

fn internal_command_error(error: impl std::fmt::Display) -> HttpCommandError {
    HttpCommandError::internal(error.to_string())
}

fn validate_playable_track(track: &PlayableTrack) -> Result<(), HttpCommandError> {
    const MAX_ID_BYTES: usize = 512;
    const MAX_TEXT_BYTES: usize = 1024;
    const MAX_ARTISTS: usize = 32;
    const MAX_DURATION_MS: u64 = 24 * 60 * 60 * 1000;

    let id = track.track_ref.key.id.as_str();
    if id.trim().is_empty()
        || id != id.trim()
        || id.contains('/')
        || id.len() > MAX_ID_BYTES
        || id.chars().any(char::is_control)
    {
        return Err(HttpCommandError::bad_request("trackRef.key.id字段无效"));
    }
    validate_track_text(
        &track.metadata.title,
        "metadata.title",
        true,
        MAX_TEXT_BYTES,
    )?;
    if track.metadata.artists.len() > MAX_ARTISTS {
        return Err(HttpCommandError::bad_request("metadata.artists数量过多"));
    }
    for artist in &track.metadata.artists {
        validate_track_text(artist, "metadata.artists", true, MAX_TEXT_BYTES)?;
    }
    if let Some(album) = track.metadata.album.as_deref() {
        validate_track_text(album, "metadata.album", false, MAX_TEXT_BYTES)?;
    }
    if track
        .metadata
        .duration_ms
        .is_some_and(|duration| duration == 0 || duration > MAX_DURATION_MS)
    {
        return Err(HttpCommandError::bad_request("metadata.durationMs字段无效"));
    }
    validate_track_identity(
        track.track_ref.key.provider,
        id,
        track
            .track_ref
            .resolver_locator
            .as_ref()
            .map(|locator| locator.as_str()),
    )
}

fn validate_track_text(
    value: &str,
    field: &str,
    required: bool,
    max_bytes: usize,
) -> Result<(), HttpCommandError> {
    if (required && value.trim().is_empty())
        || value != value.trim()
        || value.len() > max_bytes
        || value.chars().any(char::is_control)
    {
        return Err(HttpCommandError::bad_request(format!("{field}字段无效")));
    }
    Ok(())
}

fn validate_track_identity(
    provider: ProviderId,
    track_id: &str,
    locator: Option<&str>,
) -> Result<(), HttpCommandError> {
    match provider {
        ProviderId::QqMusic => {
            if !is_ascii_alphanumeric(track_id) {
                return Err(HttpCommandError::bad_request(
                    "trackRef.key.id不是有效的QQ音乐MID",
                ));
            }
            let Some(locator) = locator else {
                return Ok(());
            };
            let Some(value) = locator.strip_prefix("qqmusic:v2:") else {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            };
            let Some((locator_track, media_mid)) = value.split_once(':') else {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            };
            if !is_ascii_alphanumeric(locator_track) || !is_ascii_alphanumeric(media_mid) {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            }
            validate_locator_track(track_id, locator_track)
        }
        ProviderId::Netease => {
            if !track_id.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(HttpCommandError::bad_request(
                    "trackRef.key.id不是有效的网易云歌曲ID",
                ));
            }
            let Some(locator) = locator else {
                return Ok(());
            };
            let Some(locator_track) = locator.strip_prefix("netease:v1:") else {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            };
            if !locator_track.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            }
            validate_locator_track(track_id, locator_track)
        }
        ProviderId::Bilibili => {
            if !is_bilibili_bvid(track_id) {
                return Err(HttpCommandError::bad_request(
                    "trackRef.key.id不是有效的B站BV号",
                ));
            }
            let Some(locator) = locator else {
                return Ok(());
            };
            let Some(locator_track) = locator.strip_prefix("bilibili:v1:") else {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            };
            if !is_bilibili_bvid(locator_track) {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            }
            validate_locator_track(track_id, locator_track)
        }
        ProviderId::Kugou => {
            if track_id.is_empty()
                || !track_id
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
            {
                return Err(HttpCommandError::bad_request(
                    "trackRef.key.id不是有效的酷狗歌曲Hash",
                ));
            }
            let Some(locator) = locator else {
                return Ok(());
            };
            let mut parts = locator.split(':');
            if parts.next() != Some("kugou") {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            }
            let Some(locator_track) = parts.next() else {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            };
            let Some(album_id) = parts.next() else {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            };
            let Some(album_audio_id) = parts.next() else {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            };
            if parts.next().is_some()
                || !locator_track
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
                || !album_id.bytes().all(|byte| byte.is_ascii_digit())
                || !album_audio_id.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            }
            validate_locator_track(track_id, locator_track)
        }
    }
}

fn validate_locator_track(track_id: &str, locator_track: &str) -> Result<(), HttpCommandError> {
    if !locator_track.eq_ignore_ascii_case(track_id) {
        return Err(HttpCommandError::bad_request(
            "resolverLocator曲目与trackRef不一致",
        ));
    }
    Ok(())
}

fn is_ascii_alphanumeric(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn is_bilibili_bvid(value: &str) -> bool {
    bilibili_is_bvid(value)
}

#[derive(Clone)]
pub(crate) struct ApplicationHttpPlayerFacade {
    backend: PlayerRuntimeBackend,
    search: PlayerSearchClient,
    playback: PlaybackHandle,
    /// 与 PlaybackApplication 共享的播放模式句柄(0=顺序 1=单曲循环 2=随机)。
    play_mode: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

impl ApplicationHttpPlayerFacade {
    pub(crate) fn new(
        backend: PlayerRuntimeBackend,
        search: PlayerSearchClient,
        playback: PlaybackHandle,
        play_mode: std::sync::Arc<std::sync::atomic::AtomicU8>,
    ) -> Self {
        Self {
            backend,
            search,
            playback,
            play_mode,
        }
    }
}

impl HttpPlayerPort for ApplicationHttpPlayerFacade {
    fn status(&self) -> Result<PlayerStatus> {
        self.backend.status()
    }

    fn cache_stats(
        &self,
        keys: &[TrackKey],
    ) -> Result<(Option<AudioCacheStats>, Vec<AudioCacheTrackStatus>)> {
        Ok(self.playback.cache_stats(keys)?)
    }

    fn cached_tracks(
        &self,
        offset: usize,
        limit: usize,
        sort: miliastra_playback::CacheTrackSortKey,
        ascending: bool,
    ) -> Result<CachedTrackPage> {
        Ok(self
            .playback
            .cached_tracks(offset, limit, sort, ascending)?)
    }

    fn reset_track_statistics(&self, key: &TrackKey) -> Result<bool> {
        Ok(self.playback.reset_track_statistics(key)?)
    }

    fn invalidate_track_cache(&self, key: &TrackKey) -> Result<bool> {
        self.playback
            .invalidate_audio_cache(key)
            .map(|()| true)
            .map_err(anyhow::Error::from)
    }

    fn seek(&self, position_seconds: f64) -> Result<()> {
        self.playback
            .seek(position_seconds)
            .map_err(anyhow::Error::from)
    }

    fn play_mode(&self) -> Result<u8> {
        Ok(self.play_mode.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn set_play_mode(&self, mode: u8) -> Result<()> {
        if mode > 2 {
            anyhow::bail!("播放模式只允许 0(顺序)/1(单曲循环)/2(随机)");
        }
        self.play_mode
            .store(mode, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn search_text(&self, keyword: &str, source: &str) -> Result<String, HttpPlayerSearchError> {
        self.search
            .search_text(keyword, source)
            .map_err(player_search_error)
    }

    fn search_candidates(
        &self,
        keyword: &str,
        source: &str,
    ) -> Result<Vec<SearchCandidate>, HttpPlayerSearchError> {
        self.search
            .search_candidates(keyword, source)
            .map_err(player_search_error)
    }
}

fn player_search_error(error: PlayerSearchClientError) -> HttpPlayerSearchError {
    let message = match error {
        PlayerSearchClientError::Failed(source) => source.to_string(),
        error => error.to_string(),
    };
    HttpPlayerSearchError::new(message)
}

#[derive(Clone)]
pub(crate) struct ApplicationHttpLoginFacade {
    manager: LoginHelperManager,
}

impl ApplicationHttpLoginFacade {
    pub(crate) fn new(manager: LoginHelperManager) -> Self {
        Self { manager }
    }
}

impl HttpLoginPort for ApplicationHttpLoginFacade {
    fn providers(&self) -> Result<Vec<HttpProviderView>, HttpLoginError> {
        self.manager
            .providers()
            .map(|providers| providers.into_iter().map(provider_view).collect())
            .map_err(login_error)
    }

    fn status(&self) -> HttpLoginStatus {
        login_status(self.manager.status())
    }

    fn start(&self, provider: ProviderId) -> Result<LoginSession, HttpLoginError> {
        self.manager.start(provider).map_err(login_error)
    }

    fn cancel(&self, session_id: Uuid) -> Result<(), HttpLoginError> {
        self.manager.cancel(session_id).map_err(login_error)
    }

    fn logout(&self, provider: ProviderId) -> Result<CredentialStatus, HttpLoginError> {
        self.manager.logout(provider).map_err(login_error)
    }

    fn refresh(&self, provider: ProviderId) -> Result<CredentialStatus, HttpLoginError> {
        self.manager
            .refresh_credential(provider)
            .map_err(login_error)
    }

    fn kugou_status(&self) -> Result<miliastra_playback::KugouAccountStatus, HttpLoginError> {
        self.manager.kugou_status().map_err(login_error)
    }

    fn account_status(
        &self,
        provider: ProviderId,
    ) -> Result<Option<miliastra_playback::ProviderAccountStatus>, HttpLoginError> {
        self.manager.account_status(provider).map_err(login_error)
    }

    fn refresh_account_status(
        &self,
        provider: ProviderId,
    ) -> Result<Option<miliastra_playback::ProviderAccountStatus>, HttpLoginError> {
        self.manager
            .refresh_account_status(provider)
            .map_err(login_error)
    }

    fn kugou_claim_vip(&self) -> Result<miliastra_playback::KugouListenReport, HttpLoginError> {
        self.manager.kugou_claim_vip().map_err(login_error)
    }

    fn kugou_upgrade_vip(&self) -> Result<miliastra_playback::KugouListenReport, HttpLoginError> {
        self.manager.kugou_upgrade_vip().map_err(login_error)
    }
}

fn provider_view(view: ProviderView) -> HttpProviderView {
    HttpProviderView {
        provider: view.provider,
        configured: view.configured,
        fields: view.fields,
        refresh_supported: view.refresh_supported,
        manual_refresh_supported: view.manual_refresh_supported,
        refresh_ready: view.refresh_ready,
        refresh_state: view.refresh_state,
        last_refresh_at_ms: view.last_refresh_at_ms,
        next_refresh_check_at_ms: view.next_refresh_check_at_ms,
        last_refresh_error: view.last_refresh_error,
    }
}

fn login_status(status: LoginManagerStatus) -> HttpLoginStatus {
    HttpLoginStatus {
        active: status.active,
        session_id: status.session_id,
        provider: status.provider,
        qr_code: status.qr_code.map(|qr_code| HttpLoginQrCodeView {
            image_data_url: qr_code.image_data_url,
            received_at_ms: qr_code.received_at_ms,
        }),
        last_error: status.last_error.map(|error| HttpLoginErrorView {
            code: error.code,
            message: error.message,
            provider: error.provider,
        }),
    }
}

fn login_error(error: LoginHelperFailure) -> HttpLoginError {
    HttpLoginError {
        code: error.code,
        message: error.message,
    }
}

pub(crate) struct ApplicationHttpDebugFacade {
    sandbox: crate::debug_http_service::DebugHttpService,
    runtime: crate::runtime::ui::UiRuntimeHandle,
    operation: std::sync::Mutex<()>,
}
impl ApplicationHttpDebugFacade {
    pub(crate) fn new(
        runtime: crate::runtime::ui::UiRuntimeHandle,
        ocr: crate::runtime::ocr::OcrRuntimeHandle,
        database_path: &std::path::Path,
        playback_queue: crate::debug_sandbox_runner::PlaybackQueueLengthReader,
        playback_queue_clear: crate::debug_sandbox_runner::PlaybackQueueClearer,
        playback_queue_remove_front: crate::debug_sandbox_runner::PlaybackQueueClearer,
        task_queue_canceller: crate::debug_sandbox_runner::TaskQueueCanceller,
        command_port: Arc<dyn crate::interfaces::http::HttpCommandPort>,
    ) -> Result<Self> {
        // 播放控制、点歌与聊天发送共用同一个命令端口对象，
        // 但各自持有一份 Arc，互不占用。
        let playback_port = Arc::clone(&command_port);
        let chat_port = Arc::clone(&command_port);
        let startup_port = Arc::clone(&command_port);
        Ok(Self {
            runtime: runtime.clone(),
            sandbox: crate::debug_http_service::DebugHttpService::with_journal(
                Some(runtime),
                database_path,
            )
            .map_err(anyhow::Error::msg)?
            .with_playback_queue_reader(playback_queue)
            .with_playback_queue_clearer(playback_queue_clear)
            .with_playback_queue_remove_front(playback_queue_remove_front)
            .with_task_queue_canceller(task_queue_canceller)
            .with_playback_controller(Arc::new(move |action| {
                use crate::debug_sandbox_runner::PlaybackAction;
                let command = match action {
                    PlaybackAction::Play => crate::features::playback::PlaybackCommand::Resume,
                    PlaybackAction::Pause => crate::features::playback::PlaybackCommand::Pause,
                    PlaybackAction::SkipNext => crate::features::playback::PlaybackCommand::Next,
                    PlaybackAction::SkipPrevious => {
                        crate::features::playback::PlaybackCommand::Previous
                    }
                    PlaybackAction::SetVolume { volume } => {
                        crate::features::playback::PlaybackCommand::Volume(volume.to_string())
                    }
                };
                // 入队成功才算这一步成功；真正的播放效果由断言层验证，不把"已下发"当"已生效"。
                playback_port
                    .playback_control(command)
                    .map(|_| ())
                    .map_err(|error| error.message)
            }))
            .with_song_requester(Arc::new(move |request| {
                // 空音源沿用应用自己的默认值，与 HTTP /queue/add 的行为保持一致。
                let source = if request.source.trim().is_empty() {
                    "qqmusic".to_string()
                } else {
                    request.source.clone()
                };
                command_port
                    .remote_song(
                        request.keyword.clone(),
                        source,
                        request.prefer_accompaniment,
                        // ai_assisted=false：沙盒提交的是调用方明确给出的请求，
                        // 不冒充 AI 辅助点歌，免得污染点歌人归属与统计口径。
                        false,
                    )
                    .map(|_| ())
                    .map_err(|error| error.message)
            }))
            .with_chat_sender(Arc::new(move |request| {
                // 前缀由调用方决定：沙盒不替应用改文案，只负责把请求送进入队路径。
                chat_port
                    .chat_send(request.text.clone(), request.prefix.clone())
                    .map(|_| ())
                    .map_err(|error| error.message)
            }))
            .with_startup_launcher(Arc::new(move |request| {
                startup_port
                    .startup_task(request.kind.clone())
                    .map(|_| ())
                    .map_err(|error| error.message)
            }))
            .with_ocr(Arc::new(move |image| {
                ocr.merged_text(
                    image.clone(),
                    2,
                    crate::runtime::ocr::OcrPriority::Diagnostic,
                )
                .map_err(|e| e.to_string())
            })),
            operation: std::sync::Mutex::new(()),
        })
    }
}
/// 运行时界面状态枚举 → 稳定的线上名称。
///
/// 这五个就是当前**全部**可判别的界面状态；`unknown` 是显式的"判不出来"，
/// 不是"其它"。新的面板状态必须在这里先被命名，才能被查询与断言。
fn ui_state_kind_name(kind: crate::runtime::ui::UiStateKind) -> &'static str {
    kind.name()
}

/// 把界面分类观测转成可查询的 JSON：**状态 + 判别证据**。
///
/// 证据（模板名、搜索区域、最佳得分、阈值、命中区域、最终规则）必须一起给出，
/// 否则"为什么判成这个状态"无法被独立复核，判别依据也就无法被验证。
fn ui_state_detail_json(observation: &crate::runtime::ui::UiStateObservation) -> serde_json::Value {
    use crate::runtime::ui::UiStateObservation;
    let rect_json = |rect: crate::runtime::ui::UiEvidenceRect| {
        serde_json::json!({
            "x": rect.x(),
            "y": rect.y(),
            "width": rect.width(),
            "height": rect.height(),
        })
    };
    match observation {
        UiStateObservation::Classified(state) => {
            let classification = state.classification();
            let evidence = classification.evidence();
            let probes: Vec<serde_json::Value> = evidence
                .template_probes()
                .iter()
                .map(|probe| {
                    serde_json::json!({
                        "template": probe.template(),
                        "searchRect": rect_json(probe.search_rect()),
                        "bestScore": probe.best_score(),
                        "threshold": probe.threshold(),
                        "hitRect": probe.hit_rect().map(rect_json),
                        "outcome": probe.outcome(),
                    })
                })
                .collect();
            let marker = evidence.marker_probe().map(|marker| {
                let (width, height) = marker.coordinate_size();
                serde_json::json!({
                    "searchRect": rect_json(marker.search_rect()),
                    "coordinateWidth": width,
                    "coordinateHeight": height,
                    "blue": marker.blue_count(),
                    "yellow": marker.yellow_count(),
                    "pink": marker.pink_count(),
                })
            });
            // 世界判据（提瓦特 vs 千星）：正交于界面状态的一维。
            // 字段名与离线 /debug/ui-state 逐字一致，两条路径可直接对比。
            let world = evidence.world_probe().map(|world| {
                serde_json::json!({
                    "region": rect_json(world.region()),
                    "count": world.count(),
                    "teyvatMax": world.teyvat_max(),
                    "qianxingMin": world.qianxing_min(),
                    "guess": world.guess(),
                })
            });
            serde_json::json!({
                "ok": true,
                "classified": true,
                "kind": ui_state_kind_name(classification.kind()),
                "label": classification.label(),
                "stable": state.stable_kind().is_some(),
                "stableKind": state.stable_kind().map(ui_state_kind_name),
                "lastStableKind": state.last_stable_kind().map(ui_state_kind_name),
                "frameId": state.frame_id(),
                "evidence": {
                    "finalRule": evidence.final_rule(),
                    "templateProbes": probes,
                    "marker": marker,
                    "worldProbe": world,
                },
            })
        }
        UiStateObservation::Failed {
            last_stable_kind, ..
        } => serde_json::json!({
            "ok": true,
            "classified": false,
            "diagnostic": observation.diagnostic(),
            "lastStableKind": last_stable_kind.map(ui_state_kind_name),
        }),
    }
}

/// 只读界面状态详情：在 UI worker 上读取最新分类，**不伪造状态**。
struct UiStateDetailRoutine;
impl crate::runtime::ui::sealed::UiRoutineSealed for UiStateDetailRoutine {}
impl crate::runtime::ui::UiRoutine for UiStateDetailRoutine {
    type Output = anyhow::Result<serde_json::Value>;
    fn execute(self, context: &mut crate::runtime::ui::UiRoutineContext<'_>) -> Self::Output {
        let Some(observation) = context.latest_ui_state() else {
            return Ok(serde_json::json!({
                "ok": true,
                "classified": false,
                "reason": "尚无界面分类观测",
            }));
        };
        Ok(ui_state_detail_json(&observation))
    }
}

struct DebugAtomicRoutine(crate::debug_sandbox::AtomicCommand);
impl crate::runtime::ui::sealed::UiRoutineSealed for DebugAtomicRoutine {}
impl crate::runtime::ui::UiRoutine for DebugAtomicRoutine {
    type Output = Result<Arc<image::DynamicImage>>;
    fn execute(self, context: &mut crate::runtime::ui::UiRoutineContext<'_>) -> Self::Output {
        use crate::debug_sandbox::AtomicCommand::*;
        if crate::debug_read_only::observer().enabled() && !self.0.is_read_only() {
            anyhow::bail!("只读模式禁止调试输入");
        }
        match self.0 {
            EnsureWindow => context.device().ensure_window()?,
            EnsureForeground => context.device().ensure_foreground()?,
            EnsureReady { after_activate_ms } => {
                context.device().ensure_ready(after_activate_ms)?
            }
            Activate { after_activate_ms } => context.device().activate(after_activate_ms)?,
            Focus { after_activate_ms } => context.device().focus(after_activate_ms)?,
            InputText { text, settle_ms } => context.device().input_text(&text, settle_ms)?,
            CloseWindow => context.device().close_window()?,
            Click { x, y } => context.device().click_point(x, y)?,
            #[cfg(target_os = "windows")]
            PressKey { key } => context
                .device()
                .press_key(crate::adapters::windows::parse_key(&key)?)?,
            #[cfg(not(target_os = "windows"))]
            PressKey { .. } => anyhow::bail!("按键输入需要 Windows 设备适配层"),
            #[cfg(target_os = "windows")]
            KeyDown { key } => context
                .device()
                .key_down(crate::adapters::windows::parse_key(&key)?)?,
            #[cfg(not(target_os = "windows"))]
            KeyDown { .. } => anyhow::bail!("按键输入需要 Windows 设备适配层"),
            #[cfg(target_os = "windows")]
            KeyUp { key } => context
                .device()
                .key_up(crate::adapters::windows::parse_key(&key)?)?,
            #[cfg(not(target_os = "windows"))]
            KeyUp { .. } => anyhow::bail!("按键输入需要 Windows 设备适配层"),
            MouseDown { button } => context.device().mouse_down(match button.as_str() {
                "left" => enigo::Button::Left,
                "right" => enigo::Button::Right,
                "middle" => enigo::Button::Middle,
                _ => anyhow::bail!("invalid mouse button"),
            })?,
            MouseUp { button } => context.device().mouse_up(match button.as_str() {
                "left" => enigo::Button::Left,
                "right" => enigo::Button::Right,
                "middle" => enigo::Button::Middle,
                _ => anyhow::bail!("invalid mouse button"),
            })?,
            Scroll { x, y, amount } => context.device().scroll_point(x, y, amount)?,
            Drag {
                from_x,
                from_y,
                to_x,
                to_y,
            } => context.device().drag_point(from_x, from_y, to_x, to_y)?,
            Paste { text } => context.device().paste_text(&text, 500)?,
            Wait { milliseconds } => {
                context.wait_cancellable(std::time::Duration::from_millis(milliseconds))?
            }
            PlaybackQueueClear
            | PlaybackQueueRemoveFront
            | PlaybackPlay
            | PlaybackPause
            | PlaybackSkipNext
            | PlaybackSkipPrevious
            | PlaybackSetVolume { .. }
            | RequestSong { .. }
            | ChatSend { .. }
            | StartupTask { .. }
            | TaskQueueCancel { .. } => {
                anyhow::bail!("业务命令不得提交到 UI runtime")
            }
            Capture => {}
        }
        Ok(Arc::new(context.capture_frame()?.image().clone()))
    }
}
impl crate::interfaces::http::HttpDebugPort for ApplicationHttpDebugFacade {
    fn sandbox(&self) -> &crate::debug_http_service::DebugHttpService {
        &self.sandbox
    }
    fn enabled(&self) -> bool {
        crate::debug_mode::enabled()
    }
    fn set_enabled(&self, enabled: bool) -> Result<(), HttpCommandError> {
        // 完全控制与只读观察互斥。
        //
        // 两个开关原本各自独立：先开只读 observer、再开完全控制，会进入混合态，
        // 而 execute_direct 优先选 ReadOnlyExecutor，于是「完全控制」下所有输入
        // 命令都被「只读执行器拒绝输入或业务命令」挡掉——接口语义被推翻，
        // 使用者只会看到莫名其妙的全量拒绝。
        // 开启完全控制时主动关掉观察开关。
        if enabled && crate::debug_read_only::observer().enabled() {
            crate::debug_read_only::observer()
                .set_enabled_state(false, crate::debug_read_only::ReadOnlyState::Active);
        }
        crate::debug_mode::set_enabled(enabled);
        Ok(())
    }
    fn window_available(&self) -> Result<bool, HttpCommandError> {
        if !self.enabled() {
            return Err(HttpCommandError::bad_request("请先启用调试模式"));
        }
        self.runtime
            .submit_external(crate::debug_sandbox_runner::LiveAvailabilityRoutine)
            .map_err(|e| HttpCommandError::internal(format!("{e:#}")))?
            .wait_timeout(std::time::Duration::from_secs(10))
            .map_err(|e| HttpCommandError::internal(format!("{e:#}")))?
            .map_err(|e| HttpCommandError::internal(format!("{e:#}")))
    }
    fn ui_state_detail(&self) -> Result<String, HttpCommandError> {
        if !self.enabled() {
            return Err(HttpCommandError::bad_request("请先启用调试模式"));
        }
        let detail = self
            .runtime
            .submit_external(UiStateDetailRoutine)
            .map_err(|e| HttpCommandError::internal(format!("{e:#}")))?
            .wait_timeout(std::time::Duration::from_secs(10))
            .map_err(|e| HttpCommandError::internal(format!("{e:#}")))?
            .map_err(|e| HttpCommandError::internal(format!("{e:#}")))?;
        Ok(detail.to_string())
    }

    fn capture(&self) -> Result<Arc<image::DynamicImage>, HttpCommandError> {
        if !self.enabled() {
            return Err(HttpCommandError::bad_request("请先启用调试模式"));
        }
        let _g = self
            .operation
            .lock()
            .map_err(|_| HttpCommandError::internal("调试锁损坏"))?;
        self.runtime
            .submit_external(DebugAtomicRoutine(
                crate::debug_sandbox::AtomicCommand::Capture,
            ))
            .map_err(|e| HttpCommandError::internal(format!("{e:#}")))?
            .wait_timeout(std::time::Duration::from_secs(30))
            .map_err(|e| HttpCommandError::internal(format!("{e:#}")))?
            .map_err(|e| HttpCommandError::internal(format!("{e:#}")))
    }
}

#[derive(Clone)]
pub(crate) struct ApplicationHttpAiFacade {
    client: AiClient,
    /// 记忆库句柄。为 None 时（测试装配）记忆接口返回明确的未装配错误。
    memory: Option<std::sync::Arc<crate::features::ai_chat::AiChatMemory>>,
    /// 历史重跑清洗：给定玩家名，用新规则重放其全部问答并重建事实表。
    ///
    /// 做成闭包而不是给 facade 塞 AI 运行时依赖：重跑需要模型调用与配置快照，
    /// 那些都在 composition 侧，facade 不该为了一个维护动作把整条链路引进来。
    rebuild: Option<std::sync::Arc<dyn Fn(&str) -> anyhow::Result<usize> + Send + Sync>>,
    /// 记忆键 → 显示昵称。记忆键是 UUID，面板必须换回人能认的名字。
    ///
    /// 同样做成闭包：身份映射在 composition 侧，facade 不该直接持有它。
    display_name: Option<std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>>,
    /// 显示昵称 → 记忆键。
    ///
    /// 面板列表下发的是显示名，回传时也带着 key；但书签 URL 与旧脚本只有名字，
    /// 所以入口统一过一遍这个解析。对 UUID 输入幂等，新旧两种入参都能用。
    memory_key: Option<std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>>,
    /// 记忆维护代际：面板删除/清空后自增，作废在途的抽取/压缩提交。
    ///
    /// 不加这一步时，一条正在跑的抽取线程会把用户刚删掉的记忆再写回来
    /// （"@忘记我 之后又被想起" 的同一类问题，只是入口变成面板）。
    generation: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}

impl ApplicationHttpAiFacade {
    pub(crate) fn new(client: AiClient) -> Self {
        Self {
            client,
            memory: None,
            rebuild: None,
            display_name: None,
            memory_key: None,
            generation: None,
        }
    }

    /// 面板删改记忆后作废在途维护任务。没装配代际时什么也不做（测试装配）。
    fn invalidate_pending_memory_tasks(&self) {
        if let Some(generation) = &self.generation {
            generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// 生产装配：额外带上记忆库，供面板查询与删改。
    pub(crate) fn with_memory(
        client: AiClient,
        memory: std::sync::Arc<crate::features::ai_chat::AiChatMemory>,
        rebuild: std::sync::Arc<dyn Fn(&str) -> anyhow::Result<usize> + Send + Sync>,
        display_name: std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>,
        memory_key: std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>,
        generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        Self {
            client,
            memory: Some(memory),
            rebuild: Some(rebuild),
            display_name: Some(display_name),
            memory_key: Some(memory_key),
            generation: Some(generation),
        }
    }

    /// 把记忆键换成人能认的名字；未装配时原样返回（测试装配）。
    fn display_name_of(&self, key: &str) -> String {
        match self.display_name.as_deref() {
            Some(f) => f(key),
            None => key.to_owned(),
        }
    }

    /// 把面板传来的名字解析回记忆键。
    ///
    /// 对 UUID 幂等：UUID 既不是任何映射的 nickname 也不是 note，原样返回。
    /// 未装配时（测试）原样返回，行为不变。
    fn resolve_key(&self, name_or_key: &str) -> String {
        match self.memory_key.as_deref() {
            Some(f) => f(name_or_key),
            None => name_or_key.to_owned(),
        }
    }

    fn memory(&self) -> anyhow::Result<&crate::features::ai_chat::AiChatMemory> {
        self.memory
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("记忆库未装配"))
    }
}

impl HttpAiPort for ApplicationHttpAiFacade {
    fn recognize(&self, query: &[(String, String)]) -> Result<String> {
        self.client.recognize_with_query(query)
    }

    fn match_song(&self, query: &[(String, String)]) -> Result<String> {
        self.client.match_with_query(query)
    }

    fn pick(&self, query: &[(String, String)]) -> Result<String> {
        self.client.pick_with_query(query)
    }

    fn memory_actors(&self) -> Result<String> {
        let actors = self.memory()?.list_actors()?;
        // key 是身份（进 SQL），actor 是标签（只给人看）。
        //
        // 原来这里把 key 直接换成显示名下发，而查询接口又把显示名当 key 用——
        // 已登记玩家的键是 UUID，于是点开永远是空。两者必须分开返回。
        let actors = actors
            .into_iter()
            .map(|summary| {
                let key = summary.actor;
                json!({
                    "key": key,
                    "actor": self.display_name_of(&key),
                    "facts": summary.facts,
                    "turns": summary.turns,
                    "tokens": summary.tokens,
                })
            })
            .collect::<Vec<_>>();
        Ok(serde_json::to_string(&json!({ "actors": actors }))?)
    }

    fn memory_actor(&self, actor: &str) -> Result<String> {
        let memory = self.memory()?;
        let key = self.resolve_key(actor);
        // 用读取预算而不是 usize::MAX：后者会让 bot_lines 全表扫，
        // 而结果在面板里根本用不到（面板只展示 facts/turns/summary）。
        let mut context =
            memory.context(&key, crate::features::ai_chat::BOT_HISTORY_READ_BUDGET)?;
        // 老库有按昵称直接存的孤儿行（见 ai_chat_actor_names 的建表注释）。
        // 解析后的键查空时按原文再试一次，否则那些历史会凭空消失。
        let mut used = key.clone();
        if context.facts.is_empty() && context.turns.is_empty() && context.summary.is_none() {
            let legacy =
                memory.context(actor, crate::features::ai_chat::BOT_HISTORY_READ_BUDGET)?;
            if !legacy.facts.is_empty() || !legacy.turns.is_empty() || legacy.summary.is_some() {
                used = actor.to_owned();
                context = legacy;
            }
        }
        // context() 把 asker 填成入参本身；不换回显示名，面板会显示一行 UUID。
        let display = self.display_name_of(&used);
        let facts = context.facts;
        let summary = context.summary;
        let found = !facts.is_empty() || !context.turns.is_empty() || summary.is_some();
        let turns = context
            .turns
            .into_iter()
            .map(|turn| {
                json!({ "asker": display, "question": turn.question, "answer": turn.answer })
            })
            .collect::<Vec<_>>();
        Ok(serde_json::to_string(&json!({
            "key": used,
            "actor": display,
            "found": found,
            "facts": facts,
            "summary": summary,
            "turns": turns,
        }))?)
    }

    fn memory_hall(&self) -> Result<String> {
        let memory = self.memory()?;
        let hall = memory.hall_context()?;
        let (count, tokens) = memory.hall_stats()?;
        let lines = hall
            .lines
            .into_iter()
            .map(|(speaker, text, role)| {
                json!({ "speaker": speaker, "text": text, "role": role.as_str() })
            })
            .collect::<Vec<_>>();
        Ok(serde_json::to_string(&json!({
            "summary": hall.summary,
            "lines": lines,
            "count": count,
            "tokens": tokens,
        }))?)
    }

    fn forget_actor(&self, actor: &str) -> Result<String> {
        let memory = self.memory()?;
        let key = self.resolve_key(actor);
        let mut removed = memory.forget(&key)?;
        // 旧行可能直接存在显示名之下；原样删只命中字面量行，不会误删别人。
        if key != actor && memory.forget(actor)? {
            removed = true;
        }
        // 在途抽取看到的是删除前的 facts，提交时会把它们写回来。
        self.invalidate_pending_memory_tasks();
        Ok(serde_json::to_string(
            &json!({ "ok": removed, "removed": removed, "key": key }),
        )?)
    }

    fn delete_fact(&self, actor: &str, fact: &str) -> Result<String> {
        let memory = self.memory()?;
        let key = self.resolve_key(actor);
        let mut removed = memory.delete_fact(&key, fact)?;
        if !removed && key != actor {
            removed = memory.delete_fact(actor, fact)?;
        }
        // 被删的这条可能正在某个抽取线程的 facts 快照里，提交会复活它。
        self.invalidate_pending_memory_tasks();
        Ok(serde_json::to_string(
            &json!({ "ok": removed, "removed": removed, "key": key }),
        )?)
    }

    fn rebuild_facts(&self, actor: &str) -> Result<String> {
        let rebuild = self
            .rebuild
            .clone()
            .ok_or_else(|| anyhow::anyhow!("记忆重建未装配"))?;
        // 重放要为每一轮问答调一次模型，几十轮就是几十次请求，
        // 同步做会让 HTTP 请求挂几分钟。这里只负责受理，结果看日志与面板。
        //
        // 必须先解析成记忆键：传显示名时 all_turns 查空，重建会「成功」地
        // 写入 0 条事实——返回 accepted:true 却什么都没做。
        let key = self.resolve_key(actor);
        let display = self.display_name_of(&key);
        let worker_key = key.clone();
        let worker_display = display.clone();
        std::thread::Builder::new()
            .name("ai-memory-rebuild".to_string())
            .spawn(move || match rebuild(&worker_key) {
                Ok(count) => log::info!(
                    "玩家「{worker_display}」的记忆重建完成，共整理出 {count} 条事实"
                ),
                Err(error) => log::warn!(
                    "玩家「{worker_display}」的记忆重建失败（步骤：从历史对话抽取事实），本次重建未生效，该玩家记忆保持原样：{error:#}"
                ),
            })?;
        Ok(serde_json::to_string(&json!({
            "ok": true,
            "accepted": true,
            "key": key,
            "actor": display,
            "note": "重建在后台执行，结果见日志与记忆面板",
        }))?)
    }

    fn forget_hall(&self) -> Result<String> {
        let removed = self.memory()?.forget_hall()?;
        // 大厅压缩线程会把「旧摘要 + 刚删掉的原文」再写回去，必须一并作废。
        self.invalidate_pending_memory_tasks();
        Ok(serde_json::to_string(
            &json!({ "ok": true, "removed": removed }),
        )?)
    }
}
