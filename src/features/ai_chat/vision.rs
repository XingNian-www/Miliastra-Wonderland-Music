//! AI 识图：配置、截图编码与「这一轮要不要带图」的判据。
//!
//! 模型是否支持图片只以 API 元数据为准，不根据模型名推断；查不到或请求侧拒绝时
//! 自动降级纯文本。图片只在内存中裁剪/缩放/编码，不落盘、不写入日志正文。

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView};
use serde::{Deserialize, Serialize};

use crate::config::ai::ImageAttachment;

use super::config::{AiChatConfig, default_true};

/// AI 聊天的视觉输入配置。
///
/// 默认开启，截图范围默认整个一级大厅；模型是否支持图片由 `/models` 返回的
/// 能力字段查询决定，不根据模型名推断；查询不到或请求侧拒绝时自动降级纯文本。
/// 图片只在内存中裁剪/缩放/编码，不落盘、不写入日志正文。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AiChatVisionConfig {
    /// 是否在 AI 聊天请求中附带当前一级界面截图。
    ///
    /// 结构体默认（新配置）为 true；但旧配置缺少整个 `vision` 段时由
    /// `default_vision_config_for_legacy()` 返回 false，避免“页面显示关闭、
    /// 运行时却默认开启”的错位。`vision: {}` 缺该字段也按 false 处理。
    #[serde(default)]
    pub enabled: bool,
    /// 识图调用方式：auto=每轮自动附图；tool=暴露 look 工具；both=两者。
    #[serde(default)]
    pub mode: VisionMode,
    /// 当前轮截图直接附给主聊天模型（仍需主模型支持图片）。
    /// 与 mode 独立：tool 模式下也可开启；只影响主模型输入，不改变 look 子请求。
    #[serde(default)]
    pub direct_to_main: bool,
    /// 截图范围；P0 只实现整个一级大厅。
    #[serde(default)]
    pub capture_scope: VisionCaptureScope,
    /// 长边缩放到该像素值再编码。
    #[serde(default = "default_vision_max_edge")]
    pub max_edge: u32,
    /// JPEG 编码质量（40..=100）。
    #[serde(default = "default_vision_jpeg_quality")]
    pub jpeg_quality: u8,
    /// 编码后允许的最大字节数；超过则降采样/降质量，仍超限则放弃图片。
    #[serde(default = "default_vision_max_bytes")]
    pub max_bytes: u64,
    /// 截图+编码的截止时间（毫秒）；超时降级纯文本。
    #[serde(default = "default_vision_timeout_ms")]
    pub timeout_ms: u64,
    /// 视觉专用模型；空字符串沿用 `ai_chat.model`。
    #[serde(default)]
    pub model: String,
    /// 是否允许整屏截图（会包含 UID/HUD，用户明确选择默认开启）。
    #[serde(default = "default_true")]
    pub allow_full_screen: bool,
}

/// 识图调用方式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisionMode {
    /// 每轮自动附一级大厅截图。
    #[default]
    Auto,
    /// 不自动附图，只暴露 `look` 工具。
    Tool,
    /// 自动附图 + `look` 工具。
    Both,
}

impl VisionMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动附图",
            Self::Tool => "工具调用",
            Self::Both => "自动附图+工具",
        }
    }
}

/// 视觉截图范围。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisionCaptureScope {
    /// 整个一级大厅（P0 默认）。
    #[default]
    PrimaryScreen,
}

impl Default for AiChatVisionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: VisionMode::default(),
            direct_to_main: false,
            capture_scope: VisionCaptureScope::default(),
            max_edge: default_vision_max_edge(),
            jpeg_quality: default_vision_jpeg_quality(),
            max_bytes: default_vision_max_bytes(),
            timeout_ms: default_vision_timeout_ms(),
            model: String::new(),
            allow_full_screen: true,
        }
    }
}

/// 是否本轮自动附截图。
pub(crate) fn auto_image_enabled(config: &AiChatConfig, supports_image: bool) -> bool {
    config.vision.enabled
        && supports_image
        && matches!(config.vision.mode, VisionMode::Auto | VisionMode::Both)
}

/// 是否暴露 `look` 工具（enabled=false 时关闭）。
pub(crate) fn look_tool_available(config: &AiChatConfig) -> bool {
    config.vision.enabled
        && config.vision.allow_full_screen
        && matches!(config.vision.mode, VisionMode::Tool | VisionMode::Both)
}

/// 单轮 look 配额：最多 2 次且同 instruction 不得重复。
pub(crate) fn look_quota_allows(
    looked: usize,
    seen: &std::collections::HashSet<String>,
    key: &str,
) -> bool {
    looked < 2 && !seen.contains(key)
}

/// 把一帧截图压缩成可随请求发送的内存 JPEG。
///
/// - 长边超过 `max_edge` 时等比缩放；
/// - 按 `jpeg_quality` 编码；超过 `max_bytes` 时降质量重试，仍超限返回 `None`；
/// - 返回的对象 `Debug` 不含字节，调用方也不得把 data URL 写日志。
pub(crate) fn encode_vision_image(
    image: &DynamicImage,
    max_edge: u32,
    jpeg_quality: u8,
    max_bytes: u64,
) -> Option<ImageAttachment> {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 || max_edge == 0 || max_bytes == 0 {
        return None;
    }
    let mut target = image.clone();
    if width.max(height) > max_edge {
        let scale = f64::from(max_edge) / f64::from(width.max(height));
        let new_width = (f64::from(width) * scale).round().max(1.0) as u32;
        let new_height = (f64::from(height) * scale).round().max(1.0) as u32;
        target = target.resize_exact(new_width, new_height, FilterType::Lanczos3);
    }
    let mut quality = jpeg_quality.clamp(40, 100);
    for attempt in 0..3 {
        let mut bytes = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut bytes, quality);
        if encoder.encode_image(&target).is_err() {
            return None;
        }
        if bytes.len() as u64 <= max_bytes {
            return Some(ImageAttachment::new_jpeg(
                bytes,
                target.width(),
                target.height(),
            ));
        }
        if attempt == 2 {
            return None;
        }
        quality = quality.saturating_sub(20).max(40);
    }
    None
}

/// 旧配置缺少整个 `vision` 段时的安全默认：关闭识图。
///
/// 新配置由 `AiChatVisionConfig::default()` 决定（enabled=true）；这里只处理
/// 从旧库/旧 JSON 反序列化时缺段的情况，避免 UI 显示关闭但运行时自动附图。
pub(super) fn default_vision_config_for_legacy() -> AiChatVisionConfig {
    let mut config = AiChatVisionConfig::default();
    config.enabled = false;
    config
}

pub(super) fn default_vision_max_edge() -> u32 {
    1_600
}

pub(super) fn default_vision_jpeg_quality() -> u8 {
    100
}

pub(super) fn default_vision_max_bytes() -> u64 {
    4_000_000
}

pub(super) fn default_vision_timeout_ms() -> u64 {
    800
}
