use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use image::{DynamicImage, GenericImageView, GrayImage};
use imageproc::contours::{Contour, find_contours};
use imageproc::point::Point;
use imageproc::rect::Rect as ImageRect;
use openvino::{
    CompiledModel, Core, DeviceType, ElementType, InferRequest, RwPropertyKey, Shape, Tensor,
};

use super::{OcrLine, ResolvedOcrArgs, compare_rect_top_left, normalize_ocr_text};
use crate::geometry::Rect;

#[cfg(test)]
#[path = "openvino_perf_tests.rs"]
mod perf_tests;

const RECOGNITION_HEIGHT: u32 = 48;
const RECOGNITION_CHARACTER_THRESHOLD: f32 = 0.3;
// 默认聊天区域为 416x143，单个区块上限为 120px。限制检测形状，避免 GPU 插件因区块高度的小幅变化反复编译图；第二画布覆盖拼接批量 OCR 路径。
const CHAT_DETECTION_CANVAS_WIDTH: u32 = 416;
// 143px 聊天区域经过源去重后最多保留九行标记；不重叠区块占 143px，加八个 12px 分隔线共 239px，因此按下一个 32px 对齐后使用 256px 可覆盖完整批量拼接路径。
const CHAT_DETECTION_CANVAS_HEIGHTS: &[u32] = &[128, 256];
// 识别宽度与文本裁剪区宽高比成正比。这些分档覆盖常见聊天行，同时限制 GPU 形状特化数量。
const RECOGNITION_WIDTH_BUCKETS: &[u32] = &[96, 128, 192, 256, 384, 512, 768, 1024];
const DETECTION_CHANNEL_MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const DETECTION_CHANNEL_STD: [f32; 3] = [0.229, 0.224, 0.225];
const RECOGNITION_CHANNEL_MEAN: [f32; 3] = [0.5, 0.5, 0.5];
const RECOGNITION_CHANNEL_STD: [f32; 3] = [0.5, 0.5, 0.5];

pub(crate) struct OpenVinoEngine {
    detector: OpenVinoModel,
    recognizer: OpenVinoModel,
    rec_fixed_width: Option<u32>,
    charset: Vec<char>,
    min_confidence: f32,
    det_max_side_len: u32,
    det_score_threshold: f32,
    det_unclip_ratio: f32,
    det_min_area: u32,
    det_box_border: u32,
}

// A single OCR worker uses each model synchronously. Keep the request alive and
// reuse common input shapes; output views never escape run's postprocess callback.
struct OpenVinoModel {
    request: Option<InferRequest>,
    inputs: VecDeque<CachedInputTensor>,
    compiled: CompiledModel,
    label: &'static str,
}

struct CachedInputTensor {
    shape: Vec<usize>,
    tensor: Tensor,
    bytes: usize,
}

const INPUT_TENSOR_CACHE_ENTRIES: usize = 8;
const INPUT_TENSOR_CACHE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
struct DetectedTextBox {
    rect: ImageRect,
}

impl DetectedTextBox {
    fn expand(self, border: u32, max_width: u32, max_height: u32) -> ImageRect {
        let left = (self.rect.left() - border as i32).max(0) as u32;
        let top = (self.rect.top() - border as i32).max(0) as u32;
        let right = (self.rect.left().max(0) as u32 + self.rect.width() + border).min(max_width);
        let bottom = (self.rect.top().max(0) as u32 + self.rect.height() + border).min(max_height);
        let width = right.saturating_sub(left).max(1);
        let height = bottom.saturating_sub(top).max(1);
        ImageRect::at(left as i32, top as i32).of_size(width, height)
    }
}

struct PreprocessedTensor {
    data: Vec<f32>,
    shape: Vec<usize>,
    valid_width: u32,
    valid_height: u32,
}

#[derive(Clone, Copy)]
struct DetectionGeometry {
    mask_width: u32,
    mask_height: u32,
    valid_width: u32,
    valid_height: u32,
    original_width: u32,
    original_height: u32,
    min_area: u32,
    unclip_ratio: f32,
}

impl OpenVinoEngine {
    pub(crate) fn new(args: &ResolvedOcrArgs) -> Result<Self> {
        let config = &args.openvino;
        let det_model = required_path(&config.det_model, "ocr.openvino.det_model")?;
        let det_weights = required_path(&config.det_weights, "ocr.openvino.det_weights")?;
        let rec_model = required_path(&config.rec_model, "ocr.openvino.rec_model")?;
        let rec_weights = required_path(&config.rec_weights, "ocr.openvino.rec_weights")?;

        prepare_local_runtime_loading();
        let mut core = Core::new().map_err(|error| {
            anyhow!(
                "OpenVINO 运行时依赖不可用，初始化 Core 失败: {error:#}; 发布包应已内置 \
                 deps/openvino，请确认该目录未被删除；使用外部安装时请把 \
                 runtime/bin/intel64/Release 与 runtime/3rdparty/tbb/bin 加入 PATH，\
                 或设置 OPENVINO_INSTALL_DIR；当前后端会按配置继续尝试下一个 fallback"
            )
        })?;
        let device = device_type(&config.device);
        configure_cache(&mut core, &device, config.cache_dir.as_deref());
        let det_ir = core
            .read_model_from_file(path_string(det_model)?, path_string(det_weights)?)
            .with_context(|| format!("读取 OpenVINO 检测模型失败: {}", det_model.display()))?;
        let rec_ir = core
            .read_model_from_file(path_string(rec_model)?, path_string(rec_weights)?)
            .with_context(|| format!("读取 OpenVINO 识别模型失败: {}", rec_model.display()))?;
        let detector = OpenVinoModel::compile(&mut core, &det_ir, device.to_owned(), "detector")?;
        let recognizer = OpenVinoModel::compile(&mut core, &rec_ir, device, "recognizer")?;

        let charset = load_charset(&args.charset)?;
        let mut engine = Self {
            detector,
            recognizer,
            rec_fixed_width: config.rec_fixed_width,
            charset,
            min_confidence: args.min_confidence,
            det_max_side_len: args.det_max_side_len,
            det_score_threshold: args.det_score_threshold,
            det_unclip_ratio: args.det_unclip_ratio,
            det_min_area: args.det_min_area,
            det_box_border: args.det_box_border,
        };

        // 仅完成编译期加载不能证明后端可用。先运行两张图各一次，避免首个生产请求承担插件初始化成本。
        engine.warm_up()?;
        Ok(engine)
    }

