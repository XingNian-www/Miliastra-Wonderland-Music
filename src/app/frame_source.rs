use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use image::DynamicImage;
use image::GenericImageView;
use image::imageops::FilterType;

use super::FrameArgs;
use super::game_ui::GameUi;
use crate::runtime::ui::CapturedFrame;

#[derive(Clone, Debug)]
pub(super) struct Canvas {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) resize: bool,
}

#[derive(Debug)]
pub(super) struct Frame {
    pub(super) image: Arc<DynamicImage>,
    pub(super) captured_at: Instant,
}

pub(super) fn load_frame(args: &FrameArgs, canvas: &Canvas, game_ui: &GameUi) -> Result<Frame> {
    let started = Instant::now();
    let image = Arc::new(match &args.image {
        Some(path) => {
            image::open(path).with_context(|| format!("open image {}", path.display()))?
        }
        None => game_ui.capture()?,
    });
    let captured_at = Instant::now();
    let image = normalize_frame(image, canvas, started);
    Ok(Frame { image, captured_at })
}

pub(super) fn from_captured_frame(frame: &CapturedFrame, canvas: &Canvas) -> Frame {
    let started = Instant::now();
    Frame {
        image: normalize_frame(frame.image_arc(), canvas, started),
        captured_at: frame.captured_at(),
    }
}

fn normalize_frame(
    image: Arc<DynamicImage>,
    canvas: &Canvas,
    started: Instant,
) -> Arc<DynamicImage> {
    let (source_width, source_height) = image.dimensions();
    let image = if canvas.resize && (source_width != canvas.width || source_height != canvas.height)
    {
        Arc::new(image.resize_exact(canvas.width, canvas.height, FilterType::Triangle))
    } else {
        image
    };
    log::info!(target: "timing",
        "截图加载耗时: {}ms source={}x{} output={}x{} resize={}",
        elapsed_ms(started),
        source_width,
        source_height,
        image.width(),
        image.height(),
        canvas.resize && (source_width != canvas.width || source_height != canvas.height)
    );
    image
}

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}
