//! 模板文件加载与灰度 / 降采样缓存。

use anyhow::Result;
use anyhow::anyhow;
use image::DynamicImage;
use image::GrayImage;
use image::RgbImage;
use image::imageops::FilterType;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use super::cpu::{OpaqueRect, largest_opaque_rect, sum_gray_rect};
use anyhow::Context;

pub(super) static RGB_TEMPLATE_CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<RgbImage>>>> =
    OnceLock::new();
pub(super) static GRAY_TEMPLATE_CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<GrayTemplate>>>> =
    OnceLock::new();
/// 按整数倍降采样后的模板，键为 `(模板路径, 降采样倍数)`。
pub(super) static DOWNSCALED_GRAY_TEMPLATE_CACHE: OnceLock<
    Mutex<HashMap<(PathBuf, u32), Arc<GrayTemplate>>>,
> = OnceLock::new();

/// 灰度标准差低于该值的模板视为"纯色/近纯色"：对 SAD 几乎没有区分度。
///
/// 实测 16x16 纯色模板在 256x256 区域里，97.7% 的位置 score >= 0.80、31% >= 0.95，
/// 也就是说 min_score=0.80 时它几乎"匹配一切均匀区域"。
pub(super) const FLAT_TEMPLATE_STDDEV: f32 = 4.0;

/// 透明模板的“平坦画面”防护。
///
/// 透明化会去掉与内容形成对比的底色；只剩白字/白图标时，纯色或大面积均匀区域上
/// SAD 会接近 0，而分数又只除以 Σalpha，按 `1 - MAD/255` 就能拿到高分（实测
/// `ui-secondary-hall` 0.944、`invite-enter-hall` 0.911、`confirm-button` 0.909，
/// 均超过 0.90 阈值）。这里要求命中位置在模板 alpha 外接框内的画面标准差不得低于
/// `max(2.0, 25% × 模板外接框标准差)`：纯色画面标准差为 0，直接拒绝；真实 UI
/// 的文字/图标会把画面标准差撑起来。只对带 alpha 的模板生效。
pub(super) const FLAT_PATCH_STDDEV_FLOOR: f32 = 2.0;
pub(super) const FLAT_PATCH_STDDEV_RATIO: f32 = 0.25;

/// 文字内容模板的最低对比度（前景中位灰度与背景中位灰度之差）。
///
/// 透明化后模板只剩字形，匹配分数不再被按钮底色/胶囊底撑高；但 SAD 只除以
/// Σalpha，纯色画面仍可能拿到高分。这里用**极性无关**的灰度对比度判定：
/// 深色字（暗前景/亮背景）与浅色字（亮前景/暗背景）都取绝对值。
pub(super) const TEXT_ANCHOR_MIN_CONTRAST: f32 = 40.0;
/// 字形像素中，与背景灰度差 ≥ [`TEXT_ANCHOR_INK_DELTA`] 的比例下限。
///
/// 纯色/近纯色 patch 的 contrast 已经接近 0；ink 进一步挡掉“对比度高但只有
/// 少数噪点像字”的误命中。std/edge 只记录不设硬门槛，避免误杀浅色小字。
pub(super) const TEXT_ANCHOR_MIN_INK: f64 = 0.5;
pub(super) const TEXT_ANCHOR_INK_DELTA: u8 = 30;

/// 以**文字内容**为判据的模板文件名。
///
/// 这份清单是唯一的归类点：匹配器按文件名统一走
/// [`text_anchor_quality_ok`]，调用方不需要逐个模板重复校验。
/// 图标类模板（back/hall/submit/chat markers/friend-panel 图标/地图形状）
/// 不在此列，保持原有 SAD 语义。
pub(super) const TEXT_TEMPLATE_FILE_NAMES: &[&str] = &[
    "ui-main-menu-return.png",
    "invite-enter-hall.png",
    "invite-goto-hall.png",
    "invite-view-star.png",
    "invite-view-profile.png",
    "teyvat-profile-add-friend.png",
    "friend-block-chat.png",
    "friend-blacklist.png",
    "confirm-button.png",
    "friend-search-panel.png",
    "confirm-message-random-hall.png",
    "confirm-message-return-teyvat.png",
    "confirm-message-block.png",
    "qianxing-f2-hall-list.png",
    "qianxing-f2-invite-player.png",
    "qianxing-exit-menu.png",
    "teyvat-profile-adventure-rank.png",
    "teyvat-profile-world-level.png",
    "qianxing-profile-domain-level.png",
    "qianxing-profile-add-friend.png",
    "qianxing-profile-page-wishlist.png",
    "qianxing-profile-page-wardrobe.png",
];