    pub(crate) fn recognize_lines(&mut self, image: &DynamicImage) -> Result<Vec<OcrLine>> {
        let boxes = self.detect(image)?;
        let mut lines = Vec::with_capacity(boxes.len());
        for text_box in boxes {
            let expanded = text_box.expand(self.det_box_border, image.width(), image.height());
            let x = expanded
                .left()
                .clamp(0, image.width().saturating_sub(1) as i32) as u32;
            let y = expanded
                .top()
                .clamp(0, image.height().saturating_sub(1) as i32) as u32;
            let width = expanded.width().min(image.width().saturating_sub(x)).max(1);
            let height = expanded
                .height()
                .min(image.height().saturating_sub(y))
                .max(1);
            let crop = image.crop_imm(x, y, width, height);
            let (text, confidence) = self.recognize_text(&crop)?;
            if !text.is_empty() && confidence >= self.min_confidence {
                lines.push(OcrLine {
                    text,
                    confidence,
                    bbox: Rect::new(x as i32, y as i32, width, height),
                });
            }
        }
        for line in &mut lines {
            line.text = normalize_ocr_text(&line.text);
        }
        lines.retain(|line| !line.text.is_empty());
        lines.sort_by(|left, right| compare_rect_top_left(left.bbox, right.bbox));
        Ok(lines)
    }

    pub(crate) fn probe_detect(&mut self, image: &DynamicImage) -> Result<()> {
        self.detect(image).map(|_| ())
    }

    pub(crate) fn probe_recognize(&mut self, image: &DynamicImage) -> Result<()> {
        self.recognize_text(image).map(|_| ())
    }

    fn warm_up(&mut self) -> Result<()> {
        for &height in CHAT_DETECTION_CANVAS_HEIGHTS {
            self.probe_detect(&DynamicImage::new_rgb8(CHAT_DETECTION_CANVAS_WIDTH, height))
                .with_context(|| {
                    format!(
                        "OpenVINO 检测模型 warm-up 失败: shape=[1,3,{},{}]",
                        height, CHAT_DETECTION_CANVAS_WIDTH
                    )
                })?;
        }
        let recognition_widths = match self.rec_fixed_width {
            Some(width) => vec![width],
            None => RECOGNITION_WIDTH_BUCKETS.to_vec(),
        };
        for width in recognition_widths {
            self.probe_recognize(&DynamicImage::new_rgb8(width, RECOGNITION_HEIGHT))
                .with_context(|| {
                    format!(
                        "OpenVINO 识别模型 warm-up 失败: shape=[1,3,{},{}]",
                        RECOGNITION_HEIGHT, width
                    )
                })?;
        }
        Ok(())
    }

    fn detect(&mut self, image: &DynamicImage) -> Result<Vec<DetectedTextBox>> {
        let scaled = resize_to_max_side(image, self.det_max_side_len)
            .context("OpenVINO 检测图像缩放失败")?;
        let input = preprocess_for_det(&scaled).context("OpenVINO 检测图像预处理失败")?;
        let threshold = self.det_score_threshold;
        let min_area = self.det_min_area;
        let unclip_ratio = self.det_unclip_ratio;
        self.detector
            .run(&input.data, &input.shape, |output_data, output_shape| {
                let (batch, channels, out_h, out_w) = match output_shape {
                    [batch, channels, height, width] => (*batch, *channels, *height, *width),
                    [batch, height, width] => (*batch, 1, *height, *width),
                    [height, width] => (1, 1, *height, *width),
                    _ => {
                        bail!("OpenVINO 检测输出形状无效: {output_shape:?}");
                    }
                };
                if batch != 1 || channels != 1 {
                    bail!(
                        "OpenVINO 检测输出必须是单 batch/单 channel 概率图: shape={output_shape:?}"
                    );
                }
                if out_w == 0 || out_h == 0 {
                    bail!("OpenVINO 检测输出形状无效: {output_shape:?}");
                }
                let mask_len = out_w
                    .checked_mul(out_h)
                    .ok_or_else(|| anyhow!("OpenVINO 检测输出尺寸溢出: {output_shape:?}"))?;
                if out_w != input.shape[3] || out_h != input.shape[2] {
                    bail!(
                        "OpenVINO 检测输出空间尺寸必须与输入一致: input={}x{} output={}x{}",
                        input.shape[3],
                        input.shape[2],
                        out_w,
                        out_h
                    );
                }
                if output_data.len() < mask_len {
                    bail!(
                        "OpenVINO 检测输出尺寸无效: shape={output_shape:?} values={}",
                        output_data.len()
                    );
                }
                let mask = output_data[..mask_len]
                    .iter()
                    .map(|value| u8::from(*value > threshold) * 255)
                    .collect::<Vec<_>>();
                Ok(extract_boxes_with_unclip(
                    &mask,
                    DetectionGeometry {
                        mask_width: out_w as u32,
                        mask_height: out_h as u32,
                        valid_width: input.valid_width,
                        valid_height: input.valid_height,
                        original_width: image.width(),
                        original_height: image.height(),
                        min_area,
                        unclip_ratio,
                    },
                ))
            })
    }

    fn recognize_text(&mut self, image: &DynamicImage) -> Result<(String, f32)> {
        let input = preprocess_for_rec(image, RECOGNITION_HEIGHT, self.rec_fixed_width)
            .context("OpenVINO 识别图像预处理失败")?;
        let fixed_width = self.rec_fixed_width;
        let charset = &self.charset;
        self.recognizer
            .run(&input.data, &input.shape, |output_data, output_shape| {
                let (sequence_length, _) = ctc_output_dimensions(output_shape)?;
                let valid_sequence_length =
                    recognition_valid_sequence_length(&input, sequence_length, fixed_width)?;
                decode_ctc(
                    output_data,
                    output_shape,
                    charset,
                    Some(valid_sequence_length),
                )
            })
    }
}

impl OpenVinoModel {
    fn compile(
        core: &mut Core,
        model: &openvino::Model,
        device: DeviceType<'static>,
        label: &'static str,
    ) -> Result<Self> {
        let mut compiled = core
            .compile_model(model, device)
            .with_context(|| format!("编译 OpenVINO {label} 模型失败"))?;
        let request = compiled
            .create_infer_request()
            .with_context(|| format!("创建 OpenVINO {label} 推理请求失败"))?;
        Ok(Self {
            request: Some(request),
            inputs: VecDeque::new(),
            compiled,
            label,
        })
    }

    fn take_input(&mut self, input_shape: &[usize]) -> Result<CachedInputTensor> {
        if let Some(index) = self
            .inputs
            .iter()
            .position(|input| input.shape == input_shape)
        {
            return Ok(self.inputs.remove(index).expect("located cached tensor"));
        }
        let dimensions = input_shape
            .iter()
            .map(|&dimension| i64::try_from(dimension))
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("OpenVINO 输入维度超出范围")?;
        let shape = Shape::new(&dimensions).context("创建 OpenVINO 输入形状失败")?;
        let tensor = Tensor::new(ElementType::F32, &shape)
            .with_context(|| format!("创建 OpenVINO {} 输入张量失败", self.label))?;
        let bytes = input_shape
            .iter()
            .try_fold(std::mem::size_of::<f32>(), |bytes, &size| {
                bytes.checked_mul(size)
            })
            .ok_or_else(|| anyhow!("OpenVINO 输入张量尺寸溢出"))?;
        Ok(CachedInputTensor {
            shape: input_shape.to_vec(),
            tensor,
            bytes,
        })
    }

