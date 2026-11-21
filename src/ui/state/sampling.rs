//! 整屏稀疏采样原语与色调统计（加载界面 / 千星覆盖页 / 米黄面板）。

use image::DynamicImage;

/// 统计指定区域里"近白"像素的占比（每 2 像素取样一次，与定阈值时的量法一致）。
///
/// 千星大厅底栏有多个白色圆角按键（Enter / O / Alt+N / N），提瓦特底栏只有一个 Enter，
/// 所以这个占比能把"提瓦特的世界聊天气泡"和"千星聊天标记"区分开。
/// 与模板匹配不同，它**对位移和缩放都不敏感**——第 47 节就是栽在 1:1 灰度 SAD 上的。
/// 整屏色调统计的取样步长：每 20 像素取一点（1920x1080 下约 5.2k 个样本）。
/// **必须与定阈值时的量法一致**，改步长要重新量阈值。
pub(super) const SCREEN_TONE_SAMPLE_STEP: u32 = 20;
/// 记一个"暗点"的灰度上限。
pub(super) const SCREEN_TONE_DARK_MAX: f32 = 40.0;
/// 记一个"近白点"的通道下限：min(R,G,B) 不小于它才算近白。
pub(super) const SCREEN_TONE_WHITE_MIN: u8 = 200;

/// 黑底徽记型加载界面的**暗像素占比下限**。
///
/// 来源（实测区间中点，不是凑的）：加载帧 **98.46~98.53%**，
/// 而语料里**任何真实画面**最高只有 **26.00%**（活动面板），
/// 两者之间没有任何一帧 ⇒ 取 (26.00 + 98.46) / 2 = **62.23%**。
pub(super) const LOADING_DARK_RATIO_MIN: f32 = 0.6223;

/// 白底元素型加载界面的**近白占比下限**。
///
/// 来源：加载帧 **99.77~100%**，任何真实画面最高 **43.31%**（登录界面）
/// ⇒ 取 (43.31 + 99.77) / 2 = **71.54%**。
pub(super) const LOADING_WHITE_RATIO_MIN: f32 = 0.7154;

/// 加载界面的**色彩一致度下限**：主色桶样本占比。
///
/// 实测：黑底加载 97.67~98.19%、白底加载 99.69~100%，
/// 而暗色大厅虽然暗像素占比可达 75.23%，颜色仍分散（主色桶仅 38.56%）。
/// 要求一致性同时达标，避免把“暗场景”误判成加载界面。
pub(super) const LOADING_COLOR_CONSISTENCY_MIN: f32 = 0.90;

/// 整屏色调判据适用的最小画面尺寸。
/// 本项目所有实机截图都是 1920x1080；而证据目录里还有大量小裁剪图
/// （按钮、色条等，例如 72x42），它们整屏占比没有意义，不参与这条判据。
pub(super) const FULL_FRAME_MIN_WIDTH: u32 = 1280;
pub(super) const FULL_FRAME_MIN_HEIGHT: u32 = 720;

/// 稀疏采样探针共用的 RGB 取值器：RGB8/RGBA8 **直接取像素、不做整帧转换**；
/// 其余色彩类型回退到一次性转换。
///
/// 整屏统计类判据（色调、青色带、千星地图色调）只采样几千个点，而原来的
/// `to_rgb8()` 每次都要拷 200 万像素——那是这些判据里最贵的部分，且每帧重复多次。
pub(super) enum RgbSampler<'a> {
    Borrowed(&'a image::RgbImage),
    Rgba(&'a image::RgbaImage),
    Owned(image::RgbImage),
}

impl RgbSampler<'_> {
    pub(super) fn new(image: &DynamicImage) -> RgbSampler<'_> {
        match image {
            DynamicImage::ImageRgb8(rgb) => RgbSampler::Borrowed(rgb),
            DynamicImage::ImageRgba8(rgba) => RgbSampler::Rgba(rgba),
            other => RgbSampler::Owned(other.to_rgb8()),
        }
    }

    pub(super) fn dimensions(&self) -> (u32, u32) {
        match self {
            RgbSampler::Borrowed(rgb) => rgb.dimensions(),
            RgbSampler::Rgba(rgba) => rgba.dimensions(),
            RgbSampler::Owned(rgb) => rgb.dimensions(),
        }
    }

    pub(super) fn pixel(&self, x: u32, y: u32) -> [u8; 3] {
        match self {
            RgbSampler::Borrowed(rgb) => {
                let pixel = rgb.get_pixel(x, y);
                [pixel[0], pixel[1], pixel[2]]
            }
            RgbSampler::Rgba(rgba) => {
                let pixel = rgba.get_pixel(x, y);
                [pixel[0], pixel[1], pixel[2]]
            }
            RgbSampler::Owned(rgb) => {
                let pixel = rgb.get_pixel(x, y);
                [pixel[0], pixel[1], pixel[2]]
            }
        }
    }
}