/// 该模板是否以文字内容为判据。
pub(crate) fn is_text_template(template_path: &Path) -> bool {
    template_path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| TEXT_TEMPLATE_FILE_NAMES.contains(&name))
}

/// 已就近纯色告警过的模板路径。
///
/// 模板缺陷是静态属性，只需提示一次；用集合而非 `OnceLock<bool>` 是因为
/// 不同模板各自独立，一份坏模板不应吞掉另一份的告警。
pub(super) static FLAT_TEMPLATE_WARNED: OnceLock<Mutex<std::collections::HashSet<PathBuf>>> =
    OnceLock::new();

/// 该模板是否**首次**被判定为近纯色。首次返回 true，之后恒为 false。
pub(super) fn warn_flat_template_once(label: &Path) -> bool {
    let warned = FLAT_TEMPLATE_WARNED.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    let mut warned = warned
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    warned.insert(label.to_path_buf())
}

/// 灰度标准差：0 表示纯色。用来衡量模板对 SAD 的区分度。
pub(super) fn gray_stddev(gray: &GrayImage) -> f32 {
    let values = gray.as_raw();
    if values.is_empty() {
        return 0.0;
    }
    let count = values.len() as f64;
    let mean = values.iter().map(|value| *value as f64).sum::<f64>() / count;
    let variance = values
        .iter()
        .map(|value| {
            let delta = *value as f64 - mean;
            delta * delta
        })
        .sum::<f64>()
        / count;
    variance.sqrt() as f32
}

pub(super) struct GrayTemplate {
    pub(super) gray: GrayImage,
    pub(super) alpha: GrayImage,
    pub(super) alpha_sum: u64,
    /// alpha 全 255 时可以走纯 SAD 快路径。
    pub(super) fully_opaque: bool,
    /// 预计算的最大全不透明矩形，用于 SEA 下界剪枝。
    pub(super) opaque: Option<OpaqueRect>,
    /// 灰度标准差，用于识别缺乏区分度的纯色模板。
    #[allow(dead_code)]
    pub(super) gray_stddev: f32,
    /// alpha 外接框与框内灰度标准差；透明模板的平坦画面防护用。
    pub(super) contrast: Option<ContrastBox>,
    /// 是否启用平坦画面防护。`world-wish` / `startup-wonderland-map-star` 是既有的
    /// 透明信号图（本轮明确不动它们），保持原行为。
    pub(super) guard_flat: bool,
}

/// alpha 外接框（含透明像素的原始 RGB）+ 框内灰度标准差。
#[derive(Clone, Copy)]
pub(super) struct ContrastBox {
    pub(super) x: u32,
    pub(super) y: u32,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) stddev: f32,
}

pub(super) fn cached_rgb_template(template_path: &Path) -> Result<Arc<RgbImage>> {
    let cache = RGB_TEMPLATE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .map_err(|_| anyhow!("template cache mutex poisoned"))?;
    if let Some(image) = cache.get(template_path) {
        return Ok(Arc::clone(image));
    }
    let image = image::open(template_path)
        .with_context(|| format!("open template {}", template_path.display()))?
        .to_rgb8();
    let image = Arc::new(image);
    cache.insert(template_path.to_path_buf(), Arc::clone(&image));
    Ok(image)
}

impl GrayTemplate {
    /// 由灰度平面 + alpha 平面构造；`label` 只用于日志。
    /// 降采样版本也走这里，因此掩码/下界/纯色告警的口径与全分辨率模板完全一致。
    pub(super) fn build(gray: GrayImage, alpha: GrayImage, label: &Path) -> Self {
        // 透明 PNG 边缘不能与实时画面比较；这些像素只是占位，不属于视觉锚点。
        let alpha_sum = alpha.as_raw().iter().map(|value| *value as u64).sum();
        let pixel_count = gray.width() as u64 * gray.height() as u64;
        let fully_opaque = pixel_count > 0 && alpha_sum == pixel_count * 255;
        // SEA 下界只在一块“权重恒为 1”的矩形上成立，因此预先算出模板内最大的全不透明矩形，
        // 之后每次匹配都能零成本地拿到下界。
        let opaque = if fully_opaque {
            Some(OpaqueRect {
                x: 0,
                y: 0,
                width: gray.width(),
                height: gray.height(),
                gray_sum: gray.as_raw().iter().map(|value| *value as u64).sum(),
            })
        } else {
            largest_opaque_rect(&alpha).map(|rect| OpaqueRect {
                gray_sum: sum_gray_rect(&gray, rect.x, rect.y, rect.width, rect.height),
                ..rect
            })
        };
        let gray_stddev = gray_stddev(&gray);
        let contrast = template_contrast(&gray, &alpha);
        let file_name = label
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let guard_flat = !matches!(
            file_name,
            "world-wish.png" | "startup-wonderland-map-star.png"
        );
        if gray_stddev < FLAT_TEMPLATE_STDDEV {
            // 同一模板会被 build 两次（全分辨率与降采样版本），而模板有缺陷这件事
            // 是静态的：每帧重复告警只会把真正有用的日志挤出文件。
            // 按模板路径去重，同一份模板在整个进程生命周期内只提示一次。
            if warn_flat_template_once(label) {
                log::warn!(
                    "模板「{}」的灰度标准差仅 {:.2}，属近纯色，SAD 区分度极低，容易在大面积均匀区域误命中；建议改用多点签名断言",
                    label.display(),
                    gray_stddev
                );
            }
        }
        Self {
            gray,
            alpha,
            alpha_sum,
            fully_opaque,
            gray_stddev,
            opaque,
            contrast,
            guard_flat,
        }
    }
}

