//! 模板匹配单元测试：与朴素实现逐位对拍、GPU 精确性、文字质量校验与微基准。

use crate::geometry::Rect;
use image::DynamicImage;
use image::GenericImageView;
use image::GrayImage;
use image::Luma;
use image::Rgb;
use image::RgbImage;
use image::Rgba;
use image::RgbaImage;
use image::imageops::FilterType;
use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use template_matching::Image as MatchImage;
use template_matching::MatchTemplateMethod;
use template_matching::TemplateMatcher;
use template_matching::match_template;

// 内部实现（CPU/GPU/缓存/API）必须显式从各自子模块取；模块根只重导出真实
// 跨模块使用的名字，测试不依赖它。
use super::api::*;
use super::cache::*;
use super::cpu::*;
use super::gpu::*;

/// 回归：平坦画面防护必须按 alpha 外接框的 (x, y) 取样。
/// 曾经的实现漏了 `contrast.y`，凡是内容不在模板最顶端一行开始的模板
/// （如 `friend-more-settings.png` 的三个点在 y=20）都会取到空白行，
/// 把完美的 SAD=0 命中判成"平坦画面"而丢弃。
#[test]
fn patch_contrast_uses_contrast_origin() {
    // 模板：内容只在 y=5..7，其余透明。
    let mut gray = GrayImage::from_pixel(8, 8, Luma([100]));
    let mut alpha = GrayImage::from_pixel(8, 8, Luma([0]));
    for y in 5..8 {
        for x in 0..8 {
            alpha.put_pixel(x, y, Luma([255]));
        }
    }
    for y in 0..8 {
        for x in 0..8 {
            gray.put_pixel(x, y, Luma([100]));
        }
    }
    let template = GrayTemplate::build(
        gray,
        alpha,
        std::path::Path::new("synthetic-contrast-origin.png"),
    );
    let matcher = GrayMatcher::from_template(&template);
    assert!(matcher.contrast.is_some(), "alpha 外接框应可用于平坦防护");
    // 画面：y=0..4 平坦，y=5..7 有明显黑白交替结构（对应模板内容）。
    let mut haystack = GrayImage::from_pixel(20, 20, Luma([250]));
    for y in 5..8 {
        for x in 0..20 {
            let value = if (x + y) % 2 == 0 { 0 } else { 255 };
            haystack.put_pixel(x, y, Luma([value]));
        }
    }
    assert!(
        patch_contrast_ok(&haystack, &matcher, 0, 0),
        "必须按 contrast.y 取样；否则会误采 y=0 的平坦行并拒绝真实命中"
    );
}

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
fn old_seeded_sea(haystack: &GrayImage, matcher: &GrayMatcher<'_>, seed: GrayMatch) -> GrayMatch {
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
fn bounded_region(frame: &GrayImage, template: &GrayImage, margin: u32) -> (GrayImage, u32, u32) {
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
            if region.width() < template.gray.width() || region.height() < template.gray.height() {
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

        let (rebuild_elapsed, rebuild_data, rebuild_width) = gpu_rebuild(&region, &template.gray);
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
                let expected = gray_sad_at(&region, &template.gray, &opaque_alpha, x, y, u64::MAX);
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
            .find(|path| path.file_name().is_some_and(|n| n == "confirm-button.png"))
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
        "confirm-button.png",
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
        println!("  分块 chunks={chunks}: 不一致 {mismatch} 个，偏小（会假阳性）{too_small} 个");
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
        let groups = find_color_template_hit_groups(&frame, Some(rect), &refs, 0.9).expect("scan");
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

/// 模板资产回归：去掉背景的模板必须真的有 alpha；文字模板必须裁到字形 bbox。
///
/// 例外只有一类：模板本身就是色块/内容信号（聊天标记、world-wish、地图星）。
/// 图标模板仍按生产写死的尺寸做固定断言；文字模板的 canvas 由
/// `scripts/pure-text-templates.py` 裁到「字形 bbox + 2px 透明 padding」，
/// 搜索区已同步平移到新的模板原点。
#[test]
fn ui_template_assets_use_transparency_and_tight_crop() {
    let dir = Path::new("assets");
    let signal_templates = [
        "chat-marker-blue.png",
        "chat-marker-pink.png",
        "chat-marker-yellow.png",
        "world-wish.png",
        "startup-wonderland-map-star.png",
        "startup-paimon-menu.png",
        // 表情栏锚点：直接从实机截图裁下的图标块（红心/齿轮），不透明是刻意的；
        // 匹配走 best_embedded_template_candidate，并带平坦画面防护。
        "emoji-tray-heart.png",
        "emoji-tray-gear.png",
    ];
    // 图标/信号模板仍由生产代码按这些宽高写死搜索区/偏移，尺寸不能改。
    // 文字模板不在此列：它们的 canvas 就是字形 bbox + padding，搜索区按
    // 「旧命中左上角 + 裁剪原点」平移，不再依赖固定 canvas 尺寸。
    let fixed_dimensions = [("qianxing-map-label.png", 296u32, 68u32)];
    let mut checked = 0usize;
    for path in fixture_paths(dir) {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let image = image::open(&path).unwrap_or_else(|e| panic!("打不开 {}: {e}", path.display()));
        for (fixed_name, width, height) in fixed_dimensions {
            if name == fixed_name {
                assert_eq!(
                    (image.width(), image.height()),
                    (width, height),
                    "模板 {name} 的尺寸被改了，必须同步搜索区/偏移或恢复原尺寸"
                );
            }
        }
        if signal_templates.contains(&name.as_str()) {
            continue;
        }
        let rgba = image.to_rgba8();
        let total = (rgba.width() * rgba.height()) as f64;
        let transparent = rgba.pixels().filter(|pixel| pixel[3] == 0).count() as f64 / total;
        let opaque = rgba.pixels().filter(|pixel| pixel[3] == 255).count() as f64 / total;
        assert!(
            transparent >= 0.10,
            "模板 {name} 只有 {:.1}% 透明像素：多余背景没有去掉",
            transparent * 100.0
        );
        // 纯文字模板裁到 bbox 后不透明像素占比高（约 8–28%），但为兼容
        // 早期未裁剪资产仍用更低的 0.2% 门槛：只要字形没有被误删即可。
        let min_opaque = if is_text_template(&path) { 0.002 } else { 0.01 };
        assert!(
            opaque >= min_opaque,
            "模板 {name} 只剩 {:.2}% 不透明像素：内容被误删",
            opaque * 100.0
        );
        if is_text_template(&path) {
            // 裁到字形 bbox：四边只剩小 padding。默认 2px；4× 下采样模板为了
            // 让 canvas 宽高是倍数的整数倍（避免 floor 缩放误差）最多留 7px，
            // 这也是“没有大片透明边”的上限。原图就贴边的模板该侧可以是 0。
            let (width, height) = rgba.dimensions();
            let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0u32, 0u32);
            for (x, y, pixel) in rgba.enumerate_pixels() {
                if pixel[3] == 0 {
                    continue;
                }
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
            assert!(
                min_x <= 8 && min_y <= 8,
                "模板 {name} 左上留白 ({min_x},{min_y}) 超过 8px：没有裁到字形 bbox"
            );
            assert!(
                width - 1 - max_x <= 8 && height - 1 - max_y <= 8,
                "模板 {name} 右下留白 ({}, {}) 超过 8px：没有裁到字形 bbox",
                width - 1 - max_x,
                height - 1 - max_y
            );
        }
        checked += 1;
    }
    assert!(
        checked >= 20,
        "只检查了 {checked} 张模板，assets 目录不完整"
    );
}

/// 透明模板不能在纯色画面上拿高分。
///
/// 透明化去掉对比底色后，只剩白字/白图标的模板在纯色区域上按 alpha 归一化 SAD
/// 会接近 0；`patch_contrast_ok` 必须把这些命中挡掉。用模板自身的加权平均灰度
/// 构造纯色帧（也就是最容易误命中它的那个颜色），阈值取比生产更低的 0.90。
#[test]
fn transparent_templates_reject_flat_backgrounds() {
    let mut checked = 0usize;
    for path in fixture_paths(Path::new("assets")) {
        let rgba = image::open(&path)
            .unwrap_or_else(|e| panic!("打不开 {}: {e}", path.display()))
            .to_rgba8();
        if rgba.pixels().all(|pixel| pixel[3] == 255) {
            continue; // 不透明模板不在本次防护范围
        }
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if matches!(
            file_name,
            "world-wish.png" | "startup-wonderland-map-star.png"
        ) {
            continue; // 既有的透明信号图，本轮明确不改其行为
        }
        let (mut weighted, mut weight) = (0.0f64, 0.0f64);
        for pixel in rgba.pixels() {
            let luma = 0.299 * pixel[0] as f64 + 0.587 * pixel[1] as f64 + 0.114 * pixel[2] as f64;
            let alpha = pixel[3] as f64 / 255.0;
            weighted += luma * alpha;
            weight += alpha;
        }
        if weight == 0.0 {
            continue;
        }
        let mean = (weighted / weight).round().clamp(0.0, 255.0) as u8;
        let (width, height) = rgba.dimensions();
        let solid = RgbImage::from_pixel(width + 16, height + 16, Rgb([mean, mean, mean]));
        let hit = best_template_hit(&DynamicImage::ImageRgb8(solid), None, &path, 0.90)
            .unwrap_or_else(|e| panic!("匹配 {} 失败: {e}", path.display()));
        assert!(
            hit.is_none(),
            "模板 {} 在纯色 {} 上仍命中：{:?}",
            path.display(),
            mean,
            hit.map(|hit| (hit.x, hit.y, hit.score))
        );
        checked += 1;
    }
    assert!(checked >= 20, "只检查了 {checked} 张透明模板");
}

/// 造一张“文字模板 + 对应画面”的合成夹具：白底深字或暗底亮字。
fn text_quality_fixture(
    glyph_value: u8,
    background_value: u8,
    horizontal_glyphs: bool,
) -> (DynamicImage, DynamicImage, Rect) {
    let (width, height) = (40u32, 20u32);
    let mut template = RgbaImage::from_pixel(width, height, Rgba([glyph_value; 4]));
    for y in 0..height {
        for x in 0..width {
            // 透明背景不参与 SAD；字形是几条粗笔画。
            let glyph = if horizontal_glyphs {
                (4..36).contains(&x) && (y / 4) % 2 == 0
            } else {
                (4..16).contains(&y) && (x / 5) % 2 == 0
            };
            if glyph {
                template.put_pixel(x, y, Rgba([glyph_value, glyph_value, glyph_value, 255]));
            } else {
                template.put_pixel(x, y, Rgba([glyph_value, glyph_value, glyph_value, 0]));
            }
        }
    }
    let mut patch = RgbImage::from_pixel(80, 50, Rgb([background_value; 3]));
    for y in 0..height {
        for x in 0..width {
            let alpha = template.get_pixel(x, y)[3];
            if alpha == 0 {
                continue;
            }
            patch.put_pixel(20 + x, 15 + y, Rgb([glyph_value; 3]));
        }
    }
    (
        DynamicImage::ImageRgba8(template),
        DynamicImage::ImageRgb8(patch),
        Rect::new(20, 15, width, height),
    )
}

#[test]
fn text_anchor_quality_accepts_dark_and_light_text() {
    // 深色字 / 亮底。
    let (template, patch, hit) = text_quality_fixture(40, 230, false);
    assert!(
        text_anchor_quality_ok(&patch, hit, &template),
        "深色字应通过文字质量校验"
    );
    // 浅色字 / 暗底：极性无关。
    let (template, patch, hit) = text_quality_fixture(230, 40, false);
    assert!(
        text_anchor_quality_ok(&patch, hit, &template),
        "浅色字应通过文字质量校验"
    );
}

#[test]
fn text_anchor_quality_supports_opaque_template() {
    // 不透明模板没有 alpha 字形 mask：用 patch 边框的中位背景估计字形。
    let mut template = RgbImage::from_pixel(40, 20, Rgb([230; 3]));
    for y in 4..16 {
        for x in 0..40 {
            if (x / 5) % 2 == 0 {
                template.put_pixel(x, y, Rgb([40; 3]));
            }
        }
    }
    let mut patch = RgbImage::from_pixel(80, 50, Rgb([230; 3]));
    for y in 4..16 {
        for x in 0..40 {
            if (x / 5) % 2 == 0 {
                patch.put_pixel(20 + x, 15 + y, Rgb([40; 3]));
            }
        }
    }
    assert!(text_anchor_quality_ok(
        &DynamicImage::ImageRgb8(patch),
        Rect::new(20, 15, 40, 20),
        &DynamicImage::ImageRgb8(template)
    ));
}

#[test]
fn text_template_registry_covers_text_assets_only() {
    for name in [
        "ui-main-menu-return.png",
        "invite-enter-hall.png",
        "invite-goto-hall.png",
        "invite-view-star.png",
        "invite-view-profile.png",
        "teyvat-profile-add-friend.png",
        "friend-block-chat.png",
        "friend-blacklist.png",
        "confirm-button.png",
        "friend-search-panel.png",
        "qianxing-f2-hall-list.png",
        "qianxing-f2-invite-player.png",
        "qianxing-exit-menu.png",
        "qianxing-profile-page-wishlist.png",
        "qianxing-profile-page-wardrobe.png",
    ] {
        assert!(is_text_template(Path::new("assets").join(name).as_path()));
    }
    for name in [
        "ui-secondary-back.png",
        "ui-secondary-hall.png",
        "ui-secondary-chat-submit.png",
        "friend-panel.png",
        "chat-marker-blue.png",
        "qianxing-map-label.png",
    ] {
        assert!(!is_text_template(Path::new("assets").join(name).as_path()));
    }
}

#[test]
fn text_anchor_quality_rejects_flat_patch() {
    let (template, _, hit) = text_quality_fixture(40, 230, false);
    let flat = DynamicImage::ImageRgb8(RgbImage::from_pixel(80, 50, Rgb([160; 3])));
    assert!(
        !text_anchor_quality_ok(&flat, hit, &template),
        "纯色 patch 的 contrast 为 0，必须拒绝"
    );
}

#[test]
fn text_anchor_quality_rejects_wrong_text_via_match_score() {
    let directory = std::env::temp_dir().join(format!(
        "miliastra-text-quality-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::create_dir_all(&directory).expect("create temp dir");
    // 文件名命中文字模板清单，确保走生产匹配 + 质量校验路径。
    let path = directory.join("invite-goto-hall.png");
    let (template, _, _) = text_quality_fixture(40, 230, false);
    template.save(&path).expect("save template");

    // 错误文字：同样的对比度/ink，但笔画方向完全不同，分数必须低于阈值。
    let (_, correct_patch, _) = text_quality_fixture(40, 230, false);
    let (_, wrong_patch, _) = text_quality_fixture(40, 230, true);
    // `wrong_patch` 是横笔画；用竖笔画模板去匹配它。
    let wrong_hit = best_template_hit(&wrong_patch, None, &path, 0.95)
        .expect("match wrong text")
        .is_none();
    // 正确文字仍然命中。
    let correct_hit = best_template_hit(&correct_patch, None, &path, 0.95)
        .expect("match correct text")
        .expect("correct text should hit");
    assert!(wrong_hit, "方向不同的错误文字必须被分数拒绝");
    assert!(correct_hit.score >= 0.99);
    let _ = fs::remove_dir_all(&directory);
}

/// 模板 × 真实帧的逐条打分导出（人工审计用，默认忽略）。
///
/// 输入 TSV（制表符分隔，首行 `#` 注释）：
/// `模板路径 帧路径 x y w h 降采样 阈值 期望命中(1|0) 说明`。
/// 每条都用生产同一条 `best_template_candidate_downscaled`；`w/h = 0` 时整帧匹配。
/// 期望命中为 1 的条目必须 ≥ 阈值，为 0 的必须 < 阈值，否则测试失败。
/// 与画廊导出同一思路：只读真实帧、只调用生产匹配路径，不写死任何分数。
#[test]
#[ignore = "模板审计打分导出：需要 MILIASTRA_TEMPLATE_AUDIT / MILIASTRA_TEMPLATE_AUDIT_OUT"]
fn dump_template_audit_scores() {
    let Ok(list_path) = std::env::var("MILIASTRA_TEMPLATE_AUDIT") else {
        return;
    };
    let out_path =
        std::env::var("MILIASTRA_TEMPLATE_AUDIT_OUT").expect("MILIASTRA_TEMPLATE_AUDIT_OUT");
    let list = std::fs::read_to_string(&list_path)
        .unwrap_or_else(|error| panic!("读不到 {list_path}: {error}"));
    let mut output = String::new();
    let mut failures = Vec::new();
    let mut count = 0_usize;
    for (line_number, line) in list.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let columns: Vec<&str> = line.split('\t').collect();
        assert_eq!(
            columns.len(),
            10,
            "模板审计清单第 {} 行列数不是 10: {line:?}",
            line_number + 1
        );
        let template = columns[0];
        let frame = columns[1];
        let (x, y, w, h): (i32, i32, u32, u32) = (
            columns[2].parse().expect("x"),
            columns[3].parse().expect("y"),
            columns[4].parse().expect("w"),
            columns[5].parse().expect("h"),
        );
        let scale: u32 = columns[6].parse().expect("scale");
        let threshold: f32 = columns[7].parse().expect("threshold");
        let expect_hit = columns[8] == "1";
        let note = columns[9];
        let source = image::open(frame).unwrap_or_else(|error| panic!("打不开 {frame}: {error}"));
        // 与生产/Gallery 一致：非 1920×1080 的帧先按 Triangle 缩放到规范画布。
        let source = if source.width() == 1920 && source.height() == 1080 {
            source
        } else {
            source.resize_exact(1920, 1080, FilterType::Triangle)
        };
        let region = if w == 0 || h == 0 {
            None
        } else {
            Some(Rect::new(x, y, w, h))
        };
        let hit = best_template_candidate_downscaled(&source, region, Path::new(template), scale)
            .unwrap_or_else(|error| panic!("匹配 {template} × {frame} 失败: {error}"));
        let score = hit.as_ref().map(|hit| hit.score);
        let quality_ok = hit.as_ref().is_some_and(|hit| hit.quality_ok);
        // 命中几何用于校准搜索区：裁剪模板后原点会平移，必须按真实命中位置
        // 算出新区域的期望左上角，不能只把旧区域宽高改小。
        let hit_rect = hit.as_ref().map(|hit| {
            serde_json::json!({
                "x": hit.x,
                "y": hit.y,
                "width": hit.width,
                "height": hit.height,
            })
        });
        // 负样本“无候选”也算通过（平坦画面防护会直接拒绝，不返回分数）；
        // 文字模板的候选即使分数过阈值，只要 quality_ok=false 也按不命中处理。
        let passed = match (hit, expect_hit) {
            (Some(hit), true) => hit.quality_ok && hit.score >= threshold,
            (Some(hit), false) => !hit.quality_ok || hit.score < threshold,
            (None, false) => true,
            (None, true) => false,
        };
        if !passed {
            failures.push(format!(
                    "{template} × {frame} ({note}) 期望{} 阈值 {threshold:.3} 实际 {}（quality_ok={quality_ok}）",
                    if expect_hit { "命中" } else { "不命中" },
                    score.map(|score| format!("{score:.4}")).unwrap_or_else(|| "无候选".into()),
                ));
        }
        output.push_str(
            &serde_json::json!({
                "template": template,
                "frame": frame,
                "region": {"x": x, "y": y, "width": w, "height": h},
                "downscale": scale,
                "threshold": threshold,
                "expect_hit": expect_hit,
                "score": score,
                "quality_ok": quality_ok,
                "hit": hit_rect,
                "pass": passed,
                "note": note,
            })
            .to_string(),
        );
        output.push('\n');
        count += 1;
    }
    std::fs::write(&out_path, output).unwrap_or_else(|error| panic!("写不到 {out_path}: {error}"));
    println!(
        "模板审计：{count} 条，失败 {} 条 -> {out_path}",
        failures.len()
    );
    for failure in &failures {
        println!("  失败: {failure}");
    }
    assert!(failures.is_empty(), "模板审计存在未达预期条目");
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
