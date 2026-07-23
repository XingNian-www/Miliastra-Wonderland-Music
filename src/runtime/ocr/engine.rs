use std::cmp::Ordering;
#[cfg(feature = "ocr-mnn")]
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

#[cfg(feature = "ocr-mnn")]
use anyhow::{Context, anyhow};
use anyhow::{Result, bail};
use image::DynamicImage;
#[cfg(feature = "ocr-mnn")]
use ocr_rs::{Backend, DetOptions, OcrEngine, OcrEngineConfig};
use serde::Serialize;

#[cfg(feature = "ocr-openvino")]
#[path = "openvino.rs"]
mod openvino;

#[cfg(feature = "ocr-openvino")]
use self::openvino::OpenVinoEngine;
use crate::config::OcrConfig;
#[cfg(feature = "ocr-openvino")]
use crate::config::OpenVinoConfig;
use crate::ui::geometry::Rect;

#[derive(Clone, Debug, Default)]
pub(crate) struct OcrArgs {
    det_model: Option<PathBuf>,
    rec_model: Option<PathBuf>,
    charset: Option<PathBuf>,
    min_confidence: Option<f32>,
    #[cfg(feature = "ocr-mnn")]
    threads: Option<i32>,
    backend_priority: Option<Vec<String>>,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedOcrArgs {
    pub(crate) det_model: Option<PathBuf>,
    pub(crate) rec_model: Option<PathBuf>,
    pub(crate) charset: PathBuf,
    pub(crate) min_confidence: f32,
    #[cfg(feature = "ocr-mnn")]
    pub(crate) threads: i32,
    pub(crate) backend_priority: Vec<String>,
    #[cfg(feature = "ocr-openvino")]
    pub(crate) openvino: OpenVinoConfig,
    pub(crate) det_max_side_len: u32,
    pub(crate) det_score_threshold: f32,
    pub(crate) det_unclip_ratio: f32,
    pub(crate) det_min_area: u32,
    pub(crate) det_box_border: u32,
}

impl OcrArgs {
    pub(crate) fn resolve(&self, config: &OcrConfig) -> ResolvedOcrArgs {
        ResolvedOcrArgs {
            det_model: self.det_model.clone().or_else(|| config.det_model.clone()),
            rec_model: self.rec_model.clone().or_else(|| config.rec_model.clone()),
            charset: self
                .charset
                .clone()
                .unwrap_or_else(|| config.charset.clone()),
            min_confidence: self.min_confidence.unwrap_or(config.min_confidence),
            #[cfg(feature = "ocr-mnn")]
            threads: self.threads.unwrap_or(config.threads),
            backend_priority: self
                .backend_priority
                .clone()
                .filter(|backends| !backends.is_empty())
                .unwrap_or_else(|| config.backend_priority.clone()),
            #[cfg(feature = "ocr-openvino")]
            openvino: config.openvino.clone(),
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

pub(crate) fn make_ocr_engine(args: &ResolvedOcrArgs) -> Result<OcrEngineBackend> {
    let total_started = Instant::now();
    let backends = resolve_ocr_backends(&args.backend_priority);
    let mut failures = Vec::new();

    for backend_choice in backends {
        let backend_started = Instant::now();
        match new_ocr_engine(args, backend_choice) {
            Ok(engine) => {
                log::info!("OCR 后端已启用: {}", backend_choice.name());
                log::info!(target: "timing",
                    "OCR 后端已启用: {} 初始化={}ms 总耗时={}ms",
                    backend_choice.name(),
                    elapsed_ms(backend_started),
                    elapsed_ms(total_started)
                );
                return Ok(engine);
            }
            Err(error) => {
                let backend = backend_choice.name();
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
        "load OCR models failed det={} rec={} charset={} failures={}",
        display_optional_path(&args.det_model),
        display_optional_path(&args.rec_model),
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
                gpu: backend_choice.is_gpu(args),
                status,
            }
        })
        .collect()
}

pub(crate) fn recognize_lines(
    engine: &mut OcrEngineBackend,
    image: &DynamicImage,
) -> Result<Vec<OcrLine>> {
    engine.recognize_lines(image)
}

#[cfg(feature = "ocr-mnn")]
fn recognize_mnn_lines(engine: &OcrEngine, image: &DynamicImage) -> Result<Vec<OcrLine>> {
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

fn new_ocr_engine(
    args: &ResolvedOcrArgs,
    backend_choice: OcrBackendChoice,
) -> Result<OcrEngineBackend> {
    if backend_choice == OcrBackendChoice::OpenVino {
        #[cfg(feature = "ocr-openvino")]
        {
            return Ok(OcrEngineBackend::OpenVino(Box::new(OpenVinoEngine::new(
                args,
            )?)));
        }
        #[cfg(not(feature = "ocr-openvino"))]
        {
            bail!("OpenVINO 后端未编译，请使用 `cargo build --features ocr-openvino` 后再启用");
        }
    }

    #[cfg(feature = "ocr-mnn")]
    {
        let backend = backend_choice
            .to_mnn_backend()
            .ok_or_else(|| anyhow!("OCR 后端不是 MNN 后端: {}", backend_choice.name()))?;
        let det_model = required_mnn_path(&args.det_model, "ocr.det_model")?;
        let rec_model = required_mnn_path(&args.rec_model, "ocr.rec_model")?;
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
        let engine = OcrEngine::new(det_model, rec_model, &args.charset, Some(config))
            .map_err(|error| anyhow!("{error:#}"))?;
        Ok(OcrEngineBackend::Mnn(Box::new(engine)))
    }

    #[cfg(not(feature = "ocr-mnn"))]
    {
        let _ = args;
        bail!("MNN OCR 后端未编译；OpenVINO-only 构建请将 backend_priority 设为 openvino，");
    }
}

fn probe_ocr_backend(
    args: &ResolvedOcrArgs,
    backend_choice: OcrBackendChoice,
) -> Result<(u128, u128, u128)> {
    let init_started = Instant::now();
    let mut engine = new_ocr_engine(args, backend_choice)?;
    let init_ms = init_started.elapsed().as_millis();

    let detect_probe = DynamicImage::new_rgb8(320, 96);
    let detect_started = Instant::now();
    engine.probe_detect(&detect_probe)?;
    let detect_ms = detect_started.elapsed().as_millis();

    let rec_probe = DynamicImage::new_rgb8(192, 48);
    let rec_started = Instant::now();
    engine.probe_recognize(&rec_probe)?;
    let rec_ms = rec_started.elapsed().as_millis();

    Ok((init_ms, detect_ms, rec_ms))
}

pub(crate) enum OcrEngineBackend {
    #[cfg(feature = "ocr-mnn")]
    Mnn(Box<OcrEngine>),
    #[cfg(feature = "ocr-openvino")]
    OpenVino(Box<OpenVinoEngine>),
}

impl OcrEngineBackend {
    fn recognize_lines(&mut self, image: &DynamicImage) -> Result<Vec<OcrLine>> {
        match self {
            #[cfg(feature = "ocr-mnn")]
            Self::Mnn(engine) => recognize_mnn_lines(engine, image),
            #[cfg(feature = "ocr-openvino")]
            Self::OpenVino(engine) => engine.recognize_lines(image),
            #[cfg(not(any(feature = "ocr-mnn", feature = "ocr-openvino")))]
            _ => {
                let _ = image;
                bail!("未编译任何 OCR 后端，请启用 `ocr-mnn` 或 `ocr-openvino`")
            }
        }
    }

    fn probe_detect(&mut self, image: &DynamicImage) -> Result<()> {
        match self {
            #[cfg(feature = "ocr-mnn")]
            Self::Mnn(engine) => engine
                .recognize(image)
                .map(|_| ())
                .map_err(|error| anyhow!("检测模型首次推理失败: {error:#}")),
            #[cfg(feature = "ocr-openvino")]
            Self::OpenVino(engine) => engine.probe_detect(image),
            #[cfg(not(any(feature = "ocr-mnn", feature = "ocr-openvino")))]
            _ => {
                let _ = image;
                bail!("未编译任何 OCR 后端，请启用 `ocr-mnn` 或 `ocr-openvino`")
            }
        }
    }

    fn probe_recognize(&mut self, image: &DynamicImage) -> Result<()> {
        match self {
            #[cfg(feature = "ocr-mnn")]
            Self::Mnn(engine) => engine
                .recognize_text(image)
                .map(|_| ())
                .map_err(|error| anyhow!("识别模型首次推理失败: {error:#}")),
            #[cfg(feature = "ocr-openvino")]
            Self::OpenVino(engine) => engine.probe_recognize(image),
            #[cfg(not(any(feature = "ocr-mnn", feature = "ocr-openvino")))]
            _ => {
                let _ = image;
                bail!("未编译任何 OCR 后端，请启用 `ocr-mnn` 或 `ocr-openvino`")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OcrBackendChoice {
    Cuda,
    Vulkan,
    OpenCl,
    OpenVino,
    Cpu,
}

impl OcrBackendChoice {
    fn name(self) -> &'static str {
        match self {
            Self::Cuda => "cuda",
            Self::Vulkan => "vulkan",
            Self::OpenCl => "opencl",
            Self::OpenVino => "openvino",
            Self::Cpu => "cpu",
        }
    }

    fn is_gpu(self, args: &ResolvedOcrArgs) -> bool {
        if matches!(self, Self::Cuda | Self::Vulkan | Self::OpenCl) {
            return true;
        }
        #[cfg(feature = "ocr-openvino")]
        if self == Self::OpenVino {
            return args.openvino.device.trim().eq_ignore_ascii_case("GPU");
        }
        let _ = args;
        false
    }

    #[cfg(feature = "ocr-mnn")]
    fn to_mnn_backend(self) -> Option<Backend> {
        match self {
            Self::Cuda => Some(Backend::CUDA),
            Self::Vulkan => Some(Backend::Vulkan),
            Self::OpenCl => Some(Backend::OpenCL),
            Self::OpenVino => None,
            Self::Cpu => Some(Backend::CPU),
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
    let has_mnn_backend = backends.iter().any(|backend| {
        matches!(
            backend,
            OcrBackendChoice::Cuda
                | OcrBackendChoice::Vulkan
                | OcrBackendChoice::OpenCl
                | OcrBackendChoice::Cpu
        )
    });
    if has_mnn_backend && !backends.contains(&OcrBackendChoice::Cpu) {
        backends.push(OcrBackendChoice::Cpu);
    }
    backends
}

fn parse_ocr_backend(value: &str) -> Option<OcrBackendChoice> {
    match value.trim().to_ascii_lowercase().as_str() {
        "cuda" => Some(OcrBackendChoice::Cuda),
        "vulkan" => Some(OcrBackendChoice::Vulkan),
        "opencl" | "open-cl" => Some(OcrBackendChoice::OpenCl),
        "openvino" => Some(OcrBackendChoice::OpenVino),
        "cpu" => Some(OcrBackendChoice::Cpu),
        _ => None,
    }
}

fn normalize_ocr_text(text: &str) -> String {
    normalize_ocr_spacing(text)
}

fn display_optional_path(path: &Option<PathBuf>) -> String {
    path.as_deref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<未配置>".to_string())
}

#[cfg(feature = "ocr-mnn")]
fn required_mnn_path<'a>(path: &'a Option<PathBuf>, field: &str) -> Result<&'a Path> {
    path.as_deref()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("{field} 在启用 MNN 后端时不能为空"))
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
    #[cfg(feature = "ocr-mnn")]
    use std::path::Path;

    use super::*;
    #[cfg(feature = "ocr-mnn")]
    use crate::config::AppConfig;
    #[cfg(feature = "ocr-mnn")]
    use crate::observation::chat::SECONDARY_TITLE_RECT;

    fn backend_values(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    #[cfg(feature = "ocr-mnn")]
    fn resolves_cuda_backend_with_cpu_fallback() {
        assert_eq!(
            resolve_ocr_backends(&backend_values(&["cuda"])),
            vec![OcrBackendChoice::Cuda, OcrBackendChoice::Cpu]
        );
    }

    #[test]
    fn resolves_openvino_backend_without_implicit_cpu_fallback() {
        assert_eq!(
            resolve_ocr_backends(&backend_values(&[" OpenVINO "])),
            vec![OcrBackendChoice::OpenVino]
        );
    }

    #[test]
    #[cfg(feature = "ocr-mnn")]
    fn resolves_mixed_backend_with_cpu_fallback() {
        assert_eq!(
            resolve_ocr_backends(&backend_values(&["openvino", "cuda"])),
            vec![
                OcrBackendChoice::OpenVino,
                OcrBackendChoice::Cuda,
                OcrBackendChoice::Cpu
            ]
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
    #[cfg(feature = "ocr-mnn")]
    fn fixed_secondary_chat_fixture_recognizes_title_and_strict_friend_list() {
        let config = AppConfig::load(Path::new("config.yaml")).expect("load default config");
        let args = OcrArgs::default().resolve(&config.ocr);
        let mut engine = make_ocr_engine(&args).expect("initialize OCR engine");
        let image = image::open("tests/fixtures/ui/secondary-chat-scrolled-1920x1080.jpg")
            .expect("open fixed secondary-chat screenshot");

        let title = image.crop_imm(
            SECONDARY_TITLE_RECT.x as u32,
            SECONDARY_TITLE_RECT.y as u32,
            SECONDARY_TITLE_RECT.width,
            SECONDARY_TITLE_RECT.height,
        );
        let title = merge_ocr_lines(
            recognize_lines(&mut engine, &title).expect("recognize title"),
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
            recognize_lines(&mut engine, &friend_list).expect("recognize friend list"),
            12,
        );
        assert!(
            friend_text.contains("破鹿子") && friend_text.contains("银河乐子人"),
            "unexpected friend-list OCR: {friend_text}"
        );
    }
}
