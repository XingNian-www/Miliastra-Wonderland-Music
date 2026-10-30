//! Stateless per-frame chat recognition. Message identity belongs to the timeline.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Result, bail};
use image::DynamicImage;
use serde::Serialize;

use crate::config::{OcrConfig, RectConfig, TemplateConfig};
use crate::geometry::{Rect, crop_canvas};
use crate::privacy::redacted_chat_text;
use crate::runtime::ocr::{OcrImageBlock, OcrPriority, OcrRuntimeHandle, batch_recognize_blocks};
use crate::ui::template::{TemplateHit, dedupe_hits, find_color_template_hit_groups};

const CHAT_SCAN_RESULT_LOG_TARGET: &str = "chat_scan_result";

/// 上一次写入 info 的扫描结果，用于抑制内容未变化时的重复记录。
static LAST_LOGGED_SCAN_RESULT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 记录本次扫描结果并返回是否需要以 info 级别输出。
///
/// 聊天扫描是每秒一次级别的循环，逐次打印整段气泡文本会让文件日志按扫描次数膨胀；
/// 只有内容真的变化时才值得留一条可追溯记录。
fn remember_scan_result_for_log(scan_result: &str) -> bool {
    let mut last = LAST_LOGGED_SCAN_RESULT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last.as_deref() == Some(scan_result) {
        return false;
    }
    *last = Some(scan_result.to_owned());
    true
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TemplateArgs {
    blue_template: Option<PathBuf>,
    yellow_template: Option<PathBuf>,
    pink_template: Option<PathBuf>,
    marker_threshold: Option<f32>,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedTemplateArgs {
    pub(crate) blue_template: PathBuf,
    pub(crate) yellow_template: PathBuf,
    pub(crate) pink_template: PathBuf,
    pub(crate) marker_threshold: f32,
    pub(crate) marker_search_width: u32,
    pub(crate) marker_dedupe_x: i32,
    pub(crate) marker_dedupe_y: i32,
    pub(crate) text_left_gap: i32,
    pub(crate) block_top_padding: i32,
    pub(crate) block_bottom_padding: i32,
    pub(crate) max_block_height: i32,
    pub(crate) next_marker_min_gap: i32,
    pub(crate) right_padding: i32,
    pub(crate) same_line_y_tolerance: i32,
    pub(crate) batch_recognize: bool,
}

impl TemplateArgs {
    pub(crate) fn resolve(
        &self,
        templates: &TemplateConfig,
        ocr: &OcrConfig,
    ) -> ResolvedTemplateArgs {
        ResolvedTemplateArgs {
            blue_template: self
                .blue_template
                .clone()
                .unwrap_or_else(|| templates.blue_marker.clone()),
            yellow_template: self
                .yellow_template
                .clone()
                .unwrap_or_else(|| templates.yellow_marker.clone()),
            pink_template: self
                .pink_template
                .clone()
                .unwrap_or_else(|| templates.pink_marker.clone()),
            marker_threshold: self.marker_threshold.unwrap_or(templates.marker_threshold),
            marker_search_width: ocr.marker_search_width,
            marker_dedupe_x: ocr.marker_dedupe_x.max(ocr.marker_dedupe_min_x),
            marker_dedupe_y: ocr.marker_dedupe_y.max(ocr.marker_dedupe_min_y),
            text_left_gap: ocr.text_left_gap,
            block_top_padding: ocr.block_top_padding,
            block_bottom_padding: ocr.block_bottom_padding,
            max_block_height: ocr.max_block_height,
            next_marker_min_gap: ocr.next_marker_min_gap,
            right_padding: ocr.right_padding,
            same_line_y_tolerance: ocr.same_line_y_tolerance,
            batch_recognize: ocr.batch_recognize,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RecognizedChatMessage {
    pub(crate) text: String,
    pub(crate) sender: Option<String>,
    pub(crate) raw_sender: Option<String>,
    pub(crate) message_type: String,
    pub(crate) block: Rect,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ChatMessage {
    pub(crate) message_type: String,
    pub(crate) block: Rect,
    /// 纯正文：昵称已由扫描层剥到 `sender`。
    pub(crate) text: String,
    /// 扫描层切出来的昵称；一级大厅 OCR 与外部源之外可能为 None。
    pub(crate) sender: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChatScanTelemetry {
    pub(crate) marker_count: usize,
    pub(crate) lines: Vec<String>,
    pub(crate) marker_ms: u128,
    pub(crate) ocr_ms: u128,
    pub(crate) total_ms: u128,
    pub(crate) scope: &'static str,
}

pub(crate) trait ChatScanTelemetrySink {
    fn publish_chat_scan(&self, telemetry: ChatScanTelemetry);
}

pub(crate) struct PreparedChatScan {
    chat: DynamicImage,
    markers: Vec<TemplateHit>,
    blocks: Vec<(TemplateHit, Rect)>,
    crop_ms: u128,
    marker_ms: u128,
    block_ms: u128,
    prepare_ms: u128,
    started: Instant,
}

#[derive(Clone, Copy, Debug)]
struct ChatMarkerCounts {
    blue: usize,
    yellow: usize,
    pink: usize,
}

pub(crate) fn prepare_chat_scan(
    image: &DynamicImage,
    templates: &ResolvedTemplateArgs,
    chat_rect: Rect,
) -> Result<PreparedChatScan> {
    prepare_chat_scan_with_markers(image, templates, chat_rect, None)
}

pub(crate) fn prepare_chat_scan_with_markers(
    image: &DynamicImage,
    templates: &ResolvedTemplateArgs,
    chat_rect: Rect,
    markers: Option<Vec<TemplateHit>>,
) -> Result<PreparedChatScan> {
    let started = Instant::now();
    let crop_started = Instant::now();
    let chat = crop_canvas(image, chat_rect)?;
    let crop_ms = elapsed_ms(crop_started);
    let marker_started = Instant::now();
    let markers = match markers {
        Some(markers) => markers,
        None => find_chat_markers(&chat, templates)?,
    };
    let marker_ms = elapsed_ms(marker_started);
    let block_started = Instant::now();
    let blocks: Vec<(TemplateHit, Rect)> = markers
        .iter()
        .map(|marker| {
            let block = make_message_block(marker, &markers, chat_rect, templates)?;
            Ok((marker.clone(), block))
        })
        .collect::<Result<_>>()?;
    let block_ms = elapsed_ms(block_started);

    Ok(PreparedChatScan {
        chat,
        markers,
        blocks,
        crop_ms,
        marker_ms,
        block_ms,
        prepare_ms: elapsed_ms(started),
        started,
    })
}

pub(crate) fn recognize_prepared_chat(
    ocr: &OcrRuntimeHandle,
    priority: OcrPriority,
    templates: &ResolvedTemplateArgs,
    prepared: PreparedChatScan,
    telemetry_sink: Option<&dyn ChatScanTelemetrySink>,
) -> Result<Vec<RecognizedChatMessage>> {
    let mut messages = Vec::new();
    let ocr_started = Instant::now();
    if templates.batch_recognize {
        let block_rects = prepared
            .blocks
            .iter()
            .enumerate()
            .map(|(id, (_, rect))| OcrImageBlock { id, rect: *rect })
            .collect::<Vec<_>>();
        let texts = batch_recognize_blocks(
            ocr,
            &prepared.chat,
            &block_rects,
            templates.same_line_y_tolerance,
            priority,
        )?;
        for text in texts {
            let (marker, block) = &prepared.blocks[text.id];
            let message_type = marker_type(marker).to_string();
            let (sender, raw_sender, body) = split_ocr_sender(text.text, &message_type);
            messages.push(RecognizedChatMessage {
                sender,
                raw_sender,
                message_type,
                block: *block,
                text: body,
            });
        }
    } else {
        for (marker, block) in &prepared.blocks {
            let crop = crop_canvas(&prepared.chat, *block)?;
            let text = ocr.merged_text(crop, templates.same_line_y_tolerance, priority)?;
            let message_type = marker_type(marker).to_string();
            let (sender, raw_sender, body) = split_ocr_sender(text, &message_type);
            messages.push(RecognizedChatMessage {
                sender,
                raw_sender,
                message_type,
                block: *block,
                text: body,
            });
        }
    }
    let ocr_ms = elapsed_ms(ocr_started);
    let total_ms = elapsed_ms(prepared.started);
    if let Some(sink) = telemetry_sink {
        sink.publish_chat_scan(ChatScanTelemetry {
            marker_count: prepared.markers.len(),
            lines: messages
                .iter()
                .map(|message| {
                    format!(
                        "[{}] {}",
                        message.message_type,
                        redacted_chat_text(&message.text)
                    )
                })
                .collect(),
            marker_ms: prepared.marker_ms,
            ocr_ms,
            total_ms,
            scope: "一级聊天",
        });
    }
    // 扫描频率是每秒一次级别，逐次打印整段气泡文本会让日志按扫描次数膨胀。
    //
    // 级别按频繁度定：这是**内部 OCR 输出**，不是玩家能感知的事件（那种由
    // 上层的「收到命令 / 命令执行完成」记录）。大厅只要有人说话内容就会变，
    // 长期留在 info 里会把真正的玩家事件淹掉，因此两条都归 debug；
    // 内容有变化与无变化靠文案区分，不再靠级别区分。
    let scan_result = format_scan_result(&messages);
    if remember_scan_result_for_log(&scan_result) {
        log::debug!(target: CHAT_SCAN_RESULT_LOG_TARGET,
            "聊天扫描结果有变化：标记 {} 个，消息 {} 条，内容为 {scan_result}",
            prepared.markers.len(),
            messages.len()
        );
    } else {
        log::debug!(target: CHAT_SCAN_RESULT_LOG_TARGET,
            "聊天扫描结果与上次相同：标记 {} 个，消息 {} 条",
            prepared.markers.len(),
            messages.len()
        );
    }
    log::info!(target: "timing",
        "聊天扫描耗时: total={}ms prepare={}ms crop={}ms marker={}ms block={}ms ocr={}ms markers={} messages={}",
        total_ms,
        prepared.prepare_ms,
        prepared.crop_ms,
        prepared.marker_ms,
        prepared.block_ms,
        ocr_ms,
        prepared.markers.len(),
        messages.len()
    );
    Ok(messages)
}

fn format_scan_result(messages: &[RecognizedChatMessage]) -> String {
    if messages.is_empty() {
        return "[]".to_string();
    }
    messages
        .iter()
        .map(|message| {
            let text = compact_log_text(redacted_chat_text(&message.text));
            format!("[{}] {}", message.message_type, text)
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn compact_log_text(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        "(空)".to_string()
    } else {
        text
    }
}

fn marker_type(hit: &TemplateHit) -> &str {
    &hit.kind
}

fn find_chat_markers(
    chat: &DynamicImage,
    templates: &ResolvedTemplateArgs,
) -> Result<Vec<TemplateHit>> {
    let search_rect = Some(Rect::new(
        0,
        0,
        templates.marker_search_width.min(chat.width()),
        chat.height(),
    ));
    let template_paths = [
        templates.blue_template.as_path(),
        templates.yellow_template.as_path(),
        templates.pink_template.as_path(),
    ];
    let groups = find_color_template_hit_groups(
        chat,
        search_rect,
        &template_paths,
        templates.marker_threshold,
    )?;
    let mut markers = Vec::new();
    // blue = 大厅成员消息；pink = 好友私聊；yellow = **仅用于 OCR 分块**的其它 marker，
    // 不是大厅命令来源（金色/黄色标签不产生命令）。
    for (mut hits, marker_type) in groups.into_iter().zip(["blue", "yellow", "pink"]) {
        for hit in &mut hits {
            hit.kind = marker_type.to_string();
        }
        markers.extend(hits);
    }
    Ok(dedupe_chat_marker_hits(
        markers,
        templates.marker_dedupe_x,
        templates.marker_dedupe_y,
    ))
}

fn chat_marker_counts(markers: &[TemplateHit]) -> ChatMarkerCounts {
    let mut counts = ChatMarkerCounts {
        blue: 0,
        yellow: 0,
        pink: 0,
    };
    for marker in markers {
        match marker.kind.as_str() {
            "blue" => counts.blue += 1,
            "yellow" => counts.yellow += 1,
            "pink" => counts.pink += 1,
            _ => {}
        }
    }
    counts
}

pub(crate) fn dedupe_chat_marker_hits(
    hits: Vec<TemplateHit>,
    tolerance_x: i32,
    tolerance_y: i32,
) -> Vec<TemplateHit> {
    let tolerance_x = tolerance_x.max(0);
    let tolerance_y = tolerance_y.max(0);
    let mut by_score = hits;
    by_score.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.y.cmp(&right.y))
            .then_with(|| left.x.cmp(&right.x))
    });
    dedupe_hits(by_score, tolerance_x, tolerance_y)
}

fn make_message_block(
    marker: &TemplateHit,
    markers: &[TemplateHit],
    chat_rect: Rect,
    templates: &ResolvedTemplateArgs,
) -> Result<Rect> {
    if chat_rect.width == 0
        || chat_rect.height == 0
        || chat_rect.width > i32::MAX as u32
        || chat_rect.height > i32::MAX as u32
    {
        bail!("聊天 OCR 区域宽高必须在 1 到 {} 之间", i32::MAX);
    }
    let chat_width = i64::from(chat_rect.width);
    let chat_height = i64::from(chat_rect.height);
    // 中间计算使用 i64；即使调用方绕过配置校验，极端参数也不能回绕或生成越界裁剪。
    let start_y = (i64::from(marker.y) - i64::from(templates.block_top_padding.max(0)))
        .clamp(0, chat_height - 1);
    let next_marker = next_marker(marker, markers, templates.next_marker_min_gap);
    let max_end_y = (start_y + i64::from(templates.max_block_height.max(1))).min(chat_height);
    let boundary_end_y = next_marker
        .map(|hit| i64::from(hit.y) - i64::from(templates.block_bottom_padding.max(0)))
        .unwrap_or(max_end_y);
    let end_y = boundary_end_y
        .min(max_end_y)
        .clamp(start_y + 1, chat_height);
    let text_x =
        (i64::from(marker.x) + i64::from(marker.width) + i64::from(templates.text_left_gap.max(0)))
            .clamp(0, chat_width - 1);
    let available_width = chat_width - text_x;
    let width =
        (available_width - i64::from(templates.right_padding.max(0))).clamp(1, available_width);
    Ok(Rect::new(
        text_x as i32,
        start_y as i32,
        width as u32,
        (end_y - start_y) as u32,
    ))
}

fn next_marker<'a>(
    marker: &TemplateHit,
    markers: &'a [TemplateHit],
    next_marker_min_gap: i32,
) -> Option<&'a TemplateHit> {
    let min_y = i64::from(marker.y)
        + i64::from(next_marker_min_gap.max((marker.height as f32 * 0.6).floor() as i32));
    markers
        .iter()
        .filter(|candidate| i64::from(candidate.y) >= min_y)
        .min_by_key(|candidate| candidate.y)
}

pub(crate) fn scan_chat_markers(
    image: &DynamicImage,
    templates: &ResolvedTemplateArgs,
    chat_rect: RectConfig,
) -> Result<Vec<TemplateHit>> {
    let chat = crop_canvas(image, chat_rect.into())?;
    find_chat_markers(&chat, templates)
}

pub(crate) fn count_chat_markers(
    image: &DynamicImage,
    templates: &ResolvedTemplateArgs,
    chat_rect: RectConfig,
) -> Result<(usize, usize, usize)> {
    let markers = scan_chat_markers(image, templates, chat_rect)?;
    let counts = chat_marker_counts(&markers);
    Ok((counts.blue, counts.yellow, counts.pink))
}

pub(crate) fn count_scanned_chat_markers(markers: &[TemplateHit]) -> (usize, usize, usize) {
    let counts = chat_marker_counts(markers);
    (counts.blue, counts.yellow, counts.pink)
}

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(x: i32, y: i32, width: u32, height: u32) -> TemplateHit {
        TemplateHit {
            kind: "blue".into(),
            x,
            y,
            width,
            height,
            score: 1.0,
        }
    }

    fn templates() -> ResolvedTemplateArgs {
        TemplateArgs::default().resolve(&TemplateConfig::default(), &OcrConfig::default())
    }

    #[test]
    fn scan_result_log_recorded_only_when_content_changes() {
        // 内容未变时不重复写 info，变化后重新记录一次。
        assert!(remember_scan_result_for_log("scan-dedup-a"));
        assert!(!remember_scan_result_for_log("scan-dedup-a"));
        assert!(remember_scan_result_for_log("scan-dedup-b"));
        assert!(!remember_scan_result_for_log("scan-dedup-b"));
        assert!(remember_scan_result_for_log(""));
        assert!(!remember_scan_result_for_log(""));
    }

    #[test]
    fn message_block_preserves_default_geometry() {
        let markers = [marker(8, 10, 20, 19), marker(8, 45, 20, 19)];
        let block = make_message_block(
            &markers[0],
            &markers,
            Rect::new(0, 0, 416, 143),
            &templates(),
        )
        .unwrap();
        assert_eq!(
            (block.x, block.y, block.width, block.height),
            (36, 8, 376, 35)
        );
    }

    #[test]
    fn negative_right_padding_cannot_extend_a_crop_past_the_chat_edge() {
        let mut templates = templates();
        templates.right_padding = -1;
        let image = DynamicImage::new_rgb8(416, 143);
        let prepared = prepare_chat_scan_with_markers(
            &image,
            &templates,
            Rect::new(0, 0, 416, 143),
            Some(vec![marker(8, 10, 20, 19)]),
        )
        .unwrap();
        let block = prepared.blocks[0].1;
        assert_eq!((block.x, block.width), (36, 380));
        crop_canvas(&prepared.chat, block).expect("bounded crop despite negative padding");
    }

    #[test]
    fn extreme_message_block_controls_produce_bounded_crops() {
        let image = DynamicImage::new_rgb8(416, 143);
        for extreme in [i32::MIN, i32::MAX] {
            let mut templates = templates();
            templates.block_top_padding = extreme;
            templates.block_bottom_padding = extreme;
            templates.text_left_gap = extreme;
            templates.max_block_height = extreme;
            templates.right_padding = extreme;
            templates.next_marker_min_gap = extreme;
            let prepared = prepare_chat_scan_with_markers(
                &image,
                &templates,
                Rect::new(0, 0, 416, 143),
                Some(vec![marker(8, 10, 20, 19), marker(8, 45, 20, 19)]),
            )
            .unwrap();
            for (_, block) in prepared.blocks {
                assert!(block.width > 0 && block.height > 0);
                crop_canvas(&prepared.chat, block).expect("extreme controls must stay in bounds");
            }
        }
    }

    #[test]
    fn extreme_marker_coordinates_produce_bounded_crops() {
        let image = DynamicImage::new_rgb8(416, 143);
        for coordinate in [i32::MIN, i32::MAX] {
            let hit = marker(coordinate, coordinate, u32::MAX, u32::MAX);
            let block = make_message_block(
                &hit,
                std::slice::from_ref(&hit),
                Rect::new(0, 0, 416, 143),
                &templates(),
            )
            .unwrap();
            crop_canvas(&image, block).expect("extreme marker must stay in bounds");
        }
    }

    #[test]
    fn next_marker_threshold_does_not_wrap_past_i32_max() {
        let hit = marker(8, i32::MAX - 1, 20, 19);
        let markers = [marker(8, i32::MAX, 20, 19)];
        assert!(next_marker(&hit, &markers, i32::MAX).is_none());
    }

    #[test]
    fn message_block_rejects_empty_or_unrepresentable_canvas() {
        let hit = marker(8, 10, 20, 19);
        for rect in [
            Rect::new(0, 0, 0, 143),
            Rect::new(0, 0, 416, 0),
            Rect::new(0, 0, u32::MAX, 143),
        ] {
            assert!(
                make_message_block(&hit, std::slice::from_ref(&hit), rect, &templates()).is_err()
            );
        }
    }
    #[test]
    fn marker_geometry_defaults_and_configured_overrides_resolve_once() {
        let default = templates();
        assert_eq!(default.marker_search_width, 60);
        assert_eq!((default.marker_dedupe_x, default.marker_dedupe_y), (22, 14));
        let ocr = OcrConfig {
            marker_search_width: 72,
            marker_dedupe_min_x: 3,
            marker_dedupe_min_y: 4,
            marker_dedupe_x: 5,
            marker_dedupe_y: 6,
            ..OcrConfig::default()
        };
        let custom = TemplateArgs::default().resolve(&TemplateConfig::default(), &ocr);
        assert_eq!(custom.marker_search_width, 72);
        assert_eq!((custom.marker_dedupe_x, custom.marker_dedupe_y), (5, 6));
        let hits = vec![marker(0, 0, 10, 10), marker(10, 0, 10, 10)];
        assert_eq!(dedupe_chat_marker_hits(hits.clone(), 22, 14).len(), 1);
        assert_eq!(dedupe_chat_marker_hits(hits, 5, 6).len(), 2);
    }
}

/// 把 OCR 出的一行切成「发送者 + 纯正文」。
///
/// 一级大厅的气泡把「昵称：正文」渲染成一整块，OCR 拿到的是混合文本。
/// 在这里切一次，下游（命令、投票、AI 触发、锁文本）都能用统一的纯正文语义，
/// 不必各自再按分隔符猜。
///
/// 两类文本**不切**：
/// - 结构化点歌元数据（`歌曲名…`）没有发送者，切了会把「歌曲名」当成昵称；
/// - 切不出昵称时原样保留，交给下游按 Unknown 处理。
fn split_ocr_sender(text: String, message_type: &str) -> (Option<String>, Option<String>, String) {
    if text.trim_start().starts_with("歌曲名") {
        return (None, None, text);
    }
    let Some((sender, body)) = crate::text::split_sender_line(&text, message_type) else {
        return (None, None, text);
    };
    if sender.trim().is_empty() || sender.starts_with(['@', '＠', '#', '＃']) {
        return (None, None, text);
    }
    // 分隔符本身属于昵称与正文之间，正文从分隔符之后开始。
    let body = body
        .trim_start()
        .trim_start_matches(['：', ':', ' ', '\t', ']', '】'])
        .trim()
        .to_string();
    (Some(sender.clone()), Some(sender), body)
}

#[cfg(test)]
mod split_ocr_sender_tests {
    use super::split_ocr_sender;

    /// 一级大厅 OCR 的「昵称：正文」必须在扫描层就切开。
    #[test]
    fn splits_the_sender_out_of_the_primary_hall_line() {
        let (sender, raw, body) = split_ocr_sender("甲：@状态".to_string(), "blue");
        assert_eq!(sender.as_deref(), Some("甲"));
        assert_eq!(raw.as_deref(), Some("甲"));
        assert_eq!(body, "@状态");
    }

    #[test]
    fn splits_bracketed_sender_for_pink_lines() {
        let (sender, _, body) = split_ocr_sender("[好友]：@同意".to_string(), "pink");
        assert_eq!(sender.as_deref(), Some("好友"));
        assert_eq!(body, "@同意");
    }

    /// 结构化点歌元数据没有发送者：把「歌曲名」当昵称会把整条请求弄坏。
    #[test]
    fn never_treats_the_song_metadata_label_as_a_sender() {
        let (sender, _, body) = split_ocr_sender("歌曲名：晴天".to_string(), "blue");
        assert_eq!(sender, None);
        assert_eq!(body, "歌曲名：晴天");
    }

    /// 切不出昵称时原样保留，交给下游按 Unknown 处理。
    #[test]
    fn keeps_lines_without_a_separator_intact() {
        let (sender, _, body) = split_ocr_sender("普通聊天".to_string(), "blue");
        assert_eq!(sender, None);
        assert_eq!(body, "普通聊天");
    }

    /// 正文本身以命令前缀开头时，不能把命令当成昵称切掉。
    #[test]
    fn keeps_a_leading_command_prefix_in_the_body() {
        let (sender, _, body) = split_ocr_sender("@状态".to_string(), "blue");
        assert_eq!(sender, None);
        assert_eq!(body, "@状态");
    }
}
