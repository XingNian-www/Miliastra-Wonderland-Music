//! 世界判据（提瓦特 / 千星）：HUD 在场门、青色条纹带、底栏白键数。

use crate::geometry::Rect;
use crate::runtime::ui::UiEvidenceRect;
use crate::runtime::ui::UiWorldProbeEvidence;
use image::DynamicImage;

use super::hall::{
    HALL_BAR_OVERWORLD_MAX_TILES, HALL_BAR_OVERWORLD_MIN_TILES, HALL_BAR_WHITE_NEAR,
    hall_bar_looks_like_hall_keys, hall_bar_signature, hall_bar_signature_with_threshold,
};
use super::sampling::{FULL_FRAME_MIN_HEIGHT, FULL_FRAME_MIN_WIDTH, RgbSampler};

/// 世界判据（提瓦特 vs 千星）统计的**青色条纹带**区域。
///
/// 千星在大厅左上角派蒙头像周围多了一整套装饰（半透明蓝色圆盘、青色条纹带、
/// 黄色四角星、金色闪电条），提瓦特没有。其中西南侧这条青色条纹带最干净。
/// 坐标是**相对整帧左上角的绝对坐标**，在第 57 节的 1920x1080 帧上量出来的。
///
/// 与第 48 节的底栏近白占比同理，这是**区域上的标量统计**而不是 1:1 模板匹配，
/// 所以对位移与轻度缩放都不敏感（实测位移 ±6 像素时计数完全不变）。
pub(super) const WORLD_CYAN_BAND_RECT: Rect = Rect::new(76, 60, 18, 25);

/// 记一个像素属于青色条纹带的**相对色差下限**：R 必须比 G、B 都低这么多。
///
/// 用相对色差而不是绝对亮度阈值（例如 G>150 且 B>150 且 R<130），是为了
/// **对整屏亮度平移与增益都不敏感**。实测对比（提瓦特最高 / 千星最低之间的空档）：
/// - 绝对规则：亮度 ±31 时空档只剩 3~5，增益 0.78 时两侧区间**交错**（空档 −34）
/// - 相对规则：亮度 ±31 时空档 166（两个方向完全相同），增益 0.78/1.325 时空档 188/124
pub(super) const WORLD_CYAN_CHANNEL_DIFF: i32 = 30;

/// 判为**千星**的青色条纹带像素数**下限**。
///
/// 来源（实测空档内的**三等分点**，不是凑的）：语料上提瓦特侧最高 **26**
/// （雪山帧；冷启动与沙漠帧都是 0），千星侧最低 **154**（大厅帧）⇒ 空档 26~154。
/// 空档长度 128，三等分点 26 + 128/3 ≈ 68.7 与 26 + 256/3 ≈ 111.3，
/// 取整成 68 / 111。两侧余量对称（提瓦特侧 42，千星侧 43）。
pub(super) const WORLD_CYAN_QIANXING_MIN: usize = 111;

/// 判为**提瓦特**的青色条纹带像素数**上限**。
/// 高于它、又不到 `WORLD_CYAN_QIANXING_MIN` 的，一律返回 Unknown ——
/// 与 `unknown` 的纪律一致：判不出来就不猜。
pub(super) const WORLD_CYAN_TEYVAT_MAX: usize = 68;

/// 世界 HUD 参考块：4 张标注提瓦特 + 6 张标注千星帧在 (40,37)-(71,76) 这 1280 像素上
/// 逐通道归一化后的均值。生成脚本 \`dist/worlddata/mkref.py\`，格式：
/// magic("MWW1") + version(u32) + x(i32) + y(i32) + width(u32) + height(u32)
/// + 提瓦特参考(i16×N) + 千星参考(i16×N)，N = width*height*3，归一化值 ×1000。
///
/// 内嵌而不是走配置：它是一份**冻结的标定数据**，改了要连同第 57 节的实测一起改，
/// 不适合让用户随手配置。
pub(super) const WORLD_HUD_REF: &[u8] = include_bytes!("../../../assets/world-hud-ref.bin");
pub(super) const WORLD_HUD_REF_MAGIC: &[u8] = b"MWW1";
pub(super) const WORLD_HUD_REF_VERSION: u32 = 1;
pub(super) const WORLD_HUD_REF_SCALE: f32 = 1000.0;
pub(super) const WORLD_HUD_REF_HEADER: usize = 24;
/// 比较时允许的像素平移范围。第 57 节实测：不允许平移时，整片精确模式匹配
/// **±1 像素就翻判断**；允许 ±2 之后位移 ±1/±2/±3 都不再翻。
pub(super) const WORLD_HUD_SEARCH: i32 = 2;

