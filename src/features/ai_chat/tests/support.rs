//! 测试共用夹具。

use rusqlite::Connection;

use super::*;

/// 测试固定时刻：2026-10-26 23:00 (UTC+8)，便于断言时间行。
pub(super) const TEST_NOW_UNIX: u64 = 1_793_026_800;

pub(super) fn config() -> AiChatConfig {
    AiChatConfig {
        enabled: true,
        keyword: "橘子".to_string(),
        ..AiChatConfig::default()
    }
}

pub(super) fn memory() -> AiChatMemory {
    AiChatMemory::from_connection(Connection::open_in_memory().unwrap()).unwrap()
}
