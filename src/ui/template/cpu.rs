//! CPU 模板匹配：SEA 下界剪枝、单点 SAD、平坦画面防护与文字质量校验。

use crate::geometry::Rect;
use crate::geometry::TemplateHit;
use crate::geometry::crop_canvas;
use image::DynamicImage;
use image::GrayImage;
use image::RgbImage;
use image::imageops::FilterType;
use std::cmp::Ordering;
use std::path::Path;

use super::cache::{
    ContrastBox, FLAT_PATCH_STDDEV_FLOOR, FLAT_PATCH_STDDEV_RATIO, GrayTemplate,
    TEXT_ANCHOR_INK_DELTA, TEXT_ANCHOR_MIN_CONTRAST, TEXT_ANCHOR_MIN_INK, cached_gray_template,
    is_text_template,
};
use super::gpu::gpu_best_position;

/// 一行 RGB 的绝对差之和。
///
/// 这是聊天标记扫描的热点：实测逐字节标量循环只有约 1.8 GB/s。
/// x86_64 上用 SSE2 的 `psadbw`，一条指令算完 16 字节的绝对差求和。
#[cfg(target_arch = "x86_64")]
#[inline]
pub(super) fn row_abs_diff_sum(haystack_row: &[u8], template_row: &[u8]) -> u64 {
    use std::arch::x86_64::{
        __m128i, _mm_cvtsi128_si64, _mm_loadu_si128, _mm_sad_epu8, _mm_srli_si128,
    };

    let length = haystack_row.len().min(template_row.len());
    let mut total = 0u64;
    let mut offset = 0usize;
    // SAFETY: SSE2 是 x86_64 的基线指令集，不需要运行时检测。
    // `_mm_loadu_si128` 支持未对齐读取；循环条件保证 [offset, offset+16) 同时落在两个切片内。
    // `psadbw` 输出 8 个 16 位部分和，每半区最多 4×8×255 = 8160，转 u64 不会溢出或变号。
    unsafe {
        while offset + 16 <= length {
            let left = _mm_loadu_si128(haystack_row.as_ptr().add(offset).cast::<__m128i>());
            let right = _mm_loadu_si128(template_row.as_ptr().add(offset).cast::<__m128i>());
            let packed = _mm_sad_epu8(left, right);
            total += _mm_cvtsi128_si64(packed) as u64
                + _mm_cvtsi128_si64(_mm_srli_si128(packed, 8)) as u64;
            offset += 16;
        }
    }
    for index in offset..length {
        total += haystack_row[index].abs_diff(template_row[index]) as u64;
    }
    total
}

/// 非 x86_64 的可移植回退：结果与 SIMD 路径逐位一致。
#[cfg(not(target_arch = "x86_64"))]
#[inline]
pub(super) fn row_abs_diff_sum(haystack_row: &[u8], template_row: &[u8]) -> u64 {
    let length = haystack_row.len().min(template_row.len());
    haystack_row[..length]
        .iter()
        .zip(&template_row[..length])
        .map(|(left, right)| left.abs_diff(*right) as u64)
        .sum()
}

pub(super) fn color_sad_at(
    haystack: &RgbImage,
    template: &RgbImage,
    x: u32,
    y: u32,
    max_allowed_sad: u64,
) -> u64 {
    let haystack_width = haystack.width() as usize;
    let template_width = template.width() as usize;
    let template_height = template.height() as usize;
    let x = x as usize;
    let y = y as usize;
    let haystack_data = haystack.as_raw();
    let template_data = template.as_raw();
    let mut sad = 0u64;

    let row_bytes = template_width * 3;
    for row in 0..template_height {
        let haystack_offset = ((y + row) * haystack_width + x) * 3;
        let template_offset = row * row_bytes;
        // 整行一次算完再检查：提前退出从逐字节降到行边界，接受/拒绝判定与逐字节退出完全等价
        // （被拒绝的候选两种实现都返回某个大于 max_allowed_sad 的部分和，通过的都是完整 SAD），
        // 换来的是一条指令处理 16 字节。
        sad += row_abs_diff_sum(
            &haystack_data[haystack_offset..haystack_offset + row_bytes],
            &template_data[template_offset..template_offset + row_bytes],
        );
        if sad > max_allowed_sad {
            return sad;
        }
    }
    sad
}

