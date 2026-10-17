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
    if chars.is_empty() {
        return vec![format!("{}1/1：", label)];
    }

    let mut messages = Vec::new();
    let mut offset = 0usize;
    while offset < chars.len() {
        let index = messages.len() + 1;
        let prefix = format!("{}{}/{}：", label, index, total.max(1));
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
    fn numbered_chat_segments_obey_the_game_width_limit() {
        let messages = split_numbered_chat_message("汤面", &"很长".repeat(80));

        assert!(messages.len() > 1);
        assert!(
            messages
                .iter()
                .all(|message| display_width(message) <= MAX_CHAT_WIDTH)
        );
    }
}