/// 整屏统计，返回 (暗像素占比, 近白像素占比, 主色桶占比)。
///
/// 主色桶占比衡量**色彩一致性**：把 RGB 各通道量化到 16 级后取最大桶的样本占比。
/// 加载界面几乎整屏同一色调（黑底 ≈97.7~98.1%、白底 ≈99.7%），
/// 而暗色大厅虽然暗像素可达 75%，颜色仍分散（主色桶仅 ≈38.6%）。
/// 所以“暗/白占比”必须与“主色桶占比”同时达标，才能判加载界面。
pub(super) fn screen_tone(image: &DynamicImage) -> (f32, f32, f32) {
    let sampler = RgbSampler::new(image);
    let (width, height) = sampler.dimensions();
    let mut total = 0_u32;
    let mut dark = 0_u32;
    let mut white = 0_u32;
    let mut buckets = [0_u32; 4096];
    let mut dominant = 0_u32;
    let mut y = 0;
    while y < height {
        let mut x = 0;
        while x < width {
            let pixel = sampler.pixel(x, y);
            let luma = 0.299 * f32::from(pixel[0])
                + 0.587 * f32::from(pixel[1])
                + 0.114 * f32::from(pixel[2]);
            if luma < SCREEN_TONE_DARK_MAX {
                dark += 1;
            }
            if pixel[0].min(pixel[1]).min(pixel[2]) >= SCREEN_TONE_WHITE_MIN {
                white += 1;
            }
            let bucket = (usize::from(pixel[0] >> 4) << 8)
                | (usize::from(pixel[1] >> 4) << 4)
                | usize::from(pixel[2] >> 4);
            buckets[bucket] += 1;
            dominant = dominant.max(buckets[bucket]);
            total += 1;
            x += SCREEN_TONE_SAMPLE_STEP;
        }
        y += SCREEN_TONE_SAMPLE_STEP;
    }
    if total == 0 {
        return (0.0, 0.0, 0.0);
    }
    (
        dark as f32 / total as f32,
        white as f32 / total as f32,
        dominant as f32 / total as f32,
    )
}

/// 判断画面是否为加载界面；是则返回证据里的规则名（区分黑底/白底两种）。
pub(super) fn loading_rule(image: &DynamicImage) -> Option<&'static str> {
    // 这条判据是为**整屏截图**定义的：加载界面是整屏极端色调。
    // 对裁剪出来的小图（比如按钮、色条）整屏占比没有意义，直接不判，避免误报。
    let (width, height) = (image.width(), image.height());
    if width < FULL_FRAME_MIN_WIDTH || height < FULL_FRAME_MIN_HEIGHT {
        return None;
    }
    let (dark_ratio, white_ratio, dominant_ratio) = screen_tone(image);
    // **亮底加载变体**（例：cold-to-qianxing-59/001-t000.jpg：整屏亮天空+白塔，
    // 色彩并不统一，dominant 只有 3%）。特征是：几乎无暗像素、亮像素占比不低、
    // **画面正中有一个深色 spinner 方块**。只在整帧上判，避免裁图误报。
    // 实测：该帧 dark=0.000 white=0.283 center_dark=0.425；
    // 负样本：千星大厅帧 center_dark≤0.29 或 dark>0.02，round78 dark=0.09。
    if dark_ratio < 0.02 && white_ratio >= 0.12 && dominant_ratio < 0.5 {
        let (center_dark, center480) = loading_center_dark_ratios(image);
        // spinner 是**中心一小块**深色方块：中心 160 暗占比高，而中心 480 仍然很亮。
        // 菜单/面板的中心是大片深色内容（center480 0.2–0.55），会被这条挡掉。
        if center_dark >= 0.35 && center480 < 0.10 {
            return Some("loading_bright_screen");
        }
    }
    // 色彩一致性是加载界面的强特征：加载画面几乎整屏一色。
    // 暗色大厅也可能有 75% 暗像素，但颜色分散，不能仅凭暗占比判加载。
    if dominant_ratio < LOADING_COLOR_CONSISTENCY_MIN {
        return None;
    }
    if dark_ratio >= LOADING_DARK_RATIO_MIN {
        return Some("loading_dark_screen");
    }
    if white_ratio >= LOADING_WHITE_RATIO_MIN {
        return Some("loading_white_screen");
    }
    None
}

/// 加载界面正中的 spinner：返回 `(中心 160×160 暗占比, 中心 480×480 暗占比)`（步长 2）。
pub(super) fn loading_center_dark_ratios(image: &DynamicImage) -> (f32, f32) {
    let sampler = RgbSampler::new(image);
    let (width, height) = sampler.dimensions();
    let cx0 = (width / 2).saturating_sub(80);
    let cy0 = (height / 2).saturating_sub(80);
    let (mut dark, mut total) = (0_u32, 0_u32);
    let mut y = cy0;
    while y < (cy0 + 160).min(height) {
        let mut x = cx0;
        while x < (cx0 + 160).min(width) {
            let pixel = sampler.pixel(x, y);
            let luma = 0.299 * f32::from(pixel[0])
                + 0.587 * f32::from(pixel[1])
                + 0.114 * f32::from(pixel[2]);
            if luma < 90.0 {
                dark += 1;
            }
            total += 1;
            x += 2;
        }
        y += 2;
    }
    let (mut dark480, mut total480) = (0_u32, 0_u32);
    let big_x0 = (width / 2).saturating_sub(240);
    let big_y0 = (height / 2).saturating_sub(240);
    let mut y = big_y0;
    while y < (big_y0 + 480).min(height) {
        let mut x = big_x0;
        while x < (big_x0 + 480).min(width) {
            let pixel = sampler.pixel(x, y);
            let luma = 0.299 * f32::from(pixel[0])
                + 0.587 * f32::from(pixel[1])
                + 0.114 * f32::from(pixel[2]);
            if luma < 90.0 {
                dark480 += 1;
            }
            total480 += 1;
            x += 2;
        }
        y += 2;
    }
    (
        dark as f32 / total.max(1) as f32,
        dark480 as f32 / total480.max(1) as f32,
    )
}

