//! 系统段配置：Web/API 面板、日志、终端 TUI、诊断存档、好友投递、酷狗设备。
//!
//! 这一组是运行环境的配置：面板监听哪个地址、日志写哪里、诊断帧要不要落盘。

use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::paths::validate_nonempty_path;

/// 酷狗设备标识：GUID、设备型号与 MAC 由用户提供，用于登录与取流请求。
///
/// 三项必须同时填写；留空表示沿用凭据目录下已有的 kugou-device.json。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KugouDeviceConfig {
    #[serde(default)]
    pub device_guid: String,
    #[serde(default)]
    pub device_dev: String,
    #[serde(default)]
    pub device_mac: String,
}

impl KugouDeviceConfig {
    /// 酷狗设备信息允许整体留空（沿用已有 kugou-device.json），
    /// 但不允许只填一部分，避免半个设备身份进入登录协议。
    pub(crate) fn validate(&self) -> Result<()> {
        if self.is_empty() || self.is_complete() {
            return Ok(());
        }
        bail!("kugou.device_guid、kugou.device_dev、kugou.device_mac 必须同时填写");
    }

    /// 三项是否全部填写。
    pub fn is_complete(&self) -> bool {
        !self.device_guid.trim().is_empty()
            && !self.device_dev.trim().is_empty()
            && !self.device_mac.trim().is_empty()
    }

    /// 是否完全没有填写。
    pub fn is_empty(&self) -> bool {
        self.device_guid.trim().is_empty()
            && self.device_dev.trim().is_empty()
            && self.device_mac.trim().is_empty()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    pub host: String,
    pub port: u16,
    pub enabled: bool,
    #[serde(default)]
    pub access_token: String,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 18888,
            enabled: true,
            access_token: "miliastra-default-access-token".to_string(),
        }
    }
}

impl HttpConfig {
    pub(super) fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.host.trim().is_empty() {
            bail!("http.host 不能为空");
        }
        if self.port == 0 {
            bail!("http.port 必须大于 0");
        }
        if self.access_token.trim().is_empty() {
            bail!("http.access_token 不能为空");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    pub dir: PathBuf,
    pub level: String,
    pub rotate_daily: bool,
    pub retain_days: u32,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("deps/logs"),
            level: "info".to_string(),
            rotate_daily: true,
            retain_days: 7,
        }
    }
}

impl LoggingConfig {
    pub(super) fn validate(&self) -> Result<()> {
        validate_nonempty_path(&self.dir, "logging.dir")?;
        if !matches!(
            self.level.trim().to_ascii_lowercase().as_str(),
            "error" | "warn" | "info" | "debug" | "trace"
        ) {
            bail!("logging.level 必须是 error/warn/info/debug/trace 之一");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TuiConfig {
    pub enabled: bool,
    pub refresh_ms: u64,
    pub log_lines: usize,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            refresh_ms: 100,
            log_lines: 200,
        }
    }
}

impl TuiConfig {
    pub(super) fn validate(&self) -> Result<()> {
        if self.enabled && (self.refresh_ms == 0 || self.log_lines == 0) {
            bail!("tui.refresh_ms 和 tui.log_lines 必须大于 0");
        }
        Ok(())
    }
}

/// UI 诊断存档：把界面识别时的画面落盘，便于事后复核「当时到底是黑屏、
/// 冻结帧，还是没建模的新界面」。
///
/// 与 `deps/logs/ui-failures`（失败帧）的分工：
/// - 失败帧：**始终开启**，只在例程失败那一刻写一张，是故障证据；
/// - 本段：默认关闭，按需打开，用来抓**过程帧**（频率更高）。
///   混在一起会让故障证据被海量过程帧淹没。
///
/// 只保留一个开关，不给目录/频率之类的参数：那些是内部实现细节，
/// 暴露到面板只会让用户困惑——配置应该只保留最基础的数据。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsConfig {
    /// 是否把 UI 过程帧落盘（固定写到 `<exe>/deps/diagnostics`）。
    ///
    /// 默认关闭：无人看管时会持续占磁盘。排查「界面持续无法识别」时开一段时间。
    #[serde(default)]
    pub ui_frames: bool,
}

impl DiagnosticsConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        // 纯布尔开关，没有跨字段约束。
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FriendDeliveryConfig {
    /// 已确认未发送消息的最大自动重试次数。
    pub auto_retry_count: u32,
}
