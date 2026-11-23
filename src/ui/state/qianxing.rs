//! F2 大厅多人面板的冻结描述子。
//!
//! 千星地图的**判据**在 `classifier.rs` 里用右下角锚点模板做（见那里的注释）；
//! 这里原先还有一层「整屏暗青占比」后备判据（`qianxing_map_tone` /
//! `is_qianxing_map` / `is_bright_qianxing_map` 及 7 个阈值常量），已移除。

use image::DynamicImage;

use super::sampling::{FULL_FRAME_MIN_HEIGHT, FULL_FRAME_MIN_WIDTH, RgbSampler};

/// 千星奇域「大厅多人面板」（F2）的冻结参考。
///
/// 生成脚本 `dist/qianxingdata/mkf2ref.py`，格式：
/// magic("MWF2") + version(u32) + width(u32) + height(u32) + x(i32) + y(i32)
/// + 归一化参考(i16 x N)，N = width*height*3，归一化值 x1000。
///
/// **区域为什么取"底栏左半"**：第 60 节把三帧 F2 面板（玩家列表各不相同）与 30 帧
/// 其他界面放在一起做区域候选实验，5 个候选都分得很开；选它是因为它是**纯界面骨架**
///（「大厅列表」「新建大厅」两个按钮所在的那条底栏），既不依赖玩家内容、
/// 也不依赖大厅名字与人数，最不容易被"换了个人在大厅"带偏。
///
/// 区域用**格索引**（120x68 网格，每格 16x16 像素），不是像素坐标：
/// 第 (cx,cy) 格的采样点是 `(cx*16+8, cy*16+8)`。这个口径与做实验时完全一致。
pub(super) const QIANXING_PANEL_REF: &[u8] = include_bytes!("../../../assets/qianxing-f2-ref.bin");
pub(super) const QIANXING_PANEL_REF_MAGIC: &[u8] = b"MWF2";
pub(super) const QIANXING_PANEL_REF_VERSION: u32 = 1;
pub(super) const QIANXING_PANEL_REF_SCALE: f32 = 1000.0;
pub(super) const QIANXING_PANEL_REF_HEADER: usize = 24;
pub(super) const QIANXING_PANEL_CELL: u32 = 16;
pub(super) const QIANXING_PANEL_CELL_OFFSET: u32 = 8;

/// 判为千星大厅多人面板的**距离上限**。
///
/// 来源（实测的对数中点，不是凑的）：第 60 节用 3 帧真实 F2 面板与 30 帧其他界面量——
/// 正样本最大距离 **0.0460**，负样本最小距离 **1.0982**，空档 **23.9 倍**，
/// 对数中点 sqrt(0.0460 x 1.0982) ≈ 0.2248，取 **0.22**。
pub(super) const QIANXING_PANEL_MAX: f32 = 0.22;

/// 比距离时允许的像素平移范围。
///
/// 取 **2** 有两个依据：
/// 1. 世界判据第 57 节的实测——不允许平移时 ±1 像素就翻判断，加 ±2 搜索后 ±1/±2 都不再翻；
/// 2. 第 61 节对本判据的扰动实测——不做搜索时位移 ±3 距离 0.1969、几乎贴住阈值 0.22，
///    加 ±2 搜索后最优对齐只剩 1 像素残差，距离掉回 ≈±1 的水平。
pub(super) const QIANXING_PANEL_SEARCH: i32 = 2;

/// F2 大厅页两个底部按钮锚点的最低分。
///
/// 实测（**1/2 下采样**、真实帧集，模板去掉白色胶囊填充后）：
/// - 正样本「大厅列表」0.9582、「邀请玩家」0.9684–0.9688；
/// - 负样本最高 0.8569（主菜单/大厅），大厅/二级聊天/覆盖层/地图/加载均更低。
/// 阈值仍取 0.95：落在两侧空档内；两个锚点**同时**命中才判 F2。
///
/// 为什么从 1/4 改成 1/2：模板只留图标+文字后，1/4 下采样把笔画糊成几个像素，
/// 正样本掉到 0.9476–0.9491（低于 0.95），负样本仍在 0.86–0.89；1/2 下采样
/// 既恢复正样本余量，又保持两侧 ~0.10 的空档。区域只有 600×95 / 140×54，
/// 1/2 的总代价仍远小于整帧千星卡模板。
pub(super) const QIANXING_F2_BUTTON_MIN: f32 = 0.95;
/// 按钮锚点的降采样倍数：区域小、模板只含图标+文字，1/2 后正样本 >0.95 且余量最大。
pub(super) const QIANXING_F2_BUTTON_SCALE: u32 = 2;

