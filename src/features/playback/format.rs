use std::time::Instant;

use super::PlayerStatus;
use crate::features::reply_template::{ReplyTemplate, ReplyVars, playback as templates};

/// 渲染一条回复模板。业务层只声明模板与变量，覆盖文案由调用方（composition 层）提供。
///
/// `@状态`/`@歌词` 原来全部写死 `render(spec, None, ...)`，用户在配置中心改文案或
/// 静音后完全不生效；这里把渲染入口显式传进来，默认实现保持内置文案。
type TemplateRenderer<'a> = &'a mut dyn FnMut(&ReplyTemplate, &ReplyVars) -> String;

fn default_render(spec: &ReplyTemplate, vars: &ReplyVars) -> String {
    crate::features::reply_template::render(spec, None, vars).text
}

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
        format_duration(status.progress),
        format_duration(status.duration),
        status.volume
    )
}

/// @状态 的播放器快照变量。
///
/// 单独拆出来，是为了让"状态行模板"和"歌单前缀模板"共用同一批值：
/// 前缀模板拿到的 {内容} 就是这里渲染出的整行。
fn status_vars(
    status: &PlayerStatus,
    requester: &str,
    title: String,
    render: TemplateRenderer<'_>,
) -> ReplyVars {
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
        render(
            &templates::STATUS_REQUESTER,
            &ReplyVars::new().with("点歌人", requester),
        )
    };
    ReplyVars::new()
        .with("状态", state)
        .with("曲名", title)
        .with("进度", format_duration(status.progress))
        .with("总长", format_duration(status.duration))
        .with("音量", status.volume.to_string())
        .with("点歌人", requester_text)
}

pub(crate) fn format_status(status: &PlayerStatus) -> String {
    format_status_with_mode(status, None)
}

/// `@状态` 一行排版：曲名（含歌手）让位给前缀/进度/音量/点歌人后缀，整行 ≤ budget。
fn format_status_with_budget(
    status: &PlayerStatus,
    requester: &str,
    budget: usize,
    render: TemplateRenderer<'_>,
) -> String {
    let has_title = !optional_song_title(&status.name, &status.singer).is_empty();
    // 有歌 / 无歌用两张主模板：两种排版的冒号与空格位置不同，
    // 不能靠同一张模板加空变量凑出来。
    let spec = if has_title {
        &templates::STATUS_WITH_SONG
    } else {
        &templates::STATUS_WITHOUT_SONG
    };
    if !has_title {
        let vars = status_vars(status, requester, String::new(), render);
        return render(spec, &vars);
    }
    // 先渲染「曲名为空」的骨架，量出前后缀占的宽度，再给曲名分配剩余预算。
    let skeleton = {
        let vars = status_vars(status, requester, String::new(), render);
        render(spec, &vars)
    };
    let title_budget = budget
        .saturating_sub(crate::text::display_width(&skeleton))
        .max(8);
    let title = if status.name.trim().is_empty() {
        crate::text::chat_layout::chat_track_label(&status.singer, &[], title_budget)
    } else {
        let artists = if status.singer.trim().is_empty() {
            Vec::new()
        } else {
            vec![status.singer.clone()]
        };
        crate::text::chat_layout::chat_track_label(&status.name, &artists, title_budget)
    };
    let vars = status_vars(status, requester, title, render);
    render(spec, &vars)
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
    let mut render = default_render;
    format_status_with_mode_and_renderer(status, playlist_owner, &mut render)
}

/// 与 [`format_status_with_mode`] 相同，但覆盖文案与静音由调用方提供的渲染入口决定。
pub(crate) fn format_status_with_mode_and_renderer(
    status: &PlayerStatus,
    playlist_owner: Option<&str>,
    render: TemplateRenderer<'_>,
) -> String {
    match playlist_owner {
        Some(owner) => {
            // 歌单模式用归属替代点歌人：清空 requester 渲染整行，
            // 再由前缀模板包一层，两处都能被用户改写。
            // 前缀本身也占显示宽度，内层曲名预算要把它扣掉，保证前缀与内容同条。
            let prefix_skeleton = render(
                &templates::STATUS_PLAYLIST_PREFIX,
                &ReplyVars::new().with("歌单归属", owner).with("内容", ""),
            );
            let inner_budget = crate::text::chat_layout::CHAT_LAYOUT_BUDGET
                .saturating_sub(crate::text::display_width(&prefix_skeleton))
                .max(8);
            let mut bare = status.clone();
            bare.requester.clear();
            let inner = format_status_with_budget(&bare, "", inner_budget, render);
            render(
                &templates::STATUS_PLAYLIST_PREFIX,
                &ReplyVars::new().with("歌单归属", owner).with("内容", inner),
            )
        }
        None => format_status_with_budget(
            status,
            status.requester.trim(),
            crate::text::chat_layout::CHAT_LAYOUT_BUDGET,
            render,
        ),
    }
}

