use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, anyhow};
use image::{DynamicImage, GrayImage, RgbImage};
use serde::Serialize;
use template_matching::{Image as MatchImage, MatchTemplateMethod, find_extremes, match_template};

use super::geometry::{Point, Rect, crop_canvas};

static RGB_TEMPLATE_CACHE: OnceLock<Mutex<HashMap<PathBuf, RgbImage>>> = OnceLock::new();

#[derive(Clone, Debug, Serialize)]
pub(super) struct TemplateHit {
    pub(super) kind: String,
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) score: f32,
}

impl TemplateHit {
    pub(super) fn rect(&self) -> Rect {
        Rect::new(self.x, self.y, self.width, self.height)
    }

    pub(super) fn center(&self) -> Point {
        self.rect().center()
    }
}

pub(super) fn find_template_hits(
    image: &DynamicImage,
    search_rect: Option<Rect>,
    template_path: &Path,
    threshold: f32,
) -> Result<Vec<TemplateHit>> {
    let haystack = match search_rect {
        Some(rect) => crop_canvas(image, rect)?,
        None => image.clone(),
    };
    let template = image::open(template_path)
        .with_context(|| format!("open template {}", template_path.display()))?;

    if template.width() > haystack.width() || template.height() > haystack.height() {
        return Ok(Vec::new());
    }

    let haystack_gray = haystack.to_luma8();
    let template_gray = template.to_luma8();
    let haystack_match = to_match_image(&haystack_gray);
    let template_match = to_match_image(&template_gray);
    let result = match_template(
        haystack_match,
        template_match,
        MatchTemplateMethod::SumOfAbsoluteDifferences,
    );
    let max_sad = (template_gray.width() * template_gray.height()).max(1) as f32;
    let mut hits = Vec::new();
    for y in 0..result.height {
        for x in 0..result.width {
            let idx = (y * result.width + x) as usize;
            let score = 1.0 - result.data[idx] / max_sad;
            if score >= threshold {
                let base_x = search_rect.map(|rect| rect.x).unwrap_or(0);
                let base_y = search_rect.map(|rect| rect.y).unwrap_or(0);
                hits.push(TemplateHit {
                    kind: "template".to_string(),
                    x: base_x + x as i32,
                    y: base_y + y as i32,
                    width: template_gray.width(),
                    height: template_gray.height(),
                    score,
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
        template_gray.width().max(8) as i32 / 2,
        template_gray.height().max(8) as i32 / 2,
    ))
}

pub(super) fn find_color_template_hits(
    image: &DynamicImage,
    search_rect: Option<Rect>,
    template_path: &Path,
    threshold: f32,
) -> Result<Vec<TemplateHit>> {
    let haystack = match search_rect {
        Some(rect) => crop_canvas(image, rect)?,
        None => image.clone(),
    };
    let template_rgb = cached_rgb_template(template_path)?;

    if template_rgb.width() > haystack.width() || template_rgb.height() > haystack.height() {
        return Ok(Vec::new());
    }

    let haystack_rgb = haystack.to_rgb8();
    let max_sad = template_rgb.width() as u64 * template_rgb.height() as u64 * 3 * 255;
    let max_allowed_sad = ((1.0 - threshold).clamp(0.0, 1.0) * max_sad as f32) as u64;
    let mut hits = Vec::new();

    for y in 0..=(haystack_rgb.height() - template_rgb.height()) {
        for x in 0..=(haystack_rgb.width() - template_rgb.width()) {
            let sad = color_sad_at(&haystack_rgb, &template_rgb, x, y, max_allowed_sad);
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

fn cached_rgb_template(template_path: &Path) -> Result<RgbImage> {
    let cache = RGB_TEMPLATE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .map_err(|_| anyhow!("template cache mutex poisoned"))?;
    if let Some(image) = cache.get(template_path) {
        return Ok(image.clone());
    }
    let image = image::open(template_path)
        .with_context(|| format!("open template {}", template_path.display()))?
        .to_rgb8();
    cache.insert(template_path.to_path_buf(), image.clone());
    Ok(image)
}

fn color_sad_at(
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

    for row in 0..template_height {
        let haystack_offset = ((y + row) * haystack_width + x) * 3;
        let template_offset = row * template_width * 3;
        for channel in 0..(template_width * 3) {
            sad += haystack_data[haystack_offset + channel]
                .abs_diff(template_data[template_offset + channel]) as u64;
            if sad > max_allowed_sad {
                return sad;
            }
        }
    }
    sad
}

pub(super) fn best_template_hit(
    image: &DynamicImage,
    search_rect: Option<Rect>,
    template_path: &Path,
    threshold: f32,
) -> Result<Option<TemplateHit>> {
    let haystack = match search_rect {
        Some(rect) => crop_canvas(image, rect)?,
        None => image.clone(),
    };
    let template = image::open(template_path)
        .with_context(|| format!("open template {}", template_path.display()))?;
    if template.width() > haystack.width() || template.height() > haystack.height() {
        return Ok(None);
    }

    let haystack_gray = haystack.to_luma8();
    let template_gray = template.to_luma8();
    let result = match_template(
        to_match_image(&haystack_gray),
        to_match_image(&template_gray),
        MatchTemplateMethod::SumOfAbsoluteDifferences,
    );
    let extremes = find_extremes(&result);
    let max_sad = (template_gray.width() * template_gray.height()).max(1) as f32;
    let score = 1.0 - extremes.min_value / max_sad;
    if score < threshold {
        return Ok(None);
    }
    let base_x = search_rect.map(|rect| rect.x).unwrap_or(0);
    let base_y = search_rect.map(|rect| rect.y).unwrap_or(0);
    Ok(Some(TemplateHit {
        kind: "template".to_string(),
        x: base_x + extremes.min_value_location.0 as i32,
        y: base_y + extremes.min_value_location.1 as i32,
        width: template_gray.width(),
        height: template_gray.height(),
        score,
    }))
}

fn to_match_image(image: &GrayImage) -> MatchImage<'static> {
    let data = image
        .as_raw()
        .iter()
        .map(|value| *value as f32 / 255.0)
        .collect::<Vec<_>>();
    MatchImage::new(data, image.width(), image.height())
}

pub(super) fn dedupe_hits(
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

fn compare_hits_top_left(left: &TemplateHit, right: &TemplateHit) -> Ordering {
    (left.y / 10)
        .cmp(&(right.y / 10))
        .then_with(|| left.x.cmp(&right.x))
        .then_with(|| left.y.cmp(&right.y))
}
