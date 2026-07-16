use anyhow::Result;
use image::DynamicImage;
use image::imageops::FilterType;

use super::geometry::{Rect, crop_canvas};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChangeFingerprint {
    pub(crate) pixels: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ChangeStats {
    pub(crate) mean_abs_diff: f32,
    pub(crate) changed_ratio: f32,
}

pub(crate) fn rect_chat_change_fingerprint(
    image: &DynamicImage,
    rect: Rect,
) -> Result<ChangeFingerprint> {
    chat_change_fingerprint(image, rect)
}

fn chat_change_fingerprint(image: &DynamicImage, chat_rect: Rect) -> Result<ChangeFingerprint> {
    const WIDTH: u32 = 104;
    const HEIGHT: u32 = 36;

    let chat = crop_canvas(image, chat_rect)?;
    let gray = chat
        .resize_exact(WIDTH, HEIGHT, FilterType::Triangle)
        .to_luma8();
    Ok(ChangeFingerprint {
        pixels: gray.into_raw(),
        width: WIDTH,
        height: HEIGHT,
    })
}

pub(crate) fn change_stats(
    previous: &ChangeFingerprint,
    current: &ChangeFingerprint,
) -> ChangeStats {
    if previous.width != current.width
        || previous.height != current.height
        || previous.pixels.len() != current.pixels.len()
    {
        return ChangeStats {
            mean_abs_diff: f32::MAX,
            changed_ratio: 1.0,
        };
    }

    let mut total_diff = 0u64;
    let mut changed = 0usize;
    for (left, right) in previous.pixels.iter().zip(&current.pixels) {
        let diff = left.abs_diff(*right);
        total_diff += diff as u64;
        if diff >= 12 {
            changed += 1;
        }
    }
    let count = previous.pixels.len().max(1);
    ChangeStats {
        mean_abs_diff: total_diff as f32 / count as f32,
        changed_ratio: changed as f32 / count as f32,
    }
}
