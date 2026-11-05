//! 固定 UTC+8 的公历日期时间换算。
//!
//! 项目里的时间显示一律按 UTC+8，且**不依赖操作系统时区**（Windows 客户端与
//! Linux 服务器/CI 必须得到同样的结果，也没有夏令时偏移）。此前
//! `adapters::logging`、`features::clock_text`、`features::tarot`、
//! `composition::application` 与 playback 的 QQ 音乐适配器各自复制了一份
//! 「日序号 → 年月日」的换算；这里收拢成唯一实现，各调用点只保留自己的输出格式串。
//!
//! 算法是 Howard Hinnant 的 `civil_from_days`：先把 UTC 日序号平移到一个
//! 400 年的大周期内，再按月内序号反推月、日。它能正确处理负数日序号
//! （1970-01-01 之前）与 1582 年以前的公历日期。

/// UTC+8 相对 UTC 的固定偏移（秒）。
pub const UTC8_OFFSET_SECONDS: i64 = 8 * 60 * 60;

/// 一天的秒数。
pub const SECONDS_PER_DAY: i64 = 24 * 60 * 60;

/// UTC 日序号（1970-01-01 为 0）→ 公历 `(年, 月, 日)`。
///
/// 这是各调用点共用的底层换算；需要 UTC+8 时先用 [`utc8_datetime`] /
/// [`utc8_date`] / [`utc8_day`]，不要自己在外面加偏移再各算一遍。
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

/// Unix 秒 → UTC+8 的 `(年, 月, 日, 时, 分, 秒)`。
///
/// 用 `div_euclid`/`rem_euclid` 而不是 `/`/`%`：1970-01-01 之前的输入
/// （负秒数）也必须落在正确的日期上，而不是向零取整到同一天。
pub fn utc8_datetime(unix_seconds: i64) -> (i64, u32, u32, u32, u32, u32) {
    let local = unix_seconds + UTC8_OFFSET_SECONDS;
    let days = local.div_euclid(SECONDS_PER_DAY);
    let second_of_day = local.rem_euclid(SECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        (second_of_day / 3_600) as u32,
        (second_of_day % 3_600 / 60) as u32,
        (second_of_day % 60) as u32,
    )
}

/// Unix 秒 → UTC+8 的 `(年, 月, 日)`，只需要日期时的便捷函数。
pub fn utc8_date(unix_seconds: i64) -> (i64, u32, u32) {
    let (year, month, day, ..) = utc8_datetime(unix_seconds);
    (year, month, day)
}

/// Unix 秒 → UTC+8 的日序号（1970-01-01 的 UTC+8 日序号为 0）。
///
/// 按日固定的功能（塔罗每日牌、日志按日归档、给模型看的时钟）只关心日期，
/// 不需要再走一遍年月日换算。
pub fn utc8_day(unix_seconds: i64) -> i64 {
    (unix_seconds + UTC8_OFFSET_SECONDS).div_euclid(SECONDS_PER_DAY)
}

#[cfg(test)]
mod tests {
    use super::{
        SECONDS_PER_DAY, UTC8_OFFSET_SECONDS, civil_from_days, utc8_date, utc8_datetime, utc8_day,
    };

    #[test]
    fn epoch_is_the_utc8_morning_of_1970_01_01() {
        assert_eq!(utc8_datetime(0), (1970, 1, 1, 8, 0, 0));
        assert_eq!(utc8_datetime(-UTC8_OFFSET_SECONDS), (1970, 1, 1, 0, 0, 0));
        assert_eq!(
            utc8_datetime(-UTC8_OFFSET_SECONDS - 1),
            (1969, 12, 31, 23, 59, 59)
        );
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn leap_day_survives_the_round_trip() {
        // 2024-02-29 00:00 UTC+8；前一秒仍是 02-28。
        assert_eq!(utc8_datetime(1_709_136_000), (2024, 2, 29, 0, 0, 0));
        assert_eq!(utc8_datetime(1_709_136_000 - 1), (2024, 2, 28, 23, 59, 59));
        assert_eq!(utc8_datetime(1_709_222_400), (2024, 3, 1, 0, 0, 0));
        // 2000-02-29 是闰日；1900 不是闰年，3 月 1 日仍按 3 月 1 日算。
        assert_eq!(utc8_date(951_753_600), (2000, 2, 29));
        assert_eq!(utc8_date(-2_203_920_000), (1900, 3, 1));
    }

    #[test]
    fn crossing_midnight_advances_the_date() {
        // 2026-10-26 23:59:59 → 2026-10-27 00:00:00（UTC+8）。
        assert_eq!(utc8_datetime(1_793_030_399), (2026, 10, 26, 23, 59, 59));
        assert_eq!(utc8_datetime(1_793_030_400), (2026, 10, 27, 0, 0, 0));
        // 跨年：2025-12-31 23:59:59 → 2026-01-01 00:00:00。
        assert_eq!(utc8_datetime(1_767_196_799), (2025, 12, 31, 23, 59, 59));
        assert_eq!(utc8_datetime(1_767_196_800), (2026, 1, 1, 0, 0, 0));
    }

    #[test]
    fn utc8_day_matches_the_date_and_handles_negative_input() {
        assert_eq!(utc8_day(0), 0);
        // UTC 的最后一秒在 UTC+8 已经是当天早上，仍算第 0 天。
        assert_eq!(utc8_day(-1), 0);
        assert_eq!(utc8_day(-UTC8_OFFSET_SECONDS), 0);
        assert_eq!(utc8_day(-UTC8_OFFSET_SECONDS - 1), -1);
        assert_eq!(utc8_day(1_793_030_399), 20_752);
        assert_eq!(utc8_day(1_793_030_400), 20_753);
        assert_eq!(utc8_day(SECONDS_PER_DAY * 365), 365);
    }

    #[test]
    fn date_convenience_matches_the_full_conversion() {
        for seconds in [
            0,
            -1,
            -UTC8_OFFSET_SECONDS,
            1_709_136_000,
            1_793_030_399,
            1_793_030_400,
        ] {
            let (year, month, day, ..) = utc8_datetime(seconds);
            assert_eq!(utc8_date(seconds), (year, month, day));
        }
    }
}
