//! 千星大厅底栏结构判据（白色按键块数 / 同高 / 同一水平线）与底栏夹具吸附。

use crate::geometry::Rect;
use crate::geometry::crop_canvas;
use image::DynamicImage;

/// 底栏「白色按键块数」的范围（2px 采样下的 4 连通块，单块至少 [`HALL_BAR_TILE_MIN_AREA`] 个采样格）。
///
/// 来源（1133 帧整屏实测，2026-10-17）：大厅 **7–12 块**；提瓦特 2–4 块；主菜单 3–10 块；登录/加载 0 块。
/// 这是**结构**量而不是模板分数，对位移/缩放不敏感；改采样步长或 `screen.hall_bar_rect` 必须重测。
pub(super) const HALL_BAR_MIN_TILES: usize = 7;
/// 千星大厅底栏白键的**上限**。用户 2026-11-10 明确：大厅底栏只有 7~8 个白键
/// （聊天框关 8 个、开 7 个）；旧值 14 会把拼图/设置宫格等更宽的白色结构算进大厅。
pub(super) const HALL_BAR_MAX_TILES: usize = 8;
/// Overworld 底栏白键数的允许范围（用户 2026-11-10 第二次修正）：**1~4 个**，
/// 实测主世界通常 2 个（Enter + 聊天/派蒙等 1~2 个白键）。下限 1 排除
/// "根本没有白键"的帧（0 个 ⇒ 没有证据，不猜）；上限 4 与千星大厅的 7–8 个
/// 之间留 5–6 的空档。
pub(super) const HALL_BAR_OVERWORLD_MIN_TILES: usize = 1;
pub(super) const HALL_BAR_OVERWORLD_MAX_TILES: usize = 4;

/// 底栏白键的**白色判据**：只认纯白 255。
///
/// 用户明确要求（2026-11-06）：**不要回退到 225**，那档会把 240–254 的近白像素
/// 也算成按键——亮背景、聊天气泡、抗锯齿边缘都会混进连通块，块数/高度/水平线
/// 被污染，进而污染状态判定与日志。
///
/// 实测支持：2026-11-06 在 .119 上切一级值守连拍 5 帧，**255 与 225 得到完全相同的
/// 签名**（7 块 / 高 26 / 极差 0），说明线上抓图是无损的，收紧不丢识别能力。
///
/// 代价：若窗口被 resize 到画布（DPI 非 100%、窗口化非 1920×1080）或输入是有损
/// JPEG，纯白会被插值/压缩成 240–254，这条规则**判不出来**——这是刻意选择：
/// 宁可让 marker 那几条规则接手，也不把噪声算成按钮。
pub(super) const HALL_BAR_WHITE_EXACT: u8 = 255;
/// 近白阈值：**只用于 Overworld 判据里的「有损大厅帧」否决**。
///
/// 用户明确要求 Primary 的白键判据只认纯 255（225 会把 240–254 的噪声算成按键、
/// 污染状态数据），因此这里**不拿它数键数**，只用它回答"这排 7–8 块的近白结构
/// 是不是千星大厅的底栏"——有损压缩帧里大厅底栏恰好落在这个区间。提瓦特底栏
/// 只有 1–2 块，不会被它误否决。
pub(super) const HALL_BAR_WHITE_NEAR: u8 = 225;

/// 单块白键的**真实像素高度**允许区间。
///
/// 大厅底栏键实测 **26–28 px**（1080p 语料；用户实机另有 25 / 32 px 两档），
/// 取 **[22, 36]** 覆盖这两档、两侧留 2–4 px 余量。
/// 主菜单 / 设置宫格是整块白底 ⇒ 60–64 px，直接排除。
pub(super) const HALL_BAR_MIN_TILE_HEIGHT_PX: usize = 22;
pub(super) const HALL_BAR_MAX_TILE_HEIGHT_PX: usize = 36;

/// 所有键必须落在**同一条水平线**上：顶部 y 极差 ≤4 真实像素（"误差留一点点"）。
/// 实测大厅帧里 7–8 个键的顶部完全一致（极差 0）；设置宫格则是 985 与 1045 两行（极差 60）。
pub(super) const HALL_BAR_MAX_TILE_TOP_SPREAD_PX: usize = 4;

/// 计入「一块按键」的最小面积（单位：2px 采样格）。低于它的白点按噪点丢弃。
pub(super) const HALL_BAR_TILE_MIN_AREA: usize = 40;

/// 测试专用：把库内**有损 JPEG 夹具**底栏区域的近白像素吸附成纯白。
///
/// 线上抓图是无损的（2026-11-06 在 .119 实测：255 与 225 签名一致），但仓库里的
/// 大厅夹具是 JPEG，纯白被压缩成 240–254，严格判据会判不出来。
/// 夹具又不能换成真实帧——里面有玩家昵称/UID（MUST_READ §9）。
/// 所以在测试侧做一次吸附，模拟"无损抓图"这一前提；生产代码不受影响。
#[cfg(test)]
pub(crate) fn snap_near_white_to_pure_white(image: &mut DynamicImage, rect: Rect, white_min: u8) {
    let mut rgb = image.to_rgb8();
    let (width, height) = rgb.dimensions();
    // Rect 的 x/y 是 i32、width/height 是 u32：先夹到非负再按 u32 相加。
    let x0 = rect.x.max(0) as u32;
    let y0 = rect.y.max(0) as u32;
    let x1 = x0.saturating_add(rect.width).min(width);
    let y1 = y0.saturating_add(rect.height).min(height);
    for y in y0..y1 {
        for x in x0..x1 {
            let pixel = rgb.get_pixel(x, y);
            if pixel[0].min(pixel[1]).min(pixel[2]) >= white_min {
                rgb.put_pixel(x, y, image::Rgb([255, 255, 255]));
            }
        }
    }
    *image = DynamicImage::ImageRgb8(rgb);
}