/// 一块“模板与画面逐像素比较、权重恒为 1”的矩形，用于 SEA 下界。
#[derive(Clone, Copy)]
pub(super) struct OpaqueRect {
    pub(super) x: u32,
    pub(super) y: u32,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) gray_sum: u64,
}

/// 统一的 CPU 模板匹配器：模板灰度 + 可选 alpha 掩码。
///
/// 所有匹配路径（UI 状态分类、各 routine、`/tools/template`、沙盒 Template 断言）都经过这里，
/// 不再有每次调用重建 GPU 设备的固定开销。结果与逐像素朴素扫描逐位一致：只用数学上成立的
/// 下界剪掉不可能获胜的位置，不改变任何分数。
pub(crate) struct GrayMatcher<'a> {
    pub(super) gray: &'a GrayImage,
    /// `None` 表示模板完全不透明，可以走纯 SAD 快路径。
    pub(super) alpha: Option<&'a GrayImage>,
    pub(super) alpha_sum: u64,
    pub(super) opaque: Option<OpaqueRect>,
    /// 透明模板的平坦画面防护；不透明/豁免模板为 None。
    pub(super) contrast: Option<ContrastBox>,
}

impl<'a> GrayMatcher<'a> {
    /// 完全不透明模板（没有 alpha 掩码，例如沙盒的内联灰度断言）。
    pub(crate) fn opaque(gray: &'a GrayImage) -> Self {
        let gray_sum = gray.as_raw().iter().map(|value| *value as u64).sum();
        Self {
            gray,
            alpha: None,
            alpha_sum: gray.width() as u64 * gray.height() as u64 * 255,
            opaque: Some(OpaqueRect {
                x: 0,
                y: 0,
                width: gray.width(),
                height: gray.height(),
                gray_sum,
            }),
            contrast: None,
        }
    }

    pub(super) fn from_template(template: &'a GrayTemplate) -> Self {
        Self {
            gray: &template.gray,
            alpha: if template.fully_opaque {
                None
            } else {
                Some(&template.alpha)
            },
            alpha_sum: template.alpha_sum,
            opaque: template.opaque,
            contrast: if template.fully_opaque || !template.guard_flat {
                None
            } else {
                template.contrast
            },
        }
    }

    /// Σalpha。分数 = 1 - sad / alpha_sum，即 1 - MAD/255。
    pub(crate) fn alpha_sum(&self) -> u64 {
        self.alpha_sum
    }
}

/// 命中位置是否具备足够的画面结构。
///
/// 只对带 alpha 的模板生效；不透明模板保持原有语义。取模板 alpha 外接框内的
/// 画面灰度标准差，纯色画面为 0，必然被拒绝。
pub(super) fn patch_contrast_ok(
    haystack: &GrayImage,
    matcher: &GrayMatcher<'_>,
    x: u32,
    y: u32,
) -> bool {
    if matcher.alpha.is_none() {
        return true;
    }
    let Some(contrast) = matcher.contrast else {
        return true;
    };
    // 退化模板（1×1/2×2 的沙盒断言）没有结构可要求，保持原语义。
    if contrast.width * contrast.height < 4 {
        return true;
    }
    if x + contrast.x + contrast.width > haystack.width()
        || y + contrast.y + contrast.height > haystack.height()
    {
        return false;
    }
    let stride = haystack.width() as usize;
    let data = haystack.as_raw();
    let (mut sum, mut sum_sq) = (0u64, 0u64);
    for row in 0..contrast.height {
        let offset = (y + contrast.y + row) as usize * stride + (x + contrast.x) as usize;
        for column in 0..contrast.width {
            let value = data[offset + column as usize] as u64;
            sum += value;
            sum_sq += value * value;
        }
    }
    let count = (contrast.width * contrast.height) as f64;
    let mean = sum as f64 / count;
    let variance = (sum_sq as f64 / count - mean * mean).max(0.0);
    let required = (contrast.stddev * FLAT_PATCH_STDDEV_RATIO).max(FLAT_PATCH_STDDEV_FLOOR);
    variance.sqrt() as f32 >= required
}

/// 文字质量测量结果。std/edge 只用于排障日志，不参与判定。
#[derive(Clone, Copy, Debug)]
pub(super) struct TextAnchorMetrics {
    pub(super) contrast: f32,
    pub(super) ink: f64,
    pub(super) stddev: f32,
    pub(super) edge: f32,
}

