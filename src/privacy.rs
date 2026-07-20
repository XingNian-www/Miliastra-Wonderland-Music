pub(crate) const REDACTED_TURTLE_SOUP_BOTTOM: &str = "[海龟汤汤底已隐藏]";
pub(crate) const REDACTED_UNDERCOVER_SECRET: &str = "[谁是卧底秘密内容已隐藏]";
pub(crate) const REDACTED_UNDERCOVER_INPUT: &str = "[谁是卧底私聊内容已隐藏]";

pub(crate) fn redacted_chat_text(message: &str) -> &str {
    if contains_turtle_soup_bottom_marker(message) {
        REDACTED_TURTLE_SOUP_BOTTOM
    } else if (message.contains("你的位置：") && message.contains("你的词语："))
        || message.contains("谁是卧底谜底")
        || contains_undercover_secret_summary(message)
    {
        REDACTED_UNDERCOVER_SECRET
    } else if contains_undercover_private_input(message) {
        REDACTED_UNDERCOVER_INPUT
    } else {
        message
    }
}

fn contains_undercover_secret_summary(message: &str) -> bool {
    let has_civilian_word = message.contains("平民词:") || message.contains("平民词：");
    let has_undercover_word = message.contains("卧底词:") || message.contains("卧底词：");
    let has_undercover_positions = message.contains("卧底:") || message.contains("卧底：");
    has_civilian_word && has_undercover_word && has_undercover_positions
}

fn contains_undercover_private_input(message: &str) -> bool {
    let body = message
        .find(['：', ':', ']', '】'])
        .map_or(message, |index| {
            &message[index + message[index..].chars().next().map_or(0, char::len_utf8)..]
        })
        .trim_start_matches(['：', ':', ' ', '\t', ']', '】']);
    let Some(command) = body.strip_prefix('#').or_else(|| body.strip_prefix('＃')) else {
        return false;
    };
    let command = command
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>();
    let vote = command.strip_prefix('投').unwrap_or(&command);
    let mut chars = vote.chars();
    chars
        .next()
        .is_some_and(|position| ('A'..='K').contains(&position.to_ascii_uppercase()))
        && chars.next().is_none()
}

fn contains_turtle_soup_bottom_marker(message: &str) -> bool {
    let mut saw_soup = false;
    for ch in message.chars() {
        if ch.is_whitespace() {
            continue;
        }
        if saw_soup && ch == '底' {
            return true;
        }
        saw_soup = ch == '汤';
    }
    false
}