/// 底栏结构：`(白色按键块数, 单块中位面积)`。
///
/// 千星大厅的底栏是一排**小圆角白键**（聊天 / Enter / 👥 / O / 麦克风 / Alt+N / 喇叭 / N）：
/// 块数多（7–12），每块是**同高、同一条水平线的小圆角键**（实测高 26–28 px、顶部 y 极差 0）；
/// 提瓦特只有一个 Enter（2–4 块）；主菜单 / 设置宫格是整块白底（高 60–64 px，或分成两行）。
/// 返回 `(块数, 最小高度 px, 最大高度 px, 顶部 y 极差 px)`。
/// 用几何量而不是模板分数，是为了绕开 1:1 SAD 对缩放不鲁棒的问题（第 47 节教训）；
/// 也不用「近白占比」——那是亮度量，亮背景大厅会把它顶高。
pub(super) fn hall_bar_signature(image: &DynamicImage, rect: Rect) -> (usize, usize, usize, usize) {
    hall_bar_signature_with_threshold(image, rect, HALL_BAR_WHITE_EXACT)
}

pub(super) fn hall_bar_signature_with_threshold(
    image: &DynamicImage,
    rect: Rect,
    white_min: u8,
) -> (usize, usize, usize, usize) {
    let Ok(region) = crop_canvas(image, rect) else {
        return (0, 0, 0, 0);
    };
    let rgb = region.to_rgb8();
    let (width, height) = rgb.dimensions();
    let grid_w = (width as usize).div_ceil(2);
    let grid_h = (height as usize).div_ceil(2);
    let mut grid = vec![false; grid_w * grid_h];
    for gy in 0..grid_h {
        for gx in 0..grid_w {
            let pixel = rgb.get_pixel((gx * 2) as u32, (gy * 2) as u32);
            if pixel[0].min(pixel[1]).min(pixel[2]) >= white_min {
                grid[gy * grid_w + gx] = true;
            }
        }
    }
    let total = grid_w * grid_h;
    let mut seen = vec![false; total];
    let mut stack: Vec<usize> = Vec::new();
    let mut heights_px: Vec<usize> = Vec::new();
    let mut tops_px: Vec<usize> = Vec::new();
    for start in 0..total {
        if !grid[start] || seen[start] {
            continue;
        }
        stack.clear();
        stack.push(start);
        seen[start] = true;
        let mut area = 0_usize;
        let mut y_min = usize::MAX;
        let mut y_max = 0_usize;
        while let Some(index) = stack.pop() {
            area += 1;
            let x = index % grid_w;
            let y = index / grid_w;
            y_min = y_min.min(y);
            y_max = y_max.max(y);
            if x > 0 {
                let n = index - 1;
                if grid[n] && !seen[n] {
                    seen[n] = true;
                    stack.push(n);
                }
            }
            if x + 1 < grid_w {
                let n = index + 1;
                if grid[n] && !seen[n] {
                    seen[n] = true;
                    stack.push(n);
                }
            }
            if y > 0 {
                let n = index - grid_w;
                if grid[n] && !seen[n] {
                    seen[n] = true;
                    stack.push(n);
                }
            }
            if y + 1 < grid_h {
                let n = index + grid_w;
                if grid[n] && !seen[n] {
                    seen[n] = true;
                    stack.push(n);
                }
            }
        }
        if area >= HALL_BAR_TILE_MIN_AREA {
            // 采样格 → 真实像素：高度 ×2、顶部 y ×2（区域内的相对 y 即可，极差不受影响）。
            heights_px.push((y_max - y_min + 1) * 2);
            tops_px.push(y_min * 2);
        }
    }
    if heights_px.is_empty() {
        return (0, 0, 0, 0);
    }
    let min_height = *heights_px.iter().min().unwrap_or(&0);
    let max_height = *heights_px.iter().max().unwrap_or(&0);
    let top_spread = tops_px.iter().max().unwrap_or(&0) - tops_px.iter().min().unwrap_or(&0);
    (heights_px.len(), min_height, max_height, top_spread)
}

/// 底栏签名是否**长得像千星大厅那排白键**：块数 7–8、同高、同一水平线。
///
/// 抽出来有两个用处：① 大厅 Primary 判据（严格 255）；② Overworld 判据里把
/// 有损压缩帧（白键被压到 240–254、严格 255 数出 0 块）**否决**掉，避免把
/// 千星大厅误判成提瓦特大世界。它只做"像不像大厅"的结构判断，不改变
/// 「白色判据只认纯 255」的既有用户要求。
pub(super) fn hall_bar_looks_like_hall_keys(
    tiles: usize,
    min_height_px: usize,
    max_height_px: usize,
    top_spread_px: usize,
) -> bool {
    (HALL_BAR_MIN_TILES..=HALL_BAR_MAX_TILES).contains(&tiles)
        && min_height_px >= HALL_BAR_MIN_TILE_HEIGHT_PX
        && max_height_px <= HALL_BAR_MAX_TILE_HEIGHT_PX
        && top_spread_px <= HALL_BAR_MAX_TILE_TOP_SPREAD_PX
}
