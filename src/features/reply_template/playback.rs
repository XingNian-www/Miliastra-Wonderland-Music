//! 播放控制类回复文案。
//!
//! 本文件由 playback 模块负责；新增模板请在 `PLAYBACK` 数组追加，
//! 不要改动其它模块的文件（并行开发按文件隔离，避免写冲突）。

use super::ReplyTemplate;

/// 播放控制回复。键一旦发布不要改名，否则用户已保存的覆盖会失效。
pub(crate) const PLAYBACK: &[ReplyTemplate] = &[];
