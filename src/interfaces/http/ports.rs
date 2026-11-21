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
    /// 网页端专用：一次性扫描全部表情页签，返回 JSON 并落盘全量目录。
    fn emoji_scan(&self) -> Result<String, HttpCommandError>;
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
}
fn default_debug_mode() -> String {
    "simulated".into()
}

/// HTTP 运行请求 → debug 层的领域请求。
///
/// 协议 DTO 留在接口层（serde 的 camelCase / deny_unknown_fields 是线上契约），
/// debug 层只接收自己的 `DebugRunRequest`，不反向依赖 HTTP 类型。
impl From<HttpDebugRunRequest> for crate::debug_http_service::DebugRunRequest {
    fn from(request: HttpDebugRunRequest) -> Self {
        Self {
            task_id: request.task_id,
            mode: request.mode,
            expected_version: request.expected_version,
            assertions: request.assertions,
            fixture: request.fixture,
        }
    }
}

/// debug 领域错误 → HTTP 命令错误：种类决定状态码，原因原文透传。
///
/// 沙盒服务此前用 `HttpCommandError` 直接表达失败（几乎都是 400），
/// 这里保持逐字节一致的状态码与错误文案；新增的 not_found/conflict/internal
/// 只是把原本混在 400 里的语义留出了扩展位，当前行为不变。
impl From<crate::debug_http_service::DebugServiceError> for HttpCommandError {
    fn from(error: crate::debug_http_service::DebugServiceError) -> Self {
        use crate::debug_http_service::DebugServiceErrorKind;
        let status = match error.kind() {
            DebugServiceErrorKind::BadRequest => 400,
            DebugServiceErrorKind::NotFound => 404,
            DebugServiceErrorKind::Conflict => 409,
            DebugServiceErrorKind::Internal => 500,
        };
        Self {
            status,
            message: error.message().to_owned(),
        }
    }
}

/// 调试端口：完全控制/只读观察的全部能力。
///
/// **不再向下游泄漏 `DebugHttpService` 具体类型**——以前 `sandbox()` 把整个
/// 服务对象交出去，接口层可以直接调它的任意方法，端口边界形同虚设。
/// 现在每个沙盒操作都是一个语义明确的端口方法；调试端口的实现者必须显式接线，
/// 这里不提供任何"静默成功"的默认实现，否则漏接线会在生产里悄悄变成空操作。
pub(crate) trait HttpDebugPort: Send + Sync {
    fn enabled(&self) -> bool;
    fn set_enabled(&self, enabled: bool) -> Result<(), HttpCommandError>;
    fn capture(&self) -> Result<Arc<DynamicImage>, HttpCommandError>;
    fn window_available(&self) -> Result<bool, HttpCommandError>;
    /// 只读界面状态详情：当前分类结果 + 判别证据。未分类时如实报告，不伪造状态。
    fn ui_state_detail(&self) -> Result<String, HttpCommandError>;

    /// 切换只读观察的四态开关（关闭 / active / push / control）。
    fn set_read_only_mode(
        &self,
        enabled: bool,
        state: crate::debug_read_only::ReadOnlyState,
    ) -> Result<serde_json::Value, HttpCommandError>;
    /// 只读主动读取一帧并记录证据。
    fn read_active(
        &self,
        request: crate::debug_http_service::ReadOnlyReadRequest,
    ) -> Result<serde_json::Value, HttpCommandError>;
    /// 只读状态摘要：只读缓存，不抓帧、不推理、不发布事件。
    fn read_only_summary(&self) -> serde_json::Value;