    fn return_input(&mut self, input: CachedInputTensor) {
        // Bound both shape count and retained bytes, including unusual diagnostic images.
        if input.bytes > INPUT_TENSOR_CACHE_BYTES {
            return;
        }
        let mut bytes = self.inputs.iter().map(|entry| entry.bytes).sum::<usize>();
        while self.inputs.len() >= INPUT_TENSOR_CACHE_ENTRIES
            || bytes + input.bytes > INPUT_TENSOR_CACHE_BYTES
        {
            if let Some(oldest) = self.inputs.pop_front() {
                bytes -= oldest.bytes;
            } else {
                break;
            }
        }
        self.inputs.push_back(input);
    }

    fn run<R>(
        &mut self,
        input_data: &[f32],
        input_shape: &[usize],
        postprocess: impl FnOnce(&[f32], &[usize]) -> Result<R>,
    ) -> Result<R> {
        let mut input = self.take_input(input_shape)?;
        let result = (|| {
            let tensor_data = input
                .tensor
                .get_data_mut::<f32>()
                .with_context(|| format!("访问 OpenVINO {} 输入张量失败", self.label))?;
            if tensor_data.len() != input_data.len() {
                bail!(
                    "OpenVINO {} 输入张量元素数不匹配: tensor={} input={}",
                    self.label,
                    tensor_data.len(),
                    input_data.len()
                );
            }
            // Overwrite the full tensor, including padding, before every inference.
            tensor_data.copy_from_slice(input_data);
            if self.request.is_none() {
                self.request = Some(
                    self.compiled
                        .create_infer_request()
                        .with_context(|| format!("创建 OpenVINO {} 推理请求失败", self.label))?,
                );
            }
            let request = self.request.as_mut().expect("request initialized");
            request
                .set_input_tensor(&input.tensor)
                .with_context(|| format!("设置 OpenVINO {} 输入失败", self.label))?;
            request
                .infer()
                .with_context(|| format!("执行 OpenVINO {} 推理失败", self.label))?;
            let output = request
                .get_output_tensor_by_index(0)
                .with_context(|| format!("读取 OpenVINO {} 输出失败", self.label))?;
            if output
                .get_element_type()
                .context("读取 OpenVINO 输出元素类型失败")?
                != ElementType::F32
            {
                bail!("OpenVINO {} 输出必须是 F32，当前类型不受支持", self.label);
            }
            let output_shape = output
                .get_shape()
                .context("读取 OpenVINO 输出形状失败")?
                .get_dimensions()
                .iter()
                .map(|&dimension| usize::try_from(dimension))
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("OpenVINO 输出维度无效")?;
            let output_data = output
                .get_data::<f32>()
                .context("读取 OpenVINO 输出数据失败")?;
            // No T*C output copy: finish consuming the borrowed view before this
            // output handle drops and before the persistent request can infer again.
            postprocess(output_data, &output_shape)
        })();
        if result.is_err() || input.bytes > INPUT_TENSOR_CACHE_BYTES {
            // Rebuild after failures. Also release the request's native reference
            // to oversized one-off inputs instead of retaining them between calls.
            self.request = None;
        }
        self.return_input(input);
        result
    }
}

fn resize_to_max_side(image: &DynamicImage, max_side_len: u32) -> Result<DynamicImage> {
    if max_side_len == 0 {
        bail!("OpenVINO 检测最长边必须大于 0");
    }
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        bail!("OpenVINO 输入图像尺寸不能为空");
    }
    let max_dimension = width.max(height);
    if max_dimension <= max_side_len {
        return Ok(image.clone());
    }
    let scale = max_side_len as f64 / max_dimension as f64;
    let new_width = ((width as f64 * scale).round() as u32).max(1);
    let new_height = ((height as f64 * scale).round() as u32).max(1);
    Ok(image.resize_exact(new_width, new_height, image::imageops::FilterType::Lanczos3))
}

fn padded_size(size: u32) -> u32 {
    size.saturating_add(31) / 32 * 32
}

fn detection_canvas_size(width: u32, height: u32) -> (u32, u32) {
    if width <= CHAT_DETECTION_CANVAS_WIDTH
        && let Some(&canvas_height) = CHAT_DETECTION_CANVAS_HEIGHTS
            .iter()
            .find(|&&canvas_height| height <= canvas_height)
    {
        return (CHAT_DETECTION_CANVAS_WIDTH, canvas_height);
    }
    (padded_size(width).max(32), padded_size(height).max(32))
}

fn recognition_width_bucket(width: u32) -> u32 {
    RECOGNITION_WIDTH_BUCKETS
        .iter()
        .copied()
        .find(|&bucket| width <= bucket)
        .unwrap_or_else(|| padded_size(width).max(32))
}

fn preprocess_for_det(image: &DynamicImage) -> Result<PreprocessedTensor> {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        bail!("OpenVINO 检测输入图像尺寸不能为空");
    }
    let (padded_width, padded_height) = detection_canvas_size(width, height);
    let padded_width = padded_width as usize;
    let padded_height = padded_height as usize;
    let plane = padded_width
        .checked_mul(padded_height)
        .ok_or_else(|| anyhow!("OpenVINO 检测输入尺寸溢出"))?;
    let mut data = vec![0.0; plane * 3];
    for (y, row) in image.to_rgb8().rows().enumerate() {
        for (x, pixel) in row.enumerate() {
            let channels = pixel.0;
            for channel in 0..3 {
                data[channel * plane + y * padded_width + x] = (channels[channel] as f32 / 255.0
                    - DETECTION_CHANNEL_MEAN[channel])
                    / DETECTION_CHANNEL_STD[channel];
            }
        }
    }
    Ok(PreprocessedTensor {
        data,
        shape: vec![1, 3, padded_height, padded_width],
        valid_width: width,
        valid_height: height,
    })
}

fn preprocess_for_rec(
    image: &DynamicImage,
    target_height: u32,
    fixed_width: Option<u32>,
) -> Result<PreprocessedTensor> {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 || target_height == 0 {
        bail!("OpenVINO 识别输入图像尺寸不能为空");
    }
    let scale = target_height as f64 / height as f64;
    let scaled_width = width as f64 * scale;
    let (valid_width, target_width, source) = match fixed_width {
        Some(768) if target_height == RECOGNITION_HEIGHT => {
            // 对齐 train.py preprocess_for_model：Python round（ties-to-even），
            // 先封顶再 resize（不是裁掉后缀），输入在缩放前转 RGB。
            let valid_width = (scaled_width.round_ties_even() as u32).clamp(1, 768);
            (
                valid_width,
                768,
                Cow::Owned(DynamicImage::ImageRgb8(image.to_rgb8())),
            )
        }
        Some(_) => bail!("OpenVINO 固定识别 profile 仅支持 48x768、输出 stride 8"),
        None => {
            // 官方 bucket 的舍入、色彩转换顺序与宽度分档保持不变。
            let valid_width = (scaled_width.round() as u32).max(1);
            (
                valid_width,
                recognition_width_bucket(valid_width),
                Cow::Borrowed(image),
            )
        }
    };
    let resized = if width == valid_width && height == target_height {
        source.into_owned()
    } else {
        source.resize_exact(
            valid_width,
            target_height,
            image::imageops::FilterType::Lanczos3,
        )
    };
    let target_width = target_width as usize;
    let target_height = target_height as usize;
    let plane = target_width
        .checked_mul(target_height)
        .ok_or_else(|| anyhow!("OpenVINO 识别输入尺寸溢出"))?;
    let mut data = vec![0.0; plane * 3];
    for (y, row) in resized.to_rgb8().rows().enumerate() {
        for (x, pixel) in row.enumerate() {
            let channels = pixel.0;
            for channel in 0..3 {
                data[channel * plane + y * target_width + x] = (channels[channel] as f32 / 255.0
                    - RECOGNITION_CHANNEL_MEAN[channel])
                    / RECOGNITION_CHANNEL_STD[channel];
            }
        }
    }
    Ok(PreprocessedTensor {
        data,
        shape: vec![1, 3, target_height, target_width],
        valid_width,
        valid_height: target_height as u32,
    })
}

