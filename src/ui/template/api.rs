//! 公开模板匹配 API：命中列表、最佳候选、降采样与内嵌模板。

use crate::geometry::Rect;
use crate::geometry::TemplateHit;
use crate::geometry::crop_canvas;
use anyhow::Result;
use image::DynamicImage;
use image::GrayImage;
use image::RgbImage;
use image::imageops::FilterType;
use std::path::Path;

use super::cache::{
    GrayTemplate, cached_downscaled_gray_template, cached_gray_template, cached_rgb_template,
    is_text_template,
};
use super::cpu::{
    GrayMatch, GrayMatcher, color_sad_at, dedupe_hits, gray_hits_in_gray, gray_match_exists,
    patch_contrast_ok, text_anchor_quality_ok_template,
};
use super::gpu::best_gray_match;

pub(crate) fn find_template_hits(
    image: &DynamicImage,
    search_rect: Option<Rect>,
    template_path: &Path,
    threshold: f32,
) -> Result<Vec<TemplateHit>> {
    let haystack = match search_rect {
        Some(rect) => crop_canvas(image, rect)?,
        None => image.clone(),
    };
    let template = cached_gray_template(template_path)?;
    if template.gray.width() > haystack.width() || template.gray.height() > haystack.height() {
        return Ok(Vec::new());
    }
    let base_x = search_rect.map(|rect| rect.x).unwrap_or(0);
    let base_y = search_rect.map(|rect| rect.y).unwrap_or(0);
    let matcher = GrayMatcher::from_template(&template);
    let mut hits = gray_hits_in_gray(&haystack.to_luma8(), &matcher, threshold, (base_x, base_y));
    if is_text_template(template_path) {
        for hit in &mut hits {
            if !text_anchor_quality_ok_template(
                image,
                Rect::new(hit.x, hit.y, hit.width, hit.height),
                template_path,
            ) {
                hit.quality_ok = false;
            }
        }
        hits.retain(|hit| hit.quality_ok);
    }
    Ok(hits)
}

pub(crate) fn find_color_template_hit_groups(
    image: &DynamicImage,
    search_rect: Option<Rect>,
    template_paths: &[&Path],
    threshold: f32,
) -> Result<Vec<Vec<TemplateHit>>> {
    let haystack = match search_rect {
        Some(rect) => crop_canvas(image, rect)?,
        None => image.clone(),
    };
    let haystack_rgb = haystack.to_rgb8();
    template_paths
        .iter()
        .map(|template_path| {
            find_color_template_hits_in_rgb(&haystack_rgb, search_rect, template_path, threshold)
        })
        .collect()
}

