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
pub(crate) use general::{
    HotkeyConfig, OutputConfig, PointConfig, RectConfig, ScreenConfig, WindowConfig,
};
pub(crate) use paths::normalize_native_path_string;
pub(crate) use paths::validate_rect;
#[cfg(test)]
pub(crate) use playback::AudioCacheFileConfig;
pub(crate) use system::{HttpConfig, KugouDeviceConfig, LoggingConfig, TuiConfig};
pub(crate) use ui::{
    ChatSourceConfig, ChatSourceDisconnectAction, HallTimingConfig, InputTimingConfig, OcrConfig,
    TemplateConfig, TimingConfig,
};
