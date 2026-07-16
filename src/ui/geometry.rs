use anyhow::{Result, bail};
use image::DynamicImage;
use serde::Serialize;

use crate::config::RectConfig;

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct Point {
    pub(crate) x: i32,
    pub(crate) y: i32,
}

impl Point {
    pub(crate) const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
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
        self.x + self.width as i32
    }

    pub(crate) fn bottom(self) -> i32 {
        self.y + self.height as i32
    }

    pub(crate) fn center(self) -> Point {
        Point::new(
            self.x + self.width as i32 / 2,
            self.y + self.height as i32 / 2,
        )
    }
}

impl From<RectConfig> for Rect {
    fn from(value: RectConfig) -> Self {
        Self::new(value.x, value.y, value.width, value.height)
    }
}

pub(crate) fn crop_canvas(image: &DynamicImage, rect: Rect) -> Result<DynamicImage> {
    if rect.x < 0
        || rect.y < 0
        || rect.right() > image.width() as i32
        || rect.bottom() > image.height() as i32
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

pub(crate) fn clamp_i32(value: i32, min: i32, max: i32) -> i32 {
    value.max(min).min(max)
}
