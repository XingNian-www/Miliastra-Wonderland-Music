//! 带退避的重试循环。
//!
//! 两处 AI 调用（候选歌曲审核、海龟汤判定）原来各写一遍同样的
//! "尝试 retry_count+1 次、失败后等 retry_delay_ms" 循环，
//! 差异只在"每次失败打不打日志"和成功/失败时怎么用尝试次数。
//! 这里只保留循环本身，策略由调用方用自己的闭包表达。

use std::time::Duration;

pub(crate) enum RetryOutcome<T> {
    /// value 是结果，attempts 是第几次成功（1 起）。
    Ok { value: T, attempts: u32 },
    /// error 是最后一次错误，attempts 是已尝试次数。
    Err { error: anyhow::Error, attempts: u32 },
}

/// 最多尝试 max_attempts 次；失败后调用 wait(delay) 再下一次。
///
/// - attempt：执行一次，返回 Result；
/// - on_failure：每次失败后回调（尝试序号、总次数、错误），用于打日志；
/// - wait：退避等待，由调用方注入以便测试。
pub(crate) fn retry_with_backoff<T>(
    max_attempts: u32,
    delay: Duration,
    mut attempt: impl FnMut() -> anyhow::Result<T>,
    mut on_failure: impl FnMut(u32, u32, &anyhow::Error),
    mut wait: impl FnMut(Duration),
) -> RetryOutcome<T> {
    let max_attempts = max_attempts.max(1);
    let mut last_error = None;
    for attempt_index in 1..=max_attempts {
        match attempt() {
            Ok(value) => {
                return RetryOutcome::Ok {
                    value,
                    attempts: attempt_index,
                };
            }
            Err(error) => {
                on_failure(attempt_index, max_attempts, &error);
                last_error = Some(error);
                if attempt_index < max_attempts {
                    wait(delay);
                }
            }
        }
    }
    RetryOutcome::Err {
        error: last_error.unwrap_or_else(|| anyhow::anyhow!("重试失败")),
        attempts: max_attempts,
    }
}
