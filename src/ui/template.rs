use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, anyhow};
use image::imageops::FilterType;
use image::{DynamicImage, GrayImage, RgbImage};
use serde::Serialize;

use crate::geometry::{Point, Rect, crop_canvas};
// 命中结果只含几何与分数，已移到几何叶子（runtime 也要用）。
// 这里重导出，既有 `ui::template::TemplateHit` 调用点不必改动。
pub(crate) use crate::geometry::TemplateHit;

static RGB_TEMPLATE_CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<RgbImage>>>> = OnceLock::new();
static GRAY_TEMPLATE_CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<GrayTemplate>>>> = OnceLock::new();
/// 按整数倍降采样后的模板，键为 `(模板路径, 降采样倍数)`。
static DOWNSCALED_GRAY_TEMPLATE_CACHE: OnceLock<Mutex<HashMap<(PathBuf, u32), Arc<GrayTemplate>>>> =
    OnceLock::new();

/// 灰度标准差低于该值的模板视为"纯色/近纯色"：对 SAD 几乎没有区分度。
///
/// 实测 16x16 纯色模板在 256x256 区域里，97.7% 的位置 score >= 0.80、31% >= 0.95，
/// 也就是说 min_score=0.80 时它几乎"匹配一切均匀区域"。
const FLAT_TEMPLATE_STDDEV: f32 = 4.0;

/// 灰度标准差：0 表示纯色。用来衡量模板对 SAD 的区分度。
fn gray_stddev(gray: &GrayImage) -> f32 {
    let values = gray.as_raw();
    if values.is_empty() {
        return 0.0;
    }
    let count = values.len() as f64;
    let mean = values.iter().map(|value| *value as f64).sum::<f64>() / count;
    let variance = values
        .iter()
        .map(|value| {
            let delta = *value as f64 - mean;
            delta * delta
        })
        .sum::<f64>()
        / count;
    variance.sqrt() as f32
}

struct GrayTemplate {
    gray: GrayImage,
    alpha: GrayImage,
    alpha_sum: u64,
    /// alpha 全 255 时可以走纯 SAD 快路径。
    fully_opaque: bool,
    /// 预计算的最大全不透明矩形，用于 SEA 下界剪枝。
    opaque: Option<OpaqueRect>,
    /// 灰度标准差，用于识别缺乏区分度的纯色模板。
    #[allow(dead_code)]
    gray_stddev: f32,
}

pub(crate) fn find_template_hits(
    image: &DynamicImage,
    search_rect: Option<Rect>,
    template_path: &Path,
    threshold: f32,
) -> Result<Vec<TemplateHit>> {
    let haystack = match search_rect {
        Some(rect) => crop_canvas(image, rect)?,
        None => image.clone(),
    };
    let template = cached_gray_template(template_path)?;
    if template.gray.width() > haystack.width() || template.gray.height() > haystack.height() {
        return Ok(Vec::new());
    }
    let base_x = search_rect.map(|rect| rect.x).unwrap_or(0);
    let base_y = search_rect.map(|rect| rect.y).unwrap_or(0);
    let matcher = GrayMatcher::from_template(&template);
    Ok(gray_hits_in_gray(
        &haystack.to_luma8(),
        &matcher,
        threshold,
        (base_x, base_y),
    ))
}

pub(crate) fn find_color_template_hit_groups(
    image: &DynamicImage,
    search_rect: Option<Rect>,
    template_paths: &[&Path],
    threshold: f32,
) -> Result<Vec<Vec<TemplateHit>>> {
    let haystack = match search_rect {
        Some(rect) => crop_canvas(image, rect)?,
        None => image.clone(),
    };
    let haystack_rgb = haystack.to_rgb8();
    template_paths
        .iter()
        .map(|template_path| {
            find_color_template_hits_in_rgb(&haystack_rgb, search_rect, template_path, threshold)
        })
        .collect()
}

fn find_color_template_hits_in_rgb(
    haystack_rgb: &RgbImage,
    search_rect: Option<Rect>,
    template_path: &Path,
    threshold: f32,
) -> Result<Vec<TemplateHit>> {
    let template_rgb = cached_rgb_template(template_path)?;
    if template_rgb.width() > haystack_rgb.width() || template_rgb.height() > haystack_rgb.height()
    {
        return Ok(Vec::new());
    }

    let max_sad = template_rgb.width() as u64 * template_rgb.height() as u64 * 3 * 255;
    let max_allowed_sad = ((1.0 - threshold).clamp(0.0, 1.0) * max_sad as f32) as u64;
    let mut hits = Vec::new();

    for y in 0..=(haystack_rgb.height() - template_rgb.height()) {
        for x in 0..=(haystack_rgb.width() - template_rgb.width()) {
            let sad = color_sad_at(haystack_rgb, &template_rgb, x, y, max_allowed_sad);
            let score = 1.0 - sad as f32 / max_sad as f32;
            if score >= threshold {
                let base_x = search_rect.map(|rect| rect.x).unwrap_or(0);
                let base_y = search_rect.map(|rect| rect.y).unwrap_or(0);
                hits.push(TemplateHit {
                    kind: "template".to_string(),
                    x: base_x + x as i32,
                    y: base_y + y as i32,
                    width: template_rgb.width(),
                    height: template_rgb.height(),
                    score,
                });
            }
        }
    }
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.y.cmp(&right.y))
            .then_with(|| left.x.cmp(&right.x))
    });
    Ok(dedupe_hits(
        hits,
        template_rgb.width().max(8) as i32 / 2,
        template_rgb.height().max(8) as i32 / 2,
    ))
}

fn cached_rgb_template(template_path: &Path) -> Result<Arc<RgbImage>> {
    let cache = RGB_TEMPLATE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .map_err(|_| anyhow!("template cache mutex poisoned"))?;
    if let Some(image) = cache.get(template_path) {
        return Ok(Arc::clone(image));
    }
    let image = image::open(template_path)
        .with_context(|| format!("open template {}", template_path.display()))?
        .to_rgb8();
    let image = Arc::new(image);
    cache.insert(template_path.to_path_buf(), Arc::clone(&image));
    Ok(image)
}

impl GrayTemplate {
    /// 由灰度平面 + alpha 平面构造；`label` 只用于日志。
    /// 降采样版本也走这里，因此掩码/下界/纯色告警的口径与全分辨率模板完全一致。
    fn build(gray: GrayImage, alpha: GrayImage, label: &Path) -> Self {
        // 透明 PNG 边缘不能与实时画面比较；这些像素只是占位，不属于视觉锚点。
        let alpha_sum = alpha.as_raw().iter().map(|value| *value as u64).sum();
        let pixel_count = gray.width() as u64 * gray.height() as u64;
        let fully_opaque = pixel_count > 0 && alpha_sum == pixel_count * 255;
        // SEA 下界只在一块“权重恒为 1”的矩形上成立，因此预先算出模板内最大的全不透明矩形，
        // 之后每次匹配都能零成本地拿到下界。
        let opaque = if fully_opaque {
            Some(OpaqueRect {
                x: 0,
                y: 0,
                width: gray.width(),
                height: gray.height(),
                gray_sum: gray.as_raw().iter().map(|value| *value as u64).sum(),
            })
        } else {
            largest_opaque_rect(&alpha).map(|rect| OpaqueRect {
                gray_sum: sum_gray_rect(&gray, rect.x, rect.y, rect.width, rect.height),
                ..rect
            })
        };
        let gray_stddev = gray_stddev(&gray);
        if gray_stddev < FLAT_TEMPLATE_STDDEV {
            log::warn!(
                "模板 {} 的灰度标准差仅 {:.2}（近纯色），SAD 区分度极低，容易在大面积均匀区域误命中；建议改用多点签名断言",
                label.display(),
                gray_stddev
            );
        }
        Self {
            gray,
            alpha,
            alpha_sum,
            fully_opaque,
            gray_stddev,
            opaque,
        }
    }
}

fn cached_gray_template(template_path: &Path) -> Result<Arc<GrayTemplate>> {
    let cache = GRAY_TEMPLATE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .map_err(|_| anyhow!("template cache mutex poisoned"))?;
    if let Some(template) = cache.get(template_path) {
        return Ok(Arc::clone(template));
    }
    let rgba = image::open(template_path)
        .with_context(|| format!("open template {}", template_path.display()))?
        .to_rgba8();
    let gray = DynamicImage::ImageRgba8(rgba.clone()).to_luma8();
    let alpha = GrayImage::from_raw(
        rgba.width(),
        rgba.height(),
        rgba.pixels().map(|pixel| pixel[3]).collect(),
    )
    .expect("RGBA alpha plane must have the same dimensions");
    let template = Arc::new(GrayTemplate::build(gray, alpha, template_path));
    cache.insert(template_path.to_path_buf(), Arc::clone(&template));
    Ok(template)
}

/// 模板的降采样版本（缓存）。灰度与 alpha 用**与画面相同的滤波器**缩小，
/// 保证两侧口径一致——否则同一内容在两边会落到不同的像素上，分数会无谓地掉。
fn cached_downscaled_gray_template(template_path: &Path, scale: u32) -> Result<Arc<GrayTemplate>> {
    let scale = scale.max(1);
    let cache = DOWNSCALED_GRAY_TEMPLATE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .map_err(|_| anyhow!("template cache mutex poisoned"))?;
    let key = (template_path.to_path_buf(), scale);
    if let Some(template) = cache.get(&key) {
        return Ok(Arc::clone(template));
    }
    let base = cached_gray_template(template_path)?;
    let width = (base.gray.width() / scale).max(1);
    let height = (base.gray.height() / scale).max(1);
    let gray = DynamicImage::ImageLuma8(base.gray.clone())
        .resize_exact(width, height, FilterType::Triangle)
        .to_luma8();
    let alpha = DynamicImage::ImageLuma8(base.alpha.clone())
        .resize_exact(width, height, FilterType::Triangle)
        .to_luma8();
    let template = Arc::new(GrayTemplate::build(gray, alpha, template_path));
    cache.insert(key, Arc::clone(&template));
    Ok(template)
}

/// 一行 RGB 的绝对差之和。
///
/// 这是聊天标记扫描的热点：实测逐字节标量循环只有约 1.8 GB/s。
/// x86_64 上用 SSE2 的 `psadbw`，一条指令算完 16 字节的绝对差求和。
#[cfg(target_arch = "x86_64")]
#[inline]
fn row_abs_diff_sum(haystack_row: &[u8], template_row: &[u8]) -> u64 {
    use std::arch::x86_64::{
        __m128i, _mm_cvtsi128_si64, _mm_loadu_si128, _mm_sad_epu8, _mm_srli_si128,
    };

    let length = haystack_row.len().min(template_row.len());
    let mut total = 0u64;
    let mut offset = 0usize;
    // SAFETY: SSE2 是 x86_64 的基线指令集，不需要运行时检测。
    // `_mm_loadu_si128` 支持未对齐读取；循环条件保证 [offset, offset+16) 同时落在两个切片内。
    // `psadbw` 输出 8 个 16 位部分和，每半区最多 4×8×255 = 8160，转 u64 不会溢出或变号。
    unsafe {
        while offset + 16 <= length {
            let left = _mm_loadu_si128(haystack_row.as_ptr().add(offset).cast::<__m128i>());
            let right = _mm_loadu_si128(template_row.as_ptr().add(offset).cast::<__m128i>());
            let packed = _mm_sad_epu8(left, right);
            total += _mm_cvtsi128_si64(packed) as u64
                + _mm_cvtsi128_si64(_mm_srli_si128(packed, 8)) as u64;
            offset += 16;
        }
    }
    for index in offset..length {
        total += haystack_row[index].abs_diff(template_row[index]) as u64;
    }
    total
}

/// 非 x86_64 的可移植回退：结果与 SIMD 路径逐位一致。
#[cfg(not(target_arch = "x86_64"))]
#[inline]
fn row_abs_diff_sum(haystack_row: &[u8], template_row: &[u8]) -> u64 {
    let length = haystack_row.len().min(template_row.len());
    haystack_row[..length]
        .iter()
        .zip(&template_row[..length])
        .map(|(left, right)| left.abs_diff(*right) as u64)
        .sum()
}