fn recognition_valid_sequence_length(
    input: &PreprocessedTensor,
    sequence_length: usize,
    fixed_width: Option<u32>,
) -> Result<usize> {
    if let Some(width) = fixed_width {
        let expected_sequence_length = (width / 8) as usize;
        if width != 768 || sequence_length != expected_sequence_length {
            bail!(
                "OpenVINO 固定识别 profile 要求 48x768、输出 stride 8: timesteps={sequence_length} expected={expected_sequence_length}"
            );
        }
        return Ok(input.valid_width.div_ceil(8) as usize);
    }
    Ok(
        (sequence_length as u64 * input.valid_width as u64).div_ceil(input.shape[3] as u64)
            as usize,
    )
}

fn extract_boxes_with_unclip(mask: &[u8], geometry: DetectionGeometry) -> Vec<DetectedTextBox> {
    let Some(gray_image) =
        GrayImage::from_raw(geometry.mask_width, geometry.mask_height, mask.to_vec())
    else {
        return Vec::new();
    };
    if geometry.valid_width == 0
        || geometry.valid_height == 0
        || geometry.original_width == 0
        || geometry.original_height == 0
    {
        return Vec::new();
    }
    let contours = find_contours::<i32>(&gray_image);
    let scale_x = geometry.original_width as f32 / geometry.valid_width as f32;
    let scale_y = geometry.original_height as f32 / geometry.valid_height as f32;
    let mut boxes = Vec::new();

    for contour in contours {
        if contour.parent.is_some() || contour.points.len() < 4 {
            continue;
        }
        let contour_points =
            contour_points_in_valid_region(&contour, geometry.valid_width, geometry.valid_height);
        if contour_points.len() < 4 {
            continue;
        }
        let Some(rotated_box) = minimum_area_rect(&contour_points) else {
            continue;
        };
        if rotated_box.area() < geometry.min_area as f32 {
            continue;
        }
        let expanded_points = rotated_box
            .expand(geometry.unclip_ratio)
            .clamped_points(geometry.valid_width, geometry.valid_height);
        let scaled_points = scale_and_order_points(
            expanded_points,
            scale_x,
            scale_y,
            geometry.original_width,
            geometry.original_height,
        );
        if let Some(rect) = rect_from_ordered_points(
            &scaled_points,
            geometry.original_width,
            geometry.original_height,
        ) {
            boxes.push(DetectedTextBox { rect });
        }
    }
    boxes
}

fn contour_points_in_valid_region(
    contour: &Contour<i32>,
    valid_width: u32,
    valid_height: u32,
) -> Vec<Point<f32>> {
    let max_x = valid_width.saturating_sub(1) as f32;
    let max_y = valid_height.saturating_sub(1) as f32;
    contour
        .points
        .iter()
        .filter(|point| point.x >= 0 && point.y >= 0)
        .filter(|point| point.x < valid_width as i32 && point.y < valid_height as i32)
        .map(|point| Point::new((point.x as f32).min(max_x), (point.y as f32).min(max_y)))
        .collect()
}

#[derive(Debug, Clone, Copy)]
struct RotatedBox {
    center: Point<f32>,
    width: f32,
    height: f32,
    angle: f32,
}

impl RotatedBox {
    fn area(self) -> f32 {
        self.width * self.height
    }

    fn perimeter(self) -> f32 {
        2.0 * (self.width + self.height)
    }

    fn expand(self, unclip_ratio: f32) -> Self {
        let distance = (self.area() * unclip_ratio / self.perimeter()).max(1.0);
        Self {
            width: self.width + distance * 2.0,
            height: self.height + distance * 2.0,
            ..self
        }
    }

    fn clamped_points(self, valid_width: u32, valid_height: u32) -> [Point<f32>; 4] {
        let cos = self.angle.cos();
        let sin = self.angle.sin();
        let half_width = self.width * 0.5;
        let half_height = self.height * 0.5;
        let corners = [
            (-half_width, -half_height),
            (half_width, -half_height),
            (half_width, half_height),
            (-half_width, half_height),
        ];
        let max_x = valid_width.saturating_sub(1) as f32;
        let max_y = valid_height.saturating_sub(1) as f32;
        order_points(corners.map(|(x, y)| {
            Point::new(
                (self.center.x + x * cos - y * sin).clamp(0.0, max_x),
                (self.center.y + x * sin + y * cos).clamp(0.0, max_y),
            )
        }))
    }
}

