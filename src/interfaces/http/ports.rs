use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use image::DynamicImage;
use miliastra_playback::{
    AudioCacheStats, AudioCacheTrackStatus, CacheTrackSortKey, CachedTrackPage, CredentialStatus,
    KugouAccountStatus, KugouListenReport, LoginSession, ProviderAccountStatus, ProviderId,
    TrackKey, TrackMetadata, TrackRef,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::protocol::WebToolRequest;
use crate::features::hall::{HallCommand, HallRuntimeState};
use crate::features::playback::{
    PlaybackCommand, PlaybackRuntimeState, PlayerStatus, QueueItem, SongRequestLeaderboardEntry,
};
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
    /// 复用播放队列去重策略查询：新项是否与队列现有项重复（含结构化/待解析交叉形态）。
    fn playback_queue_contains(&self, item: QueueItem) -> Result<bool>;
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
    /// 发送一条游戏内聊天。与点歌一样是**入队**语义：返回成功只代表任务已排队。
    fn chat_send(&self, text: String, prefix: String) -> Result<String, HttpCommandError>;
    /// 提交一次游戏启动任务。kind 只接受 start_game / enter_wonderland。
    fn startup_task(&self, kind: String) -> Result<String, HttpCommandError>;
    fn request_chat_listener_mode(
        &self,
        mode: ChatListenerMode,
    ) -> Result<String, HttpCommandError>;
    fn set_operator_commands(&self, enabled: bool) -> Result<String, HttpCommandError>;
    fn set_idle_exit(&self, minutes: u32) -> Result<String, HttpCommandError>;
    fn clear_idle_exit(&self) -> Result<String, HttpCommandError>;
    /// 调试用：模拟一条聊天消息注入观察链路，按指定身份走完整路由与权限判定。
    ///
    /// role 决定消息的 authority 与发送者身份：
    /// - `user`：大厅普通成员（blue，无身份映射）
    /// - `friend`：大厅好友（blue，身份映射为 Friend）
    /// - `admin`：大厅管理员（blue，身份映射为 Admin）
    /// - `owner`：大厅主人（blue，身份映射为 Owner）
    /// - `friend_private`：好友私聊（pink，Friend）
    /// - `admin_private`：管理员私聊（pink，Admin）
    ///
    /// 只注入聊天链路，不点鼠标、不截图。
    fn simulate_chat(&self, role: String, text: String) -> Result<String, HttpCommandError>;
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
    /// 成功播放的点歌人排行榜；`since_ms` 为 None 时统计全部历史。
    fn song_request_leaderboard(
        &self,
        since_ms: Option<u64>,
        limit: usize,
    ) -> Result<Vec<SongRequestLeaderboardEntry>>;
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
enum LegacyHttpDebugCommand {
    Click {
        x: i32,
        y: i32,
    },
    #[serde(alias = "press_key")]
    Key {
        key: String,
    },
    Scroll {
        x: i32,
        y: i32,
        amount: i32,
    },
    Drag {
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
    },
    Paste {
        text: String,
    },
    Wait {
        milliseconds: u64,
    },
    Capture,
}

impl LegacyHttpDebugCommand {
    fn into_atomic(self) -> crate::debug_sandbox::AtomicCommand {
        use crate::debug_sandbox::AtomicCommand;
        match self {
            Self::Click { x, y } => AtomicCommand::Click { x, y },
            Self::Key { key } => AtomicCommand::PressKey { key },
            Self::Scroll { x, y, amount } => AtomicCommand::Scroll { x, y, amount },
            Self::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
            } => AtomicCommand::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
            },
            Self::Paste { text } => AtomicCommand::Paste { text },
            Self::Wait { milliseconds } => AtomicCommand::Wait { milliseconds },
            Self::Capture => AtomicCommand::Capture,
        }
    }
}

