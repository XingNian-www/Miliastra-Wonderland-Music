//! 给模型看的时钟文本。
//!
//! 固定 UTC+8：日期时间换算统一走 `miliastra_kernel::date_time`，
//! 与 `adapters::logging`、`features::tarot` 共用同一份「不依赖操作系统时区、
//! 也没有夏令时偏移」的实现。用 OS 本地时区会在 Linux 服务器 / CI 上
//! 变成 UTC，给模型完全错误的作息判断。
//!
//! 名称刻意不叫 `time_text`：`ui::locator::time_text` 是歌单剩余时间，不是时钟。

use miliastra_kernel::date_time::{utc8_datetime, utc8_day as kernel_utc8_day};

const WEEKDAYS: [&str; 7] = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];

/// 固定 UTC+8 的日序号（1970-01-01 为 0）。
pub(crate) fn utc8_day(unix_seconds: u64) -> i64 {
    kernel_utc8_day(unix_seconds as i64)
}

/// 给模型看的单行时间文本，形如 `2026-10-27 周二 23:47（深夜）`。
///
/// **不输出秒**：秒级对「晚安」「今天/明天」这类判断毫无价值，
/// 却会让同一分钟内的多次调用产生不同提示词，白白破坏前缀缓存。
pub(crate) fn describe(unix_seconds: u64) -> String {
    let (year, month, date, hour, minute, _) = utc8_datetime(unix_seconds as i64);
    // 1970-01-01 是周四。
    let weekday = WEEKDAYS[((utc8_day(unix_seconds) + 4).rem_euclid(7)) as usize];
    format!(
        "{year:04}-{month:02}-{date:02} {weekday} {hour:02}:{minute:02}（{}）",
        period_label(hour)
    )
}

/// 时段标签。模型直接从词拿「深夜/早上」，比让它从 `23:47` 自己推更稳。
fn period_label(hour: u32) -> &'static str {
    match hour {
        0..=4 => "凌晨",
        5..=7 => "早上",
        8..=10 => "上午",
        11..=12 => "中午",
        13..=17 => "下午",
        18..=22 => "晚上",
        _ => "深夜",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: u64) -> String {
        describe(seconds)
    }

    /// 时区回归：换成 OS 本地时区或 chrono::Local 这条必挂。
    ///
    /// 1_793_026_800 = 2026-10-26 15:00 UTC = 2026-10-26 23:00 UTC+8。
    #[test]
    fn formats_in_utc8_regardless_of_the_host_timezone() {
        let text = at(1_793_026_800);
        assert!(text.starts_with("2026-10-26"), "实际: {text}");
        assert!(text.contains("周一"), "实际: {text}");
        assert!(text.contains("23:00"), "实际: {text}");
        assert!(text.contains("深夜"), "实际: {text}");
    }

    /// 纪元锚点：1970-01-01 是周四，UTC+8 已是当天 08:00。
    #[test]
    fn epoch_lands_on_thursday_eight_am() {
        assert_eq!(at(0), "1970-01-01 周四 08:00（上午）");
    }

    /// 同一分钟内相差几十秒必须完全一致——这是缓存稳定性的核心。
    #[test]
    fn output_is_stable_within_the_same_minute() {
        assert_eq!(at(1_793_026_800), at(1_793_026_830));
    }

    /// 时段边界逐个钉死，避免以后调分档时静默改行为。
    #[test]
    fn period_boundaries_are_pinned() {
        let base = 1_793_026_800 - 23 * 3600; // 当天 00:00 (UTC+8)
        for (hour, expected) in [
            (0, "凌晨"),
            (4, "凌晨"),
            (5, "早上"),
            (7, "早上"),
            (8, "上午"),
            (10, "上午"),
            (11, "中午"),
            (12, "中午"),
            (13, "下午"),
            (17, "下午"),
            (18, "晚上"),
            (22, "晚上"),
            (23, "深夜"),
        ] {
            let text = at(base + hour * 3600);
            assert!(
                text.contains(expected),
                "{hour} 点应为 {expected}，实际: {text}"
            );
        }
    }

    /// 跨日：23:59 到次日 00:00 日期与星期一起翻转。
    #[test]
    fn midnight_rolls_the_date_forward() {
        let late = at(1_793_026_800 + 59 * 60); // 23:59
        let next = at(1_793_026_800 + 60 * 60); // 次日 00:00
        assert!(late.contains("2026-10-26"), "{late}");
        assert!(next.contains("2026-10-27"), "{next}");
        assert!(next.contains("周二"), "{next}");
    }
}
