use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use image::DynamicImage;
use image::GenericImageView;
use image::imageops::FilterType;

use super::FrameArgs;
use super::game_ui::GameUi;

#[derive(Clone, Debug)]
pub(super) struct Canvas {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) resize: bool,
}

#[derive(Debug)]
pub(super) struct Frame {
    pub(super) image: Arc<DynamicImage>,
}

pub(super) fn load_frame(args: &FrameArgs, canvas: &Canvas, game_ui: &GameUi) -> Result<Frame> {
    let started = Instant::now();
    let image = match &args.image {
        Some(path) => {
            image::open(path).with_context(|| format!("open image {}", path.display()))?
        }
        None => game_ui.capture()?,
    };
    let image = normalize_frame(image, canvas, started);
    Ok(Frame {
        image: Arc::new(image),
    })
}

fn normalize_frame(image: DynamicImage, canvas: &Canvas, started: Instant) -> DynamicImage {
    let (source_width, source_height) = image.dimensions();
    let image = if canvas.resize && (source_width != canvas.width || source_height != canvas.height)
    {
        image.resize_exact(canvas.width, canvas.height, FilterType::Triangle)
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
