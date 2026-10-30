use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use miliastra_playback::{PlayableTrack, ProviderId, TrackKey};

use crate::features::reply_template::playback as playback_templates;
use crate::features::reply_template::{self, ReplyTemplate, ReplyVars};
use crate::features::song_request::SearchCandidate;
use crate::text::{MAX_CHAT_WIDTH, char_width, display_width};

use super::matcher::same_song_query;
use super::{
    BackgroundLyricsScope, PersonalPlaylistStart, PlaybackCommand, PlaybackNavigation,
    PlaybackOutcome, PlaybackRequest, PlaybackSnapshot, PlaybackVerification, PlayerStatus,
    QueueAdvanceContext, QueueAdvanceDecision, QueueItem, QueueRemoval, estimated_player_status,
    format_lyrics, format_status_with_mode,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlaybackCommandContext {
    pub(crate) message_type: String,
    pub(crate) username: String,
    pub(crate) user_command: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlaybackPickedCandidate {
    pub(crate) text: String,
    pub(crate) track: PlayableTrack,
    pub(crate) candidate_snapshot: Vec<SearchCandidate>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PlaybackSearchFailure {
    Busy,
    Unavailable(String),
    Backend(String),
    Unexpected(String),
}

impl PlaybackSearchFailure {
    fn user_message(&self) -> &'static str {
        match self {
            Self::Busy => "歌曲搜索繁忙，请稍后再试",
            Self::Unavailable(_) => "歌曲搜索服务暂不可用，请稍后再试",
            Self::Backend(_) => "歌曲搜索后端失败，请稍后再试",
            Self::Unexpected(_) => "歌曲搜索后端返回异常，请稍后再试",
        }
    }
}

impl Display for PlaybackSearchFailure {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => formatter.write_str("player search queue full"),
            Self::Unavailable(reason) => write!(formatter, "player search unavailable: {reason}"),
            Self::Backend(reason) => write!(formatter, "player search backend failed: {reason}"),
            Self::Unexpected(reason) => {
                write!(formatter, "unexpected player search outcome: {reason}")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlaybackSelection {
    pub(crate) keyword: String,
    pub(crate) source: String,
    pub(crate) prefer_accompaniment: bool,
    pub(crate) ai_original_text: String,
    pub(crate) track: Option<PlayableTrack>,
    pub(crate) friend_username: String,
    pub(crate) requester: String,
    pub(crate) console_bypass_dedup: bool,
    pub(crate) candidate_snapshot: Vec<SearchCandidate>,
    /// 队列消费来源时携带队首 queue_item_id：确认播放成功时与播放状态原子出队，
    /// 崩溃后重启不会重播已确认消费的队首。手动点歌/播放池/恢复播放为 None。
    pub(crate) queue_item_id: Option<u64>,
}

impl PlaybackSelection {
    pub(crate) fn request(&self) -> PlaybackRequest {
        PlaybackRequest {
            keyword: self.keyword.clone(),
            source: self.source.clone(),
            prefer_accompaniment: self.prefer_accompaniment,
            track: self.track.clone(),
            requester: self.requester.clone(),
            navigation: PlaybackNavigation::Normal,
            candidate_snapshot: self.candidate_snapshot.clone(),
            queue_item_id: self.queue_item_id,
        }
    }

    fn label(&self) -> String {
        let username = self.friend_username.trim();
        if username.is_empty() {
            String::new()
        } else {
            format!("好友{}:", username)
        }
    }

    /// 去重文案的变量：两条模板（点播拦截 / 队列跳过）都只插入歌名。
    fn dedup_vars(&self) -> ReplyVars {
        ReplyVars::new().with("歌名", self.keyword.clone())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PlaybackResult {
    outcome: PlaybackOutcome,
    requested: PlaybackRequest,
    final_request: PlaybackRequest,
    status: Option<PlayerStatus>,
    source_switched: bool,
    failure_reason: Option<String>,
}

impl PlaybackResult {
    fn success(
        requested: &PlaybackRequest,
        final_request: &PlaybackRequest,
        status: PlayerStatus,
        source_switched: bool,
    ) -> Self {
        Self {
            outcome: PlaybackOutcome::Success,
            requested: requested.clone(),
            final_request: final_request.clone(),
            status: Some(status),
            source_switched,
            failure_reason: None,
        }
    }

    fn no_source(
        requested: &PlaybackRequest,
        final_request: &PlaybackRequest,
        status: Option<PlayerStatus>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            outcome: PlaybackOutcome::ItemScopedFailure,
            requested: requested.clone(),
            final_request: final_request.clone(),
            status,
            source_switched: false,
            failure_reason: Some(reason.into()),
        }
    }

    fn error(
        requested: &PlaybackRequest,
        final_request: &PlaybackRequest,
        status: Option<PlayerStatus>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            outcome: PlaybackOutcome::QueueBlockingFailure,
            requested: requested.clone(),
            final_request: final_request.clone(),
            status,
            source_switched: false,
            failure_reason: Some(reason.into()),
        }
    }

    fn dedup_limited(request: &PlaybackRequest) -> Self {
        Self {
            outcome: PlaybackOutcome::DedupLimited,
            requested: request.clone(),
            final_request: request.clone(),
            status: None,
            source_switched: false,
            failure_reason: Some("近期已播放过".to_string()),
        }
    }

    pub(crate) const fn outcome(&self) -> PlaybackOutcome {
        self.outcome
    }

    pub(crate) const fn requested(&self) -> &PlaybackRequest {
        &self.requested
    }

    pub(crate) const fn final_request(&self) -> &PlaybackRequest {
        &self.final_request
    }

    pub(crate) const fn status(&self) -> Option<&PlayerStatus> {
        self.status.as_ref()
    }

    pub(crate) const fn source_switched(&self) -> bool {
        self.source_switched
    }

    pub(crate) fn failure_reason(&self) -> Option<&str> {
        self.failure_reason.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        outcome: PlaybackOutcome,
        requested: &PlaybackRequest,
        final_request: &PlaybackRequest,
    ) -> Self {
        Self {
            outcome,
            requested: requested.clone(),
            final_request: final_request.clone(),
            status: None,
            source_switched: outcome == PlaybackOutcome::Success
                && requested.track != final_request.track,
            failure_reason: (outcome != PlaybackOutcome::Success)
                .then(|| format!("test outcome: {outcome:?}")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConsumedSelection {
    /// 来自点歌队列（严格按播放列表顺序）。
    Queue { keyword: String },
    /// 队列为空时来自播放池随机续播（仅自动兜底语义）。
    Pool { keyword: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaybackPurpose {
    Requested,
    Queue { stop_when_user_paused: bool },
    Playlist { stop_when_user_paused: bool },
    Pool { stop_when_user_paused: bool },
    Repeat,
    Previous,
    SourceRetry { stop_when_user_paused: bool },
}

impl PlaybackPurpose {
    const fn stop_when_user_paused(self) -> bool {
        match self {
            Self::Queue {
                stop_when_user_paused,
            }
            | Self::Playlist {
                stop_when_user_paused,
            }
            | Self::Pool {
                stop_when_user_paused,
            }
            | Self::SourceRetry {
                stop_when_user_paused,
            } => stop_when_user_paused,
            Self::Repeat => true,
            Self::Requested | Self::Previous => false,
        }
    }

    const fn checks_dedup(self) -> bool {
        matches!(self, Self::Requested | Self::Queue { .. })
    }

    const fn allows_source_switch(self) -> bool {
        matches!(
            self,
            Self::Requested
                | Self::Queue { .. }
                | Self::Playlist { .. }
                | Self::Pool { .. }
                | Self::Repeat
        )
    }

    const fn replies_with_play_message(self) -> bool {
        !matches!(
            self,
            Self::SourceRetry { .. } | Self::Playlist { .. } | Self::Pool { .. }
        )
    }

    /// 去重时展示给玩家的文案。
    ///
    /// 存模板键而不是成品字符串：真正渲染发生在回复出口（`finish_playback`），
    /// 那里才有 port 可以查询用户覆盖；在这里拼成字符串会把覆盖绕过去。
    fn dedup_reply(self, selection: &PlaybackSelection) -> Option<ReplyMessage> {
        match self {
            Self::Requested => Some(ReplyMessage {
                spec: &playback_templates::DEDUP_RECENTLY_PLAYED,
                vars: selection.dedup_vars(),
            }),
            Self::Queue { .. } => Some(ReplyMessage {
                spec: &playback_templates::DEDUP_SKIPPED,
                vars: selection.dedup_vars(),
            }),
            Self::Previous
            | Self::SourceRetry { .. }
            | Self::Playlist { .. }
            | Self::Pool { .. }
            | Self::Repeat => None,
        }
    }
}

/// 一条待发送的回复：模板 + 变量。
///
/// 播放链路的回复要到出口（`finish_playback`）才发送，因此这里携带模板本身，
/// 由出口用 port 渲染——这样配置覆盖对所有分支一致生效。
struct ReplyMessage {
    spec: &'static ReplyTemplate,
    vars: ReplyVars,
}

impl ReplyMessage {
    fn new(spec: &'static ReplyTemplate, vars: ReplyVars) -> Self {
        Self { spec, vars }
    }
}

struct PlaybackCompletion {
    result: PlaybackResult,
    /// 原样透传的文案（播放回执、错误原因等，文案源头不在本模块）。
    reply: Option<String>,
    /// 走模板渲染的文案；与 `reply` 互斥，两者最多一个为 Some。
    reply_template: Option<ReplyMessage>,
    update_monitor: bool,
}

impl PlaybackCompletion {
    fn new(result: PlaybackResult, reply: Option<String>, update_monitor: bool) -> Self {
        Self {
            result,
            reply,
            reply_template: None,
            update_monitor,
        }
    }

    fn templated(mut self, spec: &'static ReplyTemplate, vars: ReplyVars) -> Self {
        self.reply = None;
        self.reply_template = Some(ReplyMessage::new(spec, vars));
        self
    }
}

pub(crate) trait PlaybackExecutionPort {
    fn reply(&mut self, message: &str) -> Result<()>;

    /// 把**内部归属键**翻译成面向玩家的展示名。
    ///
    /// 歌单归属键是 `id:<uuid>` 形态（见 `playlist::owner_key`），它是内部标识，
    /// **绝不能直接渲染给玩家**——玩家会看到「[id:00000000-…的歌单] 播放中」。
    ///
    /// 翻译需要身份映射（配置层对象），而 features 层不能反向依赖配置，
    /// 所以由实现方（composition 层）覆写这个方法完成解析；
    /// 默认实现原样返回，保证不接身份的调用方（测试替身）行为不变。
    fn playlist_owner_display(&self, owner: &str) -> String {
        owner.to_string()
    }

    /// 渲染一条模板，返回成品文案（不发送）。
    ///
    /// 业务层只声明"用哪条模板、带哪些变量"，**不知道配置存在**；
    /// 覆盖文案的查找由实现方（composition 层）负责，这样 features 层
    /// 不会反向依赖配置中心。
    ///
    /// 单独拆出"只渲染不发送"，是给需要在一条消息里拼多段模板的调用点用的
    /// （如完整队列的分批消息）；默认实现按内置文案渲染，保证不接配置的
    /// 调用方（测试替身）也能工作。
    fn render_reply(&mut self, spec: &ReplyTemplate, vars: &ReplyVars) -> String {
        reply_template::render(spec, None, vars).text
    }

    /// 按模板键回复。
    fn reply_template(&mut self, spec: &ReplyTemplate, vars: &ReplyVars) -> Result<()> {
        let text = self.render_reply(spec, vars);
        self.reply(&text)
    }

    fn update_monitor(&mut self);
    fn search_and_pick(
        &mut self,
        keyword: &str,
        source: &str,
        prefer_accompaniment: bool,
    ) -> std::result::Result<Option<PlaybackPickedCandidate>, PlaybackSearchFailure>;
    fn song_dedup_limited(&mut self, request: &PlaybackRequest) -> Result<bool>;
    fn play_and_verify(&mut self, request: &PlaybackRequest) -> Result<PlaybackVerification>;
    fn is_track_unavailable_error(&self, _error: &anyhow::Error) -> bool {
        false
    }
    fn item_scoped_playback_error_message(&self, _error: &anyhow::Error) -> Option<&'static str> {
        None
    }
    fn player_status(&mut self) -> Result<PlayerStatus>;
    fn playback_queue(&mut self) -> Result<Vec<QueueItem>>;
    fn enqueue_playlist_item(&mut self, _item: QueueItem) -> Result<bool> {
        Ok(false)
    }
    fn remove_playlist_auto_items(&mut self) -> Result<()> {
        Ok(())
    }
    /// 从播放池随机挑一首，排除本轮已尝试的曲目。
    fn pick_playback_pool_track(
        &mut self,
        _excluded: &HashSet<TrackKey>,
    ) -> Result<Option<PlayableTrack>> {
        Ok(None)
    }
    fn remove_playback_queue(&mut self, removal: QueueRemoval) -> Result<()>;
    fn user_pause_active(&mut self) -> Result<bool> {
        Ok(false)
    }
    /// 值守器安排的队列推进是否可以开始下一首。手动播放命令使用不检查此闸门的用途。
    fn automatic_queue_advance_allowed(&mut self) -> Result<bool> {
        Ok(true)
    }
    /// 预加载曲目音源解析（后台、尽力而为；失败静默，播放时重新解析）。
    fn preload_track(&mut self, _track: &PlayableTrack) -> Result<()> {
        Ok(())
    }
}

pub(crate) trait PlaybackCommandPort: PlaybackExecutionPort {
    fn reply_batch(&mut self, messages: &[String], delay_ms: u64) -> Result<()>;
    fn log_executed(&mut self, context: &PlaybackCommandContext, final_command: &str)
    -> Result<()>;
    fn pause_by_user(&mut self) -> Result<String>;
    fn resume_by_user(&mut self) -> Result<String>;
    fn previous_playback_request(&mut self) -> Result<Option<PlaybackRequest>> {
        Ok(None)
    }
    fn set_volume(&mut self, volume: &str) -> Result<()>;
    fn remove_playback_pool_track(&mut self, key: &TrackKey) -> Result<bool>;
    fn toggle_lyrics(&mut self) -> Result<String>;
    fn remove_playback_queue_indexes(
        &mut self,
        indexes: Vec<usize>,
    ) -> Result<Vec<(usize, QueueItem)>>;
    fn clear_playback_queue(&mut self) -> Result<usize>;
    /// 排队的正式任务需要中断连续歌词时返回 true。
    fn should_stop_continuous_lyrics(&mut self) -> Result<bool> {
        Ok(false)
    }
    fn start_background_lyrics(
        &mut self,
        _duration: Option<Duration>,
        _scope: BackgroundLyricsScope,
    ) -> Result<bool> {
        Ok(false)
    }
    fn stop_background_lyrics(&mut self) -> Result<bool> {
        Ok(false)
    }
    fn wait(&mut self, duration: Duration);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlaybackWorkload {
    pub(crate) has_pending_playback_task: bool,
    pub(crate) command_executing: bool,
}

pub(crate) trait PlaybackMonitorPort {
    fn now(&self) -> Instant;
    fn is_running(&self) -> bool;
    fn is_paused(&self) -> bool;
    fn wait(&mut self, duration: Duration);
    fn player_status(&mut self) -> Result<PlayerStatus>;
    fn playback_queue(&mut self) -> Result<Vec<QueueItem>>;
    fn workload(&mut self) -> Result<PlaybackWorkload>;
    fn maybe_advance_queue(
        &mut self,
        status: PlayerStatus,
        context: QueueAdvanceContext,
    ) -> Result<QueueAdvanceDecision>;
    fn enqueue_advance_queue(&mut self, reason: &'static str) -> Result<()>;
    fn update_monitor(&mut self);
}

#[derive(Clone, Debug)]
pub(crate) struct PlaybackApplicationConfig {
    pub(crate) console_bypass_dedup: bool,
    pub(crate) queue_max_size: usize,
    /// 热更新共享句柄（阶段 7）：保存 timing.playback.monitor_tick_ms 后立即生效。
    pub(crate) monitor_tick_ms: Arc<RwLock<u64>>,
    /// 热更新共享句柄（阶段 7）：保存 timing.playback.monitor_status_ms 后立即生效。
    pub(crate) monitor_status_ms: Arc<RwLock<u64>>,
    pub(crate) help_batch_ms: u64,
}

/// 播放模式:顺序播放。
pub(crate) const PLAY_MODE_SEQUENTIAL: u8 = 0;
/// 播放模式:单曲循环(重复当前曲目,不消费队列)。
pub(crate) const PLAY_MODE_REPEAT_ONE: u8 = 1;
/// 播放模式:随机(仅作用于队列耗尽后的歌曲池)。
pub(crate) const PLAY_MODE_SHUFFLE: u8 = 2;

#[derive(Clone, Debug)]
struct PersonalPlaylistQueue {
    /// 激活该模式的昵称；用于「当前为谁的歌单」提示与状态注明。
    owner: String,
    tracks: Vec<PlayableTrack>,
    cursor: usize,
    shuffle_state: u64,
    last_played_key: Option<TrackKey>,
}

#[derive(Clone, Debug)]
pub(crate) struct PlaybackApplication {
    config: PlaybackApplicationConfig,
    /// 播放模式(Web 运行期设置,不持久化)。
    play_mode: Arc<AtomicU8>,
    /// 最近一次成功播放的选择(单曲循环重播用)。
    last_played: Arc<Mutex<Option<PlaybackRequest>>>,
    /// 个人歌单自动填充队列；激活时不从默认播放池续播。
    personal_playlist: Arc<Mutex<Option<PersonalPlaylistQueue>>>,
    /// 当前正在播放的曲目是否来自个人歌单自动队列；@状态 据此决定是否标注归属。
    current_playlist_auto: Arc<AtomicBool>,
    /// 队首 QueueBlockingFailure 的连续失败计数（item id -> (次数, 首次失败时刻)）。
    /// 超限后按项目级失败丢弃，避免队首永久毒化整条队列。
    queue_block_failures: Arc<Mutex<HashMap<u64, (u32, Instant)>>>,
}

#[derive(Default)]
pub(crate) struct LyricTracker {
    current_key: Option<TrackKey>,
    previous_lyric: Option<String>,
}

impl LyricTracker {
    pub(crate) fn observe(&mut self, status: &PlayerStatus) -> bool {
        if status.status != "playing" {
            return false;
        }
        let key = status
            .current_track
            .as_ref()
            .map(|track| &track.track_ref.key);
        if let Some(key) = key
            && self.current_key.as_ref() != Some(key)
        {
            self.current_key = Some(key.clone());
            self.previous_lyric = None;
        }

        let lyric = status.lyric_line_text.trim();
        if lyric.is_empty() {
            self.previous_lyric = None;
            return false;
        }
        if self.previous_lyric.as_deref() == Some(lyric) {
            return false;
        }
        self.previous_lyric = Some(lyric.to_string());
        true
    }
}

impl PersonalPlaylistQueue {
    fn next_track_excluding(&mut self, queued: &HashSet<TrackKey>) -> Option<PlayableTrack> {
        for _ in 0..self.tracks.len() {
            if self.cursor >= self.tracks.len() {
                self.cursor = 0;
                for index in (1..self.tracks.len()).rev() {
                    self.shuffle_state = self
                        .shuffle_state
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    let swap = (self.shuffle_state as usize) % (index + 1);
                    self.tracks.swap(index, swap);
                }
            }
            let track = self.tracks[self.cursor].clone();
            self.cursor += 1;
            if !queued.contains(&track.track_ref.key) {
                return Some(track);
            }
        }
        None
    }
}

fn playlist_queue_item(track: PlayableTrack) -> QueueItem {
    QueueItem {
        keyword: track.metadata.title.clone(),
        source: track.track_ref.key.provider.as_str().to_owned(),
        track: Some(track),
        playlist_auto: true,
        ..QueueItem::default()
    }
}

impl PlaybackApplication {
    pub(crate) fn new(config: PlaybackApplicationConfig) -> Self {
        Self {
            config,
            play_mode: Arc::new(AtomicU8::new(PLAY_MODE_SHUFFLE)),
            last_played: Arc::new(Mutex::new(None)),
            personal_playlist: Arc::new(Mutex::new(None)),
            current_playlist_auto: Arc::new(AtomicBool::new(false)),
            queue_block_failures: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 记录一次队首 QueueBlockingFailure；返回是否已达重试上限。
    fn record_queue_block_failure(&self, item_id: u64) -> bool {
        const MAX_CONSECUTIVE: u32 = 3;
        const WINDOW: Duration = Duration::from_secs(300);
        let now = Instant::now();
        let mut failures = self
            .queue_block_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = failures.entry(item_id).or_insert((0, now));
        if now.duration_since(entry.1) > WINDOW {
            *entry = (0, now);
        }
        entry.0 = entry.0.saturating_add(1);
        entry.0 >= MAX_CONSECUTIVE
    }

    fn clear_queue_block_failure(&self, item_id: u64) {
        if let Ok(mut failures) = self.queue_block_failures.lock() {
            failures.remove(&item_id);
        }
    }

    pub(crate) fn start_personal_playlist<P: PlaybackExecutionPort + ?Sized>(
        &self,
        owner: &str,
        tracks: Vec<PlayableTrack>,
        port: &mut P,
    ) -> Result<PersonalPlaylistStart> {
        // 幂等：已在歌单模式时不重建队列、不重排随机顺序、不重复入队。
        if let Some(current) = self.personal_playlist_owner() {
            return Ok(PersonalPlaylistStart::AlreadyActive { owner: current });
        }
        if tracks.is_empty() {
            return Ok(PersonalPlaylistStart::Started(0));
        }
        port.remove_playlist_auto_items()?;
        let shuffle_state = tracks
            .iter()
            .fold(0x9E37_79B9_7F4A_7C15u64, |state, track| {
                track
                    .track_ref
                    .key
                    .id
                    .bytes()
                    .fold(state, |hash, byte| hash.rotate_left(5) ^ u64::from(byte))
            });
        *self
            .personal_playlist
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(PersonalPlaylistQueue {
            owner: owner.to_owned(),
            tracks,
            cursor: 0,
            shuffle_state,
            last_played_key: None,
        });
        let queued = self.refill_personal_playlist_queue(port)?;
        Ok(PersonalPlaylistStart::Started(queued))
    }

    pub(crate) fn stop_personal_playlist<P: PlaybackExecutionPort + ?Sized>(
        &self,
        port: &mut P,
    ) -> Result<bool> {
        // 一次加锁完成「判定 + 清状态」，再去清队列。
        //
        // 原来是 check-then-act：先读 active（锁已释放）、再清队列、最后才 take。
        // 窗口里 start 可能刚写入新模式并 `refill_personal_playlist_queue` 入队，
        // 随后的 remove_playlist_auto_items 会把对方刚入队的自动项清掉，
        // 而模式仍然激活 —— 歌单看起来在跑但没有下一首。
        //
        // 顺序也反过来了：先摘掉模式，再清队列，避免中间态「模式还在、队列已空」。
        let was_active = self
            .personal_playlist
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .is_some();
        port.remove_playlist_auto_items()?;
        Ok(was_active)
    }

    fn personal_playlist_active(&self) -> bool {
        self.personal_playlist_owner().is_some()
    }

    /// 当前歌单模式的归属昵称；未激活时为 None。
    fn personal_playlist_owner(&self) -> Option<String> {
        self.personal_playlist
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|queue| queue.owner.clone())
    }

    /// 场景切换（换大厅/进千星/进入游戏）结束个人歌单模式；返回被结束的归属昵称。
    /// 只清内存模式，残留自动项由调用方清理。
    pub(crate) fn take_personal_playlist_mode(&self) -> Option<String> {
        self.personal_playlist
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .map(|queue| queue.owner)
    }

    /// 当前歌单模式的归属；未激活为 None（端口用）。
    pub(crate) fn playlist_mode_owner(&self) -> Option<String> {
        self.personal_playlist_owner()
    }

    /// owner 与当前模式匹配时，把新曲目并入运行中的随机池（导入用）；返回是否有新增。
    pub(crate) fn extend_personal_playlist_tracks(
        &self,
        owner: &str,
        tracks: &[PlayableTrack],
    ) -> bool {
        let mut guard = self
            .personal_playlist
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(mode) = guard.as_mut() else {
            return false;
        };
        if mode.owner != owner {
            return false;
        }
        let mut added = false;
        for track in tracks {
            if !mode
                .tracks
                .iter()
                .any(|existing| existing.track_ref.key == track.track_ref.key)
            {
                mode.tracks.push(track.clone());
                added = true;
            }
        }
        added
    }

    fn current_is_playlist_auto(&self) -> bool {
        self.current_playlist_auto.load(Ordering::Relaxed)
    }

    /// 记录本次起播来源，供 @状态 判断是否标注歌单归属。
    fn note_playback_purpose(&self, purpose: PlaybackPurpose) {
        self.current_playlist_auto.store(
            matches!(purpose, PlaybackPurpose::Playlist { .. }),
            Ordering::Relaxed,
        );
    }

    fn refill_personal_playlist_queue<P: PlaybackExecutionPort + ?Sized>(
        &self,
        port: &mut P,
    ) -> Result<usize> {
        let mut queue = port.playback_queue()?;
        // 先在锁内算出**要补哪些歌**，再出锁逐条入队。
        //
        // 原实现在锁内直接调 `port.enqueue_playlist_item` 落盘：
        // 一把进程级互斥锁跨越磁盘写入，落盘一慢，
        // playlist_mode_owner / take_personal_playlist_mode /
        // extend_personal_playlist_tracks 全部被拖住，命令线程跟着停摆。
        let pending = {
            let mut mode = self
                .personal_playlist
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(mode) = mode.as_mut() else {
                return Ok(0);
            };
            let max_auto = mode.tracks.len().min(self.config.queue_max_size);
            let mut pending = Vec::new();
            while queue.len() + pending.len() < self.config.queue_max_size
                && queue.iter().filter(|item| item.playlist_auto).count() + pending.len() < max_auto
            {
                let mut queued_keys = queue
                    .iter()
                    .filter_map(|item| item.track.as_ref().map(|track| track.track_ref.key.clone()))
                    .collect::<HashSet<_>>();
                queued_keys.extend(pending.iter().filter_map(|item: &QueueItem| {
                    item.track.as_ref().map(|track| track.track_ref.key.clone())
                }));
                if mode.tracks.len() > 1
                    && let Some(last_played) = &mode.last_played_key
                {
                    queued_keys.insert(last_played.clone());
                }
                let Some(track) = mode.next_track_excluding(&queued_keys) else {
                    break;
                };
                pending.push(playlist_queue_item(track));
            }
            pending
        };
        // 锁外落盘。任一条失败就停手：与原来的 break 语义一致。
        let mut added = 0;
        for item in pending {
            if !port.enqueue_playlist_item(item)? {
                break;
            }
            added += 1;
        }
        Ok(added)
    }

    pub(crate) fn play_mode(&self) -> u8 {
        match self.play_mode.load(Ordering::Relaxed) {
            PLAY_MODE_REPEAT_ONE => PLAY_MODE_REPEAT_ONE,
            PLAY_MODE_SHUFFLE => PLAY_MODE_SHUFFLE,
            _ => PLAY_MODE_SEQUENTIAL,
        }
    }

    /// 供 Web 面板共享同一个模式句柄(facade 通过它读写模式)。
    pub(crate) fn play_mode_handle(&self) -> Arc<AtomicU8> {
        self.play_mode.clone()
    }

    pub(crate) fn execute_command<P: PlaybackCommandPort + ?Sized>(
        &self,
        context: &PlaybackCommandContext,
        command: &PlaybackCommand,
        port: &mut P,
    ) -> Result<()> {
        match command {
            PlaybackCommand::Pause => {
                // 内置播放器后端只返回 ack 字符串（"ok"），固定回复友好文案。
                port.pause_by_user()?;
                port.log_executed(context, "pause")?;
                port.update_monitor();
                port.reply_template(&playback_templates::PAUSE_OK, &ReplyVars::new())?;
            }
            PlaybackCommand::Resume | PlaybackCommand::Play => {
                port.resume_by_user()?;
                port.log_executed(context, "resume")?;
                port.update_monitor();
                port.reply_template(&playback_templates::RESUME_OK, &ReplyVars::new())?;
            }
            PlaybackCommand::Next => {
                // 严格按播放列表：队列有歌播队列第一首（播放确认消息已回复）；
                // 队列空才落到播放池随机续播，并明确标注来源；都空明确告知。
                match self.consume_queue("手动下一首", port)? {
                    Some(ConsumedSelection::Queue { .. }) => {}
                    Some(ConsumedSelection::Pool { keyword }) => {
                        port.reply_template(
                            &playback_templates::NEXT_POOL_FALLBACK,
                            &ReplyVars::new().with("关键词", keyword),
                        )?;
                    }
                    None => {
                        port.reply_template(&playback_templates::NEXT_EMPTY, &ReplyVars::new())?;
                    }
                }
                port.log_executed(context, "next queue")?;
            }
            PlaybackCommand::DeleteCurrentPoolTrack => {
                let status = port.player_status()?;
                let Some(track) = status.current_track else {
                    port.log_executed(context, "delete current pool track unavailable")?;
                    port.reply_template(
                        &playback_templates::DELETE_TRACK_UNAVAILABLE,
                        &ReplyVars::new(),
                    )?;
                    return Ok(());
                };
                let removed = port.remove_playback_pool_track(&track.track_ref.key)?;
                port.log_executed(
                    context,
                    if removed {
                        "delete current pool track"
                    } else {
                        "delete current pool track missing; next"
                    },
                )?;
                match self.consume_queue("删除当前歌曲并下一曲", port)? {
                    Some(ConsumedSelection::Queue { .. }) => {}
                    Some(ConsumedSelection::Pool { keyword }) => {
                        let spec = if removed {
                            &playback_templates::DELETE_REMOVED_POOL_FALLBACK
                        } else {
                            &playback_templates::DELETE_NOT_IN_POOL_POOL_FALLBACK
                        };
                        port.reply_template(spec, &ReplyVars::new().with("关键词", keyword))?;
                    }
                    None => {
                        let spec = if removed {
                            &playback_templates::DELETE_REMOVED_QUEUE_EMPTY
                        } else {
                            &playback_templates::DELETE_NOT_IN_POOL_QUEUE_EMPTY
                        };
                        port.reply_template(spec, &ReplyVars::new())?;
                    }
                }
            }
            PlaybackCommand::Previous => {
                if let Some(request) = port.previous_playback_request()? {
                    let completion =
                        self.run_request(&request, &request, PlaybackPurpose::Previous, port)?;
                    self.finish_playback(completion, port)?;
                    port.log_executed(context, "previous uri")?;
                } else {
                    // 无历史请求：没有可回退的上一曲，明确告知，不转发给引擎。
                    port.update_monitor();
                    port.log_executed(context, "previous")?;
                    port.reply_template(&playback_templates::PREVIOUS_MISSING, &ReplyVars::new())?;
                }
            }
            PlaybackCommand::Volume(volume) => {
                port.set_volume(volume)?;
                port.log_executed(context, &format!("volume {}", volume))?;
                port.reply_template(
                    &playback_templates::VOLUME_SET,
                    &ReplyVars::new().with("音量", volume.to_string()),
                )?;
            }
            PlaybackCommand::Status => {
                let status = port.player_status()?;
                port.log_executed(context, "status")?;
                // 只对确实来自个人歌单自动队列的当前曲目标注归属；大厅点歌/窗口选曲保留点歌人。
                // 归属键是 `id:<uuid>` 内部形态，必须先翻成展示名再渲染：
                // 否则玩家会看到「[id:00000000-…的歌单] 播放中」。
                let playlist_owner = if self.current_is_playlist_auto() {
                    self.personal_playlist_owner()
                        .map(|owner| port.playlist_owner_display(&owner))
                } else {
                    None
                };
                let text = format_status_with_mode(&status, playlist_owner.as_deref());
                port.reply(&text)?;
            }
            PlaybackCommand::Lyrics => {
                let status = port.player_status()?;
                port.log_executed(context, "lyrics")?;
                port.reply(&format_lyrics(&status))?;
            }
            PlaybackCommand::ToggleLyrics => {
                let mode = port.toggle_lyrics()?;
                port.log_executed(context, "lyrics toggle")?;
                let spec = if mode == "translation" {
                    &playback_templates::LYRICS_TOGGLE_TRANSLATION
                } else {
                    &playback_templates::LYRICS_TOGGLE_ORIGINAL
                };
                port.reply_template(spec, &ReplyVars::new())?;
            }
            PlaybackCommand::LyricsFor(seconds) => {
                let started = port.start_background_lyrics(
                    Some(Duration::from_secs(u64::from(*seconds))),
                    BackgroundLyricsScope::AllSongs,
                )?;
                port.log_executed(context, &format!("lyrics {}s", seconds))?;
                let spec = if started {
                    &playback_templates::LYRICS_TIMED_STARTED
                } else {
                    &playback_templates::LYRICS_ALREADY_RUNNING
                };
                port.reply_template(spec, &ReplyVars::new().with("秒数", seconds.to_string()))?;
            }
            PlaybackCommand::ContinuousLyrics => {
                port.stop_background_lyrics()?;
                port.log_executed(context, "lyrics continuous")?;
                self.reply_continuous_lyrics(port)?;
            }
            PlaybackCommand::BackgroundLyrics => {
                let started =
                    port.start_background_lyrics(None, BackgroundLyricsScope::AllSongs)?;
                port.log_executed(context, "lyrics background")?;
                let spec = if started {
                    &playback_templates::LYRICS_BACKGROUND_STARTED
                } else {
                    &playback_templates::LYRICS_ALREADY_RUNNING
                };
                port.reply_template(spec, &ReplyVars::new())?;
            }
            PlaybackCommand::SingleSongLyrics => {
                let started =
                    port.start_background_lyrics(None, BackgroundLyricsScope::CurrentSong)?;
                port.log_executed(context, "lyrics single song")?;
                let spec = if started {
                    &playback_templates::LYRICS_SINGLE_SONG_STARTED
                } else {
                    &playback_templates::LYRICS_ALREADY_RUNNING
                };
                port.reply_template(spec, &ReplyVars::new())?;
            }
            PlaybackCommand::StopBackgroundLyrics => {
                let stopped = port.stop_background_lyrics()?;
                port.log_executed(context, "lyrics background stop")?;
                let spec = if stopped {
                    &playback_templates::LYRICS_BACKGROUND_STOPPED
                } else {
                    &playback_templates::LYRICS_BACKGROUND_NOT_RUNNING
                };
                port.reply_template(spec, &ReplyVars::new())?;
            }
            PlaybackCommand::Queue => {
                port.log_executed(context, "queue list")?;
                self.reply_full_queue(port)?;
            }
            PlaybackCommand::QueueDelete(indexes) => {
                if indexes.is_empty() {
                    port.log_executed(context, "queue delete invalid")?;
                    port.reply_template(
                        &playback_templates::QUEUE_DELETE_INVALID,
                        &ReplyVars::new(),
                    )?;
                    return Ok(());
                }
                let removed = port.remove_playback_queue_indexes(indexes.clone())?;
                if removed.is_empty() {
                    port.log_executed(context, "queue delete none")?;
                    port.reply_template(
                        &playback_templates::QUEUE_DELETE_MISSING,
                        &ReplyVars::new(),
                    )?;
                } else {
                    let removed_text = removed
                        .iter()
                        .map(|(index, item)| format!("{}.{}", index, item.keyword))
                        .collect::<Vec<_>>()
                        .join(", ");
                    port.log_executed(context, &format!("queue delete {}", removed_text))?;
                    port.reply_template(
                        &playback_templates::QUEUE_DELETE_DONE,
                        &ReplyVars::new().with("条目", removed_text),
                    )?;
                }
            }
            PlaybackCommand::QueueClear => {
                let count = port.clear_playback_queue()?;
                port.log_executed(context, &format!("queue clear {}", count))?;
                if count == 0 {
                    port.reply_template(&playback_templates::QUEUE_EMPTY, &ReplyVars::new())?;
                } else {
                    port.reply_template(
                        &playback_templates::QUEUE_CLEARED,
                        &ReplyVars::new().with("数量", count.to_string()),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn reply_full_queue<P: PlaybackCommandPort + ?Sized>(&self, port: &mut P) -> Result<()> {
        let queue = port.playback_queue()?;
        if queue.is_empty() {
            return port.reply_template(&playback_templates::QUEUE_EMPTY, &ReplyVars::new());
        }

        let header_spec = &playback_templates::QUEUE_HEADER;
        let entry_spec = &playback_templates::QUEUE_ENTRY;
        let mut render = |spec: &ReplyTemplate, vars: &ReplyVars| port.render_reply(spec, vars);
        let messages = split_full_queue_messages(
            &queue,
            self.config.queue_max_size,
            header_spec,
            entry_spec,
            &mut render,
        );
        port.reply_batch(&messages, self.config.help_batch_ms)
    }

    fn reply_continuous_lyrics<P: PlaybackCommandPort + ?Sized>(&self, port: &mut P) -> Result<()> {
        let poll = Duration::from_millis(
            self.config
                .monitor_status_ms
                .read()
                .expect("播放状态校准间隔共享锁已中毒")
                .max(1),
        );
        let mut lyric_tracker = LyricTracker::default();

        loop {
            if port.should_stop_continuous_lyrics()? {
                log::debug!("持续歌词推送已结束（播放已停止或模式已切换）");
                break;
            }

            match port.player_status() {
                Ok(status) => {
                    if lyric_tracker.observe(&status) {
                        port.reply(&format_lyrics(&status))?;
                    }
                }
                Err(error) => {
                    log::warn!(
                        "持续歌词推送时读取播放器状态失败（步骤：每轮歌词刷新）；本轮跳过，下一轮继续：{error:#}"
                    );
                }
            }

            port.wait(poll);
        }
        Ok(())
    }

    pub(crate) fn run_monitor_loop<P: PlaybackMonitorPort + ?Sized>(&self, port: &mut P) {
        let mut snapshot: Option<PlaybackSnapshot> = None;
        let mut next_status_at = port.now();
        while port.is_running() {
            // 每轮重读热更新共享值：运行中保存 monitor_tick_ms/monitor_status_ms
            // 立即影响轮询间隔与状态校准间隔（钳制语义与启动时一致）。
            let tick_ms = self
                .config
                .monitor_tick_ms
                .read()
                .expect("播放监控循环间隔共享锁已中毒")
                .max(50);
            let status_ms = self
                .config
                .monitor_status_ms
                .read()
                .expect("播放状态校准间隔共享锁已中毒")
                .max(tick_ms);
            if port.is_paused() {
                port.wait(Duration::from_millis(tick_ms));
                continue;
            }
            let now = port.now();
            if snapshot.is_none() || now >= next_status_at {
                match port.player_status() {
                    Ok(status) => {
                        snapshot = Some(PlaybackSnapshot {
                            status,
                            captured_at: now,
                        });
                        next_status_at = now + Duration::from_millis(status_ms);
                    }
                    Err(error) => {
                        log::error!(
                            "播放监控查询播放器状态失败（步骤：定期校准播放进度）；本轮不校准，下次轮询重试，期间进度显示可能滞后：{error:#}"
                        );
                        snapshot = None;
                        next_status_at = now + Duration::from_millis(status_ms);
                    }
                }
            }
            if let Some(playback_snapshot) = snapshot.as_ref() {
                match self.handle_monitor_snapshot(playback_snapshot, port) {
                    Ok(true) => {
                        let now = port.now();
                        snapshot = port.player_status().ok().map(|status| PlaybackSnapshot {
                            status,
                            captured_at: now,
                        });
                        next_status_at = now + Duration::from_millis(status_ms);
                    }
                    Ok(false) => {}
                    Err(error) => {
                        log::error!(
                            "播放监控处理本轮观测失败（步骤：解析播放状态并决定是否推进队列）；本轮不推进，下次轮询重试：{error:#}"
                        );
                        next_status_at = port.now() + Duration::from_millis(status_ms);
                    }
                }
            }
            port.wait(Duration::from_millis(tick_ms));
        }
    }

    pub(crate) fn handle_monitor_snapshot<P: PlaybackMonitorPort + ?Sized>(
        &self,
        snapshot: &PlaybackSnapshot,
        port: &mut P,
    ) -> Result<bool> {
        let workload = port.workload()?;
        let context = QueueAdvanceContext {
            queue_empty: port.playback_queue()?.is_empty(),
            has_pending_playback_task: workload.has_pending_playback_task,
            command_executing: workload.command_executing,
        };
        let decision = port.maybe_advance_queue(estimated_player_status(snapshot), context)?;
        port.update_monitor();
        match decision {
            QueueAdvanceDecision::None => Ok(false),
            QueueAdvanceDecision::PlaybackStateChanged => Ok(true),
            QueueAdvanceDecision::AdvanceQueue { reason } => {
                port.enqueue_advance_queue(reason)?;
                Ok(true)
            }
        }
    }

    /// 消费队列：队列有歌播队列，队列空时从播放池随机续播。
    /// 返回成功播放的歌曲（含来源），无内容可播时返回 None，供调用方明确回复。
    pub(crate) fn consume_queue<P: PlaybackExecutionPort + ?Sized>(
        &self,
        reason: &str,
        port: &mut P,
    ) -> Result<Option<ConsumedSelection>> {
        self.consume_queue_inner(
            reason,
            PlaybackPurpose::Queue {
                stop_when_user_paused: false,
            },
            port,
        )
    }

    pub(crate) fn consume_queue_after_monitor<P: PlaybackExecutionPort + ?Sized>(
        &self,
        reason: &str,
        port: &mut P,
    ) -> Result<Option<ConsumedSelection>> {
        self.consume_queue_inner(
            reason,
            PlaybackPurpose::Queue {
                stop_when_user_paused: true,
            },
            port,
        )
    }

    fn consume_queue_inner<P: PlaybackExecutionPort + ?Sized>(
        &self,
        reason: &str,
        purpose: PlaybackPurpose,
        port: &mut P,
    ) -> Result<Option<ConsumedSelection>> {
        // 播放池随机播放时排除本轮所有已尝试歌曲，保证候选有限耗尽。
        let mut pool_excluded = HashSet::new();
        // 本轮个人歌单已尝试且失败的曲目（按曲目身份），防止 refill 无限回灌。
        let mut playlist_attempted: HashSet<String> = HashSet::new();
        loop {
            if purpose.stop_when_user_paused() && port.user_pause_active()? {
                log::debug!("暂不自动出队：播放器处于用户暂停状态");
                return Ok(None);
            }
            if purpose.stop_when_user_paused() && !port.automatic_queue_advance_allowed()? {
                log::debug!("暂不自动出队：配置重载尚未处理完");
                return Ok(None);
            }
            self.refill_personal_playlist_queue(port)?;
            // 歌单项项目级失败后会被 refill 回灌；剔除本轮已尝试的曲目，保证循环收敛。
            if self.personal_playlist_active() && !playlist_attempted.is_empty() {
                for item in port.playback_queue()? {
                    if item.playlist_auto
                        && playlist_attempted.contains(&playlist_attempt_key(&item))
                    {
                        port.remove_playback_queue(QueueRemoval::Id(item.id))?;
                    }
                }
            }
            let queue = port.playback_queue()?;
            // 点歌队列独立于播放模式，始终优先按入队顺序消费。
            // 只有队列为空时，单曲循环才作用于自动续播。
            if queue.is_empty()
                && !self.personal_playlist_active()
                && purpose.stop_when_user_paused()
                && self.play_mode() == PLAY_MODE_REPEAT_ONE
            {
                let last = self
                    .last_played
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if let Some(selection) = last {
                    log::info!(
                        "单曲循环：重新播放「{}」（触发原因：{}）",
                        selection.keyword,
                        reason
                    );
                    let completion =
                        self.run_request(&selection, &selection, PlaybackPurpose::Repeat, port)?;
                    match self.finish_playback(completion, port)?.outcome() {
                        PlaybackOutcome::Success => {
                            return Ok(Some(ConsumedSelection::Queue {
                                keyword: selection.keyword,
                            }));
                        }
                        PlaybackOutcome::QueueBlockingFailure => {
                            log::warn!(
                                "单曲循环重播「{}」被阻断（队列阻塞失败），保留当前队列状态等待下次重试",
                                selection.keyword
                            );
                            return Ok(None);
                        }
                        _ => {
                            log::warn!(
                                "单曲循环重播「{}」失败，已清除重播记忆；下一轮改播队列中的曲目",
                                selection.keyword
                            );
                            *self
                                .last_played
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                            continue;
                        }
                    }
                }
            }
            let item = queue.into_iter().next();
            let Some(item) = item else {
                if self.personal_playlist_active() {
                    return Ok(None);
                }
                let Some(track) = port.pick_playback_pool_track(&pool_excluded)? else {
                    return Ok(None);
                };
                pool_excluded.insert(track.track_ref.key.clone());
                let pool_purpose = PlaybackPurpose::Pool {
                    stop_when_user_paused: purpose.stop_when_user_paused(),
                };
                let selection = pool_selection(&track);
                log::info!(
                    "队列为空，从播放池随机播放「{}」（触发原因：{}）",
                    selection.keyword,
                    reason
                );
                let completion = self.run_selection(&selection, pool_purpose, port)?;
                match self.finish_playback(completion, port)?.outcome() {
                    PlaybackOutcome::Success => {
                        return Ok(Some(ConsumedSelection::Pool {
                            keyword: selection.keyword,
                        }));
                    }
                    PlaybackOutcome::QueueBlockingFailure => return Ok(None),
                    _ => {
                        log::warn!(
                            "播放池随机抽到的「{}」不可播放，已跳过并继续抽下一首",
                            selection.keyword
                        );
                        continue;
                    }
                }
            };
            pool_excluded.clear();
            log::info!(
                "从队列取出「{}」开始播放（触发原因：{}）",
                item.keyword,
                reason
            );
            let request = PlaybackSelection {
                keyword: item.keyword.clone(),
                source: item.source.clone(),
                prefer_accompaniment: item.prefer_accompaniment,
                ai_original_text: item.ai_original_text.clone(),
                track: item.track.clone(),
                friend_username: item.friend_username.clone(),
                requester: item.requester.clone(),
                console_bypass_dedup: item.dedup_bypass,
                candidate_snapshot: item.candidate_snapshot.clone(),
                // 确认播放成功时与播放状态原子出队（同一笔持久化）。
                queue_item_id: Some(item.id),
            };
            let item_purpose = if item.playlist_auto {
                PlaybackPurpose::Playlist {
                    stop_when_user_paused: purpose.stop_when_user_paused(),
                }
            } else {
                purpose
            };
            let completion = self.run_selection(&request, item_purpose, port)?;
            let outcome = completion.result.outcome();
            if matches!(
                outcome,
                PlaybackOutcome::Success
                    | PlaybackOutcome::ItemScopedFailure
                    | PlaybackOutcome::DedupLimited
            ) {
                port.remove_playback_queue(QueueRemoval::Id(item.id))?;
            }
            let result = self.finish_playback(completion, port)?;
            match result.outcome() {
                PlaybackOutcome::Success => {
                    self.clear_queue_block_failure(item.id);
                    if self.personal_playlist_active()
                        && let Some(track) = &item.track
                        && let Some(mode) = self
                            .personal_playlist
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .as_mut()
                    {
                        mode.last_played_key = Some(track.track_ref.key.clone());
                    }
                    self.refill_personal_playlist_queue(port)?;
                    // 预加载后续曲目音源（解析 + 音频预下载），缩短下次切歌延迟；失败静默。
                    self.preload_next_track(port, &pool_excluded)?;
                    return Ok(Some(ConsumedSelection::Queue {
                        keyword: item.keyword,
                    }));
                }
                PlaybackOutcome::ItemScopedFailure => {
                    log::error!(
                        "队列中的「{}」不可播放（平台无音源），已从队列丢弃并继续下一首",
                        item.keyword
                    );
                    self.clear_queue_block_failure(item.id);
                    if item.playlist_auto {
                        playlist_attempted.insert(playlist_attempt_key(&item));
                    }
                }
                PlaybackOutcome::QueueBlockingFailure => {
                    log::error!(
                        "队列中的「{}」播放被阻断（非单曲问题），保留在队首等待重试",
                        item.keyword
                    );
                    // 连续失败超限的队首不再无限重试：丢弃并继续，避免毒化整条队列。
                    if self.record_queue_block_failure(item.id) {
                        log::error!(
                            "队列中的「{}」连续失败次数已达上限，已跳过以免阻塞整条队列",
                            item.keyword
                        );
                        port.remove_playback_queue(QueueRemoval::Id(item.id))?;
                        self.clear_queue_block_failure(item.id);
                        // 必须记进 playlist_attempted：否则循环顶部 refill 会拿**新 id**
                        // 把同一首回灌进队列，而失败计数以 id 为键，计数随之归零。
                        // 整张歌单都被阻断时，consume_queue_inner 会无退避、无终止地热循环。
                        if item.playlist_auto {
                            playlist_attempted.insert(playlist_attempt_key(&item));
                        }
                        continue;
                    }
                    return Ok(None);
                }
                PlaybackOutcome::DedupLimited => {
                    log::info!("队列中的「{}」近期已播放过，同歌去重规则跳过", item.keyword);
                    self.clear_queue_block_failure(item.id);
                    if item.playlist_auto {
                        playlist_attempted.insert(playlist_attempt_key(&item));
                    }
                }
            }
        }
    }

    pub(crate) fn play_confirmed<P: PlaybackExecutionPort + ?Sized>(
        &self,
        request: &PlaybackSelection,
        port: &mut P,
    ) -> Result<PlaybackResult> {
        let completion = self.run_selection(request, PlaybackPurpose::Requested, port)?;
        let result = self.finish_playback(completion, port)?;
        // 点歌确认播放成功后，同样预加载下一首（队列优先，空则池随机），
        // 连续播放时下一首直接命中本地缓存，避免起播等待。
        if result.outcome() == PlaybackOutcome::Success {
            self.preload_next_track(port, &HashSet::new())?;
        }
        Ok(result)
    }

    /// 直接播放一首已收藏的结构化曲目（个人歌单窗口里 `@1`~`@5` 选中后使用）。
    ///
    /// 与点歌确认走同一条 play_confirmed 路径，因此同样计入播放池与后续预加载。
    ///
    /// 注意：`@歌单随机` 不走这里——它把抽到的曲子**加入队列**（`enqueue_next`），
    /// 不打断当前播放。此前注释写成「@歌单随机 使用」是错的。
    pub(crate) fn play_structured_track<P: PlaybackExecutionPort + ?Sized>(
        &self,
        track: &PlayableTrack,
        port: &mut P,
    ) -> Result<()> {
        let selection = pool_selection(track);
        self.play_confirmed(&selection, port).map(|_| ())
    }

    /// 预加载下一首曲目音源：解析 + 音频数据预下载（启用缓存时），
    /// 失败静默（播放时重新解析）。
    fn preload_next_track<P: PlaybackExecutionPort + ?Sized>(
        &self,
        port: &mut P,
        pool_excluded: &HashSet<TrackKey>,
    ) -> Result<()> {
        let next = port
            .playback_queue()?
            .into_iter()
            .next()
            .and_then(|item| item.track)
            .or_else(|| {
                (!self.personal_playlist_active())
                    .then(|| port.pick_playback_pool_track(pool_excluded).ok().flatten())
                    .flatten()
            });
        if let Some(track) = next
            && let Err(error) = port.preload_track(&track)
        {
            log::debug!(
                "预加载下一首音源失败（步骤：提前解析并下载下一首）；不影响当前播放，切歌时会重新解析：{error:#}"
            );
        }
        Ok(())
    }

    fn run_selection<P: PlaybackExecutionPort + ?Sized>(
        &self,
        selection: &PlaybackSelection,
        purpose: PlaybackPurpose,
        port: &mut P,
    ) -> Result<PlaybackCompletion> {
        self.note_playback_purpose(purpose);
        let requested = selection.request();
        if purpose.stop_when_user_paused() && port.user_pause_active()? {
            log::debug!("暂不播放：进入播放流程前用户已暂停播放器");
            return Ok(PlaybackCompletion::new(
                PlaybackResult::error(&requested, &requested, None, "用户暂停"),
                None,
                false,
            ));
        }
        if purpose.checks_dedup() && self.selection_dedup_limited(selection, port)? {
            log::info!(
                "点歌「{}」未播放：该曲目短时间内已播过，同歌去重规则拦截",
                selection.keyword
            );
            let mut completion =
                PlaybackCompletion::new(PlaybackResult::dedup_limited(&requested), None, false);
            completion.reply_template = purpose.dedup_reply(selection);
            return Ok(completion);
        }
        if selection.track.is_none() {
            let source = if selection.source.trim().is_empty() {
                "qqmusic"
            } else {
                &selection.source
            };
            let picked = match port.search_and_pick(
                &selection.keyword,
                source,
                selection.prefer_accompaniment,
            ) {
                Ok(Some(picked)) => picked,
                Ok(None) => {
                    return Ok(PlaybackCompletion::new(
                        PlaybackResult::no_source(
                            &requested,
                            &requested,
                            None,
                            "平台无对应歌曲音源",
                        ),
                        None,
                        false,
                    )
                    .templated(&playback_templates::NO_SOURCE, ReplyVars::new()));
                }
                Err(error) => {
                    log::error!(
                        "点歌搜索失败（步骤：在平台搜索「{}」）；本次点歌未执行，玩家会收到失败提示：{error}",
                        selection.keyword
                    );
                    let message = ReplyMessage::new(
                        &playback_templates::SEARCH_FAILED,
                        ReplyVars::new()
                            .with("前缀", selection.label())
                            .with("原因", error.user_message()),
                    );
                    let mut completion = PlaybackCompletion::new(
                        PlaybackResult::error(&requested, &requested, None, error.to_string()),
                        None,
                        false,
                    );
                    completion.reply_template = Some(message);
                    return Ok(completion);
                }
            };
            log::debug!("平台搜索命中：把「{}」解析为可播放音源", picked.text);
            let mut resolved = selection.clone();
            resolved.keyword = picked.text;
            resolved.source = source.to_string();
            resolved.track = Some(picked.track);
            resolved.candidate_snapshot = picked.candidate_snapshot;
            return self.run_request(&requested, &resolved.request(), purpose, port);
        }
        self.run_request(&requested, &requested, purpose, port)
    }

    fn selection_dedup_limited<P: PlaybackExecutionPort + ?Sized>(
        &self,
        request: &PlaybackSelection,
        port: &mut P,
    ) -> Result<bool> {
        if request.console_bypass_dedup && self.config.console_bypass_dedup {
            return Ok(false);
        }
        port.song_dedup_limited(&request.request())
    }

    fn run_request<P: PlaybackExecutionPort + ?Sized>(
        &self,
        requested: &PlaybackRequest,
        request: &PlaybackRequest,
        purpose: PlaybackPurpose,
        port: &mut P,
    ) -> Result<PlaybackCompletion> {
        self.note_playback_purpose(purpose);
        if purpose.stop_when_user_paused() && port.user_pause_active()? {
            log::debug!("暂不播放：发送播放请求前用户已暂停播放器");
            return Ok(PlaybackCompletion::new(
                PlaybackResult::error(requested, request, None, "用户暂停"),
                None,
                false,
            ));
        }
        if purpose.stop_when_user_paused() && !port.automatic_queue_advance_allowed()? {
            log::debug!("暂不播放：发送播放请求前配置重载已生效，等待重载处理完");
            return Ok(PlaybackCompletion::new(
                PlaybackResult::error(requested, request, None, "配置重载待处理"),
                None,
                false,
            ));
        }
        let verification = match port.play_and_verify(request) {
            Ok(verification) => verification,
            Err(error) => {
                log::warn!(
                    "播放「{}」失败（步骤：向播放后端派发请求）；将按错误类型判断是换源还是直接回报失败：{error:#}",
                    request.keyword
                );
                let item_message = port.item_scoped_playback_error_message(&error);
                if port.is_track_unavailable_error(&error) || item_message.is_some() {
                    let reason = item_message.map_or_else(
                        || "平台无对应歌曲音源".to_string(),
                        |message| format!("{}: {message}", request_source(request)),
                    );
                    if purpose.allows_source_switch() {
                        return self
                            .switch_source_and_play(requested, request, purpose, &reason, port);
                    }
                    let mut completion = PlaybackCompletion::new(
                        PlaybackResult::no_source(requested, request, None, reason),
                        None,
                        false,
                    );
                    if purpose.replies_with_play_message() {
                        completion.reply_template = Some(match item_message {
                            Some(message) => ReplyMessage::new(
                                &playback_templates::ITEM_FAILURE,
                                ReplyVars::new()
                                    .with("来源", request_source(request))
                                    .with("原因", message),
                            ),
                            None => {
                                ReplyMessage::new(&playback_templates::NO_SOURCE, ReplyVars::new())
                            }
                        });
                    }
                    return Ok(completion);
                }
                let message = error.to_string();
                let reply = if message.trim().is_empty() {
                    "平台无对应歌曲音源".to_string()
                } else {
                    message.trim().to_string()
                };
                log::error!(
                    "播放「{}」失败（步骤：播放后端报错且不属于可换源的音源问题）；本次点歌未生效：{reply}",
                    request.keyword
                );
                let mut completion = PlaybackCompletion::new(
                    PlaybackResult::error(requested, request, None, reply.clone()),
                    None,
                    false,
                );
                if purpose.replies_with_play_message()
                    || matches!(purpose, PlaybackPurpose::SourceRetry { .. })
                {
                    if message.trim().is_empty() {
                        completion.reply_template = Some(ReplyMessage::new(
                            &playback_templates::NO_SOURCE,
                            ReplyVars::new(),
                        ));
                    } else {
                        // 播放器返回的原始错误文本：文案源头不在本模块，原样透传。
                        completion.reply = Some(reply);
                    }
                }
                return Ok(completion);
            }
        };
        self.complete_verification(requested, request, verification, purpose, port)
    }

    fn complete_verification<P: PlaybackExecutionPort + ?Sized>(
        &self,
        requested: &PlaybackRequest,
        request: &PlaybackRequest,
        verification: PlaybackVerification,
        purpose: PlaybackPurpose,
        _port: &mut P,
    ) -> Result<PlaybackCompletion> {
        let PlaybackVerification::Success { status, message } = verification;
        Ok(PlaybackCompletion::new(
            PlaybackResult::success(
                requested,
                request,
                status,
                matches!(purpose, PlaybackPurpose::SourceRetry { .. }),
            ),
            purpose.replies_with_play_message().then_some(message),
            true,
        ))
    }

    fn switch_source_and_play<P: PlaybackExecutionPort + ?Sized>(
        &self,
        requested: &PlaybackRequest,
        request: &PlaybackRequest,
        purpose: PlaybackPurpose,
        failure_reason: &str,
        port: &mut P,
    ) -> Result<PlaybackCompletion> {
        let candidates = alternate_snapshot_candidates(request);
        let mut reasons = vec![failure_reason.to_string()];
        let mut completion = PlaybackCompletion::new(
            PlaybackResult::no_source(requested, request, None, failure_reason),
            None,
            true,
        );
        // 每个备用提供商最多贡献一个匹配候选。SourceRetry 不会递归切换，因此不可用快照数量有界。
        for candidate in candidates {
            let mut resolved = request.clone();
            resolved.keyword = candidate.text.clone();
            resolved.source = candidate.track_ref.key.provider.to_string();
            resolved.track = Some(candidate.playable_track());
            log::info!(
                "原音源不可用，尝试换源播放「{}」：改用平台「{}」",
                resolved.keyword,
                resolved.source
            );
            completion = self.run_request(
                requested,
                &resolved,
                PlaybackPurpose::SourceRetry {
                    stop_when_user_paused: purpose.stop_when_user_paused(),
                },
                port,
            )?;
            match completion.result.outcome() {
                PlaybackOutcome::Success => {
                    if purpose.replies_with_play_message() {
                        completion.reply = None;
                        completion.reply_template = Some(ReplyMessage::new(
                            &playback_templates::SOURCE_SWITCHED,
                            ReplyVars::new()
                                .with("歌名", compact_candidate_title(&resolved.keyword)),
                        ));
                    }
                    return Ok(completion);
                }
                PlaybackOutcome::ItemScopedFailure => {
                    if let Some(reason) = completion.result.failure_reason()
                        && !reasons.iter().any(|existing| existing == reason)
                    {
                        reasons.push(reason.to_string());
                    }
                }
                _ => {
                    if !purpose.replies_with_play_message() {
                        completion.reply = None;
                    }
                    return Ok(completion);
                }
            }
        }
        let reason = reasons.join("; ");
        completion.result.failure_reason = Some(reason.clone());
        if purpose.replies_with_play_message() {
            completion.reply = None;
            completion.reply_template = Some(if matches!(purpose, PlaybackPurpose::Queue { .. }) {
                ReplyMessage::new(
                    &playback_templates::QUEUE_SKIPPED,
                    ReplyVars::new().with("原因", reason),
                )
            } else {
                // 多个平台失败原因拼接：动态文本本身作为变量注入。
                ReplyMessage::new(
                    &playback_templates::FAILURE_REASON,
                    ReplyVars::new().with("原因", reason),
                )
            });
        }
        Ok(completion)
    }

    fn finish_playback<P: PlaybackExecutionPort + ?Sized>(
        &self,
        completion: PlaybackCompletion,
        port: &mut P,
    ) -> Result<PlaybackResult> {
        if completion.result.outcome() == PlaybackOutcome::Success {
            let mut request = completion.result.final_request().clone();
            // 重播最终成功曲目，不再消费原队列项或重复执行上一曲历史导航。
            request.queue_item_id = None;
            request.navigation = PlaybackNavigation::Normal;
            *self
                .last_played
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(request);
        }
        if completion.update_monitor {
            port.update_monitor();
        }
        if let Some(reply) = completion.reply {
            port.reply(&reply)?;
        } else if let Some(message) = completion.reply_template {
            port.reply_template(message.spec, &message.vars)?;
        }
        Ok(completion.result)
    }
}

fn pool_selection(track: &PlayableTrack) -> PlaybackSelection {
    let title = track.metadata.title.trim();
    let joined_artists = track.metadata.artists.join(" ");
    let artist = joined_artists.trim();
    let keyword = if artist.is_empty() {
        title.to_string()
    } else {
        format!("{title} - {artist}")
    };
    PlaybackSelection {
        keyword,
        source: track.track_ref.key.provider.as_str().to_string(),
        prefer_accompaniment: false,
        ai_original_text: String::new(),
        track: Some(track.clone()),
        friend_username: String::new(),
        requester: String::new(),
        console_bypass_dedup: false,
        candidate_snapshot: Vec::new(),
        queue_item_id: None,
    }
}

fn select_snapshot_candidate(
    candidates: &[SearchCandidate],
    source: &str,
    prefer_accompaniment: bool,
    reference: &PlayableTrack,
) -> Option<SearchCandidate> {
    let source_candidates = candidates
        .iter()
        .filter(|candidate| candidate.track_ref.key.provider.as_str() == source)
        .filter(|candidate| {
            candidate.eligibility != crate::features::song_request::CandidateEligibility::Ineligible
        })
        .filter(|candidate| candidate_matches_reference(candidate, reference))
        .cloned()
        .collect::<Vec<_>>();
    if source_candidates.is_empty() {
        return None;
    }
    let accompaniment = source_candidates
        .iter()
        .filter(|candidate| is_accompaniment_candidate(&candidate.text))
        .cloned()
        .collect::<Vec<_>>();
    let comparable = if prefer_accompaniment && !accompaniment.is_empty() {
        accompaniment
    } else {
        source_candidates
    };
    comparable
        .into_iter()
        .max_by_key(|candidate| candidate.eligibility.preference_rank())
}

/// 个人歌单「本轮已尝试」的稳定键：优先曲目身份，退化为关键词。
fn playlist_attempt_key(item: &QueueItem) -> String {
    item.track
        .as_ref()
        .map(|track| format!("{:?}", track.track_ref.key))
        .unwrap_or_else(|| item.keyword.clone())
}

fn alternate_snapshot_candidates(request: &PlaybackRequest) -> Vec<SearchCandidate> {
    let Some(reference) = request.track.as_ref() else {
        return Vec::new();
    };
    let mut candidates = ProviderId::ALL
        .into_iter()
        .filter(|provider| *provider != reference.track_ref.key.provider)
        .filter_map(|provider| {
            select_snapshot_candidate(
                &request.candidate_snapshot,
                provider.as_str(),
                request.prefer_accompaniment,
                reference,
            )
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| {
        std::cmp::Reverse((
            request.prefer_accompaniment && is_accompaniment_candidate(&candidate.text),
            candidate.eligibility.preference_rank(),
        ))
    });
    candidates
}

fn candidate_matches_reference(candidate: &SearchCandidate, reference: &PlayableTrack) -> bool {
    if !same_song_query(&candidate.metadata.title, &reference.metadata.title) {
        return false;
    }
    if candidate.metadata.artists.is_empty() || reference.metadata.artists.is_empty() {
        return false;
    }
    if !candidate.metadata.artists.iter().any(|candidate_artist| {
        reference
            .metadata
            .artists
            .iter()
            .any(|reference_artist| same_song_query(candidate_artist, reference_artist))
    }) {
        return false;
    }
    match (
        candidate.metadata.duration_ms,
        reference.metadata.duration_ms,
    ) {
        (Some(candidate_ms), Some(reference_ms)) => {
            let tolerance_ms = (reference_ms / 20).max(5_000);
            candidate_ms.abs_diff(reference_ms) <= tolerance_ms
        }
        _ => true,
    }
}

fn is_accompaniment_candidate(text: &str) -> bool {
    ["伴奏", "纯音乐", "instrumental", "karaoke", "off vocal"]
        .iter()
        .any(|word| text.to_ascii_lowercase().contains(word))
}

fn request_source(request: &PlaybackRequest) -> &str {
    request
        .track
        .as_ref()
        .map(|track| track.track_ref.key.provider.as_str())
        .filter(|source| !source.trim().is_empty())
        .unwrap_or_else(|| request.source.trim())
}

fn compact_candidate_title(candidate: &str) -> String {
    let candidate = candidate.trim();
    if candidate.is_empty() {
        return "未知-未知".to_string();
    }
    candidate.replace(" - ", "-")
}

/// 把完整队列切成不超过聊天宽度的多条消息。
///
/// 表头与单条条目的文案由模板提供；渲染走调用方给的 `render`，这样用户对
/// `playback.queue.header` / `playback.queue.entry` 的覆盖与其它回复一致生效。
fn split_full_queue_messages(
    queue: &[QueueItem],
    queue_max_size: usize,
    header_spec: &ReplyTemplate,
    entry_spec: &ReplyTemplate,
    render: &mut dyn FnMut(&ReplyTemplate, &ReplyVars) -> String,
) -> Vec<String> {
    let header = render(
        header_spec,
        &ReplyVars::new()
            .with("当前", queue.len().to_string())
            .with("上限", queue_max_size.to_string()),
    );
    let mut messages = Vec::new();
    let mut current = header;
    let mut has_entry = false;

    for (index, item) in queue.iter().enumerate() {
        let entry = render(
            entry_spec,
            &ReplyVars::new()
                .with("序号", (index + 1).to_string())
                .with("歌名", item.keyword.clone()),
        );
        let entry_width = display_width(&entry);

        if entry_width > MAX_CHAT_WIDTH {
            if has_entry || !current.is_empty() {
                messages.push(std::mem::take(&mut current));
            }
            has_entry = false;
            messages.extend(split_display_width(&entry, MAX_CHAT_WIDTH));
            continue;
        }

        let separator = if has_entry { ", " } else { "" };
        if display_width(&current) + display_width(separator) + entry_width <= MAX_CHAT_WIDTH {
            current.push_str(separator);
            current.push_str(&entry);
            has_entry = true;
            continue;
        }

        if !current.is_empty() {
            messages.push(std::mem::take(&mut current));
        }
        current.push_str(&entry);
        has_entry = true;
    }

    if !current.is_empty() {
        messages.push(current);
    }
    messages
}

fn split_display_width(value: &str, max_width: usize) -> Vec<String> {
    let max_width = max_width.max(1);
    let mut pieces = Vec::new();
    let mut remaining = value;
    while !remaining.is_empty() {
        let mut width = 0;
        let mut end = 0;
        for (index, ch) in remaining.char_indices() {
            let next_width = char_width(ch);
            if end != 0 && width + next_width > max_width {
                break;
            }
            width += next_width;
            end = index + ch.len_utf8();
            if width >= max_width {
                break;
            }
        }
        if end == 0 {
            let ch = remaining.chars().next().expect("remaining is not empty");
            end = ch.len_utf8();
        }
        pieces.push(remaining[..end].to_string());
        remaining = &remaining[end..];
    }
    pieces
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;

    use anyhow::{Result, bail};

    use super::*;
    use crate::features::playback::{test_candidate, test_track};
    use miliastra_kernel::clock::{Clock, ManualClock};

    #[test]
    fn lyrics_wait_for_playback_and_resume_without_repeating_the_previous_line() {
        let mut tracker = LyricTracker::default();
        let mut status = PlayerStatus {
            current_track: Some(test_track("miliastra://track/qqmusic/1", "song - artist")),
            status: "paused".to_string(),
            lyric_line_text: "第一句".to_string(),
            ..PlayerStatus::default()
        };

        assert!(!tracker.observe(&status));
        status.status = "playing".to_string();
        assert!(tracker.observe(&status));
        status.status = "paused".to_string();
        assert!(!tracker.observe(&status));
        status.status = "playing".to_string();
        assert!(!tracker.observe(&status));

        status.status = "paused".to_string();
        status.lyric_line_text = "第二句".to_string();
        assert!(!tracker.observe(&status));
        status.status = "playing".to_string();
        assert!(tracker.observe(&status));

        status.status = "stopped".to_string();
        status.lyric_line_text = "第三句".to_string();
        assert!(!tracker.observe(&status));
        status.status = "playing".to_string();
        status.current_track = Some(test_track("miliastra://track/qqmusic/2", "next - artist"));
        status.lyric_line_text = "第二句".to_string();
        assert!(tracker.observe(&status));
    }

    struct MonitorPort {
        clock: Arc<ManualClock>,
        waits: usize,
        status_reads: usize,
        /// 测试用：第几次 wait 前把共享 tick 值改为新值（模拟运行中热更新）。
        tick_override: Option<(usize, u64)>,
        /// 与 PlaybackApplicationConfig.monitor_tick_ms 共享同一句柄。
        tick: Arc<RwLock<u64>>,
        wait_durations: Vec<Duration>,
    }

    impl PlaybackMonitorPort for MonitorPort {
        fn now(&self) -> Instant {
            self.clock.now()
        }

        fn is_running(&self) -> bool {
            self.waits < 4
        }

        fn is_paused(&self) -> bool {
            false
        }

        fn wait(&mut self, duration: Duration) {
            if let Some((at_wait, value)) = self.tick_override
                && self.waits + 1 == at_wait
            {
                *self
                    .tick
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = value;
            }
            self.clock.advance(duration).unwrap();
            self.waits += 1;
            self.wait_durations.push(duration);
        }

        fn player_status(&mut self) -> Result<PlayerStatus> {
            self.status_reads += 1;
            Ok(PlayerStatus {
                status: "playing".to_string(),
                current_uri: "miliastra://track/qqmusic/current".to_string(),
                name: "当前歌曲".to_string(),
                duration: 180.0,
                progress: self.waits as f64,
                ..PlayerStatus::default()
            })
        }

        fn playback_queue(&mut self) -> Result<Vec<QueueItem>> {
            Ok(Vec::new())
        }

        fn workload(&mut self) -> Result<PlaybackWorkload> {
            Ok(PlaybackWorkload {
                has_pending_playback_task: false,
                command_executing: false,
            })
        }

        fn maybe_advance_queue(
            &mut self,
            _status: PlayerStatus,
            _context: QueueAdvanceContext,
        ) -> Result<QueueAdvanceDecision> {
            Ok(QueueAdvanceDecision::None)
        }

        fn enqueue_advance_queue(&mut self, _reason: &'static str) -> Result<()> {
            unreachable!("the stable player status does not advance the queue")
        }

        fn update_monitor(&mut self) {}
    }

    struct FailingPlaybackPort {
        queue: Vec<QueueItem>,
        removed: Vec<QueueRemoval>,
        replies: Vec<String>,
        track_unavailable: bool,
        always_unavailable: bool,
        item_scoped_error: bool,
        play_attempts: usize,
        pool: Vec<PlayableTrack>,
    }

    struct VerifyingPlaybackPort {
        queue: Vec<QueueItem>,
        verifications: VecDeque<PlaybackVerification>,
        removed_ids: Vec<u64>,
        replies: Vec<String>,
        reply_error: bool,
        played_uris: Vec<String>,
        user_paused: bool,
        reload_pending: bool,
        pool: Vec<PlayableTrack>,
        preloaded: Vec<String>,
    }

    #[test]
    fn monitor_status_schedule_uses_the_port_clock() {
        let clock = Arc::new(ManualClock::new(Instant::now()));
        let mut port = MonitorPort {
            clock,
            waits: 0,
            status_reads: 0,
            tick_override: None,
            tick: Arc::new(RwLock::new(50)),
            wait_durations: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(100)),
            help_batch_ms: 0,
        });

        application.run_monitor_loop(&mut port);

        assert_eq!(port.status_reads, 2);
    }

    #[test]
    fn monitor_loop_reads_live_tick_values_each_round() {
        let clock = Arc::new(ManualClock::new(Instant::now()));
        let tick = Arc::new(RwLock::new(50u64));
        let mut port = MonitorPort {
            clock,
            waits: 0,
            status_reads: 0,
            // 第 2 次 wait 前把共享 tick 从 50 改为 500：循环每轮重读共享值
            // 时第 3 次起的 wait 时长应为 500ms；修复前启动时只读一次，恒为 50ms。
            tick_override: Some((2, 500)),
            tick: tick.clone(),
            wait_durations: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: tick,
            monitor_status_ms: Arc::new(RwLock::new(100)),
            help_batch_ms: 0,
        });

        application.run_monitor_loop(&mut port);

        assert_eq!(
            port.wait_durations,
            [
                Duration::from_millis(50),
                Duration::from_millis(50),
                Duration::from_millis(500),
                Duration::from_millis(500),
            ]
        );
    }

    #[test]
    fn queued_songs_ignore_play_mode_and_keep_insertion_order() {
        let queue = [1, 2].map(|id| QueueItem {
            id,
            keyword: format!("队列歌曲{id}"),
            track: Some(test_track(
                &format!("miliastra://track/qqmusic/{id}"),
                &format!("队列歌曲{id} - 歌手{id}"),
            )),
            ..QueueItem::default()
        });
        let verifications = [1, 2].map(|id| PlaybackVerification::Success {
            status: PlayerStatus {
                status: "playing".to_string(),
                current_uri: format!("miliastra://track/qqmusic/{id}"),
                ..PlayerStatus::default()
            },
            message: format!("开始播放: 队列歌曲{id}"),
        });
        let mut port = VerifyingPlaybackPort {
            queue: queue.into(),
            verifications: verifications.into(),
            removed_ids: Vec::new(),
            replies: Vec::new(),
            reply_error: false,
            played_uris: Vec::new(),
            user_paused: false,
            reload_pending: false,
            pool: Vec::new(),
            preloaded: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(100)),
            help_batch_ms: 0,
        });
        application
            .play_mode_handle()
            .store(PLAY_MODE_REPEAT_ONE, Ordering::Relaxed);

        application.consume_queue("test", &mut port).unwrap();
        application.consume_queue("test", &mut port).unwrap();

        assert_eq!(port.removed_ids, [1, 2]);
        assert_eq!(
            port.played_uris,
            ["miliastra://track/qqmusic/1", "miliastra://track/qqmusic/2"]
        );
    }

    impl PlaybackExecutionPort for VerifyingPlaybackPort {
        fn reply(&mut self, message: &str) -> Result<()> {
            self.replies.push(message.to_string());
            if self.reply_error {
                bail!("reply failed");
            }
            Ok(())
        }

        fn update_monitor(&mut self) {}

        fn search_and_pick(
            &mut self,
            _keyword: &str,
            _source: &str,
            _prefer_accompaniment: bool,
        ) -> std::result::Result<Option<PlaybackPickedCandidate>, PlaybackSearchFailure> {
            unreachable!("queued items already have URIs")
        }

        fn song_dedup_limited(&mut self, _request: &PlaybackRequest) -> Result<bool> {
            Ok(false)
        }

        fn play_and_verify(&mut self, request: &PlaybackRequest) -> Result<PlaybackVerification> {
            self.played_uris.push(request.uri());
            Ok(self
                .verifications
                .pop_front()
                .expect("verification outcome"))
        }

        fn player_status(&mut self) -> Result<PlayerStatus> {
            Ok(PlayerStatus {
                status: "playing".to_string(),
                current_uri: "miliastra://track/qqmusic/current".to_string(),
                name: "当前歌曲".to_string(),
                singer: String::new(),
                album_name: String::new(),
                lyric_line_text: String::new(),
                duration: 180.0,
                progress: 10.0,
                playback_rate: 1.0,
                volume: 50,
                requester: String::new(),
                ..PlayerStatus::default()
            })
        }

        fn playback_queue(&mut self) -> Result<Vec<QueueItem>> {
            Ok(self.queue.clone())
        }

        fn pick_playback_pool_track(
            &mut self,
            excluded: &HashSet<TrackKey>,
        ) -> Result<Option<PlayableTrack>> {
            Ok(self
                .pool
                .iter()
                .find(|track| !excluded.contains(&track.track_ref.key))
                .cloned())
        }

        fn remove_playback_queue(&mut self, removal: QueueRemoval) -> Result<()> {
            let QueueRemoval::Id(id) = removal else {
                unreachable!("queue consumption removes by id")
            };
            self.removed_ids.push(id);
            self.queue.retain(|item| item.id != id);
            Ok(())
        }

        fn preload_track(&mut self, track: &PlayableTrack) -> Result<()> {
            self.preloaded.push(track.track_ref.key.to_string());
            Ok(())
        }

        fn user_pause_active(&mut self) -> Result<bool> {
            Ok(self.user_paused)
        }

        fn automatic_queue_advance_allowed(&mut self) -> Result<bool> {
            Ok(!self.reload_pending)
        }
    }

    impl PlaybackExecutionPort for FailingPlaybackPort {
        fn reply(&mut self, message: &str) -> Result<()> {
            self.replies.push(message.to_string());
            Ok(())
        }

        fn update_monitor(&mut self) {}

        fn search_and_pick(
            &mut self,
            _keyword: &str,
            _source: &str,
            _prefer_accompaniment: bool,
        ) -> std::result::Result<Option<PlaybackPickedCandidate>, PlaybackSearchFailure> {
            unreachable!("the queued item already has a URI")
        }

        fn song_dedup_limited(&mut self, _request: &PlaybackRequest) -> Result<bool> {
            Ok(false)
        }

        fn play_and_verify(&mut self, request: &PlaybackRequest) -> Result<PlaybackVerification> {
            self.play_attempts += 1;
            if self.track_unavailable && (self.always_unavailable || self.play_attempts == 1) {
                bail!("track unavailable")
            }
            if self.track_unavailable {
                return Ok(PlaybackVerification::Success {
                    status: PlayerStatus {
                        status: "playing".to_string(),
                        current_uri: request.uri(),
                        ..PlayerStatus::default()
                    },
                    message: "开始播放: 备用歌曲".to_string(),
                });
            }
            bail!("player unavailable")
        }

        fn is_track_unavailable_error(&self, error: &anyhow::Error) -> bool {
            self.track_unavailable && error.to_string() == "track unavailable"
        }

        fn item_scoped_playback_error_message(
            &self,
            _error: &anyhow::Error,
        ) -> Option<&'static str> {
            self.item_scoped_error
                .then_some("登录凭据已失效，请重新登录")
        }

        fn player_status(&mut self) -> Result<PlayerStatus> {
            unreachable!("playback never started")
        }

        fn playback_queue(&mut self) -> Result<Vec<QueueItem>> {
            Ok(self.queue.clone())
        }

        fn pick_playback_pool_track(
            &mut self,
            excluded: &HashSet<TrackKey>,
        ) -> Result<Option<PlayableTrack>> {
            Ok(self
                .pool
                .iter()
                .find(|track| !excluded.contains(&track.track_ref.key))
                .cloned())
        }

        fn remove_playback_queue(&mut self, removal: QueueRemoval) -> Result<()> {
            let QueueRemoval::Id(id) = removal else {
                unreachable!("queue consumption removes by id")
            };
            self.removed.push(QueueRemoval::Id(id));
            self.queue.retain(|item| item.id != id);
            Ok(())
        }
    }

    #[test]
    fn personal_playlist_mode_does_not_fall_back_to_default_pool() {
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(100)),
            monitor_status_ms: Arc::new(RwLock::new(100)),
            help_batch_ms: 0,
        });
        *application.personal_playlist.lock().unwrap() = Some(PersonalPlaylistQueue {
            owner: "甲".to_string(),
            tracks: vec![test_track(
                "miliastra://track/qqmusic/playlist",
                "歌单歌曲 - 歌手",
            )],
            cursor: 0,
            shuffle_state: 1,
            last_played_key: None,
        });
        let mut port = FailingPlaybackPort {
            queue: Vec::new(),
            removed: Vec::new(),
            replies: Vec::new(),
            track_unavailable: false,
            always_unavailable: false,
            item_scoped_error: false,
            play_attempts: 0,
            pool: vec![test_track(
                "miliastra://track/qqmusic/default-pool",
                "大厅曲库歌曲 - 歌手",
            )],
        };

        assert!(
            application
                .consume_queue("test", &mut port)
                .unwrap()
                .is_none()
        );
        assert_eq!(port.play_attempts, 0);
    }

    #[test]
    fn personal_playlist_queue_cycles_without_duplicate_pending_tracks() {
        let tracks = vec![
            test_track("miliastra://track/qqmusic/a", "A - 歌手"),
            test_track("miliastra://track/qqmusic/b", "B - 歌手"),
        ];
        let mut mode = PersonalPlaylistQueue {
            owner: "甲".to_string(),
            tracks,
            cursor: 0,
            shuffle_state: 1,
            last_played_key: None,
        };
        let first = mode.next_track_excluding(&HashSet::new()).unwrap();
        let second = mode
            .next_track_excluding(&HashSet::from([first.track_ref.key.clone()]))
            .unwrap();
        assert_ne!(first.track_ref.key, second.track_ref.key);
        let repeated = mode.next_track_excluding(&HashSet::new()).unwrap();
        assert!(
            mode.tracks
                .iter()
                .any(|track| track.track_ref.key == repeated.track_ref.key)
        );
    }

    #[test]
    fn start_personal_playlist_is_idempotent_and_keeps_the_first_owner() {
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(100)),
            monitor_status_ms: Arc::new(RwLock::new(100)),
            help_batch_ms: 0,
        });
        *application.personal_playlist.lock().unwrap() = Some(PersonalPlaylistQueue {
            owner: "甲".to_string(),
            tracks: vec![test_track("miliastra://track/qqmusic/a", "A - 歌手")],
            cursor: 0,
            shuffle_state: 1,
            last_played_key: None,
        });
        let mut port = FailingPlaybackPort {
            queue: Vec::new(),
            removed: Vec::new(),
            replies: Vec::new(),
            track_unavailable: false,
            always_unavailable: false,
            item_scoped_error: false,
            play_attempts: 0,
            pool: Vec::new(),
        };
        let outcome = application
            .start_personal_playlist(
                "乙",
                vec![test_track("miliastra://track/qqmusic/b", "B - 歌手")],
                &mut port,
            )
            .unwrap();
        assert_eq!(
            outcome,
            PersonalPlaylistStart::AlreadyActive {
                owner: "甲".to_string()
            }
        );
        assert_eq!(application.personal_playlist_owner().as_deref(), Some("甲"));
        assert!(
            port.removed.is_empty(),
            "already active must not touch the queue"
        );
        let tracks = application
            .personal_playlist
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .tracks
            .clone();
        assert_eq!(tracks.len(), 1, "the original queue must be preserved");
    }

    #[test]
    fn status_owner_only_follows_playlist_auto_source() {
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(100)),
            monitor_status_ms: Arc::new(RwLock::new(100)),
            help_batch_ms: 0,
        });
        *application.personal_playlist.lock().unwrap() = Some(PersonalPlaylistQueue {
            owner: "甲".to_string(),
            tracks: vec![test_track("miliastra://track/qqmusic/a", "A - 歌手")],
            cursor: 0,
            shuffle_state: 1,
            last_played_key: None,
        });
        application.note_playback_purpose(PlaybackPurpose::Playlist {
            stop_when_user_paused: true,
        });
        assert!(application.current_is_playlist_auto());
        assert_eq!(application.personal_playlist_owner().as_deref(), Some("甲"));
        application.note_playback_purpose(PlaybackPurpose::Requested);
        assert!(!application.current_is_playlist_auto());
    }

    #[test]
    fn queue_head_is_preserved_when_playback_fails() {
        let item = QueueItem {
            id: 7,
            keyword: "测试歌曲".to_string(),
            track: Some(test_track(
                "miliastra://track/qqmusic/7",
                "测试歌曲 - 测试歌手",
            )),
            ..QueueItem::default()
        };
        let mut port = FailingPlaybackPort {
            queue: vec![item],
            removed: Vec::new(),
            replies: Vec::new(),
            track_unavailable: false,
            always_unavailable: false,
            item_scoped_error: false,
            play_attempts: 0,
            pool: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(50)),
            help_batch_ms: 0,
        });

        application
            .consume_queue("test", &mut port)
            .expect("queue consumption should report the failure and stop");

        assert!(port.removed.is_empty());
        assert_eq!(port.replies, ["player unavailable"]);
    }

    #[test]
    fn item_scoped_provider_failure_is_removed_and_queue_continues() {
        let queue = [1, 2]
            .into_iter()
            .map(|id| QueueItem {
                id,
                keyword: format!("酷狗歌曲{id}"),
                track: Some(test_track(
                    &format!("miliastra://track/kugou/{id}"),
                    &format!("酷狗歌曲{id} - 歌手"),
                )),
                ..QueueItem::default()
            })
            .collect();
        let mut port = FailingPlaybackPort {
            queue,
            removed: Vec::new(),
            replies: Vec::new(),
            track_unavailable: false,
            always_unavailable: false,
            item_scoped_error: true,
            play_attempts: 0,
            pool: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(50)),
            help_batch_ms: 0,
        });

        application
            .consume_queue("credential-rejected", &mut port)
            .expect("deterministic provider failure should not block queue");

        assert!(port.queue.is_empty());
        assert_eq!(port.removed, [QueueRemoval::Id(1), QueueRemoval::Id(2)]);
        assert_eq!(port.play_attempts, 2);
        assert_eq!(
            port.replies,
            [
                "kugou: 登录凭据已失效，请重新登录，已跳过当前歌曲",
                "kugou: 登录凭据已失效，请重新登录，已跳过当前歌曲"
            ]
        );
    }

    #[test]
    fn track_unavailable_queue_item_uses_the_alternate_source() {
        let item = QueueItem {
            id: 17,
            keyword: "不可用候选".to_string(),
            track: Some(test_track(
                "miliastra://track/qqmusic/17",
                "不可用候选 - 测试歌手",
            )),
            candidate_snapshot: vec![test_candidate(
                "不可用候选 - 测试歌手",
                "miliastra://track/netease/backup",
            )],
            ..QueueItem::default()
        };
        let mut port = FailingPlaybackPort {
            queue: vec![item],
            removed: Vec::new(),
            replies: Vec::new(),
            track_unavailable: true,
            always_unavailable: false,
            item_scoped_error: false,
            play_attempts: 0,
            pool: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(50)),
            help_batch_ms: 0,
        });

        application
            .consume_queue("track-unavailable", &mut port)
            .expect("unavailable candidate should use the alternate source");

        assert!(port.queue.is_empty());
        assert_eq!(port.removed, [QueueRemoval::Id(17)]);
        assert_eq!(port.play_attempts, 2);
        assert_eq!(port.replies, ["已自动换源:不可用候选-测试歌手"]);
    }

    #[test]
    fn alternate_source_selection_skips_an_unrelated_first_result() {
        let reference = test_track("miliastra://track/qqmusic/original", "目标歌曲 - 原歌手");
        let candidates = vec![
            test_candidate("无关歌曲 - 其他歌手", "miliastra://track/netease/wrong"),
            test_candidate("目标歌曲 - 原歌手", "miliastra://track/netease/right"),
        ];

        let selected = select_snapshot_candidate(&candidates, "netease", false, &reference)
            .expect("matching structured candidate");

        assert_eq!(selected.track_ref.key.id, "right");
    }

    #[test]
    fn track_unavailable_during_source_retry_does_not_retry_again() {
        let item = QueueItem {
            id: 18,
            keyword: "持续不可用候选".to_string(),
            track: Some(test_track(
                "miliastra://track/qqmusic/18",
                "持续不可用候选 - 测试歌手",
            )),
            candidate_snapshot: vec![test_candidate(
                "持续不可用候选 - 测试歌手",
                "miliastra://track/netease/backup",
            )],
            ..QueueItem::default()
        };
        let mut port = FailingPlaybackPort {
            queue: vec![item],
            removed: Vec::new(),
            replies: Vec::new(),
            track_unavailable: true,
            always_unavailable: true,
            item_scoped_error: false,
            play_attempts: 0,
            pool: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(50)),
            help_batch_ms: 0,
        });

        application
            .consume_queue("track-unavailable", &mut port)
            .expect("retry failure should be reported without another source switch");

        assert_eq!(port.play_attempts, 2);
        assert_eq!(port.removed, [QueueRemoval::Id(18)]);
        assert!(port.queue.is_empty());
        assert_eq!(port.replies, ["平台无对应歌曲音源，已跳过当前歌曲"]);
    }

    #[test]
    fn successful_queue_item_is_removed_before_reply_failure_is_reported() {
        let item = QueueItem {
            id: 13,
            keyword: "已成功播放".to_string(),
            track: Some(test_track(
                "miliastra://track/qqmusic/13",
                "已成功播放 - 测试歌手",
            )),
            ..QueueItem::default()
        };
        let mut port = VerifyingPlaybackPort {
            queue: vec![item],
            verifications: VecDeque::from([PlaybackVerification::Success {
                status: PlayerStatus {
                    status: "playing".to_string(),
                    current_uri: "miliastra://track/qqmusic/13".to_string(),
                    ..PlayerStatus::default()
                },
                message: "开始播放: 已成功播放".to_string(),
            }]),
            removed_ids: Vec::new(),
            replies: Vec::new(),
            reply_error: true,
            played_uris: Vec::new(),
            user_paused: false,
            reload_pending: false,
            pool: Vec::new(),
            preloaded: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(50)),
            help_batch_ms: 0,
        });

        let error = application
            .consume_queue("test", &mut port)
            .expect_err("reply failure should still be reported");

        assert_eq!(error.to_string(), "reply failed");
        assert!(port.queue.is_empty());
        assert_eq!(port.removed_ids, [13]);
    }

    #[test]
    fn automatic_queue_keeps_items_when_user_pause_is_active() {
        let item = QueueItem {
            id: 8,
            keyword: "暂停期间歌曲".to_string(),
            track: Some(test_track(
                "miliastra://track/qqmusic/8",
                "暂停期间歌曲 - 测试歌手",
            )),
            ..QueueItem::default()
        };
        let mut port = VerifyingPlaybackPort {
            queue: vec![item],
            verifications: VecDeque::new(),
            removed_ids: Vec::new(),
            replies: Vec::new(),
            reply_error: false,
            played_uris: Vec::new(),
            user_paused: true,
            reload_pending: false,
            pool: Vec::new(),
            preloaded: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(50)),
            help_batch_ms: 0,
        });

        application
            .consume_queue_after_monitor("闲置退出", &mut port)
            .expect("automatic queue consumption should be gated");

        assert!(port.removed_ids.is_empty());
        assert!(port.replies.is_empty());
        assert_eq!(port.queue.len(), 1);
    }

    #[test]
    fn automatic_queue_keeps_items_when_reload_is_pending() {
        let item = QueueItem {
            id: 9,
            keyword: "重载前保留歌曲".to_string(),
            track: Some(test_track(
                "miliastra://track/qqmusic/9",
                "重载前保留歌曲 - 测试歌手",
            )),
            ..QueueItem::default()
        };
        let mut port = VerifyingPlaybackPort {
            queue: vec![item],
            verifications: VecDeque::new(),
            removed_ids: Vec::new(),
            replies: Vec::new(),
            reply_error: false,
            played_uris: Vec::new(),
            user_paused: false,
            reload_pending: true,
            pool: Vec::new(),
            preloaded: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(50)),
            help_batch_ms: 0,
        });

        application
            .consume_queue_after_monitor("自然结束", &mut port)
            .expect("pending reload should gate automatic queue consumption");

        assert!(port.played_uris.is_empty());
        assert!(port.removed_ids.is_empty());
        assert_eq!(port.queue.len(), 1);

        // 替代进程持有新的 LiveConfigs 实例（没有待处理重载），因此未变化的队首可以正常消费。
        port.reload_pending = false;
        port.verifications.push_back(PlaybackVerification::Success {
            status: PlayerStatus {
                status: "playing".to_string(),
                current_uri: "miliastra://track/qqmusic/9".to_string(),
                ..PlayerStatus::default()
            },
            message: "开始播放: 重载前保留歌曲".to_string(),
        });
        application
            .consume_queue_after_monitor("重载后恢复", &mut port)
            .expect("replacement process should consume the preserved queue head");

        assert_eq!(port.played_uris, ["miliastra://track/qqmusic/9"]);
        assert_eq!(port.removed_ids, [9]);
        assert!(port.queue.is_empty());
    }

    #[test]
    fn queue_drops_a_song_only_after_the_alternate_source_has_no_candidate() {
        let first = QueueItem {
            id: 11,
            keyword: "两个平台都无音源".to_string(),
            source: "qqmusic".to_string(),
            track: Some(test_track(
                "miliastra://track/qqmusic/missing",
                "两个平台都无音源 - 测试歌手",
            )),
            ..QueueItem::default()
        };
        let second = QueueItem {
            id: 12,
            keyword: "下一首可播放".to_string(),
            source: "qqmusic".to_string(),
            track: Some(test_track(
                "miliastra://track/qqmusic/next",
                "下一首可播放 - 测试歌手",
            )),
            ..QueueItem::default()
        };
        let mut port = FailingPlaybackPort {
            queue: vec![first, second],
            removed: Vec::new(),
            replies: Vec::new(),
            track_unavailable: true,
            always_unavailable: true,
            item_scoped_error: false,
            play_attempts: 0,
            pool: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(50)),
            help_batch_ms: 0,
        });

        application
            .consume_queue("test", &mut port)
            .expect("confirmed no-source item should be dropped before the next song");

        assert!(port.queue.is_empty());
        assert_eq!(port.removed, [QueueRemoval::Id(11), QueueRemoval::Id(12)]);
        // 每首歌各尝试一次：换源时无候选快照，直接确认项目级失败并丢弃。
        assert_eq!(port.play_attempts, 2);
        assert_eq!(
            port.replies,
            [
                "平台无对应歌曲音源，已跳过当前歌曲",
                "平台无对应歌曲音源，已跳过当前歌曲"
            ]
        );
    }

    #[test]
    fn queue_empty_falls_back_to_pool_random_playback() {
        let pool_track = test_track("miliastra://track/qqmusic/pool-1", "池歌一 - 歌手A");
        let mut port = VerifyingPlaybackPort {
            queue: Vec::new(),
            verifications: VecDeque::from([PlaybackVerification::Success {
                status: PlayerStatus {
                    status: "playing".to_string(),
                    current_uri: "miliastra://track/qqmusic/pool-1".to_string(),
                    ..PlayerStatus::default()
                },
                message: "开始播放: 池歌一".to_string(),
            }]),
            removed_ids: Vec::new(),
            replies: Vec::new(),
            reply_error: false,
            played_uris: Vec::new(),
            user_paused: false,
            reload_pending: false,
            pool: vec![pool_track],
            preloaded: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(50)),
            help_batch_ms: 0,
        });

        assert_eq!(application.play_mode(), PLAY_MODE_SHUFFLE);
        application
            .consume_queue_after_monitor("自然结束", &mut port)
            .unwrap();

        // 队列空时从播放池随机播放，且不产生出队、不回复聊天。
        assert_eq!(port.played_uris, ["miliastra://track/qqmusic/pool-1"]);
        assert!(port.removed_ids.is_empty());
        assert!(port.replies.is_empty());
    }

    #[test]
    fn pool_track_failure_skips_to_next_candidate_then_stops_when_exhausted() {
        let mut port = FailingPlaybackPort {
            queue: Vec::new(),
            removed: Vec::new(),
            replies: Vec::new(),
            track_unavailable: true,
            always_unavailable: true,
            item_scoped_error: false,
            play_attempts: 0,
            pool: vec![
                test_track("miliastra://track/qqmusic/pool-1", "池歌一 - 歌手A"),
                test_track("miliastra://track/qqmusic/pool-2", "池歌二 - 歌手B"),
            ],
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(50)),
            help_batch_ms: 0,
        });

        application
            .consume_queue_after_monitor("自然结束", &mut port)
            .unwrap();

        // 所有池歌各尝试一次后耗尽候选，静默结束，不在失败歌曲之间循环。
        assert_eq!(port.play_attempts, 2);
        assert!(port.replies.is_empty());
    }

    #[test]
    fn successful_playback_preloads_the_next_queue_track() {
        let first = QueueItem {
            id: 21,
            keyword: "第一首".to_string(),
            track: Some(test_track("miliastra://track/qqmusic/21", "第一首 - 歌手A")),
            ..QueueItem::default()
        };
        let second = QueueItem {
            id: 22,
            keyword: "第二首".to_string(),
            track: Some(test_track("miliastra://track/qqmusic/22", "第二首 - 歌手B")),
            ..QueueItem::default()
        };
        let mut port = VerifyingPlaybackPort {
            queue: vec![first, second],
            verifications: VecDeque::from([PlaybackVerification::Success {
                status: PlayerStatus {
                    status: "playing".to_string(),
                    current_uri: "miliastra://track/qqmusic/21".to_string(),
                    ..PlayerStatus::default()
                },
                message: "开始播放: 第一首".to_string(),
            }]),
            removed_ids: Vec::new(),
            replies: Vec::new(),
            reply_error: false,
            played_uris: Vec::new(),
            user_paused: false,
            reload_pending: false,
            pool: Vec::new(),
            preloaded: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(50)),
            help_batch_ms: 0,
        });

        application
            .consume_queue_after_monitor("自然结束", &mut port)
            .unwrap();

        // 只播第一首；成功后预加载队列中的下一首音源。
        assert_eq!(port.played_uris, ["miliastra://track/qqmusic/21"]);
        assert_eq!(
            port.preloaded,
            ["miliastra://track/qqmusic/22"],
            "应预加载队列下一首的音源解析"
        );
    }

    #[derive(Default)]
    struct NavigationCommandPort {
        previous_request: Option<PlaybackRequest>,
        played_uris: Vec<String>,
        verifications: VecDeque<PlaybackVerification>,
        replies: Vec<String>,
        batch_replies: Vec<Vec<String>>,
        batch_delays: Vec<u64>,
        queue: Vec<QueueItem>,
        pool: Vec<PlayableTrack>,
        status_updates: VecDeque<PlayerStatus>,
        clock: Option<Arc<ManualClock>>,
        status: PlayerStatus,
        stop_continuous_lyrics_after_statuses: Option<usize>,
        status_calls: usize,
        background_lyrics_starts: Vec<(Option<Duration>, BackgroundLyricsScope)>,
    }

    impl PlaybackExecutionPort for NavigationCommandPort {
        fn reply(&mut self, message: &str) -> Result<()> {
            self.replies.push(message.to_string());
            Ok(())
        }

        fn update_monitor(&mut self) {}

        fn search_and_pick(
            &mut self,
            _keyword: &str,
            _source: &str,
            _prefer_accompaniment: bool,
        ) -> std::result::Result<Option<PlaybackPickedCandidate>, PlaybackSearchFailure> {
            unreachable!("navigation target already has a URI")
        }

        fn song_dedup_limited(&mut self, request: &PlaybackRequest) -> Result<bool> {
            Ok(self.played_uris.contains(&request.uri()))
        }

        fn play_and_verify(&mut self, request: &PlaybackRequest) -> Result<PlaybackVerification> {
            self.played_uris.push(request.uri());
            Ok(self
                .verifications
                .pop_front()
                .expect("verification outcome"))
        }

        fn player_status(&mut self) -> Result<PlayerStatus> {
            self.status_calls += 1;
            if let Some(status) = self.status_updates.pop_front() {
                self.status = status;
            }
            Ok(self.status.clone())
        }

        fn playback_queue(&mut self) -> Result<Vec<QueueItem>> {
            Ok(self.queue.clone())
        }

        fn pick_playback_pool_track(
            &mut self,
            excluded: &HashSet<TrackKey>,
        ) -> Result<Option<PlayableTrack>> {
            Ok(self
                .pool
                .iter()
                .find(|track| !excluded.contains(&track.track_ref.key))
                .cloned())
        }

        fn remove_playback_queue(&mut self, removal: QueueRemoval) -> Result<()> {
            if let QueueRemoval::Id(id) = removal {
                self.queue.retain(|item| item.id != id);
            }
            Ok(())
        }
    }

    impl PlaybackCommandPort for NavigationCommandPort {
        fn reply_batch(&mut self, messages: &[String], delay_ms: u64) -> Result<()> {
            self.batch_replies.push(messages.to_vec());
            self.batch_delays.push(delay_ms);
            Ok(())
        }

        fn log_executed(
            &mut self,
            _context: &PlaybackCommandContext,
            _final_command: &str,
        ) -> Result<()> {
            Ok(())
        }

        fn pause_by_user(&mut self) -> Result<String> {
            Ok(String::new())
        }

        fn resume_by_user(&mut self) -> Result<String> {
            Ok(String::new())
        }

        fn previous_playback_request(&mut self) -> Result<Option<PlaybackRequest>> {
            Ok(self.previous_request.clone())
        }

        fn set_volume(&mut self, _volume: &str) -> Result<()> {
            Ok(())
        }

        fn remove_playback_pool_track(&mut self, key: &TrackKey) -> Result<bool> {
            let before = self.pool.len();
            self.pool.retain(|track| &track.track_ref.key != key);
            Ok(self.pool.len() != before)
        }

        fn toggle_lyrics(&mut self) -> Result<String> {
            Ok("translation".to_string())
        }

        fn remove_playback_queue_indexes(
            &mut self,
            _indexes: Vec<usize>,
        ) -> Result<Vec<(usize, QueueItem)>> {
            Ok(Vec::new())
        }

        fn clear_playback_queue(&mut self) -> Result<usize> {
            Ok(0)
        }

        fn should_stop_continuous_lyrics(&mut self) -> Result<bool> {
            Ok(self
                .stop_continuous_lyrics_after_statuses
                .is_some_and(|limit| self.status_calls >= limit))
        }

        fn start_background_lyrics(
            &mut self,
            duration: Option<Duration>,
            scope: BackgroundLyricsScope,
        ) -> Result<bool> {
            self.background_lyrics_starts.push((duration, scope));
            Ok(true)
        }

        fn wait(&mut self, duration: Duration) {
            if let Some(clock) = &self.clock {
                clock.advance(duration).expect("advance test clock");
            }
        }
    }

    fn repeat_application() -> PlaybackApplication {
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(100)),
            help_batch_ms: 0,
        });
        application
            .play_mode_handle()
            .store(PLAY_MODE_REPEAT_ONE, Ordering::Relaxed);
        application
    }

    fn repeat_verification(track: &PlayableTrack) -> PlaybackVerification {
        PlaybackVerification::Success {
            status: PlayerStatus {
                status: "playing".to_string(),
                current_uri: track.track_ref.key.to_string(),
                current_track: Some(track.clone()),
                ..PlayerStatus::default()
            },
            message: "playing".to_string(),
        }
    }

    #[test]
    fn repeat_one_automatic_replay_ignores_request_dedup() {
        let track = test_track("miliastra://track/qqmusic/repeat", "Repeat - Artist");
        let mut port = NavigationCommandPort {
            verifications: [repeat_verification(&track), repeat_verification(&track)].into(),
            ..NavigationCommandPort::default()
        };
        let application = repeat_application();
        let mut selection = pool_selection(&track);
        selection.requester = "friend".to_string();

        application.play_confirmed(&selection, &mut port).unwrap();
        assert!(port.song_dedup_limited(&selection.request()).unwrap());
        let repeated = application
            .consume_queue_after_monitor("natural end", &mut port)
            .unwrap();

        assert!(repeated.is_some());
        assert_eq!(port.played_uris, vec![selection.request().uri(); 2]);
    }

    #[test]
    fn repeat_one_manual_commands_advance_to_pool() {
        for command in [
            PlaybackCommand::Next,
            PlaybackCommand::DeleteCurrentPoolTrack,
        ] {
            let current = test_track("miliastra://track/qqmusic/current", "Current - Artist");
            let next = test_track("miliastra://track/qqmusic/next", "Next - Artist");
            let mut port = NavigationCommandPort {
                verifications: [repeat_verification(&current), repeat_verification(&next)].into(),
                pool: vec![next.clone(), current.clone()],
                status: PlayerStatus {
                    current_track: Some(current.clone()),
                    ..PlayerStatus::default()
                },
                ..NavigationCommandPort::default()
            };
            let application = repeat_application();
            let mut selection = pool_selection(&current);
            selection.console_bypass_dedup = true;
            application.play_confirmed(&selection, &mut port).unwrap();
            let context = PlaybackCommandContext {
                message_type: "console".to_string(),
                username: "tester".to_string(),
                user_command: "next".to_string(),
            };

            application
                .execute_command(&context, &command, &mut port)
                .unwrap();

            assert_eq!(
                port.played_uris,
                [
                    current.track_ref.key.to_string(),
                    next.track_ref.key.to_string()
                ],
                "manual command {command:?} must advance"
            );
            if matches!(command, PlaybackCommand::DeleteCurrentPoolTrack) {
                assert_eq!(port.pool, [next]);
            }
        }
    }

    #[test]
    fn repeat_one_previous_replays_the_previous_track() {
        let current = test_track("miliastra://track/qqmusic/current", "Current - Artist");
        let previous = test_track("miliastra://track/qqmusic/previous", "Previous - Artist");
        let mut previous_request = pool_selection(&previous).request();
        previous_request.navigation = PlaybackNavigation::Previous;
        let mut port = NavigationCommandPort {
            previous_request: Some(previous_request),
            verifications: [
                repeat_verification(&current),
                repeat_verification(&previous),
                repeat_verification(&previous),
            ]
            .into(),
            ..NavigationCommandPort::default()
        };
        let application = repeat_application();
        let mut selection = pool_selection(&current);
        selection.console_bypass_dedup = true;
        application.play_confirmed(&selection, &mut port).unwrap();

        application
            .execute_command(
                &PlaybackCommandContext {
                    message_type: "console".to_string(),
                    username: "tester".to_string(),
                    user_command: "previous".to_string(),
                },
                &PlaybackCommand::Previous,
                &mut port,
            )
            .unwrap();
        application
            .consume_queue_after_monitor("natural end", &mut port)
            .unwrap();

        assert_eq!(
            port.played_uris,
            [
                current.track_ref.key.to_string(),
                previous.track_ref.key.to_string(),
                previous.track_ref.key.to_string(),
            ]
        );
    }

    struct SourceSwitchRepeatPort {
        inner: NavigationCommandPort,
        unavailable: TrackKey,
        attempted: Vec<String>,
    }

    impl PlaybackExecutionPort for SourceSwitchRepeatPort {
        fn reply(&mut self, message: &str) -> Result<()> {
            self.inner.reply(message)
        }
        fn update_monitor(&mut self) {}
        fn search_and_pick(
            &mut self,
            _keyword: &str,
            _source: &str,
            _prefer_accompaniment: bool,
        ) -> std::result::Result<Option<PlaybackPickedCandidate>, PlaybackSearchFailure> {
            unreachable!("source switch uses the candidate snapshot")
        }
        fn song_dedup_limited(&mut self, request: &PlaybackRequest) -> Result<bool> {
            self.inner.song_dedup_limited(request)
        }
        fn play_and_verify(&mut self, request: &PlaybackRequest) -> Result<PlaybackVerification> {
            self.attempted.push(request.uri());
            if request.track.as_ref().unwrap().track_ref.key == self.unavailable {
                bail!("track unavailable");
            }
            self.inner.play_and_verify(request)
        }
        fn is_track_unavailable_error(&self, error: &anyhow::Error) -> bool {
            error.to_string() == "track unavailable"
        }
        fn player_status(&mut self) -> Result<PlayerStatus> {
            self.inner.player_status()
        }
        fn playback_queue(&mut self) -> Result<Vec<QueueItem>> {
            self.inner.playback_queue()
        }
        fn remove_playback_queue(&mut self, removal: QueueRemoval) -> Result<()> {
            self.inner.remove_playback_queue(removal)
        }
    }

    #[derive(Default)]
    struct SourceFailurePort {
        inner: NavigationCommandPort,
        errors: VecDeque<&'static str>,
        requests: Vec<PlaybackRequest>,
        pause_after_failure: bool,
        paused: bool,
    }

    impl PlaybackExecutionPort for SourceFailurePort {
        fn reply(&mut self, message: &str) -> Result<()> {
            self.inner.reply(message)
        }
        fn update_monitor(&mut self) {}
        fn search_and_pick(
            &mut self,
            _keyword: &str,
            _source: &str,
            _prefer_accompaniment: bool,
        ) -> std::result::Result<Option<PlaybackPickedCandidate>, PlaybackSearchFailure> {
            unreachable!("fallback uses the saved snapshot without searching again")
        }
        fn song_dedup_limited(&mut self, request: &PlaybackRequest) -> Result<bool> {
            self.inner.song_dedup_limited(request)
        }
        fn play_and_verify(&mut self, request: &PlaybackRequest) -> Result<PlaybackVerification> {
            self.requests.push(request.clone());
            if let Some(error) = self.errors.pop_front() {
                self.paused = self.pause_after_failure;
                bail!("{error}");
            }
            self.inner.play_and_verify(request)
        }
        fn is_track_unavailable_error(&self, error: &anyhow::Error) -> bool {
            error.to_string() == "track_unavailable"
        }
        fn item_scoped_playback_error_message(
            &self,
            error: &anyhow::Error,
        ) -> Option<&'static str> {
            (error.to_string() == "relogin_required").then_some("登录凭据已失效，请重新登录")
        }
        fn player_status(&mut self) -> Result<PlayerStatus> {
            self.inner.player_status()
        }
        fn playback_queue(&mut self) -> Result<Vec<QueueItem>> {
            self.inner.playback_queue()
        }
        fn remove_playback_queue(&mut self, removal: QueueRemoval) -> Result<()> {
            self.inner.remove_playback_queue(removal)
        }
        fn user_pause_active(&mut self) -> Result<bool> {
            Ok(self.paused)
        }
    }

    #[test]
    fn rejected_kugou_queue_head_advances_to_playable_next_track() {
        let next = test_track("miliastra://track/netease/next", "Next - Artist");
        let mut port = SourceFailurePort {
            inner: NavigationCommandPort {
                queue: vec![
                    QueueItem {
                        id: 1,
                        keyword: "Rejected".into(),
                        track: Some(test_track(
                            "miliastra://track/kugou/rejected",
                            "Song - Artist",
                        )),
                        ..QueueItem::default()
                    },
                    QueueItem {
                        id: 2,
                        keyword: "Next".into(),
                        track: Some(next.clone()),
                        ..QueueItem::default()
                    },
                ],
                verifications: [repeat_verification(&next)].into(),
                ..NavigationCommandPort::default()
            },
            errors: ["relogin_required"].into(),
            ..SourceFailurePort::default()
        };
        let played = repeat_application()
            .consume_queue_after_monitor("idle_keep_alive", &mut port)
            .unwrap();
        assert_eq!(
            played,
            Some(ConsumedSelection::Queue {
                keyword: "Next".into()
            })
        );
        assert!(port.inner.queue.is_empty());
        assert_eq!(port.requests.len(), 2);
        assert_eq!(port.inner.played_uris, [next.track_ref.key.to_string()]);
        assert_eq!(
            port.inner.replies[0],
            "kugou: 登录凭据已失效，请重新登录，已跳过当前歌曲"
        );
    }

    #[test]
    fn rejected_source_tries_each_matching_platform_once_and_retains_request_identity() {
        let original = test_track("miliastra://track/kugou/original", "Song - Artist");
        let netease = test_candidate("Song - Artist", "miliastra://track/netease/backup");
        let mut selection = pool_selection(&original);
        selection.requester = "friend".into();
        selection.queue_item_id = Some(11);
        selection.candidate_snapshot = vec![
            test_candidate("Song - Artist", "miliastra://track/kugou/same-provider"),
            test_candidate("Song - Artist", "miliastra://track/qqmusic/backup"),
            test_candidate("Song - Artist", "miliastra://track/qqmusic/duplicate"),
            test_candidate("Unrelated - Other", "miliastra://track/bilibili/wrong"),
            netease.clone(),
        ];
        let mut port = SourceFailurePort {
            inner: NavigationCommandPort {
                verifications: [repeat_verification(&netease.playable_track())].into(),
                ..NavigationCommandPort::default()
            },
            errors: ["relogin_required", "track_unavailable"].into(),
            ..SourceFailurePort::default()
        };
        let result = repeat_application()
            .play_confirmed(&selection, &mut port)
            .unwrap();
        assert_eq!(result.outcome(), PlaybackOutcome::Success);
        assert!(result.source_switched());
        assert_eq!(result.requested().uri(), original.track_ref.key.to_string());
        assert_eq!(
            result.final_request().uri(),
            netease.track_ref.key.to_string()
        );
        assert_eq!(
            port.requests.iter().map(request_source).collect::<Vec<_>>(),
            ["kugou", "qqmusic", "netease"]
        );
        assert!(
            port.requests
                .iter()
                .all(|request| request.requester == "friend" && request.queue_item_id == Some(11))
        );
        assert_eq!(port.inner.replies, ["已自动换源:Song-Artist"]);
    }

    #[test]
    fn alternate_credential_failure_is_skipped_but_player_failure_or_pause_preserves_head() {
        for (second_error, pause, removed) in [
            ("relogin_required", false, true),
            ("player unavailable", false, false),
            ("relogin_required", true, false),
        ] {
            let mut port = SourceFailurePort {
                inner: NavigationCommandPort {
                    queue: vec![QueueItem {
                        id: 3,
                        keyword: "Song".into(),
                        track: Some(test_track(
                            "miliastra://track/kugou/original",
                            "Song - Artist",
                        )),
                        candidate_snapshot: vec![test_candidate(
                            "Song - Artist",
                            "miliastra://track/qqmusic/backup",
                        )],
                        ..QueueItem::default()
                    }],
                    ..NavigationCommandPort::default()
                },
                errors: ["relogin_required", second_error].into(),
                pause_after_failure: pause,
                ..SourceFailurePort::default()
            };
            assert!(
                repeat_application()
                    .consume_queue_after_monitor("idle_keep_alive", &mut port)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(port.inner.queue.is_empty(), removed);
            assert_eq!(port.requests.len(), if pause { 1 } else { 2 });
            if removed {
                assert_eq!(port.inner.replies.len(), 1);
                assert!(port.inner.replies[0].contains("qqmusic: 登录凭据已失效"));
                assert!(port.inner.replies[0].ends_with("已跳过当前歌曲"));
            } else if pause {
                assert!(port.inner.replies.is_empty());
            } else {
                assert_eq!(port.inner.replies, ["player unavailable"]);
            }
        }
    }

    #[test]
    fn repeat_one_source_switch_replays_the_successful_source() {
        let original = test_track("miliastra://track/qqmusic/original", "Song - Artist");
        let alternate = test_candidate("Song - Artist", "miliastra://track/netease/alternate");
        let alternate_track = alternate.playable_track();
        let mut port = SourceSwitchRepeatPort {
            inner: NavigationCommandPort {
                verifications: [
                    repeat_verification(&alternate_track),
                    repeat_verification(&alternate_track),
                ]
                .into(),
                ..NavigationCommandPort::default()
            },
            unavailable: original.track_ref.key.clone(),
            attempted: Vec::new(),
        };
        let application = repeat_application();
        let mut selection = pool_selection(&original);
        selection.candidate_snapshot = vec![alternate];

        let result = application.play_confirmed(&selection, &mut port).unwrap();
        assert!(result.source_switched());
        application
            .consume_queue_after_monitor("natural end", &mut port)
            .unwrap();

        assert_eq!(
            port.attempted,
            [
                original.track_ref.key.to_string(),
                alternate_track.track_ref.key.to_string(),
                alternate_track.track_ref.key.to_string(),
            ]
        );
    }

    /// 只为"覆盖文案真的会到达玩家"这一件事存在的端口：它对指定模板键返回覆盖文案，
    /// 其余行为全部走 trait 默认实现（即内置默认文案）。
    ///
    /// 这样测试不依赖 composition 层就能验证：`reply_template` 确实经由
    /// `render_reply` 取文案，而不是绕过它直接拼内置字符串。
    struct OverridingPort {
        replies: Vec<String>,
        overrides: Vec<(&'static str, &'static str)>,
    }

    impl PlaybackExecutionPort for OverridingPort {
        fn reply(&mut self, message: &str) -> Result<()> {
            self.replies.push(message.to_string());
            Ok(())
        }

        fn render_reply(&mut self, spec: &ReplyTemplate, vars: &ReplyVars) -> String {
            match self
                .overrides
                .iter()
                .find(|(key, _)| *key == spec.key)
                .map(|(_, text)| *text)
            {
                // 覆盖文案同样支持占位符，走同一套渲染。
                Some(text) => {
                    reply_template::render(
                        &ReplyTemplate {
                            key: spec.key,
                            default: text,
                            summary: spec.summary,
                            group: spec.group,
                        },
                        None,
                        vars,
                    )
                    .text
                }
                None => reply_template::render(spec, None, vars).text,
            }
        }

        fn update_monitor(&mut self) {}
        fn search_and_pick(
            &mut self,
            _keyword: &str,
            _source: &str,
            _prefer_accompaniment: bool,
        ) -> std::result::Result<Option<PlaybackPickedCandidate>, PlaybackSearchFailure> {
            unreachable!("this port is only used for reply assertions")
        }
        fn song_dedup_limited(&mut self, _request: &PlaybackRequest) -> Result<bool> {
            Ok(false)
        }
        fn play_and_verify(&mut self, _request: &PlaybackRequest) -> Result<PlaybackVerification> {
            unreachable!("this port is only used for reply assertions")
        }
        fn player_status(&mut self) -> Result<PlayerStatus> {
            Ok(PlayerStatus::default())
        }
        fn playback_queue(&mut self) -> Result<Vec<QueueItem>> {
            Ok(Vec::new())
        }
        fn remove_playback_queue(&mut self, _removal: QueueRemoval) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn reply_template_goes_through_the_render_seam_so_overrides_reach_the_player() {
        let mut port = OverridingPort {
            replies: Vec::new(),
            overrides: vec![
                ("playback.volume.set", "音量调到 {音量} 啦"),
                ("playback.queue.empty", "队列空空的"),
            ],
        };

        // 默认实现（无覆盖）与覆盖后的实现都必须经由 render_reply。
        port.reply_template(
            &playback_templates::VOLUME_SET,
            &ReplyVars::new().with("音量", "80"),
        )
        .expect("templated reply");
        port.reply_template(&playback_templates::QUEUE_EMPTY, &ReplyVars::new())
            .expect("templated reply");

        assert_eq!(port.replies, ["音量调到 80 啦", "队列空空的"]);
    }

    #[test]
    fn full_queue_command_uses_batch_delivery_without_compressing_entries() {
        let mut port = NavigationCommandPort {
            previous_request: None,
            played_uris: Vec::new(),
            verifications: VecDeque::new(),
            replies: Vec::new(),
            batch_replies: Vec::new(),
            batch_delays: Vec::new(),
            queue: vec![
                QueueItem {
                    keyword: "晴天".to_string(),
                    ..QueueItem::default()
                },
                QueueItem {
                    keyword: "青花瓷".to_string(),
                    ..QueueItem::default()
                },
            ],
            pool: Vec::new(),
            status_updates: VecDeque::new(),
            clock: None,
            status: PlayerStatus::default(),
            stop_continuous_lyrics_after_statuses: None,
            status_calls: 0,
            background_lyrics_starts: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(50)),
            help_batch_ms: 321,
        });
        let context = PlaybackCommandContext {
            message_type: "blue".to_string(),
            username: "tester".to_string(),
            user_command: "@队列".to_string(),
        };

        application
            .execute_command(&context, &PlaybackCommand::Queue, &mut port)
            .expect("full queue command");

        assert!(port.replies.is_empty());
        assert_eq!(port.batch_delays, [321]);
        assert_eq!(
            port.batch_replies,
            vec![vec!["完整队列(2/20): 1.晴天, 2.青花瓷".to_string()]]
        );
    }

    #[test]
    fn timed_lyrics_starts_the_background_monitor_for_the_requested_duration() {
        let mut port = NavigationCommandPort {
            previous_request: None,
            played_uris: Vec::new(),
            verifications: VecDeque::new(),
            replies: Vec::new(),
            batch_replies: Vec::new(),
            batch_delays: Vec::new(),
            queue: Vec::new(),
            pool: Vec::new(),
            status_updates: VecDeque::new(),
            clock: None,
            status: PlayerStatus::default(),
            stop_continuous_lyrics_after_statuses: None,
            status_calls: 0,
            background_lyrics_starts: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(1_000)),
            help_batch_ms: 0,
        });
        let context = PlaybackCommandContext {
            message_type: "blue".to_string(),
            username: "tester".to_string(),
            user_command: "@歌词 5".to_string(),
        };

        application
            .execute_command(&context, &PlaybackCommand::LyricsFor(5), &mut port)
            .expect("timed lyrics command");

        assert_eq!(
            port.background_lyrics_starts,
            [(
                Some(Duration::from_secs(5)),
                BackgroundLyricsScope::AllSongs
            )]
        );
        assert_eq!(
            port.replies,
            ["限时歌词已开启，将持续5秒；默认显示翻译，发送@切换可切换原文/翻译"]
        );
    }

    #[test]
    fn single_song_lyrics_starts_the_current_song_background_monitor() {
        let mut port = NavigationCommandPort {
            previous_request: None,
            played_uris: Vec::new(),
            verifications: VecDeque::new(),
            replies: Vec::new(),
            batch_replies: Vec::new(),
            batch_delays: Vec::new(),
            queue: Vec::new(),
            pool: Vec::new(),
            status_updates: VecDeque::new(),
            clock: None,
            status: PlayerStatus::default(),
            stop_continuous_lyrics_after_statuses: None,
            status_calls: 0,
            background_lyrics_starts: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(1_000)),
            help_batch_ms: 0,
        });
        let context = PlaybackCommandContext {
            message_type: "blue".to_string(),
            username: "tester".to_string(),
            user_command: "@单曲歌词".to_string(),
        };

        application
            .execute_command(&context, &PlaybackCommand::SingleSongLyrics, &mut port)
            .expect("single-song lyrics command");

        assert_eq!(
            port.background_lyrics_starts,
            [(None, BackgroundLyricsScope::CurrentSong)]
        );
        assert_eq!(
            port.replies,
            ["单曲后台歌词已开启；默认显示翻译，发送@切换可切换原文/翻译；切歌后自动停止"]
        );
    }

    #[test]
    fn continuous_lyrics_follows_song_changes_until_a_formal_task_is_queued() {
        let start = Instant::now();
        let clock = Arc::new(ManualClock::new(start));
        let playing = |uri: &str, lyric: &str| PlayerStatus {
            status: "playing".to_string(),
            current_track: Some(test_track(uri, "lyrics test - test artist")),
            current_uri: uri.to_string(),
            lyric_line_text: lyric.to_string(),
            ..PlayerStatus::default()
        };
        let mut port = NavigationCommandPort {
            previous_request: None,
            played_uris: Vec::new(),
            verifications: VecDeque::new(),
            replies: Vec::new(),
            batch_replies: Vec::new(),
            batch_delays: Vec::new(),
            queue: Vec::new(),
            pool: Vec::new(),
            status_updates: VecDeque::from([
                playing("miliastra://track/qqmusic/1", "第一句"),
                playing("miliastra://track/qqmusic/1", "第一句"),
                playing("miliastra://track/qqmusic/2", "新歌第一句"),
            ]),
            clock: Some(Arc::clone(&clock)),
            status: PlayerStatus::default(),
            stop_continuous_lyrics_after_statuses: Some(3),
            status_calls: 0,
            background_lyrics_starts: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(1_000)),
            help_batch_ms: 0,
        });
        let context = PlaybackCommandContext {
            message_type: "blue".to_string(),
            username: "tester".to_string(),
            user_command: "@持续歌词".to_string(),
        };

        application
            .execute_command(&context, &PlaybackCommand::ContinuousLyrics, &mut port)
            .expect("continuous lyrics command");

        assert_eq!(port.replies, ["歌词: 第一句", "歌词: 新歌第一句"]);
        assert_eq!(port.status_calls, 3);
    }

    #[test]
    fn full_queue_messages_split_without_dropping_long_entries() {
        let queue = [
            QueueItem {
                keyword: "甲".repeat(30),
                ..QueueItem::default()
            },
            QueueItem {
                keyword: "乙".repeat(30),
                ..QueueItem::default()
            },
        ];

        let messages = split_full_queue_messages(
            &queue,
            20,
            &playback_templates::QUEUE_HEADER,
            &playback_templates::QUEUE_ENTRY,
            &mut |spec, vars| reply_template::render(spec, None, vars).text,
        );
        let combined = messages.join(" ");

        assert!(messages.len() > 1);
        assert!(
            messages
                .iter()
                .all(|message| display_width(message) <= MAX_CHAT_WIDTH)
        );
        assert!(combined.contains(&format!("1.{}", "甲".repeat(30))));
        assert!(combined.contains(&format!("2.{}", "乙".repeat(30))));
    }

    #[test]
    fn next_command_with_empty_queue_does_not_forward_to_engine_navigation() {
        // 队列与播放池都为空：Next 静默完成并回复当前状态。
        let mut port = NavigationCommandPort {
            previous_request: None,
            played_uris: Vec::new(),
            verifications: VecDeque::new(),
            replies: Vec::new(),
            batch_replies: Vec::new(),
            batch_delays: Vec::new(),
            queue: Vec::new(),
            pool: Vec::new(),
            status_updates: VecDeque::new(),
            clock: None,
            status: PlayerStatus::default(),
            stop_continuous_lyrics_after_statuses: None,
            status_calls: 0,
            background_lyrics_starts: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(50)),
            help_batch_ms: 0,
        });
        let context = PlaybackCommandContext {
            message_type: "blue".to_string(),
            username: "tester".to_string(),
            user_command: "@下一首".to_string(),
        };

        application
            .execute_command(&context, &PlaybackCommand::Next, &mut port)
            .expect("next command with empty queue must not fail");

        assert!(port.played_uris.is_empty());
        assert_eq!(port.replies, ["队列和播放池都为空，无可播放歌曲"]);
    }

    #[test]
    fn next_command_with_empty_queue_plays_a_pool_track_and_replies_its_info() {
        // 队列空但播放池有歌：Next 从池中随机播一首，并直接返回歌曲信息。
        let pool_uri = "miliastra://track/kugou/pool1";
        let mut port = NavigationCommandPort {
            previous_request: None,
            played_uris: Vec::new(),
            verifications: VecDeque::from([PlaybackVerification::Success {
                status: PlayerStatus {
                    status: "playing".to_string(),
                    current_uri: pool_uri.to_string(),
                    name: "播放池歌曲 - 测试歌手".to_string(),
                    duration: 180.0,
                    progress: 1.0,
                    ..PlayerStatus::default()
                },
                message: "开始播放: 播放池歌曲 - 测试歌手".to_string(),
            }]),
            replies: Vec::new(),
            batch_replies: Vec::new(),
            batch_delays: Vec::new(),
            queue: Vec::new(),
            pool: vec![test_track(pool_uri, "播放池歌曲 - 测试歌手")],
            status_updates: VecDeque::new(),
            clock: None,
            status: PlayerStatus::default(),
            stop_continuous_lyrics_after_statuses: None,
            status_calls: 0,
            background_lyrics_starts: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(50)),
            help_batch_ms: 0,
        });
        let context = PlaybackCommandContext {
            message_type: "blue".to_string(),
            username: "tester".to_string(),
            user_command: "@下一首".to_string(),
        };

        application
            .execute_command(&context, &PlaybackCommand::Next, &mut port)
            .expect("next command with pool track must succeed");

        assert_eq!(port.played_uris, [pool_uri]);
        assert_eq!(
            port.replies,
            ["播放列表为空，随机续播: 播放池歌曲 - 测试歌手"]
        );
    }

    #[test]
    fn delete_command_removes_current_pool_track_and_plays_the_next_one() {
        let current_uri = "miliastra://track/qqmusic/current-pool";
        let next_uri = "miliastra://track/qqmusic/next-pool";
        let current_track = test_track(current_uri, "待删除歌曲 - 歌手A");
        let mut port = NavigationCommandPort {
            previous_request: None,
            played_uris: Vec::new(),
            verifications: VecDeque::from([PlaybackVerification::Success {
                status: PlayerStatus {
                    status: "playing".to_string(),
                    current_uri: next_uri.to_string(),
                    ..PlayerStatus::default()
                },
                message: "开始播放: 下一首歌曲 - 歌手B".to_string(),
            }]),
            replies: Vec::new(),
            batch_replies: Vec::new(),
            batch_delays: Vec::new(),
            queue: Vec::new(),
            pool: vec![
                current_track.clone(),
                test_track(next_uri, "下一首歌曲 - 歌手B"),
            ],
            status_updates: VecDeque::new(),
            clock: None,
            status: PlayerStatus {
                status: "playing".to_string(),
                current_uri: current_uri.to_string(),
                current_track: Some(current_track),
                ..PlayerStatus::default()
            },
            stop_continuous_lyrics_after_statuses: None,
            status_calls: 0,
            background_lyrics_starts: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(50)),
            help_batch_ms: 0,
        });
        let context = PlaybackCommandContext {
            message_type: "blue".to_string(),
            username: "tester".to_string(),
            user_command: "@删除".to_string(),
        };

        application
            .execute_command(
                &context,
                &PlaybackCommand::DeleteCurrentPoolTrack,
                &mut port,
            )
            .expect("delete command must succeed");

        assert_eq!(port.pool.len(), 1);
        assert_eq!(port.pool[0].track_ref.key.id, "next-pool");
        assert_eq!(port.played_uris, [next_uri]);
        assert_eq!(
            port.replies,
            ["已从播放池删除当前歌曲，随机续播: 下一首歌曲 - 歌手B"]
        );
    }

    #[test]
    fn delete_command_still_plays_next_when_current_track_is_not_in_pool() {
        let current_track = test_track("miliastra://track/qqmusic/outside", "池外歌曲 - 歌手A");
        let next_uri = "miliastra://track/qqmusic/pool-next";
        let mut port = NavigationCommandPort {
            previous_request: None,
            played_uris: Vec::new(),
            verifications: VecDeque::from([PlaybackVerification::Success {
                status: PlayerStatus {
                    status: "playing".to_string(),
                    current_uri: next_uri.to_string(),
                    ..PlayerStatus::default()
                },
                message: "开始播放: 池内歌曲 - 歌手B".to_string(),
            }]),
            replies: Vec::new(),
            batch_replies: Vec::new(),
            batch_delays: Vec::new(),
            queue: Vec::new(),
            pool: vec![test_track(next_uri, "池内歌曲 - 歌手B")],
            status_updates: VecDeque::new(),
            clock: None,
            status: PlayerStatus {
                status: "playing".to_string(),
                current_track: Some(current_track),
                ..PlayerStatus::default()
            },
            stop_continuous_lyrics_after_statuses: None,
            status_calls: 0,
            background_lyrics_starts: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(50)),
            help_batch_ms: 0,
        });
        let context = PlaybackCommandContext {
            message_type: "blue".to_string(),
            username: "tester".to_string(),
            user_command: "@删除".to_string(),
        };

        application
            .execute_command(
                &context,
                &PlaybackCommand::DeleteCurrentPoolTrack,
                &mut port,
            )
            .expect("delete command must advance even when current track is outside pool");

        assert_eq!(port.pool.len(), 1);
        assert_eq!(port.played_uris, [next_uri]);
        assert_eq!(
            port.replies,
            ["当前歌曲不在播放池中，随机续播: 池内歌曲 - 歌手B"]
        );
    }

    #[test]
    fn next_command_with_queue_item_plays_the_queue_head_and_replies_it() {
        // 队列有歌：Next 严格播队列第一首，回复「正在播放: 点歌词」。
        let queue_uri = "miliastra://track/qqmusic/77";
        let mut port = NavigationCommandPort {
            previous_request: None,
            played_uris: Vec::new(),
            verifications: VecDeque::from([PlaybackVerification::Success {
                status: PlayerStatus {
                    status: "playing".to_string(),
                    current_uri: queue_uri.to_string(),
                    name: "队列歌曲 - 测试歌手".to_string(),
                    duration: 180.0,
                    progress: 1.0,
                    ..PlayerStatus::default()
                },
                message: "开始播放: 队列歌曲 - 测试歌手".to_string(),
            }]),
            replies: Vec::new(),
            batch_replies: Vec::new(),
            batch_delays: Vec::new(),
            queue: vec![QueueItem {
                id: 77,
                keyword: "队列歌曲".to_string(),
                track: Some(test_track(queue_uri, "队列歌曲 - 测试歌手")),
                ..QueueItem::default()
            }],
            pool: vec![test_track(
                "miliastra://track/kugou/pool77",
                "池歌曲 - 测试歌手",
            )],
            status_updates: VecDeque::new(),
            clock: None,
            status: PlayerStatus::default(),
            stop_continuous_lyrics_after_statuses: None,
            status_calls: 0,
            background_lyrics_starts: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(50)),
            help_batch_ms: 0,
        });
        let context = PlaybackCommandContext {
            message_type: "blue".to_string(),
            username: "tester".to_string(),
            user_command: "@下一首".to_string(),
        };

        application
            .execute_command(&context, &PlaybackCommand::Next, &mut port)
            .expect("next command with queue item must succeed");

        assert_eq!(port.played_uris, [queue_uri]);
        assert_eq!(port.replies, ["开始播放: 队列歌曲 - 测试歌手"]);
    }

    #[test]
    fn previous_command_without_history_does_not_forward_to_engine_navigation() {
        // 无历史请求：Previous 静默完成并回复当前状态。
        let mut port = NavigationCommandPort {
            previous_request: None,
            played_uris: Vec::new(),
            verifications: VecDeque::new(),
            replies: Vec::new(),
            batch_replies: Vec::new(),
            batch_delays: Vec::new(),
            queue: Vec::new(),
            pool: Vec::new(),
            status_updates: VecDeque::new(),
            clock: None,
            status: PlayerStatus::default(),
            stop_continuous_lyrics_after_statuses: None,
            status_calls: 0,
            background_lyrics_starts: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: Arc::new(RwLock::new(50)),
            monitor_status_ms: Arc::new(RwLock::new(50)),
            help_batch_ms: 0,
        });
        let context = PlaybackCommandContext {
            message_type: "blue".to_string(),
            username: "tester".to_string(),
            user_command: "@上一首".to_string(),
        };

        application
            .execute_command(&context, &PlaybackCommand::Previous, &mut port)
            .expect("previous command without history must not fail");

        assert_eq!(port.played_uris, Vec::<String>::new());
        assert_eq!(port.replies, ["没有上一曲记录"]);
    }
}
