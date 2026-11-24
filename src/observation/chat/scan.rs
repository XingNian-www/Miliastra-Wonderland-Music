//! Stateless per-frame chat recognition. Message identity belongs to the timeline.

use std::path::PathBuf;
use std::sync::OnceLock;
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

/// 生产路径固定使用的去重窗口（命名空间编号）。
/// 测试窗口从 1 起单调递增，永不与生产窗口重叠：并行用例既不会读到生产路径的上一帧，也不会互相覆盖对方。
const PRODUCTION_SCAN_LOG_WINDOW: u64 = 0;

/// 扫描结果日志的进程级去重器（状态见 [`ScanLogDedup`]）。
static SCAN_LOG_DEDUP: OnceLock<ScanLogDedup> = OnceLock::new();

/// 扫描结果日志的进程级去重器。
///
/// 为什么是进程级：抑制的是**日志**的重复，而日志文件是进程级的——同一进程里
/// 所有扫描（大厅观察线程、`/tools/scan-chat` 诊断）写同一个文件，各自各记一份
/// 会让同一段文本重复出现。
///
/// 为什么不挂在调用方：扫描层是刻意无状态的（见模块文档），调用方（观察线程、
/// 诊断服务）也不该为了日志去持有可变状态；把抑制器做成入参又等于让每个调用点
/// 自己定义日志语义。去重只需要「上次记过什么」这一份事实，进程内共享才是它的
/// 真实语义。
/// 去重状态按**窗口**分槽：每个窗口只与自己的上一帧比较。生产路径恒用窗口 0，
/// 测试每次取一个从 1 起递增的新窗口，因此用例之间（以及用例与生产路径之间）
/// 看不到、也清不掉对方的上一帧。
/// 为什么不是「窗口一变就清空唯一的上一帧」：窗口编号是进程级的，若只有一个槽，
/// 并行用例会互相把对方刚写下的上一帧清掉，让「同一内容第二次必须被抑制」的
/// 断言偶发失败。分槽后跨窗口零干扰，同一窗口内仍然严格去重。
struct ScanLogDedup {
    /// 窗口 -> 该窗口上一次落账的内容；比较与落账在同一个临界区里完成：
    /// 两帧同时到达时，恰好一帧判定为「有变化」。
    ///
    /// 生产进程只会出现窗口 0 这一条；非 0 窗口只由测试创建，条数由用例数
    /// 决定，不会在长跑中增长。
    last: std::sync::Mutex<std::collections::BTreeMap<u64, String>>,
}

impl ScanLogDedup {
    const fn new() -> Self {
        Self {
            last: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// 记录一次扫描结果，返回它是不是「该窗口内自上次记录以来第一次出现」。
    fn record(&self, scan_result: &str, window: u64) -> bool {
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let unchanged = last
            .get(&window)
            .is_some_and(|previous| previous.as_str() == scan_result);
        if unchanged {
            return false;
        }
        last.insert(window, scan_result.to_owned());
        true
    }
}

/// 记录本次扫描结果，返回它是否应该以「有变化」的文案记录。
///
/// 聊天扫描是每秒一次级别的循环，逐次打印整段气泡文本会让文件日志按扫描次数
/// 膨胀；只有内容真的变化时才值得留一条可追溯记录。
//
// 调用方在 `composition`（大厅观察线程与 `/tools/scan-chat` 诊断）；那一层当前
// 无法参与构建时这里看起来“没人用”，但删掉会让真正的接线断开，因此显式标注意图。
#[cfg_attr(not(test), allow(dead_code))]
fn remember_scan_result_for_log(scan_result: &str) -> bool {
    let dedup = SCAN_LOG_DEDUP.get_or_init(ScanLogDedup::new);
    dedup.record(scan_result, PRODUCTION_SCAN_LOG_WINDOW)
}

/// 测试专用：分配一份全新的去重窗口，返回去重器与窗口编号。
///
/// 窗口编号从 1 起单调递增，因此并行用例各拿一个互不相交的命名空间：既从
/// 「该窗口还没记录过任何扫描」开始，也不会读到或覆盖别的用例的上一帧。
/// 它仍然覆盖真实的全局路径——所有窗口共享同一把锁、同一个去重器。
#[cfg(test)]
fn fresh_scan_log_dedup() -> (&'static ScanLogDedup, u64) {
    static NEXT_TEST_WINDOW: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(PRODUCTION_SCAN_LOG_WINDOW + 1);
    let window = NEXT_TEST_WINDOW.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    (SCAN_LOG_DEDUP.get_or_init(ScanLogDedup::new), window)
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
    /// 发言人名字是否出现在**左侧好友列表**里（仅二级大厅 OCR 路径会算）。
    ///
    /// 用户规则：「只要是左侧列表中的都是好友」，所以这是**权限下界**的依据：
    /// 大厅里未登记的好友也应拿到 Friend 级命令，而不是掉回 User。
    ///
    /// **失败必须是 false**：列表 OCR 读空/失败一律 `false`，退回原有的
    /// 「HallMember → User」下界。绝不能因为列表读不到就让所有人失去权限。
    /// 外部源恒为 `false`（它没有屏幕侧栏可看）。
    pub(crate) sender_in_friend_list: bool,
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
                // 一级大厅（primary）没有「左侧好友列表」这个概念，且权限由
                // 第一级的粉色/蓝色标记决定，这里恒 false。
                sender_in_friend_list: false,
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
                // 一级大厅（primary）没有「左侧好友列表」这个概念，且权限由
                // 第一级的粉色/蓝色标记决定，这里恒 false。
                sender_in_friend_list: false,
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
            quality_ok: true,
        }
    }

