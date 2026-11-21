//! 主菜单大世界（派蒙 ESC）左侧功能宫格的布局无关检测。

use image::DynamicImage;

/// 主菜单大世界左侧功能宫格的**布局无关**检测参数。
///
/// 用户 2026-11-10 指出固定「队伍配置/成就」坐标不稳定、蓝色区域会变，要求**极快**；
/// 因此不再用固定模板/固定列距，改为：
///  ① downscale 1/8 后按**瓦片底色**取掩码（深蓝灰，不是白色图标）；
///  ② 4 连通域分析，取尺寸/长宽比符合"功能瓦片"的连通块；
///  ③ 要求足够多块（≥8）且排成多列多行（≥3 列、≥2 行）——**只看结构，不看坐标**；
///  ④ 允许菜单项平移/增删/顺序变化：检测只依赖"多块同尺寸瓦片构成的宫格"。
///
/// 颜色范围来自 1920×1080 真实派蒙 ESC 帧的瓦片底色实测（≈(66,75,92)），
/// 不是商城/图标/OCR 文案。
pub(super) const PAIMON_GRID_DOWNSCALE: u32 = 8;
/// 只扫画面左侧这一条（菜单面板所在侧），不是固定瓦片坐标。
pub(super) const PAIMON_GRID_LEFT_PERCENT: u32 = 45;
pub(super) const PAIMON_GRID_MIN_TILES: usize = 8;
pub(super) const PAIMON_GRID_MIN_COLS: usize = 3;
pub(super) const PAIMON_GRID_MIN_ROWS: usize = 2;

/// 一块「功能瓦片」在 1/8 下采样网格里的尺寸/面积范围。
pub(super) const PAIMON_GRID_TILE_MIN_AREA: usize = 60;
pub(super) const PAIMON_GRID_TILE_MAX_AREA: usize = 900;
pub(super) const PAIMON_GRID_TILE_MIN_W: usize = 10;
pub(super) const PAIMON_GRID_TILE_MAX_W: usize = 26;
pub(super) const PAIMON_GRID_TILE_MIN_H: usize = 12;
pub(super) const PAIMON_GRID_TILE_MAX_H: usize = 30;

/// 瓦片底色（深蓝灰）的通道范围；图标白、面板浅底、世界背景都不落在这里。
pub(super) fn is_paimon_grid_tile_pixel(pixel: &image::Rgb<u8>) -> bool {
    let (r, g, b) = (pixel[0] as i32, pixel[1] as i32, pixel[2] as i32);
    (40..=115).contains(&r)
        && (45..=125).contains(&g)
        && (60..=150).contains(&b)
        && b > r + 8
        && g + 2 >= r
        && (5..=40).contains(&(b - g))
}

