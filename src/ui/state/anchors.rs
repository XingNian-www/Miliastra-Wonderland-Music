//! 锚点测量：千星地图右下角形状锚点、个人资料页双按钮、OCR 文本后备。

use crate::geometry::Rect;
use crate::geometry::crop_canvas;
use crate::runtime::ocr::OcrPriority;
use crate::runtime::ocr::OcrRuntimeHandle;
use crate::ui::template::TemplateHit;
use anyhow::Result;
use image::DynamicImage;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use super::sampling::{FULL_FRAME_MIN_HEIGHT, FULL_FRAME_MIN_WIDTH};

/// 右下角锚点的**搜索区**（规范画布 1920×1080 坐标）。
///
/// 裁的是「千星奇域·大厅」文字 + 庙宇图标这一段 UI（红色未读角标在 alpha 层清掉，
/// 但模板尺寸/位置保持不变），**不含 UID**，
/// 更不含整条右下角大背景。真实帧 `07-qianxing-map.jpg` 的像素边界实测：
/// 文字 x 1609–1787 / y 991–1014，庙宇图标 x 1807–1875 / y 987–1050，
/// 红色角标 x 1871–1890 / y 980–993；UID 从 y≈1047 才开始，所以模板下沿停在 1052。
pub(super) const QIANXING_MAP_ANCHOR_SEARCH: Rect = Rect::new(1596, 980, 304, 76);
/// 模板尺寸（`assets/qianxing-map-label.png`）：搜索区正中，四周各留 4 px 位移余量。
pub(super) const QIANXING_MAP_ANCHOR_TEMPLATE_WIDTH: u32 = 296;
pub(super) const QIANXING_MAP_ANCHOR_TEMPLATE_HEIGHT: u32 = 68;
/// 模板在搜索区内的左上角偏移（= 规范坐标 x1600 / y984）。
pub(super) const QIANXING_MAP_ANCHOR_INSET: i32 = 4;
/// 位移搜索半径（像素）。HUD 锚点是固定布局，实测 9 帧真实地图全部落在 0 位移。
pub(super) const QIANXING_MAP_ANCHOR_SHIFT: i32 = 4;
/// 二值化亮度阈值（0–255）：文字/图标实心笔画远高于它，深蓝/紫背景远低于它。
pub(super) const QIANXING_MAP_ANCHOR_BRIGHT_MIN: u8 = 150;
/// 形状 F1 阈值。
///
/// 来源（实测空档中点，不是凑的）：2636 张整帧证据里，9 帧真实千星地图
/// **0.9594–1.0000**；其余最高 **0.4489**（提瓦特地图，右下角同样有
/// 「文字 + 圆形图标 + 红点」，是最难的近邻）。空档宽 0.51，阈值取 0.75。
pub(super) const QIANXING_MAP_ANCHOR_SCORE_MIN: f32 = 0.75;

// ---------------------------------------------------------------------------
// 千星退出菜单（只认第一行「退出至登录界面」）
// ---------------------------------------------------------------------------

/// 「退出至登录界面」单行纯文字模板的搜索区（规范画布 1920×1080）。
///
/// 真实帧 `qianxing-ui-59/r12-ctrl-gear.jpg`、`r16-y1000-a.jpg` 上该行均固定
/// 命中 (878,526)，模板 197×29（字形 bbox + 2px 透明 padding）。两帧位置完全
/// 一致，菜单本身是固定叠层，因此搜索区只覆盖该行 ±32px，不再扫全帧：
/// `48×40 @856,512` 的旧讨论尺寸放不下 197px 宽的整行文字，按生产实测改为
/// 现在这块 240×96。
///
/// 只匹配第一行是用户确认的语义：退出菜单第二行「退出至桌面」不参与判据。
pub(super) const QIANXING_EXIT_MENU_SEARCH: Rect = Rect::new(838, 484, 240, 96);
pub(super) const QIANXING_EXIT_MENU_SCALE: u32 = 1;
/// 第一行文字的实测左上角（模板原点）与容差。
pub(super) const QIANXING_EXIT_MENU_ORIGIN: (i32, i32) = (878, 525);
pub(super) const QIANXING_EXIT_MENU_TOLERANCE: i32 = 8;