fn color_sad_at(
    haystack: &RgbImage,
    template: &RgbImage,
    x: u32,
    y: u32,
    max_allowed_sad: u64,
) -> u64 {
    let haystack_width = haystack.width() as usize;
    let template_width = template.width() as usize;
    let template_height = template.height() as usize;
    let x = x as usize;
    let y = y as usize;
    let haystack_data = haystack.as_raw();
    let template_data = template.as_raw();
    let mut sad = 0u64;

    let row_bytes = template_width * 3;
    for row in 0..template_height {
        let haystack_offset = ((y + row) * haystack_width + x) * 3;
        let template_offset = row * row_bytes;
        // 整行一次算完再检查：提前退出从逐字节降到行边界，接受/拒绝判定与逐字节退出完全等价
        // （被拒绝的候选两种实现都返回某个大于 max_allowed_sad 的部分和，通过的都是完整 SAD），
        // 换来的是一条指令处理 16 字节。
        sad += row_abs_diff_sum(
            &haystack_data[haystack_offset..haystack_offset + row_bytes],
            &template_data[template_offset..template_offset + row_bytes],
        );
        if sad > max_allowed_sad {
            return sad;
        }
    }
    sad
}

pub(crate) fn best_template_hit(
    image: &DynamicImage,
    search_rect: Option<Rect>,
    template_path: &Path,
    threshold: f32,
) -> Result<Option<TemplateHit>> {
    Ok(best_template_candidate(image, search_rect, template_path)?
        .filter(|candidate| candidate.score >= threshold))
}

/// Check presence without selecting a best position or constructing hit evidence.
pub(crate) fn template_exists(
    image: &DynamicImage,
    search_rect: Option<Rect>,
    template_path: &Path,
    threshold: f32,
) -> Result<bool> {
    if !threshold.is_finite() || threshold < 0.0 || threshold > 1.0 {
        return Ok(best_template_hit(image, search_rect, template_path, threshold)?.is_some());
    }
    let haystack = match search_rect {
        Some(rect) => crop_canvas(image, rect)?,
        None => image.clone(),
    };
    let template = cached_gray_template(template_path)?;
    if template.gray.width() > haystack.width() || template.gray.height() > haystack.height() {
        return Ok(false);
    }
    let matcher = GrayMatcher::from_template(&template);
    Ok(gray_match_exists(&haystack.to_luma8(), &matcher, threshold))
}

pub(crate) fn best_template_candidate(
    image: &DynamicImage,
    search_rect: Option<Rect>,
    template_path: &Path,
) -> Result<Option<TemplateHit>> {
    let haystack = match search_rect {
        Some(rect) => crop_canvas(image, rect)?,
        None => image.clone(),
    };
    let template = cached_gray_template(template_path)?;
    if template.gray.width() > haystack.width() || template.gray.height() > haystack.height() {
        return Ok(None);
    }

    let haystack_gray = haystack.to_luma8();
    let max_sad = template.alpha_sum;
    if max_sad == 0 {
        return Ok(None);
    }
    let matcher = GrayMatcher::from_template(&template);
    let found = best_gray_match(&haystack_gray, &matcher).unwrap_or(GrayMatch {
        x: 0,
        y: 0,
        sad: max_sad.saturating_add(1),
    });

    let score = 1.0 - found.sad as f32 / max_sad as f32;
    let base_x = search_rect.map(|rect| rect.x).unwrap_or(0);
    let base_y = search_rect.map(|rect| rect.y).unwrap_or(0);
    Ok(Some(TemplateHit {
        kind: "template".to_string(),
        x: base_x + found.x as i32,
        y: base_y + found.y as i32,
        width: template.gray.width(),
        height: template.gray.height(),
        score,
    }))
}

/// 与 [`best_template_candidate`] 同样的灰度 SAD，但**先按整数倍降采样再匹配**。
///
/// 用途：区域/模板都很大的探针（代价 ≈ 候选位置数 × 模板像素数）。两个维度同时除以
/// `scale`，总代价降到约 `1/scale⁴`（`scale=4` 即约 256×）。命中坐标乘回 `scale`，
/// 因此仍在**原图坐标系**里，只是位置精度降到 `scale` 像素——对"这个面板在不在"足够，
/// 而需要点击精度的路径（例程）仍走全分辨率的 [`best_template_hit`]，不受影响。
///
/// `scale=1` 时直接退化为全分辨率匹配。
pub(crate) fn best_template_candidate_downscaled(
    image: &DynamicImage,
    search_rect: Option<Rect>,
    template_path: &Path,
    scale: u32,
) -> Result<Option<TemplateHit>> {
    let scale = scale.max(1);
    if scale == 1 {
        return best_template_candidate(image, search_rect, template_path);
    }
    let haystack = match search_rect {
        Some(rect) => crop_canvas(image, rect)?,
        None => image.clone(),
    };
    // 画面侧与模板侧必须用同一个滤波器、同一个倍数缩小，否则同一内容会落到不同像素上。
    let haystack_gray = DynamicImage::ImageLuma8(haystack.to_luma8())
        .resize_exact(
            (haystack.width() / scale).max(1),
            (haystack.height() / scale).max(1),
            FilterType::Triangle,
        )
        .to_luma8();
    let template = cached_downscaled_gray_template(template_path, scale)?;
    if template.gray.width() > haystack_gray.width()
        || template.gray.height() > haystack_gray.height()
    {
        return Ok(None);
    }
    let max_sad = template.alpha_sum;
    if max_sad == 0 {
        return Ok(None);
    }
    let matcher = GrayMatcher::from_template(&template);
    let found = best_gray_match(&haystack_gray, &matcher).unwrap_or(GrayMatch {
        x: 0,
        y: 0,
        sad: max_sad.saturating_add(1),
    });
    let score = 1.0 - found.sad as f32 / max_sad as f32;
    let base_x = search_rect.map(|rect| rect.x).unwrap_or(0);
    let base_y = search_rect.map(|rect| rect.y).unwrap_or(0);
    Ok(Some(TemplateHit {
        kind: "template".to_string(),
        x: base_x + found.x as i32 * scale as i32,
        y: base_y + found.y as i32 * scale as i32,
        width: template.gray.width() * scale,
        height: template.gray.height() * scale,
        score,
    }))
}

/// 一块“模板与画面逐像素比较、权重恒为 1”的矩形，用于 SEA 下界。
#[derive(Clone, Copy)]
struct OpaqueRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    gray_sum: u64,
}

/// 统一的 CPU 模板匹配器：模板灰度 + 可选 alpha 掩码。
///
/// 所有匹配路径（UI 状态分类、各 routine、`/tools/template`、沙盒 Template 断言）都经过这里，
/// 不再有每次调用重建 GPU 设备的固定开销。结果与逐像素朴素扫描逐位一致：只用数学上成立的
/// 下界剪掉不可能获胜的位置，不改变任何分数。
pub(crate) struct GrayMatcher<'a> {
    gray: &'a GrayImage,
    /// `None` 表示模板完全不透明，可以走纯 SAD 快路径。
    alpha: Option<&'a GrayImage>,
    alpha_sum: u64,
    opaque: Option<OpaqueRect>,
}

impl<'a> GrayMatcher<'a> {
    /// 完全不透明模板（没有 alpha 掩码，例如沙盒的内联灰度断言）。
    pub(crate) fn opaque(gray: &'a GrayImage) -> Self {
        let gray_sum = gray.as_raw().iter().map(|value| *value as u64).sum();
        Self {
            gray,
            alpha: None,
            alpha_sum: gray.width() as u64 * gray.height() as u64 * 255,
            opaque: Some(OpaqueRect {
                x: 0,
                y: 0,
                width: gray.width(),
                height: gray.height(),
                gray_sum,
            }),
        }
    }

    fn from_template(template: &'a GrayTemplate) -> Self {
        Self {
            gray: &template.gray,
            alpha: if template.fully_opaque {
                None
            } else {
                Some(&template.alpha)
            },
            alpha_sum: template.alpha_sum,
            opaque: template.opaque,
        }
    }

    /// Σalpha。分数 = 1 - sad / alpha_sum，即 1 - MAD/255。
    pub(crate) fn alpha_sum(&self) -> u64 {
        self.alpha_sum
    }
}

/// 一次匹配的最优位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GrayMatch {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) sad: u64,
}

/// 扫描工作量低于该阈值时不值得付积分图的构建代价。
const SEA_MIN_WORK: u64 = 400_000;

/// GPU 侧 SAD 计算着色器（带边界判断，修掉 crate 的行首污染缺陷）。
const GPU_SAD_SHADER: &str = r#"
struct Uniforms {
    input_width: u32,
    input_height: u32,
    template_width: u32,
    template_height: u32,
    _reserved_a: u32,
    _reserved_b: u32,
    chunks: u32,
    chunk_rows: u32,
};
@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var<storage, read> input_buf: array<u32>;
@group(0) @binding(2) var<storage, read> template_buf: array<u32>;
@group(0) @binding(3) var<storage, read_write> result_buf: array<atomic<u32>>;

@compute @workgroup_size(16, 16, 1)
fn main_sad(@builtin(global_invocation_id) gid: vec3<u32>) {
    let rw = uniforms.input_width - uniforms.template_width + 1u;
    let rh = uniforms.input_height - uniforms.template_height + 1u;
    // 边界判断：缺了它，越界线程会污染下一行的行首条目。
    if (gid.x >= rw || gid.y >= rh) { return; }
    // 把模板行按 gid.z 分片，让每个 (x,y) 由多个线程并行累加：
    // 大模板单个线程要串行 th*tw 次，workgroup 数量太少会让 GPU 严重欠载。
    let row_begin = gid.z * uniforms.chunk_rows;
    let row_end = min(row_begin + uniforms.chunk_rows, uniforms.template_height);
    var total = 0u;
    for (var r = row_begin; r < row_end; r++) {
        let input_base = (gid.y + r) * uniforms.input_width + gid.x;
        let template_base = r * uniforms.template_width;
        for (var c = 0u; c < uniforms.template_width; c++) {
            let a = input_buf[input_base + c];
            let b = template_buf[template_base + c];
            total += max(a, b) - min(a, b);
        }
    }
    atomicAdd(&result_buf[gid.y * rw + gid.x], total);
}
"#;

/// 进程内复用的 GPU 匹配器：设备、管线与缓冲只创建一次。
///
/// 与旧实现的关键差别：template-matching crate 每次调用都新建 wgpu 设备
/// （实测固定开销 210–250ms），这里改为复用，每帧只剩提交与回读。
struct GpuSearcher {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    uniform: wgpu::Buffer,
    input: wgpu::Buffer,
    needle: wgpu::Buffer,
    result: wgpu::Buffer,
    staging: wgpu::Buffer,
    input_bytes: u64,
    needle_bytes: u64,
    result_bytes: u64,
    /// 复用的全零块，用于每次分发前清零原子累加结果图。
    zeroes: Vec<u8>,
}

/// 低于该工作量时直接走 CPU：小模板的 SEA 剪枝比 GPU 的固定开销更划算。
const GPU_MIN_WORK: u64 = 3_000_000;

/// 期望的并行线程数：大模板的瓶颈是单线程串行 th*tw 次导致的占用率不足，
/// 据此把模板行切成若干片交给不同线程并行累加。
const GPU_TARGET_THREADS: u64 = 262_144;

enum GpuState {
    Uninitialized,
    Ready(Box<GpuSearcher>),
    Unavailable,
}

static GPU_STATE: OnceLock<Mutex<GpuState>> = OnceLock::new();