    /// 保存（或覆盖）一个沙盒任务，返回任务 id 的调试表示。
    fn save(&self, task: crate::debug_sandbox::SandboxTask) -> Result<String, HttpCommandError>;
    /// 直接执行一条原子命令（不建任务、不建运行记录）。
    fn execute_direct(
        &self,
        command: crate::debug_sandbox::AtomicCommand,
    ) -> Result<crate::debug_sandbox::SandboxRun, HttpCommandError>;
    /// 按领域请求启动一次运行。
    fn start(
        &self,
        request: crate::debug_http_service::DebugRunRequest,
    ) -> Result<crate::debug_sandbox::SandboxRun, HttpCommandError>;
    /// 以源运行的夹具与断言重放该任务。
    fn replay(&self, source_id: &str)
    -> Result<crate::debug_sandbox::SandboxRun, HttpCommandError>;
    /// 单步推进（all=false）或推进到结束（all=true）。
    fn advance(
        &self,
        id: &str,
        all: bool,
    ) -> Result<crate::debug_sandbox::SandboxRun, HttpCommandError>;
    /// 运行的聚合状态快照（`{ run, task }`，或归档记录）。
    fn status_snapshot(&self, id: &str) -> Result<serde_json::Value, HttpCommandError>;
    /// 取消运行。
    fn cancel(&self, id: &str) -> Result<crate::debug_sandbox::SandboxRun, HttpCommandError>;
    /// 取某次运行的证据；`index` 可选，用于只取某一步。
    fn evidence(
        &self,
        id: &str,
        index: Option<usize>,
    ) -> Result<Vec<serde_json::Value>, HttpCommandError>;
    /// 列出已保存的沙盒任务。
    fn tasks(&self) -> Result<Vec<crate::debug_sandbox::SandboxTask>, HttpCommandError>;
    /// 列出已知的运行记录（含已归档的）。
    fn runs(&self) -> Result<Vec<serde_json::Value>, HttpCommandError>;
}

pub(crate) trait HttpHallPort: Send + Sync {
    fn capture_hall_screenshot(&self) -> Result<Arc<DynamicImage>>;
}