/// 模板 alpha 外接框及框内（含透明像素原始 RGB）的灰度标准差。
pub(super) fn template_contrast(gray: &GrayImage, alpha: &GrayImage) -> Option<ContrastBox> {
    let (width, height) = gray.dimensions();
    let alpha_data = alpha.as_raw();
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0, 0);
    for y in 0..height {
        for x in 0..width {
            if alpha_data[(y * width + x) as usize] == 0 {
                continue;
            }
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    if min_x == u32::MAX {
        return None;
    }
    let (box_width, box_height) = (max_x - min_x + 1, max_y - min_y + 1);
    let gray_data = gray.as_raw();
    let (mut sum, mut sum_sq) = (0u64, 0u64);
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let value = gray_data[(y * width + x) as usize] as u64;
            sum += value;
            sum_sq += value * value;
        }
    }
    let count = (box_width * box_height) as f64;
    let mean = sum as f64 / count;
    let variance = (sum_sq as f64 / count - mean * mean).max(0.0);
    Some(ContrastBox {
        x: min_x,
        y: min_y,
        width: box_width,
        height: box_height,
        stddev: variance.sqrt() as f32,
    })
}

pub(super) fn cached_gray_template(template_path: &Path) -> Result<Arc<GrayTemplate>> {
    let cache = GRAY_TEMPLATE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .map_err(|_| anyhow!("template cache mutex poisoned"))?;
    if let Some(template) = cache.get(template_path) {
        return Ok(Arc::clone(template));
    }
    let rgba = image::open(template_path)
        .with_context(|| format!("open template {}", template_path.display()))?
        .to_rgba8();
    let gray = DynamicImage::ImageRgba8(rgba.clone()).to_luma8();
    let alpha = GrayImage::from_raw(
        rgba.width(),
        rgba.height(),
        rgba.pixels().map(|pixel| pixel[3]).collect(),
    )
    .expect("RGBA alpha plane must have the same dimensions");
    let template = Arc::new(GrayTemplate::build(gray, alpha, template_path));
    cache.insert(template_path.to_path_buf(), Arc::clone(&template));
    Ok(template)
}

/// 模板的降采样版本（缓存）。灰度与 alpha 用**与画面相同的滤波器**缩小，
/// 保证两侧口径一致——否则同一内容在两边会落到不同的像素上，分数会无谓地掉。
pub(super) fn cached_downscaled_gray_template(
    template_path: &Path,
    scale: u32,
) -> Result<Arc<GrayTemplate>> {
    let scale = scale.max(1);
    let cache = DOWNSCALED_GRAY_TEMPLATE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .map_err(|_| anyhow!("template cache mutex poisoned"))?;
    let key = (template_path.to_path_buf(), scale);
    if let Some(template) = cache.get(&key) {
        return Ok(Arc::clone(template));
    }
    let base = cached_gray_template(template_path)?;
    let width = (base.gray.width() / scale).max(1);
    let height = (base.gray.height() / scale).max(1);
    let gray = DynamicImage::ImageLuma8(base.gray.clone())
        .resize_exact(width, height, FilterType::Triangle)
        .to_luma8();
    let alpha = DynamicImage::ImageLuma8(base.alpha.clone())
        .resize_exact(width, height, FilterType::Triangle)
        .to_luma8();
    let template = Arc::new(GrayTemplate::build(gray, alpha, template_path));
    cache.insert(key, Arc::clone(&template));
    Ok(template)
}