/// 文字内容模板的命中质量校验（极性无关）。
///
/// `image` 是整帧，`hit` 是匹配器给出的命中矩形（绝对坐标），`template` 是模板原图。
/// 字形 mask **只由 alpha 决定**：
/// - 带 alpha 的模板：alpha > 0 即字形；
/// - 完全不透明的模板：用 patch 边框的中位灰度估计背景，再把与背景差
///   ≥ [`TEXT_ANCHOR_INK_DELTA`] 的像素当作字形。
///
/// 模板灰度刻意不参与：SAD 匹配对模板做反色等值变换后分数完全相同，拿模板灰度
/// 去挑字形会把"白字模板匹配到黑字按钮"这类真实命中判成失败。
///
/// 判定只用 contrast/ink：`contrast >= 40` 且 `ink >= 0.5`。纯色 patch 的对比度为 0，
/// 直接返回 false；std/edge 仅作为辅助信号记录，不设硬门槛。
pub(crate) fn text_anchor_quality_ok(
    image: &DynamicImage,
    hit: Rect,
    template: &DynamicImage,
) -> bool {
    let rgba = template.to_rgba8();
    let alpha = GrayImage::from_raw(
        rgba.width(),
        rgba.height(),
        rgba.pixels().map(|pixel| pixel[3]).collect(),
    )
    .unwrap_or_else(|| GrayImage::new(rgba.width(), rgba.height()));
    text_anchor_quality_ok_gray(image, hit, &alpha).is_some()
}

/// 已缓存模板的入口：调用方不必为每帧重新解码模板。
pub(super) fn text_anchor_quality_ok_template(
    image: &DynamicImage,
    hit: Rect,
    template_path: &Path,
) -> bool {
    if !is_text_template(template_path) {
        return true;
    }
    let Ok(template) = cached_gray_template(template_path) else {
        return false;
    };
    text_anchor_quality_ok_gray(image, hit, &template.alpha).is_some()
}

pub(super) fn text_anchor_quality_ok_gray(
    image: &DynamicImage,
    hit: Rect,
    template_alpha: &GrayImage,
) -> Option<TextAnchorMetrics> {
    let metrics = text_anchor_metrics(image, hit, template_alpha)?;
    if metrics.contrast < TEXT_ANCHOR_MIN_CONTRAST || metrics.ink < TEXT_ANCHOR_MIN_INK {
        log::trace!(
            "文字质量校验拒绝：命中 {}×{} @({}, {})，对比度 {:.1}（要求不低于 {:.0}），字形覆盖率 {:.2}（要求不低于 {:.2}），画面标准差 {:.1}，水平边缘 {:.1}",
            hit.width,
            hit.height,
            hit.x,
            hit.y,
            metrics.contrast,
            TEXT_ANCHOR_MIN_CONTRAST,
            metrics.ink,
            TEXT_ANCHOR_MIN_INK,
            metrics.stddev,
            metrics.edge,
        );
        return None;
    }
    Some(metrics)
}