fn minimum_area_rect(points: &[Point<f32>]) -> Option<RotatedBox> {
    let hull = convex_hull(points);
    if hull.len() < 3 {
        return None;
    }
    let mut best = None;
    let mut best_area = f32::INFINITY;
    for index in 0..hull.len() {
        let first = hull[index];
        let second = hull[(index + 1) % hull.len()];
        let dx = second.x - first.x;
        let dy = second.y - first.y;
        if dx.abs() < f32::EPSILON && dy.abs() < f32::EPSILON {
            continue;
        }
        let angle = dy.atan2(dx);
        let cos = angle.cos();
        let sin = angle.sin();
        let (mut min_x, mut max_x) = (f32::INFINITY, f32::NEG_INFINITY);
        let (mut min_y, mut max_y) = (f32::INFINITY, f32::NEG_INFINITY);
        for point in &hull {
            let x = point.x * cos + point.y * sin;
            let y = -point.x * sin + point.y * cos;
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
        let width = max_x - min_x;
        let height = max_y - min_y;
        let area = width * height;
        if width <= 0.0 || height <= 0.0 || area >= best_area {
            continue;
        }
        let center_x = (min_x + max_x) * 0.5;
        let center_y = (min_y + max_y) * 0.5;
        best_area = area;
        best = Some(RotatedBox {
            center: Point::new(
                center_x * cos - center_y * sin,
                center_x * sin + center_y * cos,
            ),
            width,
            height,
            angle,
        });
    }
    best
}

fn convex_hull(points: &[Point<f32>]) -> Vec<Point<f32>> {
    let mut sorted = points.to_vec();
    sorted.sort_by(|left, right| {
        left.x
            .partial_cmp(&right.x)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.y.partial_cmp(&right.y).unwrap_or(Ordering::Equal))
    });
    sorted.dedup_by(|left, right| {
        (left.x - right.x).abs() < f32::EPSILON && (left.y - right.y).abs() < f32::EPSILON
    });
    if sorted.len() <= 2 {
        return sorted;
    }
    let mut lower = Vec::new();
    for point in &sorted {
        while lower.len() >= 2
            && cross(lower[lower.len() - 2], lower[lower.len() - 1], *point) <= 0.0
        {
            lower.pop();
        }
        lower.push(*point);
    }
    let mut upper = Vec::new();
    for point in sorted.iter().rev() {
        while upper.len() >= 2
            && cross(upper[upper.len() - 2], upper[upper.len() - 1], *point) <= 0.0
        {
            upper.pop();
        }
        upper.push(*point);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

fn cross(origin: Point<f32>, first: Point<f32>, second: Point<f32>) -> f32 {
    (first.x - origin.x) * (second.y - origin.y) - (first.y - origin.y) * (second.x - origin.x)
}

fn scale_and_order_points(
    points: [Point<f32>; 4],
    scale_x: f32,
    scale_y: f32,
    original_width: u32,
    original_height: u32,
) -> [Point<f32>; 4] {
    let max_x = original_width.saturating_sub(1) as f32;
    let max_y = original_height.saturating_sub(1) as f32;
    order_points(points.map(|point| {
        Point::new(
            (point.x * scale_x).clamp(0.0, max_x),
            (point.y * scale_y).clamp(0.0, max_y),
        )
    }))
}

fn order_points(points: [Point<f32>; 4]) -> [Point<f32>; 4] {
    let mut top_left = points[0];
    let mut top_right = points[0];
    let mut bottom_right = points[0];
    let mut bottom_left = points[0];
    for point in points {
        let sum = point.x + point.y;
        let diff = point.x - point.y;
        if sum < top_left.x + top_left.y {
            top_left = point;
        }
        if sum > bottom_right.x + bottom_right.y {
            bottom_right = point;
        }
        if diff > top_right.x - top_right.y {
            top_right = point;
        }
        if diff < bottom_left.x - bottom_left.y {
            bottom_left = point;
        }
    }
    [top_left, top_right, bottom_right, bottom_left]
}

fn rect_from_ordered_points(
    points: &[Point<f32>; 4],
    original_width: u32,
    original_height: u32,
) -> Option<ImageRect> {
    let min_x = points
        .iter()
        .map(|point| point.x)
        .fold(f32::INFINITY, f32::min)
        .floor()
        .max(0.0) as u32;
    let min_y = points
        .iter()
        .map(|point| point.y)
        .fold(f32::INFINITY, f32::min)
        .floor()
        .max(0.0) as u32;
    let max_x = points
        .iter()
        .map(|point| point.x)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .min(original_width as f32) as u32;
    let max_y = points
        .iter()
        .map(|point| point.y)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .min(original_height as f32) as u32;
    if max_x <= min_x || max_y <= min_y {
        return None;
    }
    Some(ImageRect::at(min_x as i32, min_y as i32).of_size(max_x - min_x, max_y - min_y))
}

fn ctc_output_dimensions(shape: &[usize]) -> Result<(usize, usize)> {
    let (sequence_length, class_count) = match shape.len() {
        2 => (shape[0], shape[1]),
        3 => (shape[1], shape[2]),
        4 => (shape[shape.len() - 2], shape[shape.len() - 1]),
        _ => bail!("OpenVINO 识别输出形状无效: {shape:?}"),
    };
    let batch = shape[..shape.len() - 2]
        .iter()
        .try_fold(1usize, |product, dimension| product.checked_mul(*dimension))
        .ok_or_else(|| anyhow!("OpenVINO 识别输出 batch 尺寸溢出: {shape:?}"))?;
    if batch != 1 {
        bail!("OpenVINO 识别输出必须是单 batch: shape={shape:?}");
    }
    Ok((sequence_length, class_count))
}

fn decode_ctc(
    data: &[f32],
    shape: &[usize],
    charset: &[char],
    valid_sequence_length: Option<usize>,
) -> Result<(String, f32)> {
    let (full_sequence_length, class_count) = ctc_output_dimensions(shape)?;
    let sequence_length = valid_sequence_length
        .unwrap_or(full_sequence_length)
        .min(full_sequence_length);
    if class_count == 0 || sequence_length == 0 {
        return Ok((String::new(), 0.0));
    }
    if class_count != charset.len() {
        bail!(
            "OpenVINO 识别输出类别数与字符集不匹配: classes={} charset={}",
            class_count,
            charset.len()
        );
    }
    let required = full_sequence_length
        .checked_mul(class_count)
        .ok_or_else(|| anyhow!("OpenVINO 识别输出尺寸溢出"))?;
    if data.len() < required {
        bail!(
            "OpenVINO 识别输出数据不足: shape={shape:?} values={}",
            data.len()
        );
    }

    let mut previous = 0usize;
    let mut characters = Vec::new();
    for timestep in 0..sequence_length {
        let row = &data[timestep * class_count..(timestep + 1) * class_count];
        let (max_index, max_value) = row
            .iter()
            .enumerate()
            .max_by(|(_, left), (_, right)| left.total_cmp(right))
            .ok_or_else(|| anyhow!("OpenVINO 识别输出行为空"))?;
        let repeated = max_index == previous;
        previous = max_index; // Low-confidence tokens still participate in CTC collapse.
        if max_index == 0 || repeated {
            continue;
        }
        let score = if row.iter().all(|value| *value >= 0.0 && *value <= 1.0)
            && (row.iter().sum::<f32>() - 1.0).abs() <= 0.05
        {
            *max_value
        } else if max_value.is_finite() {
            // For a finite total_cmp winner, this is also the numeric maximum.
            // Keep summation order and the original exp(value - maximum) expression.
            softmax_probability_with_maximum(*max_value, row, *max_value)
        } else {
            // total_cmp and f32::max differ for NaN; retain the original fallback.
            softmax_probability(*max_value, row)
        };
        if max_index < charset.len() && score >= RECOGNITION_CHARACTER_THRESHOLD {
            characters.push((charset[max_index], score));
        }
    }
    let confidence = if characters.is_empty() {
        0.0
    } else {
        characters.iter().map(|(_, score)| score).sum::<f32>() / characters.len() as f32
    };
    Ok((
        characters.into_iter().map(|(ch, _)| ch).collect(),
        confidence,
    ))
}

fn softmax_probability(value: f32, row: &[f32]) -> f32 {
    let maximum = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    softmax_probability_with_maximum(value, row, maximum)
}

fn softmax_probability_with_maximum(value: f32, row: &[f32], maximum: f32) -> f32 {
    let denominator = row
        .iter()
        .map(|candidate| (*candidate - maximum).exp())
        .sum::<f32>();
    (value - maximum).exp() / denominator.max(f32::MIN_POSITIVE)
}

fn required_path<'a>(path: &'a Option<std::path::PathBuf>, field: &str) -> Result<&'a Path> {
    path.as_deref()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("{field} 未配置"))
}

