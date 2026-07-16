use std::time::Instant;

use super::PlayerStatus;

#[derive(Clone, Debug)]
pub(crate) struct PlaybackSnapshot {
    pub(crate) status: PlayerStatus,
    pub(crate) captured_at: Instant,
}

pub(crate) fn is_playing(status: &PlayerStatus) -> bool {
    status.status == "playing"
}

pub(crate) fn estimated_player_status(snapshot: &PlaybackSnapshot) -> PlayerStatus {
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

pub(crate) fn format_play_message(status: &PlayerStatus) -> String {
    format!(
        "播放: {} ({}/{}) 音量{}",
        song_title(&status.name, &status.singer),
        format_time(status.progress),
        format_time(status.duration),
        status.volume
    )
}

pub(crate) fn format_status(status: &PlayerStatus) -> String {
    let title = optional_song_title(&status.name, &status.singer);
    let progress = format_seconds(status.progress);
    let duration = format_seconds(status.duration);
    if title.is_empty() {
        format!(
            "状态: {} ({}/{}) 音量{}",
            status.status, progress, duration, status.volume
        )
    } else {
        format!(
            "状态: {} {} ({}/{}) 音量{}",
            status.status, title, progress, duration, status.volume
        )
    }
}

pub(crate) fn format_lyrics(status: &PlayerStatus) -> String {
    let text = status.lyric_line_text.trim();
    if text.is_empty() {
        "当前无歌词".to_string()
    } else {
        format!("歌词: {}", text)
    }
}

fn format_seconds(value: f64) -> String {
    if !value.is_finite() || value <= 0.0 {
        return "0:00".to_string();
    }
    let seconds = value.round() as u64;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn optional_song_title(name: &str, singer: &str) -> String {
    let name = name.trim();
    let singer = singer.trim();
    match (name.is_empty(), singer.is_empty()) {
        (true, true) => String::new(),
        (false, true) => name.to_string(),
        (true, false) => singer.to_string(),
        (false, false) => format!("{} - {}", name, singer),
    }
}

pub(crate) fn song_title(name: &str, singer: &str) -> String {
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