pub(crate) fn format_lyrics(status: &PlayerStatus) -> String {
    let mut render = default_render;
    format_lyrics_with(status, &mut render)
}

/// 与 [`format_lyrics`] 相同，但走调用方的模板渲染入口（覆盖文案 / 静音）。
pub(crate) fn format_lyrics_with(status: &PlayerStatus, render: TemplateRenderer<'_>) -> String {
    let text = status.lyric_line_text.trim();
    let spec = if text.is_empty() {
        &templates::LYRICS_EMPTY
    } else {
        &templates::LYRICS_LINE
    };
    render(spec, &ReplyVars::new().with("歌词", text))
}

/// 播放时长/进度统一格式化：mm:ss。
///
/// 取整用 floor：进度不能虚报（59.6 秒还没到 1 分钟）。
/// 原来有两份实现（一份 round、一份 floor），同一进度能差一秒。
pub(crate) fn format_duration(value: f64) -> String {
    if !value.is_finite() || value <= 0.0 {
        return "0:00".to_string();
    }
    let seconds = value.floor() as u64;
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

#[cfg(test)]
mod tests {
    use super::{
        PlayerStatus, format_lyrics_with, format_status, format_status_with_mode,
        format_status_with_mode_and_renderer,
    };
    use crate::features::reply_template::{ReplyTemplate, ReplyVars, playback as templates};

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
        assert!(!rendered.contains(uuid), "输出不得含身份 UUID: {rendered}");
        // 反例守卫：若将来有人把内部键直接传进来，这行会立刻暴露问题。
        let leaked = format_status_with_mode(&status, Some(&internal_key));
        assert!(
            leaked.contains(uuid),
            "本层不做翻译，收到内部键就会泄露；调用方必须翻译后再传: {leaked}"
        );
    }

    /// B07-P1-6：格式化函数不能绕过调用方给的渲染入口。
    ///
    /// 旧实现全部写死 `render(spec, None, ...)`，面板覆盖与静音对 `@状态`/`@歌词`
    /// 完全不生效。这里用探针渲染器断言每个模板都真的经过它。
    #[test]
    fn formatters_never_bypass_the_supplied_renderer() {
        let status = PlayerStatus {
            status: "playing".into(),
            name: "歌".into(),
            singer: "手".into(),
            duration: 100.0,
            progress: 1.0,
            volume: 80,
            requester: "点歌人".into(),
            lyric_line_text: "第一行".into(),
            ..Default::default()
        };
        let mut seen: Vec<&'static str> = Vec::new();
        let plain_status;
        let playlist_status;
        let lyrics;
        {
            let mut render = |spec: &ReplyTemplate, _vars: &ReplyVars| {
                seen.push(spec.key);
                format!("<{}>", spec.key)
            };
            plain_status = format_status_with_mode_and_renderer(&status, None, &mut render);
            playlist_status =
                format_status_with_mode_and_renderer(&status, Some("甲"), &mut render);
            lyrics = format_lyrics_with(&status, &mut render);
        }

        assert!(
            plain_status.contains(templates::STATUS_WITH_SONG.key),
            "普通状态行要经渲染入口：{plain_status}"
        );
        assert!(
            playlist_status.contains(templates::STATUS_PLAYLIST_PREFIX.key),
            "歌单前缀也要走覆盖入口：{playlist_status}"
        );
        for key in [
            templates::STATUS_REQUESTER.key,
            templates::STATUS_WITH_SONG.key,
        ] {
            assert!(seen.contains(&key), "格式器绕过了渲染入口：缺 {key}");
        }

        assert!(
            lyrics.contains(templates::LYRICS_LINE.key),
            "歌词也要走覆盖入口：{lyrics}"
        );
        assert!(seen.contains(&templates::LYRICS_LINE.key));
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
