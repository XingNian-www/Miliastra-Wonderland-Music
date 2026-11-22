//! 聊天文本脱敏：只影响日志/诊断/HTTP 展示，不改变业务参数与比较键。
//!
//! 判定**一律**先做归一化：入参是**未归一化的原始 OCR 文本**（`message.text`），
//! 全角/半角、空格与大小写抖动都会出现在同一条命令里。下面每一条判定吃到的
//! 都是 [`folded_for_match`] 的输出，没有例外——不要新增按原文 `contains`
//! 的分支，那会让「全角能脱敏、半角漏脱敏」的缺陷重新长出来。
//! 归一化只用于匹配，返回给调用方的始终是原文（命中时返回占位串），
//! 不改变业务参数与比较键。

use crate::text::{OCR_TRAILING_NOISE, normalize_chat_text};

pub(crate) const REDACTED_TURTLE_SOUP_BOTTOM: &str = "[海龟汤汤底已隐藏]";
pub(crate) const REDACTED_UNDERCOVER_SECRET: &str = "[谁是卧底秘密内容已隐藏]";
pub(crate) const REDACTED_UNDERCOVER_INPUT: &str = "[谁是卧底私聊内容已隐藏]";
pub(crate) const REDACTED_PLAYLIST_IMPORT: &str = "[歌单导入命令与分享链接已隐藏]";

/// 判定用的归一化视图：全角 ASCII 折半角、丢弃空白、折成小写。
///
/// 复用 `crate::text::normalize_chat_text`（消空白 + 折全角），再补一次小写；
/// 不在这里复制第二套折算规则。`MUST_READ.md` 说的“OCR 链路已归一化”对
/// `redacted_chat_text` 的入参并不成立，所以这条路径必须自己折一次。
/// 折完之后 `：` 一律是 `:`，判定只写半角冒号即可同时覆盖两种输入。
fn folded_for_match(message: &str) -> String {
    normalize_chat_text(message).to_lowercase()
}

/// 歌单导入命令名（小写；比较对象是 [`folded_for_match`] 的输出）。
///
/// 命令不依赖空格，所以归一化后的比较串里没有空白；
/// `@` 也可能是全角 `＠`，归一化后同样是半角。
const PLAYLIST_IMPORT_COMMANDS: &[&str] = &["@导入qq歌单", "@导入网易歌单", "@导入网易云歌单"];

pub(crate) fn redacted_chat_text(message: &str) -> &str {
    let folded = folded_for_match(message);
    if contains_playlist_import_command(&folded) {
        REDACTED_PLAYLIST_IMPORT
    } else if contains_turtle_soup_bottom_marker(&folded) {
        REDACTED_TURTLE_SOUP_BOTTOM
    } else if contains_undercover_secret(&folded) {
        REDACTED_UNDERCOVER_SECRET
    } else if contains_undercover_private_input(&folded) {
        REDACTED_UNDERCOVER_INPUT
    } else {
        message
    }
}

fn contains_playlist_import_command(folded: &str) -> bool {
    PLAYLIST_IMPORT_COMMANDS
        .iter()
        .any(|command| folded.contains(command))
}

/// 卧底秘密：座位/词语单发给玩家，或主持人播报的谜底与平民词/卧底词对照。
///
/// 入参已归一化，因此：
/// - `你的位置：` / `你的位置:` 是同一条判定（折全角后都是半角冒号）；
/// - 空白已被丢弃，`你的位置 ： A` 这类 OCR 抖动同样命中。
fn contains_undercover_secret(folded: &str) -> bool {
    (folded.contains("你的位置:") && folded.contains("你的词语:"))
        || folded.contains("谁是卧底谜底")
        || contains_undercover_secret_summary(folded)
}

fn contains_undercover_secret_summary(folded: &str) -> bool {
    folded.contains("平民词:") && folded.contains("卧底词:") && folded.contains("卧底:")
}

/// 卧底私聊输入：`#A` 或 `#投A`，位置字母 A–K。
///
/// 入参已归一化（无空白、全角折半角、小写），所以这里只需处理半角 `#`、
/// 小写 `a..=k` 与半角 `:`。OCR 偶尔把句末标点一起读进气泡（`#投A。`），
/// 尾噪声先用 `OCR_TRAILING_NOISE` 修剪再判，否则整条命令会被漏脱敏。
fn is_undercover_private_command(candidate: &str) -> bool {
    let candidate = candidate.trim_start_matches([':', ']', '】']);
    let candidate = candidate.trim_end_matches(OCR_TRAILING_NOISE);
    let Some(command) = candidate.strip_prefix('#') else {
        return false;
    };
    let vote = command.strip_prefix('投').unwrap_or(command);
    let mut chars = vote.chars();
    chars
        .next()
        .is_some_and(|position| ('a'..='k').contains(&position))
        && chars.next().is_none()
}