/// 比较时允许的**尺度**搜索范围。
///
/// 第 58 节的扰动实测：0.85 / 1.15 缩放后拒判（`classify_at` 把分辨率归一之后，
/// UI 本身还是被放大/缩小了）。这里直接把这些尺度纳入搜索：每个采样格点改成取
/// 对应大小窗口的平均（scale != 1 即降采样），把放大带来的模糊吃进平均值里。
///
/// 范围取 ±15%：与第 58 节扰动实测用的 0.85 / 1.15 对齐。
/// **不放开无边界搜索**：搜索范围越大越容易撞上菜单内容，
/// 范围是否安全由 `hud_presence_is_separated_by_reference_distance` 那条
/// "在场与不在场区间不重叠"的断言守住，而不是靠感觉。
pub(super) const WORLD_HUD_SCALES: [f32; 7] = [0.85, 0.90, 0.95, 1.00, 1.05, 1.10, 1.15];

/// HUD 在场判定的**距离上限**（两组参考里较近的那一个）。
///
/// 来源（实测的对数中点，不是凑的）：用本节实际落地的算法（±2 搜索、1280 像素、
/// 逐通道平均绝对差）在语料上量——HUD 在场最大 **0.0579**（白天大厅），
/// HUD 不在最小 **1.0000**（纯白加载）⇒ 空档 17.3 倍，
/// 对数中点 √(0.0579×1.0000) ≈ 0.2406，取 **0.24**。
pub(super) const WORLD_HUD_PRESENT_MAX: f32 = 0.24;

/// 从内嵌参考块里解出区域与两组参考的字节切片。
pub(super) fn parse_world_hud_ref() -> Option<(Rect, &'static [u8], &'static [u8])> {
    let bytes = WORLD_HUD_REF;
    if bytes.len() < WORLD_HUD_REF_HEADER || &bytes[0..4] != WORLD_HUD_REF_MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    if version != WORLD_HUD_REF_VERSION {
        return None;
    }
    let x = i32::from_le_bytes(bytes[8..12].try_into().ok()?);
    let y = i32::from_le_bytes(bytes[12..16].try_into().ok()?);
    let width = u32::from_le_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_le_bytes(bytes[20..24].try_into().ok()?);
    let values = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(3)?;
    if bytes.len() != WORLD_HUD_REF_HEADER + values * 2 * 2 {
        return None;
    }
    let teyvat = &bytes[WORLD_HUD_REF_HEADER..WORLD_HUD_REF_HEADER + values * 2];
    let qianxing = &bytes[WORLD_HUD_REF_HEADER + values * 2..];
    Some((Rect::new(x, y, width, height), teyvat, qianxing))
}

/// 读参考块里的第 index 个归一化值。
pub(super) fn world_hud_ref_value(block: &[u8], index: usize) -> f32 {
    let offset = index * 2;
    let raw = i16::from_le_bytes([block[offset], block[offset + 1]]);
    f32::from(raw) / WORLD_HUD_REF_SCALE
}