/// 左侧菜单「多瓦片宫格结构」检测。返回 `(瓦片数, 行数, 列数)`；不是该结构返回 `None`。
///
/// **耗时**：1920×1080 → 只看左侧 45% → 1/8 采样（≈32k 像素）→ 14k 格掩码 +
/// 4 连通域；实测单帧远低于 5ms（见 `paimon_grid_detection_is_fast`）。
pub(super) fn paimon_menu_grid_structure(image: &DynamicImage) -> Option<(usize, usize, usize)> {
    let (width, height) = (image.width(), image.height());
    if width < 640 || height < 360 {
        return None;
    }
    let step = PAIMON_GRID_DOWNSCALE;
    let grid_w = (width * PAIMON_GRID_LEFT_PERCENT / 100 / step).max(1) as usize;
    let grid_h = (height / step).max(1) as usize;
    // Rgb8 帧直接借用；RGBA/灰度帧才做一次转换（线上截图是 Rgb8）。
    let converted = if image.as_rgb8().is_none() {
        Some(image.to_rgb8())
    } else {
        None
    };
    let rgb = image.as_rgb8().or(converted.as_ref())?;

    let mut mask = vec![false; grid_w * grid_h];
    for gy in 0..grid_h {
        let y = (gy as u32 * step + step / 2).min(height - 1);
        for gx in 0..grid_w {
            let x = (gx as u32 * step + step / 2).min(width - 1);
            mask[gy * grid_w + gx] = is_paimon_grid_tile_pixel(rgb.get_pixel(x, y));
        }
    }

    // 4 连通域 + 尺寸/长宽比过滤，收集"瓦片"包围盒。
    let mut seen = vec![false; mask.len()];
    let mut stack: Vec<usize> = Vec::new();
    let mut tiles: Vec<(i32, i32, i32, i32)> = Vec::new(); // (x0, y0, w, h)
    for start in 0..mask.len() {
        if !mask[start] || seen[start] {
            continue;
        }
        stack.clear();
        stack.push(start);
        seen[start] = true;
        let (mut area, mut x0, mut x1, mut y0, mut y1) = (0usize, i32::MAX, 0, i32::MAX, 0);
        while let Some(index) = stack.pop() {
            area += 1;
            let x = (index % grid_w) as i32;
            let y = (index / grid_w) as i32;
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
            let push = |nx: usize, ny: usize, stack: &mut Vec<usize>, seen: &mut Vec<bool>| {
                let n = ny * grid_w + nx;
                if mask[n] && !seen[n] {
                    seen[n] = true;
                    stack.push(n);
                }
            };
            if x > 0 {
                push((x - 1) as usize, y as usize, &mut stack, &mut seen);
            }
            if (x as usize) + 1 < grid_w {
                push((x + 1) as usize, y as usize, &mut stack, &mut seen);
            }
            if y > 0 {
                push(x as usize, (y - 1) as usize, &mut stack, &mut seen);
            }
            if (y as usize) + 1 < grid_h {
                push(x as usize, (y + 1) as usize, &mut stack, &mut seen);
            }
        }
        let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
        if (PAIMON_GRID_TILE_MIN_AREA..=PAIMON_GRID_TILE_MAX_AREA).contains(&area)
            && (PAIMON_GRID_TILE_MIN_W..=PAIMON_GRID_TILE_MAX_W).contains(&(w as usize))
            && (PAIMON_GRID_TILE_MIN_H..=PAIMON_GRID_TILE_MAX_H).contains(&(h as usize))
            && {
                let ratio = w as f32 / h as f32;
                (0.55..=1.2).contains(&ratio)
            }
        {
            tiles.push((x0, y0, w, h));
        }
    }
    if tiles.len() < PAIMON_GRID_MIN_TILES {
        return None;
    }

    // 宫格结构：按包围盒起点聚列/聚行（容差=中位宽/高的一半），不依赖绝对坐标。
    let mut widths: Vec<i32> = tiles.iter().map(|tile| tile.2).collect();
    let mut heights: Vec<i32> = tiles.iter().map(|tile| tile.3).collect();
    widths.sort_unstable();
    heights.sort_unstable();
    let median_w = widths[widths.len() / 2];
    let median_h = heights[heights.len() / 2];
    let consistent = tiles
        .iter()
        .filter(|tile| (tile.2 - median_w).abs() <= 3 && (tile.3 - median_h).abs() <= 3)
        .count();
    if consistent * 5 < tiles.len() * 4 {
        // 至少 80% 的块尺寸一致，否则不是同一套功能宫格。
        return None;
    }
    let mut xs: Vec<i32> = tiles.iter().map(|tile| tile.0).collect();
    let mut ys: Vec<i32> = tiles.iter().map(|tile| tile.1).collect();
    xs.sort_unstable();
    ys.sort_unstable();
    let cols = cluster_axis_count(&xs, (median_w / 2).max(2));
    let rows = cluster_axis_count(&ys, (median_h / 2).max(2));
    if cols < PAIMON_GRID_MIN_COLS || rows < PAIMON_GRID_MIN_ROWS {
        return None;
    }
    Some((tiles.len(), rows, cols))
}

/// 把一维坐标聚成簇并返回簇数（容差内算同一列/行）。
pub(super) fn cluster_axis_count(sorted: &[i32], tolerance: i32) -> usize {
    let mut count = 0;
    let mut last: Option<i32> = None;
    for &value in sorted {
        match last {
            Some(previous) if (value - previous).abs() <= tolerance => {}
            _ => {
                count += 1;
                last = Some(value);
            }
        }
    }
    count
}