fn u32_bytes(values: &[u32]) -> Vec<u8> {
    // 一次分配 + 定长切片写入：避免逐元素扩容与边界检查，大模板上是主要开销之一。
    let mut bytes = vec![0u8; values.len() * 4];
    for (chunk, value) in bytes.chunks_exact_mut(4).zip(values) {
        chunk.copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

impl GpuSearcher {
    fn buffer(device: &wgpu::Device, size: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: size.max(4),
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// 创建一次。优先独显，其次集显；软件适配器与虚拟显卡不参与（那类情况交给 CPU 兜底）。
    fn create() -> Option<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            dx12_shader_compiler: Default::default(),
        });
        let candidates: Vec<wgpu::Adapter> = instance
            .enumerate_adapters(wgpu::Backends::all())
            .filter(|adapter| {
                matches!(
                    adapter.get_info().device_type,
                    wgpu::DeviceType::DiscreteGpu | wgpu::DeviceType::IntegratedGpu
                )
            })
            .collect();
        // 独显优先，避免多显卡机器上退到集显（实测独显比集显快约 8 倍）。
        let adapter = candidates
            .iter()
            .find(|adapter| adapter.get_info().device_type == wgpu::DeviceType::DiscreteGpu)
            .or_else(|| candidates.first())?;
        let info = adapter.get_info();

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: None,
                features: wgpu::Features::empty(),
                limits: wgpu::Limits::default(),
            },
            None,
        ))
        .ok()?;

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(GPU_SAD_SHADER)),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: None,
            layout: Some(&pipeline_layout),
            module: &module,
            entry_point: "main_sad",
        });

        log::info!(
            "模板匹配 GPU 后端: {} | {:?} | {:?}",
            info.name,
            info.device_type,
            info.backend
        );

        Some(Self {
            uniform: Self::buffer(&device, 32, wgpu::BufferUsages::UNIFORM),
            input: Self::buffer(&device, 4, wgpu::BufferUsages::STORAGE),
            needle: Self::buffer(&device, 4, wgpu::BufferUsages::STORAGE),
            result: Self::buffer(
                &device,
                4,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            ),
            staging: Self::buffer(&device, 4, wgpu::BufferUsages::MAP_READ),
            input_bytes: 4,
            needle_bytes: 4,
            result_bytes: 4,
            zeroes: vec![0u8; 4],
            device,
            queue,
            pipeline,
            layout,
        })
    }

    fn ensure(&mut self, input_bytes: u64, needle_bytes: u64, result_bytes: u64) {
        if input_bytes > self.input_bytes {
            self.input_bytes = input_bytes.next_power_of_two();
            self.input = Self::buffer(&self.device, self.input_bytes, wgpu::BufferUsages::STORAGE);
        }
        if needle_bytes > self.needle_bytes {
            self.needle_bytes = needle_bytes.next_power_of_two();
            self.needle =
                Self::buffer(&self.device, self.needle_bytes, wgpu::BufferUsages::STORAGE);
        }
        if result_bytes > self.result_bytes {
            self.result_bytes = result_bytes.next_power_of_two();
            self.result = Self::buffer(
                &self.device,
                self.result_bytes,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            );
            self.staging = Self::buffer(
                &self.device,
                self.result_bytes,
                wgpu::BufferUsages::MAP_READ,
            );
            self.zeroes = vec![0u8; self.result_bytes as usize];
        }
    }

    /// 上传阶段：确保缓冲容量、写入 uniform/画面/模板，并建立 bind group。
    fn upload(&mut self, input: &[u32], needle: &[u32], dims: [u32; 8]) -> wgpu::BindGroup {
        let (iw, ih, tw, th) = (dims[0], dims[1], dims[2], dims[3]);
        let result_bytes = (iw - tw + 1) as u64 * (ih - th + 1) as u64 * 4;
        self.ensure(
            input.len() as u64 * 4,
            needle.len() as u64 * 4,
            result_bytes,
        );

        self.queue.write_buffer(&self.uniform, 0, &u32_bytes(&dims));
        self.queue.write_buffer(&self.input, 0, &u32_bytes(input));
        self.queue.write_buffer(&self.needle, 0, &u32_bytes(needle));
        // 原子累加前清零结果图。
        self.queue
            .write_buffer(&self.result, 0, &self.zeroes[..result_bytes as usize]);

        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.input.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.needle.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: self.result.as_entire_binding(),
                },
            ],
        })
    }

    /// 计算阶段：提交并等待 GPU，再回读结果图。
    fn compute(&mut self, dims: [u32; 8], bind_group: &wgpu::BindGroup) -> Option<Vec<u32>> {
        let (iw, ih, tw, th) = (dims[0], dims[1], dims[2], dims[3]);
        let chunks = dims[6].max(1);
        let rw = iw - tw + 1;
        let rh = ih - th + 1;
        let result_bytes = rw as u64 * rh as u64 * 4;

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: None });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            pass.dispatch_workgroups(rw.div_ceil(16), rh.div_ceil(16), chunks);
        }
        encoder.copy_buffer_to_buffer(&self.result, 0, &self.staging, 0, result_bytes);
        self.queue.submit(Some(encoder.finish()));

        let slice = self.staging.slice(0..result_bytes);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device.poll(wgpu::Maintain::Wait);
        receiver.recv().ok()?.ok()?;
        let data = slice.get_mapped_range();
        let values: Vec<u32> = data
            .chunks_exact(4)
            .map(|chunk| u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();
        drop(data);
        self.staging.unmap();
        Some(values)
    }

    /// 全图扫描：上传 + 计算。
    fn search(&mut self, input: &[u32], needle: &[u32], dims: [u32; 8]) -> Option<Vec<u32>> {
        let bind_group = self.upload(input, needle, dims);
        self.compute(dims, &bind_group)
    }
}

/// 用 GPU 定位最优点。不可用时返回 None，由 CPU 兜底。
fn gpu_best_position(haystack: &GrayImage, matcher: &GrayMatcher<'_>) -> Option<(u32, u32)> {
    // GPU 侧忽略 alpha，因此只有完全不透明的模板才走 GPU；带掩码的模板交给 CPU。
    // 可用 MILIASTRA_TEMPLATE_MATCH=cpu 强制走 CPU。
    if matcher.alpha.is_some()
        || matches!(
            std::env::var("MILIASTRA_TEMPLATE_MATCH").as_deref(),
            Ok("cpu")
        )
    {
        return None;
    }
    let (iw, ih) = (haystack.width(), haystack.height());
    let (tw, th) = (matcher.gray.width(), matcher.gray.height());
    if tw == 0 || th == 0 || tw > iw || th > ih {
        return None;
    }
    // 工作量太小时 CPU 的 SEA 剪枝已经够快，GPU 的提交+回读固定开销反而更贵。
    // 实测分界：28×19 的标记类模板 CPU 0.11ms 优于 GPU；46×44 以上 GPU 明显更快。
    let positions = (iw - tw + 1) as u64 * (ih - th + 1) as u64;
    if positions * (tw as u64 * th as u64) < GPU_MIN_WORK {
        return None;
    }

    let state = GPU_STATE.get_or_init(|| Mutex::new(GpuState::Uninitialized));
    let mut guard = state.lock().ok()?;
    if matches!(*guard, GpuState::Uninitialized) {
        *guard = match GpuSearcher::create() {
            Some(searcher) => GpuState::Ready(Box::new(searcher)),
            None => {
                log::warn!("没有可用的硬件 GPU 适配器，模板匹配退回 CPU");
                GpuState::Unavailable
            }
        };
    }
    let GpuState::Ready(searcher) = &mut *guard else {
        return None;
    };

    let input: Vec<u32> = haystack.as_raw().iter().map(|v| *v as u32).collect();
    let needle: Vec<u32> = matcher.gray.as_raw().iter().map(|v| *v as u32).collect();
    // 按位置数把模板行分片：位置越少，每个位置分到的线程越多，避免 GPU 欠载。
    let chunks = (GPU_TARGET_THREADS / positions.max(1)).clamp(1, 256) as u32;
    let chunk_rows = th.div_ceil(chunks);
    let dims = [iw, ih, tw, th, 0, 0, chunks, chunk_rows];
    let values = searcher.search(&input, &needle, dims)?;

    let rw = (iw - tw + 1) as usize;
    let mut best = u32::MAX;
    let mut best_index = 0usize;
    for (index, value) in values.iter().enumerate() {
        if *value < best {
            best = *value;
            best_index = index;
        }
    }
    Some(((best_index % rw) as u32, (best_index / rw) as u32))
}

/// 寻找 SAD 最小的位置：优先 GPU（一次全图扫描），CPU 兜底。
///
/// GPU 只负责定位；随后把 GPU 得到的最优值作为初始上界再跑一次 CPU 扫描，
/// 于是既拿到 GPU 的速度，又保持精确的全局最优与并列取栅格序最前的语义。
/// GPU 不可用（无独显/集显、驱动异常、创建失败）或模板带 alpha 掩码时，退回纯 CPU 扫描。
pub(crate) fn best_gray_match(
    haystack: &GrayImage,
    matcher: &GrayMatcher<'_>,
) -> Option<GrayMatch> {
    if let Some((x, y)) = gpu_best_position(haystack, matcher) {
        let sad = sad_at(haystack, matcher, x, y, u64::MAX);
        return cpu_best_gray_match_seeded(haystack, matcher, Some(GrayMatch { x, y, sad }));
    }
    cpu_best_gray_match(haystack, matcher)
}

/// 纯 CPU 的候选扫描。
pub(crate) fn cpu_best_gray_match(
    haystack: &GrayImage,
    matcher: &GrayMatcher<'_>,
) -> Option<GrayMatch> {
    cpu_best_gray_match_seeded(haystack, matcher, None)
}

/// 在整张 `haystack` 上寻找 SAD 最小的位置。
///
/// 语义与朴素栅格扫描完全一致：以 `alpha_sum + 1` 为初始上界，只有严格更小的 SAD 才改写结果，
/// 因此“所有位置都很差”时同样返回 (0, 0) 与 `alpha_sum + 1`；并列时取栅格序最前的位置。
///
/// `seed` 给定时以它作为初始上界（仍然精确：下界大于 seed 的位置不可能更优）。
fn cpu_best_gray_match_seeded(
    haystack: &GrayImage,
    matcher: &GrayMatcher<'_>,
    seed: Option<GrayMatch>,
) -> Option<GrayMatch> {
    if matcher.gray.width() == 0
        || matcher.gray.height() == 0
        || matcher.gray.width() > haystack.width()
        || matcher.gray.height() > haystack.height()
    {
        return None;
    }

    let positions_x = (haystack.width() - matcher.gray.width() + 1) as usize;
    let positions_y = (haystack.height() - matcher.gray.height() + 1) as usize;
    let position_count = positions_x * positions_y;
    let template_area = matcher.gray.width() as u64 * matcher.gray.height() as u64;

    let mut best = seed.unwrap_or(GrayMatch {
        x: 0,
        y: 0,
        sad: matcher.alpha_sum.saturating_add(1),
    });
    // 是否已经有可信的当前最优；决定并列时能否改写位置。
    let mut refined = seed.is_some();
    let mut best_index = seed
        .map(|seed| (seed.y as usize * positions_x) + seed.x as usize)
        .unwrap_or(0);

    let sea_rect = matcher.opaque.filter(|opaque| {
        opaque.width > 0
            && opaque.height > 0
            && position_count as u64 * template_area >= SEA_MIN_WORK
    });

    if let Some(opaque) = sea_rect {
        // 连续消除（SEA）：该矩形内每个像素权重都是 1，于是
        //   SAD >= |Σ画面 - Σ模板| = |盒和 - 模板和|
        // 下界只需一次积分图查表，且与模板大小无关。按下界升序处理，上界会尽快收紧。
        let integral = integral_image(haystack);
        let haystack_width = haystack.width() as usize;
        let mut bounds: Vec<(u32, u32)> = Vec::with_capacity(position_count);
        for y in 0..positions_y {
            for x in 0..positions_x {
                let box_sum = integral_rect(
                    &integral,
                    haystack_width,
                    x + opaque.x as usize,
                    y + opaque.y as usize,
                    opaque.width as usize,
                    opaque.height as usize,
                );
                let bound = box_sum.abs_diff(opaque.gray_sum);
                // Keep equality: an earlier raster position may tie the verified GPU seed.
                if seed.is_some_and(|seed| bound > seed.sad) {
                    continue;
                }
                bounds.push((
                    bound.min(u32::MAX as u64) as u32,
                    (y * positions_x + x) as u32,
                ));
            }
        }
        bounds.sort_unstable_by_key(|entry| entry.0);

        for (bound, index) in bounds {
            let bound = bound as u64;
            // 下界大于当前上界的位置不可能严格更优，其后只会更差。
            if refined && bound > best.sad {
                break;
            }
            let index = index as usize;
            let x = (index % positions_x) as u32;
            let y = (index / positions_x) as u32;
            let sad = sad_at(haystack, matcher, x, y, best.sad);
            if sad < best.sad || (refined && sad == best.sad && index < best_index) {
                best = GrayMatch { x, y, sad };
                best_index = index;
                refined = true;
            }
        }
        return Some(best);
    }

    for y in 0..positions_y {
        for x in 0..positions_x {
            let index = y * positions_x + x;
            let sad = sad_at(haystack, matcher, x as u32, y as u32, best.sad);
            if sad < best.sad || (refined && sad == best.sad && index < best_index) {
                best = GrayMatch {
                    x: x as u32,
                    y: y as u32,
                    sad,
                };
                best_index = index;
                refined = true;
            }
        }
    }
    Some(best)
}