/// 画面与两组世界参考的归一化平均绝对差，各取 ±2 像素搜索范围内的最小值。
///
/// 返回 \`(到提瓦特, 到千星)\`；画面太小或参考块损坏时返回 \`None\`（拒判）。
/// 归一化是逐通道减区域均值再除平均绝对偏差 —— 与青色标量用相对色差同理，
/// 为的是对整屏亮度平移与增益都不敏感。
pub(super) fn world_hud_distances(image: &DynamicImage) -> Option<(f32, f32, f32)> {
    if image.width() < FULL_FRAME_MIN_WIDTH || image.height() < FULL_FRAME_MIN_HEIGHT {
        return None;
    }
    let (rect, teyvat_ref, qianxing_ref) = parse_world_hud_ref()?;
    let sampler = RgbSampler::new(image);
    let (image_width, image_height) = sampler.dimensions();
    let width = rect.width as i32;
    let height = rect.height as i32;
    let count = (width * height) as f32;

    let pixel = |x: i32, y: i32| -> [f32; 3] {
        let clamped_x = x.clamp(0, image_width as i32 - 1) as u32;
        let clamped_y = y.clamp(0, image_height as i32 - 1) as u32;
        let p = sampler.pixel(clamped_x, clamped_y);
        [f32::from(p[0]), f32::from(p[1]), f32::from(p[2])]
    };

    let mut best_teyvat = f32::MAX;
    let mut best_qianxing = f32::MAX;
    // 达到最佳距离时用的尺度。世界判据的青色标量必须用**同一个尺度**去量，
    // 否则门放行了缩放帧、标量却还在原始坐标上数，会数出接近 0 的青色
    // 从而**自信判成提瓦特** —— 那比拒判更糟。
    let mut best_scale = 1.0_f32;
    for scale in WORLD_HUD_SCALES {
        // 预计算每个采样格点覆盖的整数像素窗口。它与 dx/dy 无关，
        // 放到外层算一次，省掉内层重复的 floor/ceil。
        // scale = 1 时每个窗口就是一个像素，退化成逐像素比较（与首版一致）。
        let mut windows: Vec<[i32; 4]> = Vec::with_capacity((width * height) as usize);
        for j in 0..height {
            for i in 0..width {
                // **原点也要跟着缩放**：UI 整体被放大/缩小时，
                // 区域的原点会同步移动，只缩放步长是错的（第 58 节踩过这个坑：
                // 只缩放步长会让 0.85 / 1.15 的帧无论怎么搜都找不到参考）。
                let left = (rect.x as f32 + i as f32) * scale;
                let top = (rect.y as f32 + j as f32) * scale;
                let first_x = left.floor() as i32;
                let first_y = top.floor() as i32;
                let last_x = ((left + scale).ceil() as i32).max(first_x + 1);
                let last_y = ((top + scale).ceil() as i32).max(first_y + 1);
                windows.push([first_x, first_y, last_x, last_y]);
            }
        }
        // 采样一个格点：取窗口内的平均。scale != 1 时这是**降采样**，
        // 正是为了把放大后的模糊吃进平均值里，而不是逐个像素去比。
        let sample = |window: [i32; 4], dx: i32, dy: i32| -> [f32; 3] {
            let mut acc = [0.0_f32; 3];
            let mut samples = 0.0_f32;
            for y in window[1]..window[3] {
                for x in window[0]..window[2] {
                    let p = pixel(x + dx, y + dy);
                    acc[0] += p[0];
                    acc[1] += p[1];
                    acc[2] += p[2];
                    samples += 1.0;
                }
            }
            if samples <= 0.0 {
                return pixel(window[0] + dx, window[1] + dy);
            }
            [acc[0] / samples, acc[1] / samples, acc[2] / samples]
        };
        for dy in -WORLD_HUD_SEARCH..=WORLD_HUD_SEARCH {
            for dx in -WORLD_HUD_SEARCH..=WORLD_HUD_SEARCH {
                // 每个格点**只采样一次**，均值/偏差/参考三遍复用同一批样本。
                // 此前三遍各采样一次，等于同一次比较里把最贵的采样做了三遍。
                let sampled = windows
                    .iter()
                    .map(|window| sample(*window, dx, dy))
                    .collect::<Vec<[f32; 3]>>();
                // 第一遍：逐通道均值。
                let mut sums = [0.0_f32; 3];
                for p in &sampled {
                    sums[0] += p[0];
                    sums[1] += p[1];
                    sums[2] += p[2];
                }
                let means = [sums[0] / count, sums[1] / count, sums[2] / count];
                // 第二遍：逐通道平均绝对偏差。
                let mut deviations = [0.0_f32; 3];
                for p in &sampled {
                    deviations[0] += (p[0] - means[0]).abs();
                    deviations[1] += (p[1] - means[1]).abs();
                    deviations[2] += (p[2] - means[2]).abs();
                }
                let deviations = [
                    deviations[0] / count,
                    deviations[1] / count,
                    deviations[2] / count,
                ];
                // 第三遍：与两组参考比。
                let mut total_teyvat = 0.0_f32;
                let mut total_qianxing = 0.0_f32;
                for (index, p) in sampled.iter().enumerate() {
                    for channel in 0..3 {
                        let deviation = if deviations[channel] > 1e-6 {
                            deviations[channel]
                        } else {
                            1.0
                        };
                        let normalized = (p[channel] - means[channel]) / deviation;
                        total_teyvat += (normalized
                            - world_hud_ref_value(teyvat_ref, index * 3 + channel))
                        .abs();
                        total_qianxing += (normalized
                            - world_hud_ref_value(qianxing_ref, index * 3 + channel))
                        .abs();
                    }
                }
                let divisor = count * 3.0;
                let teyvat_distance = total_teyvat / divisor;
                let qianxing_distance = total_qianxing / divisor;
                if teyvat_distance.min(qianxing_distance) < best_teyvat.min(best_qianxing) {
                    best_scale = scale;
                }
                best_teyvat = best_teyvat.min(teyvat_distance);
                best_qianxing = best_qianxing.min(qianxing_distance);
            }
        }
    }
    Some((best_teyvat, best_qianxing, best_scale))
}

