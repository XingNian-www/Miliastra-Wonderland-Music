pub(crate) fn normalize_comparison_text(text: &str) -> String {
    text.chars()
        .filter_map(normalize_comparison_char)
        .flat_map(|ch| ch.to_lowercase())
        .collect()
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
}