pub(super) fn text_anchor_metrics(
    image: &DynamicImage,
    hit: Rect,
    template_alpha: &GrayImage,
) -> Option<TextAnchorMetrics> {
    if hit.width == 0 || hit.height == 0 || hit.x < 0 || hit.y < 0 {
        return None;
    }
    let patch = crop_canvas(image, hit).ok()?.to_luma8();
    let (patch_width, patch_height) = patch.dimensions();
    if patch_width == 0 || patch_height == 0 {
        return None;
    }
    let patch_data = patch.as_raw();
    let glyph_mask: Vec<bool> = if template_alpha.as_raw().iter().all(|alpha| *alpha == 255) {
        // 不透明模板：用 patch 边框的中位灰度当背景，再按差值估字形。
        let background = border_median_gray(&patch);
        patch_data
            .iter()
            .map(|value| value.abs_diff(background) >= TEXT_ANCHOR_INK_DELTA)
            .collect()
    } else {
        let resized = if template_alpha.dimensions() == (patch_width, patch_height) {
            std::borrow::Cow::Borrowed(template_alpha)
        } else {
            std::borrow::Cow::Owned(image::imageops::resize(
                template_alpha,
                patch_width,
                patch_height,
                FilterType::Triangle,
            ))
        };
        // 同尺寸时 alpha > 0 即字形；缩放后把滤波产生的极低值当透明，避免把
        // 整块 patch 都算进前景。
        let threshold = if resized.dimensions() == template_alpha.dimensions() {
            0
        } else {
            16
        };
        resized
            .as_raw()
            .iter()
            .map(|alpha| *alpha > threshold)
            .collect()
    };

    let mut glyph_values = Vec::new();
    for (index, is_glyph) in glyph_mask.iter().enumerate() {
        if *is_glyph {
            glyph_values.push(patch_data[index]);
        }
    }
    if glyph_values.len() < 8 {
        return None;
    }
    // 背景取**字形邻域**的非字形像素，而不是整块 hit 的中位数：
    // 文字模板虽然已经裁到字形 bbox，框内仍有透明像素，命中 patch 也可能跨到
    // 卡片/菜单底色；整块中位数会被这些非字形灰度带偏。
    // 邻域半径 4px 足够覆盖抗锯齿边缘，又不会跨到别的按钮。
    let background = if let Some(mut values) = glyph_neighborhood_background(&patch, &glyph_mask, 4)
    {
        median_u8(&mut values)
    } else {
        border_median_gray(&patch)
    };
    let foreground = median_u8(&mut glyph_values);

    let contrast = foreground.abs_diff(background) as f32;
    let ink = glyph_mask
        .iter()
        .enumerate()
        .filter(|(index, is_glyph)| {
            **is_glyph && patch_data[*index].abs_diff(background) >= TEXT_ANCHOR_INK_DELTA
        })
        .count() as f64
        / glyph_mask.iter().filter(|is_glyph| **is_glyph).count() as f64;

    let (stddev, edge) = patch_gray_stddev_and_edge(&patch);
    Some(TextAnchorMetrics {
        contrast,
        ink,
        stddev,
        edge,
    })
}

/// 字形邻域内的非字形像素灰度（半径 `radius`），用于估计局部背景。
///
/// 用积分图统计每个像素周围是否有字形，复杂度 O(width×height)，避免逐像素扫描
/// 半径窗口。没有邻域背景（例如字形铺满整块）时返回 `None`。
pub(super) fn glyph_neighborhood_background(
    patch: &GrayImage,
    glyph_mask: &[bool],
    radius: u32,
) -> Option<Vec<u8>> {
    let (width, height) = patch.dimensions();
    let stride = width as usize + 1;
    let mut integral = vec![0u32; stride * (height as usize + 1)];
    for y in 0..height as usize {
        let mut row_sum = 0u32;
        for x in 0..width as usize {
            row_sum += u32::from(glyph_mask[y * width as usize + x]);
            integral[(y + 1) * stride + x + 1] = integral[y * stride + x + 1] + row_sum;
        }
    }
    let box_sum = |x0: usize, y0: usize, x1: usize, y1: usize| -> u32 {
        integral[y1 * stride + x1] + integral[y0 * stride + x0]
            - integral[y0 * stride + x1]
            - integral[y1 * stride + x0]
    };
    let mut values = Vec::new();
    for y in 0..height as usize {
        for x in 0..width as usize {
            if glyph_mask[y * width as usize + x] {
                continue;
            }
            let x0 = x.saturating_sub(radius as usize);
            let y0 = y.saturating_sub(radius as usize);
            let x1 = (x + radius as usize + 1).min(width as usize);
            let y1 = (y + radius as usize + 1).min(height as usize);
            if box_sum(x0, y0, x1, y1) > 0 {
                values.push(patch.as_raw()[y * width as usize + x]);
            }
        }
    }
    (!values.is_empty()).then_some(values)
}

pub(super) fn median_u8(values: &mut [u8]) -> u8 {
    values.sort_unstable();
    values[values.len() / 2]
}

pub(super) fn border_median_gray(patch: &GrayImage) -> u8 {
    let (width, height) = patch.dimensions();
    let data = patch.as_raw();
    let mut border = Vec::with_capacity((width as usize + height as usize) * 2);
    for x in 0..width as usize {
        border.push(data[x]);
        border.push(data[(height as usize - 1) * width as usize + x]);
    }
    for y in 1..height as usize - 1 {
        border.push(data[y * width as usize]);
        border.push(data[y * width as usize + width as usize - 1]);
    }
    if border.is_empty() {
        return 0;
    }
    median_u8(&mut border)
}

