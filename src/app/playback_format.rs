use std::time::Instant;

use super::feeluown::PlayerStatus;

#[derive(Clone, Debug)]
pub(super) struct PlaybackSnapshot {
    pub(super) status: PlayerStatus,
    pub(super) captured_at: Instant,
}

pub(super) fn is_playing(status: &PlayerStatus) -> bool {
    status.status == "playing"
}

pub(super) fn estimated_player_status(snapshot: &PlaybackSnapshot) -> PlayerStatus {
    let mut status = snapshot.status.clone();
    if status.status == "playing" && status.progress.is_finite() {
        status.progress += snapshot.captured_at.elapsed().as_secs_f64();
        if status.duration.is_finite() && status.duration > 0.0 {
            status.progress = status.progress.min(status.duration);
        }
    }
    status
}

pub(super) fn playback_remaining_seconds(status: &PlayerStatus) -> Option<f64> {
    if !status.duration.is_finite() || !status.progress.is_finite() {
        return None;
    }
    if status.duration <= 0.0 || status.progress < 0.0 || status.progress > status.duration {
        return None;
    }
    Some(status.duration - status.progress)
}

pub(super) fn playback_progress_restarted(before: f64, after: f64) -> bool {
    before.is_finite() && after.is_finite() && before > 2.0 && (after < 2.0 || after + 1.0 < before)
}

pub(super) fn format_play_message(status: &PlayerStatus) -> String {
    format!(
        "播放: {} ({}/{}) 音量{}",
        song_title(&status.name, &status.singer),
        format_time(status.progress),
        format_time(status.duration),
        status.volume
    )
}

pub(super) fn song_title(name: &str, singer: &str) -> String {
    let name = if name.trim().is_empty() {
        "未知"
    } else {
        name.trim()
    };
    let singer = if singer.trim().is_empty() {
        "未知"
    } else {
        singer.trim()
    };
    format!("{} - {}", name, singer)
}

pub(super) fn format_time(value: f64) -> String {
    if !value.is_finite() || value <= 0.0 {
        return "0:00".to_string();
    }
    let total = value.floor() as i64;
    format!("{}:{:02}", total / 60, total % 60)
}
