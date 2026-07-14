use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use image::DynamicImage;
use ocr_rs::OcrEngine;
use serde::Serialize;

use super::ResolvedTemplateArgs;
use super::chat_output::redacted_chat_text;
use super::geometry::{Rect, clamp_i32, crop_canvas};
use super::monitor::{MonitorShared, OcrSnapshot};
use super::ocr::merged_ocr_text;
use super::ocr_batch;
use super::template_match::{TemplateHit, dedupe_hits, find_color_template_hits};
use crate::config::RectConfig;

const CHAT_MARKER_SEARCH_WIDTH: u32 = 60;
const CHAT_SCAN_RESULT_LOG_TARGET: &str = "chat_scan_result";

#[derive(Clone, Debug, Serialize)]
pub(super) struct ChatMessage {
    pub(super) message_type: String,
    pub(super) block: Rect,
    pub(super) text: String,
}

pub(super) struct PreparedChatScan {
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

pub(super) fn prepare_chat_scan(
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

pub(super) fn recognize_prepared_chat(
    engine: &OcrEngine,
    templates: &ResolvedTemplateArgs,
    prepared: PreparedChatScan,
    monitor: Option<&MonitorShared>,
) -> Result<Vec<ChatMessage>> {
    let mut messages = Vec::new();
    let ocr_started = Instant::now();
    if templates.batch_recognize {
        let block_rects: Vec<Rect> = prepared.blocks.iter().map(|(_, r)| *r).collect();
        let texts = ocr_batch::batch_recognize_blocks(
            engine,
            &prepared.chat,
            &block_rects,
            templates.same_line_y_tolerance,
        )?;
        for ((marker, block), text) in prepared.blocks.iter().zip(texts) {
            messages.push(ChatMessage {
                message_type: marker_type(marker).to_string(),
                block: *block,
                text,
            });
        }
    } else {
        for (marker, block) in &prepared.blocks {
            let crop = crop_canvas(&prepared.chat, *block)?;
            let text = merged_ocr_text(engine, &crop, templates.same_line_y_tolerance)?;
            messages.push(ChatMessage {
                message_type: marker_type(marker).to_string(),
                block: *block,
                text,
            });
        }
    }
    let ocr_ms = elapsed_ms(ocr_started);
    let total_ms = elapsed_ms(prepared.started);
    if let Some(monitor) = monitor {
        monitor.set_ocr(OcrSnapshot::new(
            prepared.markers.len(),
            messages
                .iter()
                .map(|message| {
                    format!(
                        "[{}] {}",
                        message.message_type,
                        redacted_chat_text(&message.text)
                    )
                })
                .collect(),
            prepared.marker_ms,
            ocr_ms,
            total_ms,
            "一级聊天",
        ));
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

pub(super) fn count_chat_markers(
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
