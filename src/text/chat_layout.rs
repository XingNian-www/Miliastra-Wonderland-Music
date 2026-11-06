//! 聊天排版共享层（方案 A / P0+P1）。
//!
//! 只负责**展示层**：把结构化歌曲信息裁成一行、把正文按显示宽度切成多段。
//! 队列 keyword / 去重身份（`SearchCandidate::text`）等数据语义不在这里改动。

use super::{char_width, display_width, split_numbered_chat_message};

/// 单行排版预算：游戏聊天显示宽度 80，留 2 列安全边距。
pub(crate) const CHAT_LAYOUT_BUDGET: usize = 78;
/// 单条回复最多拆成几段（正文 prose）。
pub(crate) const MAX_SEGMENTS_PER_REPLY: usize = 6;
/// 一批回复最多拆成几段。
pub(crate) const MAX_SEGMENTS_PER_BATCH: usize = 10;
/// 超出段数上限时最后一段的截断标记。
pub(crate) const TRUNCATED_MARKER: &str = "…（内容已截断）";

const ARTIST_SEPARATOR: &str = " / ";
const TITLE_SEPARATOR: &str = " - ";
const SHORT_MARKER: &str = "…";
const MAX_TITLE_WIDTH: usize = 40;
const MIN_ARTISTS_WIDTH: usize = 8;
const MAX_ARTISTS_WIDTH: usize = 40;

/// 按显示宽度截到 budget 以内，超长时保留开头并追加 marker（marker 也计入预算）。
fn clip_to_width(text: &str, budget: usize, marker: &str) -> String {
    if display_width(text) <= budget {
        return text.to_string();
    }
    let marker_width = display_width(marker);
    let target = budget.saturating_sub(marker_width);
    let mut out = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let next = width.saturating_add(char_width(ch));
        if next > target {
            break;
        }
        out.push(ch);
        width = next;
    }
    if marker_width > budget {
        // 预算连 marker 都放不下：只保留能放下的 marker 前缀。
        let mut clipped = String::new();
        let mut w = 0usize;
        for ch in marker.chars() {
            let next = w.saturating_add(char_width(ch));
            if next > budget {
                break;
            }
            clipped.push(ch);
            w = next;
        }
        return clipped;
    }
    out.push_str(marker);
    out
}

/// 歌手列表展示：完整放得下就完整输出；否则「前 N 位 + ` 等{总人数}位`」。
///
/// 只在发生裁切时出现「等X位」；空列表返回空串（由 `chat_track_label` 决定是否省略分隔符）。
/// 返回值显示宽度恒 ≤ budget。
pub(crate) fn chat_artists(artists: &[String], budget: usize) -> String {
    let non_empty: Vec<&str> = artists
        .iter()
        .map(String::as_str)
        .filter(|artist| !artist.trim().is_empty())
        .collect();
    if non_empty.is_empty() || budget == 0 {
        return String::new();
    }
    let full = non_empty.join(ARTIST_SEPARATOR);
    if display_width(&full) <= budget {
        return full;
    }
    let suffix = format!(" 等{}位", non_empty.len());
    let suffix_width = display_width(&suffix);
    if suffix_width >= budget {
        return clip_to_width(&suffix, budget, "");
    }
    let mut chosen = 0usize;
    let mut joined = String::new();
    for (index, artist) in non_empty.iter().enumerate() {
        let candidate = if index == 0 {
            (*artist).to_string()
        } else {
            format!("{joined}{ARTIST_SEPARATOR}{artist}")
        };
        if display_width(&candidate).saturating_add(suffix_width) > budget {
            break;
        }
        joined = candidate;
        chosen = index + 1;
    }
    if chosen == 0 {
        // 连第一位歌手都放不下：至少把「等X位」告诉玩家。
        return clip_to_width(&suffix, budget, "");
    }
    format!("{joined}{suffix}")
}