/// 直接调试命令**就是**任务模型里的同一个 `AtomicCommand`，不再维护第二份词表——
/// 这次就是因为两套词表才出现"任务模型能跑、/debug/command 不能跑"的分裂。
///
/// 兼容旧写法：旧请求把命令字段平铺在 body 上（`{"command":"click","x":1,"y":2}`），
/// 新写法与任务模型一致（`{"command":{"click":{"x":1,"y":2}}}`）。两者都接受，
/// 但 `AtomicCommand` 是唯一事实来源，新增原子命令不需要再改这里。
pub(crate) struct HttpDebugCommand(crate::debug_sandbox::AtomicCommand);

impl HttpDebugCommand {
    pub(crate) fn into_atomic(self) -> crate::debug_sandbox::AtomicCommand {
        self.0
    }
}

impl<'de> serde::Deserialize<'de> for HttpDebugCommand {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(inner) = value.get("command") {
            if let Ok(command) =
                serde_json::from_value::<crate::debug_sandbox::AtomicCommand>(inner.clone())
            {
                return Ok(Self(command));
            }
        }
        if let Ok(command) =
            serde_json::from_value::<crate::debug_sandbox::AtomicCommand>(value.clone())
        {
            return Ok(Self(command));
        }
        serde_json::from_value::<LegacyHttpDebugCommand>(value)
            .map(|legacy| Self(legacy.into_atomic()))
            .map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct HttpDebugRunRequest {
    pub task_id: String,
    #[serde(default = "default_debug_mode")]
    pub mode: String,
    pub expected_version: Option<u32>,
    #[serde(default)]
    pub assertions: Vec<crate::debug_sandbox_runner::StepAssertion>,
    pub fixture: Option<crate::debug_sandbox::SandboxFixture>,
    pub plugin_id: Option<String>,
    pub manifest: Option<crate::debug_sandbox::PluginManifest>,
}
fn default_debug_mode() -> String {
    "simulated".into()
}

pub(crate) trait HttpDebugPort: Send + Sync {
    fn sandbox(&self) -> &crate::debug_http_service::DebugHttpService;