/// 播放器端口：状态、缓存维护、播放位置、播放模式与搜索。
///
/// **全部方法都是必选的**，这里不提供任何"静默空操作"的默认实现。
/// 以前 `cache_stats` / `cached_tracks` / `reset_track_statistics` /
/// `invalidate_track_cache` / `seek` / `play_mode` / `set_play_mode` 都有返回
/// `Ok((None, vec![]))` / `Ok(false)` / `Ok(())` / `Ok(0)` 的默认实现：
/// 生产实现漏接一个方法时，接口会"成功"但什么都没做，面板与调用方都看不出异常。
/// 现在漏接是编译错误；每个实现者必须显式回答"这个能力到底有没有接线"。
pub(crate) trait HttpPlayerPort: Send + Sync {
    fn status(&self) -> Result<PlayerStatus>;
    /// 批量查询音频缓存总览与逐曲状态；未启用缓存时返回 `None` 与空状态列表。
    fn cache_stats(
        &self,
        keys: &[TrackKey],
    ) -> Result<(Option<AudioCacheStats>, Vec<AudioCacheTrackStatus>)>;
    /// 分页查询磁盘缓存歌曲列表；`sort`/`ascending` 透传给播放器的元数据索引。
    fn cached_tracks(
        &self,
        offset: usize,
        limit: usize,
        sort: CacheTrackSortKey,
        ascending: bool,
    ) -> Result<CachedTrackPage>;
    /// 清零单曲统计（保留缓存与元数据）；返回是否真的清掉了一行。
    fn reset_track_statistics(&self, key: &TrackKey) -> Result<bool>;
    /// 删除指定曲目的磁盘音频缓存（下次播放重新下载）；返回是否真的删掉了。
    fn invalidate_track_cache(&self, key: &TrackKey) -> Result<bool>;
    /// 跳转到指定播放位置（秒）。
    fn seek(&self, position_seconds: f64) -> Result<()>;
    /// 当前播放模式：0=顺序 1=单曲循环 2=随机。
    fn play_mode(&self) -> Result<u8>;
    /// 设置播放模式（0=顺序 1=单曲循环 2=随机）。
    fn set_play_mode(&self, mode: u8) -> Result<()>;
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

/// AI 点歌端口：识歌、歌曲身份匹配与候选挑选。
///
/// 这三个方法都是**对模型的一次问答**，输入是 HTTP 查询参数，输出是模型回答的
/// 文本；它们不碰聊天记忆、不碰提示词装配，因此与 [`HttpAiMemoryPort`] 正交。
pub(crate) trait HttpAiMusicPort: Send + Sync {
    fn recognize(&self, query: &[(String, String)]) -> Result<String>;
    fn match_song(&self, query: &[(String, String)]) -> Result<String>;
    fn pick(&self, query: &[(String, String)]) -> Result<String>;
}

/// AI 聊天记忆端口：记忆库查询/删改、历史重建与提示词装配预览。
///
/// 这一组方法共享同一份「聊天记忆」事实源与同一条提示词装配链路，和点歌无关。
/// 拆成两个端口后，装配点必须分别回答「点歌能力接了吗」「记忆能力接了吗」；
/// 混在一个 `HttpAiPort` 里时，只要实现了其中一半，另一半就悄悄缺位，
/// 边界形同虚设。
///
/// **全部方法都是必选的**：`chat_preview` 以前带一个静态返回"未装配"的默认
/// 实现，会让漏接线的生产装配在运行期只表现为一句功能错误，而不是编译失败。
pub(crate) trait HttpAiMemoryPort: Send + Sync {
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
    /// 用当前抽取规则重放该玩家全部历史问答，重建事实表。
    fn rebuild_facts(&self, actor: &str) -> Result<String>;
    /// 装配预览：用给定配置（可含未保存草稿）拼出 system/tail/tools，不调用模型。
    fn chat_preview(
        &self,
        config: &crate::config::AppConfig,
        asker: &str,
        question: &str,
    ) -> Result<String>;
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
    pub ai_music: Arc<dyn HttpAiMusicPort>,
    pub ai_memory: Arc<dyn HttpAiMemoryPort>,
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
        ai_music: Arc<dyn HttpAiMusicPort>,
        ai_memory: Arc<dyn HttpAiMemoryPort>,
    ) -> Self {
        Self {
            commands,
            tasks,
            queries,
            hall,
            debug: None,
            player,
            login,
            ai_music,
            ai_memory,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_http_service::{DebugRunRequest, DebugServiceError};

    /// 领域错误 → HTTP 错误的映射必须保住原有的状态码与文案：
    /// 线上协议里状态码是契约，文案是调用方与日志能看到的唯一解释。
    #[test]
    fn debug_service_error_maps_to_the_same_http_status_and_message() {
        for (error, status) in [
            (DebugServiceError::bad_request("bad"), 400u16),
            (DebugServiceError::not_found("gone"), 404),
            (DebugServiceError::conflict("stale"), 409),
            (DebugServiceError::internal("boom"), 500),
        ] {
            let message = error.message().to_owned();
            let mapped: HttpCommandError = error.into();
            assert_eq!(mapped.status, status);
            assert_eq!(mapped.message, message);
        }
    }

    /// 沙盒服务此前所有失败都是 400，文案原样透传；这条断言锁住"行为等价"。
    #[test]
    fn bad_request_keeps_the_original_message_verbatim() {
        let mapped: HttpCommandError = DebugServiceError::bad_request("任务不存在").into();
        assert_eq!(mapped.status, 400);
        assert_eq!(mapped.message, "任务不存在");
        assert_eq!(mapped.to_string(), "任务不存在");
    }

    /// HTTP 运行请求 → 领域请求是逐字段搬运，不丢字段、不改默认值的落地结果。
    #[test]
    fn http_debug_run_request_converts_field_by_field() {
        let request: HttpDebugRunRequest = serde_json::from_value(serde_json::json!({
            "taskId": "t1",
            "expectedVersion": 7,
            "fixture": {"width": 2, "height": 2, "rgba": [1, 2, 3, 255]},
        }))
        .unwrap();
        let domain: DebugRunRequest = request.into();
        assert_eq!(domain.task_id, "t1");
        assert_eq!(domain.mode, "simulated");
        assert_eq!(domain.expected_version, Some(7));
        assert!(domain.assertions.is_empty());
        assert!(domain.fixture.is_some());
    }
}
