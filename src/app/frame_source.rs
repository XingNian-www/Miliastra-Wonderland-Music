use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use image::DynamicImage;
use image::GenericImageView;
use image::imageops::FilterType;

use super::FrameArgs;
use super::config;
use super::window;

static WINDOW_CAPTURE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Clone, Debug)]
pub(super) struct Canvas {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) resize: bool,
}

#[derive(Debug)]
pub(super) struct Frame {
    pub(super) image: DynamicImage,
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
        None => {
            let _guard = WINDOW_CAPTURE_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .map_err(|_| anyhow!("window capture mutex poisoned"))?;
            window::capture_game(window_config)?
        }
    };
    let (source_width, source_height) = image.dimensions();
    let image = if canvas.resize && (source_width != canvas.width || source_height != canvas.height)
    {
        image.resize_exact(canvas.width, canvas.height, FilterType::Triangle)
    } else {
        image
    };
    log::debug!(
        "截图加载耗时: {}ms source={}x{} output={}x{} resize={}",
        elapsed_ms(started),
        source_width,
        source_height,
        image.width(),
        image.height(),
        canvas.resize && (source_width != canvas.width || source_height != canvas.height)
    );
    Ok(Frame { image })
}

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}