// ---------------------------------------------------------------------------
// 个人资料页右下角「心愿单」+「衣橱」双按钮判据
// ---------------------------------------------------------------------------

/// 两个按钮的**搜索区**（规范画布 1920×1080 坐标）。
///
/// 真实帧 `Snipaste_2026-11-12_19-51-34.png` 实测按钮像素边界：
/// - 心愿单：x 1464–1655、y 996–1043（192×48）
/// - 衣橱：  x 1680–1871、y 996–1043（192×48）
///
/// 透明化后模板只保留字形（心愿单 66×23、衣橱 45×23，见
/// `pure-text-templates.md`），模板原点变为按钮原点 + 字形裁剪原点：
/// 心愿单 → (1526,1008)，衣橱 → (1753,1008)。搜索区分别围绕这两个实测原点
/// 给 ±32px 位移余量，且模板窗口最低到 y=1036，仍碰不到 y≥1044 的 UID 行。
///
/// 两个按钮分开给区域而不是共用一个 520×130：字形模板很小，分开后每次
/// 只要约 8M 次像素比较（共用大区且要在字形模板上保持分数需要 1× 匹配，
/// 代价约 125M），同时天然避免两个按钮互相串位。
pub(super) const PROFILE_PAGE_WISHLIST_SEARCH: Rect = Rect::new(1480, 976, 140, 60);
pub(super) const PROFILE_PAGE_WARDROBE_SEARCH: Rect = Rect::new(1707, 976, 140, 60);
/// 字形模板很小，直接用 1× 匹配：不做降采样就没有采样相位/量化误差，
/// 正样本能到 1.0，代价也远低于旧版 192×48 模板的 1/2 下采样。
pub(super) const PROFILE_PAGE_BUTTON_SCALE: u32 = 1;
/// 双命中阈值。
///
/// 纯文字化后分数不再被白色胶囊底撑高：
/// - 正样本（真实帧 + 同页 3 帧）实测 **1.0000 / 0.9999**；
/// - 最难的负样本（同位置、同尺寸的「另存为 / 保存」对话框）显著下降，
///   其它页面右下角按钮更不会同时命中两个字形。
/// 阈值仍取 **0.975**（旧版实测空档中点），没有为了新模板降阈值。
pub(super) const PROFILE_PAGE_BUTTON_SCORE_MIN: f32 = 0.975;
/// 两个按钮模板在规范画布上的实测左上角（模板 = 字形 bbox + 2px）。
pub(super) const PROFILE_PAGE_WISHLIST_ORIGIN: (i32, i32) = (1526, 1008);
pub(super) const PROFILE_PAGE_WARDROBE_ORIGIN: (i32, i32) = (1753, 1008);
/// 命中位置容差（像素）。按钮是固定布局；1× 匹配没有量化误差，
/// 容差 8 px 足够吸收缩放取整，又远小于两按钮 216 px 的间距。
///
/// **为什么必须有位置门控**：两个字形模板都很小，单看分数可能被别的
/// 白色胶囊按钮里的近似文字撑高；要求两个模板分别落在各自按钮位置，
/// 才是真正的「两个按钮同时命中」。
pub(super) const PROFILE_PAGE_BUTTON_POSITION_TOLERANCE: i32 = 8;

/// 一次锚点匹配的结果：形状分 + 命中位置（规范画布坐标，已含位移）。
#[derive(Clone, Copy, Debug)]
pub(super) struct QianxingMapAnchorMatch {
    pub(super) score: f32,
    pub(super) x: i32,
    pub(super) y: i32,
}

/// 锚点模板的**二值形状**（缓存；模板缺陷是静态属性，无需每帧重算）。
pub(super) struct AnchorShape {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) bright: Vec<bool>,
    pub(super) bright_count: u32,
}

pub(super) static ANCHOR_SHAPE_CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<AnchorShape>>>> =
    OnceLock::new();