/// 解析冻结参考，返回 `(x, y, width, height)` 与**参考本体的字节切片**。
///
/// 参考本体按小端 i16 存。这里**不解码成 `&[i16]`**（那要么 `unsafe`、要么多一次分配），
/// 而是在算距离时逐个解码——480 个格点、1440 个值，开销可以忽略，也省掉一块 `unsafe`。
pub(super) fn parse_qianxing_panel_ref() -> Option<(u32, u32, u32, u32, &'static [u8])> {
    let bytes = QIANXING_PANEL_REF;
    if bytes.len() < QIANXING_PANEL_REF_HEADER || &bytes[0..4] != QIANXING_PANEL_REF_MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    if version != QIANXING_PANEL_REF_VERSION {
        return None;
    }
    let width = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
    let height = u32::from_le_bytes(bytes[12..16].try_into().ok()?);
    let x = i32::from_le_bytes(bytes[16..20].try_into().ok()?);
    let y = i32::from_le_bytes(bytes[20..24].try_into().ok()?);
    if x < 0 || y < 0 || width == 0 || height == 0 {
        return None;
    }
    let values = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(3)?;
    let body = &bytes[QIANXING_PANEL_REF_HEADER..];
    if body.len() != values * 2 {
        return None;
    }
    Some((x as u32, y as u32, width, height, body))
}

/// 按格采样并归一化：减通道均值，再除平均绝对偏差。
///
/// 与参考用的是**同一套归一化**（世界判据也是这一套），所以对整体明暗变化不敏感，
/// 只比"空间花纹"。平均绝对偏差太小时返回 `None`（纯色块没有花纹可比，应当拒判）。
pub(super) fn qianxing_panel_sample(
    sampler: &RgbSampler<'_>,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    offset_x: i32,
    offset_y: i32,
) -> Option<Vec<f32>> {
    let (image_width, image_height) = sampler.dimensions();
    if image_width < FULL_FRAME_MIN_WIDTH || image_height < FULL_FRAME_MIN_HEIGHT {
        return None;
    }
    let mut raw = Vec::with_capacity((width * height * 3) as usize);
    for cell_y in y..y + height {
        for cell_x in x..x + width {
            // **必须钳位**：最后一行格心是 `67*16+8 = 1080`，而 1080 行画面的有效行是 0..1079。
            // 定阈值时的 dump 脚本做了同样的钳位（`if y >= height { y = height-1 }`），
            // 两边口径必须一致，否则参考与实际采样对不上——第一版漏了钳位，
            // 于是正样本直接"量不出来"，被分离度用例当场抓住。
            // 平移后的采样点先**边界检查**：越界说明这个偏移量在这个画面上算不了，
            // 直接放弃该偏移（上层会去试别的偏移量），**不做钳位**——
            // 钳位会把边缘一整行压到同一像素上，凭空制造出一段假花纹。
            let px = (cell_x * QIANXING_PANEL_CELL + QIANXING_PANEL_CELL_OFFSET) as i32 + offset_x;
            let py = (cell_y * QIANXING_PANEL_CELL + QIANXING_PANEL_CELL_OFFSET) as i32 + offset_y;
            if px < 0 || py < 0 || px >= image_width as i32 || py >= image_height as i32 {
                return None;
            }
            let pixel = sampler.pixel(px as u32, py as u32);
            raw.push(f32::from(pixel[0]));
            raw.push(f32::from(pixel[1]));
            raw.push(f32::from(pixel[2]));
        }
    }
    if raw.is_empty() {
        return None;
    }
    let mean = raw.iter().sum::<f32>() / raw.len() as f32;
    for value in raw.iter_mut() {
        *value -= mean;
    }
    let deviation = raw.iter().map(|value| value.abs()).sum::<f32>() / raw.len() as f32;
    if deviation < 1e-6 {
        return None;
    }
    for value in raw.iter_mut() {
        *value /= deviation;
    }
    Some(raw)
}