/// 优先使用应用旁随包提供的运行时，再回退到 `openvino-finder` 的环境和系统搜索路径。
///
/// 依赖包将 OpenVINO 放在 `openvino/runtime/...`，本地开发构建可能直接将 `openvino_c.dll` 放在可执行文件旁。
/// `openvino-sys` 只搜索环境变量和已知系统位置，因此在 `Core::new()` 加载库前先暴露本地布局；本地加载失败后回退到现有 `PATH`/`OPENVINO_INSTALL_DIR` 搜索路径。
fn prepare_local_runtime_loading() {
    static PREPARE: std::sync::Once = std::sync::Once::new();
    PREPARE.call_once(|| {
        #[cfg(windows)]
        {
            let Some(executable_dir) = std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(Path::to_path_buf))
            else {
                log::debug!("无法定位 EXE 目录，继续使用 OpenVINO 环境变量查找");
                return;
            };
            let working_dir = std::env::current_dir().ok();
            let roots = local_runtime_roots(&executable_dir, working_dir.as_deref());
            let search_paths = local_runtime_search_paths(&roots);
            let Some(library) = search_paths
                .iter()
                .map(|directory| directory.join("openvino_c.dll"))
                .find(|path| path.is_file())
            else {
                log::debug!(
                    "EXE 目录未发现本地 OpenVINO runtime，继续使用环境变量查找: {}",
                    executable_dir.display()
                );
                return;
            };

            prepend_process_path(&search_paths);
            match openvino_sys::library::load_from(&library) {
                Ok(()) => log::info!(
                    "已优先加载 EXE 目录中的 OpenVINO runtime: {}",
                    library.display()
                ),
                Err(error) => log::warn!(
                    "加载 EXE 目录中的 OpenVINO runtime 失败，继续使用环境变量查找: path={} error={error}",
                    library.display()
                ),
            }
        }
    });
}

/// 本地运行时查找根：优先 `deps/openvino`（统一依赖布局），兼容旧版 EXE 旁 `openvino/`
/// 与开发构建把 `openvino_c.dll` 直接放 EXE 旁/工作目录的布局。
#[cfg(windows)]
fn local_runtime_roots(executable_dir: &Path, working_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::with_capacity(3);
    push_unique_path(&mut roots, executable_dir.join("deps").join("openvino"));
    push_unique_path(&mut roots, executable_dir.to_path_buf());
    if let Some(working_dir) = working_dir {
        push_unique_path(&mut roots, working_dir.to_path_buf());
    }
    roots
}

#[cfg(windows)]
fn local_runtime_search_paths(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(roots.len() * 8);
    for root in roots {
        for install_root in [root.clone(), root.join("openvino")] {
            push_unique_path(&mut paths, install_root.clone());
            push_unique_path(
                &mut paths,
                install_root
                    .join("runtime")
                    .join("bin")
                    .join("intel64")
                    .join("Release"),
            );
            push_unique_path(
                &mut paths,
                install_root.join("runtime").join("bin").join("intel64"),
            );
            push_unique_path(
                &mut paths,
                install_root
                    .join("runtime")
                    .join("3rdparty")
                    .join("tbb")
                    .join("bin"),
            );
        }
    }
    paths
}

/// 把本地 OpenVINO/TBB 目录加入进程 DLL 搜索路径。
///
/// openvino-sys 通过 libloading 的 flags=0 加载 openvino_c.dll；这种加载方式
/// 不保证使用 AddDllDirectory 注册的用户目录。必须在 OCR 工作线程启动前把
/// 运行时目录加入 PATH，才能让 OpenVINO 的 TBB 与插件依赖按传统规则解析。
#[cfg(windows)]
fn prepend_process_path(paths: &[PathBuf]) {
    let mut ordered = Vec::with_capacity(paths.len());
    for path in paths.iter().filter(|path| path.is_dir()) {
        push_unique_path(&mut ordered, path.clone());
    }
    if ordered.is_empty() {
        return;
    }
    if let Some(existing) = std::env::var_os("PATH") {
        for path in std::env::split_paths(&existing) {
            push_unique_path(&mut ordered, path);
        }
    }
    let Ok(path) = std::env::join_paths(ordered) else {
        log::warn!("无法拼接 OpenVINO 本地 DLL 搜索路径，继续使用原 PATH");
        return;
    };
    // 此函数由 Once 在 OCR 初始化期间调用，早于 OCR 工作线程创建。
    unsafe { std::env::set_var("PATH", path) };
}

#[cfg(windows)]
fn push_unique_path(paths: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !paths.iter().any(|path| path == &candidate) {
        paths.push(candidate);
    }
}

fn path_string(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow!("OpenVINO 模型路径不是有效 UTF-8: {}", path.display()))
}

fn device_type(value: &str) -> DeviceType<'static> {
    DeviceType::from(value.trim().to_ascii_uppercase().as_str()).to_owned()
}

fn configure_cache(core: &mut Core, device: &DeviceType<'static>, cache_dir: Option<&Path>) {
    let Some(cache_dir) = cache_dir else {
        return;
    };
    if let Err(error) = std::fs::create_dir_all(cache_dir) {
        log::warn!(
            "创建 OpenVINO 缓存目录失败，继续无持久化缓存: {} error={error:#}",
            cache_dir.display()
        );
        return;
    }
    let cache_path = cache_dir.to_string_lossy();
    if let Err(error) = core.set_properties(
        device,
        [
            (RwPropertyKey::CacheDir, cache_path.as_ref()),
            // 更大的缓存块可避免下次启动重新编译 GPU 内核。
            (RwPropertyKey::CacheMode, "OPTIMIZE_SPEED"),
        ],
    ) {
        log::warn!(
            "配置 OpenVINO 持久化缓存失败，继续无持久化缓存: device={} dir={} error={error:#}",
            device,
            cache_dir.display()
        );
    } else {
        log::info!(
            "已启用 OpenVINO 持久化缓存: device={} dir={}",
            device,
            cache_dir.display()
        );
    }
}