pub(super) fn cached_anchor_shape(path: &Path) -> Result<Arc<AnchorShape>> {
    let cache = ANCHOR_SHAPE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .map_err(|_| anyhow::anyhow!("anchor template cache mutex poisoned"))?;
    if let Some(shape) = cache.get(path) {
        return Ok(Arc::clone(shape));
    }
    let gray = image::open(path)
        .map_err(|error| anyhow::anyhow!("打开锚点模板 {} 失败: {error}", path.display()))?
        .to_luma8();
    let bright: Vec<bool> = gray
        .pixels()
        .map(|pixel| pixel[0] >= QIANXING_MAP_ANCHOR_BRIGHT_MIN)
        .collect();
    let bright_count = bright.iter().filter(|bright| **bright).count() as u32;
    let shape = Arc::new(AnchorShape {
        width: gray.width(),
        height: gray.height(),
        bright,
        bright_count,
    });
    cache.insert(path.to_path_buf(), Arc::clone(&shape));
    Ok(shape)
}

/// 锚点搜索区在当前画面上的位置（按宽高比缩放到规范画布）；画面太小就拒判。
pub(super) fn qianxing_map_anchor_rect(image: &DynamicImage) -> Option<Rect> {
    if image.width() < FULL_FRAME_MIN_WIDTH || image.height() < FULL_FRAME_MIN_HEIGHT {
        return None;
    }
    let scale_x = image.width() as f32 / 1920.0;
    let scale_y = image.height() as f32 / 1080.0;
    Some(Rect::new(
        (QIANXING_MAP_ANCHOR_SEARCH.x as f32 * scale_x).round() as i32,
        (QIANXING_MAP_ANCHOR_SEARCH.y as f32 * scale_y).round() as i32,
        (QIANXING_MAP_ANCHOR_SEARCH.width as f32 * scale_x).round() as u32,
        (QIANXING_MAP_ANCHOR_SEARCH.height as f32 * scale_y).round() as u32,
    ))
}

/// 单个按钮是否「分数达标 **且** 落在自己那个位置上」。
pub(super) fn profile_page_button_hit(
    hit: &TemplateHit,
    origin: (i32, i32),
    score_min: f32,
) -> bool {
    hit.score >= score_min
        && (hit.x - origin.0).abs() <= PROFILE_PAGE_BUTTON_POSITION_TOLERANCE
        && (hit.y - origin.1).abs() <= PROFILE_PAGE_BUTTON_POSITION_TOLERANCE
}

/// 个人资料页两个按钮搜索区在当前画面上的位置（按宽高比缩放到规范画布）；
/// 画面太小就拒判（与千星地图锚点同一护栏）。
pub(super) fn profile_page_button_rects(image: &DynamicImage) -> Option<(Rect, Rect)> {
    if image.width() < FULL_FRAME_MIN_WIDTH || image.height() < FULL_FRAME_MIN_HEIGHT {
        return None;
    }
    let scale_x = image.width() as f32 / 1920.0;
    let scale_y = image.height() as f32 / 1080.0;
    let scale = |rect: Rect| {
        Rect::new(
            (rect.x as f32 * scale_x).round() as i32,
            (rect.y as f32 * scale_y).round() as i32,
            (rect.width as f32 * scale_x).round() as u32,
            (rect.height as f32 * scale_y).round() as u32,
        )
    };
    Some((
        scale(PROFILE_PAGE_WISHLIST_SEARCH),
        scale(PROFILE_PAGE_WARDROBE_SEARCH),
    ))
}

