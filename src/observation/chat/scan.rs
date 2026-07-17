use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use image::DynamicImage;
use serde::Serialize;

use crate::config::{OcrConfig, RectConfig, TemplateConfig};
use crate::privacy::redacted_chat_text;
use crate::runtime::ocr::{OcrImageBlock, OcrPriority, OcrRuntimeHandle, batch_recognize_blocks};
use crate::ui::change_detection::{ChangeFingerprint, rect_chat_change_fingerprint};
use crate::ui::geometry::{Rect, clamp_i32, crop_canvas};
use crate::ui::template::{TemplateHit, dedupe_hits, find_color_template_hits};

const CHAT_MARKER_SEARCH_WIDTH: u32 = 60;
const CHAT_SCAN_RESULT_LOG_TARGET: &str = "chat_scan_result";

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
            marker_dedupe_x: ocr.marker_dedupe_x,
            marker_dedupe_y: ocr.marker_dedupe_y,
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
pub(crate) struct ChatMessage {
    pub(crate) message_type: String,
    pub(crate) block: Rect,
    pub(crate) text: String,
    #[serde(skip)]
    pub(crate) visual: ChangeFingerprint,
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
    let started = Instant::now();
    let crop_started = Instant::now();
    let chat = crop_canvas(image, chat_rect)?;
    let crop_ms = elapsed_ms(crop_started);
    let marker_started = Instant::now();
    let markers = find_chat_markers(&chat, templates)?;
    let marker_ms = elapsed_ms(marker_started);
    let block_started = Instant::now();
    let blocks: Vec<(TemplateHit, Rect)> = markers
        .iter()
        .map(|marker| {
            let block = make_message_block(marker, &markers, chat_rect, templates);
            (marker.clone(), block)
        })
        .collect();
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
) -> Result<Vec<ChatMessage>> {
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
            messages.push(ChatMessage {
                message_type: marker_type(marker).to_string(),
                block: *block,
                text: text.text,
                visual: rect_chat_change_fingerprint(&prepared.chat, *block)?,
            });
        }
    } else {
        for (marker, block) in &prepared.blocks {
            let crop = crop_canvas(&prepared.chat, *block)?;
            let text = ocr.merged_text(crop, templates.same_line_y_tolerance, priority)?;
            messages.push(ChatMessage {
                message_type: marker_type(marker).to_string(),
                block: *block,
                text,
                visual: rect_chat_change_fingerprint(&prepared.chat, *block)?,
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
    log::info!(target: CHAT_SCAN_RESULT_LOG_TARGET,
        "聊天扫描结果: markers={} messages={} {}",
        prepared.markers.len(),
        messages.len(),
        format_scan_result(&messages)
    );
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

fn format_scan_result(messages: &[ChatMessage]) -> String {
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
        CHAT_MARKER_SEARCH_WIDTH.min(chat.width()),
        chat.height(),
    ));
    let mut markers = Vec::new();
    markers.extend(find_markers(
        chat,
        search_rect,
        &templates.blue_template,
        "blue",
        templates.marker_threshold,
    )?);
    markers.extend(find_markers(
        chat,
        search_rect,
        &templates.yellow_template,
        "yellow",
        templates.marker_threshold,
    )?);
    markers.extend(find_markers(
        chat,
        search_rect,
        &templates.pink_template,
        "pink",
        templates.marker_threshold,
    )?);
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

fn dedupe_chat_marker_hits(
    hits: Vec<TemplateHit>,
    tolerance_x: i32,
    tolerance_y: i32,
) -> Vec<TemplateHit> {
    let tolerance_x = tolerance_x.max(22);
    let tolerance_y = tolerance_y.max(14);
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

fn find_markers(
    image: &DynamicImage,
    search_rect: Option<Rect>,
    template: &Path,
    marker_type: &str,
    threshold: f32,
) -> Result<Vec<TemplateHit>> {
    let mut hits = find_color_template_hits(image, search_rect, template, threshold)?;
    for hit in &mut hits {
        hit.kind = marker_type.to_string();
    }
    Ok(hits)
}

fn make_message_block(
    marker: &TemplateHit,
    markers: &[TemplateHit],
    chat_rect: Rect,
    templates: &ResolvedTemplateArgs,
) -> Rect {
    let start_y = clamp_i32(
        marker.y - templates.block_top_padding,
        0,
        chat_rect.height as i32 - 1,
    );
    let next_marker = next_marker(marker, markers, templates.next_marker_min_gap);
    let max_end_y = clamp_i32(
        start_y + templates.max_block_height,
        start_y + 1,
        chat_rect.height as i32,
    );
    let boundary_end_y = next_marker
        .map(|hit| hit.y - templates.block_bottom_padding)
        .unwrap_or(max_end_y);
    let end_y = clamp_i32(
        boundary_end_y.min(max_end_y),
        start_y + 1,
        chat_rect.height as i32,
    );
    let text_x = clamp_i32(
        marker.x + marker.width as i32 + templates.text_left_gap,
        0,
        chat_rect.width as i32 - 1,
    );
    let width = clamp_i32(
        chat_rect.width as i32 - text_x - templates.right_padding,
        1,
        chat_rect.width as i32,
    ) as u32;
    Rect::new(text_x, start_y, width, (end_y - start_y) as u32)
}

fn next_marker<'a>(
    marker: &TemplateHit,
    markers: &'a [TemplateHit],
    next_marker_min_gap: i32,
) -> Option<&'a TemplateHit> {
    let min_y = marker.y + next_marker_min_gap.max((marker.height as f32 * 0.6).floor() as i32);
    markers
        .iter()
        .filter(|candidate| candidate.y >= min_y)
        .min_by_key(|candidate| candidate.y)
}

pub(crate) fn count_chat_markers(
    image: &DynamicImage,
    templates: &ResolvedTemplateArgs,
    chat_rect: RectConfig,
) -> Result<(usize, usize, usize)> {
    let chat = crop_canvas(image, chat_rect.into())?;
    let markers = find_chat_markers(&chat, templates)?;
    let counts = chat_marker_counts(&markers);
    Ok((counts.blue, counts.yellow, counts.pink))
}

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}
