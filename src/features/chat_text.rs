pub(crate) use crate::text::{MAX_CHAT_WIDTH, display_width, split_numbered_chat_message};

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

pub(crate) fn command_identity(text: &str) -> String {
    let normalized = normalize_comparison_text(text);
    if normalized.is_empty() {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        normalized
    }
}

use crate::text::normalize_comparison_text;

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
