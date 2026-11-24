use anyhow::Result;
use image::DynamicImage;
use image::imageops::FilterType;

use super::geometry::{Rect, crop_canvas};

/// 聊天指纹的固定尺寸：抽一块区域后统一缩放到它。
const CHAT_FINGERPRINT_WIDTH: u32 = 104;
const CHAT_FINGERPRINT_HEIGHT: u32 = 36;

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

/// 「没有可观测区域」的指纹：与抽到一块纯黑区域等价，任两帧之间恒相等。
///
/// 用于「气泡存在但发言人区域不可用」：那块区域本来就没有内容，
/// 不该因为 `Option` 而把整条气泡判成不稳定（那等于把刚救回来的消息又丢掉）。
pub(crate) fn blank_change_fingerprint() -> ChangeFingerprint {
    ChangeFingerprint {
        pixels: vec![0u8; (CHAT_FINGERPRINT_WIDTH * CHAT_FINGERPRINT_HEIGHT) as usize],
        width: CHAT_FINGERPRINT_WIDTH,
        height: CHAT_FINGERPRINT_HEIGHT,
    }
}

fn chat_change_fingerprint(image: &DynamicImage, chat_rect: Rect) -> Result<ChangeFingerprint> {
    const WIDTH: u32 = CHAT_FINGERPRINT_WIDTH;
    const HEIGHT: u32 = CHAT_FINGERPRINT_HEIGHT;

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