/// 歌曲展示标签：歌名优先、歌手让位，始终 ≤budget。
///
/// 无歌手时只输出歌名；极端长歌名/极小预算时丢弃歌手部分，不截断序号等结构信息。
pub(crate) fn chat_track_label(title: &str, artists: &[String], budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    let has_artists = artists.iter().any(|artist| !artist.trim().is_empty());
    let title_budget = budget.min(MAX_TITLE_WIDTH);
    let title = clip_to_width(title.trim(), title_budget, SHORT_MARKER);
    if !has_artists {
        return clip_to_width(&title, budget, SHORT_MARKER);
    }
    let separator_width = display_width(TITLE_SEPARATOR);
    let remaining = budget.saturating_sub(display_width(&title) + separator_width);
    if remaining < MIN_ARTISTS_WIDTH {
        // 歌名已经占满：丢弃歌手，保证整行不被截断。
        return title;
    }
    let artists_budget = remaining.min(MAX_ARTISTS_WIDTH);
    let artists_text = chat_artists(artists, artists_budget);
    if artists_text.is_empty() {
        return title;
    }
    clip_to_width(
        &format!("{title}{TITLE_SEPARATOR}{artists_text}"),
        budget,
        SHORT_MARKER,
    )
}

/// 候选行：`{index}. {歌名} - {歌手} [{时长}]`，严格一行。
///
/// 序号与歌名开头永不截；时长为空则省略方括号。返回值显示宽度恒 ≤ budget。
pub(crate) fn chat_candidate_line(
    index: usize,
    title: &str,
    artists: &[String],
    duration: Option<&str>,
    budget: usize,
) -> String {
    let prefix = format!("{index}. ");
    let suffix = duration
        .map(|value| format!(" [{value}]"))
        .unwrap_or_default();
    let label_budget = budget
        .saturating_sub(display_width(&prefix) + display_width(&suffix))
        .max(MIN_ARTISTS_WIDTH);
    let label = chat_track_label(title, artists, label_budget);
    clip_to_width(&format!("{prefix}{label}{suffix}"), budget, SHORT_MARKER)
}

/// 正文 prose 自动分段：
/// 先按 `\n` 切逻辑行（空行丢弃），每行按 80 宽标点优先切段；不加编号、段内无换行；
/// 总段数 ≤cap，超限时最后一段截断并追加 [`TRUNCATED_MARKER`]；空/纯空白返回空表。
///
/// 幂等：对已分段结果（`join("\n")`）再展开得到相同结果。
pub(crate) fn expand_chat_segments(message: &str, cap: usize) -> Vec<String> {
    if cap == 0 || message.trim().is_empty() {
        return Vec::new();
    }
    let mut segments = Vec::new();
    for line in message.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        segments.extend(split_numbered_chat_message("", line));
    }
    cap_chat_segments(segments, cap)
}

/// 给「已切好的段」套段数上限：超限保留前 cap-1 段，最后一段截断并加标记。
pub(crate) fn cap_chat_segments(mut segments: Vec<String>, cap: usize) -> Vec<String> {
    if cap == 0 || segments.is_empty() {
        return Vec::new();
    }
    if segments.len() <= cap {
        return segments;
    }
    segments.truncate(cap);
    let last = segments.pop().unwrap_or_default();
    let room = CHAT_LAYOUT_BUDGET.saturating_sub(display_width(TRUNCATED_MARKER));
    segments.push(format!(
        "{}{}",
        clip_to_width(&last, room, ""),
        TRUNCATED_MARKER
    ));
    segments
}

/// 必须保持单行的系统通告/结构化回执（方案 A 的「刻意压缩」白名单）。
///
/// 命中的文本不走 prose 自动换行，仍交给 `ui::chat_output::fit_chat_message`
/// 做 invite/microphone/colon/truncate 兜底；`@邀请确认@邀请拒绝` 这类
/// 两个命令必须同条、冒号前缀必须保留，拆段会破坏语义。
pub(crate) fn is_single_line_notice(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        // 邀请确认/拒绝：两个命令必须同条，拆段会改变语义。
        "@邀请确认",
        "@邀请拒绝",
        // 麦克风切换通告。
        "麦克风",
        // 卧底身份/秘密词（只发给本人或裁判，必须同条）。
        "你的位置：",
        "你的词语：",
        "平民词:",
        "卧底词:",
        // 点歌/决策类结构化回执：动作后缀必须与提示同条。
        ",@确认",
        ",@跳过",
        ",@换源",
        ",@AI",
        ",@选择",
    ];
    let trimmed = text.trim();
    if MARKERS.iter().any(|marker| trimmed.contains(marker)) {
        return true;
    }
    // `@状态` 是唯一「带音量的单行」：前缀是播放状态（歌单模式还带 `[归属的歌单]`）。
    // 不能只认「音量」二字——普通 prose（如帮助文案）提到音量也会被误判成单行。
    is_playback_status_line(trimmed)
}