pub(super) fn patch_gray_stddev_and_edge(patch: &GrayImage) -> (f32, f32) {
    let data = patch.as_raw();
    if data.is_empty() {
        return (0.0, 0.0);
    }
    let count = data.len() as f64;
    let mean = data.iter().map(|value| *value as f64).sum::<f64>() / count;
    let variance = data
        .iter()
        .map(|value| {
            let delta = *value as f64 - mean;
            delta * delta
        })
        .sum::<f64>()
        / count;
    let width = patch.width() as usize;
    let height = patch.height() as usize;
    let mut edge_sum = 0u64;
    let mut edge_count = 0u64;
    if width > 1 {
        for y in 0..height {
            for x in 0..width - 1 {
                edge_sum += data[y * width + x].abs_diff(data[y * width + x + 1]) as u64;
                edge_count += 1;
            }
        }
    }
    let edge = if edge_count == 0 {
        0.0
    } else {
        edge_sum as f32 / edge_count as f32
    };
    (variance.sqrt() as f32, edge)
}

/// 一次匹配的最优位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GrayMatch {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) sad: u64,
}

/// 扫描工作量低于该阈值时不值得付积分图的构建代价。
pub(super) const SEA_MIN_WORK: u64 = 400_000;

/// 纯 CPU 的候选扫描。
pub(crate) fn cpu_best_gray_match(
    haystack: &GrayImage,
    matcher: &GrayMatcher<'_>,
) -> Option<GrayMatch> {
    cpu_best_gray_match_seeded(haystack, matcher, None)
}

/// 在整张 `haystack` 上寻找 SAD 最小的位置。
///
/// 语义与朴素栅格扫描完全一致：以 `alpha_sum + 1` 为初始上界，只有严格更小的 SAD 才改写结果，
/// 因此“所有位置都很差”时同样返回 (0, 0) 与 `alpha_sum + 1`；并列时取栅格序最前的位置。
///
/// `seed` 给定时以它作为初始上界（仍然精确：下界大于 seed 的位置不可能更优）。
pub(super) fn cpu_best_gray_match_seeded(
    haystack: &GrayImage,
    matcher: &GrayMatcher<'_>,
    seed: Option<GrayMatch>,
) -> Option<GrayMatch> {
    if matcher.gray.width() == 0
        || matcher.gray.height() == 0
        || matcher.gray.width() > haystack.width()
        || matcher.gray.height() > haystack.height()
    {
        return None;
    }

    let positions_x = (haystack.width() - matcher.gray.width() + 1) as usize;
    let positions_y = (haystack.height() - matcher.gray.height() + 1) as usize;
    let position_count = positions_x * positions_y;
    let template_area = matcher.gray.width() as u64 * matcher.gray.height() as u64;

    let mut best = seed.unwrap_or(GrayMatch {
        x: 0,
        y: 0,
        sad: matcher.alpha_sum.saturating_add(1),
    });
    // 是否已经有可信的当前最优；决定并列时能否改写位置。
    let mut refined = seed.is_some();
    let mut best_index = seed
        .map(|seed| (seed.y as usize * positions_x) + seed.x as usize)
        .unwrap_or(0);

    let sea_rect = matcher.opaque.filter(|opaque| {
        opaque.width > 0
            && opaque.height > 0
            && position_count as u64 * template_area >= SEA_MIN_WORK
    });

    if let Some(opaque) = sea_rect {
        // 连续消除（SEA）：该矩形内每个像素权重都是 1，于是
        //   SAD >= |Σ画面 - Σ模板| = |盒和 - 模板和|
        // 下界只需一次积分图查表，且与模板大小无关。按下界升序处理，上界会尽快收紧。
        let integral = integral_image(haystack);
        let haystack_width = haystack.width() as usize;
        let mut bounds: Vec<(u32, u32)> = Vec::with_capacity(position_count);
        for y in 0..positions_y {
            for x in 0..positions_x {
                let box_sum = integral_rect(
                    &integral,
                    haystack_width,
                    x + opaque.x as usize,
                    y + opaque.y as usize,
                    opaque.width as usize,
                    opaque.height as usize,
                );
                let bound = box_sum.abs_diff(opaque.gray_sum);
                // Keep equality: an earlier raster position may tie the verified GPU seed.
                if seed.is_some_and(|seed| bound > seed.sad) {
                    continue;
                }
                bounds.push((
                    bound.min(u32::MAX as u64) as u32,
                    (y * positions_x + x) as u32,
                ));
            }
        }
        bounds.sort_unstable_by_key(|entry| entry.0);

        for (bound, index) in bounds {
            let bound = bound as u64;
            // 下界大于当前上界的位置不可能严格更优，其后只会更差。
            if refined && bound > best.sad {
                break;
            }
            let index = index as usize;
            let x = (index % positions_x) as u32;
            let y = (index / positions_x) as u32;
            let sad = sad_at(haystack, matcher, x, y, best.sad);
            if sad < best.sad || (refined && sad == best.sad && index < best_index) {
                best = GrayMatch { x, y, sad };
                best_index = index;
                refined = true;
            }
        }
        return Some(best);
    }

    for y in 0..positions_y {
        for x in 0..positions_x {
            let index = y * positions_x + x;
            let sad = sad_at(haystack, matcher, x as u32, y as u32, best.sad);
            if sad < best.sad || (refined && sad == best.sad && index < best_index) {
                best = GrayMatch {
                    x: x as u32,
                    y: y as u32,
                    sad,
                };
                best_index = index;
                refined = true;
            }
        }
    }
    Some(best)
}

