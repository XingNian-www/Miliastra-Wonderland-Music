use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use image::{DynamicImage, GenericImage, ImageBuffer, Rgba};

use super::{OcrLine, OcrPriority, OcrRuntimeHandle, merge_ocr_lines};
use crate::ui::geometry::Rect;

const BLOCK_GAP: u32 = 12;
const GAP_COLOR: Rgba<u8> = Rgba([180, 180, 180, 255]);

#[derive(Clone, Debug)]
pub(crate) struct OcrImageBlock<Id> {
    pub(crate) id: Id,
    pub(crate) rect: Rect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OcrBlockText<Id> {
    pub(crate) id: Id,
    pub(crate) text: String,
}

pub(crate) fn batch_recognize_blocks<Id: Clone>(
    ocr: &OcrRuntimeHandle,
    chat: &DynamicImage,
    blocks: &[OcrImageBlock<Id>],
    same_line_y_tolerance: i32,
    priority: OcrPriority,
) -> Result<Vec<OcrBlockText<Id>>> {
    if blocks.is_empty() {
        return Ok(Vec::new());
    }

    let started = Instant::now();

    let crops: Vec<DynamicImage> = blocks
        .iter()
        .map(|block| {
            let rect = block.rect;
            if rect.x < 0
                || rect.y < 0
                || rect.right() > chat.width() as i32
                || rect.bottom() > chat.height() as i32
            {
                return Err(anyhow!(
                    "批量 crop rect {},{},{},{} outside chat {}x{}",
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    chat.width(),
                    chat.height()
                ));
            }
            Ok(chat.crop_imm(rect.x as u32, rect.y as u32, rect.width, rect.height))
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

    let lines = ocr.recognize_lines(combined, priority)?;
    log::info!(target: "timing",
        "批量 OCR 拼接: blocks={} combined={}x{} lines={} 耗时={}ms",
        blocks.len(),
        max_width,
        total_height,
        lines.len(),
        started.elapsed().as_millis()
    );

    let mut block_lines: Vec<Vec<OcrLine>> = vec![Vec::new(); blocks.len()];
    for mut line in lines {
        let mut owner = None;
        for (i, &offset) in y_offsets.iter().enumerate() {
            let block_h = crops[i].height() as i32;
            let block_w = crops[i].width() as i32;
            let top = line.bbox.y;
            let bottom = line.bbox.bottom();
            let left = line.bbox.x;
            let right = line.bbox.right();
            if top >= offset as i32
                && bottom <= offset as i32 + block_h
                && left >= 0
                && right <= block_w
                && owner.replace(i).is_some()
            {
                return Err(anyhow!("OCR 识别框同时归属于多个标识图块"));
            }
        }
        let Some(owner) = owner else {
            return Err(anyhow!(
                "OCR 识别框无法唯一归属标识图块: {},{},{},{}",
                line.bbox.x,
                line.bbox.y,
                line.bbox.width,
                line.bbox.height
            ));
        };
        line.bbox.y -= y_offsets[owner] as i32;
        block_lines[owner].push(line);
    }

    Ok(blocks
        .iter()
        .zip(block_lines)
        .map(|(block, lines)| OcrBlockText {
            id: block.id.clone(),
            text: merge_ocr_lines(lines, same_line_y_tolerance),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::ocr::{OcrDevice, OcrPriority, OcrRuntime};

    struct TaggedBlockDevice;

    impl OcrDevice for TaggedBlockDevice {
        fn recognize_lines(&mut self, _image: &DynamicImage) -> Result<Vec<OcrLine>> {
            Ok(vec![
                OcrLine {
                    text: "上".to_string(),
                    confidence: 1.0,
                    bbox: Rect::new(0, 1, 5, 3),
                },
                OcrLine {
                    text: "下".to_string(),
                    confidence: 1.0,
                    bbox: Rect::new(0, 18, 5, 3),
                },
            ])
        }
    }

    #[test]
    fn tagged_blocks_keep_caller_ids_through_combined_ocr() {
        let runtime = OcrRuntime::start(TaggedBlockDevice, 2).unwrap();
        let blocks = vec![
            OcrImageBlock {
                id: "top",
                rect: Rect::new(0, 0, 10, 5),
            },
            OcrImageBlock {
                id: "bottom",
                rect: Rect::new(0, 10, 10, 5),
            },
        ];

        let results = batch_recognize_blocks(
            &runtime.handle(),
            &DynamicImage::new_rgba8(20, 20),
            &blocks,
            2,
            OcrPriority::ChatObservation,
        )
        .unwrap();

        assert_eq!(results[0].id, "top");
        assert_eq!(results[0].text, "上");
        assert_eq!(results[1].id, "bottom");
        assert_eq!(results[1].text, "下");
        runtime.shutdown().unwrap();
    }

    struct CrossingBlockDevice;

    impl OcrDevice for CrossingBlockDevice {
        fn recognize_lines(&mut self, _image: &DynamicImage) -> Result<Vec<OcrLine>> {
            Ok(vec![OcrLine {
                text: "跨界".to_string(),
                confidence: 1.0,
                bbox: Rect::new(0, 4, 5, 15),
            }])
        }
    }

    #[test]
    fn combined_ocr_rejects_a_line_without_one_unambiguous_block() {
        let runtime = OcrRuntime::start(CrossingBlockDevice, 1).unwrap();
        let blocks = vec![
            OcrImageBlock {
                id: 1,
                rect: Rect::new(0, 0, 10, 5),
            },
            OcrImageBlock {
                id: 2,
                rect: Rect::new(0, 10, 10, 5),
            },
        ];

        let error = batch_recognize_blocks(
            &runtime.handle(),
            &DynamicImage::new_rgba8(20, 20),
            &blocks,
            2,
            OcrPriority::ChatObservation,
        )
        .unwrap_err();

        assert!(error.to_string().contains("无法唯一归属标识图块"));
        runtime.shutdown().unwrap();
    }
}