/// 左上角 HUD（派蒙头像区）在不在；在则返回它所在的**尺度**。
///
/// 世界判据只在它**在场**时才有意义：全屏菜单的左上角是菜单内容，
/// 青色计数恒为 0，不加这道门就会把菜单误报成提瓦特。
///
/// 返回尺度是为了让青色标量在**同一尺度**上量。否则门放行了缩放帧、
/// 标量还在原始坐标上数，会数出接近 0 的青色从而自信判成提瓦特 —— 那比拒判更糟。
pub(super) fn world_hud_scale(image: &DynamicImage) -> Option<f32> {
    let (teyvat, qianxing, scale) = world_hud_distances(image)?;
    if teyvat.min(qianxing) <= WORLD_HUD_PRESENT_MAX {
        Some(scale)
    } else {
        None
    }
}

/// 世界判据的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorldGuess {
    Teyvat,
    Qianxing,
    /// 计数落在两侧空档之间，或画面太小根本量不了。**显式拒判**，不猜。
    Unknown,
}

impl WorldGuess {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Teyvat => "teyvat",
            Self::Qianxing => "qianxing",
            Self::Unknown => "unknown",
        }
    }
}

/// 统计左上角青色条纹带里的像素数；画面太小（不是整帧）时返回 `None`。
///
/// 这条判据只在**整帧截图**上有意义：它用的是相对整帧左上角的绝对坐标。
/// 与 `loading_rule` 一样，小裁剪图（按钮、色条）直接不判，避免误报。
/// 统计左上角青色条纹带里的格点数；画面太小（不是整帧）时返回 `None`。
///
/// 这条判据只在**整帧截图**上有意义：它用的是相对整帧左上角的绝对坐标。
/// 与 `loading_rule` 一样，小裁剪图（按钮、色条）直接不判，避免误报。
///
/// `scale` 是 HUD 在场检测找到的尺度。**UI 被整体缩放时区域原点要跟着缩**，
/// 否则会在错误坐标上数、数出接近 0 的青色，进而自信判成提瓦特。
/// 每个格点取对应大小窗口的**多数票**（scale = 1 时窗口只有一个像素，
/// 与第一版逐像素的规则逐字相同）。
pub(super) fn world_cyan_band_count(image: &DynamicImage, scale: f32) -> Option<usize> {
    if image.width() < FULL_FRAME_MIN_WIDTH || image.height() < FULL_FRAME_MIN_HEIGHT {
        return None;
    }
    let sampler = RgbSampler::new(image);
    let (image_width, image_height) = sampler.dimensions();
    let step = scale.max(1.0);
    let threshold = WORLD_CYAN_CHANNEL_DIFF as f32;
    let mut count = 0_usize;
    for j in 0..WORLD_CYAN_BAND_RECT.height {
        for i in 0..WORLD_CYAN_BAND_RECT.width {
            let left = (WORLD_CYAN_BAND_RECT.x as f32 + i as f32) * scale;
            let top = (WORLD_CYAN_BAND_RECT.y as f32 + j as f32) * scale;
            let first_x = left.floor() as i32;
            let first_y = top.floor() as i32;
            let last_x = ((left + step).ceil() as i32).max(first_x + 1);
            let last_y = ((top + step).ceil() as i32).max(first_y + 1);
            let mut hits = 0.0_f32;
            let mut samples = 0.0_f32;
            for y in first_y..last_y {
                for x in first_x..last_x {
                    let clamped_x = x.clamp(0, image_width as i32 - 1) as u32;
                    let clamped_y = y.clamp(0, image_height as i32 - 1) as u32;
                    let pixel = sampler.pixel(clamped_x, clamped_y);
                    let red = f32::from(pixel[0]);
                    let green = f32::from(pixel[1]);
                    let blue = f32::from(pixel[2]);
                    if green - red > threshold && blue - red > threshold {
                        hits += 1.0;
                    }
                    samples += 1.0;
                }
            }
            if samples > 0.0 && hits * 2.0 >= samples {
                count += 1;
            }
        }
    }
    Some(count)
}

