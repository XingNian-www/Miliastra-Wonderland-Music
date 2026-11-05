//! 跨 crate 共享的文本归一化。

/// 归一化：全角 ASCII 折半角、统一小写、空白折叠成单个空格。
///
/// 两个调用方曾经各写一份逐字相同的实现：应用层的 AI 搜索词比对与
/// playback 的曲库检索。playback 不能依赖应用层，所以放这里。
pub fn normalize_folded(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '\u{3000}' => ' ',
            '\u{FF01}'..='\u{FF5E}' => char::from_u32(ch as u32 - 0xFEE0).unwrap_or(ch),
            other => other,
        })
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