fn contains_undercover_private_input(folded: &str) -> bool {
    // 昵称本身可能带冒号（「玩家:甲：#投A」），所以不能只在第一个分隔符处切一刀：
    // 每个分隔符之后都试一次，任意一处能解析成「#A / #投A」即成立。
    // 整串也试一次，保留「没有分隔符、正文直接是命令」的既有行为。
    is_undercover_private_command(folded)
        || folded
            .match_indices([':', ']', '】'])
            .any(|(index, separator)| {
                is_undercover_private_command(&folded[index + separator.len()..])
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// B12-P1-7：这些变体只要漏一个，分享链接或私聊投票原文就会进日志。
    #[test]
    fn playlist_import_matching_tolerates_fullwidth_spaces_and_case() {
        for message in [
            // 全角 ＠
            "＠导入QQ歌单1000000001",
            // 命令内空格
            "@导入 QQ 歌单 1000000001",
            // 小写 qq
            "@导入qq歌单1000000001",
            // 混合大小写 + 分享链接
            "@导入Qq歌单https://y.qq.com/n/ryqq/playlist/123",
            // 全角 ＠ + 空格 + 网易歌单
            "＠导入 网易 歌单https://music.163.com/playlist?id=123&uct2=private",
        ] {
            assert_eq!(
                redacted_chat_text(message),
                REDACTED_PLAYLIST_IMPORT,
                "歌单导入命令未被脱敏：{message}"
            );
        }
    }

    #[test]
    fn redacts_playlist_import_links_from_chat_logs() {
        assert_eq!(
            redacted_chat_text("@导入网易歌单https://music.163.com/playlist?id=123&uct2=private"),
            REDACTED_PLAYLIST_IMPORT
        );
        assert_eq!(
            redacted_chat_text("@导入QQ歌单1000000001"),
            REDACTED_PLAYLIST_IMPORT
        );
    }

    /// B12-P1-7：半角冒号、昵称含冒号、全角位置字母都必须能识别。
    #[test]
    fn undercover_private_input_matching_tolerates_colon_and_position_variants() {
        for message in [
            // 半角冒号
            "玩家甲：#投A",
            // 半角冒号 + 全角位置字母
            "玩家甲:#投Ａ",
            // 昵称本身含冒号：不能只在第一个冒号处切一刀
            "玩家:甲：#投K",
            // 全角 ＃ + 全角小写字母
            "玩家甲：＃投ａ",
            // 全角括号结尾
            "【玩家甲】#B",
        ] {
            assert_eq!(
                redacted_chat_text(message),
                REDACTED_UNDERCOVER_INPUT,
                "卧底私聊输入未被识别：{message}"
            );
        }
    }

    /// 归一化只用于判定，不能把不相关的聊天一起脱敏。
    #[test]
    fn ordinary_chat_is_returned_unchanged() {
        for message in ["今天打歌单吗", "玩家甲：#投AB", "普通聊天：你好"] {
            assert_eq!(redacted_chat_text(message), message);
        }
    }

    /// 卧底秘密单发：全角与半角冒号都必须脱敏。
    ///
    /// 半角变体曾经直接漏掉：位置和自己的词被原样写进日志，正是这一轮要修的
    /// 那类缺陷。两条分支判的是同一种内容，不允许按字形不同区别对待。
    #[test]
    fn undercover_secret_matching_tolerates_halfwidth_colon() {
        for message in [
            "你的位置：A；你的词语：苹果",
            "你的位置:A；你的词语:苹果",
            "你的位置 : A ； 你的词语 : 苹果",
            "你的位置:Ｋ；你的词语:橘子",
        ] {
            assert_eq!(
                redacted_chat_text(message),
                REDACTED_UNDERCOVER_SECRET,
                "卧底秘密未被脱敏：{message}"
            );
        }
    }

    /// 谜底播报：全角/半角的「平民词 / 卧底词 / 卧底」标记都要命中。
    #[test]
    fn undercover_secret_summary_tolerates_halfwidth_colon() {
        for message in [
            "本局谜底：平民词：苹果；卧底词：橘子；卧底：玩家甲",
            "本局谜底：平民词:苹果；卧底词:橘子；卧底:玩家甲",
            "谁是卧底谜底（第 3 局）",
        ] {
            assert_eq!(
                redacted_chat_text(message),
                REDACTED_UNDERCOVER_SECRET,
                "谜底播报未被脱敏：{message}"
            );
        }
    }

    /// OCR 会把句末标点读进气泡：`#投A。` 与 `#投A` 是同一个命令，
    /// 修剪 `OCR_TRAILING_NOISE` 后必须一起命中，否则投票原文就进日志了。
    #[test]
    fn undercover_private_input_tolerates_ocr_trailing_noise() {
        for message in [
            "#投A。",
            "玩家甲：#投A。",
            "#B！",
            "#k、",
            "【玩家甲】#投C」",
        ] {
            assert_eq!(
                redacted_chat_text(message),
                REDACTED_UNDERCOVER_INPUT,
                "带 OCR 尾噪声的私聊输入未被识别：{message}"
            );
        }
    }
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