/// 由青色条纹带计数判世界；落在空档里就返回 `WorldGuess::Unknown`。
pub(super) fn guess_world_from_cyan_count(count: Option<usize>) -> WorldGuess {
    match count {
        None => WorldGuess::Unknown,
        Some(count) if count >= WORLD_CYAN_QIANXING_MIN => WorldGuess::Qianxing,
        Some(count) if count <= WORLD_CYAN_TEYVAT_MAX => WorldGuess::Teyvat,
        Some(_) => WorldGuess::Unknown,
    }
}

/// 组装世界判据的证据。
///
/// **只在 HUD 在场时给结论。** 这条判据读的是左上角派蒙头像区的青色条纹带，
/// 而全屏菜单（奇域枢纽菜单、主菜单大世界、提瓦特地图、背包、祈愿…）的左上角是
/// **菜单内容**不是 HUD，那里青色计数恒为 0 —— 不加这道门就会把菜单一律
/// 自信地误报成「提瓦特」，正是目标禁止的行为。
///
/// 门用的是 **HUD 在场检测**（`world_hud_scale`），**不是界面 kind**。
/// 用 kind 做过一版，实测有两个毛病：`unknown` 里同时装着全屏菜单和提瓦特大世界，
/// 而 app 的 `overworld` 只能靠"连续 2 帧 unknown"的回退得到 —— 于是那版门
/// 既挡不住菜单也判不了提瓦特，只剩千星大厅一侧能用。
///
/// 门之外仍然把计数报出来（便于复核），但结论固定为 `not_applicable`。
/// 门内如果计数落在两侧空档里，结论仍然是 `unknown`。
/// 画面是不是「**提瓦特大世界**」——人站在提瓦特露天地面、HUD 俱全。
///
/// **用户口径（task-105 修正）**：Overworld = **HUD 在场 + 底栏白键数 1~4 个**
/// （实测主世界通常 2 个；大厅 7–8 个，5–6 是空档）。旧的「青色条纹带」判据在
/// `teleport-53` 的连续生活帧上落在 unknown 空档（650 帧语料里 195 帧提瓦特
/// 被判 Unknown），所以主判据换成底栏白键数；青带仍由 `world_probe_evidence`
/// 作为「哪个世界」的正交证据单独上报。
///
/// 负例纪律：**有损压缩帧**里的千星大厅底栏白键会掉到 240–254，严格 255 数出
/// 0 块。这里额外用近白结构（`HALL_BAR_WHITE_NEAR`）确认"这排 7–8 块像不像
/// 大厅底栏"，像就**不判提瓦特**——而不是回退成用 225 数键（那会违反用户
/// 「只认纯 255」的要求）。主菜单/面板/地图 HUD 不在场，直接被门挡住。
pub(crate) fn world_is_teyvat(image: &DynamicImage) -> bool {
    // 外部调用方（如启动链"是否已在大世界"）没有 screen 配置；用默认画布上的
    // 底栏区域（与默认配置 0,985,600,95 一致）走同一条判据。
    world_is_teyvat_in(
        image,
        crate::config::ScreenConfig::default().hall_bar_rect.into(),
    )
}