// Invert the final f32 comparison, not real-number arithmetic: rounding can accept
// an integer SAD just above ((1 - threshold) * alpha_sum) truncated to u64.
fn presence_sad_limit(alpha_sum: u64, threshold: f32) -> Option<u64> {
    let accepts = |sad: u64| 1.0 - sad as f32 / alpha_sum as f32 >= threshold;
    if alpha_sum == 0 || !accepts(0) {
        return None;
    }
    let (mut low, mut high) = (0, alpha_sum);
    while low < high {
        let middle = low + (high - low) / 2 + (high - low) % 2;
        if accepts(middle) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    Some(low)
}

fn gray_match_exists(haystack: &GrayImage, matcher: &GrayMatcher<'_>, threshold: f32) -> bool {
    if matcher.gray.width() == 0
        || matcher.gray.height() == 0
        || matcher.gray.width() > haystack.width()
        || matcher.gray.height() > haystack.height()
    {
        return false;
    }
    let Some(limit) = presence_sad_limit(matcher.alpha_sum, threshold) else {
        return false;
    };
    if let Some((x, y)) = gpu_best_position(haystack, matcher) {
        // GPU is only a hint. A rejected hint (or unavailable GPU) still scans CPU.
        if sad_at(haystack, matcher, x, y, limit) <= limit {
            return true;
        }
    }
    let positions_x = haystack.width() - matcher.gray.width() + 1;
    let positions_y = haystack.height() - matcher.gray.height() + 1;
    let sea_rect = matcher.opaque.filter(|opaque| {
        opaque.width > 0
            && opaque.height > 0
            && positions_x as u64
                * positions_y as u64
                * matcher.gray.width() as u64
                * matcher.gray.height() as u64
                >= SEA_MIN_WORK
    });
    let integral = sea_rect.map(|_| integral_image(haystack));
    for y in 0..positions_y {
        for x in 0..positions_x {
            if let (Some(opaque), Some(integral)) = (sea_rect, integral.as_ref()) {
                let box_sum = integral_rect(
                    integral,
                    haystack.width() as usize,
                    (x + opaque.x) as usize,
                    (y + opaque.y) as usize,
                    opaque.width as usize,
                    opaque.height as usize,
                );
                if box_sum.abs_diff(opaque.gray_sum) > limit {
                    continue;
                }
            }
            if sad_at(haystack, matcher, x, y, limit) <= limit {
                return true;
            }
        }
    }
    false
}

/// 收集区域内全部分数达到阈值的位置（`/tools/template` 的命中列表）。
fn gray_hits_in_gray(
    haystack: &GrayImage,
    matcher: &GrayMatcher<'_>,
    threshold: f32,
    base: (i32, i32),
) -> Vec<TemplateHit> {
    let max_sad = matcher.alpha_sum;
    if max_sad == 0
        || matcher.gray.width() == 0
        || matcher.gray.height() == 0
        || matcher.gray.width() > haystack.width()
        || matcher.gray.height() > haystack.height()
    {
        return Vec::new();
    }
    let max_allowed_sad = ((1.0 - threshold).clamp(0.0, 1.0) * max_sad as f32) as u64;
    let mut hits = Vec::new();

    for y in 0..=(haystack.height() - matcher.gray.height()) {
        for x in 0..=(haystack.width() - matcher.gray.width()) {
            let sad = sad_at(haystack, matcher, x, y, max_allowed_sad);
            let score = 1.0 - sad as f32 / max_sad as f32;
            if score >= threshold {
                hits.push(TemplateHit {
                    kind: "template".to_string(),
                    x: base.0 + x as i32,
                    y: base.1 + y as i32,
                    width: matcher.gray.width(),
                    height: matcher.gray.height(),
                    score,
                });
            }
        }
    }
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.y.cmp(&right.y))
            .then_with(|| left.x.cmp(&right.x))
    });
    dedupe_hits(
        hits,
        matcher.gray.width().max(8) as i32 / 2,
        matcher.gray.height().max(8) as i32 / 2,
    )
}

/// 单点 SAD。`max_allowed` 是当前上界：一旦超出就立刻返回（部分失真消除，PDE），
/// 返回值此时必然大于上界，调用方据此判定该位置不可能获胜。
fn sad_at(
    haystack: &GrayImage,
    matcher: &GrayMatcher<'_>,
    x: u32,
    y: u32,
    max_allowed: u64,
) -> u64 {
    let haystack_width = haystack.width() as usize;
    let template_width = matcher.gray.width() as usize;
    let template_height = matcher.gray.height() as usize;
    let x = x as usize;
    let y = y as usize;
    let haystack_data = haystack.as_raw();
    let template_data = matcher.gray.as_raw();
    let mut sad = 0u64;

    match matcher.alpha {
        // 完全不透明：内层是连续切片，没有逐像素的 alpha 读写，便于编译器向量化。
        None => {
            for row in 0..template_height {
                let haystack_offset = (y + row) * haystack_width + x;
                let template_offset = row * template_width;
                let haystack_row =
                    &haystack_data[haystack_offset..haystack_offset + template_width];
                let template_row =
                    &template_data[template_offset..template_offset + template_width];
                for (haystack_pixel, template_pixel) in haystack_row.iter().zip(template_row) {
                    sad += haystack_pixel.abs_diff(*template_pixel) as u64;
                }
                if sad > max_allowed {
                    return sad;
                }
            }
        }
        // 带 alpha：逐像素公式与改造前完全一致（含整数除法与截断顺序）。
        Some(alpha) => {
            let alpha_data = alpha.as_raw();
            for row in 0..template_height {
                let haystack_offset = (y + row) * haystack_width + x;
                let template_offset = row * template_width;
                for column in 0..template_width {
                    let weight = alpha_data[template_offset + column] as u64;
                    if weight == 0 {
                        continue;
                    }
                    sad += haystack_data[haystack_offset + column]
                        .abs_diff(template_data[template_offset + column])
                        as u64
                        * weight
                        / 255;
                    if sad > max_allowed {
                        return sad;
                    }
                }
            }
        }
    }
    sad
}

/// 积分图（前缀和）。u32 足够：1920×1080×255 仍小于 2^32。
fn integral_image(image: &GrayImage) -> Vec<u32> {
    let width = image.width() as usize;
    let height = image.height() as usize;
    let stride = width + 1;
    let data = image.as_raw();
    let mut integral = vec![0u32; stride * (height + 1)];

    for row in 0..height {
        let mut row_sum = 0u32;
        for column in 0..width {
            row_sum += data[row * width + column] as u32;
            integral[(row + 1) * stride + column + 1] =
                integral[row * stride + column + 1] + row_sum;
        }
    }
    integral
}