pub(super) fn find_color_template_hits_in_rgb(
    haystack_rgb: &RgbImage,
    search_rect: Option<Rect>,
    template_path: &Path,
    threshold: f32,
) -> Result<Vec<TemplateHit>> {
    let template_rgb = cached_rgb_template(template_path)?;
    if template_rgb.width() > haystack_rgb.width() || template_rgb.height() > haystack_rgb.height()
    {
        return Ok(Vec::new());
    }

    let max_sad = template_rgb.width() as u64 * template_rgb.height() as u64 * 3 * 255;
    let max_allowed_sad = ((1.0 - threshold).clamp(0.0, 1.0) * max_sad as f32) as u64;
    let mut hits = Vec::new();

    for y in 0..=(haystack_rgb.height() - template_rgb.height()) {
        for x in 0..=(haystack_rgb.width() - template_rgb.width()) {
            let sad = color_sad_at(haystack_rgb, &template_rgb, x, y, max_allowed_sad);
            let score = 1.0 - sad as f32 / max_sad as f32;
            if score >= threshold {
                let base_x = search_rect.map(|rect| rect.x).unwrap_or(0);
                let base_y = search_rect.map(|rect| rect.y).unwrap_or(0);
                hits.push(TemplateHit {
                    kind: "template".to_string(),
                    x: base_x + x as i32,
                    y: base_y + y as i32,
                    width: template_rgb.width(),
                    height: template_rgb.height(),
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
    Ok(dedupe_hits(
        hits,
        template_rgb.width().max(8) as i32 / 2,
        template_rgb.height().max(8) as i32 / 2,
    ))
}

pub(crate) fn best_template_hit(
    image: &DynamicImage,
    search_rect: Option<Rect>,
    template_path: &Path,
    threshold: f32,
) -> Result<Option<TemplateHit>> {
    let candidate = best_template_candidate(image, search_rect, template_path)?;
    // **逐帧调用**，所以未达阈值只走 trace：默认级别下不留痕，
    // 排障时把 logging.level 调到 trace 就能看到每一帧的分数。
    // 规范 §五要求「写明哪个模板、哪个区域、期望多少、实际多少」，四要素齐全。
    if let Some(candidate) = &candidate
        && candidate.score < threshold
    {
        log::trace!(
            "模板「{}」未达阈值：搜索区域 {}，期望分数 {:.3}，实际 {:.3}",
            template_path.display(),
            describe_search_rect(search_rect),
            threshold,
            candidate.score
        );
    }
    Ok(candidate.filter(|candidate| candidate.quality_ok && candidate.score >= threshold))
}

/// 搜索区域的简短描述：给了区域就写几何，全图就写「整幅画面」。
pub(super) fn describe_search_rect(search_rect: Option<Rect>) -> String {
    match search_rect {
        Some(rect) => format!("({}, {}) {}×{}", rect.x, rect.y, rect.width, rect.height),
        None => "整幅画面".to_string(),
    }
}

/// Check presence without selecting a best position or constructing hit evidence.
pub(crate) fn template_exists(
    image: &DynamicImage,
    search_rect: Option<Rect>,
    template_path: &Path,
    threshold: f32,
) -> Result<bool> {
    if !threshold.is_finite() || threshold < 0.0 || threshold > 1.0 {
        return Ok(best_template_hit(image, search_rect, template_path, threshold)?.is_some());
    }
    // 文字模板必须走候选路径，才能对命中位置做内容质量校验。
    if is_text_template(template_path) {
        return Ok(best_template_candidate(image, search_rect, template_path)?
            .is_some_and(|candidate| candidate.quality_ok && candidate.score >= threshold));
    }
    let haystack = match search_rect {
        Some(rect) => crop_canvas(image, rect)?,
        None => image.clone(),
    };
    let template = cached_gray_template(template_path)?;
    if template.gray.width() > haystack.width() || template.gray.height() > haystack.height() {
        return Ok(false);
    }
    let matcher = GrayMatcher::from_template(&template);
    Ok(gray_match_exists(&haystack.to_luma8(), &matcher, threshold))
}

pub(crate) fn best_template_candidate(
    image: &DynamicImage,
    search_rect: Option<Rect>,
    template_path: &Path,
) -> Result<Option<TemplateHit>> {
    let haystack = match search_rect {
        Some(rect) => crop_canvas(image, rect)?,
        None => image.clone(),
    };
    let template = cached_gray_template(template_path)?;
    if template.gray.width() > haystack.width() || template.gray.height() > haystack.height() {
        return Ok(None);
    }

    let haystack_gray = haystack.to_luma8();
    let max_sad = template.alpha_sum;
    if max_sad == 0 {
        return Ok(None);
    }
    let matcher = GrayMatcher::from_template(&template);
    let found = best_gray_match(&haystack_gray, &matcher).unwrap_or(GrayMatch {
        x: 0,
        y: 0,
        sad: max_sad.saturating_add(1),
    });

    if !patch_contrast_ok(&haystack_gray, &matcher, found.x, found.y) {
        log::trace!(
            "模板「{}」最优点 ({} , {}) 落在平坦画面，按透明模板防护拒绝",
            template_path.display(),
            found.x,
            found.y
        );
        return Ok(None);
    }
    let score = 1.0 - found.sad as f32 / max_sad as f32;
    let base_x = search_rect.map(|rect| rect.x).unwrap_or(0);
    let base_y = search_rect.map(|rect| rect.y).unwrap_or(0);
    let quality_ok = text_anchor_quality_ok_template(
        image,
        Rect::new(
            base_x + found.x as i32,
            base_y + found.y as i32,
            template.gray.width(),
            template.gray.height(),
        ),
        template_path,
    );
    if !quality_ok {
        log::trace!(
            "模板「{}」命中 ({}, {}) 但文字内容质量不足，标记为拒绝候选（分数 {:.3}）",
            template_path.display(),
            base_x + found.x as i32,
            base_y + found.y as i32,
            score,
        );
    }
    Ok(Some(TemplateHit {
        kind: "template".to_string(),
        x: base_x + found.x as i32,
        y: base_y + found.y as i32,
        width: template.gray.width(),
        height: template.gray.height(),
        score,
        quality_ok,
    }))
}

/// 用内存里的模板图片做同样的灰度 SAD 匹配（不读磁盘、不走模板缓存）。
///
/// 用途：代码自带的小图标锚点（如表情面板的红心/齿轮）。这类模板属于 UI 结构，
/// 不交给用户配置；同时避免为一次匹配写临时文件。
pub(crate) fn best_embedded_template_candidate(
    image: &DynamicImage,
    search_rect: Rect,
    template_image: &DynamicImage,
) -> Result<Option<TemplateHit>> {
    let haystack = crop_canvas(image, search_rect)?;
    let rgba = template_image.to_rgba8();
    let gray = DynamicImage::ImageRgba8(rgba.clone()).to_luma8();
    let alpha = GrayImage::from_raw(
        rgba.width(),
        rgba.height(),
        rgba.pixels().map(|pixel| pixel[3]).collect(),
    )
    .expect("RGBA alpha plane must have the same dimensions");
    let template = GrayTemplate::build(gray, alpha, Path::new("<embedded-emoji-anchor>"));
    if template.gray.width() > haystack.width() || template.gray.height() > haystack.height() {
        return Ok(None);
    }
    let haystack_gray = haystack.to_luma8();
    let max_sad = template.alpha_sum;
    if max_sad == 0 {
        return Ok(None);
    }
    let matcher = GrayMatcher::from_template(&template);
    let found = best_gray_match(&haystack_gray, &matcher).unwrap_or(GrayMatch {
        x: 0,
        y: 0,
        sad: max_sad.saturating_add(1),
    });
    if !patch_contrast_ok(&haystack_gray, &matcher, found.x, found.y) {
        return Ok(None);
    }
    let score = 1.0 - found.sad as f32 / max_sad as f32;
    Ok(Some(TemplateHit {
        kind: "template".to_string(),
        x: search_rect.x + found.x as i32,
        y: search_rect.y + found.y as i32,
        width: template.gray.width(),
        height: template.gray.height(),
        score,
        quality_ok: true,
    }))
}

/// 与 [`best_template_candidate`] 同样的灰度 SAD，但**先按整数倍降采样再匹配**。
///
/// 用途：区域/模板都很大的探针（代价 ≈ 候选位置数 × 模板像素数）。两个维度同时除以
/// `scale`，总代价降到约 `1/scale⁴`（`scale=4` 即约 256×）。命中坐标乘回 `scale`，
/// 因此仍在**原图坐标系**里，只是位置精度降到 `scale` 像素——对"这个面板在不在"足够，
/// 而需要点击精度的路径（例程）仍走全分辨率的 [`best_template_hit`]，不受影响。
///
/// `scale=1` 时直接退化为全分辨率匹配。
pub(crate) fn best_template_candidate_downscaled(
    image: &DynamicImage,
    search_rect: Option<Rect>,
    template_path: &Path,
    scale: u32,
) -> Result<Option<TemplateHit>> {
    let scale = scale.max(1);
    if scale == 1 {
        return best_template_candidate(image, search_rect, template_path);
    }
    let haystack = match search_rect {
        Some(rect) => crop_canvas(image, rect)?,
        None => image.clone(),
    };
    // 画面侧与模板侧必须用同一个滤波器、同一个倍数缩小，否则同一内容会落到不同像素上。
    let haystack_gray = DynamicImage::ImageLuma8(haystack.to_luma8())
        .resize_exact(
            (haystack.width() / scale).max(1),
            (haystack.height() / scale).max(1),
            FilterType::Triangle,
        )
        .to_luma8();
    let template = cached_downscaled_gray_template(template_path, scale)?;
    if template.gray.width() > haystack_gray.width()
        || template.gray.height() > haystack_gray.height()
    {
        return Ok(None);
    }
    let max_sad = template.alpha_sum;
    if max_sad == 0 {
        return Ok(None);
    }
    let matcher = GrayMatcher::from_template(&template);
    let found = best_gray_match(&haystack_gray, &matcher).unwrap_or(GrayMatch {
        x: 0,
        y: 0,
        sad: max_sad.saturating_add(1),
    });
    if !patch_contrast_ok(&haystack_gray, &matcher, found.x, found.y) {
        log::trace!(
            "模板「{}」降采样最优点 ({} , {}) 落在平坦画面，按透明模板防护拒绝",
            template_path.display(),
            found.x,
            found.y
        );
        return Ok(None);
    }
    let score = 1.0 - found.sad as f32 / max_sad as f32;
    let base_x = search_rect.map(|rect| rect.x).unwrap_or(0);
    let base_y = search_rect.map(|rect| rect.y).unwrap_or(0);
    let hit_x = base_x + found.x as i32 * scale as i32;
    let hit_y = base_y + found.y as i32 * scale as i32;
    let hit_width = template.gray.width() * scale;
    let hit_height = template.gray.height() * scale;
    // 质量校验用全分辨率模板的 alpha：降采样模板只有 1/scale 的字形细节，
    // 拿它当 mask 会把边缘算进背景。`cached_gray_template` 已经缓存，无额外解码。
    let quality_ok = text_anchor_quality_ok_template(
        image,
        Rect::new(hit_x, hit_y, hit_width, hit_height),
        template_path,
    );
    if !quality_ok {
        log::trace!(
            "模板「{}」降采样命中 ({}, {}) 但文字内容质量不足，标记为拒绝候选（分数 {:.3}）",
            template_path.display(),
            hit_x,
            hit_y,
            score,
        );
    }
    Ok(Some(TemplateHit {
        kind: "template".to_string(),
        x: hit_x,
        y: hit_y,
        width: hit_width,
        height: hit_height,
        score,
        quality_ok,
    }))
}
