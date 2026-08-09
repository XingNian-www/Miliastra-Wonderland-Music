use std::fmt::{Display, Formatter};
use std::time::{Duration, Instant};

use anyhow::Result;
use miliastra_playback::{PlayableTrack, TrackKey};

use crate::features::song_request::SearchCandidate;
use crate::text::{MAX_CHAT_WIDTH, char_width, display_width};

use super::matcher::same_song_query;
use super::{
    BackgroundLyricsScope, PlaybackCommand, PlaybackNavigation, PlaybackOutcome, PlaybackRequest,
    PlaybackSnapshot, PlaybackVerification, PlayerStatus, QueueAdvanceContext,
    QueueAdvanceDecision, QueueItem, QueueRemoval, estimated_player_status, format_lyrics,
    format_play_message, format_status,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlaybackCommandContext {
    pub(crate) message_type: String,
    pub(crate) username: String,
    pub(crate) user_command: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AlternatePlaybackSource {
    QqMusic,
    Netease,
}

impl AlternatePlaybackSource {
    fn other_than(current: &str) -> Self {
        if current == "netease" {
            Self::QqMusic
        } else {
            Self::Netease
        }
    }

    const fn id(self) -> &'static str {
        match self {
            Self::QqMusic => "qqmusic",
            Self::Netease => "netease",
        }
    }
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

    fn dedup_reject_message(&self) -> String {
        format!("{}近期已播放过,请稍后再点", self.keyword)
    }

    fn dedup_skip_message(&self) -> String {
        format!("{}近期已播放过,已跳过", self.keyword)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaybackPurpose {
    Requested,
    Queue { stop_when_user_paused: bool },
    Pool { stop_when_user_paused: bool },
    Previous,
    SourceRetry { stop_when_user_paused: bool },
}

impl PlaybackPurpose {
    const fn stop_when_user_paused(self) -> bool {
        match self {
            Self::Queue {
                stop_when_user_paused,
            }
            | Self::Pool {
                stop_when_user_paused,
            }
            | Self::SourceRetry {
                stop_when_user_paused,
            } => stop_when_user_paused,
            Self::Requested | Self::Previous => false,
        }
    }

    const fn checks_dedup(self) -> bool {
        matches!(self, Self::Requested | Self::Queue { .. })
    }

    const fn allows_source_switch(self) -> bool {
        matches!(
            self,
            Self::Requested | Self::Queue { .. } | Self::Pool { .. }
        )
    }

    const fn replies_with_play_message(self) -> bool {
        !matches!(self, Self::SourceRetry { .. } | Self::Pool { .. })
    }

    fn dedup_reply(self, selection: &PlaybackSelection) -> Option<String> {
        match self {
            Self::Requested => Some(selection.dedup_reject_message()),
            Self::Queue { .. } => Some(selection.dedup_skip_message()),
            Self::Previous | Self::SourceRetry { .. } | Self::Pool { .. } => None,
        }
    }
}

struct PlaybackCompletion {
    result: PlaybackResult,
    reply: Option<String>,
    update_monitor: bool,
}

impl PlaybackCompletion {
    fn new(result: PlaybackResult, reply: Option<String>, update_monitor: bool) -> Self {
        Self {
            result,
            reply,
            update_monitor,
        }
    }
}

pub(crate) trait PlaybackExecutionPort {
    fn reply(&mut self, message: &str) -> Result<()>;
    fn update_monitor(&mut self);
    fn search_and_pick(
        &mut self,
        keyword: &str,
        source: &str,
        prefer_accompaniment: bool,
    ) -> std::result::Result<Option<PlaybackPickedCandidate>, PlaybackSearchFailure>;
    #[allow(dead_code)]
    fn ai_search_and_pick(
        &mut self,
        _keyword: &str,
        _source: &str,
        _prefer_accompaniment: bool,
    ) -> std::result::Result<Option<PlaybackPickedCandidate>, PlaybackSearchFailure> {
        Err(PlaybackSearchFailure::Unavailable(
            "点歌 AI 未启用".to_string(),
        ))
    }
    fn song_dedup_limited(&mut self, request: &PlaybackRequest) -> Result<bool>;
    fn play_and_verify(&mut self, request: &PlaybackRequest) -> Result<PlaybackVerification>;
    fn is_track_unavailable_error(&self, _error: &anyhow::Error) -> bool {
        false
    }
    fn player_status(&mut self) -> Result<PlayerStatus>;
    fn playback_queue(&mut self) -> Result<Vec<QueueItem>>;
    /// 从播放池随机挑一首（排除 `exclude`），用于队列播完后的随机播放。
    fn pick_playback_pool_track(
        &mut self,
        _exclude: Option<&TrackKey>,
    ) -> Result<Option<PlayableTrack>> {
        Ok(None)
    }
    fn remove_playback_queue(&mut self, removal: QueueRemoval) -> Result<()>;
    fn user_pause_active(&mut self) -> Result<bool> {
        Ok(false)
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
    fn next_external(&mut self) -> Result<String>;
    fn previous_external(&mut self) -> Result<String>;
    fn set_volume(&mut self, volume: &str) -> Result<()>;
    fn remove_playback_queue_indexes(
        &mut self,
        indexes: Vec<usize>,
    ) -> Result<Vec<(usize, QueueItem)>>;
    fn clear_playback_queue(&mut self) -> Result<usize>;
    /// Returns true when a queued formal task should interrupt continuous lyrics.
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

#[derive(Clone, Copy, Debug)]
pub(crate) struct PlaybackApplicationConfig {
    pub(crate) console_bypass_dedup: bool,
    pub(crate) queue_max_size: usize,
    pub(crate) monitor_tick_ms: u64,
    pub(crate) monitor_status_ms: u64,
    pub(crate) help_batch_ms: u64,
}

#[derive(Default)]
pub(crate) struct LyricTracker {
    current_key: Option<TrackKey>,
    previous_lyric: Option<String>,
}

impl LyricTracker {
    pub(crate) fn observe(&mut self, status: &PlayerStatus) -> bool {
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

#[derive(Clone, Debug)]
pub(crate) struct PlaybackApplication {
    config: PlaybackApplicationConfig,
}

impl PlaybackApplication {
    pub(crate) const fn new(config: PlaybackApplicationConfig) -> Self {
        Self { config }
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
                port.reply("已暂停")?;
            }
            PlaybackCommand::Resume | PlaybackCommand::Play => {
                port.resume_by_user()?;
                port.log_executed(context, "resume")?;
                port.update_monitor();
                port.reply("已恢复播放")?;
            }
            PlaybackCommand::Next => {
                if !port.playback_queue()?.is_empty() {
                    self.consume_queue("手动下一首", port)?;
                    port.log_executed(context, "next queue")?;
                } else {
                    let _message = port.next_external()?;
                    port.update_monitor();
                    port.log_executed(context, "next native playback")?;
                    self.reply_player_status_after_skip(port)?;
                }
            }
            PlaybackCommand::Previous => {
                if let Some(request) = port.previous_playback_request()? {
                    let completion =
                        self.run_request(&request, &request, PlaybackPurpose::Previous, port)?;
                    self.finish_playback(completion, port)?;
                    port.log_executed(context, "previous uri")?;
                } else {
                    let _message = port.previous_external()?;
                    port.update_monitor();
                    port.log_executed(context, "previous")?;
                    self.reply_player_status_after_skip(port)?;
                }
            }
            PlaybackCommand::Volume(volume) => {
                port.set_volume(volume)?;
                port.log_executed(context, &format!("volume {}", volume))?;
                port.reply(&format!("音量已设置为 {}", volume))?;
            }
            PlaybackCommand::Status => {
                let status = port.player_status()?;
                port.log_executed(context, "status")?;
                port.reply(&format_status(&status))?;
            }
            PlaybackCommand::Lyrics => {
                let status = port.player_status()?;
                port.log_executed(context, "lyrics")?;
                port.reply(&format_lyrics(&status))?;
            }
            PlaybackCommand::LyricsFor(seconds) => {
                let started = port.start_background_lyrics(
                    Some(Duration::from_secs(u64::from(*seconds))),
                    BackgroundLyricsScope::AllSongs,
                )?;
                port.log_executed(context, &format!("lyrics {}s", seconds))?;
                let message = if started {
                    format!(
                        "限时歌词已开启，将持续{}秒；正式命令执行期间暂不发送，完成后自动继续",
                        seconds
                    )
                } else {
                    "后台歌词已经在运行".to_string()
                };
                port.reply(&message)?;
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
                port.reply(if started {
                    "后台歌词已开启，正式命令执行期间暂不发送，完成后自动继续"
                } else {
                    "后台歌词已经在运行"
                })?;
            }
            PlaybackCommand::SingleSongLyrics => {
                let started =
                    port.start_background_lyrics(None, BackgroundLyricsScope::CurrentSong)?;
                port.log_executed(context, "lyrics single song")?;
                port.reply(if started {
                    "单曲后台歌词已开启，切歌后自动停止"
                } else {
                    "后台歌词已经在运行"
                })?;
            }
            PlaybackCommand::StopBackgroundLyrics => {
                let stopped = port.stop_background_lyrics()?;
                port.log_executed(context, "lyrics background stop")?;
                port.reply(if stopped {
                    "后台歌词已停止"
                } else {
                    "后台歌词当前未运行"
                })?;
            }
            PlaybackCommand::Queue => {
                port.log_executed(context, "queue list")?;
                self.reply_full_queue(port)?;
            }
            PlaybackCommand::QueueDelete(indexes) => {
                if indexes.is_empty() {
                    port.log_executed(context, "queue delete invalid")?;
                    port.reply("没有匹配到有效队列序号")?;
                    return Ok(());
                }
                let removed = port.remove_playback_queue_indexes(indexes.clone())?;
                if removed.is_empty() {
                    port.log_executed(context, "queue delete none")?;
                    port.reply("队列删除失败或序号不存在")?;
                } else {
                    let removed_text = removed
                        .iter()
                        .map(|(index, item)| format!("{}.{}", index, item.keyword))
                        .collect::<Vec<_>>()
                        .join(", ");
                    port.log_executed(context, &format!("queue delete {}", removed_text))?;
                    port.reply(&format!("队列已删除: {}", removed_text))?;
                }
            }
            PlaybackCommand::QueueClear => {
                let count = port.clear_playback_queue()?;
                port.log_executed(context, &format!("queue clear {}", count))?;
                if count == 0 {
                    port.reply("队列为空")?;
                } else {
                    port.reply(&format!("队列已清空: {} 首", count))?;
                }
            }
        }
        Ok(())
    }

    fn reply_full_queue<P: PlaybackCommandPort + ?Sized>(&self, port: &mut P) -> Result<()> {
        let queue = port.playback_queue()?;
        if queue.is_empty() {
            return port.reply("队列为空");
        }

        let messages = split_full_queue_messages(&queue, self.config.queue_max_size);
        port.reply_batch(&messages, self.config.help_batch_ms)
    }

    fn reply_continuous_lyrics<P: PlaybackCommandPort + ?Sized>(&self, port: &mut P) -> Result<()> {
        let poll = Duration::from_millis(self.config.monitor_status_ms.max(1));
        let mut lyric_tracker = LyricTracker::default();

        loop {
            if port.should_stop_continuous_lyrics()? {
                log::info!("持续歌词输出因正式任务到来结束");
                break;
            }

            match port.player_status() {
                Ok(status) => {
                    if lyric_tracker.observe(&status) {
                        port.reply(&format_lyrics(&status))?;
                    }
                }
                Err(error) => {
                    log::warn!("持续歌词读取播放器状态失败: {error:#}");
                }
            }

            port.wait(poll);
        }
        Ok(())
    }

    /// 内置播放器切歌同步生效，直接读一次状态回复。
    fn reply_player_status_after_skip<P: PlaybackCommandPort + ?Sized>(
        &self,
        port: &mut P,
    ) -> Result<()> {
        match port.player_status() {
            Ok(status) if super::is_playing(&status) || status.status == "paused" => {
                return port.reply(&format_play_message(&status));
            }
            Ok(_) => {}
            Err(error) => {
                log::error!("切歌后查询播放状态失败: {error:#}");
            }
        }
        port.reply("切歌完成")
    }

    pub(crate) fn run_monitor_loop<P: PlaybackMonitorPort + ?Sized>(&self, port: &mut P) {
        let tick_ms = self.config.monitor_tick_ms.max(50);
        let status_ms = self.config.monitor_status_ms.max(tick_ms);
        let mut snapshot: Option<PlaybackSnapshot> = None;
        let mut next_status_at = port.now();
        while port.is_running() {
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
                        log::error!("播放监控状态查询失败: {error:#}");
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
                        log::error!("播放监控处理失败: {error:#}");
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

    pub(crate) fn consume_queue<P: PlaybackExecutionPort + ?Sized>(
        &self,
        reason: &str,
        port: &mut P,
    ) -> Result<()> {
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
    ) -> Result<()> {
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
    ) -> Result<()> {
        // 播放池随机播放时排除最近尝试过的歌曲，避免失效歌曲原地死循环
        let mut pool_exclude: Option<TrackKey> = None;
        loop {
            if purpose.stop_when_user_paused() && port.user_pause_active()? {
                log::info!("自动出队已跳过: 播放器处于用户暂停状态");
                return Ok(());
            }
            let Some(item) = port.playback_queue()?.into_iter().next() else {
                let Some(track) = port.pick_playback_pool_track(pool_exclude.as_ref())? else {
                    return Ok(());
                };
                pool_exclude = Some(track.track_ref.key.clone());
                let pool_purpose = PlaybackPurpose::Pool {
                    stop_when_user_paused: purpose.stop_when_user_paused(),
                };
                let selection = pool_selection(&track);
                log::info!("播放池随机播放({}): {}", reason, selection.keyword);
                let completion = self.run_selection(&selection, pool_purpose, port)?;
                match self.finish_playback(completion, port)?.outcome() {
                    PlaybackOutcome::Success => return Ok(()),
                    PlaybackOutcome::QueueBlockingFailure => return Ok(()),
                    _ => {
                        log::warn!("播放池歌曲不可用，已跳过: {}", selection.keyword);
                        continue;
                    }
                }
            };
            pool_exclude = None;
            log::info!("消费队列({}): {}", reason, item.keyword);
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
            };
            let completion = self.run_selection(&request, purpose, port)?;
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
                    // 预加载后续曲目音源，缩短下次切歌延迟；失败静默（播放时重新解析）。
                    let next = port
                        .playback_queue()?
                        .into_iter()
                        .next()
                        .and_then(|item| item.track)
                        .or_else(|| {
                            port.pick_playback_pool_track(pool_exclude.as_ref())
                                .ok()
                                .flatten()
                        });
                    if let Some(track) = next {
                        if let Err(error) = port.preload_track(&track) {
                            log::debug!("预加载下一首音源失败: {error:#}");
                        }
                    }
                    return Ok(());
                }
                PlaybackOutcome::ItemScopedFailure => {
                    log::error!("队列项不可播放，已丢弃: {}", item.keyword);
                }
                PlaybackOutcome::QueueBlockingFailure => {
                    log::error!("队列项播放被阻断，保留在队首: {}", item.keyword);
                    return Ok(());
                }
                PlaybackOutcome::DedupLimited => {
                    log::info!("队列项近期已播放过，已跳过: {}", item.keyword);
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
        self.finish_playback(completion, port)
    }

    fn run_selection<P: PlaybackExecutionPort + ?Sized>(
        &self,
        selection: &PlaybackSelection,
        purpose: PlaybackPurpose,
        port: &mut P,
    ) -> Result<PlaybackCompletion> {
        let requested = selection.request();
        if purpose.stop_when_user_paused() && port.user_pause_active()? {
            log::info!("自动出队已跳过: 用户暂停状态在播放前生效");
            return Ok(PlaybackCompletion::new(
                PlaybackResult::error(&requested, &requested, None, "用户暂停"),
                None,
                false,
            ));
        }
        if purpose.checks_dedup() && self.selection_dedup_limited(selection, port)? {
            log::info!(
                "长时间同歌去重拦截: keyword={} uri={}",
                selection.keyword,
                selection
                    .track
                    .as_ref()
                    .map(|track| track.track_ref.key.to_string())
                    .unwrap_or_default()
            );
            return Ok(PlaybackCompletion::new(
                PlaybackResult::dedup_limited(&requested),
                purpose.dedup_reply(selection),
                false,
            ));
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
                        Some("平台无对应歌曲音源".to_string()),
                        false,
                    ));
                }
                Err(error) => {
                    log::error!("点歌搜索失败: {error}");
                    return Ok(PlaybackCompletion::new(
                        PlaybackResult::error(&requested, &requested, None, error.to_string()),
                        Some(format!("{}{}", selection.label(), error.user_message())),
                        false,
                    ));
                }
            };
            log::info!(
                "播放器候选: {} -> {}",
                picked.text,
                picked.track.track_ref.key
            );
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
        if purpose.stop_when_user_paused() && port.user_pause_active()? {
            log::info!("自动出队已跳过: 用户暂停状态在播放请求发送前生效");
            return Ok(PlaybackCompletion::new(
                PlaybackResult::error(requested, request, None, "用户暂停"),
                None,
                false,
            ));
        }
        let verification = match port.play_and_verify(request) {
            Ok(verification) => verification,
            Err(error)
                if purpose.allows_source_switch() && port.is_track_unavailable_error(&error) =>
            {
                log::info!(
                    "请求歌曲已被播放器标记为不可用，尝试备用来源: uri={}",
                    request.uri()
                );
                return self.switch_source_and_play(requested, request, None, purpose, port);
            }
            Err(error) => {
                if matches!(purpose, PlaybackPurpose::SourceRetry { .. })
                    && port.is_track_unavailable_error(&error)
                {
                    log::info!(
                        "备用来源也已确认无音源，完成项目级失败: uri={}",
                        request.uri()
                    );
                    return Ok(PlaybackCompletion::new(
                        PlaybackResult::no_source(
                            requested,
                            request,
                            None,
                            "所有候选平台均无对应歌曲音源",
                        ),
                        Some("平台无对应歌曲音源".to_string()),
                        false,
                    ));
                }
                let message = error.to_string();
                let reply = if message.trim().is_empty() {
                    "平台无对应歌曲音源".to_string()
                } else {
                    message.trim().to_string()
                };
                log::error!("播放候选失败: {reply}");
                return Ok(PlaybackCompletion::new(
                    PlaybackResult::error(requested, request, None, reply.clone()),
                    purpose.replies_with_play_message().then_some(reply),
                    false,
                ));
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
        _mismatch_status: Option<&PlayerStatus>,
        purpose: PlaybackPurpose,
        port: &mut P,
    ) -> Result<PlaybackCompletion> {
        let current_source = request_source(request);
        let next_source = AlternatePlaybackSource::other_than(current_source);
        let candidate = request.track.as_ref().and_then(|reference| {
            select_snapshot_candidate(
                &request.candidate_snapshot,
                next_source.id(),
                request.prefer_accompaniment,
                reference,
            )
        });
        let Some(candidate) = candidate else {
            return Ok(PlaybackCompletion::new(
                PlaybackResult::no_source(
                    requested,
                    request,
                    None,
                    format!("{} 平台无对应歌曲音源", next_source.id()),
                ),
                purpose
                    .replies_with_play_message()
                    .then(|| "平台无对应歌曲音源".to_string()),
                true,
            ));
        };
        let picked = PlaybackPickedCandidate {
            text: candidate.text.clone(),
            track: candidate.playable_track(),
            candidate_snapshot: request.candidate_snapshot.clone(),
        };
        log::info!(
            "AI 自动换源候选: source={} keyword={} uri={}",
            next_source.id(),
            picked.text,
            picked.track.track_ref.key
        );
        let resolved = PlaybackSelection {
            keyword: picked.text.clone(),
            source: next_source.id().to_string(),
            prefer_accompaniment: request.prefer_accompaniment,
            ai_original_text: String::new(),
            track: Some(picked.track),
            friend_username: String::new(),
            requester: request.requester.clone(),
            console_bypass_dedup: false,
            candidate_snapshot: request.candidate_snapshot.clone(),
        };
        let mut completion = self.run_request(
            requested,
            &resolved.request(),
            PlaybackPurpose::SourceRetry {
                stop_when_user_paused: purpose.stop_when_user_paused(),
            },
            port,
        )?;
        if completion.result.outcome() == PlaybackOutcome::Success {
            completion.reply = Some(format!(
                "因平台无音源,已由AI自动切换至:{}",
                compact_candidate_title(&resolved.keyword)
            ));
        }
        Ok(completion)
    }

    fn finish_playback<P: PlaybackExecutionPort + ?Sized>(
        &self,
        completion: PlaybackCompletion,
        port: &mut P,
    ) -> Result<PlaybackResult> {
        if completion.update_monitor {
            port.update_monitor();
        }
        if let Some(reply) = completion.reply {
            port.reply(&reply)?;
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

fn split_full_queue_messages(queue: &[QueueItem], queue_max_size: usize) -> Vec<String> {
    let header = format!("完整队列({}/{}): ", queue.len(), queue_max_size);
    let mut messages = Vec::new();
    let mut current = header;
    let mut has_entry = false;

    for (index, item) in queue.iter().enumerate() {
        let entry = format!("{}.{}", index + 1, item.keyword);
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

    struct MonitorPort {
        clock: Arc<ManualClock>,
        waits: usize,
        status_reads: usize,
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
            self.clock.advance(duration).unwrap();
            self.waits += 1;
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
        play_attempts: usize,
        ai_searches: Vec<(String, String, bool)>,
        pool: Vec<PlayableTrack>,
    }

    struct VerifyingPlaybackPort {
        queue: Vec<QueueItem>,
        verifications: VecDeque<PlaybackVerification>,
        removed_ids: Vec<u64>,
        replies: Vec<String>,
        reply_error: bool,
        #[allow(dead_code)]
        ai_search_result:
            std::result::Result<Option<PlaybackPickedCandidate>, PlaybackSearchFailure>,
        ai_search_requests: Vec<(String, String, bool)>,
        played_uris: Vec<String>,
        user_paused: bool,
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
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: 50,
            monitor_status_ms: 100,
            help_batch_ms: 0,
        });

        application.run_monitor_loop(&mut port);

        assert_eq!(port.status_reads, 2);
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

        fn ai_search_and_pick(
            &mut self,
            keyword: &str,
            source: &str,
            prefer_accompaniment: bool,
        ) -> std::result::Result<Option<PlaybackPickedCandidate>, PlaybackSearchFailure> {
            self.ai_search_requests.push((
                keyword.to_string(),
                source.to_string(),
                prefer_accompaniment,
            ));
            self.ai_search_result.clone()
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
            exclude: Option<&TrackKey>,
        ) -> Result<Option<PlayableTrack>> {
            Ok(self
                .pool
                .iter()
                .find(|track| exclude != Some(&track.track_ref.key))
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

        fn ai_search_and_pick(
            &mut self,
            keyword: &str,
            source: &str,
            prefer_accompaniment: bool,
        ) -> std::result::Result<Option<PlaybackPickedCandidate>, PlaybackSearchFailure> {
            self.ai_searches.push((
                keyword.to_string(),
                source.to_string(),
                prefer_accompaniment,
            ));
            Ok(self.track_unavailable.then(|| PlaybackPickedCandidate {
                text: "备用歌曲 - 歌手".to_string(),
                track: test_track("miliastra://track/netease/backup", "备用歌曲 - 歌手"),
                candidate_snapshot: Vec::new(),
            }))
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

        fn player_status(&mut self) -> Result<PlayerStatus> {
            unreachable!("playback never started")
        }

        fn playback_queue(&mut self) -> Result<Vec<QueueItem>> {
            Ok(self.queue.clone())
        }

        fn pick_playback_pool_track(
            &mut self,
            exclude: Option<&TrackKey>,
        ) -> Result<Option<PlayableTrack>> {
            Ok(self
                .pool
                .iter()
                .find(|track| exclude != Some(&track.track_ref.key))
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
            play_attempts: 0,
            ai_searches: Vec::new(),
            pool: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: 50,
            monitor_status_ms: 50,
            help_batch_ms: 0,
        });

        application
            .consume_queue("test", &mut port)
            .expect("queue consumption should report the failure and stop");

        assert!(port.removed.is_empty());
        assert_eq!(port.replies, ["player unavailable"]);
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
            play_attempts: 0,
            ai_searches: Vec::new(),
            pool: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: 50,
            monitor_status_ms: 50,
            help_batch_ms: 0,
        });

        application
            .consume_queue("track-unavailable", &mut port)
            .expect("unavailable candidate should use the alternate source");

        assert!(port.queue.is_empty());
        assert_eq!(port.removed, [QueueRemoval::Id(17)]);
        assert_eq!(port.ai_searches, []);
        assert_eq!(port.play_attempts, 2);
        assert_eq!(
            port.replies,
            ["因平台无音源,已由AI自动切换至:不可用候选-测试歌手"]
        );
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
            play_attempts: 0,
            ai_searches: Vec::new(),
            pool: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: 50,
            monitor_status_ms: 50,
            help_batch_ms: 0,
        });

        application
            .consume_queue("track-unavailable", &mut port)
            .expect("retry failure should be reported without another source switch");

        assert_eq!(port.play_attempts, 2);
        assert_eq!(port.ai_searches, []);
        assert_eq!(port.removed, [QueueRemoval::Id(18)]);
        assert!(port.queue.is_empty());
        assert_eq!(port.replies, ["平台无对应歌曲音源"]);
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
            ai_search_result: Ok(None),
            ai_search_requests: Vec::new(),
            played_uris: Vec::new(),
            user_paused: false,
            pool: Vec::new(),
            preloaded: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: 50,
            monitor_status_ms: 50,
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
            ai_search_result: Ok(None),
            ai_search_requests: Vec::new(),
            played_uris: Vec::new(),
            user_paused: true,
            pool: Vec::new(),
            preloaded: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: 50,
            monitor_status_ms: 50,
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
            play_attempts: 0,
            ai_searches: Vec::new(),
            pool: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: 50,
            monitor_status_ms: 50,
            help_batch_ms: 0,
        });

        application
            .consume_queue("test", &mut port)
            .expect("confirmed no-source item should be dropped before the next song");

        assert!(port.queue.is_empty());
        assert_eq!(port.removed, [QueueRemoval::Id(11), QueueRemoval::Id(12)]);
        // 每首歌各尝试一次：换源时无候选快照，直接确认项目级失败并丢弃。
        assert_eq!(port.play_attempts, 2);
        assert_eq!(port.replies, ["平台无对应歌曲音源", "平台无对应歌曲音源"]);
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
            ai_search_result: Ok(None),
            ai_search_requests: Vec::new(),
            played_uris: Vec::new(),
            user_paused: false,
            pool: vec![pool_track],
            preloaded: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: 50,
            monitor_status_ms: 50,
            help_batch_ms: 0,
        });

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
            play_attempts: 0,
            ai_searches: Vec::new(),
            pool: vec![test_track(
                "miliastra://track/qqmusic/pool-1",
                "池歌一 - 歌手A",
            )],
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: 50,
            monitor_status_ms: 50,
            help_batch_ms: 0,
        });

        application
            .consume_queue_after_monitor("自然结束", &mut port)
            .unwrap();

        // 池歌播放失败：排除后池中无候选，静默结束，不死循环。
        assert_eq!(port.play_attempts, 1);
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
            ai_search_result: Ok(None),
            ai_search_requests: Vec::new(),
            played_uris: Vec::new(),
            user_paused: false,
            pool: Vec::new(),
            preloaded: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: 50,
            monitor_status_ms: 50,
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

    struct NavigationCommandPort {
        previous_request: Option<PlaybackRequest>,
        played_uris: Vec<String>,
        previous_calls: usize,
        verifications: VecDeque<PlaybackVerification>,
        replies: Vec<String>,
        batch_replies: Vec<Vec<String>>,
        batch_delays: Vec<u64>,
        queue: Vec<QueueItem>,
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
            self.status_calls += 1;
            if let Some(status) = self.status_updates.pop_front() {
                self.status = status;
            }
            Ok(self.status.clone())
        }

        fn playback_queue(&mut self) -> Result<Vec<QueueItem>> {
            Ok(self.queue.clone())
        }

        fn remove_playback_queue(&mut self, _removal: QueueRemoval) -> Result<()> {
            unreachable!("navigation does not consume the queue")
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

        fn next_external(&mut self) -> Result<String> {
            unreachable!("known previous URI should avoid native navigation")
        }

        fn previous_external(&mut self) -> Result<String> {
            self.previous_calls += 1;
            Ok(String::new())
        }

        fn set_volume(&mut self, _volume: &str) -> Result<()> {
            Ok(())
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

    #[test]
    fn full_queue_command_uses_batch_delivery_without_compressing_entries() {
        let mut port = NavigationCommandPort {
            previous_request: None,
            played_uris: Vec::new(),
            previous_calls: 0,
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
            monitor_tick_ms: 50,
            monitor_status_ms: 50,
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
            previous_calls: 0,
            verifications: VecDeque::new(),
            replies: Vec::new(),
            batch_replies: Vec::new(),
            batch_delays: Vec::new(),
            queue: Vec::new(),
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
            monitor_tick_ms: 50,
            monitor_status_ms: 1_000,
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
            ["限时歌词已开启，将持续5秒；正式命令执行期间暂不发送，完成后自动继续"]
        );
    }

    #[test]
    fn single_song_lyrics_starts_the_current_song_background_monitor() {
        let mut port = NavigationCommandPort {
            previous_request: None,
            played_uris: Vec::new(),
            previous_calls: 0,
            verifications: VecDeque::new(),
            replies: Vec::new(),
            batch_replies: Vec::new(),
            batch_delays: Vec::new(),
            queue: Vec::new(),
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
            monitor_tick_ms: 50,
            monitor_status_ms: 1_000,
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
        assert_eq!(port.replies, ["单曲后台歌词已开启，切歌后自动停止"]);
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
            previous_calls: 0,
            verifications: VecDeque::new(),
            replies: Vec::new(),
            batch_replies: Vec::new(),
            batch_delays: Vec::new(),
            queue: Vec::new(),
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
            monitor_tick_ms: 50,
            monitor_status_ms: 1_000,
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

        let messages = split_full_queue_messages(&queue, 20);
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
    fn previous_prefers_known_uri_over_native_navigation() {
        let previous_uri = "miliastra://track/qqmusic/previous";
        let mut port = NavigationCommandPort {
            previous_request: Some(PlaybackRequest {
                keyword: "上一首歌曲".to_string(),
                source: "qqmusic".to_string(),
                prefer_accompaniment: false,
                track: Some(test_track(previous_uri, "上一首歌曲 - 测试歌手")),
                requester: String::new(),
                navigation: PlaybackNavigation::Previous,
                candidate_snapshot: Vec::new(),
            }),
            played_uris: Vec::new(),
            previous_calls: 0,
            verifications: VecDeque::from([PlaybackVerification::Success {
                status: PlayerStatus {
                    status: "playing".to_string(),
                    current_uri: previous_uri.to_string(),
                    name: "上一首歌曲".to_string(),
                    duration: 180.0,
                    progress: 1.0,
                    ..PlayerStatus::default()
                },
                message: "开始播放: 上一首歌曲".to_string(),
            }]),
            replies: Vec::new(),
            batch_replies: Vec::new(),
            batch_delays: Vec::new(),
            queue: Vec::new(),
            status_updates: VecDeque::new(),
            clock: None,
            status: PlayerStatus {
                status: "playing".to_string(),
                current_uri: previous_uri.to_string(),
                name: "上一首歌曲".to_string(),
                duration: 180.0,
                progress: 1.0,
                ..PlayerStatus::default()
            },
            stop_continuous_lyrics_after_statuses: None,
            status_calls: 0,
            background_lyrics_starts: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            monitor_tick_ms: 50,
            monitor_status_ms: 50,
            help_batch_ms: 0,
        });
        let context = PlaybackCommandContext {
            message_type: "blue".to_string(),
            username: "tester".to_string(),
            user_command: "@上一首".to_string(),
        };

        application
            .execute_command(&context, &PlaybackCommand::Previous, &mut port)
            .expect("previous command");

        assert_eq!(port.played_uris, [previous_uri]);
        assert_eq!(port.previous_calls, 0);
    }
}
