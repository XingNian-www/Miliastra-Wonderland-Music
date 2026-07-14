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
