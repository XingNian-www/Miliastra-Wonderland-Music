//! 触发冷却闸门。

use super::*;
use std::time::{Duration, Instant};

#[test]
fn trigger_gate_enforces_cooldown_per_actor() {
    let gate = TriggerGate::new();
    let now = Instant::now();
    let cd = Duration::from_secs(8);

    assert!(gate.check("甲", now, cd).is_ok());
    assert!(gate.check("甲", now, cd).is_err(), "同一人立刻再触发被拦");
    assert!(gate.check("乙", now, cd).is_ok(), "不同人互不影响");
    assert!(gate.check("甲", now + Duration::from_secs(9), cd).is_ok());
}

/// 冷却值由调用方每次传入：同一条冷却记录，换用更短的冷却后立刻可再次触发。
/// 这保证 `ai_chat.cooldown_seconds` 保存后下一条消息就按新值判定，无需重启。
#[test]
fn trigger_gate_reads_cooldown_from_caller_each_check() {
    let gate = TriggerGate::new();
    let now = Instant::now();
    assert!(gate.check("甲", now, Duration::from_secs(8)).is_ok());
    let later = now + Duration::from_secs(3);
    assert!(
        gate.check("甲", later, Duration::from_secs(8)).is_err(),
        "旧冷却 8 秒下，3 秒后仍在冷却"
    );
    assert!(
        gate.check("甲", later, Duration::from_secs(2)).is_ok(),
        "改成 2 秒冷却后，同一时刻应立刻放行（证明按调用时传入的实时值判定）"
    );
}
