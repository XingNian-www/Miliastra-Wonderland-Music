//! 共享文本工具：OCR 归一化、比较键、显示宽度与聊天分片。
//!
//! 归一化只在这里实现一份，命令锁、聊天比较与身份匹配都必须复用；
//! 约束见 technical-docs/MUST_READ.md 第 1 节。

/// 命令路由与已执行命令去重共用的 OCR 尾部噪声字符。
pub(crate) const OCR_TRAILING_NOISE: &[char] = &[
    '，', ',', '。', '.', '！', '!', '？', '?', '、', '；', ';', '：', ':', '·', '…', '~', '～',
    '—', '」', '』', '”', '’', '）', ')', '】', ']', ' ', '\t', '　',
];

/// 归一化聊天文本：**只做不丢信息的修正**，且**空白一律丢弃**。
///
/// - 全角 ASCII 折半角：游戏里的冒号/括号本来就是半角，OCR 偶尔读成全角；
///   折半角是把它改回屏幕上的真实字形，属于纠错。
/// - 空白全部丢弃：OCR 的空白两个方向都会错（同一位置有时多读一个空格、有时整段漏掉），
///   保留它只会让同一句话时而带空格时而不带；统一丢弃后文本才是确定且可逐字比较的。
///
/// 命令锁与聊天比较共用这一份实现，禁止再各自复制一份（否则同/全角抖动会互相打不中）。
pub(crate) fn normalize_chat_text(text: &str) -> String {
    text.chars()
        .filter_map(|ch| match ch {
            // 全角 ！ 到 ～ 与半角 ASCII 一一对应，码位偏移 0xFEE0。
            '\u{FF01}'..='\u{FF5E}' => Some(char::from_u32(ch as u32 - 0xFEE0).unwrap_or(ch)),
            ch if ch.is_whitespace() => None,
            other => Some(other),
        })
        .collect()
}

pub(crate) fn normalize_comparison_text(text: &str) -> String {
    text.chars()
        .filter_map(normalize_comparison_char)
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

pub(crate) const MAX_CHAT_WIDTH: usize = 80;

pub(crate) fn display_width(value: &str) -> usize {
    value.chars().map(char_width).sum()
}

pub(crate) fn char_width(ch: char) -> usize {
    if ch.is_ascii() { 1 } else { 2 }
}

/// 把用户配置的多行文案拆成实际要发送的消息。
///
/// 规则（与面板说明一致）：
/// 1. 先按换行切分——一行就是一条消息，空行忽略。
/// 2. 若某一行仍超过聊天宽度（MAX_CHAT_WIDTH），按显示宽度硬切，
///    不加 "1/2：" 这类编号：用户写的就是要发的内容，编号会污染文案。
///
/// 不限制条数：文案由用户自己配置，条数后果也由用户自己承担。
pub(crate) fn split_configured_lines(text: &str) -> Vec<String> {
    let mut messages = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        messages.extend(wrap_to_chat_width(line));
    }
    messages
}

/// 按显示宽度硬切一行，保持字符边界。
fn wrap_to_chat_width(line: &str) -> Vec<String> {
    if display_width(line) <= MAX_CHAT_WIDTH {
        return vec![line.to_string()];
    }
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    let mut width = 0usize;
    for ch in line.chars() {
        let next = char_width(ch);
        if width + next > MAX_CHAT_WIDTH && !chunk.is_empty() {
            chunks.push(chunk.trim_end().to_string());
            chunk = String::new();
            width = 0;
        }
        chunk.push(ch);
        width += next;
    }
    if !chunk.trim().is_empty() {
        chunks.push(chunk.trim_end().to_string());
    }
    chunks.retain(|item| !item.is_empty());
    chunks
}

/// 按聊天宽度适配一条消息：放得下就原样返回，放不下才分段编号。
///
/// 这个判断必须收在一处：渲染方（组装层）和测试助手中任何一处忘记"短文本不编号"，
/// 都会让玩家看到与内容无关的 "1/1：" 前缀。
pub(crate) fn fit_chat_message(label: &str, message: &str) -> Vec<String> {
    if display_width(message) <= MAX_CHAT_WIDTH {
        vec![message.to_string()]
    } else {
        split_numbered_chat_message(label, message)
    }
}