/// `@状态` 行判据：含音量且以播放状态前缀开头（或歌单前缀包裹）。
fn is_playback_status_line(text: &str) -> bool {
    if !text.contains("音量") {
        return false;
    }
    const STATUS_PREFIXES: &[&str] = &["播放:", "暂停:", "停止:", "未知:"];
    if STATUS_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
    {
        return true;
    }
    // 歌单模式：[{归属}的歌单] {状态}:…音量…
    if let Some(rest) = text.strip_prefix('[') {
        if let Some((_, rest)) = rest.split_once("的歌单] ") {
            return STATUS_PREFIXES
                .iter()
                .any(|prefix| rest.starts_with(prefix));
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artists(count: usize) -> Vec<String> {
        (0..count).map(|index| format!("歌手{index}")).collect()
    }

    #[test]
    fn chat_artists_keeps_full_list_when_it_fits_and_marks_when_truncated() {
        assert_eq!(chat_artists(&[], 24), "");
        assert_eq!(chat_artists(&artists(2), 24), "歌手0 / 歌手1");
        let many = artists(60);
        for budget in [8, 16, 24, 40] {
            let text = chat_artists(&many, budget);
            assert!(display_width(&text) <= budget, "budget={budget} text={text}");
            assert!(text.contains("等60位"), "budget={budget} text={text}");
        }
        let small = chat_artists(&artists(2), 2);
        assert!(display_width(&small) <= 2);
    }

    #[test]
    fn chat_candidate_line_is_one_line_and_never_drops_the_index() {
        let title = "夜".repeat(73); // 146 显示宽度
        let line = chat_candidate_line(
            3,
            &title,
            &artists(60),
            Some("04:12"),
            CHAT_LAYOUT_BUDGET,
        );
        assert!(display_width(&line) <= CHAT_LAYOUT_BUDGET, "{line}");
        assert!(line.starts_with("3. 夜"), "序号与歌名开头必须保留：{line}");
        assert!(!line.contains('\n'));

        let ascii = chat_candidate_line(
            1,
            &"A".repeat(160),
            &artists(10),
            None,
            CHAT_LAYOUT_BUDGET,
        );
        assert!(display_width(&ascii) <= CHAT_LAYOUT_BUDGET, "{ascii}");
        assert!(ascii.starts_with("1. A"));
        for index in 1..=5 {
            let line = chat_candidate_line(index, "晴天", &[], None, CHAT_LAYOUT_BUDGET);
            assert!(line.starts_with(&format!("{index}. 晴天")), "{line}");
        }
    }

    #[test]
    fn chat_track_label_prioritizes_title_and_keeps_prefix_room() {
        let prefix = "队列已加入(第3首，共4/5): ";
        let budget = CHAT_LAYOUT_BUDGET.saturating_sub(display_width(prefix));
        let label = chat_track_label("夜空中最亮的星", &artists(60), budget);
        let line = format!("{prefix}{label}");
        assert!(display_width(&line) <= CHAT_LAYOUT_BUDGET, "{line}");
        assert!(line.starts_with(prefix));
        assert!(label.starts_with("夜空中最亮的星"));
        assert_eq!(chat_track_label("晴天", &[], 20), "晴天");
    }

    #[test]
    fn single_line_notices_are_recognized() {
        assert!(is_single_line_notice("甲邀请BOT前往大厅,30s内@邀请确认@邀请拒绝,默认通过"));
        assert!(is_single_line_notice("甲 执行了切换麦克风状态！"));
        assert!(is_single_line_notice("你的位置：A；你的词语：苹果"));
        assert!(is_single_line_notice("@点歌搜索到:晴天,@确认@跳过@选择"));
        // @状态 行（含音量、以播放状态前缀开头）保持单行。
        assert!(is_single_line_notice("播放:晴天 - 周杰伦 (1:05/3:05) 音量80  点歌人"));
        assert!(is_single_line_notice("[某人的歌单] 暂停:晴天 (1:05/3:05) 音量80"));
        // 歌词/控制台是 prose：必须走自动换行，不能进单行压缩。
        assert!(!is_single_line_notice("歌词: 第一句歌词，第二句歌词。"));
        assert!(!is_single_line_notice("[控制台]: 这是一段很长的控制台输出文本。"));
        // 普通 prose 提到「音量」不应被误判为 @状态 行。
        assert!(!is_single_line_notice("这个玩法会显示音量 80 之类的信息"));
        assert!(!is_single_line_notice("这是一段很长的普通玩法说明文字"));
    }

    #[test]
    fn long_lyrics_and_console_text_are_expanded_not_compressed() {
        let lyrics = format!("歌词: {}", "第一句歌词，第二句歌词。".repeat(12));
        assert!(!is_single_line_notice(&lyrics));
        let segments = expand_chat_segments(&lyrics, MAX_SEGMENTS_PER_REPLY);
        assert!(segments.len() > 1, "长歌词必须分段：{segments:?}");
        assert!(segments
            .iter()
            .all(|segment| display_width(segment) <= CHAT_LAYOUT_BUDGET + 2));
        assert!(segments[0].starts_with("歌词:"));

        let console = format!("[控制台]: {}", "这是一段很长的控制台输出文本。".repeat(12));
        assert!(!is_single_line_notice(&console));
        let segments = expand_chat_segments(&console, MAX_SEGMENTS_PER_REPLY);
        assert!(segments.len() > 1, "长控制台文本必须分段：{segments:?}");
        assert!(segments[0].starts_with("[控制台]:"));
    }

    #[test]
    fn cap_chat_segments_marks_the_last_segment() {
        let segments: Vec<String> = (0..12).map(|i| format!("第{i}段")).collect();
        let capped = cap_chat_segments(segments, MAX_SEGMENTS_PER_BATCH);
        assert_eq!(capped.len(), MAX_SEGMENTS_PER_BATCH);
        assert!(capped.last().unwrap().contains(TRUNCATED_MARKER));
        assert!(display_width(capped.last().unwrap()) <= CHAT_LAYOUT_BUDGET);
    }

    #[test]
    fn expand_chat_segments_splits_lines_caps_and_is_idempotent() {
        assert!(expand_chat_segments("   \n  ", 6).is_empty());
        let long = "这是很长的一段歌词内容，用来验证超过聊天宽度时的自动换行与段数上限。".repeat(12);
        let segments = expand_chat_segments(&long, MAX_SEGMENTS_PER_REPLY);
        assert!(segments.len() <= MAX_SEGMENTS_PER_REPLY);
        assert!(segments.len() > 1);
        assert!(segments
            .iter()
            .all(|segment| !segment.contains('\n') && display_width(segment) <= CHAT_LAYOUT_BUDGET + 2));
        assert!(
            segments.last().unwrap().contains(TRUNCATED_MARKER),
            "超过上限时最后一段必须带截断标记"
        );
        let again = expand_chat_segments(&segments.join("\n"), MAX_SEGMENTS_PER_REPLY);
        assert_eq!(again, segments, "展开必须幂等");

        let multi = "第一行\n\n第二行";
        assert_eq!(
            expand_chat_segments(multi, 6),
            vec!["第一行".to_string(), "第二行".to_string()],
            "多行不合并、空行丢弃"
        );
        let short = "短文本一条";
        assert_eq!(expand_chat_segments(short, 6), vec![short.to_string()]);
    }
}
