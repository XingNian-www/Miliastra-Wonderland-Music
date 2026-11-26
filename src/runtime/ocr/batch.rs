use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use image::{DynamicImage, GenericImage, ImageBuffer, Rgba};

use super::{OcrLine, OcrPriority, OcrRuntimeHandle, merge_ocr_lines};
use crate::geometry::Rect;

/// 块间灰色分隔带的高度。
///
/// 分隔带的作用是**物理隔断**：让每块文本在拼接图上彼此不相邻，OCR 的检测框
/// 才不会被诱导跨越两块。12px 对中文字高（约 20~40px）太窄——检测器会把
/// 相邻两块的相邻行并成一个框，中心点归属就成了掷硬币。
///
/// 取 48px（约一个半行高）后，两块文本之间留有明显的空白带，跨块并框显著减少。
/// 代价是拼接图更高，但 OCR 一次调用的成本远低于多出来的几十像素。
const BLOCK_GAP: u32 = 48;

/// 分隔带填充色。
///
/// 用**浅灰**而不是纯白/纯黑：它与深色气泡背景、浅色气泡背景都有明显对比，
/// 不会像本体颜色那样把分隔带「读成」某一块的延续。
///
/// 注意：分隔带**不参与**归属判定（见 `center_owner`）。判定只看几何区间，
/// 所以这里用什么颜色都不会因为 JPG 有损压缩的振铃/色偏而误判——这正是
/// 「按灰值精确比对」那种做法在有损样本上不可靠的原因。
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

/// 单张拼接图的高度上限。
///
/// 拼接图过高会让 OCR 检测器在巨大的画布上搜索，既慢又容易把远处两行并框；
/// 也更容易撞上引擎自身的输入尺寸限制。超过上限就分片、逐片识别，
/// 分片之间仍各自带分隔带（隔断性质不变）。
const MAX_COMBINED_HEIGHT: u32 = 4000;

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

    // 先按高度预算分片：每片单独拼接、单独 OCR，最后按原顺序拼回。
    // 顺序必须保持——调用方按下标取结果。
    let max_width_probe = blocks
        .iter()
        .map(|block| block.rect.height)
        .max()
        .unwrap_or(0);
    let per_block_budget = MAX_COMBINED_HEIGHT
        .checked_div(1)
        .unwrap_or(MAX_COMBINED_HEIGHT)
        .max(max_width_probe)
        .max(1);
    let mut chunks: Vec<&[OcrImageBlock<Id>]> = Vec::new();
    let mut start = 0usize;
    let mut height = 0u32;
    for (index, block) in blocks.iter().enumerate() {
        let block_height = block.rect.height.max(1);
        let with_gap = block_height + if index > start { BLOCK_GAP } else { 0 };
        if index > start && height.saturating_add(with_gap) > per_block_budget {
            chunks.push(&blocks[start..index]);
            start = index;
            height = block_height;
        } else {
            height = height.saturating_add(with_gap);
        }
    }
    chunks.push(&blocks[start..]);
    if chunks.len() > 1 {
        let mut out = Vec::with_capacity(blocks.len());
        for chunk in chunks {
            out.extend(batch_recognize_chunk(
                ocr,
                chat,
                chunk,
                same_line_y_tolerance,
                priority,
            )?);
        }
        return Ok(out);
    }
    batch_recognize_chunk(ocr, chat, blocks, same_line_y_tolerance, priority)
}

