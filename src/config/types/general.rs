//! 基础与通用段配置：窗口、屏幕区域、稳定性、输出、状态文件、全局热键。
//!
//! 这些段是其它业务的坐标/时序底座：窗口决定坐标基准，屏幕区域决定截图切分，
//! 稳定性决定判定门槛，输出与热键决定怎么点、怎么停。

use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::paths::{validate_nonempty_path, validate_rect_in_canvas};

/// 几何值类型住在根级叶子 `geometry`（W0 搬迁）：`geometry` 被各层引用却不引用
/// 业务层，`config → geometry` 是向下的合法边。这里保留重导出，使
/// `crate::config::{RectConfig, PointConfig}` 的既有路径零变更。
pub(crate) use crate::geometry::{PointConfig, RectConfig};

const BUILTIN_STABILITY_COUNT: u32 = 2;
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StabilityConfig {
    pub default_count: u32,
    pub ui_state_count: u32,
    pub secondary_hall_count: u32,
}

impl Default for StabilityConfig {
    fn default() -> Self {
        Self {
            default_count: 2,
            ui_state_count: 0,
            secondary_hall_count: 0,
        }
    }
}

pub(crate) fn resolve_stability_count(local: u32, global: u32) -> u32 {
    if local > 1 {
        local
    } else if global > 1 {
        global
    } else {
        BUILTIN_STABILITY_COUNT
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowCaptureMethod {
    /// 从目标窗口客户区 DC 读取，适合通常的窗口化/无边框运行。
    #[default]
    Gdi,
    /// 请求目标窗口自行绘制客户区；窗口被遮挡时可能有效，但部分游戏不支持。
    PrintWindow,
    /// 从桌面 DC 裁剪目标窗口客户区；要求窗口实际可见且未被遮挡。
    Desktop,
    /// 使用 Windows Graphics Capture 捕获指定 HWND（Windows 10 1903+）。
    Wgc,
}

impl WindowCaptureMethod {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Gdi => "GDI 窗口 DC",
            Self::PrintWindow => "PrintWindow",
            Self::Desktop => "桌面 DC 裁剪",
            Self::Wgc => "Windows Graphics Capture",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowConfig {
    /// 支持逗号、分号、竖线或空白分隔的多个进程名。
    pub target_process: String,
    pub content_width: u32,
    pub content_height: u32,
    pub auto_activate_window: bool,
    pub focus_point: PointConfig,
    /// Windows 截图来源；缺失时维持旧版 GDI 行为。
    #[serde(default)]
    pub capture_method: WindowCaptureMethod,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            target_process: "yuanshen.exe,GenshinImpact.exe".to_string(),
            content_width: 1920,
            content_height: 1080,
            auto_activate_window: false,
            focus_point: PointConfig::new(1919, 1000),
            capture_method: WindowCaptureMethod::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenConfig {
    pub expected_width: u32,
    pub expected_height: u32,
    pub warn_on_size_mismatch: bool,
    pub chat_rect: RectConfig,
    pub friend_rect: RectConfig,
    pub secondary_back_rect: RectConfig,
    pub secondary_hall_rect: RectConfig,
    /// 二级聊天输入行右侧「发送」圆钮（深色圆底 + 黄色对勾）的搜索区。
    /// 它是**聊天专属锚点**：主菜单、主菜单大世界、千星多人面板、千星地图都没有这个按钮，
    /// 用来把只有左上圆形返回按钮的菜单/淡出过渡帧挡在 Secondary 之外。
    #[serde(default = "default_secondary_chat_submit_rect")]
    pub secondary_chat_submit_rect: RectConfig,
    /// 主菜单「返回提瓦特」按钮的搜索区。实测 (1690,1000) 205x60 可把主菜单
    /// 与其余 58 帧完全分开（见 technical-docs/ui-state-machine.md §35）。
    #[serde(default = "default_main_menu_rect")]
    pub main_menu_rect: RectConfig,
    /// 大厅底栏 👥 O 图标组的搜索区。
    #[serde(default = "default_hall_bar_rect")]
    pub hall_bar_rect: RectConfig,
    /// F2 千星大厅页右下「邀请玩家」按钮的搜索区。
    /// 左下「大厅列表」按钮复用 `hall_bar_rect`（两者都在底栏）；
    /// 只在这个窄区域里找右侧锚点，避免全帧或整条底栏匹配。
    #[serde(default = "default_qianxing_f2_invite_rect")]
    pub qianxing_f2_invite_rect: RectConfig,

    #[serde(default = "default_world_wish_rect")]
    pub world_wish_rect: RectConfig,
    pub hall_name_rect: RectConfig,
    pub hall_member_count_rect: RectConfig,
    pub hall_time_rect: RectConfig,
    pub hall_member_list_rect: RectConfig,
}

fn default_hall_bar_rect() -> RectConfig {
    // 底栏的横向位置会随聊天面板状态移动（实测同一模板在大厅帧上可出现在 x=180 或 x=369），
    // 所以搜索区必须够宽；过窄会把真实大厅帧漏掉（第 47 节踩过）。
    RectConfig {
        // 第 48 节改成"统计这段区域的近白像素占比"，而不是模板匹配。
        // 该量对位移和缩放都不敏感，正好绕开第 47 节栽的 1:1 SAD 限制。
        // 阈值就是在**这个区域**上量的，两边必须一致，不能随便改。
        x: 0,
        y: 985,
        width: 600,
        height: 95,
    }
}

fn default_qianxing_f2_invite_rect() -> RectConfig {
    // 「邀请玩家」纯文字模板（旧 canvas 240x50，现裁到字形 113x28）。
    // 28 帧（1920 与 1440p 归一化）实测命中左上角恒为 (1662,1003)，
    // 因此区域只保留 12px 定位余量，不再覆盖整条底栏右侧；原点与旧区域同奇偶。
    RectConfig {
        x: 1650,
        y: 993,
        width: 140,
        height: 54,
    }
}
fn default_main_menu_rect() -> RectConfig {
    // 按「返回提瓦特」整串文字外扩约 8px：模板本身只有这串字（137x25，不含定位图标），
    // 搜索区必须能容纳整块模板，否则匹配器直接返回空、主菜单又会漏判。
    // 上边界避开左侧图标，下边界停在 UID 行之前。
    RectConfig {
        x: 1668,
        y: 998,
        width: 153,
        height: 44,
    }
}

fn default_secondary_chat_submit_rect() -> RectConfig {
    // 模板是 52×52 的发送圆钮（深色圆底 + 黄色对勾）。实测在 round78-live.png /
    // secondary-full.png / sec-fixture.jpg 上命中位置恒为 (958,984)（三帧完全一致），
    // 因此搜索区只需 ±24px 余量；区域不能贴到画面底边（y+height ≤ 1080）。
    RectConfig {
        x: 930,
        y: 950,
        width: 150,
        height: 124,
    }
}

fn default_world_wish_rect() -> RectConfig {
    RectConfig {
        x: 1300,
        y: 0,
        width: 620,
        height: 110,
    }
}

impl Default for ScreenConfig {
    fn default() -> Self {
        // 默认值与 WindowConfig 的 1920x1080 一致，区域沿用完整配置模板
        // （src/config/test.full.yaml）的坐标，保证 AppConfig::default()
        // 能通过 validate() 的尺寸一致性和画布内校验。
        Self {
            expected_width: 1920,
            expected_height: 1080,
            warn_on_size_mismatch: true,
            chat_rect: RectConfig {
                x: 39,
                y: 879,
                width: 416,
                height: 143,
            },
            friend_rect: RectConfig {
                x: 170,
                y: 1018,
                width: 50,
                height: 40,
            },
            secondary_back_rect: RectConfig {
                x: 15,
                y: 15,
                width: 65,
                height: 65,
            },
            secondary_hall_rect: RectConfig {
                x: 10,
                y: 190,
                width: 65,
                height: 55,
            },
            secondary_chat_submit_rect: default_secondary_chat_submit_rect(),
            main_menu_rect: default_main_menu_rect(),
            hall_bar_rect: default_hall_bar_rect(),
            qianxing_f2_invite_rect: default_qianxing_f2_invite_rect(),
            world_wish_rect: default_world_wish_rect(),
            hall_name_rect: RectConfig {
                x: 75,
                y: 425,
                width: 325,
                height: 40,
            },
            hall_member_count_rect: RectConfig {
                x: 75,
                y: 470,
                width: 450,
                height: 50,
            },
            hall_time_rect: RectConfig {
                x: 430,
                y: 520,
                width: 110,
                height: 40,
            },
            hall_member_list_rect: RectConfig {
                x: 1280,
                y: 110,
                width: 560,
                height: 850,
            },
        }
    }
}

impl ScreenConfig {
    pub(super) fn validate(&self) -> Result<()> {
        for (rect, field) in [
            (self.chat_rect, "screen.chat_rect"),
            (self.friend_rect, "screen.friend_rect"),
            (self.secondary_back_rect, "screen.secondary_back_rect"),
            (self.secondary_hall_rect, "screen.secondary_hall_rect"),
            (
                self.secondary_chat_submit_rect,
                "screen.secondary_chat_submit_rect",
            ),
            (self.main_menu_rect, "screen.main_menu_rect"),
            (self.hall_bar_rect, "screen.hall_bar_rect"),
            (
                self.qianxing_f2_invite_rect,
                "screen.qianxing_f2_invite_rect",
            ),
            (self.world_wish_rect, "screen.world_wish_rect"),
            (self.hall_name_rect, "screen.hall_name_rect"),
            (self.hall_member_count_rect, "screen.hall_member_count_rect"),
            (self.hall_time_rect, "screen.hall_time_rect"),
            (self.hall_member_list_rect, "screen.hall_member_list_rect"),
        ] {
            validate_rect_in_canvas(rect, field, (self.expected_width, self.expected_height))?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputConfig {
    pub send_enabled: bool,
    pub focus_point: PointConfig,
    pub chat_click_2: PointConfig,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            send_enabled: true,
            focus_point: PointConfig::new(1919, 1000),
            chat_click_2: PointConfig::new(600, 1013),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateConfig {
    /// 统一数据库路径；由启动引导（BootstrapConfig.database_path）注入，
    /// 配置库 JSON 中不持久化，缺失时用空路径占位（注入前不参与校验）。
    #[serde(default)]
    pub playback_state_path: PathBuf,
    pub hall_state_path: PathBuf,
    pub executed_commands_log_path: PathBuf,
}

impl Default for StateConfig {
    fn default() -> Self {
        Self {
            playback_state_path: PathBuf::from("deps/data/playback.sqlite3"),
            hall_state_path: PathBuf::from("deps/data/hall-state.json"),
            executed_commands_log_path: PathBuf::from("deps/data/executed-commands.log"),
        }
    }
}

impl StateConfig {
    pub(super) fn validate(&self) -> Result<()> {
        for (path, field) in [
            (&self.playback_state_path, "state.playback_state_path"),
            (&self.hall_state_path, "state.hall_state_path"),
            (
                &self.executed_commands_log_path,
                "state.executed_commands_log_path",
            ),
        ] {
            validate_nonempty_path(path, field)?;
        }
        if self
            .playback_state_path
            .file_name()
            .and_then(|name| name.to_str())
            != Some("playback.sqlite3")
        {
            bail!("state.playback_state_path 文件名必须是 playback.sqlite3");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HotkeyConfig {
    pub enabled: bool,
    pub pause_key: String,
    pub exit_key: String,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            pause_key: "F7".to_string(),
            exit_key: "F12".to_string(),
        }
    }
}

impl HotkeyConfig {
    pub(super) fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.pause_key.trim().is_empty() || self.exit_key.trim().is_empty() {
            bail!("hotkeys.pause_key 和 hotkeys.exit_key 不能为空");
        }
        if self.pause_key.eq_ignore_ascii_case(&self.exit_key) {
            bail!("hotkeys.pause_key 和 hotkeys.exit_key 不能相同");
        }
        for (value, field) in [
            (&self.pause_key, "hotkeys.pause_key"),
            (&self.exit_key, "hotkeys.exit_key"),
        ] {
            parse_hotkey_virtual_code(value)
                .map_err(|_| anyhow::anyhow!("{field} 仅支持 F1-F12 或单个 ASCII 字符"))?;
        }
        Ok(())
    }
}

pub(crate) fn parse_hotkey_virtual_code(key: &str) -> Result<u16> {
    let normalized = key.trim().to_ascii_uppercase();
    let code = match normalized.as_str() {
        "F1" => 0x70,
        "F2" => 0x71,
        "F3" => 0x72,
        "F4" => 0x73,
        "F5" => 0x74,
        "F6" => 0x75,
        "F7" => 0x76,
        "F8" => 0x77,
        "F9" => 0x78,
        "F10" => 0x79,
        "F11" => 0x7A,
        "F12" => 0x7B,
        single if single.len() == 1 && single.is_ascii() => single.as_bytes()[0] as u16,
        _ => bail!("不支持的热键: {key}"),
    };
    Ok(code)
}