pub(crate) fn split_numbered_chat_message(label: &str, message: &str) -> Vec<String> {
    let source = normalize_segment_source(message);
    let mut expected_total = 1usize;
    for _ in 0..16 {
        let messages = split_numbered_with_total(label, &source, expected_total);
        if messages.len() == expected_total {
            return messages;
        }
        expected_total = messages.len().max(1);
    }
    split_numbered_with_total(label, &source, expected_total)
}

fn split_numbered_with_total(label: &str, source: &str, total: usize) -> Vec<String> {
    let chars = source.chars().collect::<Vec<_>>();
    // 空 label 表示「只按显示宽度切分、不要编号」：AI 聊天这类纯文本回复
    // 若带上 "1/1：" 前缀，玩家会看到与内容无关的编号噪声。
    let numbered = !label.is_empty();
    if chars.is_empty() {
        return vec![if numbered {
            format!("{}1/1：", label)
        } else {
            String::new()
        }];
    }

    let mut messages = Vec::new();
    let mut offset = 0usize;
    while offset < chars.len() {
        let index = messages.len() + 1;
        let prefix = if numbered {
            format!("{}{}/{}：", label, index, total.max(1))
        } else {
            String::new()
        };
        let available = MAX_CHAT_WIDTH.saturating_sub(display_width(&prefix));
        let mut chunk = String::new();
        let mut width = 0usize;
        while offset < chars.len() {
            let next_width = char_width(chars[offset]);
            if !chunk.is_empty() && width + next_width > available {
                break;
            }
            if chunk.is_empty() && next_width > available {
                break;
            }
            chunk.push(chars[offset]);
            width += next_width;
            offset += 1;
        }
        if chunk.is_empty() {
            chunk.push(chars[offset]);
            offset += 1;
        }
        messages.push(format!("{}{}", prefix, chunk));
    }
    messages
}

fn normalize_segment_source(message: &str) -> String {
    let mut output = String::new();
    let mut previous_was_line_break = false;
    for ch in message.trim().chars() {
        match ch {
            '\r' => {}
            '\n' => {
                if !previous_was_line_break && !output.ends_with(' ') {
                    output.push(' ');
                }
                previous_was_line_break = true;
            }
            _ => {
                output.push(ch);
                previous_was_line_break = false;
            }
        }
    }
    output
}

fn normalize_comparison_char(ch: char) -> Option<char> {
    if ch.is_whitespace() || is_comparison_punctuation(ch) {
        return None;
    }
    if ('\u{ff01}'..='\u{ff5e}').contains(&ch) {
        return char::from_u32(ch as u32 - 0xfee0);
    }
    Some(ch)
}