    fn enabled(&self) -> bool;
    fn set_enabled(&self, enabled: bool) -> Result<(), HttpCommandError>;
    fn capture(&self) -> Result<Arc<DynamicImage>, HttpCommandError>;
    fn window_available(&self) -> Result<bool, HttpCommandError>;
    /// 只读界面状态详情：当前分类结果 + 判别证据。未分类时如实报告，不伪造状态。
    fn ui_state_detail(&self) -> Result<String, HttpCommandError>;
}

pub(crate) trait HttpHallPort: Send + Sync {
    fn capture_hall_screenshot(&self) -> Result<Arc<DynamicImage>>;
}

pub(crate) trait HttpPlayerPort: Send + Sync {
    fn status(&self) -> Result<PlayerStatus>;
    fn cache_stats(
        &self,
        _keys: &[TrackKey],
    ) -> Result<(Option<AudioCacheStats>, Vec<AudioCacheTrackStatus>)> {
        Ok((None, Vec::new()))
    }
    /// 分页查询磁盘缓存歌曲列表；默认实现返回空页（播放器未接入缓存时）。
    /// `sort`/`ascending` 透传给播放器的元数据索引。
    fn cached_tracks(
        &self,
        offset: usize,
        limit: usize,
        sort: CacheTrackSortKey,
        ascending: bool,
    ) -> Result<CachedTrackPage> {
        let _ = (sort, ascending);
        Ok(CachedTrackPage::empty(offset, limit))
    }
    /// 清零单曲统计；默认实现表示未接入播放器缓存。
    fn reset_track_statistics(&self, _key: &TrackKey) -> Result<bool> {
        Ok(false)
    }
    /// 删除指定曲目的磁盘音频缓存（下次播放重新下载）；默认实现表示未接入。
    fn invalidate_track_cache(&self, _key: &TrackKey) -> Result<bool> {
        Ok(false)
    }
    /// 跳转到指定播放位置（秒）；默认实现表示未接入。
    fn seek(&self, _position_seconds: f64) -> Result<()> {
        Ok(())
    }
    /// 当前播放模式：0=顺序 1=单曲循环 2=随机。
    fn play_mode(&self) -> Result<u8> {
        Ok(0)
    }
    /// 设置播放模式（0=顺序 1=单曲循环 2=随机）。
    fn set_play_mode(&self, mode: u8) -> Result<()> {
        let _ = mode;
        Ok(())
    }
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
    pub refresh_supported: bool,
    pub manual_refresh_supported: bool,
    pub refresh_ready: bool,
    pub refresh_state: &'static str,
    pub last_refresh_at_ms: Option<u64>,
    pub next_refresh_check_at_ms: Option<u64>,
    pub last_refresh_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HttpLoginErrorView {
    pub code: String,
    pub message: String,
    pub provider: Option<ProviderId>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HttpLoginQrCodeView {
    pub image_data_url: String,
    pub received_at_ms: u64,
}

impl std::fmt::Debug for HttpLoginQrCodeView {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpLoginQrCodeView")
            .field("received_at_ms", &self.received_at_ms)
            .finish_non_exhaustive()
    }
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
    pub qr_code: Option<HttpLoginQrCodeView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<HttpLoginErrorView>,
}

#[derive(Clone, Debug)]
pub(crate) struct HttpLoginError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for HttpLoginError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HttpLoginError {}

pub(crate) trait HttpLoginPort: Send + Sync {
    fn providers(&self) -> Result<Vec<HttpProviderView>, HttpLoginError>;
    fn status(&self) -> HttpLoginStatus;
    fn start(&self, provider: ProviderId) -> Result<LoginSession, HttpLoginError>;
    fn cancel(&self, session_id: Uuid) -> Result<(), HttpLoginError>;
    fn logout(&self, provider: ProviderId) -> Result<CredentialStatus, HttpLoginError>;
    fn refresh(&self, provider: ProviderId) -> Result<CredentialStatus, HttpLoginError>;
    fn kugou_status(&self) -> Result<KugouAccountStatus, HttpLoginError>;
    fn account_status(
        &self,
        provider: ProviderId,
    ) -> Result<Option<ProviderAccountStatus>, HttpLoginError>;
    fn refresh_account_status(
        &self,
        provider: ProviderId,
    ) -> Result<Option<ProviderAccountStatus>, HttpLoginError>;
    fn kugou_claim_vip(&self) -> Result<KugouListenReport, HttpLoginError>;
    fn kugou_upgrade_vip(&self) -> Result<KugouListenReport, HttpLoginError>;
}

pub(crate) trait HttpAiPort: Send + Sync {
    fn recognize(&self, query: &[(String, String)]) -> Result<String>;
    fn match_song(&self, query: &[(String, String)]) -> Result<String>;
    fn pick(&self, query: &[(String, String)]) -> Result<String>;

    /// 列出所有有记忆的玩家及其规模。
    fn memory_actors(&self) -> Result<String>;
    /// 读取某个玩家的完整记忆（事实 + 摘要 + 原文）。
    fn memory_actor(&self, actor: &str) -> Result<String>;
    /// 读取大厅共享记忆。
    fn memory_hall(&self) -> Result<String>;
    /// 删除某个玩家的记忆。
    fn forget_actor(&self, actor: &str) -> Result<String>;
    /// 删除某条事实。
    fn delete_fact(&self, actor: &str, fact: &str) -> Result<String>;
    /// 清空大厅共享记忆。
    fn forget_hall(&self) -> Result<String>;
}

#[derive(Clone)]
pub(crate) struct HttpApplicationPorts {
    pub commands: Arc<dyn HttpCommandPort>,
    pub tasks: Arc<dyn HttpTaskPort>,
    pub queries: Arc<dyn HttpQueryPort>,
    pub hall: Arc<dyn HttpHallPort>,
    pub debug: Option<Arc<dyn HttpDebugPort>>,
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
            debug: None,
            player,
            login,
            ai,
        }
    }
}