fn integral_rect(
    integral: &[u32],
    image_width: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> u64 {
    let stride = image_width + 1;
    let top_left = integral[y * stride + x] as u64;
    let top_right = integral[y * stride + x + width] as u64;
    let bottom_left = integral[(y + height) * stride + x] as u64;
    let bottom_right = integral[(y + height) * stride + x + width] as u64;
    bottom_right + top_left - top_right - bottom_left
}

fn sum_gray_rect(image: &GrayImage, x: u32, y: u32, width: u32, height: u32) -> u64 {
    let stride = image.width() as usize;
    let data = image.as_raw();
    let mut sum = 0u64;
    for row in 0..height as usize {
        let offset = (y as usize + row) * stride + x as usize;
        for column in 0..width as usize {
            sum += data[offset + column] as u64;
        }
    }
    sum
}

/// 模板内最大的全 255 轴对齐矩形（直方图法），没有则返回 `None`。
/// 只在模板首次载入时算一次。
fn largest_opaque_rect(alpha: &GrayImage) -> Option<OpaqueRect> {
    let width = alpha.width() as usize;
    let height = alpha.height() as usize;
    if width == 0 || height == 0 {
        return None;
    }
    let data = alpha.as_raw();
    let mut heights = vec![0usize; width];
    let mut stack: Vec<usize> = Vec::with_capacity(width + 1);
    let mut best: Option<(usize, usize, usize, usize)> = None;

    for row in 0..height {
        for column in 0..width {
            heights[column] = if data[row * width + column] == 255 {
                heights[column] + 1
            } else {
                0
            };
        }
        stack.clear();
        let mut column = 0usize;
        while column <= width {
            let current = if column == width { 0 } else { heights[column] };
            while let Some(&top) = stack.last() {
                if heights[top] <= current {
                    break;
                }
                stack.pop();
                let rect_height = heights[top];
                let left = stack.last().map(|&index| index + 1).unwrap_or(0);
                let rect_width = column - left;
                if rect_height > 0 {
                    let area = rect_height * rect_width;
                    let best_area = best.map(|(_, _, w, h)| w * h).unwrap_or(0);
                    if area > best_area {
                        best = Some((left, row + 1 - rect_height, rect_width, rect_height));
                    }
                }
            }
            stack.push(column);
            column += 1;
        }
    }

    best.map(|(x, y, width, height)| OpaqueRect {
        x: x as u32,
        y: y as u32,
        width: width as u32,
        height: height as u32,
        gray_sum: 0,
    })
}

/// 改造前的逐像素朴素扫描。仅用于测试，作为优化实现的对照基准。
#[cfg(test)]
fn gray_sad_at(
    haystack: &GrayImage,
    template: &GrayImage,
    alpha: &GrayImage,
    x: u32,
    y: u32,
    max_allowed_sad: u64,
) -> u64 {
    let haystack_width = haystack.width() as usize;
    let template_width = template.width() as usize;
    let template_height = template.height() as usize;
    let x = x as usize;
    let y = y as usize;
    let haystack_data = haystack.as_raw();
    let template_data = template.as_raw();
    let mut sad = 0u64;

    for row in 0..template_height {
        let haystack_offset = (y + row) * haystack_width + x;
        let template_offset = row * template_width;
        for column in 0..template_width {
            let alpha = alpha.as_raw()[template_offset + column] as u64;
            if alpha == 0 {
                continue;
            }
            sad += haystack_data[haystack_offset + column]
                .abs_diff(template_data[template_offset + column]) as u64
                * alpha
                / 255;
            if sad > max_allowed_sad {
                return sad;
            }
        }
    }
    sad
}

pub(crate) fn dedupe_hits(
    mut hits: Vec<TemplateHit>,
    tolerance_x: i32,
    tolerance_y: i32,
) -> Vec<TemplateHit> {
    let mut picked: Vec<TemplateHit> = Vec::new();
    hits.sort_by(|left, right| right.score.total_cmp(&left.score));
    for hit in hits {
        if picked.iter().any(|existing| {
            (existing.x - hit.x).abs() <= tolerance_x && (existing.y - hit.y).abs() <= tolerance_y
        }) {
            continue;
        }
        picked.push(hit);
    }
    picked.sort_by(compare_hits_top_left);
    picked
}

fn compare_hits_top_left(left: &TemplateHit, right: &TemplateHit) -> Ordering {
    (left.y / 10)
        .cmp(&(right.y / 10))
        .then_with(|| left.x.cmp(&right.x))
        .then_with(|| left.y.cmp(&right.y))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use image::{DynamicImage, GenericImageView, Luma, Rgb, RgbImage, Rgba, RgbaImage};
    use template_matching::{
        Image as MatchImage, MatchTemplateMethod, TemplateMatcher, match_template,
    };

    use super::*;

    #[test]
    fn best_template_hit_finds_best_gray_sad_match() {
        let mut haystack = GrayImage::from_pixel(6, 5, Luma([0]));
        let template = GrayImage::from_fn(2, 2, |x, y| match (x, y) {
            (0, 0) => Luma([10]),
            (1, 0) => Luma([40]),
            (0, 1) => Luma([90]),
            _ => Luma([160]),
        });
        for y in 0..template.height() {
            for x in 0..template.width() {
                haystack.put_pixel(3 + x, 2 + y, *template.get_pixel(x, y));
            }
        }

        let path = std::env::temp_dir().join(format!(
            "miliastra-template-test-{}-{}.png",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        template.save(&path).expect("save template");

        let hit = best_template_hit(&DynamicImage::ImageLuma8(haystack), None, &path, 0.99)
            .expect("match template")
            .expect("template hit");
        let below_threshold = DynamicImage::ImageLuma8(GrayImage::from_pixel(3, 3, Luma([0])));
        let candidate = best_template_candidate(&below_threshold, None, &path)
            .expect("inspect candidate")
            .expect("best candidate even below threshold");
        let rejected =
            best_template_hit(&below_threshold, None, &path, 0.99).expect("thresholded candidate");
        for threshold in [-1.0, -0.001, 0.0, 1.01, f32::NAN, f32::NEG_INFINITY] {
            assert_eq!(
                template_exists(&below_threshold, None, &path, threshold).unwrap(),
                best_template_hit(&below_threshold, None, &path, threshold)
                    .unwrap()
                    .is_some(),
                "threshold={threshold}"
            );
        }
        let _ = fs::remove_file(&path);

        assert_eq!((hit.x, hit.y), (3, 2));
        assert_eq!((hit.width, hit.height), (2, 2));
        assert!(hit.score > 0.999);
        assert!(candidate.score < 0.99);
        assert!(rejected.is_none());
    }

    #[test]
    fn best_template_hit_ignores_transparent_template_background() {
        let mut haystack = RgbImage::from_pixel(8, 7, Rgb([24, 96, 160]));
        let mut template = RgbaImage::from_pixel(3, 3, Rgba([0, 0, 0, 0]));
        template.put_pixel(1, 0, Rgba([255, 255, 255, 255]));
        template.put_pixel(0, 1, Rgba([220, 220, 220, 255]));
        template.put_pixel(1, 1, Rgba([255, 255, 255, 255]));
        template.put_pixel(2, 1, Rgba([220, 220, 220, 255]));
        template.put_pixel(1, 2, Rgba([255, 255, 255, 255]));
        for (x, y, pixel) in [
            (1, 0, [255, 255, 255]),
            (0, 1, [220, 220, 220]),
            (1, 1, [255, 255, 255]),
            (2, 1, [220, 220, 220]),
            (1, 2, [255, 255, 255]),
        ] {
            haystack.put_pixel(3 + x, 2 + y, Rgb(pixel));
        }

        let path = std::env::temp_dir().join(format!(
            "miliastra-transparent-template-test-{}-{}.png",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        template.save(&path).expect("save transparent template");
        let hit = best_template_hit(&DynamicImage::ImageRgb8(haystack), None, &path, 0.99)
            .expect("match transparent template")
            .expect("transparent template should match");
        let _ = fs::remove_file(&path);

        assert_eq!((hit.x, hit.y), (3, 2));
        assert!(hit.score > 0.999);
    }

    #[test]
    fn presence_api_matches_best_and_preserves_crop_load_errors() {
        let path = std::env::temp_dir().join(format!(
            "miliastra-presence-test-{}-{}.png",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let template = RgbaImage::from_fn(3, 2, |x, y| {
            let value = (x * 50 + y * 30) as u8;
            Rgba([value, value, value, [0, 73, 255][x as usize]])
        });
        template.save(&path).unwrap();
        let image = DynamicImage::ImageLuma8(synthetic_gray(8, 6, 91));
        for rect in [
            None,
            Some(Rect::new(2, 1, 4, 3)),
            Some(Rect::new(0, 0, 1, 1)),
        ] {
            let candidate = best_template_candidate(&image, rect, &path).unwrap();
            let score = candidate.as_ref().map(|hit| hit.score).unwrap_or(0.5);
            for threshold in [
                score,
                score.next_up(),
                score.next_down(),
                0.0,
                1.0,
                f32::NAN,
                f32::NEG_INFINITY,
                f32::INFINITY,
            ] {
                assert_eq!(
                    template_exists(&image, rect, &path, threshold).unwrap(),
                    best_template_hit(&image, rect, &path, threshold)
                        .unwrap()
                        .is_some()
                );
            }
        }
        let first = cached_rgb_template(&path).unwrap();
        let second = cached_rgb_template(&path).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.as_ref(), &image::open(&path).unwrap().to_rgb8());
        fs::remove_file(&path).unwrap();
        let missing = path.with_extension("missing.png");
        assert!(template_exists(&image, None, &missing, 0.5).is_err());
        let invalid = Some(Rect::new(50, 50, 2, 2));
        assert!(template_exists(&image, invalid, &path, 0.5).is_err());
        assert!(best_template_hit(&image, invalid, &path, 0.5).is_err());
    }

    // Pre-O5 SEA reference: collect every bound before sorting.
    fn old_seeded_sea(
        haystack: &GrayImage,
        matcher: &GrayMatcher<'_>,
        seed: GrayMatch,
    ) -> GrayMatch {
        let opaque = matcher.opaque.expect("SEA rectangle");
        let positions_x = (haystack.width() - matcher.gray.width() + 1) as usize;
        let positions_y = (haystack.height() - matcher.gray.height() + 1) as usize;
        let integral = integral_image(haystack);
        let mut bounds = Vec::with_capacity(positions_x * positions_y);
        for y in 0..positions_y {
            for x in 0..positions_x {
                let sum = integral_rect(
                    &integral,
                    haystack.width() as usize,
                    x + opaque.x as usize,
                    y + opaque.y as usize,
                    opaque.width as usize,
                    opaque.height as usize,
                );
                bounds.push((
                    sum.abs_diff(opaque.gray_sum).min(u32::MAX as u64) as u32,
                    (y * positions_x + x) as u32,
                ));
            }
        }
        bounds.sort_unstable_by_key(|entry| entry.0);
        let mut best = seed;
        let mut best_index = seed.y as usize * positions_x + seed.x as usize;
        for (bound, index) in bounds {
            if bound as u64 > best.sad {
                break;
            }
            let index = index as usize;
            let x = (index % positions_x) as u32;
            let y = (index / positions_x) as u32;
            let sad = sad_at(haystack, matcher, x, y, best.sad);
            if sad < best.sad || (sad == best.sad && index < best_index) {
                best = GrayMatch { x, y, sad };
                best_index = index;
            }
        }
        best
    }

    fn synthetic_gray(width: u32, height: u32, mut seed: u32) -> GrayImage {
        GrayImage::from_fn(width, height, |_, _| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            Luma([(seed >> 24) as u8])
        })
    }

    #[test]
    fn seeded_sea_prefilter_matches_old_and_raster_reference() {
        for case in 0..12 {
            let haystack = synthetic_gray(64, 64, 123 + case);
            let gray = synthetic_gray(16, 16, 456 + case);
            let alpha = GrayImage::from_fn(16, 16, |x, y| {
                Luma([if case % 2 == 0 || (x < 12 && y < 12) {
                    255
                } else {
                    73
                }])
            });
            let template = GrayTemplate::build(gray, alpha, Path::new("synthetic"));
            let matcher = GrayMatcher::from_template(&template);
            let expected = reference_best(
                &haystack,
                &template.gray,
                &template.alpha,
                template.alpha_sum,
            );
            for (x, y) in [(48, 48), (23, 17), (expected.0, expected.1)] {
                let seed = GrayMatch {
                    x,
                    y,
                    sad: sad_at(&haystack, &matcher, x, y, u64::MAX),
                };
                let actual = cpu_best_gray_match_seeded(&haystack, &matcher, Some(seed)).unwrap();
                assert_eq!(actual, old_seeded_sea(&haystack, &matcher, seed));
                assert_eq!((actual.x, actual.y, actual.sad), expected);
            }
        }
        // Every bound equals seed SAD; rejecting equality would lose the earlier tie.
        let haystack = GrayImage::from_pixel(64, 64, Luma([11]));
        let gray = GrayImage::from_pixel(16, 16, Luma([10]));
        let matcher = GrayMatcher::opaque(&gray);
        let seed = GrayMatch {
            x: 48,
            y: 48,
            sad: 256,
        };
        assert_eq!(
            cpu_best_gray_match_seeded(&haystack, &matcher, Some(seed)),
            Some(GrayMatch {
                x: 0,
                y: 0,
                sad: 256
            })
        );
    }

    #[test]
    fn presence_limit_preserves_final_f32_boundary() {
        for alpha_sum in [1, 73, 255, 1020, 65_280, 16_777_217, 1_000_000_000] {
            for sad in [0, 1, alpha_sum / 3, alpha_sum / 2, alpha_sum - 1, alpha_sum] {
                let score = 1.0 - sad as f32 / alpha_sum as f32;
                for threshold in [
                    score,
                    score.next_up(),
                    score.next_down(),
                    f32::NAN,
                    f32::INFINITY,
                    f32::NEG_INFINITY,
                    -0.1,
                    1.1,
                ] {
                    assert_eq!(
                        presence_sad_limit(alpha_sum, threshold).is_some_and(|limit| sad <= limit),
                        score >= threshold,
                        "alpha={alpha_sum} sad={sad} threshold={threshold}"
                    );
                }
            }
        }
        assert_eq!(presence_sad_limit(0, -1.0), None);
        let score = 1.0 - 1.0f32 / 255.0;
        assert_eq!(((1.0 - score) * 255.0) as u64, 0);
        assert_eq!(presence_sad_limit(255, score), Some(1));
    }

    #[test]
    fn presence_matches_best_for_alpha_floor_sea_and_invalid_dimensions() {
        for case in 0..16 {
            let large = case >= 8;
            let haystack = synthetic_gray(
                if large { 64 } else { 13 },
                if large { 64 } else { 11 },
                700 + case,
            );
            let size = if large { 16 } else { 3 };
            let gray = synthetic_gray(size, size, 900 + case);
            let alpha = GrayImage::from_fn(size, size, |x, y| {
                Luma([match case % 4 {
                    0 => 255,
                    1 if x < size - 1 && y < size - 1 => 255,
                    1 => 73,
                    2 => [0, 1, 73, 254][((x + y) % 4) as usize],
                    _ => 0,
                }])
            });
            let template = GrayTemplate::build(gray, alpha, Path::new("presence"));
            let matcher = GrayMatcher::from_template(&template);
            let expected = reference_best(
                &haystack,
                &template.gray,
                &template.alpha,
                template.alpha_sum,
            );
            let score = 1.0 - expected.2 as f32 / template.alpha_sum as f32;
            for threshold in [
                0.0,
                0.5,
                0.9,
                1.0,
                score,
                score.next_up(),
                score.next_down(),
                f32::NAN,
                f32::NEG_INFINITY,
                f32::INFINITY,
                -0.1,
                1.1,
            ] {
                assert_eq!(
                    gray_match_exists(&haystack, &matcher, threshold),
                    template.alpha_sum > 0 && score >= threshold,
                    "case={case} threshold={threshold}"
                );
            }
        }
        let gray = GrayImage::from_pixel(1, 1, Luma([0]));
        let alpha = GrayImage::from_pixel(1, 1, Luma([73]));
        let template = GrayTemplate::build(gray, alpha, Path::new("floor"));
        let matcher = GrayMatcher::from_template(&template);
        assert!(gray_match_exists(
            &GrayImage::from_pixel(1, 1, Luma([3])),
            &matcher,
            1.0
        ));
        assert!(!gray_match_exists(
            &GrayImage::from_pixel(1, 1, Luma([4])),
            &matcher,
            1.0
        ));
        assert!(!gray_match_exists(&GrayImage::new(0, 0), &matcher, -1.0));
        assert!(!gray_match_exists(
            &template.gray,
            &GrayMatcher::opaque(&GrayImage::new(0, 0)),
            -1.0
        ));
    }

    /// Run explicitly in release, single-threaded; no external fixtures or production switch.
    #[test]
    #[ignore]
    fn template_optimization_old_vs_new_bench() {
        use std::hint::black_box;
        fn measure(mut run: impl FnMut(), rounds: usize) -> f64 {
            for _ in 0..10 {
                run();
            }
            let start = std::time::Instant::now();
            for _ in 0..rounds {
                run();
            }
            start.elapsed().as_secs_f64() * 1_000_000.0 / rounds as f64
        }
        assert!(!cfg!(debug_assertions), "run this benchmark with --release");
        let haystack = synthetic_gray(128, 96, 42);
        let gray = image::imageops::crop_imm(&haystack, 20, 10, 24, 24).to_image();
        let matcher = GrayMatcher::opaque(&gray);
        let seed = GrayMatch {
            x: 20,
            y: 10,
            sad: 0,
        };
        assert_eq!(
            old_seeded_sea(&haystack, &matcher, seed),
            cpu_best_gray_match_seeded(&haystack, &matcher, Some(seed)).unwrap()
        );
        let old = measure(
            || {
                black_box(old_seeded_sea(&haystack, &matcher, seed));
            },
            200,
        );
        let new = measure(
            || {
                black_box(cpu_best_gray_match_seeded(&haystack, &matcher, Some(seed)));
            },
            200,
        );
        println!(
            "seed SEA old={old:.3}us new={new:.3}us ratio={:.3}",
            new / old
        );
        // Mask one pixel so this CPU existence benchmark never initializes GPU.
        let mut alpha = GrayImage::from_pixel(24, 24, Luma([255]));
        alpha.put_pixel(23, 23, Luma([254]));
        let template = GrayTemplate::build(gray, alpha, Path::new("bench"));
        let matcher = GrayMatcher::from_template(&template);
        for (label, image) in [
            ("positive", haystack),
            ("negative", GrayImage::from_pixel(128, 96, Luma([0]))),
        ] {
            let old_presence = || {
                cpu_best_gray_match(&image, &matcher)
                    .is_some_and(|hit| 1.0 - hit.sad as f32 / matcher.alpha_sum as f32 >= 0.99)
            };
            assert_eq!(old_presence(), gray_match_exists(&image, &matcher, 0.99));
            let old = measure(
                || {
                    black_box(old_presence());
                },
                200,
            );
            let new = measure(
                || {
                    black_box(gray_match_exists(&image, &matcher, 0.99));
                },
                200,
            );
            println!(
                "exists {label} old={old:.3}us new={new:.3}us ratio={:.3}",
                new / old
            );
        }
        let rgb = Arc::new(RgbImage::from_fn(256, 256, |x, y| {
            Rgb([x as u8, y as u8, (x + y) as u8])
        }));
        assert_eq!(rgb.as_ref(), &rgb.as_ref().clone());
        let old = measure(
            || {
                black_box(rgb.as_ref().clone());
            },
            25_000,
        );
        let new = measure(
            || {
                black_box(Arc::clone(&rgb));
            },
            25_000,
        );
        println!(
            "RGB cache clone old={old:.3}us new={new:.3}us ratio={:.3}",
            new / old
        );
    }

    /// 改造前的朴素扫描，作为逐位对照的基准。
    fn reference_best(
        haystack: &GrayImage,
        gray: &GrayImage,
        alpha: &GrayImage,
        alpha_sum: u64,
    ) -> (u32, u32, u64) {
        let mut best_sad = alpha_sum.saturating_add(1);
        let mut best = (0u32, 0u32, best_sad);
        for y in 0..=(haystack.height() - gray.height()) {
            for x in 0..=(haystack.width() - gray.width()) {
                let sad = gray_sad_at(haystack, gray, alpha, x, y, best_sad);
                if sad < best_sad {
                    best_sad = sad;
                    best = (x, y, sad);
                }
            }
        }
        best
    }

    fn fixture_paths(directory: &Path) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(directory) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "png"))
            .collect();
        paths.sort();
        paths
    }

    fn fixture_dirs() -> Option<(Vec<PathBuf>, Vec<PathBuf>)> {
        let frames = fixture_paths(Path::new(&std::env::var("MILIASTRA_MATCH_FRAMES").ok()?));
        let templates = fixture_paths(Path::new(&std::env::var("MILIASTRA_MATCH_TEMPLATES").ok()?));
        if frames.is_empty() || templates.is_empty() {
            return None;
        }
        Some((frames, templates))
    }

    /// 从画面里取一块与模板尺寸相关的固定大小区域，让比较成本可控。
    fn bounded_region(
        frame: &GrayImage,
        template: &GrayImage,
        margin: u32,
    ) -> (GrayImage, u32, u32) {
        let width = (template.width() + margin).min(frame.width());
        let height = (template.height() + margin).min(frame.height());
        let x = (frame.width() - width) / 2;
        let y = (frame.height() - height) / 2;
        (frame.view(x, y, width, height).to_image(), x, y)
    }

    /// 真实截图回归：优化实现必须与朴素扫描逐位一致。
    /// 设置 MILIASTRA_MATCH_FRAMES / MILIASTRA_MATCH_TEMPLATES 后生效，未设置则跳过。
    #[test]
    fn fixture_cpu_matcher_matches_reference() {
        let Some((frames, templates)) = fixture_dirs() else {
            return;
        };
        let mut compared = 0usize;
        // 朴素基准代价是“位置数 × 模板像素数”，因此只在有界窗口上对照，避免整帧扫描。
        for frame_path in frames.iter().take(4) {
            let haystack = image::open(frame_path).expect("open frame").to_luma8();
            for template_path in &templates {
                let template = cached_gray_template(template_path).expect("load template");
                if template.gray.width() > haystack.width()
                    || template.gray.height() > haystack.height()
                    || template.alpha_sum == 0
                {
                    continue;
                }
                let (region, _, _) = bounded_region(&haystack, &template.gray, 48);
                if region.width() < template.gray.width()
                    || region.height() < template.gray.height()
                {
                    continue;
                }
                let expected =
                    reference_best(&region, &template.gray, &template.alpha, template.alpha_sum);
                let matcher = GrayMatcher::from_template(&template);
                let found = best_gray_match(&region, &matcher).expect("best match");
                assert_eq!(
                    (found.x, found.y, found.sad),
                    expected,
                    "优化实现与朴素扫描不一致: {} + {}",
                    frame_path.display(),
                    template_path.display()
                );
                compared += 1;
            }
        }
        assert!(compared > 0, "没有可比较的帧/模板组合");
        println!("CPU 优化实现与朴素扫描逐位一致，共比较 {compared} 组");
    }
    /// CPU 与 GPU（wgpu）两条路径的速度对比。
    ///
    /// GPU 侧给两种计时：`重建` 是旧生产路径（crate 自由函数每次新建 wgpu 设备），
    /// `复用` 是同一次会话内重复使用一个 TemplateMatcher，用来把“建设备”和“算 SAD”分开。
    #[test]
    fn fixture_cpu_vs_gpu_speed() {
        let Some((frames, templates)) = fixture_dirs() else {
            return;
        };
        let frame = image::open(&frames[0]).expect("open frame").to_luma8();
        println!(
            "帧: {} ({}x{})",
            frames[0].display(),
            frame.width(),
            frame.height()
        );
        let mut reused: Option<TemplateMatcher> = None;
        let mut rows = 0usize;
        for template_path in templates.iter().take(8) {
            let template = cached_gray_template(template_path).expect("load template");
            if template.gray.width() > frame.width() || template.gray.height() > frame.height() {
                continue;
            }
            let (region, _, _) = bounded_region(&frame, &template.gray, 64);
            let matcher = GrayMatcher::from_template(&template);

            let cpu_start = std::time::Instant::now();
            let cpu = cpu_best_gray_match(&region, &matcher).expect("cpu match");
            let cpu_elapsed = cpu_start.elapsed().as_secs_f64();

            let (rebuild_elapsed, rebuild_data, rebuild_width) =
                gpu_rebuild(&region, &template.gray);
            let (reuse_elapsed, reuse_data, reuse_width) =
                gpu_reuse(&mut reused, &region, &template.gray);

            // 与 GPU 对拍：在“安全列”上多点采样，逐点比较整数 SAD。
            // 不能拿 GPU 结果图的 argmin 当答案，也不能取每行最左若干列：
            // crate 的 shader 缺边界判断，越界线程会把那里覆盖成偏小的“部分和”，
            // 从而伪造出更低的 SAD（假阳性）。
            let corruption = rebuild_width.div_ceil(16) * 16 - rebuild_width;
            let (region_height, region_width) = (region.height(), region.width());
            let (template_height, template_width) = (template.gray.height(), template.gray.width());
            // crate 完全忽略 alpha，因此采样比较要用“全不透明”语义。
            let opaque_alpha = GrayImage::from_pixel(template_width, template_height, Luma([255]));
            let mut sampled = 0usize;
            let mut y = 0;
            while y + template_height <= region_height {
                let mut x = corruption;
                while x + template_width <= region_width {
                    let expected =
                        gray_sad_at(&region, &template.gray, &opaque_alpha, x, y, u64::MAX);
                    let gpu_value = rebuild_data[(y * rebuild_width + x) as usize] * 255.0;
                    let reuse_value = reuse_data[(y * reuse_width + x) as usize] * 255.0;
                    // GPU 侧全程 f32 累加（并先归一化到 0..1 再乘回 255），大模板会有可观舍入误差；
                    // CPU 侧是 u64 整数精确累加。因此这里用相对容差而不是精确相等。
                    let expected = expected as f32;
                    let tolerance = expected * 1e-3 + 1.0;
                    assert!(
                        (gpu_value - expected).abs() <= tolerance
                            && (reuse_value - expected).abs() <= tolerance,
                        "采样位置 ({x},{y}) CPU/GPU 不一致: cpu={expected} rebuild={gpu_value} reuse={reuse_value} 容差={tolerance}"
                    );
                    sampled += 1;
                    x += 11;
                }
                y += 5;
            }
            assert!(sampled > 0, "没有采样到可比较的位置");
            let argmin = rebuild_data.iter().copied().fold(f32::INFINITY, f32::min) * 255.0;
            // 只有最优点不落在被污染的行首区时才能直接对拍。
            if template.fully_opaque && cpu.x >= corruption {
                let at_best = gpu_value_at(&rebuild_data, rebuild_width, &cpu);
                assert!(
                    (at_best - cpu.sad as f32).abs() <= 1.0,
                    "不透明模板在 CPU 最优点应与 GPU 一致: cpu={} gpu={}",
                    cpu.sad,
                    at_best
                );
            }

            println!(
                "  {} {:>4}x{:<4} 区域{:>4}x{:<4} | CPU {:>7.2}ms | GPU重建 {:>8.2}ms ({:>7.1}x) | GPU复用 {:>7.2}ms ({:>5.1}x) | 采样{}点全一致 | sad={} | 图内最小={:.0} 污染列宽={}",
                template_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                template.gray.width(),
                template.gray.height(),
                region_width,
                region_height,
                cpu_elapsed * 1000.0,
                rebuild_elapsed * 1000.0,
                rebuild_elapsed / cpu_elapsed.max(f64::MIN_POSITIVE),
                reuse_elapsed * 1000.0,
                reuse_elapsed / cpu_elapsed.max(f64::MIN_POSITIVE),
                sampled,
                cpu.sad,
                argmin,
                corruption,
            );
            rows += 1;
        }
        assert!(rows > 0, "没有可比较的模板");
    }

    /// 报告本机可用的 GPU 适配器，以及"高优先级偏好"会选中哪一个。
    ///
    /// 用来回答 GPU 路径的可用性限制：集显/独显/虚拟显卡/软件回退各是什么情况。
    #[test]
    fn adapter_report() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            dx12_shader_compiler: Default::default(),
        });
        let adapters: Vec<_> = instance.enumerate_adapters(wgpu::Backends::all()).collect();
        println!("共发现 {} 个适配器:", adapters.len());
        for adapter in &adapters {
            let info = adapter.get_info();
            println!(
                "  - {} | backend={:?} | type={:?} | driver={} {}",
                info.name, info.backend, info.device_type, info.driver, info.driver_info
            );
        }

        // 与 crate 完全相同的参数。
        let chosen = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }));
        match chosen {
            Some(adapter) => {
                let info = adapter.get_info();
                println!(
                    "crate 参数(HighPerformance)选中: {} | type={:?} | backend={:?}",
                    info.name, info.device_type, info.backend
                );
            }
            None => println!("!! HighPerformance 无可用适配器 —— 生产路径会直接 panic"),
        }

        let fallback = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: true,
        }));
        match fallback {
            Some(adapter) => println!("软件回退适配器可用: {}", adapter.get_info().name),
            None => println!("软件回退适配器不可用"),
        }
    }

    /// 自包含的 GPU SAD 微基准：对每个可用 GPU 适配器分别编译/创建/执行，
    /// 用来横向比较集显与独显。shader 里带边界判断（crate 缺这一步）。
    #[test]
    fn gpu_adapter_bench() {
        const SHADER: &str = r#"
struct Uniforms {
    input_width: u32,
    input_height: u32,
    template_width: u32,
    template_height: u32,
};
@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var<storage, read> input_buf: array<f32>;
@group(0) @binding(2) var<storage, read> template_buf: array<f32>;
@group(0) @binding(3) var<storage, read_write> result_buf: array<f32>;

@compute @workgroup_size(16, 16, 1)
fn main_sad(@builtin(global_invocation_id) gid: vec3<u32>) {
    let rw = uniforms.input_width - uniforms.template_width + 1u;
    let rh = uniforms.input_height - uniforms.template_height + 1u;
    if (gid.x >= rw || gid.y >= rh) { return; }
    var total = 0.0;
    for (var j = 0u; j < uniforms.template_height; j++) {
        for (var i = 0u; i < uniforms.template_width; i++) {
            total += abs(input_buf[(gid.y + j) * uniforms.input_width + (gid.x + i)]
                       - template_buf[j * uniforms.template_width + i]);
        }
    }
    result_buf[gid.y * rw + gid.x] = total;
}
"#;

        let Some((frames, templates)) = fixture_dirs() else {
            return;
        };
        let frame = image::open(&frames[0]).expect("open frame").to_luma8();
        let template = cached_gray_template(
            &templates
                .iter()
                .find(|path| path.file_name().is_some_and(|n| n == "friend-confirm.png"))
                .cloned()
                .unwrap_or_else(|| templates[0].clone()),
        )
        .expect("load template");
        let (region, _, _) = bounded_region(&frame, &template.gray, 64);
        let (iw, ih) = (region.width(), region.height());
        let (tw, th) = (template.gray.width(), template.gray.height());
        let (rw, rh) = (iw - tw + 1, ih - th + 1);
        println!("模板 {tw}x{th} 区域 {iw}x{ih} 结果图 {rw}x{rh}");

        let input: Vec<f32> = region.as_raw().iter().map(|v| *v as f32 / 255.0).collect();
        let needle: Vec<f32> = template
            .gray
            .as_raw()
            .iter()
            .map(|v| *v as f32 / 255.0)
            .collect();
        let uniforms: [u32; 4] = [iw, ih, tw, th];

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            dx12_shader_compiler: Default::default(),
        });
        // 只取原生驱动的集显与独显各一个：跳过 Mesa "Dozen"（D3D12→Vulkan 转译层）与软件适配器，
        // 那类适配器既不代表真实驱动性能，在同一进程里混用多个后端还会触发驱动崩溃。
        let adapters: Vec<_> = instance
            .enumerate_adapters(wgpu::Backends::all())
            .filter(|adapter| {
                let info = adapter.get_info();
                matches!(
                    info.device_type,
                    wgpu::DeviceType::IntegratedGpu | wgpu::DeviceType::DiscreteGpu
                ) && !info.driver.contains("Dozen")
            })
            .collect();

        let mut seen_types: Vec<wgpu::DeviceType> = Vec::new();
        for adapter in adapters {
            let info = adapter.get_info();
            if seen_types.contains(&info.device_type) {
                continue;
            }
            seen_types.push(info.device_type);

            // 设备创建成本
            let create_start = std::time::Instant::now();
            let (device, queue) = match pollster::block_on(adapter.request_device(
                &wgpu::DeviceDescriptor {
                    label: None,
                    features: wgpu::Features::empty(),
                    limits: wgpu::Limits::default(),
                },
                None,
            )) {
                Ok(pair) => pair,
                Err(error) => {
                    println!(
                        "  {:<45} {:?} 创建设备失败: {error}",
                        info.name, info.backend
                    );
                    continue;
                }
            };
            let create_ms = create_start.elapsed().as_secs_f64() * 1000.0;

            let pipeline_start = std::time::Instant::now();
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: None,
                source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(SHADER)),
            });
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: None,
                entries: &[0u32, 1, 2, 3]
                    .iter()
                    .map(|binding| wgpu::BindGroupLayoutEntry {
                        binding: *binding,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer {
                            ty: if *binding == 0 {
                                wgpu::BufferBindingType::Uniform
                            } else if *binding == 3 {
                                wgpu::BufferBindingType::Storage { read_only: false }
                            } else {
                                wgpu::BufferBindingType::Storage { read_only: true }
                            },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    })
                    .collect::<Vec<_>>(),
            });
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[&layout],
                push_constant_ranges: &[],
            });
            let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: None,
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: "main_sad",
            });

            let make = |bytes: &[u8], usage: wgpu::BufferUsages| {
                let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: None,
                    size: bytes.len() as u64,
                    usage: usage | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                queue.write_buffer(&buffer, 0, bytes);
                buffer
            };
            let uniform_bytes: Vec<u8> = uniforms.iter().flat_map(|v| v.to_ne_bytes()).collect();
            let input_bytes: Vec<u8> = input.iter().flat_map(|v| v.to_ne_bytes()).collect();
            let needle_bytes: Vec<u8> = needle.iter().flat_map(|v| v.to_ne_bytes()).collect();
            let result_bytes = (rw * rh) as usize * 4;

            let uniform_buffer = make(&uniform_bytes, wgpu::BufferUsages::UNIFORM);
            let input_buffer = make(&input_bytes, wgpu::BufferUsages::STORAGE);
            let needle_buffer = make(&needle_bytes, wgpu::BufferUsages::STORAGE);
            let result_buffer = make(
                &vec![0u8; result_bytes],
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            );
            let staging = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: result_bytes as u64,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: uniform_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: input_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: needle_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: result_buffer.as_entire_binding(),
                    },
                ],
            });
            let pipeline_ms = pipeline_start.elapsed().as_secs_f64() * 1000.0;

            let run = || {
                let mut encoder =
                    device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
                {
                    let mut pass =
                        encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: None });
                    pass.set_pipeline(&pipeline);
                    pass.set_bind_group(0, &bind_group, &[]);
                    pass.dispatch_workgroups(rw.div_ceil(16), rh.div_ceil(16), 1);
                }
                encoder.copy_buffer_to_buffer(&result_buffer, 0, &staging, 0, result_bytes as u64);
                queue.submit(Some(encoder.finish()));
            };

            // 预热（含编译/上传/驱动缓存）
            run();
            device.poll(wgpu::Maintain::Wait);

            const ROUNDS: usize = 20;
            let mut samples = Vec::with_capacity(ROUNDS);
            for _ in 0..ROUNDS {
                let start = std::time::Instant::now();
                run();
                device.poll(wgpu::Maintain::Wait);
                samples.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            samples.sort_by(f64::total_cmp);
            let median = samples[ROUNDS / 2];
            let best = samples[0];

            // 读回并校验：与 CPU 在不透明语义下的 SAD 对齐
            let slice = staging.slice(0..result_bytes as u64);
            let (sender, receiver) = std::sync::mpsc::channel();
            slice.map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(result);
            });
            device.poll(wgpu::Maintain::Wait);
            receiver.recv().expect("mapped").expect("readback");
            let data = slice.get_mapped_range();
            let values: Vec<f32> = data
                .chunks_exact(4)
                .map(|c| f32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            drop(data);
            staging.unmap();
            let opaque_alpha = GrayImage::from_pixel(tw, th, Luma([255]));
            let (mut worst, mut worst_at) = (0.0f32, (0u32, 0u32));
            for y in 0..rh {
                for x in 0..rw {
                    let expected =
                        gray_sad_at(&region, &template.gray, &opaque_alpha, x, y, u64::MAX) as f32;
                    let gpu = values[(y * rw + x) as usize] * 255.0;
                    let diff = (gpu - expected).abs() / expected.max(1.0);
                    if diff > worst {
                        worst = diff;
                        worst_at = (x, y);
                    }
                }
            }
            println!(
                "  {:<45} {:?} {:?} | 建设备 {:>7.1}ms 建管线 {:>6.1}ms | 每帧 中位 {:>5.2}ms 最快 {:>5.2}ms | 最大相对误差 {:.2e}@({},{})",
                info.name,
                info.device_type,
                info.backend,
                create_ms,
                pipeline_ms,
                median,
                best,
                worst,
                worst_at.0,
                worst_at.1
            );
        }
    }

    /// GPU 各阶段耗时拆解：区分固定开销（提交+回读）与可优化部分（归一化/上传）。
    #[test]
    fn gpu_phase_bench() {
        let Some((frames, templates)) = fixture_dirs() else {
            return;
        };
        let frame = image::open(&frames[0]).expect("open frame").to_luma8();
        let mut searcher = match GpuSearcher::create() {
            Some(searcher) => searcher,
            None => {
                println!("没有可用 GPU 适配器");
                return;
            }
        };
        for wanted in [
            "chat-marker-blue.png",
            "friend-panel.png",
            "friend-confirm.png",
            "friend-blacklist.png",
        ] {
            let Some(path) = templates
                .iter()
                .find(|path| path.file_name().is_some_and(|name| name == wanted))
                .cloned()
            else {
                continue;
            };
            let template = cached_gray_template(&path).expect("load template");
            let (region, _, _) = bounded_region(&frame, &template.gray, 64);
            let (iw, ih) = (region.width(), region.height());
            let (tw, th) = (template.gray.width(), template.gray.height());
            let positions = (iw - tw + 1) as u64 * (ih - th + 1) as u64;
            let chunks = (GPU_TARGET_THREADS / positions.max(1)).clamp(1, 256) as u32;
            let dims = [iw, ih, tw, th, 0, 0, chunks, th.div_ceil(chunks)];

            const ROUNDS: usize = 30;
            let mut normalize = Vec::with_capacity(ROUNDS);
            let mut upload = Vec::with_capacity(ROUNDS);
            let mut compute = Vec::with_capacity(ROUNDS);
            for _ in 0..ROUNDS {
                let start = std::time::Instant::now();
                let input: Vec<u32> = region.as_raw().iter().map(|v| *v as u32).collect();
                let needle: Vec<u32> = template.gray.as_raw().iter().map(|v| *v as u32).collect();
                normalize.push(start.elapsed().as_secs_f64() * 1000.0);

                let start = std::time::Instant::now();
                let bind_group = searcher.upload(&input, &needle, dims);
                upload.push(start.elapsed().as_secs_f64() * 1000.0);

                let start = std::time::Instant::now();
                let _ = searcher.compute(dims, &bind_group).expect("compute");
                compute.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            let median = |samples: &mut Vec<f64>| {
                samples.sort_by(f64::total_cmp);
                samples[samples.len() / 2]
            };
            println!(
                "  {:<26} {:>4}x{:<4} 区域{:>4}x{:<4} | 归一化 {:>6.2}ms | 上传 {:>6.2}ms | 提交+回读 {:>6.2}ms",
                wanted,
                tw,
                th,
                iw,
                ih,
                median(&mut normalize),
                median(&mut upload),
                median(&mut compute)
            );
        }
    }

    /// 分块正确性：对同一区域用多种分片配置跑 GPU，逐点与 CPU 精确 SAD 比对。
    ///
    /// 这是"分块会不会造成假阳性"的直接证据：任何漏行/重行/漏清零都会让某个位置的
    /// 部分和偏离 CPU 值，逐点相等即排除了这些可能。
    #[test]
    fn gpu_chunked_map_is_exact() {
        let Some((frames, templates)) = fixture_dirs() else {
            return;
        };
        let frame = image::open(&frames[0]).expect("open frame").to_luma8();
        let mut searcher = match GpuSearcher::create() {
            Some(searcher) => searcher,
            None => {
                println!("没有可用 GPU 适配器");
                return;
            }
        };
        // 覆盖：不分片 / 恰好整除 / 不整除（尾片残缺）/ 分片数远大于模板行数。
        const CHUNKS: [u32; 6] = [1, 3, 7, 15, 62, 256];
        let mut cases = 0;
        let mut checks = 0usize;
        for path in &templates {
            let Ok(template) = cached_gray_template(path) else {
                continue;
            };
            // GPU 路径只处理完全不透明的模板。
            if !template.fully_opaque {
                continue;
            }
            let (region, _, _) = bounded_region(&frame, &template.gray, 4);
            let (iw, ih) = (region.width(), region.height());
            let (tw, th) = (template.gray.width(), template.gray.height());
            if tw == 0 || th == 0 || tw > iw || th > ih {
                continue;
            }
            let input: Vec<u32> = region.as_raw().iter().map(|v| *v as u32).collect();
            let needle: Vec<u32> = template.gray.as_raw().iter().map(|v| *v as u32).collect();
            let rw = (iw - tw + 1) as usize;
            let rh = (ih - th + 1) as usize;
            let alpha = GrayImage::from_pixel(tw, th, Luma([255]));
            for chunks in CHUNKS {
                let dims = [iw, ih, tw, th, 0, 0, chunks, th.div_ceil(chunks)];
                let map = searcher.search(&input, &needle, dims).expect("gpu map");
                assert_eq!(map.len(), rw * rh, "结果图长度不符");
                for y in 0..rh {
                    for x in 0..rw {
                        let expected = gray_sad_at(
                            &region,
                            &template.gray,
                            &alpha,
                            x as u32,
                            y as u32,
                            u64::MAX,
                        ) as u32;
                        let gpu = map[y * rw + x];
                        assert_eq!(
                            gpu, expected,
                            "chunk={chunks} th={th} 位置({x},{y}) GPU={gpu} CPU={expected}"
                        );
                        checks += 1;
                    }
                }
            }
            cases += 1;
            if cases >= 6 {
                break;
            }
        }
        assert!(cases > 0, "没有可用的不透明测试模板");
        println!(
            "分块精确性：{cases} 个模板 × {} 种分片配置，共 {checks} 个位置逐位一致",
            CHUNKS.len()
        );
    }

    /// 纯色模板审计：分块会不会因此误匹配？
    ///
    /// 纯色模板是"部分累加"最危险的场景——若结果图被读到累加中途，少算的那部分
    /// 恰好是真实 SAD 的 k/th，数值"合情合理地小"，看起来就是一个高分命中。
    #[test]
    fn solid_template_audit() {
        let Some((frames, _)) = fixture_dirs() else {
            return;
        };
        let frame = image::open(&frames[0]).expect("open frame").to_luma8();
        let (width, height) = (256u32, 256u32);
        let (x0, y0) = ((frame.width() - width) / 2, (frame.height() - height) / 2);
        let region = image::imageops::crop_imm(&frame, x0, y0, width, height).to_image();
        let raw = region.as_raw();
        let mean = (raw.iter().map(|v| *v as u64).sum::<u64>() / raw.len() as u64) as u8;
        let (tw, th) = (16u32, 16u32);
        let needle = GrayImage::from_pixel(tw, th, Luma([mean]));
        let matcher = GrayMatcher::opaque(&needle);
        let rw = (width - tw + 1) as usize;
        let rh = (height - th + 1) as usize;
        let mut cpu_map = vec![0u64; rw * rh];
        let mut over = [0usize; 4];
        for y in 0..rh {
            for x in 0..rw {
                let sad = sad_at(&region, &matcher, x as u32, y as u32, u64::MAX);
                cpu_map[y * rw + x] = sad;
                let score = 1.0 - sad as f32 / matcher.alpha_sum() as f32;
                for (i, threshold) in [0.80f32, 0.90, 0.95, 1.0].iter().enumerate() {
                    if score >= *threshold {
                        over[i] += 1;
                    }
                }
            }
        }
        let total = (rw * rh) as f64;
        println!(
            "纯色模板 {tw}x{th} 值={mean}，区域 {width}x{height}，共 {} 个位置",
            rw * rh
        );
        println!(
            "  分值 >=0.80: {} 个 ({:.1}%)",
            over[0],
            over[0] as f64 * 100.0 / total
        );
        println!(
            "  分值 >=0.90: {} 个，>=0.95: {} 个，==1.00: {} 个",
            over[1], over[2], over[3]
        );
        let Some(mut searcher) = GpuSearcher::create() else {
            return;
        };
        let input: Vec<u32> = raw.iter().map(|v| *v as u32).collect();
        let words: Vec<u32> = needle.as_raw().iter().map(|v| *v as u32).collect();
        for chunks in [1u32, 64, 256] {
            let dims = [width, height, tw, th, 0, 0, chunks, th.div_ceil(chunks)];
            let map = searcher.search(&input, &words, dims).expect("gpu map");
            let mismatch = (0..rw * rh)
                .filter(|i| map[*i] as u64 != cpu_map[*i])
                .count();
            let too_small = (0..rw * rh)
                .filter(|i| (map[*i] as u64) < cpu_map[*i])
                .count();
            println!(
                "  分块 chunks={chunks}: 不一致 {mismatch} 个，偏小（会假阳性）{too_small} 个"
            );
            assert_eq!(mismatch, 0, "chunks={chunks} 存在不一致位置");
        }
    }

    /// 纯色模板识别：确认护栏能区分"纯色"与"真实有纹理"的模板。
    #[test]
    fn flat_template_is_flagged() {
        let flat = GrayImage::from_pixel(16, 16, Luma([217]));
        assert_eq!(gray_stddev(&flat), 0.0, "纯色模板标准差应为 0");
        assert!(gray_stddev(&flat) < FLAT_TEMPLATE_STDDEV);

        let Some((_, templates)) = fixture_dirs() else {
            return;
        };
        let mut lowest = f32::MAX;
        let mut lowest_name = String::new();
        let mut passed = 0usize;
        let mut counted = 0usize;
        for path in templates.iter().take(20) {
            let Ok(template) = cached_gray_template(path) else {
                continue;
            };
            counted += 1;
            if template.gray_stddev >= FLAT_TEMPLATE_STDDEV {
                passed += 1;
            }
            if template.gray_stddev < lowest {
                lowest = template.gray_stddev;
                lowest_name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into();
            }
        }
        println!("纯色模板 stddev=0.00（已标记）；真实模板 {counted} 个中 {passed} 个通过门槛");
        println!("  最低者: {lowest_name} stddev={lowest:.2}（门槛 {FLAT_TEMPLATE_STDDEV}）");
        assert!(passed > 0, "所有真实模板都被判为纯色，门槛过严");
    }

    /// 行级提前退出必须与逐字节提前退出给出完全相同的接受/拒绝判定。
    #[test]
    fn color_sad_row_blocking_preserves_accept_reject() {
        fn reference_color_sad(haystack: &RgbImage, template: &RgbImage, x: u32, y: u32) -> u64 {
            let mut sad = 0u64;
            for row in 0..template.height() {
                for column in 0..template.width() {
                    let haystack_pixel = haystack.get_pixel(x + column, y + row);
                    let template_pixel = template.get_pixel(column, row);
                    for channel in 0..3 {
                        sad += haystack_pixel.0[channel].abs_diff(template_pixel.0[channel]) as u64;
                    }
                }
            }
            sad
        }

        let (width, height) = (37u32, 29u32);
        let (template_width, template_height) = (9u32, 7u32);
        let mut seed = 0x1234_5678u32;
        let mut raw = Vec::with_capacity((width * height * 3) as usize);
        for _ in 0..(width * height * 3) {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            raw.push((seed >> 24) as u8);
        }
        let haystack = RgbImage::from_raw(width, height, raw).expect("haystack");
        let template = crate::geometry::crop_canvas(
            &DynamicImage::ImageRgb8(haystack.clone()),
            crate::geometry::Rect::new(5, 4, template_width, template_height),
        )
        .expect("crop")
        .to_rgb8();
        let max_sad = (template_width * template_height * 3 * 255) as f32;

        let mut accepted = 0usize;
        let mut rejected = 0usize;
        for threshold in [0.4f32, 0.7, 0.9, 1.0] {
            let max_allowed = ((1.0 - threshold).clamp(0.0, 1.0) * max_sad) as u64;
            for y in 0..=(height - template_height) {
                for x in 0..=(width - template_width) {
                    let got = color_sad_at(&haystack, &template, x, y, max_allowed);
                    let want = reference_color_sad(&haystack, &template, x, y);
                    assert_eq!(
                        got > max_allowed,
                        want > max_allowed,
                        "threshold={threshold} x={x} y={y} got={got} want={want}"
                    );
                    if want > max_allowed {
                        rejected += 1;
                    } else {
                        accepted += 1;
                    }
                }
            }
        }
        // 两种结果都要覆盖到，否则这个用例可能什么都没验证。
        assert!(accepted > 0, "没有任何候选被接受");
        assert!(rejected > 0, "没有任何候选被拒绝");
    }

    /// 彩色模板路径（聊天标记）的实际开销：确认它到底是不是瓶颈。
    #[test]
    fn color_marker_scan_cost_is_bounded() {
        let Some((frames, templates)) = fixture_dirs() else {
            return;
        };
        let frame = image::open(&frames[0]).expect("open frame");
        let paths: Vec<std::path::PathBuf> = [
            "chat-marker-blue.png",
            "chat-marker-yellow.png",
            "chat-marker-pink.png",
        ]
        .iter()
        .filter_map(|name| {
            templates
                .iter()
                .find(|path| {
                    path.file_name()
                        .is_some_and(|file| file.to_string_lossy().as_ref() == *name)
                })
                .cloned()
        })
        .collect();
        if paths.len() < 3 {
            return;
        }
        let refs: Vec<&std::path::Path> = paths.iter().map(|path| path.as_path()).collect();
        // 生产调用点只在这条 60 像素宽的条带里搜索（CHAT_MARKER_SEARCH_WIDTH）。
        let rect = crate::geometry::Rect::new(0, 0, 60, frame.height());
        const ROUNDS: usize = 20;
        let mut samples = Vec::with_capacity(ROUNDS);
        let mut hits = 0usize;
        for _ in 0..ROUNDS {
            let start = std::time::Instant::now();
            let groups =
                find_color_template_hit_groups(&frame, Some(rect), &refs, 0.9).expect("scan");
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
            hits = groups.iter().map(|group| group.len()).sum();
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "彩色标记扫描 60x{} × 3 模板（阈值 0.9）: 中位 {:.2}ms 最快 {:.2}ms hits={hits}",
            frame.height(),
            samples[ROUNDS / 2],
            samples[0]
        );
    }

    fn gpu_value_at(data: &[f32], width: u32, position: &GrayMatch) -> f32 {
        data[(position.y * width + position.x) as usize] * 255.0
    }

    fn to_match(image: &GrayImage) -> MatchImage<'static> {
        MatchImage::new(
            image
                .as_raw()
                .iter()
                .map(|value| *value as f32 / 255.0)
                .collect::<Vec<_>>(),
            image.width(),
            image.height(),
        )
    }

    /// 旧生产路径：crate 自由函数，每次调用都新建 wgpu 设备。
    fn gpu_rebuild(haystack: &GrayImage, gray: &GrayImage) -> (f64, Vec<f32>, u32) {
        let start = std::time::Instant::now();
        let result = match_template(
            to_match(haystack),
            to_match(gray),
            MatchTemplateMethod::SumOfAbsoluteDifferences,
        );
        (
            start.elapsed().as_secs_f64(),
            result.data.to_vec(),
            result.width,
        )
    }

    /// 复用同一个 TemplateMatcher，把设备与管线创建成本排除在计时之外。
    fn gpu_reuse(
        matcher: &mut Option<TemplateMatcher>,
        haystack: &GrayImage,
        gray: &GrayImage,
    ) -> (f64, Vec<f32>, u32) {
        if matcher.is_none() {
            let mut fresh = TemplateMatcher::new();
            fresh.match_template(
                to_match(haystack),
                to_match(gray),
                MatchTemplateMethod::SumOfAbsoluteDifferences,
            );
            let _ = fresh.wait_for_result().expect("gpu warmup");
            *matcher = Some(fresh);
        }
        let active = matcher.as_mut().expect("matcher");
        let start = std::time::Instant::now();
        active.match_template(
            to_match(haystack),
            to_match(gray),
            MatchTemplateMethod::SumOfAbsoluteDifferences,
        );
        let result = active.wait_for_result().expect("gpu result");
        (
            start.elapsed().as_secs_f64(),
            result.data.to_vec(),
            result.width,
        )
    }
}