/// 与 [`world_is_teyvat`] 相同的判据，但由调用方显式给出底栏区域
/// （界面分类器自己有 `screen` 配置，必须用实际区域，不能用默认值）。
pub(crate) fn world_is_teyvat_in(image: &DynamicImage, hall_bar: Rect) -> bool {
    // 必须用 `world_hud_scale` 而不是 `world_hud_distances`：前者带**HUD 在场门**
    // （`teyvat.min(qianxing) <= WORLD_HUD_PRESENT_MAX`），后者没有。
    // 少了这道门，全屏面板会被放行，而它底栏也几乎没有白键。
    if world_hud_scale(image).is_none() {
        // HUD 不在场 ⇒ 这是全屏面板/菜单，不是大世界。
        return false;
    }
    // 白色判据严格 255；先量一次。
    let (raw_tiles, _, _, _) = hall_bar_signature(image, hall_bar);
    // 近白（225）只在两种情况下用：① 有损帧的大厅底栏否决；② 严格 255 = 0 时
    // 还原"无损抓图里本该是纯白"的底栏键数（等价于仓库测试里的
    // `snap_near_white_to_pure_white`，线上 DPI 缩放/有损输入也需要）。
    // **绝不用近白去数 Primary 大厅的键**——那条判据仍只认纯 255。
    let (near_tiles, near_min_height, near_max_height, near_top_spread) =
        hall_bar_signature_with_threshold(image, hall_bar, HALL_BAR_WHITE_NEAR);
    if hall_bar_looks_like_hall_keys(
        near_tiles,
        near_min_height,
        near_max_height,
        near_top_spread,
    ) {
        // 近白结构就是千星大厅那排 7–8 键（有损帧）⇒ 不是提瓦特。
        return false;
    }
    let tiles = if raw_tiles > 0 { raw_tiles } else { near_tiles };
    (HALL_BAR_OVERWORLD_MIN_TILES..=HALL_BAR_OVERWORLD_MAX_TILES).contains(&tiles)
}

pub(super) fn world_probe_evidence(image: &DynamicImage) -> UiWorldProbeEvidence {
    let hud_scale = world_hud_scale(image);
    let count = world_cyan_band_count(image, hud_scale.unwrap_or(1.0));
    let guess = match hud_scale {
        Some(_) => guess_world_from_cyan_count(count).name(),
        None => "not_applicable",
    };
    UiWorldProbeEvidence::new(
        UiEvidenceRect::new(
            WORLD_CYAN_BAND_RECT.x,
            WORLD_CYAN_BAND_RECT.y,
            WORLD_CYAN_BAND_RECT.width,
            WORLD_CYAN_BAND_RECT.height,
        ),
        count,
        WORLD_CYAN_TEYVAT_MAX,
        WORLD_CYAN_QIANXING_MIN,
        guess,
    )
}
