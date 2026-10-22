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
    let state = match status.status.as_str() {
        "playing" => "播放",
        "paused" => "暂停",
        "stopped" => "停止",
        _ => "未知",
    };
    let requester = status.requester.trim();
    let requester = if requester.is_empty() {
        String::new()
    } else {
        format!("  {requester}")
    };
    if title.is_empty() {
        format!(
            "{}:({}/{}) 音量{}{}",
            state, progress, duration, status.volume, requester
        )
    } else {
        format!(
            "{}:{} ({}/{}) 音量{}{}",
            state, title, progress, duration, status.volume, requester
        )
    }
}

/// @状态 文本。歌单模式下用 `[归属的歌单]` 前缀，并省略尾部的点歌人。
pub(crate) fn format_status_with_mode(
    status: &PlayerStatus,
    playlist_owner: Option<&str>,
) -> String {
    match playlist_owner {
        Some(owner) => {
            let mut bare = status.clone();
            bare.requester.clear();
            format!("[{}的歌单] {}", owner, format_status(&bare))
        }
        None => format_status(status),
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

#[cfg(test)]
mod tests {
    use super::{PlayerStatus, format_status, format_status_with_mode};

    #[test]
    fn status_uses_owner_playlist_marker_and_drops_requester() {
        let status = PlayerStatus {
            status: "playing".into(),
            name: "歌".into(),
            singer: "手".into(),
            duration: 100.0,
            progress: 10.0,
            volume: 80,
            requester: "乙".into(),
            ..Default::default()
        };
        let plain = format_status_with_mode(&status, None);
        assert!(plain.ends_with('乙'), "大厅模式保留点歌人: {plain}");
        let playlist = format_status_with_mode(&status, Some("甲"));
        assert!(playlist.starts_with("[甲的歌单] "), "{playlist}");
        assert!(
            !playlist.contains('乙'),
            "歌单模式用归属替代点歌人: {playlist}"
        );
    }

    #[test]
    fn status_uses_chinese_state_and_includes_requester() {
        let status = PlayerStatus {
            status: "playing".to_string(),
            name: "测试歌曲".to_string(),
            singer: "测试歌手".to_string(),
            progress: 65.0,
            duration: 185.0,
            volume: 80,
            requester: "点歌人".to_string(),
            ..PlayerStatus::default()
        };
        assert_eq!(
            format_status(&status),
            "播放:测试歌曲 - 测试歌手 (1:05/3:05) 音量80  点歌人"
        );
    }

    #[test]
    fn status_maps_all_transport_states_and_omits_empty_requester() {
        for (raw, expected) in [
            ("playing", "播放"),
            ("paused", "暂停"),
            ("stopped", "停止"),
            ("unknown", "未知"),
            ("other", "未知"),
        ] {
            let status = PlayerStatus {
                status: raw.to_string(),
                ..PlayerStatus::default()
            };
            assert_eq!(
                format_status(&status),
                format!("{expected}:(0:00/0:00) 音量0")
            );
        }
    }
}
