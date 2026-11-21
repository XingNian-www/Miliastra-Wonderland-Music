//! 触发冷却闸门。
//!
//! 冷却时长**不缓存**：由调用方每次 `check` 时传入（来自
//! `live_configs.snapshot().ai_chat.cooldown_seconds`），保存后下一条消息即按新值判定。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::budget::MAX_TRACKED_ACTORS;

/// 每个玩家的触发冷却。
///
/// 冷却时长**不缓存**：由调用方每次 `check` 时传入（来自
/// `live_configs.snapshot().ai_chat.cooldown_seconds`），保存后下一条消息即按新值判定。
pub(crate) struct TriggerGate {
    recent: Mutex<std::collections::HashMap<String, Instant>>,
}

impl TriggerGate {
    pub(crate) fn new() -> Self {
        Self {
            recent: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 允许触发返回 true；否则返回还需等待的秒数。
    ///
    /// `cooldown` 由调用方每次从实时配置读取，保证不必重启即可改变冷却。
    pub(crate) fn check(
        &self,
        actor: &str,
        now: Instant,
        cooldown: Duration,
    ) -> std::result::Result<(), u64> {
        let Ok(mut recent) = self.recent.lock() else {
            return Err(1);
        };
        recent.retain(|_, at| now.saturating_duration_since(*at) < cooldown);
        if let Some(at) = recent.get(actor) {
            let wait = cooldown.saturating_sub(now.saturating_duration_since(*at));
            return Err(wait.as_millis().div_ceil(1000).max(1) as u64);
        }
        if recent.len() >= MAX_TRACKED_ACTORS {
            return Err(1);
        }
        recent.insert(actor.to_string(), now);
        Ok(())
    }
}
