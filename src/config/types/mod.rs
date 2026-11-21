//! 各业务域配置结构体、默认值与 `validate()`。
//!
//! `AppConfig` 的每个顶层段都对应一个结构体；拆到本目录后仍由
//! [`super::AppConfig`] 聚合，外部对 `crate::config::XxxConfig` 的引用
//! 通过父模块的精炼 `pub(crate) use` 保持零变更。
//!
//! - [`aggregate`]：`AppConfig` / `BootstrapConfig` 聚合与默认值；
//! - [`general`]：窗口、屏幕区域、稳定性、输出、状态文件、全局热键；
//! - [`ui`]：外部聊天源、时序、OCR、模板图片；
//! - [`playback`]：播放器凭据、登录助手与音频缓存；
//! - [`system`]：面板、日志、TUI、诊断存档、好友投递、酷狗设备；
//! - [`paths`]：路径解析与几何/数值校验等共享助手。

mod aggregate;
mod general;
mod paths;
mod playback;
mod system;
mod ui;

pub(crate) use aggregate::{AppConfig, BootstrapConfig, MAX_REPLY_OVERRIDE_CHARS};
pub(crate) use general::StateConfig;
pub(crate) use general::resolve_stability_count;
pub(crate) use general::{
    HotkeyConfig, OutputConfig, PointConfig, RectConfig, ScreenConfig, WindowConfig,
};
pub(crate) use general::{StabilityConfig, WindowCaptureMethod, parse_hotkey_virtual_code};
pub(crate) use paths::validate_rect;
pub(crate) use paths::{
    normalize_native_path, normalize_native_path_string, normalize_separators_text,
};
pub(crate) use paths::{
    resolve_optional_path, resolve_path, resolve_path_with_default, validate_nonempty_path,
    validate_point_in_canvas, validate_rect_in_canvas, validate_unit_interval,
};
pub(crate) use playback::{AudioCacheFileConfig, PlaybackConfig};
// 这些结构体只在 crate 内部经 `AppConfig` 或模块路径使用；`mod.rs` 只重导出
// 需要跨文件引用的类型，其余按需经 `types::<域>::Xxx` 访问，避免制造
// 「重导出了但没人用」的死代码告警。
pub(crate) use system::{HttpConfig, KugouDeviceConfig, LoggingConfig, TuiConfig};
pub(crate) use ui::{
    ChatScanTimingConfig, ChatSourceConfig, ChatSourceDisconnectAction, CommandTimingConfig,
    DecisionTimingConfig, ExternalTimingConfig, HallTimingConfig, InputTimingConfig, OcrConfig,
    OpenVinoConfig, TemplateConfig, TimingConfig,
};