    fn templates() -> ResolvedTemplateArgs {
        TemplateArgs::default().resolve(&TemplateConfig::default(), &OcrConfig::default())
    }

    #[test]
    fn scan_result_log_recorded_only_when_content_changes() {
        // 生产入口固定落在进程级去重器上。这里取一份全新窗口（与生产窗口 0 及
        // 其它用例的窗口互不相交），从「该窗口还没记录过任何扫描」开始断言。
        let (dedup, window) = fresh_scan_log_dedup();
        // 内容未变时不重复写，变化后重新记录一次。
        assert!(dedup.record("scan-dedup-a", window));
        assert!(!dedup.record("scan-dedup-a", window));
        assert!(dedup.record("scan-dedup-b", window));
        assert!(!dedup.record("scan-dedup-b", window));
        // 内容变回旧值同样算「有变化」：日志要能反映此刻屏幕上的内容。
        assert!(dedup.record("scan-dedup-a", window));
        assert!(!dedup.record("scan-dedup-a", window));

        // 空内容也是合法结果：空大厅必须记录一次，之后不重复。
        assert!(dedup.record("", window));
        assert!(!dedup.record("", window));

        // 换一份新窗口等价于「换了一份干净的去重命名空间」：新窗口没有历史，
        // 同样的内容重新算作第一次。这正是用例之间互不串味的来源。
        let (fresh, next_window) = fresh_scan_log_dedup();
        assert!(next_window > window, "开新窗口必须推进窗口编号");
        assert!(fresh.record("scan-dedup-a", next_window));
        assert!(!fresh.record("scan-dedup-a", next_window));

        // 生产入口与去重器是同一份状态（窗口恒为 0，上面测试窗口不会碰到它）：
        // 连续两次上报同一内容，第一次算变化、第二次必须被抑制——否则观察线程
        // 与诊断服务会各自记一份相同的文本。
        let first = remember_scan_result_for_log("scan-prod-entry");
        assert!(first, "生产入口第一次上报新内容必须记录");
        assert!(
            !remember_scan_result_for_log("scan-prod-entry"),
            "生产入口连续两次上报同一内容必须只记一次"
        );
    }

    /// 并发扫描时，「同样的内容」只能有一次被判定为「有变化」。
    ///
    /// 生产里大厅观察线程与 `/tools/scan-chat` 诊断会同时扫描；若比较与落账
    /// 不在同一个临界区，同一段文本会被两条日志各记一次。这里让 8 个线程同时
    /// 报同一个全新内容，恰好一个线程拿到 true。
    ///
    /// 用例使用自己的窗口：即使与本模块其它用例并行执行，也不会有谁清掉这个
    /// 窗口的上一帧，因此结果与调度顺序无关。
    #[test]
    fn concurrent_scan_results_record_a_shared_payload_exactly_once() {
        let (dedup, window) = fresh_scan_log_dedup();
        let barrier = std::sync::Barrier::new(8);
        let recorded = std::sync::Mutex::new(Vec::<bool>::new());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let recorded = &recorded;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    let changed = dedup.record("scan-concurrent", window);
                    recorded
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(changed);
                });
            }
        });
        let recorded = recorded
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            recorded.iter().filter(|changed| **changed).count(),
            1,
            "同一内容并发上报时只能记一次：{recorded:?}"
        );
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
