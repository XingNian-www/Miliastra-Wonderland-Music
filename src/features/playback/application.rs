use std::fmt::{Display, Formatter};
use std::time::{Duration, Instant};

use anyhow::Result;

use super::{
    MismatchDecision, PlaybackAttempt, PlaybackCommand, PlaybackNavigation, PlaybackOutcome,
    PlaybackRequest, PlaybackSnapshot, PlaybackVerification, PlayerStatus, QueueAdvanceContext,
    QueueAdvanceDecision, QueueItem, QueueRemoval, estimated_player_status, format_lyrics,
    format_play_message, format_status, song_title,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlaybackCommandContext {
    pub(crate) message_type: String,
    pub(crate) username: String,
    pub(crate) user_command: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlaybackDecision {
    Confirm,
    Skip,
    SwitchSource,
    Ai,
    Timeout,
    Stopped,
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

    const fn label(self) -> &'static str {
        match self {
            Self::QqMusic => "QQ",
            Self::Netease => "网易",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlaybackPickedCandidate {
    pub(crate) text: String,
    pub(crate) uri: String,
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
    pub(crate) uri: String,
    pub(crate) friend_username: String,
    pub(crate) console_bypass_dedup: bool,
}

impl PlaybackSelection {
    pub(crate) fn request(&self) -> PlaybackRequest {
        PlaybackRequest {
            keyword: self.keyword.clone(),
            source: self.source.clone(),
            prefer_accompaniment: self.prefer_accompaniment,
            uri: self.uri.clone(),
            navigation: PlaybackNavigation::Normal,
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

pub(crate) trait PlaybackExecutionPort {
    fn reply(&mut self, message: &str) -> Result<()>;
    fn update_monitor(&mut self);
    fn search_and_pick(
        &mut self,
        keyword: &str,
        source: &str,
        prefer_accompaniment: bool,
    ) -> std::result::Result<Option<PlaybackPickedCandidate>, PlaybackSearchFailure>;
    fn song_dedup_limited(&mut self, request: &PlaybackRequest) -> Result<bool>;
    fn play_request_uri(&mut self, request: &PlaybackRequest) -> Result<PlaybackAttempt>;
    fn verify_playback_started(
        &mut self,
        request: &PlaybackRequest,
        attempt: &mut PlaybackAttempt,
    ) -> Result<PlaybackVerification>;
    fn reject_mismatch_as_no_source(&mut self, status: Option<&PlayerStatus>) -> Result<()>;
    fn player_status(&mut self) -> Result<PlayerStatus>;
    fn wait_for_decision(
        &mut self,
        allow_switch_source: bool,
        allow_ai: bool,
        timeout_confirms: bool,
    ) -> Result<PlaybackDecision>;
    fn playback_queue(&mut self) -> Result<Vec<QueueItem>>;
    fn remove_playback_queue(&mut self, removal: QueueRemoval) -> Result<()>;
}

pub(crate) trait PlaybackCommandPort: PlaybackExecutionPort {
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
    fn wait(&mut self, duration: Duration);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlaybackWorkload {
    pub(crate) has_pending_playback_task: bool,
    pub(crate) command_executing: bool,
    pub(crate) song_command_executing: bool,
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
    pub(crate) skip_status_initial_ms: u64,
    pub(crate) skip_status_poll_ms: u64,
    pub(crate) skip_status_retries: u32,
    pub(crate) monitor_tick_ms: u64,
    pub(crate) monitor_status_ms: u64,
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
                let message = port.pause_by_user()?;
                port.log_executed(context, "pause")?;
                port.update_monitor();
                port.reply(if message.trim().is_empty() {
                    "已暂停"
                } else {
                    message.trim()
                })?;
            }
            PlaybackCommand::Resume | PlaybackCommand::Play => {
                let message = port.resume_by_user()?;
                port.log_executed(context, "resume")?;
                port.update_monitor();
                port.reply(if message.trim().is_empty() {
                    "已恢复播放"
                } else {
                    message.trim()
                })?;
            }
            PlaybackCommand::Next => {
                if !port.playback_queue()?.is_empty() {
                    self.consume_queue("手动下一首", port)?;
                    port.log_executed(context, "next queue")?;
                } else {
                    let message = port.next_external()?;
                    port.update_monitor();
                    port.log_executed(context, "next feeluown")?;
                    self.reply_player_status_after_skip(message.trim(), port)?;
                }
            }
            PlaybackCommand::Previous => {
                if let Some(request) = port.previous_playback_request()? {
                    self.play_request(&request, false, false, port)?;
                    port.log_executed(context, "previous uri")?;
                } else {
                    let message = port.previous_external()?;
                    port.update_monitor();
                    port.log_executed(context, "previous")?;
                    self.reply_player_status_after_skip(message.trim(), port)?;
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
            PlaybackCommand::Queue => {
                port.log_executed(context, "queue list")?;
                self.reply_queue(port)?;
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

    fn reply_queue<P: PlaybackCommandPort + ?Sized>(&self, port: &mut P) -> Result<()> {
        let queue = port.playback_queue()?;
        if queue.is_empty() {
            port.reply("队列为空")
        } else {
            let entries = queue
                .iter()
                .enumerate()
                .map(|(index, item)| format!("{}.{}", index + 1, item.keyword))
                .collect::<Vec<_>>()
                .join(", ");
            port.reply(&format!(
                "队列({}/{}): {}",
                queue.len(),
                self.config.queue_max_size,
                entries
            ))
        }
    }

    fn reply_player_status_after_skip<P: PlaybackCommandPort + ?Sized>(
        &self,
        fallback: &str,
        port: &mut P,
    ) -> Result<()> {
        port.wait(Duration::from_millis(self.config.skip_status_initial_ms));
        for _ in 0..self.config.skip_status_retries {
            match port.player_status() {
                Ok(status) if super::is_playing(&status) || status.status == "paused" => {
                    return port.reply(&format_play_message(&status));
                }
                Ok(_) => port.wait(Duration::from_millis(self.config.skip_status_poll_ms)),
                Err(error) => {
                    log::error!("切歌后查询播放状态失败: {error:#}");
                    break;
                }
            }
        }
        if fallback.is_empty() {
            port.reply("切歌完成")
        } else {
            port.reply(fallback)
        }
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
            song_command_executing: workload.song_command_executing,
        };
        let decision = port.maybe_advance_queue(estimated_player_status(snapshot), context)?;
        port.update_monitor();
        match decision {
            QueueAdvanceDecision::None => Ok(false),
            QueueAdvanceDecision::PlaybackStateChanged
            | QueueAdvanceDecision::PauseForQueue
            | QueueAdvanceDecision::ResumeIfIdle => Ok(true),
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
        loop {
            let Some(item) = port.playback_queue()?.into_iter().next() else {
                return Ok(());
            };
            log::info!("消费队列({}): {}", reason, item.keyword);
            let request = PlaybackSelection {
                keyword: item.keyword.clone(),
                source: item.source.clone(),
                prefer_accompaniment: item.prefer_accompaniment,
                ai_original_text: item.ai_original_text.clone(),
                uri: item.uri.clone(),
                friend_username: item.friend_username.clone(),
                console_bypass_dedup: item.dedup_bypass,
            };
            if self.selection_dedup_limited(&request, port)? {
                port.remove_playback_queue(QueueRemoval::Id(item.id))?;
                log::info!("队列项近期已播放过，已跳过: {}", item.keyword);
                port.reply(&request.dedup_skip_message())?;
                continue;
            }
            let allow_switch_source = request.uri.trim().is_empty();
            let outcome = self.play_confirmed(&request, allow_switch_source, port)?;
            match outcome {
                PlaybackOutcome::Success => {
                    port.remove_playback_queue(QueueRemoval::Id(item.id))?;
                    return Ok(());
                }
                PlaybackOutcome::NoSource => {
                    port.remove_playback_queue(QueueRemoval::Id(item.id))?;
                    log::error!("队列项无音源，已丢弃: {}", item.keyword);
                }
                PlaybackOutcome::Error => {
                    log::error!("队列项播放失败，保留在队首: {}", item.keyword);
                    return Ok(());
                }
                PlaybackOutcome::DedupLimited => {
                    port.remove_playback_queue(QueueRemoval::Id(item.id))?;
                    log::info!("队列项近期已播放过，已跳过: {}", item.keyword);
                }
            }
        }
    }

    pub(crate) fn play_confirmed<P: PlaybackExecutionPort + ?Sized>(
        &self,
        request: &PlaybackSelection,
        allow_switch_source: bool,
        port: &mut P,
    ) -> Result<PlaybackOutcome> {
        if self.selection_dedup_limited(request, port)? {
            log::info!(
                "长时间同歌去重拦截: keyword={} uri={}",
                request.keyword,
                request.uri
            );
            port.reply(&request.dedup_reject_message())?;
            return Ok(PlaybackOutcome::DedupLimited);
        }
        if request.uri.trim().is_empty() {
            let source = if request.source.trim().is_empty() {
                "qqmusic"
            } else {
                &request.source
            };
            let picked = match port.search_and_pick(
                &request.keyword,
                source,
                request.prefer_accompaniment,
            ) {
                Ok(Some(picked)) => picked,
                Ok(None) => {
                    port.reply("平台无对应歌曲音源")?;
                    return Ok(PlaybackOutcome::NoSource);
                }
                Err(error) => {
                    log::error!("点歌搜索失败: {error}");
                    port.reply(&format!("{}{}", request.label(), error.user_message()))?;
                    return Ok(PlaybackOutcome::Error);
                }
            };
            log::info!("播放器候选: {} -> {}", picked.text, picked.uri);
            let mut resolved = request.clone();
            resolved.keyword = picked.text;
            resolved.source = source.to_string();
            resolved.uri = picked.uri;
            return self.play_confirmed(&resolved, allow_switch_source, port);
        }
        self.play_request(&request.request(), allow_switch_source, false, port)
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

    fn play_request<P: PlaybackExecutionPort + ?Sized>(
        &self,
        request: &PlaybackRequest,
        allow_switch_source: bool,
        confirm_after_switch: bool,
        port: &mut P,
    ) -> Result<PlaybackOutcome> {
        let mut attempt = match port.play_request_uri(request) {
            Ok(attempt) => attempt,
            Err(error) => {
                let message = error.to_string();
                log::error!("播放候选失败: {message}");
                port.reply(if message.trim().is_empty() {
                    "平台无对应歌曲音源"
                } else {
                    message.trim()
                })?;
                return Ok(PlaybackOutcome::Error);
            }
        };
        self.complete_verification(
            request,
            &mut attempt,
            allow_switch_source,
            confirm_after_switch,
            port,
        )
    }

    fn complete_verification<P: PlaybackExecutionPort + ?Sized>(
        &self,
        request: &PlaybackRequest,
        attempt: &mut PlaybackAttempt,
        allow_switch_source: bool,
        confirm_after_switch: bool,
        port: &mut P,
    ) -> Result<PlaybackOutcome> {
        match port.verify_playback_started(request, attempt)? {
            PlaybackVerification::Success { status, message } => {
                if confirm_after_switch {
                    match self.confirm_switched_source_result(&status, port)? {
                        PlaybackDecision::Skip => {
                            port.reject_mismatch_as_no_source(Some(&status))?;
                            self.report_no_source(Some(&status), false, port)?;
                            port.update_monitor();
                            return Ok(PlaybackOutcome::NoSource);
                        }
                        PlaybackDecision::Stopped => return Ok(PlaybackOutcome::Error),
                        _ => {}
                    }
                }
                port.reply(&message)?;
                port.update_monitor();
                Ok(PlaybackOutcome::Success)
            }
            PlaybackVerification::NoSource => {
                self.report_no_source(None, false, port)?;
                port.update_monitor();
                Ok(PlaybackOutcome::NoSource)
            }
            PlaybackVerification::MismatchedCandidate(mismatch) => {
                match self.handle_mismatch(
                    request,
                    &mismatch.status,
                    &mismatch.local_reason,
                    allow_switch_source,
                ) {
                    MismatchDecision::NoSource => {
                        port.reject_mismatch_as_no_source(Some(&mismatch.status))?;
                        self.report_no_source(Some(&mismatch.status), false, port)?;
                        port.update_monitor();
                        Ok(PlaybackOutcome::NoSource)
                    }
                    MismatchDecision::SwitchSource => self.switch_source_and_play(
                        &request.keyword,
                        &request.source,
                        request.prefer_accompaniment,
                        port,
                    ),
                }
            }
        }
    }

    fn handle_mismatch(
        &self,
        request: &PlaybackRequest,
        status: &PlayerStatus,
        local_reason: &str,
        allow_switch_source: bool,
    ) -> MismatchDecision {
        log::info!("歌曲暂不匹配: {}", local_reason);
        if request.uri.trim().is_empty() || status.current_uri.trim().is_empty() {
            log::info!("播放确认缺少 URI，拒绝使用歌名或歌手兜底");
            return MismatchDecision::NoSource;
        }
        if allow_switch_source {
            MismatchDecision::SwitchSource
        } else {
            MismatchDecision::NoSource
        }
    }

    fn switch_source_and_play<P: PlaybackExecutionPort + ?Sized>(
        &self,
        keyword: &str,
        current_source: &str,
        prefer_accompaniment: bool,
        port: &mut P,
    ) -> Result<PlaybackOutcome> {
        let next_source = AlternatePlaybackSource::other_than(current_source);
        port.reply(&format!("换源到{}: {}", next_source.label(), keyword))?;
        let request = PlaybackSelection {
            keyword: keyword.to_string(),
            source: next_source.id().to_string(),
            prefer_accompaniment,
            ai_original_text: String::new(),
            uri: String::new(),
            friend_username: String::new(),
            console_bypass_dedup: false,
        };
        let outcome = self.play_confirmed(&request, false, port)?;
        if outcome == PlaybackOutcome::Success
            && let Ok(status) = port.player_status()
            && matches!(
                self.confirm_switched_source_result(&status, port)?,
                PlaybackDecision::Skip
            )
        {
            port.reject_mismatch_as_no_source(Some(&status))?;
            self.report_no_source(Some(&status), false, port)?;
            return Ok(PlaybackOutcome::NoSource);
        }
        Ok(outcome)
    }

    fn confirm_switched_source_result<P: PlaybackExecutionPort + ?Sized>(
        &self,
        status: &PlayerStatus,
        port: &mut P,
    ) -> Result<PlaybackDecision> {
        let message = format!(
            "换源结果:{},@确认@跳过",
            song_title(&status.name, &status.singer)
        );
        if port.reply(&message).is_err() {
            return Ok(PlaybackDecision::Timeout);
        }
        port.wait_for_decision(false, false, true)
    }

    fn report_no_source<P: PlaybackExecutionPort + ?Sized>(
        &self,
        status: Option<&PlayerStatus>,
        pause_playback: bool,
        port: &mut P,
    ) -> Result<()> {
        if pause_playback && status.is_some_and(|status| status.status == "playing") {
            let _ = port.reject_mismatch_as_no_source(status);
            port.update_monitor();
        }
        port.reply("平台无对应歌曲音源")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;

    use anyhow::{Result, bail};

    use super::super::controller::PlaybackMismatch;
    use super::*;
    use crate::runtime::clock::{Clock, ManualClock};

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
                current_uri: "fuo://qqmusic/songs/current".to_string(),
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
                song_command_executing: false,
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
    }

    struct VerifyingPlaybackPort {
        queue: Vec<QueueItem>,
        verifications: VecDeque<PlaybackVerification>,
        removed_ids: Vec<u64>,
        replies: Vec<String>,
        decision_calls: usize,
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
            skip_status_initial_ms: 0,
            skip_status_poll_ms: 0,
            skip_status_retries: 0,
            monitor_tick_ms: 50,
            monitor_status_ms: 100,
        });

        application.run_monitor_loop(&mut port);

        assert_eq!(port.status_reads, 2);
    }

    impl PlaybackExecutionPort for VerifyingPlaybackPort {
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
            unreachable!("queued items already have URIs")
        }

        fn song_dedup_limited(&mut self, _request: &PlaybackRequest) -> Result<bool> {
            Ok(false)
        }

        fn play_request_uri(&mut self, request: &PlaybackRequest) -> Result<PlaybackAttempt> {
            Ok(PlaybackAttempt::for_test(&request.uri))
        }

        fn verify_playback_started(
            &mut self,
            _request: &PlaybackRequest,
            _attempt: &mut PlaybackAttempt,
        ) -> Result<PlaybackVerification> {
            Ok(self
                .verifications
                .pop_front()
                .expect("verification outcome"))
        }

        fn reject_mismatch_as_no_source(&mut self, _status: Option<&PlayerStatus>) -> Result<()> {
            Ok(())
        }

        fn player_status(&mut self) -> Result<PlayerStatus> {
            Ok(PlayerStatus {
                status: "playing".to_string(),
                current_uri: "fuo://qqmusic/songs/current".to_string(),
                name: "当前歌曲".to_string(),
                singer: String::new(),
                album_name: String::new(),
                lyric_line_text: String::new(),
                duration: 180.0,
                progress: 10.0,
                playback_rate: 1.0,
                volume: 50,
            })
        }

        fn wait_for_decision(
            &mut self,
            _allow_switch_source: bool,
            _allow_ai: bool,
            _timeout_confirms: bool,
        ) -> Result<PlaybackDecision> {
            self.decision_calls += 1;
            Ok(PlaybackDecision::Confirm)
        }

        fn playback_queue(&mut self) -> Result<Vec<QueueItem>> {
            Ok(self.queue.clone())
        }

        fn remove_playback_queue(&mut self, removal: QueueRemoval) -> Result<()> {
            let QueueRemoval::Id(id) = removal else {
                unreachable!("queue consumption removes by id")
            };
            self.removed_ids.push(id);
            self.queue.retain(|item| item.id != id);
            Ok(())
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

        fn play_request_uri(&mut self, _request: &PlaybackRequest) -> Result<PlaybackAttempt> {
            bail!("player unavailable")
        }

        fn verify_playback_started(
            &mut self,
            _request: &PlaybackRequest,
            _attempt: &mut PlaybackAttempt,
        ) -> Result<PlaybackVerification> {
            unreachable!("playback never started")
        }

        fn reject_mismatch_as_no_source(&mut self, _status: Option<&PlayerStatus>) -> Result<()> {
            Ok(())
        }

        fn player_status(&mut self) -> Result<PlayerStatus> {
            unreachable!("playback never started")
        }

        fn wait_for_decision(
            &mut self,
            _allow_switch_source: bool,
            _allow_ai: bool,
            _timeout_confirms: bool,
        ) -> Result<PlaybackDecision> {
            unreachable!("playback never started")
        }

        fn playback_queue(&mut self) -> Result<Vec<QueueItem>> {
            Ok(self.queue.clone())
        }

        fn remove_playback_queue(&mut self, removal: QueueRemoval) -> Result<()> {
            self.removed.push(removal);
            Ok(())
        }
    }

    #[test]
    fn queue_head_is_preserved_when_playback_fails() {
        let item = QueueItem {
            id: 7,
            keyword: "测试歌曲".to_string(),
            uri: "fuo://qqmusic/songs/7".to_string(),
            ..QueueItem::default()
        };
        let mut port = FailingPlaybackPort {
            queue: vec![item],
            removed: Vec::new(),
            replies: Vec::new(),
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            skip_status_initial_ms: 0,
            skip_status_poll_ms: 0,
            skip_status_retries: 0,
            monitor_tick_ms: 50,
            monitor_status_ms: 50,
        });

        application
            .consume_queue("test", &mut port)
            .expect("queue consumption should report the failure and stop");

        assert!(port.removed.is_empty());
        assert_eq!(port.replies, ["player unavailable"]);
    }

    #[test]
    fn no_source_item_is_removed_before_the_next_item_plays() {
        let first = QueueItem {
            id: 1,
            keyword: "无音源歌曲".to_string(),
            uri: "fuo://qqmusic/songs/missing".to_string(),
            ..QueueItem::default()
        };
        let second = QueueItem {
            id: 2,
            keyword: "可播放歌曲".to_string(),
            uri: "fuo://qqmusic/songs/ok".to_string(),
            ..QueueItem::default()
        };
        let mut port = VerifyingPlaybackPort {
            queue: vec![first, second],
            verifications: VecDeque::from([
                PlaybackVerification::NoSource,
                PlaybackVerification::Success {
                    status: PlayerStatus {
                        status: "playing".to_string(),
                        current_uri: "fuo://qqmusic/songs/ok".to_string(),
                        name: "可播放歌曲".to_string(),
                        singer: String::new(),
                        album_name: String::new(),
                        lyric_line_text: String::new(),
                        duration: 180.0,
                        progress: 0.0,
                        playback_rate: 1.0,
                        volume: 50,
                    },
                    message: "开始播放: 可播放歌曲".to_string(),
                },
            ]),
            removed_ids: Vec::new(),
            replies: Vec::new(),
            decision_calls: 0,
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            skip_status_initial_ms: 0,
            skip_status_poll_ms: 0,
            skip_status_retries: 0,
            monitor_tick_ms: 50,
            monitor_status_ms: 50,
        });

        application
            .consume_queue("test", &mut port)
            .expect("queue consumption");

        assert!(port.queue.is_empty());
        assert_eq!(port.removed_ids, [1, 2]);
        assert_eq!(port.replies, ["平台无对应歌曲音源", "开始播放: 可播放歌曲"]);
    }

    #[test]
    fn known_queue_uri_does_not_trigger_source_switch_on_mismatch() {
        let item = QueueItem {
            id: 9,
            keyword: "队列歌曲".to_string(),
            source: "qqmusic".to_string(),
            uri: "fuo://qqmusic/songs/requested".to_string(),
            ..QueueItem::default()
        };
        let mut port = VerifyingPlaybackPort {
            queue: vec![item],
            verifications: VecDeque::from([PlaybackVerification::MismatchedCandidate(
                PlaybackMismatch {
                    status: PlayerStatus {
                        status: "playing".to_string(),
                        current_uri: "fuo://netease/songs/other".to_string(),
                        ..PlayerStatus::default()
                    },
                    local_reason: "URI不一致".to_string(),
                },
            )]),
            removed_ids: Vec::new(),
            replies: Vec::new(),
            decision_calls: 0,
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            skip_status_initial_ms: 0,
            skip_status_poll_ms: 0,
            skip_status_retries: 0,
            monitor_tick_ms: 50,
            monitor_status_ms: 50,
        });

        application
            .consume_queue("test", &mut port)
            .expect("queue mismatch should be handled");

        assert_eq!(port.decision_calls, 0);
        assert_eq!(port.removed_ids, [9]);
    }

    struct NavigationCommandPort {
        previous_request: Option<PlaybackRequest>,
        played_uris: Vec<String>,
        previous_calls: usize,
        verifications: VecDeque<PlaybackVerification>,
        replies: Vec<String>,
        status: PlayerStatus,
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

        fn play_request_uri(&mut self, request: &PlaybackRequest) -> Result<PlaybackAttempt> {
            self.played_uris.push(request.uri.clone());
            Ok(PlaybackAttempt::for_test(&request.uri))
        }

        fn verify_playback_started(
            &mut self,
            _request: &PlaybackRequest,
            _attempt: &mut PlaybackAttempt,
        ) -> Result<PlaybackVerification> {
            Ok(self
                .verifications
                .pop_front()
                .expect("verification outcome"))
        }

        fn reject_mismatch_as_no_source(&mut self, _status: Option<&PlayerStatus>) -> Result<()> {
            Ok(())
        }

        fn player_status(&mut self) -> Result<PlayerStatus> {
            Ok(self.status.clone())
        }

        fn wait_for_decision(
            &mut self,
            _allow_switch_source: bool,
            _allow_ai: bool,
            _timeout_confirms: bool,
        ) -> Result<PlaybackDecision> {
            Ok(PlaybackDecision::Confirm)
        }

        fn playback_queue(&mut self) -> Result<Vec<QueueItem>> {
            Ok(Vec::new())
        }

        fn remove_playback_queue(&mut self, _removal: QueueRemoval) -> Result<()> {
            unreachable!("navigation does not consume the queue")
        }
    }

    impl PlaybackCommandPort for NavigationCommandPort {
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

        fn wait(&mut self, _duration: Duration) {}
    }

    #[test]
    fn previous_prefers_known_uri_over_native_navigation() {
        let previous_uri = "fuo://qqmusic/songs/previous";
        let mut port = NavigationCommandPort {
            previous_request: Some(PlaybackRequest {
                keyword: "上一首歌曲".to_string(),
                source: "qqmusic".to_string(),
                prefer_accompaniment: false,
                uri: previous_uri.to_string(),
                navigation: PlaybackNavigation::Previous,
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
            status: PlayerStatus {
                status: "playing".to_string(),
                current_uri: previous_uri.to_string(),
                name: "上一首歌曲".to_string(),
                duration: 180.0,
                progress: 1.0,
                ..PlayerStatus::default()
            },
        };
        let application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: true,
            queue_max_size: 20,
            skip_status_initial_ms: 0,
            skip_status_poll_ms: 0,
            skip_status_retries: 1,
            monitor_tick_ms: 50,
            monitor_status_ms: 50,
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