/// 右上关闭按钮白色占比（千星页的关闭按钮比提瓦特面板更大更白）。
pub(super) fn overlay_close_white_fraction(image: &DynamicImage) -> f32 {
    let sampler = RgbSampler::new(image);
    let (width, height) = sampler.dimensions();
    if width < FULL_FRAME_MIN_WIDTH || height < FULL_FRAME_MIN_HEIGHT {
        return 0.0;
    }
    let x1 = width.saturating_sub(20);
    let x0 = x1.saturating_sub(80);
    let (mut white, mut total) = (0_u32, 0_u32);
    let mut y = 10;
    while y < 70.min(height) {
        let mut x = x0;
        while x < x1 {
            let pixel = sampler.pixel(x, y);
            if pixel[0].min(pixel[1]).min(pixel[2]) >= 220 {
                white += 1;
            }
            total += 1;
            x += 2;
        }
        y += 2;
    }
    white as f32 / total.max(1) as f32
}

/// 页面"米黄面板占比"：提瓦特面板多为米黄大面板，千星页多为深蓝/星空。
pub(super) fn beige_panel_fraction(image: &DynamicImage) -> f32 {
    let sampler = RgbSampler::new(image);
    let (width, height) = sampler.dimensions();
    let (mut beige, mut total) = (0_u32, 0_u32);
    let mut y = 0;
    while y < height {
        let mut x = 0;
        while x < width {
            let pixel = sampler.pixel(x, y);
            let (r, g, b) = (pixel[0] as i32, pixel[1] as i32, pixel[2] as i32);
            if r > 195 && g > 185 && b > 165 && (r - b).abs() < 45 {
                beige += 1;
            }
            total += 1;
            x += 8;
        }
        y += 8;
    }
    beige as f32 / total.max(1) as f32
}

/// 千星全屏覆盖页（商城 / 装扮搭配 / 更换角色 / 颂愿 / 纪游 / 我的奇域 / 档案卡 ...）
/// 的**结构性**判据：右上角有一个白色圆形关闭按钮（深色 X 在白色圆内）。
///
/// 这些页面各自内容不同，但都是"整屏深色页 + 右上关闭"。用**关闭按钮白色占比**
/// （右上角 100×60 区域，近白像素占比）判定，不依赖任何单个页面的文案/图标。
///
/// 实测（1920×1080）：覆盖页 0.374–0.377；负样本最高 0.261（一张加载帧，
/// 且加载判据更早命中）；千星大厅 0.128、二级聊天 0.130、千星地图 0.030。
pub(super) fn is_qianxing_overlay_close(image: &DynamicImage) -> bool {
    let sampler = RgbSampler::new(image);
    let (width, height) = sampler.dimensions();
    if width < FULL_FRAME_MIN_WIDTH || height < FULL_FRAME_MIN_HEIGHT {
        return false;
    }
    // 纯白/纯黑/无蓝色调的测试合成帧不算覆盖页（避免误伤空画面测试）。
    let mut total_blue = 0_u32;
    let mut total_samples = 0_u32;
    let mut yy = 0;
    while yy < height {
        let mut xx = 0;
        while xx < width {
            let pixel = sampler.pixel(xx, yy);
            if (pixel[2] as i32) > (pixel[0] as i32) + 15
                && pixel[2] > 60
                && pixel[0] < 120
                && pixel[1] < 160
            {
                total_blue += 1;
            }
            total_samples += 1;
            xx += 16;
        }
        yy += 16;
    }
    if total_samples == 0 || total_blue as f32 / (total_samples as f32) < 0.20 {
        return false;
    }
    let x1 = width.saturating_sub(20);
    let x0 = x1.saturating_sub(80);
    let (mut white, mut total) = (0_u32, 0_u32);
    let mut y = 10;
    while y < 70.min(height) {
        let mut x = x0;
        while x < x1 {
            let pixel = sampler.pixel(x, y);
            if pixel[0].min(pixel[1]).min(pixel[2]) >= 220 {
                white += 1;
            }
            total += 1;
            x += 2;
        }
        y += 2;
    }
    // 阈值 0.35：真实千星覆盖页右上白色关闭按钮占比 0.374–0.377；
    // 大厅卡/主菜单（含返回提瓦特胶囊）只有 0.31 左右，用它避免把大厅卡抢成覆盖页。
    total > 0 && white as f32 / (total as f32) >= 0.35
}