fn is_comparison_punctuation(ch: char) -> bool {
    ch.is_ascii_punctuation()
        || matches!(
            ch,
            '，' | '。'
                | '、'
                | '；'
                | '：'
                | '？'
                | '！'
                | '（'
                | '）'
                | '【'
                | '】'
                | '《'
                | '》'
                | '“'
                | '”'
                | '‘'
                | '’'
                | '￥'
                | '·'
                | '—'
                | '～'
                | '…'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison_text_ignores_width_case_whitespace_and_punctuation() {
        assert_eq!(normalize_comparison_text(" ＡbＣ，萌！ "), "abc萌");
        assert_eq!(normalize_comparison_text("当前 大厅"), "当前大厅");
    }

    #[test]
    fn configured_lines_are_split_by_newline_into_separate_messages() {
        // 用户在一条回复里写多行 = 要发多条消息。
        let messages = split_configured_lines("已收到命令\n已暂停");
        assert_eq!(messages, ["已收到命令", "已暂停"]);
    }

    #[test]
    fn blank_lines_are_ignored_and_windows_newlines_are_supported() {
        let messages = split_configured_lines("第一条\r\n\r\n\n第二条\n   \n");
        assert_eq!(messages, ["第一条", "第二条"]);
    }

    #[test]
    fn overlong_lines_are_wrapped_without_numbering() {
        // 超宽行按显示宽度硬切，但不加 "1/2：" 编号：用户写的就是要发的原文。
        let long = "字".repeat(100);
        let messages = split_configured_lines(&long);
        assert!(messages.len() > 1);
        for message in &messages {
            assert!(display_width(message) <= MAX_CHAT_WIDTH, "{message}");
            assert!(!message.contains('/'), "拆条不应插入编号前缀");
        }
        assert_eq!(messages.concat(), long);
    }

    #[test]
    fn a_single_short_line_stays_one_message() {
        assert_eq!(split_configured_lines("已暂停"), ["已暂停"]);
    }

    #[test]
    fn empty_text_produces_no_messages() {
        assert!(split_configured_lines("").is_empty());
        assert!(split_configured_lines("   \n  \n").is_empty());
    }

    #[test]
    fn short_messages_are_not_numbered() {
        // 放得下就原样发送：给短文案加 "1/1：" 是纯噪声。
        // 这条同时守住生产路径（组装层渲染）与测试助手共用的同一个判断，
        // 两边曾经分叉过一次，导致生产给塔罗短文案加了编号。
        let messages = fit_chat_message("塔罗", "今日塔罗牌");
        assert_eq!(messages, ["今日塔罗牌"]);
    }

    #[test]
    fn overlong_messages_are_split_and_numbered() {
        let messages = fit_chat_message("塔罗", &"很长".repeat(80));
        assert!(messages.len() > 1);
        assert!(messages[0].starts_with("塔罗1/"));
        assert!(
            messages
                .iter()
                .all(|message| display_width(message) <= MAX_CHAT_WIDTH)
        );
    }

    #[test]
    fn numbered_chat_segments_obey_the_game_width_limit() {
        let messages = split_numbered_chat_message("汤面", &"很长".repeat(80));

        assert!(messages.len() > 1);
        assert!(
            messages
                .iter()
                .all(|message| display_width(message) <= MAX_CHAT_WIDTH)
        );
    }

    /// 空 label 表示"只按宽度切、不编号"。
    /// AI 聊天把模型回答交给这里分片，若带上 "1/1：" 前缀，玩家会看到与内容无关的编号。
    #[test]
    fn empty_label_splits_without_numbering_noise() {
        assert_eq!(
            split_numbered_chat_message("", "今天天气不错"),
            vec!["今天天气不错".to_string()]
        );

        let long = "很长".repeat(80);
        let messages = split_numbered_chat_message("", &long);
        assert!(messages.len() > 1, "超宽文本仍要分片");
        assert!(
            messages.iter().all(|message| !message.contains("1/")),
            "空 label 不应出现编号前缀: {messages:?}"
        );
        assert!(
            messages
                .iter()
                .all(|message| display_width(message) <= MAX_CHAT_WIDTH)
        );
        // 分片后拼回原文，确认没有内容丢失。
        assert_eq!(messages.concat(), long);
    }
}

/// 把一行聊天文本切成「发送者 + 正文」。
///
/// 一级/二级大厅的 OCR 文本本身带昵称前缀（`[昵称]:内容`、`昵称：内容`），
/// 消息结构里**没有**独立的 sender 字段，因此任何需要昵称的消费者都要走这里，
/// 不要各自再写一套切分规则（全角冒号、方括号、昵称里的标点都要一致）。
///
/// 拆不开就返回 `None`：调用方必须把「拿不到发送者」当作 Unknown 处理，
/// 不能编造身份去授权或记账。
///
/// 放在 `text` 而不是 `interfaces`：OCR 扫描层（`observation`）也要用它把
/// 昵称从正文里剥出来，而 `observation` 不允许依赖 `interfaces`。
pub(crate) fn split_sender_line<'a>(
    text: &'a str,
    message_type: &str,
) -> Option<(String, &'a str)> {
    if message_type == "pink" || text.trim_start().starts_with(['[', '【']) {
        return extract_bracket_sender(text);
    }
    let separator_index = text.find(['：', ':', ']', '】'])?;
    let separator_len = text[separator_index..].chars().next()?.len_utf8();
    Some((
        text[..separator_index].trim().to_string(),
        &text[separator_index + separator_len..],
    ))
}

fn extract_bracket_sender(text: &str) -> Option<(String, &str)> {
    let (start, open) = text
        .char_indices()
        .find(|(_, ch)| matches!(ch, '[' | '【'))?;
    let close = if open == '[' { ']' } else { '】' };
    let name_start = start + open.len_utf8();
    let end = name_start + text[name_start..].find(close)?;
    let username = text[name_start..end].trim();
    if username.is_empty() {
        None
    } else {
        // Split after the closing bracket so punctuation inside a nickname is preserved.
        Some((username.to_string(), &text[end + close.len_utf8()..]))
    }
}
