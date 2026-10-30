use std::time::Instant;

use super::PlayerStatus;
use crate::features::reply_template::{ReplyVars, playback as templates};

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

/// @状态 的播放器快照变量。
///
/// 单独拆出来，是为了让"状态行模板"和"歌单前缀模板"共用同一批值：
/// 前缀模板拿到的 {内容} 就是这里渲染出的整行。
fn status_vars(status: &PlayerStatus, requester: &str) -> ReplyVars {
    let title = optional_song_title(&status.name, &status.singer);
    let state = match status.status.as_str() {
        "playing" => "播放",
        "paused" => "暂停",
        "stopped" => "停止",
        _ => "未知",
    };
    // 点歌人片段本身也是模板：留空即整段消失，用户可改成任意措辞。
    let requester_text = if requester.is_empty() {
        String::new()
    } else {
        crate::features::reply_template::render(
            &templates::STATUS_REQUESTER,
            None,
            &ReplyVars::new().with("点歌人", requester),
        )
        .text
    };
    ReplyVars::new()
        .with("状态", state)
        .with("曲名", title)
        .with("进度", format_seconds(status.progress))
        .with("总长", format_seconds(status.duration))
        .with("音量", status.volume.to_string())
        .with("点歌人", requester_text)
}

pub(crate) fn format_status(status: &PlayerStatus) -> String {
    let vars = status_vars(status, status.requester.trim());
    // 有歌 / 无歌用两张主模板：两种排版的冒号与空格位置不同，
    // 不能靠同一张模板加空变量凑出来。
    let spec = if vars.get("曲名").is_some_and(|title| title.is_empty()) {
        &templates::STATUS_WITHOUT_SONG
    } else {
        &templates::STATUS_WITH_SONG
    };
    crate::features::reply_template::render(spec, None, &vars).text
}

/// @状态 文本。歌单模式下用 `[归属的歌单]` 前缀，并省略尾部的点歌人。
///
/// `playlist_owner` 必须是**已翻译过的展示名**，不是内部 `id:<uuid>` 归属键。
/// 归属键转展示名由调用方经 [`PlaybackExecutionPort::playlist_owner_display`]
/// 完成——那一层能拿到身份映射，本层拿不到。
pub(crate) fn format_status_with_mode(
    status: &PlayerStatus,
    playlist_owner: Option<&str>,
) -> String {
    match playlist_owner {
        Some(owner) => {
            // 歌单模式用归属替代点歌人：清空 requester 渲染整行，
            // 再由前缀模板包一层，两处都能被用户改写。
            let mut bare = status.clone();
            bare.requester.clear();
            let inner = format_status(&bare);
            crate::features::reply_template::render(
                &templates::STATUS_PLAYLIST_PREFIX,
                None,
                &ReplyVars::new().with("歌单归属", owner).with("内容", inner),
            )
            .text
        }
        None => format_status(status),
    }
}

pub(crate) fn format_lyrics(status: &PlayerStatus) -> String {
    let text = status.lyric_line_text.trim();
    let spec = if text.is_empty() {
        &templates::LYRICS_EMPTY
    } else {
        &templates::LYRICS_LINE
    };
    crate::features::reply_template::render(spec, None, &ReplyVars::new().with("歌词", text)).text
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
    fn status_line_templates_preserve_the_pre_split_output() {
        // 拆分前后必须逐字一致：这是"模板化不改变现有行为"的直接证据。
        let status = PlayerStatus {
            status: "playing".into(),
            name: "歌".into(),
            singer: "手".into(),
            duration: 185.0,
            progress: 65.0,
            volume: 80,
            requester: "点歌人".into(),
            ..Default::default()
        };
        assert_eq!(
            format_status(&status),
            "播放:歌 - 手 (1:05/3:05) 音量80  点歌人"
        );
        // 无曲目信息：冒号后直接接括号。
        let bare = PlayerStatus {
            status: "paused".into(),
            ..Default::default()
        };
        assert_eq!(format_status(&bare), "暂停:(0:00/0:00) 音量0");
        // 歌单模式：归属替代点歌人。
        assert_eq!(
            format_status_with_mode(&status, Some("甲")),
            "[甲的歌单] 播放:歌 - 手 (1:05/3:05) 音量80"
        );
    }

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

    /// 回归：歌单模式前缀里的归属必须是**展示名**，不能泄露内部 `id:<uuid>` 归属键。
    ///
    /// 实机现象：`@状态` 输出 `[id:00000000-0000-4000-8000-000000000001的歌单] 播放中 …`，
    /// 玩家看到一长串 UUID。归属键是内部标识，翻译由调用方（composition 层）
    /// 经 `playlist_owner_display` 完成，本层只负责渲染传入的字符串。
    #[test]
    fn playlist_prefix_never_leaks_an_internal_owner_key() {
        let status = PlayerStatus {
            status: "playing".into(),
            name: "歌".into(),
            singer: "手".into(),
            duration: 100.0,
            progress: 10.0,
            volume: 80,
            ..Default::default()
        };
        let uuid = "00000000-0000-4000-8000-000000000001";
        let internal_key = format!("id:{uuid}");
        // 修好后调用方传进来的是展示名；这里断言展示名被原样使用，
        // 并显式确认输出里不含 UUID 片段。
        let rendered = format_status_with_mode(&status, Some("示例玩家"));
        assert!(
            rendered.starts_with("[示例玩家的歌单] "),
            "前缀应使用展示名: {rendered}"
        );
        assert!(
            !rendered.contains(uuid),
            "输出不得含身份 UUID: {rendered}"
        );
        // 反例守卫：若将来有人把内部键直接传进来，这行会立刻暴露问题。
        let leaked = format_status_with_mode(&status, Some(&internal_key));
        assert!(
            leaked.contains(uuid),
            "本层不做翻译，收到内部键就会泄露；调用方必须翻译后再传: {leaked}"
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