fn batch_recognize_chunk<Id: Clone>(
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
        // DB 检测器会扩展检测框（尤其是 unclip_ratio=2.0），有效文本可能跨越区块边缘或灰色分隔线；使用中心点分配即可，无需拒绝消息。
        let Some(owner) = center_owner(line.bbox, &y_offsets, &crops)? else {
            // 中心点落在分隔线中属于拼接噪声，不是消息。
            continue;
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

fn center_owner(bbox: Rect, y_offsets: &[u32], crops: &[DynamicImage]) -> Result<Option<usize>> {
    let center_x = bbox.x + bbox.width as i32 / 2;
    let center_y = bbox.y + bbox.height as i32 / 2;
    let mut owner = None;
    for (i, &offset) in y_offsets.iter().enumerate() {
        let block_h = crops[i].height() as i32;
        let block_w = crops[i].width() as i32;
        if center_y >= offset as i32
            && center_y < offset as i32 + block_h
            && center_x >= 0
            && center_x < block_w
            && owner.replace(i).is_some()
        {
            return Err(anyhow!("OCR 识别框同时归属于多个标识图块"));
        }
    }
    Ok(owner)
}

#[cfg(test)]
mod tests {
    /// 真实 MNN 验证：同一批二级聊天块，**分块串行** 与 **拼接批量** 必须给出同样的文本。
    ///
    /// 两个样本分别覆盖两种压缩：无损 PNG 与有损 JPG。JPG 会在文本边缘产生振铃，
    /// 若归属判定依赖「分隔带灰值是否精确」，有损样本上必然误判——所以判定只看
    /// 几何区间，分隔带只负责物理隔断。
    #[test]
    #[ignore = "requires MNN models + real screenshots"]
    fn real_batch_matches_per_block_on_png_and_jpg() {
        // 只用**入库**的 fixture：纯截图、无标注、不含任何真实玩家信息。
        // PNG 那一张是无损的，用来确认「批量识别 == 逐块识别」；
        // JPG 那一张是有损的，用来确认「有损压缩不会改变归属结果」。
        for path in [
            "tests/fixtures/ui/secondary-with-hall-row-1920x1080.png",
            "tests/fixtures/ui/secondary-chat-scrolled-1920x1080.jpg",
        ] {
            let Ok(image) = image::open(path) else {
                continue;
            };
            let device = match crate::runtime::ocr::ProductionOcrDevice::new(
                crate::runtime::ocr::OcrArgs::default()
                    .resolve(&crate::config::bundled_config().ocr),
            ) {
                Ok(device) => device,
                Err(error) => {
                    println!("skip {path}: {error:#}");
                    continue;
                }
            };
            let runtime = crate::runtime::ocr::OcrRuntime::start_configured(
                device,
                crate::runtime::ocr::OcrRuntimeConfig {
                    queue_capacity: 8,
                    request_timeout: std::time::Duration::from_secs(30),
                    shutdown_timeout: std::time::Duration::from_secs(5),
                },
            )
            .expect("start ocr runtime");
            let handle = runtime.handle();

            // 取若干真实气泡/标题区域作为块。
            let rects = [
                crate::geometry::Rect::new(410, 126, 313, 138),
                crate::geometry::Rect::new(410, 400, 256, 65),
                crate::geometry::Rect::new(410, 660, 200, 60),
            ];
            let blocks = rects
                .iter()
                .enumerate()
                .map(|(id, rect)| OcrImageBlock { id, rect: *rect })
                .collect::<Vec<_>>();

            let batched =
                batch_recognize_blocks(&handle, &image, &blocks, 10, OcrPriority::ChatObservation)
                    .expect("batch");

            let mut per_block = Vec::new();
            for block in &blocks {
                let crop = image.crop_imm(
                    block.rect.x as u32,
                    block.rect.y as u32,
                    block.rect.width,
                    block.rect.height,
                );
                let text = handle
                    .merged_text(crop, 10, OcrPriority::ChatObservation)
                    .expect("per-block");
                per_block.push(text);
            }

            println!("\n=== {path}");
            for (index, text) in per_block.iter().enumerate() {
                let batched_text = &batched[index].text;
                let same = text.trim() == batched_text.trim();
                println!(
                    "  block{index}: per_block={text:?} batched={batched_text:?} {}",
                    if same { "SAME" } else { "*** DIFF ***" }
                );
                assert_eq!(
                    text.trim(),
                    batched_text.trim(),
                    "block{index} 拼接批量与逐块串行的识别结果必须一致（{path}）"
                );
            }
            runtime.shutdown().expect("shutdown");
        }
    }

    use super::*;
    use crate::runtime::ocr::{OcrDevice, OcrPriority, OcrRuntime};

    struct TaggedBlockDevice;

    impl OcrDevice for TaggedBlockDevice {
        fn recognize_lines(&mut self, _image: &DynamicImage) -> Result<Vec<OcrLine>> {
            // 两个识别框分别落在第一块与第二块内部。第二块的起点是
            // `块高(5) + 分隔带`，随 `BLOCK_GAP` 变化，这里按常量推算而不是写死，
            // 否则调整分隔带宽度就会让测试假红（它验的是归属，不是布局尺寸）。
            let second_top = 5 + BLOCK_GAP as i32;
            Ok(vec![
                OcrLine {
                    text: "上".to_string(),
                    confidence: 1.0,
                    bbox: Rect::new(0, 1, 5, 3),
                },
                OcrLine {
                    text: "下".to_string(),
                    confidence: 1.0,
                    bbox: Rect::new(0, second_top + 1, 5, 3),
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
                bbox: Rect::new(0, 37, 104, 27),
            }])
        }
    }

    #[test]
    fn combined_ocr_assigns_a_border_crossing_line_by_center() {
        let runtime = OcrRuntime::start(CrossingBlockDevice, 1).unwrap();
        let blocks = vec![
            OcrImageBlock {
                id: 1,
                rect: Rect::new(0, 0, 110, 55),
            },
            OcrImageBlock {
                id: 2,
                rect: Rect::new(0, 60, 110, 10),
            },
        ];

        let results = batch_recognize_blocks(
            &runtime.handle(),
            &DynamicImage::new_rgba8(120, 100),
            &blocks,
            2,
            OcrPriority::ChatObservation,
        )
        .unwrap();

        assert_eq!(results[0].text, "跨界");
        assert_eq!(results[1].text, "");
        runtime.shutdown().unwrap();
    }

    struct SeparatorNoiseDevice;

    impl OcrDevice for SeparatorNoiseDevice {
        fn recognize_lines(&mut self, _image: &DynamicImage) -> Result<Vec<OcrLine>> {
            Ok(vec![OcrLine {
                text: "间隔噪声".to_string(),
                confidence: 1.0,
                bbox: Rect::new(0, 56, 104, 10),
            }])
        }
    }

    #[test]
    fn combined_ocr_ignores_lines_centered_in_the_separator() {
        let runtime = OcrRuntime::start(SeparatorNoiseDevice, 1).unwrap();
        let blocks = vec![
            OcrImageBlock {
                id: 1,
                rect: Rect::new(0, 0, 110, 55),
            },
            OcrImageBlock {
                id: 2,
                rect: Rect::new(0, 60, 110, 10),
            },
        ];

        let results = batch_recognize_blocks(
            &runtime.handle(),
            &DynamicImage::new_rgba8(120, 100),
            &blocks,
            2,
            OcrPriority::ChatObservation,
        )
        .unwrap();

        assert_eq!(results[0].text, "");
        assert_eq!(results[1].text, "");
        runtime.shutdown().unwrap();
    }

    /// 固定截图用例里两块的高度，供拼接高度断言推算（随 `BLOCK_GAP` 变化）。
    const FIXED_BLOCK_HEIGHTS: [u32; 2] = [72, 70];

    struct FixedFixtureBlockDevice;

    impl OcrDevice for FixedFixtureBlockDevice {
        fn recognize_lines(&mut self, image: &DynamicImage) -> Result<Vec<OcrLine>> {
            // 高度 = 各块高度之和 + 分隔带 × (块数 - 1)。按常量推算，
            // 免得调分隔带宽度就把这条「归属正确」的测试变成假红。
            let expected: u32 = FIXED_BLOCK_HEIGHTS.iter().sum::<u32>()
                + BLOCK_GAP * (FIXED_BLOCK_HEIGHTS.len() as u32 - 1);
            assert_eq!((image.width(), image.height()), (480, expected));
            Ok(vec![
                OcrLine {
                    text: "昵称A".to_string(),
                    confidence: 1.0,
                    bbox: Rect::new(110, 20, 80, 24),
                },
                OcrLine {
                    // 落在第二块内部：起点 = 第一块高 + 分隔带（随 `BLOCK_GAP` 变化）。
                    text: "玩家丙".to_string(),
                    confidence: 1.0,
                    bbox: Rect::new(0, (FIXED_BLOCK_HEIGHTS[0] + BLOCK_GAP + 32) as i32, 100, 24),
                },
            ])
        }
    }

    #[test]
    fn fixed_secondary_chat_blocks_preserve_title_and_friend_row_ids() {
        if crate::test_support::corpus::skip_without_corpus(
            "fixed_secondary_chat_blocks_preserve_title_and_friend_row_ids",
        ) {
            return;
        }
        let image = image::open("tests/fixtures/ui/secondary-chat-scrolled-1920x1080.jpg")
            .expect("open fixed secondary-chat screenshot");
        let runtime = OcrRuntime::start(FixedFixtureBlockDevice, 1).unwrap();
        let blocks = vec![
            OcrImageBlock {
                id: "title",
                rect: Rect::new(600, 24, 480, 72),
            },
            OcrImageBlock {
                id: "friend-row",
                rect: Rect::new(80, 300, 170, 70),
            },
        ];

        let results = batch_recognize_blocks(
            &runtime.handle(),
            &image,
            &blocks,
            12,
            OcrPriority::UiConfirmation,
        )
        .expect("recognize fixed screenshot blocks");

        assert_eq!(
            results,
            [
                OcrBlockText {
                    id: "title",
                    text: "昵称A".to_string(),
                },
                OcrBlockText {
                    id: "friend-row",
                    text: "玩家丙".to_string(),
                },
            ]
        );
        runtime.shutdown().unwrap();
    }
}
