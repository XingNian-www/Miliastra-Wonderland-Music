pub(crate) const MAX_CHAT_WIDTH: usize = 80;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandSyntax<'a, T> {
    pub(crate) matched: &'static str,
    pub(crate) argument: &'a str,
    pub(crate) command: T,
}

pub(crate) fn parse_prefixed_command<'a>(
    text: &'a str,
    prefix: &'static str,
    allows_argument: bool,
) -> Option<&'a str> {
    let rest = strip_ascii_case_prefix(text, prefix)?;
    if !rest.is_empty() && rest.starts_with('/') {
        return None;
    }
    let argument = rest
        .trim_start_matches(['：', ':', ' ', '\t'])
        .trim_end_matches([']', '】'])
        .trim();
    (allows_argument || argument.is_empty()).then_some(argument)
}

pub(crate) fn strip_ascii_case_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then_some(&text[prefix.len()..])
}

pub(crate) fn shortcut_argument<'a>(payload: &'a str, prefix: &str) -> Option<&'a str> {
    let value = payload.strip_prefix(prefix)?;
    let value = value.trim_start_matches(['：', ':', ' ', '\t']).trim();
    (!value.is_empty()).then_some(value)
}

pub(crate) fn compact_command(payload: &str) -> String {
    payload.chars().filter(|ch| !ch.is_whitespace()).collect()
}

pub(crate) fn is_command_boundary(ch: Option<char>) -> bool {
    match ch {
        None => true,
        Some(ch) => {
            ch.is_whitespace()
                || matches!(
                    ch,
                    '，' | ',' | '。' | '.' | '!' | '！' | '?' | '？' | ']' | '】'
                )
        }
    }
}

pub(crate) fn display_width(value: &str) -> usize {
    value.chars().map(char_width).sum()
}

pub(crate) fn char_width(ch: char) -> usize {
    if ch.is_ascii() { 1 } else { 2 }
}

pub(crate) fn command_identity(text: &str) -> String {
    let normalized = normalize_comparison_text(text);
    if normalized.is_empty() {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        normalized
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

use crate::text::normalize_comparison_text;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixed_command_requires_the_declared_argument_shape() {
        assert_eq!(
            parse_prefixed_command("音量：50】", "音量", true),
            Some("50")
        );
        assert_eq!(parse_prefixed_command("暂停", "暂停", false), Some(""));
        assert_eq!(parse_prefixed_command("暂停 现在", "暂停", false), None);
        assert_eq!(parse_prefixed_command("暂停/下一首", "暂停", false), None);
    }
}
