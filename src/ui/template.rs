use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, anyhow};
use image::{DynamicImage, GrayImage, RgbImage};
use serde::Serialize;
use template_matching::{Image as MatchImage, MatchTemplateMethod, match_template};

use super::geometry::{Point, Rect, crop_canvas};

static RGB_TEMPLATE_CACHE: OnceLock<Mutex<HashMap<PathBuf, RgbImage>>> = OnceLock::new();
static GRAY_TEMPLATE_CACHE: OnceLock<Mutex<HashMap<PathBuf, GrayImage>>> = OnceLock::new();

#[derive(Clone, Debug, Serialize)]
pub(crate) struct TemplateHit {
    pub(crate) kind: String,
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) score: f32,
}

impl TemplateHit {
    pub(crate) fn rect(&self) -> Rect {
        Rect::new(self.x, self.y, self.width, self.height)
    }

    pub(crate) fn center(&self) -> Point {
        self.rect().center()
    }
}

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

pub(crate) fn find_color_template_hits(
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

fn cached_gray_template(template_path: &Path) -> Result<GrayImage> {
    let cache = GRAY_TEMPLATE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .map_err(|_| anyhow!("template cache mutex poisoned"))?;
    if let Some(image) = cache.get(template_path) {
        return Ok(image.clone());
    }
    let image = image::open(template_path)
        .with_context(|| format!("open template {}", template_path.display()))?
        .to_luma8();
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

pub(crate) fn best_template_hit(
    image: &DynamicImage,
    search_rect: Option<Rect>,
    template_path: &Path,
    threshold: f32,
) -> Result<Option<TemplateHit>> {
    let haystack = match search_rect {
        Some(rect) => crop_canvas(image, rect)?,
        None => image.clone(),
    };
    let template_gray = cached_gray_template(template_path)?;
    if template_gray.width() > haystack.width() || template_gray.height() > haystack.height() {
        return Ok(None);
    }

    let haystack_gray = haystack.to_luma8();
    let max_sad = template_gray.width() as u64 * template_gray.height() as u64 * 255;
    if max_sad == 0 {
        return Ok(None);
    }
    let max_allowed_sad = ((1.0 - threshold).clamp(0.0, 1.0) * max_sad as f32) as u64;
    let mut best_sad = max_allowed_sad.saturating_add(1);
    let mut best_x = 0;
    let mut best_y = 0;

    for y in 0..=(haystack_gray.height() - template_gray.height()) {
        for x in 0..=(haystack_gray.width() - template_gray.width()) {
            let sad = gray_sad_at(&haystack_gray, &template_gray, x, y, best_sad);
            if sad < best_sad {
                best_sad = sad;
                best_x = x;
                best_y = y;
            }
        }
    }

    let score = 1.0 - best_sad as f32 / max_sad as f32;
    if score < threshold {
        return Ok(None);
    }
    let base_x = search_rect.map(|rect| rect.x).unwrap_or(0);
    let base_y = search_rect.map(|rect| rect.y).unwrap_or(0);
    Ok(Some(TemplateHit {
        kind: "template".to_string(),
        x: base_x + best_x as i32,
        y: base_y + best_y as i32,
        width: template_gray.width(),
        height: template_gray.height(),
        score,
    }))
}

fn gray_sad_at(
    haystack: &GrayImage,
    template: &GrayImage,
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
            sad += haystack_data[haystack_offset + column]
                .abs_diff(template_data[template_offset + column]) as u64;
            if sad > max_allowed_sad {
                return sad;
            }
        }
    }
    sad
}

fn to_match_image(image: &GrayImage) -> MatchImage<'static> {
    let data = image
        .as_raw()
        .iter()
        .map(|value| *value as f32 / 255.0)
        .collect::<Vec<_>>();
    MatchImage::new(data, image.width(), image.height())
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

fn compare_hits_top_left(left: &TemplateHit, right: &TemplateHit) -> Ordering {
    (left.y / 10)
        .cmp(&(right.y / 10))
        .then_with(|| left.x.cmp(&right.x))
        .then_with(|| left.y.cmp(&right.y))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use image::{DynamicImage, Luma};

    use super::*;

    #[test]
    fn best_template_hit_finds_best_gray_sad_match() {
        let mut haystack = GrayImage::from_pixel(6, 5, Luma([0]));
        let template = GrayImage::from_fn(2, 2, |x, y| match (x, y) {
            (0, 0) => Luma([10]),
            (1, 0) => Luma([40]),
            (0, 1) => Luma([90]),
            _ => Luma([160]),
        });
        for y in 0..template.height() {
            for x in 0..template.width() {
                haystack.put_pixel(3 + x, 2 + y, *template.get_pixel(x, y));
            }
        }

        let path = std::env::temp_dir().join(format!(
            "miliastra-template-test-{}-{}.png",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        template.save(&path).expect("save template");

        let hit = best_template_hit(&DynamicImage::ImageLuma8(haystack), None, &path, 0.99)
            .expect("match template")
            .expect("template hit");
        let _ = fs::remove_file(&path);

        assert_eq!((hit.x, hit.y), (3, 2));
        assert_eq!((hit.width, hit.height), (2, 2));
        assert!(hit.score > 0.999);
    }
}