/// 右下角锚点的**形状 F1**（0–1）与命中位置；量不了（画面太小 / 模板尺寸不符）返回 `None`。
///
/// 为什么不是普通灰度 SAD：模板要同时覆盖「深色虚空版」与「亮底版」两种地图，两者
/// 右下角背景亮度差很多，整块灰度 SAD 会被背景拉低；而只比较亮块（masked SAD）又
/// 对“画面里凭空多出一块亮光”没有惩罚——实测千星大厅帧能混到 0.86。这里对
/// **模板亮块与画面亮块做 F1**：少一块（缺字/缺图标）和多一块（背景里有别的亮东西）
/// 都会被惩罚，实测负样本最高只有 0.4489。
///
/// 成本：304×76 的小裁剪 + 81 个位移 × 20128 像素；命中后（≥0.90）直接短路，
/// 单帧远低于 1 ms，不需要降采样，也不需要 OCR。
pub(super) fn qianxing_map_anchor_measure(
    image: &DynamicImage,
    template: &Path,
) -> Result<Option<QianxingMapAnchorMatch>> {
    let Some(rect) = qianxing_map_anchor_rect(image) else {
        return Ok(None);
    };
    let shape = cached_anchor_shape(template)?;
    if shape.width != QIANXING_MAP_ANCHOR_TEMPLATE_WIDTH
        || shape.height != QIANXING_MAP_ANCHOR_TEMPLATE_HEIGHT
    {
        return Ok(None);
    }
    let (width, height) = (
        QIANXING_MAP_ANCHOR_SEARCH.width,
        QIANXING_MAP_ANCHOR_SEARCH.height,
    );
    let crop = crop_canvas(image, rect)?;
    let crop = if crop.width() == width && crop.height() == height {
        crop
    } else {
        crop.resize_exact(width, height, image::imageops::FilterType::Triangle)
    };
    let gray = crop.to_luma8();
    let frame: Vec<bool> = gray
        .pixels()
        .map(|pixel| pixel[0] >= QIANXING_MAP_ANCHOR_BRIGHT_MIN)
        .collect();

    let score_at = |dx: i32, dy: i32| -> f32 {
        let mut intersection = 0_u32;
        let mut frame_bright = 0_u32;
        for ty in 0..shape.height {
            let y = (QIANXING_MAP_ANCHOR_INSET + dy) as u32 + ty;
            let row = (y * width) as usize;
            for tx in 0..shape.width {
                let x = (QIANXING_MAP_ANCHOR_INSET + dx) as u32 + tx;
                if frame[row + x as usize] {
                    frame_bright += 1;
                    if shape.bright[(ty * shape.width + tx) as usize] {
                        intersection += 1;
                    }
                }
            }
        }
        let denominator = shape.bright_count + frame_bright;
        if denominator == 0 {
            0.0
        } else {
            2.0 * intersection as f32 / denominator as f32
        }
    };

    let center = score_at(0, 0);
    let (best_score, best_dx, best_dy) = if center >= QIANXING_MAP_ANCHOR_SCORE_MIN + 0.15 {
        // 命中已经足够强，不必再扫其余 80 个位移（正样本全部走这条）。
        (center, 0, 0)
    } else {
        let mut best = (center, 0, 0);
        for dy in -QIANXING_MAP_ANCHOR_SHIFT..=QIANXING_MAP_ANCHOR_SHIFT {
            for dx in -QIANXING_MAP_ANCHOR_SHIFT..=QIANXING_MAP_ANCHOR_SHIFT {
                if dx == 0 && dy == 0 {
                    continue;
                }
                let score = score_at(dx, dy);
                if score > best.0 {
                    best = (score, dx, dy);
                }
            }
        }
        best
    };
    Ok(Some(QianxingMapAnchorMatch {
        score: best_score,
        x: QIANXING_MAP_ANCHOR_SEARCH.x + QIANXING_MAP_ANCHOR_INSET + best_dx,
        y: QIANXING_MAP_ANCHOR_SEARCH.y + QIANXING_MAP_ANCHOR_INSET + best_dy,
    }))
}

/// 右下角标签的**文本判据**（OCR 可选后备用）。
///
/// 去空白后必须**同时**出现「千星奇域」和「大厅」：单看「千星奇域」会把主菜单、
/// 活动页也算进去；单看「大厅」会命中大厅列表/大厅卡。纯字符串函数，
/// 离线单测直接喂文本，不需要 OCR 模型。
pub(super) fn qianxing_map_label_text_matches(text: &str) -> bool {
    let normalized: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
    normalized.contains("千星奇域") && normalized.contains("大厅")
}

/// 对右下角 ROI 调一次 OCR 再套文本判据（可选后备）。
///
/// 只在回退探针里使用（`probe_game_state`，`Unknown` 且每 `GAME_STATE_PROBE_INTERVAL`
/// 一次），不给常规路径增加 OCR 负担；离线门禁/单测走 `game_entry = None` 分支，
/// 完全不依赖 OCR 也能验证模板路径。
pub(super) fn qianxing_map_label_via_ocr(
    ocr: &OcrRuntimeHandle,
    image: &DynamicImage,
    region: Rect,
) -> Result<bool> {
    let lines = ocr.recognize_lines(crop_canvas(image, region)?, OcrPriority::UiConfirmation)?;
    Ok(lines
        .iter()
        .any(|line| qianxing_map_label_text_matches(&line.text)))
}
