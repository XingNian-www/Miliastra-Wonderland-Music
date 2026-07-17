use std::cmp::Ordering;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use image::DynamicImage;
use ocr_rs::{Backend, DetOptions, OcrEngine, OcrEngineConfig};
use serde::Serialize;

use crate::config::OcrConfig;
use crate::ui::geometry::Rect;

#[derive(Clone, Debug, Default)]
pub(crate) struct OcrArgs {
    det_model: Option<PathBuf>,
    rec_model: Option<PathBuf>,
    charset: Option<PathBuf>,
    min_confidence: Option<f32>,
    threads: Option<i32>,
    backend_priority: Option<Vec<String>>,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedOcrArgs {
    pub(crate) det_model: PathBuf,
    pub(crate) rec_model: PathBuf,
    pub(crate) charset: PathBuf,
    pub(crate) min_confidence: f32,
    pub(crate) threads: i32,
    pub(crate) backend_priority: Vec<String>,
    pub(crate) det_max_side_len: u32,
    pub(crate) det_score_threshold: f32,
    pub(crate) det_unclip_ratio: f32,
    pub(crate) det_min_area: u32,
    pub(crate) det_box_border: u32,
}

impl OcrArgs {
    pub(crate) fn resolve(&self, config: &OcrConfig) -> ResolvedOcrArgs {
        ResolvedOcrArgs {
            det_model: self
                .det_model
                .clone()
                .unwrap_or_else(|| config.det_model.clone()),
            rec_model: self
                .rec_model
                .clone()
                .unwrap_or_else(|| config.rec_model.clone()),
            charset: self
                .charset
                .clone()
                .unwrap_or_else(|| config.charset.clone()),
            min_confidence: self.min_confidence.unwrap_or(config.min_confidence),
            threads: self.threads.unwrap_or(config.threads),
            backend_priority: self
                .backend_priority
                .clone()
                .filter(|backends| !backends.is_empty())
                .unwrap_or_else(|| config.backend_priority.clone()),
            det_max_side_len: config.det_max_side_len,
            det_score_threshold: config.det_score_threshold,
            det_unclip_ratio: config.det_unclip_ratio,
            det_min_area: config.det_min_area,
            det_box_border: config.det_box_border,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct OcrBackendProbeResult {
    pub(crate) name: &'static str,
    pub(crate) gpu: bool,
    pub(crate) status: OcrBackendProbeStatus,
}

#[derive(Clone, Debug)]
pub(crate) enum OcrBackendProbeStatus {
    Available {
        init_ms: u128,
        detect_ms: u128,
        rec_ms: u128,
    },
    Failed {
        elapsed_ms: u128,
        error: String,
    },
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct OcrLine {
    pub(crate) text: String,
    pub(crate) confidence: f32,
    pub(crate) bbox: Rect,
}

pub(crate) fn make_ocr_engine(args: &ResolvedOcrArgs) -> Result<OcrEngine> {
    let total_started = Instant::now();
    let backends = resolve_ocr_backends(&args.backend_priority);
    let mut failures = Vec::new();

    for backend_choice in backends {
        let backend = backend_choice.to_backend();
        let backend_started = Instant::now();
        match new_ocr_engine(args, backend) {
            Ok(engine) => {
                log::info!("OCR 后端已启用: {}", backend_name(backend));
                log::info!(target: "timing",
                    "OCR 后端已启用: {} 初始化={}ms 总耗时={}ms",
                    backend_name(backend),
                    elapsed_ms(backend_started),
                    elapsed_ms(total_started)
                );
                return Ok(engine);
            }
            Err(error) => {
                let backend = backend_name(backend);
                let backend_ms = elapsed_ms(backend_started);
                let message = format!("{backend}: {error:#}");
                log::warn!("OCR 后端初始化失败，尝试下一个: {message}");
                log::warn!(target: "timing",
                    "OCR 后端初始化失败耗时: backend={} total={}ms error={error:#}",
                    backend,
                    backend_ms
                );
                failures.push(message);
            }
        }
    }

    bail!(
        "load PaddleOCR models failed det={} rec={} charset={} failures={}",
        args.det_model.display(),
        args.rec_model.display(),
        args.charset.display(),
        failures.join(" | ")
    )
}

pub(crate) fn probe_ocr_backend_support(args: &ResolvedOcrArgs) -> Vec<OcrBackendProbeResult> {
    resolve_ocr_backends(&args.backend_priority)
        .into_iter()
        .map(|backend_choice| {
            let started = Instant::now();
            let status = match probe_ocr_backend(args, backend_choice) {
                Ok((init_ms, detect_ms, rec_ms)) => OcrBackendProbeStatus::Available {
                    init_ms,
                    detect_ms,
                    rec_ms,
                },
                Err(error) => OcrBackendProbeStatus::Failed {
                    elapsed_ms: started.elapsed().as_millis(),
                    error: format!("{error:#}"),
                },
            };

            OcrBackendProbeResult {
                name: backend_choice.name(),
                gpu: backend_choice.is_gpu(),
                status,
            }
        })
        .collect()
}

pub(crate) fn recognize_lines(engine: &OcrEngine, image: &DynamicImage) -> Result<Vec<OcrLine>> {
    let started = Instant::now();
    let mut lines: Vec<OcrLine> = engine
        .recognize(image)
        .context("PaddleOCR recognize image")?
        .into_iter()
        .map(|item| OcrLine {
            text: normalize_ocr_text(&item.text),
            confidence: item.confidence,
            bbox: Rect::new(
                item.bbox.rect.left(),
                item.bbox.rect.top(),
                item.bbox.rect.width(),
                item.bbox.rect.height(),
            ),
        })
        .filter(|line| !line.text.is_empty())
        .collect();
    lines.sort_by(|left, right| compare_rect_top_left(left.bbox, right.bbox));
    log::info!(target: "timing",
        "OCR 识别耗时: {}ms image={}x{} lines={}",
        elapsed_ms(started),
        image.width(),
        image.height(),
        lines.len()
    );
    Ok(lines)
}

fn new_ocr_engine(args: &ResolvedOcrArgs, backend: Backend) -> Result<OcrEngine> {
    let mut det_options = DetOptions::new()
        .with_max_side_len(args.det_max_side_len)
        .with_score_threshold(args.det_score_threshold)
        .with_min_area(args.det_min_area)
        .with_box_border(args.det_box_border);
    det_options.unclip_ratio = args.det_unclip_ratio;

    let config = OcrEngineConfig::new()
        .with_backend(backend)
        .with_threads(args.threads)
        .with_det_options(det_options)
        .with_min_result_confidence(args.min_confidence);
    OcrEngine::new(
        &args.det_model,
        &args.rec_model,
        &args.charset,
        Some(config),
    )
    .map_err(|error| anyhow!("{error:#}"))
}

fn probe_ocr_backend(
    args: &ResolvedOcrArgs,
    backend_choice: OcrBackendChoice,
) -> Result<(u128, u128, u128)> {
    let init_started = Instant::now();
    let engine = new_ocr_engine(args, backend_choice.to_backend())?;
    let init_ms = init_started.elapsed().as_millis();

    let detect_probe = DynamicImage::new_rgb8(320, 96);
    let detect_started = Instant::now();
    engine
        .recognize(&detect_probe)
        .map_err(|error| anyhow!("检测模型首次推理失败: {error:#}"))?;
    let detect_ms = detect_started.elapsed().as_millis();

    let rec_probe = DynamicImage::new_rgb8(192, 48);
    let rec_started = Instant::now();
    engine
        .recognize_text(&rec_probe)
        .map_err(|error| anyhow!("识别模型首次推理失败: {error:#}"))?;
    let rec_ms = rec_started.elapsed().as_millis();

    Ok((init_ms, detect_ms, rec_ms))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OcrBackendChoice {
    Cuda,
    Vulkan,
    OpenCl,
    Cpu,
}

impl OcrBackendChoice {
    fn name(self) -> &'static str {
        match self {
            Self::Cuda => "cuda",
            Self::Vulkan => "vulkan",
            Self::OpenCl => "opencl",
            Self::Cpu => "cpu",
        }
    }

    fn is_gpu(self) -> bool {
        matches!(self, Self::Cuda | Self::Vulkan | Self::OpenCl)
    }

    fn to_backend(self) -> Backend {
        match self {
            Self::Cuda => Backend::CUDA,
            Self::Vulkan => Backend::Vulkan,
            Self::OpenCl => Backend::OpenCL,
            Self::Cpu => Backend::CPU,
        }
    }
}

fn resolve_ocr_backends(values: &[String]) -> Vec<OcrBackendChoice> {
    let mut backends = Vec::new();
    for value in values {
        match parse_ocr_backend(value) {
            Some(backend) if !backends.contains(&backend) => backends.push(backend),
            Some(_) => {}
            None => log::warn!("未知 OCR 后端配置，已忽略: {}", value),
        }
    }
    if !backends.contains(&OcrBackendChoice::Cpu) {
        backends.push(OcrBackendChoice::Cpu);
    }
    backends
}

fn parse_ocr_backend(value: &str) -> Option<OcrBackendChoice> {
    match value.trim().to_ascii_lowercase().as_str() {
        "cuda" => Some(OcrBackendChoice::Cuda),
        "vulkan" => Some(OcrBackendChoice::Vulkan),
        "opencl" | "open-cl" => Some(OcrBackendChoice::OpenCl),
        "cpu" => Some(OcrBackendChoice::Cpu),
        _ => None,
    }
}

fn backend_name(backend: Backend) -> &'static str {
    match backend {
        Backend::CPU => "cpu",
        Backend::CUDA => "cuda",
        Backend::Vulkan => "vulkan",
        Backend::OpenCL => "opencl",
        Backend::Metal => "metal",
        Backend::OpenGL => "opengl",
        Backend::CoreML => "coreml",
    }
}

fn normalize_ocr_text(text: &str) -> String {
    normalize_ocr_spacing(text)
}

pub(crate) fn merge_ocr_lines(mut items: Vec<OcrLine>, same_line_y_tolerance: i32) -> String {
    items.sort_by(|left, right| compare_rect_top_left(left.bbox, right.bbox));
    let mut lines: Vec<(i32, Vec<OcrLine>)> = Vec::new();
    for item in items {
        if let Some((line_y, line_items)) = lines.last_mut()
            && (item.bbox.y - *line_y).abs() <= same_line_y_tolerance
        {
            let count = line_items.len() as i32;
            *line_y = ((*line_y * count) + item.bbox.y) / (count + 1);
            line_items.push(item);
            continue;
        }
        lines.push((item.bbox.y, vec![item]));
    }
    normalize_ocr_spacing(
        &lines
            .into_iter()
            .map(|(_, mut line_items)| {
                line_items.sort_by_key(|item| item.bbox.x);
                normalize_ocr_spacing(
                    &line_items
                        .into_iter()
                        .map(|item| item.text)
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn normalize_ocr_spacing(text: &str) -> String {
    let mut output = String::new();
    let mut previous_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !previous_space && !output.is_empty() {
                output.push(' ');
                previous_space = true;
            }
        } else {
            output.push(ch);
            previous_space = false;
        }
    }
    let chars = output.trim().chars().collect::<Vec<_>>();
    let mut compact = String::new();
    for (index, ch) in chars.iter().enumerate() {
        if *ch == ' ' {
            let previous = index.checked_sub(1).and_then(|i| chars.get(i)).copied();
            let next = chars.get(index + 1).copied();
            if previous.is_some_and(is_cjk) && next.is_some_and(is_cjk) {
                continue;
            }
            if next.is_some_and(is_closing_punctuation) {
                continue;
            }
            if previous.is_some_and(is_opening_punctuation) {
                continue;
            }
        }
        compact.push(*ch);
    }
    compact.trim().to_string()
}

fn is_cjk(ch: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&ch)
}

fn is_closing_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '，' | '。'
            | '！'
            | '？'
            | '、'
            | '；'
            | '：'
            | ','
            | '.'
            | '!'
            | '?'
            | ';'
            | ':'
            | '）'
            | '】'
            | ']'
            | '}'
    )
}

fn is_opening_punctuation(ch: char) -> bool {
    matches!(ch, '（' | '【' | '[' | '{')
}

fn compare_rect_top_left(left: Rect, right: Rect) -> Ordering {
    (left.y / 10)
        .cmp(&(right.y / 10))
        .then_with(|| left.x.cmp(&right.x))
        .then_with(|| left.y.cmp(&right.y))
}

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::config::AppConfig;
    use crate::observation::chat::SECONDARY_TITLE_RECT;

    fn backend_values(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn resolves_cuda_backend_with_cpu_fallback() {
        assert_eq!(
            resolve_ocr_backends(&backend_values(&["cuda"])),
            vec![OcrBackendChoice::Cuda, OcrBackendChoice::Cpu]
        );
    }

    #[test]
    fn resolves_backend_priority_case_insensitively_and_deduplicates() {
        assert_eq!(
            resolve_ocr_backends(&backend_values(&[" CUDA ", "open-cl", "CUDA", "cpu"])),
            vec![
                OcrBackendChoice::Cuda,
                OcrBackendChoice::OpenCl,
                OcrBackendChoice::Cpu
            ]
        );
    }

    #[test]
    fn fixed_secondary_chat_fixture_recognizes_title_and_strict_friend_list() {
        let config = AppConfig::load(Path::new("config.yaml")).expect("load default config");
        let args = OcrArgs::default().resolve(&config.ocr);
        let engine = make_ocr_engine(&args).expect("initialize OCR engine");
        let image = image::open("tests/fixtures/ui/secondary-chat-scrolled-1920x1080.jpg")
            .expect("open fixed secondary-chat screenshot");

        let title = image.crop_imm(
            SECONDARY_TITLE_RECT.x as u32,
            SECONDARY_TITLE_RECT.y as u32,
            SECONDARY_TITLE_RECT.width,
            SECONDARY_TITLE_RECT.height,
        );
        let title = merge_ocr_lines(
            recognize_lines(&engine, &title).expect("recognize title"),
            12,
        );
        assert!(title.contains("香菜"), "unexpected title OCR: {title}");

        let friend_rect: Rect = config.invite.friend_list_region.into();
        let friend_list = image.crop_imm(
            friend_rect.x as u32,
            friend_rect.y as u32,
            friend_rect.width,
            friend_rect.height,
        );
        let friend_text = merge_ocr_lines(
            recognize_lines(&engine, &friend_list).expect("recognize friend list"),
            12,
        );
        assert!(
            friend_text.contains("破鹿子") && friend_text.contains("银河乐子人"),
            "unexpected friend-list OCR: {friend_text}"
        );
    }
}