/// 画面是不是**千星奇域的大厅多人面板**（在大厅里按 F2 打开的那个）。
///
/// 判法：底栏左半的归一化描述子与冻结参考的**逐通道平均绝对差**不超过阈值。
/// 量不了（画面太小 / 纯色）就返回 `false`，交给上层的其它判据或拒判。
pub(super) fn is_qianxing_player_panel(image: &DynamicImage) -> bool {
    qianxing_player_panel_distance(image).is_some_and(|distance| distance <= QIANXING_PANEL_MAX)
}

/// 与冻结参考的逐通道平均绝对差；`None` 表示这次量不了（画面太小 / 纯色 / 参考损坏）。
///
/// 单独暴露出来，是为了让分离度自证用例能断言**两侧区间不重叠**，而不是只看布尔结论——
/// 只看布尔就退化成了"调阈值凑已观测结果"。
pub(super) fn qianxing_player_panel_distance(image: &DynamicImage) -> Option<f32> {
    // **允许 ±2 像素平移，取 25 个偏移里最小的那个距离。**
    //
    // 这不是可选项：第 61 节的扰动实测里，不做搜索时位移 ±3 的距离是 **0.1969**，
    // 而阈值是 **0.22**——只有 10% 余量，等于在悬崖边上。
    // 项目里世界判据早就踩过同一个坑（第 57 节："不允许平移时 ±1 像素就翻判断"），
    // 修法就是给它加 ±2 搜索。这里照做：位移 ±3 时最优对齐只剩 1 像素残差，
    // 距离会掉回 ≈±1 的水平（约 0.10），余量从 10% 变成一倍以上。
    //
    // **这是原理性修法，不是调阈值**：阈值 0.22 是实验量出来的空档中点，一个字没动。
    let (x, y, width, height, reference) = parse_qianxing_panel_ref()?;
    // **整帧不做任何转换**：此前每个偏移各自 `to_rgb8()` 一次（25 个偏移 = 25 次全帧拷贝），
    // 改用共享取值器后 RGBA 帧零转换，只做逐格点采样。
    let sampler = RgbSampler::new(image);
    let mut best: Option<f32> = None;
    for offset_y in -QIANXING_PANEL_SEARCH..=QIANXING_PANEL_SEARCH {
        for offset_x in -QIANXING_PANEL_SEARCH..=QIANXING_PANEL_SEARCH {
            let Some(distance) = qianxing_panel_distance_at(
                &sampler, reference, x, y, width, height, offset_x, offset_y,
            ) else {
                continue;
            };
            best = Some(best.map_or(distance, |current: f32| current.min(distance)));
        }
    }
    best
}

/// 在给定平移量下与参考比距离。采样点越界时返回 `None`（该偏移算不了）。
#[allow(clippy::too_many_arguments)]
pub(super) fn qianxing_panel_distance_at(
    sampler: &RgbSampler<'_>,
    reference: &[u8],
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    offset_x: i32,
    offset_y: i32,
) -> Option<f32> {
    let sample = qianxing_panel_sample(sampler, x, y, width, height, offset_x, offset_y)?;
    if sample.len() * 2 != reference.len() {
        return None;
    }
    let mut total = 0.0_f32;
    for (index, value) in sample.iter().enumerate() {
        let raw = i16::from_le_bytes([reference[index * 2], reference[index * 2 + 1]]);
        total += (*value - f32::from(raw) / QIANXING_PANEL_REF_SCALE).abs();
    }
    Some(total / sample.len() as f32)
}
