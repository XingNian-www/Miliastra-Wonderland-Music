use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use image::DynamicImage;
use image::GenericImageView;
use image::imageops::FilterType;

use super::FrameArgs;
use super::window;
use crate::config;

static WINDOW_CAPTURE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

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

pub(super) fn load_frame(
    args: &FrameArgs,
    canvas: &Canvas,
    window_config: &config::WindowConfig,
) -> Result<Frame> {
    let started = Instant::now();
    let image = match &args.image {
        Some(path) => {
            image::open(path).with_context(|| format!("open image {}", path.display()))?
        }
        None => capture_game_blocking(window_config)?,
    };
    let image = normalize_frame(image, canvas, started);
    Ok(Frame {
        image: Arc::new(image),
    })
}

fn capture_game_blocking(window_config: &config::WindowConfig) -> Result<DynamicImage> {
    let _guard = WINDOW_CAPTURE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| anyhow!("window capture mutex poisoned"))?;
    window::capture_game(window_config)
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