// Invert the final f32 comparison, not real-number arithmetic: rounding can accept
// an integer SAD just above ((1 - threshold) * alpha_sum) truncated to u64.
pub(super) fn presence_sad_limit(alpha_sum: u64, threshold: f32) -> Option<u64> {
    let accepts = |sad: u64| 1.0 - sad as f32 / alpha_sum as f32 >= threshold;
    if alpha_sum == 0 || !accepts(0) {
        return None;
    }
    let (mut low, mut high) = (0, alpha_sum);
    while low < high {
        let middle = low + (high - low) / 2 + (high - low) % 2;
        if accepts(middle) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    Some(low)
}

pub(super) fn gray_match_exists(
    haystack: &GrayImage,
    matcher: &GrayMatcher<'_>,
    threshold: f32,
) -> bool {
    if matcher.gray.width() == 0
        || matcher.gray.height() == 0
        || matcher.gray.width() > haystack.width()
        || matcher.gray.height() > haystack.height()
    {
        return false;
    }
    let Some(limit) = presence_sad_limit(matcher.alpha_sum, threshold) else {
        return false;
    };
    if let Some((x, y)) = gpu_best_position(haystack, matcher) {
        // GPU is only a hint. A rejected hint (or unavailable GPU) still scans CPU.
        if sad_at(haystack, matcher, x, y, limit) <= limit
            && patch_contrast_ok(haystack, matcher, x, y)
        {
            return true;
        }
    }
    let positions_x = haystack.width() - matcher.gray.width() + 1;
    let positions_y = haystack.height() - matcher.gray.height() + 1;
    let sea_rect = matcher.opaque.filter(|opaque| {
        opaque.width > 0
            && opaque.height > 0
            && positions_x as u64
                * positions_y as u64
                * matcher.gray.width() as u64
                * matcher.gray.height() as u64
                >= SEA_MIN_WORK
    });
    let integral = sea_rect.map(|_| integral_image(haystack));
    for y in 0..positions_y {
        for x in 0..positions_x {
            if let (Some(opaque), Some(integral)) = (sea_rect, integral.as_ref()) {
                let box_sum = integral_rect(
                    integral,
                    haystack.width() as usize,
                    (x + opaque.x) as usize,
                    (y + opaque.y) as usize,
                    opaque.width as usize,
                    opaque.height as usize,
                );
                if box_sum.abs_diff(opaque.gray_sum) > limit {
                    continue;
                }
            }
            if sad_at(haystack, matcher, x, y, limit) <= limit
                && patch_contrast_ok(haystack, matcher, x, y)
            {
                return true;
            }
        }
    }
    false
}

/// 收集区域内全部分数达到阈值的位置（`/tools/template` 的命中列表）。
pub(super) fn gray_hits_in_gray(
    haystack: &GrayImage,
    matcher: &GrayMatcher<'_>,
    threshold: f32,
    base: (i32, i32),
) -> Vec<TemplateHit> {
    let max_sad = matcher.alpha_sum;
    if max_sad == 0
        || matcher.gray.width() == 0
        || matcher.gray.height() == 0
        || matcher.gray.width() > haystack.width()
        || matcher.gray.height() > haystack.height()
    {
        return Vec::new();
    }
    let max_allowed_sad = ((1.0 - threshold).clamp(0.0, 1.0) * max_sad as f32) as u64;
    let mut hits = Vec::new();

    for y in 0..=(haystack.height() - matcher.gray.height()) {
        for x in 0..=(haystack.width() - matcher.gray.width()) {
            let sad = sad_at(haystack, matcher, x, y, max_allowed_sad);
            let score = 1.0 - sad as f32 / max_sad as f32;
            if score >= threshold && patch_contrast_ok(haystack, matcher, x, y) {
                hits.push(TemplateHit {
                    kind: "template".to_string(),
                    x: base.0 + x as i32,
                    y: base.1 + y as i32,
                    width: matcher.gray.width(),
                    height: matcher.gray.height(),
                    score,
                    quality_ok: true,
                });
            }
        }
    }
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.y.cmp(&right.y))
            .then_with(|| left.x.cmp(&right.x))
    });
    dedupe_hits(
        hits,
        matcher.gray.width().max(8) as i32 / 2,
        matcher.gray.height().max(8) as i32 / 2,
    )
}

