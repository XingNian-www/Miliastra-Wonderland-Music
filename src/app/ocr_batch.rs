use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use image::{DynamicImage, GenericImage, ImageBuffer, Rgba};
use ocr_rs::OcrEngine;

use super::Rect;
use super::ocr::{OcrLine, merge_ocr_lines, recognize_lines};

const BLOCK_GAP: u32 = 12;
const GAP_COLOR: Rgba<u8> = Rgba([180, 180, 180, 255]);

pub(super) fn batch_recognize_blocks(
    engine: &OcrEngine,
    chat: &DynamicImage,
    blocks: &[Rect],
    same_line_y_tolerance: i32,
) -> Result<Vec<String>> {
    if blocks.is_empty() {
        return Ok(Vec::new());
    }

    let started = Instant::now();

    let crops: Vec<DynamicImage> = blocks
        .iter()
        .map(|block| {
            if block.x < 0
                || block.y < 0
                || block.right() > chat.width() as i32
                || block.bottom() > chat.height() as i32
            {
                return Err(anyhow!(
                    "批量 crop rect {},{},{},{} outside chat {}x{}",
                    block.x,
                    block.y,
                    block.width,
                    block.height,
                    chat.width(),
                    chat.height()
                ));
            }
            Ok(chat.crop_imm(block.x as u32, block.y as u32, block.width, block.height))
        })
        .collect::<Result<_>>()?;

    let max_width = crops.iter().map(|c| c.width()).max().unwrap_or(1).max(1);
    let total_height: u32 = crops.iter().map(|c| c.height()).sum::<u32>()
        + BLOCK_GAP * crops.len().saturating_sub(1) as u32;
    let total_height = total_height.max(1);

    let mut combined: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_pixel(max_width, total_height, GAP_COLOR);
    let mut y_offsets: Vec<u32> = Vec::with_capacity(crops.len());
    let mut y = 0u32;
    for crop in &crops {
        combined
            .copy_from(crop, 0, y)
            .with_context(|| format!("拼接 OCR 块到 y={}", y))?;
        y_offsets.push(y);
        y += crop.height() + BLOCK_GAP;
    }
    let combined = DynamicImage::ImageRgba8(combined);

    let lines = recognize_lines(engine, &combined)?;
    log::debug!(
        "批量 OCR 拼接: blocks={} combined={}x{} lines={} 耗时={}ms",
        blocks.len(),
        max_width,
        total_height,
        lines.len(),
        started.elapsed().as_millis()
    );

    let mut block_lines: Vec<Vec<OcrLine>> = vec![Vec::new(); blocks.len()];
    for mut line in lines {
        let line_center_y = line.bbox.y + line.bbox.height as i32 / 2;
        for (i, &offset) in y_offsets.iter().enumerate() {
            let block_h = crops[i].height() as i32;
            let block_w = crops[i].width() as i32;
            if line_center_y >= offset as i32 && line_center_y < offset as i32 + block_h {
                if line.bbox.x < block_w {
                    line.bbox.y -= offset as i32;
                    block_lines[i].push(line);
                }
                break;
            }
        }
    }

    Ok(block_lines
        .into_iter()
        .map(|lines| merge_ocr_lines(lines, same_line_y_tolerance))
        .collect())
}
