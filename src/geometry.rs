use anyhow::{Result, bail};
use image::DynamicImage;
use serde::Serialize;

use crate::config::RectConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Point {
    pub(crate) x: i32,
    pub(crate) y: i32,
}

impl Point {
    pub(crate) const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Rect {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl Rect {
    pub(crate) const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub(crate) fn right(self) -> i32 {
        clamp_i32(i64::from(self.x) + i64::from(self.width))
    }

    pub(crate) fn bottom(self) -> i32 {
        clamp_i32(i64::from(self.y) + i64::from(self.height))
    }

    pub(crate) fn center(self) -> Point {
        Point::new(
            clamp_i32(i64::from(self.x) + i64::from(self.width) / 2),
            clamp_i32(i64::from(self.y) + i64::from(self.height) / 2),
        )
    }
}

/// 把 i64 的几何结果钳进 `i32` 区间。
///
/// `width`/`height` 是 `u32`，直接 `as i32` 后做有符号加法会在 Debug 下 panic、
/// Release 下回绕成负数；而下游多处用 `rect.right() > image.width() as i32`
/// 做越界校验，回绕出的负数会让越界矩形被当成画布内。
fn clamp_i32(value: i64) -> i32 {
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

impl From<RectConfig> for Rect {
    fn from(value: RectConfig) -> Self {
        Self::new(value.x, value.y, value.width, value.height)
    }
}

/// 把 rect 裁剪进画布范围(坐标下限 0、宽高收窄),防止越界/溢出输入。
///
/// 负起点按**裁剪**处理：落在画布左/上侧之外的部分要同时从宽高里扣掉。
/// 只把起点挪到 0（旧实现）等于把整块区域平移进画面：`x=-200,width=50`
/// 会变成画布内 `(0,0,50,h)` 的非空区域，于是裁剪出一片用户从未指定的画面。
pub(crate) fn clamp_rect(rect: Rect, canvas_width: u32, canvas_height: u32) -> Rect {
    // 画布尺寸同样进 i64：`canvas_width as i32` 在超过 `i32::MAX` 时为负，
    // 而 `clamp(0, 负数)` 会直接 panic。
    let canvas_width = i64::from(canvas_width).min(i64::from(i32::MAX));
    let canvas_height = i64::from(canvas_height).min(i64::from(i32::MAX));
    let x0 = i64::from(rect.x).clamp(0, canvas_width);
    let y0 = i64::from(rect.y).clamp(0, canvas_height);
    let x1 = (i64::from(rect.x) + i64::from(rect.width)).clamp(x0, canvas_width);
    let y1 = (i64::from(rect.y) + i64::from(rect.height)).clamp(y0, canvas_height);
    Rect::new(x0 as i32, y0 as i32, (x1 - x0) as u32, (y1 - y0) as u32)
}

pub(crate) fn crop_canvas(image: &DynamicImage, rect: Rect) -> Result<DynamicImage> {
    // 用 i64 做边界运算,防止 x+width 溢出回绕(负数)绕过越界检查。
    let right = i64::from(rect.x) + i64::from(rect.width);
    let bottom = i64::from(rect.y) + i64::from(rect.height);
    if rect.x < 0
        || rect.y < 0
        || right > i64::from(image.width())
        || bottom > i64::from(image.height())
    {
        bail!(
            "crop rect {},{},{},{} outside image {}x{}",
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            image.width(),
            image.height()
        );
    }
    Ok(image.crop_imm(rect.x as u32, rect.y as u32, rect.width, rect.height))
}

pub(crate) fn parse_rect(value: &str) -> Result<Rect> {
    let parts = value
        .split(',')
        .map(str::trim)
        .map(str::parse::<i32>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if parts.len() != 4 {
        bail!("rect must be x,y,width,height");
    }
    if parts[2] <= 0 || parts[3] <= 0 {
        bail!("rect width and height must be positive");
    }
    Ok(Rect::new(
        parts[0],
        parts[1],
        parts[2] as u32,
        parts[3] as u32,
    ))
}

/// 一次模板匹配命中的结果。
///
/// 只含几何与分数，没有任何匹配算法或图像资源，因此放在几何叶子里：
/// `runtime` 的证据 DTO 用它作字段类型，arg 若留在 `ui::template`（那里有 wgpu），
/// 会造成 runtime 反向依赖整个模板匹配器。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct TemplateHit {
    pub(crate) kind: String,
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) score: f32,
    /// 命中位置是否通过文字内容质量校验；非文字模板恒为 true。
    /// 不进入序列化：证据 JSON 仍只暴露几何与分数，避免改变对外协议。
    #[serde(skip)]
    pub(crate) quality_ok: bool,
}

impl TemplateHit {
    pub(crate) fn rect(&self) -> Rect {
        Rect::new(self.x, self.y, self.width, self.height)
    }

    pub(crate) fn center(&self) -> Point {
        self.rect().center()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_edges_saturate_instead_of_wrapping_on_out_of_range_sizes() {
        // `x + width` 超过 i32::MAX：Debug 下旧实现会 panic，Release 下回绕成负数。
        let overflowing = Rect::new(i32::MAX, i32::MAX, 2, 2);
        assert_eq!(overflowing.right(), i32::MAX);
        assert_eq!(overflowing.bottom(), i32::MAX);

        // `width as i32` 本身回绕：旧实现 right() 变成 -1，下游
        // `rect.right() > image.width() as i32` 的越界检查因此失效。
        let huge = Rect::new(0, 0, u32::MAX, u32::MAX);
        assert_eq!(huge.right(), i32::MAX);
        assert_eq!(huge.bottom(), i32::MAX);
        assert_eq!(huge.center(), Point::new(i32::MAX, i32::MAX));
    }

    #[test]
    fn clamp_rect_crops_the_out_of_canvas_part_instead_of_shifting_the_rect() {
        // 完全落在画布左/上侧之外：裁剪结果必须为空，不能被平移成 (0,0,50,40)。
        assert_eq!(
            clamp_rect(Rect::new(-200, -100, 50, 40), 1_920, 1_080),
            Rect::new(0, 0, 0, 0)
        );

        // 跨越左/上边界：只保留画布内可见部分（300 里可见 200，200 里可见 150）。
        assert_eq!(
            clamp_rect(Rect::new(-100, -50, 300, 200), 1_920, 1_080),
            Rect::new(0, 0, 200, 150)
        );

        // 跨越右/下边界：宽高按画布收窄。
        assert_eq!(
            clamp_rect(Rect::new(700, 500, 300, 300), 800, 600),
            Rect::new(700, 500, 100, 100)
        );

        // 1080p 标定矩形落在 720p 画布上：高度收窄到画布下边界。
        assert_eq!(
            clamp_rect(Rect::new(390, 87, 790, 870), 1_280, 720),
            Rect::new(390, 87, 790, 633)
        );
    }

    #[test]
    fn clamp_rect_tolerates_a_canvas_larger_than_i32() {
        // `canvas_width as i32` 为负时，旧实现在 `.clamp(0, 负数)` 直接 panic。
        assert_eq!(
            clamp_rect(Rect::new(10, 10, 10, 10), u32::MAX, u32::MAX),
            Rect::new(10, 10, 10, 10)
        );
    }
}