/// 单点 SAD。`max_allowed` 是当前上界：一旦超出就立刻返回（部分失真消除，PDE），
/// 返回值此时必然大于上界，调用方据此判定该位置不可能获胜。
pub(super) fn sad_at(
    haystack: &GrayImage,
    matcher: &GrayMatcher<'_>,
    x: u32,
    y: u32,
    max_allowed: u64,
) -> u64 {
    let haystack_width = haystack.width() as usize;
    let template_width = matcher.gray.width() as usize;
    let template_height = matcher.gray.height() as usize;
    let x = x as usize;
    let y = y as usize;
    let haystack_data = haystack.as_raw();
    let template_data = matcher.gray.as_raw();
    let mut sad = 0u64;

    match matcher.alpha {
        // 完全不透明：内层是连续切片，没有逐像素的 alpha 读写，便于编译器向量化。
        None => {
            for row in 0..template_height {
                let haystack_offset = (y + row) * haystack_width + x;
                let template_offset = row * template_width;
                let haystack_row =
                    &haystack_data[haystack_offset..haystack_offset + template_width];
                let template_row =
                    &template_data[template_offset..template_offset + template_width];
                for (haystack_pixel, template_pixel) in haystack_row.iter().zip(template_row) {
                    sad += haystack_pixel.abs_diff(*template_pixel) as u64;
                }
                if sad > max_allowed {
                    return sad;
                }
            }
        }
        // 带 alpha：逐像素公式与改造前完全一致（含整数除法与截断顺序）。
        Some(alpha) => {
            let alpha_data = alpha.as_raw();
            for row in 0..template_height {
                let haystack_offset = (y + row) * haystack_width + x;
                let template_offset = row * template_width;
                for column in 0..template_width {
                    let weight = alpha_data[template_offset + column] as u64;
                    if weight == 0 {
                        continue;
                    }
                    sad += haystack_data[haystack_offset + column]
                        .abs_diff(template_data[template_offset + column])
                        as u64
                        * weight
                        / 255;
                    if sad > max_allowed {
                        return sad;
                    }
                }
            }
        }
    }
    sad
}

/// 积分图（前缀和）。u32 足够：1920×1080×255 仍小于 2^32。
pub(super) fn integral_image(image: &GrayImage) -> Vec<u32> {
    let width = image.width() as usize;
    let height = image.height() as usize;
    let stride = width + 1;
    let data = image.as_raw();
    let mut integral = vec![0u32; stride * (height + 1)];

    for row in 0..height {
        let mut row_sum = 0u32;
        for column in 0..width {
            row_sum += data[row * width + column] as u32;
            integral[(row + 1) * stride + column + 1] =
                integral[row * stride + column + 1] + row_sum;
        }
    }
    integral
}