fn load_charset(path: &Path) -> Result<Vec<char>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取 OCR 字符集失败: {}", path.display()))?;
    let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
    let mut charset = vec![' '];
    charset.extend(content.chars().filter(|ch| *ch != '\n' && *ch != '\r'));
    charset.push(' ');
    if charset.len() < 3 {
        bail!("OCR 字符集过小: {}", path.display());
    }
    Ok(charset)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[cfg(windows)]
    #[test]
    fn local_runtime_search_paths_prioritize_executable_directory() {
        let executable_dir = Path::new(r"C:\app");
        let roots = local_runtime_roots(executable_dir, Some(executable_dir));
        assert_eq!(
            roots,
            vec![
                PathBuf::from(r"C:\app\deps\openvino"),
                PathBuf::from(r"C:\app")
            ]
        );

        let paths = local_runtime_search_paths(&roots);
        assert_eq!(paths[0], PathBuf::from(r"C:\app\deps\openvino"));
        assert_eq!(
            paths[1],
            PathBuf::from(r"C:\app\deps\openvino\runtime\bin\intel64\Release")
        );
        assert_eq!(paths[8], PathBuf::from(r"C:\app"));
        assert_eq!(
            paths[9],
            PathBuf::from(r"C:\app\runtime\bin\intel64\Release")
        );
        assert_eq!(paths[12], PathBuf::from(r"C:\app\openvino"));
    }

    #[test]
    fn charset_accepts_utf8_bom_without_shifting_classes() -> Result<()> {
        let root = std::env::temp_dir().join(format!("miliastra-charset-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root)?;
        let path = root.join("charset.txt");
        let result = (|| -> Result<()> {
            std::fs::write(&path, "香\r\n菜\r\n")?;
            let plain = load_charset(&path)?;
            std::fs::write(&path, "\u{feff}香\r\n菜\r\n")?;
            assert_eq!(load_charset(&path)?, plain);
            assert_eq!(plain, vec![' ', '香', '菜', ' ']);
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(root);
        result
    }

    /// 仅在提供本地模型目录时运行真实 IR 与运行时冒烟检查。
    ///
    /// 这样普通 CI 不依赖单独安装的 OpenVINO 运行时，同时为发布检查提供模型加载、推理、DB 后处理和 CTC 解码的完整验证。
    #[test]
    #[ignore = "requires local OpenVINO runtime and IR models"]
    fn configured_ir_models_recognize_fixture() -> Result<()> {
        let model_root = std::env::var_os("OPENVINO_OCR_IR_ROOT")
            .context("OPENVINO_OCR_IR_ROOT is required for the OpenVINO smoke test")?;
        let model_root = PathBuf::from(model_root);
        let charset = std::env::var_os("OPENVINO_OCR_CHARSET")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("models/ppocr_keys_v6_small.txt"));
        let args = ResolvedOcrArgs {
            det_model: None,
            rec_model: None,
            charset,
            min_confidence: 0.0,
            #[cfg(feature = "ocr-mnn")]
            threads: 4,
            backend_priority: vec!["openvino".to_string()],
            openvino: crate::config::OpenVinoConfig {
                det_model: Some(model_root.join("PP-OCRv6_small_det.xml")),
                det_weights: Some(model_root.join("PP-OCRv6_small_det.bin")),
                rec_model: Some(model_root.join("PP-OCRv6_small_rec.xml")),
                rec_weights: Some(model_root.join("PP-OCRv6_small_rec.bin")),
                rec_fixed_width: None,
                device: "CPU".to_string(),
                cache_dir: None,
            },
            det_max_side_len: 960,
            det_score_threshold: 0.3,
            det_unclip_ratio: 2.0,
            det_min_area: 9,
            det_box_border: 0,
        };
        let mut engine = OpenVinoEngine::new(&args)?;
        let title = image::load_from_memory(include_bytes!("fixtures/chat-title.png"))?;
        let text = super::super::merge_ocr_lines(engine.recognize_lines(&title)?, 12);
        assert!(
            text.contains("香菜"),
            "unexpected OpenVINO title OCR: {text}"
        );
        Ok(())
    }

    #[test]
    fn detection_preprocessing_pads_and_normalizes_in_nchw_order() {
        let image =
            DynamicImage::ImageRgb8(image::RgbImage::from_pixel(1, 1, image::Rgb([255, 0, 0])));
        let tensor = preprocess_for_det(&image).unwrap();
        assert_eq!(tensor.shape, vec![1, 3, 128, 416]);
        assert_eq!(tensor.valid_width, 1);
        assert_eq!(tensor.valid_height, 1);
        let plane = 128 * 416;
        assert!((tensor.data[0] - (1.0 - 0.485) / 0.229).abs() < 1e-6);
        assert!((tensor.data[plane] - (0.0 - 0.456) / 0.224).abs() < 1e-6);
        assert!((tensor.data[plane * 2] - (0.0 - 0.406) / 0.225).abs() < 1e-6);
        assert_eq!(tensor.data[1], 0.0);
        assert_eq!(tensor.data[plane + 1], 0.0);
    }

    #[test]
    fn detection_preprocessing_uses_fixed_chat_canvas_buckets() {
        assert_eq!(detection_canvas_size(320, 72), (416, 128));
        assert_eq!(detection_canvas_size(400, 200), (416, 256));
        assert_eq!(detection_canvas_size(480, 72), (480, 96));
    }

    #[test]
    fn recognition_preprocessing_scales_height_and_uses_minus_one_to_one_range() {
        let image =
            DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 1, image::Rgb([255, 0, 128])));
        let tensor = preprocess_for_rec(&image, 48, None).unwrap();
        assert_eq!(tensor.shape, vec![1, 3, 48, 96]);
        assert_eq!(tensor.valid_width, 96);
        assert!((tensor.data[0] - 1.0).abs() < 1e-6);
        assert!((tensor.data[96 * 48] + 1.0).abs() < 1e-6);
    }

    #[test]
    fn recognition_preprocessing_selects_buckets_and_keeps_valid_width() {
        let image = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            3,
            1,
            image::Rgb([255, 255, 255]),
        ));
        let tensor = preprocess_for_rec(&image, 48, None).unwrap();
        assert_eq!(tensor.shape, vec![1, 3, 48, 192]);
        assert_eq!(tensor.valid_width, 144);
        let plane = 48 * 192;
        assert_eq!(tensor.data[plane - 1], 0.0);
        assert_eq!(tensor.data[plane + 192 - 1], 0.0);
    }

    #[test]
    fn recognition_default_bucket_keeps_uncapped_width_and_rounding() {
        let wide = preprocess_for_rec(&DynamicImage::new_rgb8(1600, 48), 48, None).unwrap();
        assert_eq!(wide.shape, [1, 3, 48, 1600]);
        assert_eq!(wide.valid_width, 1600);
        let tie = preprocess_for_rec(&DynamicImage::new_rgb8(5, 96), 48, None).unwrap();
        assert_eq!(tie.valid_width, 3); // 保留原 bucket 的 half-away-from-zero。
        assert_eq!(tie.shape, [1, 3, 48, 96]);
        assert_eq!(
            recognition_valid_sequence_length(&tie, 12, None).unwrap(),
            1
        );
    }

    #[test]
    fn recognition_fixed768_short_line_keeps_rgb_pixels_and_zero_padding() {
        let image = DynamicImage::ImageRgba8(image::RgbaImage::from_fn(5, 96, |x, y| {
            image::Rgba([(x * 51) as u8, (y * 2) as u8, 128, (x * 40) as u8])
        }));
        let tensor = preprocess_for_rec(&image, 48, Some(768)).unwrap();
        // 5 * (48 / 96) = 2.5；Python round 得 2，bucket 的原规则得 3。
        assert_eq!(tensor.shape, [1, 3, 48, 768]);
        assert_eq!(tensor.valid_width, 2);
        assert_eq!(tensor.valid_height, 48);
        let expected = image::imageops::resize(
            &image.to_rgb8(),
            2,
            48,
            image::imageops::FilterType::Lanczos3,
        );
        let plane = 48 * 768;
        for channel in 0..3 {
            for y in 0..48 {
                for x in 0..768 {
                    let actual = tensor.data[channel * plane + y * 768 + x];
                    let expected = if x < 2 {
                        (expected.get_pixel(x as u32, y as u32)[channel] as f32 / 255.0 - 0.5) / 0.5
                    } else {
                        0.0
                    };
                    assert_eq!(actual, expected, "channel={channel} x={x} y={y}");
                }
            }
        }
        assert_eq!(
            recognition_valid_sequence_length(&tensor, 96, Some(768)).unwrap(),
            1
        );
        assert!(recognition_valid_sequence_length(&tensor, 95, Some(768)).is_err());
        // 2.5 的另一侧 ties-to-even 与至少一像素保护。
        for (width, expected) in [(7, 4), (1, 1)] {
            let tensor =
                preprocess_for_rec(&DynamicImage::new_rgb8(width, 96), 48, Some(768)).unwrap();
            assert_eq!(tensor.valid_width, expected);
        }
    }

    #[test]
    fn recognition_fixed768_long_line_resizes_complete_pixels_instead_of_cropping() {
        let image = DynamicImage::ImageRgb8(image::RgbImage::from_fn(1600, 48, |x, _| {
            if x < 800 {
                image::Rgb([255, 0, 0])
            } else {
                image::Rgb([0, 0, 255])
            }
        }));
        let tensor = preprocess_for_rec(&image, 48, Some(768)).unwrap();
        assert_eq!(tensor.shape, [1, 3, 48, 768]);
        assert_eq!(tensor.valid_width, 768);
        let expected = image::imageops::resize(
            &image.to_rgb8(),
            768,
            48,
            image::imageops::FilterType::Lanczos3,
        );
        let plane = 48 * 768;
        for (x, y, pixel) in expected.enumerate_pixels() {
            for channel in 0..3 {
                assert_eq!(
                    tensor.data[channel * plane + y as usize * 768 + x as usize],
                    (pixel[channel] as f32 / 255.0 - 0.5) / 0.5
                );
            }
        }
        // 右半蓝色仍存在；直接裁 768 像素会只留下红色。
        assert_eq!(tensor.data[767], -1.0);
        assert_eq!(tensor.data[2 * plane + 767], 1.0);
        assert_eq!(
            recognition_valid_sequence_length(&tensor, 96, Some(768)).unwrap(),
            96
        );
    }

    #[test]
    fn recognition_profiles_reject_empty_images_and_invalid_fixed_geometry() {
        for fixed_width in [None, Some(768)] {
            for (width, height) in [(0, 0), (0, 48), (48, 0)] {
                assert!(
                    preprocess_for_rec(&DynamicImage::new_rgb8(width, height), 48, fixed_width)
                        .is_err()
                );
            }
        }
        let image = DynamicImage::new_rgb8(5, 10);
        assert!(preprocess_for_rec(&image, 0, None).is_err());
        assert!(preprocess_for_rec(&image, 32, Some(768)).is_err());
        assert!(preprocess_for_rec(&image, 48, Some(760)).is_err());
    }

    #[test]
    fn recognition_fixed768_decodes_only_ceil_valid_width_over_stride() {
        let input = preprocess_for_rec(&DynamicImage::new_rgb8(9, 48), 48, Some(768)).unwrap();
        let valid = recognition_valid_sequence_length(&input, 96, Some(768)).unwrap();
        assert_eq!(valid, 2);
        let mut data = vec![0.0; 96 * 3];
        for row in data.chunks_exact_mut(3) {
            row[0] = 1.0;
        }
        data[0..3].copy_from_slice(&[0.05, 0.9, 0.05]);
        data[6..9].copy_from_slice(&[0.05, 0.05, 0.9]); // padding 内的“乙”必须丢弃。
        let (text, confidence) =
            decode_ctc(&data, &[1, 96, 3], &[' ', '甲', '乙'], Some(valid)).unwrap();
        assert_eq!(text, "甲");
        assert_eq!(confidence, 0.9);
    }

    #[test]
    fn extraction_keeps_outer_contour_and_ignores_hole() {
        let mut mask = GrayImage::new(32, 32);
        for y in 5..27 {
            for x in 4..28 {
                mask.put_pixel(x, y, image::Luma([255]));
            }
        }
        for y in 10..22 {
            for x in 10..22 {
                mask.put_pixel(x, y, image::Luma([0]));
            }
        }
        let boxes = extract_boxes_with_unclip(
            mask.as_raw(),
            DetectionGeometry {
                mask_width: 32,
                mask_height: 32,
                valid_width: 32,
                valid_height: 32,
                original_width: 32,
                original_height: 32,
                min_area: 9,
                unclip_ratio: 2.0,
            },
        );
        assert_eq!(boxes.len(), 1);
        assert!(boxes[0].rect.width() >= 20);
        assert!(boxes[0].rect.height() >= 20);
    }

    #[test]
    fn decodes_ctc_probabilities_and_skips_repeated_blank_tokens() {
        let charset = vec![' ', '甲', '乙', ' '];
        let data = [
            0.05, 0.9, 0.05, 0.0, 0.1, 0.85, 0.05, 0.0, 0.95, 0.02, 0.02, 0.01, 0.05, 0.05, 0.85,
            0.05,
        ];
        let (text, confidence) = decode_ctc(&data, &[1, 4, 4], &charset, None).unwrap();
        assert_eq!(text, "甲乙");
        assert!(confidence > 0.8);
    }

    #[test]
    fn decodes_ctc_only_within_valid_sequence_length() {
        let charset = vec![' ', '甲', '乙'];
        let data = [0.05, 0.9, 0.05, 0.9, 0.05, 0.05, 0.05, 0.05, 0.9];
        let (text, _) = decode_ctc(&data, &[3, 3], &charset, Some(2)).unwrap();
        assert_eq!(text, "甲");
    }

    #[test]
    fn decodes_logit_output() {
        let charset = vec![' ', '甲', '乙'];
        let data = [0.0, 4.0, 0.0, 4.0, 0.0, 0.0];
        let (text, confidence) = decode_ctc(&data, &[2, 3], &charset, None).unwrap();
        assert_eq!(text, "甲");
        assert!(confidence > 0.9);
    }

    #[test]
    fn rejects_ctc_contract_mismatches_instead_of_dropping_classes() {
        let charset = vec![' ', '甲', '乙'];
        let class_mismatch = decode_ctc(&[0.0; 8], &[2, 4], &charset, None)
            .expect_err("class-count mismatch must be rejected");
        assert!(class_mismatch.to_string().contains("类别数"));

        let batch_mismatch = decode_ctc(&[0.0; 6], &[2, 1, 3], &charset, None)
            .expect_err("multi-batch output must be rejected");
        assert!(batch_mismatch.to_string().contains("单 batch"));
    }
}