pub(super) fn integral_rect(
    integral: &[u32],
    image_width: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> u64 {
    let stride = image_width + 1;
    let top_left = integral[y * stride + x] as u64;
    let top_right = integral[y * stride + x + width] as u64;
    let bottom_left = integral[(y + height) * stride + x] as u64;
    let bottom_right = integral[(y + height) * stride + x + width] as u64;
    bottom_right + top_left - top_right - bottom_left
}

pub(super) fn sum_gray_rect(image: &GrayImage, x: u32, y: u32, width: u32, height: u32) -> u64 {
    let stride = image.width() as usize;
    let data = image.as_raw();
    let mut sum = 0u64;
    for row in 0..height as usize {
        let offset = (y as usize + row) * stride + x as usize;
        for column in 0..width as usize {
            sum += data[offset + column] as u64;
        }
    }
    sum
}

/// 模板内最大的全 255 轴对齐矩形（直方图法），没有则返回 `None`。
/// 只在模板首次载入时算一次。
pub(super) fn largest_opaque_rect(alpha: &GrayImage) -> Option<OpaqueRect> {
    let width = alpha.width() as usize;
    let height = alpha.height() as usize;
    if width == 0 || height == 0 {
        return None;
    }
    let data = alpha.as_raw();
    let mut heights = vec![0usize; width];
    let mut stack: Vec<usize> = Vec::with_capacity(width + 1);
    let mut best: Option<(usize, usize, usize, usize)> = None;

    for row in 0..height {
        for column in 0..width {
            heights[column] = if data[row * width + column] == 255 {
                heights[column] + 1
            } else {
                0
            };
        }
        stack.clear();
        let mut column = 0usize;
        while column <= width {
            let current = if column == width { 0 } else { heights[column] };
            while let Some(&top) = stack.last() {
                if heights[top] <= current {
                    break;
                }
                stack.pop();
                let rect_height = heights[top];
                let left = stack.last().map(|&index| index + 1).unwrap_or(0);
                let rect_width = column - left;
                if rect_height > 0 {
                    let area = rect_height * rect_width;
                    let best_area = best.map(|(_, _, w, h)| w * h).unwrap_or(0);
                    if area > best_area {
                        best = Some((left, row + 1 - rect_height, rect_width, rect_height));
                    }
                }
            }
            stack.push(column);
            column += 1;
        }
    }

    best.map(|(x, y, width, height)| OpaqueRect {
        x: x as u32,
        y: y as u32,
        width: width as u32,
        height: height as u32,
        gray_sum: 0,
    })
}

/// 改造前的逐像素朴素扫描。仅用于测试，作为优化实现的对照基准。
#[cfg(test)]
pub(super) fn gray_sad_at(
    haystack: &GrayImage,
    template: &GrayImage,
    alpha: &GrayImage,
    x: u32,
    y: u32,
    max_allowed_sad: u64,
) -> u64 {
    let haystack_width = haystack.width() as usize;
    let template_width = template.width() as usize;
    let template_height = template.height() as usize;
    let x = x as usize;
    let y = y as usize;
    let haystack_data = haystack.as_raw();
    let template_data = template.as_raw();
    let mut sad = 0u64;

    for row in 0..template_height {
        let haystack_offset = (y + row) * haystack_width + x;
        let template_offset = row * template_width;
        for column in 0..template_width {
            let alpha = alpha.as_raw()[template_offset + column] as u64;
            if alpha == 0 {
                continue;
            }
            sad += haystack_data[haystack_offset + column]
                .abs_diff(template_data[template_offset + column]) as u64
                * alpha
                / 255;
            if sad > max_allowed_sad {
                return sad;
            }
        }
    }
    sad
}

pub(crate) fn dedupe_hits(
    mut hits: Vec<TemplateHit>,
    tolerance_x: i32,
    tolerance_y: i32,
) -> Vec<TemplateHit> {
    let mut picked: Vec<TemplateHit> = Vec::new();
    hits.sort_by(|left, right| right.score.total_cmp(&left.score));
    for hit in hits {
        if picked.iter().any(|existing| {
            (existing.x - hit.x).abs() <= tolerance_x && (existing.y - hit.y).abs() <= tolerance_y
        }) {
            continue;
        }
        picked.push(hit);
    }
    picked.sort_by(compare_hits_top_left);
    picked
}

pub(super) fn compare_hits_top_left(left: &TemplateHit, right: &TemplateHit) -> Ordering {
    (left.y / 10)
        .cmp(&(right.y / 10))
        .then_with(|| left.x.cmp(&right.x))
        .then_with(|| left.y.cmp(&right.y))
}
