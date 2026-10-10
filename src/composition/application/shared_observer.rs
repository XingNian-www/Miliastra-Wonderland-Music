use super::*;
use crate::observation::chat::{
    ChatIdentity, SharedChatInput, latest_incoming_bubble_rect, scan_chat_markers,
};
use std::hash::{Hash, Hasher};
use std::sync::mpsc::{self, RecvTimeoutError};

const OBSERVER_WAIT: Duration = Duration::from_millis(100);
// Same normalized title geometry as the secondary conversation OCR fixtures.
const PASSIVE_SECONDARY_TITLE: Rect = Rect::new(600, 24, 480, 72);

pub(super) struct SharedChatObserverGuard {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Drop for SharedChatObserverGuard {
    fn drop(&mut self) {
        self.stop.store(true, AtomicOrdering::Release);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            log::error!("shared chat observer panicked during shutdown");
        }
    }
}

impl ApplicationRuntime {
    pub(super) fn start_shared_chat_observer(&self) -> Result<SharedChatObserverGuard> {
        let ui = self
            .ui
            .ui_runtime
            .as_ref()
            .context("UI runtime unavailable for shared observer")?
            .handle();
        let config = self.lifecycle.config.clone();
        let live_configs = self.lifecycle.live_configs.clone();
        let ocr = self.ui.ocr.clone();
        let templates = self.ui.chat_templates.clone();
        let monitor = self.lifecycle.monitor.clone();
        let shared = self.ui.shared_ocr.clone();
        let running = Arc::clone(&self.lifecycle.running);
        let draining = Arc::clone(&self.lifecycle.http_reload_draining);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let initial_interval = live_configs.snapshot().timing.loop_idle_ms.max(1);
        let subscription = ui
            .declare_frame_demand(FrameDemand::new(Duration::from_millis(initial_interval))?)
            .context("declare independent shared chat frame demand")?;
        let (ready, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("shared-chat-observer".into())
            .spawn(move || {
                let canvas = Canvas {
                    width: config.screen.expected_width,
                    height: config.screen.expected_height,
                    resize: true,
                };
                let mut subscription = subscription;
                let mut interval = initial_interval;
                let mut gate = PassiveScanGate::default();
                let mut ready = Some(ready);
                while running.load(AtomicOrdering::Acquire)
                    && !worker_stop.load(AtomicOrdering::Acquire)
                {
                    let live = live_configs.snapshot();
                    let next_interval = live.timing.loop_idle_ms.max(1);
                    if next_interval != interval {
                        match ui.declare_frame_demand(
                            FrameDemand::new(Duration::from_millis(next_interval))
                                .expect("nonzero observer interval"),
                        ) {
                            Ok(next) => {
                                subscription = next;
                                interval = next_interval;
                            }
                            Err(error) => {
                                let _ = shared.report_failure(format!(
                                    "update passive frame demand: {error}"
                                ));
                            }
                        }
                    }
                    let publication = match subscription.recv_timeout(OBSERVER_WAIT) {
                        Ok(publication) => publication,
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => {
                            let _ =
                                shared.report_failure("passive frame subscription disconnected");
                            break;
                        }
                    };
                    // The first publication proves the runtime has activated the demand.
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(());
                    }
                    let published = match publication {
                        FramePublication::Captured(frame) => frame,
                        FramePublication::Failed(failure) => {
                            let _ = shared.report_failure(format!(
                                "passive frame capture: {}",
                                failure.reason()
                            ));
                            continue;
                        }
                    };
                    if draining.load(AtomicOrdering::Acquire)
                        || !running.load(AtomicOrdering::Acquire)
                        || worker_stop.load(AtomicOrdering::Acquire)
                    {
                        continue;
                    }
                    let performance = log::log_enabled!(target: "performance", log::Level::Debug);
                    let observer_started = performance.then(Instant::now);
                    let mut stage_started = observer_started;
                    let mut normalize_time = Duration::ZERO;
                    let mut layout_time = Duration::ZERO;
                    let mut hash_time = Duration::ZERO;
                    let mut gate_time = Duration::ZERO;
                    let mut ocr_time = Duration::ZERO;
                    let mut publish_age = Duration::ZERO;
                    let mut layout_kind = "unknown";
                    let mut outcome = "failed";
                    let result = (|| -> Result<()> {
                        let refresh = shared.pending_refresh()?;
                        if !capture_satisfies_refresh(published.captured_at(), refresh) {
                            outcome = "waiting_refresh";
                            return Ok(());
                        }
                        stage_started = performance.then(Instant::now);
                        let frame = from_captured_frame(&published, &canvas);
                        normalize_time = finish_observer_stage(&mut stage_started);
                        let layout = passive_layout(&frame, &config, &templates)?;
                        layout_time = finish_observer_stage(&mut stage_started);
                        layout_kind = match &layout {
                            PassiveLayout::Primary(_) => "primary",
                            PassiveLayout::Secondary { .. } => "secondary",
                            PassiveLayout::NoChat => "no_chat",
                        };
                        let signature =
                            layout.signature(&frame.image, config.screen.chat_rect.into())?;
                        hash_time = finish_observer_stage(&mut stage_started);
                        let now = Instant::now();
                        let scan_due = gate.scan_due(
                            signature,
                            now,
                            refresh.is_some(),
                            Duration::from_millis(live.timing.chat_scan.fallback_ms.max(1)),
                            Duration::from_millis(live.timing.chat_scan.change_debounce_ms),
                        );
                        gate_time = finish_observer_stage(&mut stage_started);
                        if !scan_due {
                            outcome = "gated";
                            return Ok(());
                        }
                        match layout {
                            PassiveLayout::Primary(markers) => {
                                let prepared = prepare_chat_scan_with_markers(
                                    &frame.image,
                                    &templates,
                                    config.screen.chat_rect.into(),
                                    Some(markers),
                                )?;
                                let messages = recognize_prepared_chat(
                                    &ocr,
                                    OcrPriority::ChatObservation,
                                    &templates,
                                    prepared,
                                    Some(&monitor),
                                )?;
                                ocr_time = finish_observer_stage(&mut stage_started);
                                shared.publish(
                                    frame.captured_at,
                                    ChatIdentity::PrimaryHall,
                                    messages
                                        .into_iter()
                                        .map(|message| SharedChatInput {
                                            text: message.text,
                                            sender: None,
                                            raw_sender: None,
                                            message_type: message.message_type,
                                            block: message.block,
                                        })
                                        .collect(),
                                )?;
                            }
                            PassiveLayout::Secondary {
                                hall,
                                bubbles,
                                latest,
                            } => {
                                let started = Instant::now();
                                // A fresh secondary scan always recognizes the title as well.
                                let title = passive_text(
                                    &ocr,
                                    &frame.image,
                                    PASSIVE_SECONDARY_TITLE,
                                    config.ocr.same_line_y_tolerance,
                                )?;
                                let identity = if !hall && !bubbles.is_empty() {
                                    ChatIdentity::Unknown
                                } else {
                                    passive_secondary_identity(hall, &title)
                                };
                                let mut messages = Vec::new();
                                if hall || !bubbles.is_empty() {
                                    messages = recognize_passive_hall(
                                        bubbles
                                            .iter()
                                            .map(|bubble| (bubble.rect, bubble.sender_rect())),
                                        |rect| {
                                            passive_text(
                                                &ocr,
                                                &frame.image,
                                                rect,
                                                config.ocr.same_line_y_tolerance,
                                            )
                                        },
                                    )?;
                                } else if let Some(rect) = latest {
                                    let text = passive_text(
                                        &ocr,
                                        &frame.image,
                                        rect,
                                        config.ocr.same_line_y_tolerance,
                                    )?;
                                    let raw_sender = matches!(&identity, ChatIdentity::Friend(_))
                                        .then(|| title.clone());
                                    let sender = raw_sender.as_deref().and_then(passive_sender);
                                    messages.push(SharedChatInput {
                                        text,
                                        sender,
                                        raw_sender,
                                        message_type: "pink".into(),
                                        block: rect,
                                    });
                                }
                                monitor.publish_chat_scan(ChatScanTelemetry {
                                    marker_count: messages.len(),
                                    lines: messages
                                        .iter()
                                        .map(|message| message.text.clone())
                                        .collect(),
                                    marker_ms: 0,
                                    ocr_ms: started.elapsed().as_millis(),
                                    total_ms: started.elapsed().as_millis(),
                                    scope: "shared-secondary",
                                });
                                ocr_time = finish_observer_stage(&mut stage_started);
                                shared.publish(frame.captured_at, identity, messages)?;
                            }
                            PassiveLayout::NoChat => {
                                shared.publish_no_chat(
                                    frame.captured_at,
                                    "no reliable primary markers or secondary chat geometry",
                                )?;
                            }
                        }
                        if performance {
                            publish_age = frame.captured_at.elapsed();
                        }
                        outcome = "published";
                        gate.complete(now);
                        Ok(())
                    })();
                    if let Some(started) = observer_started {
                        log::debug!(
                            target: "performance",
                            "shared_observer frame_id={} layout={} outcome={} normalize_us={} layout_us={} hash_us={} gate_us={} ocr_total_us={} total_us={} capture_age_us={} capture_to_publish_us={}",
                            published.id(),
                            layout_kind,
                            outcome,
                            normalize_time.as_micros(),
                            layout_time.as_micros(),
                            hash_time.as_micros(),
                            gate_time.as_micros(),
                            ocr_time.as_micros(),
                            started.elapsed().as_micros(),
                            published.captured_at().elapsed().as_micros(),
                            publish_age.as_micros(),
                        );
                    }
                    if let Err(error) = result {
                        // Failures never advance the successful scan cache or satisfy refresh.
                        let _ =
                            shared.report_failure(format!("passive chat recognition: {error:#}"));
                        log::warn!("shared chat observer recognition failed: {error:#}");
                    }
                }
            })
            .context("spawn shared chat observer")?;
        let guard = SharedChatObserverGuard {
            stop,
            worker: Some(worker),
        };
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .context("wait for shared chat frame demand activation")?;
        Ok(guard)
    }
}

fn finish_observer_stage(started: &mut Option<Instant>) -> Duration {
    let Some(previous) = started.as_mut() else {
        return Duration::ZERO;
    };
    let now = Instant::now();
    let elapsed = now.saturating_duration_since(*previous);
    *previous = now;
    elapsed
}

enum PassiveLayout {
    Primary(Vec<crate::ui::template::TemplateHit>),
    Secondary {
        hall: bool,
        bubbles: Vec<SecondaryHallBubble>,
        latest: Option<Rect>,
    },
    NoChat,
}

fn passive_layout(
    frame: &Frame,
    config: &AppConfig,
    templates: &ResolvedTemplateArgs,
) -> Result<PassiveLayout> {
    // Geometry is from this immutable frame, independent of full-screen stability.
    let markers = match frame.marker_hits_for_image() {
        Some(markers) => markers,
        None => scan_chat_markers(&frame.image, templates, config.screen.chat_rect)?,
    };
    if !markers.is_empty() {
        return Ok(PassiveLayout::Primary(markers));
    }
    let bubbles = secondary_hall_bubbles(&frame.image)?;
    let back = best_template_hit(
        &frame.image,
        Some(config.screen.secondary_back_rect.into()),
        &config.templates.secondary_back,
        config.templates.marker_threshold,
    )?
    .is_some();
    if !back && bubbles.is_empty() {
        return Ok(PassiveLayout::NoChat);
    }
    let anchor: Rect = config.screen.secondary_hall_rect.into();
    let friends: Rect = config.invite.friend_list_region.into();
    let hall_search = Rect::new(
        anchor.x.min(friends.x),
        anchor.y.min(friends.y),
        (anchor.right().max(friends.right()) - anchor.x.min(friends.x)) as u32,
        (anchor.bottom().max(friends.bottom()) - anchor.y.min(friends.y)) as u32,
    );
    let hall = best_template_hit(
        &frame.image,
        Some(hall_search),
        &config.templates.secondary_hall,
        config.templates.marker_threshold,
    )?
    .is_some();
    let latest = latest_incoming_bubble_rect(&frame.image);
    let classified_secondary = matches!(frame.ui_state.as_ref(),
        Some(crate::runtime::ui::UiStateObservation::Classified(state))
            if state.classification().kind() == UiStateKind::Secondary);
    // The back button is shared with menus; it alone does not prove chat geometry.
    if !hall && bubbles.is_empty() && latest.is_none() && !classified_secondary {
        return Ok(PassiveLayout::NoChat);
    }
    Ok(PassiveLayout::Secondary {
        hall,
        bubbles,
        latest,
    })
}

impl PassiveLayout {
    fn signature(&self, image: &DynamicImage, primary_rect: Rect) -> Result<u64> {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        match self {
            Self::NoChat => 0_u8.hash(&mut hash),
            Self::Primary(markers) => {
                1_u8.hash(&mut hash);
                for marker in markers {
                    marker.kind.hash(&mut hash);
                    (marker.x, marker.y, marker.width, marker.height).hash(&mut hash);
                }
                hash_region(image, primary_rect, &mut hash)?;
            }
            Self::Secondary {
                hall,
                bubbles,
                latest,
            } => {
                2_u8.hash(&mut hash);
                hall.hash(&mut hash);
                hash_region(image, PASSIVE_SECONDARY_TITLE, &mut hash)?;
                if *hall || !bubbles.is_empty() {
                    for bubble in bubbles {
                        hash_region(image, bubble.rect, &mut hash)?;
                        hash_region(image, bubble.sender_rect(), &mut hash)?;
                    }
                } else if let Some(rect) = latest {
                    hash_region(image, *rect, &mut hash)?;
                }
            }
        }
        Ok(hash.finish())
    }
}

fn hash_region(image: &DynamicImage, rect: Rect, hash: &mut impl Hasher) -> Result<()> {
    (rect.x, rect.y, rect.width, rect.height).hash(hash);
    let right = i64::from(rect.x) + i64::from(rect.width);
    let bottom = i64::from(rect.y) + i64::from(rect.height);
    if rect.x < 0
        || rect.y < 0
        || right > i64::from(image.width())
        || bottom > i64::from(image.height())
    {
        anyhow::bail!(
            "crop rect {},{},{},{} outside image {}x{}",
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            image.width(),
            image.height()
        );
    }
    let bytes_per_pixel = usize::from(image.color().bytes_per_pixel());
    let row_bytes = rect.width as usize * bytes_per_pixel;
    // Preserve the byte-slice Hash length prefix, then stream the same row-major bytes.
    (row_bytes * rect.height as usize).hash(hash);
    if row_bytes == 0 || rect.height == 0 {
        return Ok(());
    }
    let stride = image.width() as usize * bytes_per_pixel;
    let bytes = image.as_bytes();
    for y in rect.y as usize..bottom as usize {
        let start = y * stride + rect.x as usize * bytes_per_pixel;
        hash.write(&bytes[start..start + row_bytes]);
    }
    Ok(())
}

fn passive_text(
    ocr: &OcrRuntimeHandle,
    image: &DynamicImage,
    rect: Rect,
    tolerance: i32,
) -> Result<String> {
    Ok(ocr.merged_text(
        crop_canvas(image, rect)?,
        tolerance,
        OcrPriority::ChatObservation,
    )?)
}

fn recognize_passive_hall(
    regions: impl IntoIterator<Item = (Rect, Rect)>,
    mut recognize: impl FnMut(Rect) -> Result<String>,
) -> Result<Vec<SharedChatInput>> {
    regions
        .into_iter()
        .map(|(block, sender_rect)| {
            let text = recognize(block)?;
            let raw_sender = recognize(sender_rect)?;
            Ok(SharedChatInput {
                text,
                sender: passive_sender(&raw_sender),
                raw_sender: Some(raw_sender),
                message_type: "blue".into(),
                block,
            })
        })
        .collect()
}

fn passive_sender(raw: &str) -> Option<String> {
    let identity = parse_secondary_sender_identity(raw);
    let effective = identity.effective_name().trim();
    (!effective.is_empty()).then(|| effective.to_owned())
}

fn passive_secondary_identity(hall: bool, title: &str) -> ChatIdentity {
    if hall {
        return ChatIdentity::SecondaryHall;
    }
    match passive_sender(title) {
        Some(name)
            if !matches!(
                name.as_str(),
                "\u{597d}\u{53cb}"
                    | "\u{804a}\u{5929}"
                    | "\u{5f53}\u{524d}\u{5927}\u{5385}"
                    | "\u{5927}\u{5385}"
                    | "\u{964c}\u{751f}\u{4eba}"
                    | "\u{4e16}\u{754c}"
            ) =>
        {
            ChatIdentity::Friend(name.into())
        }
        _ => ChatIdentity::Unknown,
    }
}

fn capture_satisfies_refresh(captured_at: Instant, refresh: Option<Instant>) -> bool {
    refresh.is_none_or(|requested_at| captured_at >= requested_at)
}

#[derive(Default)]
struct PassiveScanGate {
    published: Option<u64>,
    candidate: Option<(u64, Instant)>,
    pending: Option<u64>,
    last_scan: Option<Instant>,
}

impl PassiveScanGate {
    fn scan_due(
        &mut self,
        signature: u64,
        now: Instant,
        forced: bool,
        fallback: Duration,
        debounce: Duration,
    ) -> bool {
        let fallback_due = self
            .last_scan
            .is_none_or(|last| now.saturating_duration_since(last) >= fallback);
        let changed = self.published != Some(signature);
        if !changed {
            self.candidate = None;
        } else if self
            .candidate
            .is_none_or(|(previous, _)| previous != signature)
        {
            self.candidate = Some((signature, now));
        }
        let settled = self
            .candidate
            .is_some_and(|(_, first)| now.saturating_duration_since(first) >= debounce);
        let due = forced || fallback_due || (changed && settled);
        self.pending = due.then_some(signature);
        due
    }

    fn complete(&mut self, now: Instant) {
        self.published = self.pending.take();
        self.last_scan = Some(now);
        self.candidate = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference_hash_region(
        image: &DynamicImage,
        rect: Rect,
        hash: &mut impl Hasher,
    ) -> Result<()> {
        (rect.x, rect.y, rect.width, rect.height).hash(hash);
        crop_canvas(image, rect)?.as_bytes().hash(hash);
        Ok(())
    }

    fn assert_region_hash_equivalent(image: &DynamicImage, rects: &[Rect]) {
        let mut actual = std::collections::hash_map::DefaultHasher::new();
        let mut expected = std::collections::hash_map::DefaultHasher::new();
        for &rect in rects {
            let actual_result = hash_region(image, rect, &mut actual).map_err(|e| e.to_string());
            let expected_result =
                reference_hash_region(image, rect, &mut expected).map_err(|e| e.to_string());
            assert_eq!(actual_result, expected_result, "{rect:?}");
            assert_eq!(actual.finish(), expected.finish(), "{rect:?}");
        }
        assert_eq!(actual.finish(), expected.finish());
    }

    #[test]
    fn observer_stage_timing_is_disabled_without_a_start_and_advances_when_enabled() {
        let mut disabled = None;
        assert_eq!(finish_observer_stage(&mut disabled), Duration::ZERO);
        assert!(disabled.is_none());

        let previous = Instant::now() - Duration::from_millis(10);
        let mut enabled = Some(previous);
        let elapsed = finish_observer_stage(&mut enabled);
        assert!(elapsed >= Duration::from_millis(10));
        assert_eq!(enabled, Some(previous + elapsed));
    }

    #[test]
    fn roi_hash_matches_crop_for_all_pixel_formats_and_valid_boundaries() {
        let rgba = DynamicImage::ImageRgba8(image::RgbaImage::from_fn(7, 5, |x, y| {
            image::Rgba([
                (x * 23 + y * 7) as u8,
                (x * 11 + y * 31) as u8,
                (x * 3 + y * 41) as u8,
                (x * 17 + y * 13) as u8,
            ])
        }));
        let images = [
            DynamicImage::ImageLuma8(rgba.to_luma8()),
            DynamicImage::ImageLumaA8(rgba.to_luma_alpha8()),
            DynamicImage::ImageRgb8(rgba.to_rgb8()),
            rgba.clone(),
            DynamicImage::ImageLuma16(rgba.to_luma16()),
            DynamicImage::ImageLumaA16(rgba.to_luma_alpha16()),
            DynamicImage::ImageRgb16(rgba.to_rgb16()),
            DynamicImage::ImageRgba16(rgba.to_rgba16()),
            DynamicImage::ImageRgb32F(rgba.to_rgb32f()),
            DynamicImage::ImageRgba32F(rgba.to_rgba32f()),
        ];
        for image in &images {
            for x in 0..=image.width() {
                for y in 0..=image.height() {
                    for width in 0..=image.width() - x {
                        for height in 0..=image.height() - y {
                            assert_region_hash_equivalent(
                                image,
                                &[Rect::new(x as i32, y as i32, width, height)],
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn roi_hash_matches_crop_errors_and_partial_hash_state() {
        let image = DynamicImage::new_rgb8(7, 5);
        for rect in [
            Rect::new(-1, 0, 1, 1),
            Rect::new(0, -1, 1, 1),
            Rect::new(-1, -1, 0, 0),
            Rect::new(7, 0, 1, 1),
            Rect::new(0, 5, 1, 1),
            Rect::new(8, 0, 0, 0),
            Rect::new(0, 6, 0, 0),
            Rect::new(i32::MAX, i32::MAX, u32::MAX, u32::MAX),
            Rect::new(1, 1, u32::MAX, u32::MAX),
        ] {
            assert_region_hash_equivalent(&image, &[Rect::new(1, 1, 2, 2), rect]);
        }
    }

    #[test]
    fn roi_hash_matches_crop_for_empty_and_multiple_regions() {
        let image = DynamicImage::ImageRgba8(image::RgbaImage::from_fn(7, 5, |x, y| {
            image::Rgba([x as u8, y as u8, (x + y * 7) as u8, 255])
        }));
        assert_region_hash_equivalent(&image, &[]);
        assert_region_hash_equivalent(
            &image,
            &[
                Rect::new(1, 1, 3, 2),
                Rect::new(7, 5, 0, 0),
                Rect::new(7, 0, 0, 5),
                Rect::new(0, 5, 7, 0),
                Rect::new(0, 0, 7, 5),
                Rect::new(4, 3, 3, 2),
                Rect::new(1, 1, 3, 2),
            ],
        );
        assert_region_hash_equivalent(&DynamicImage::new_rgba8(0, 0), &[Rect::new(0, 0, 0, 0)]);
    }

    #[test]
    #[ignore = "release-only ROI hashing microbenchmark; run explicitly with --nocapture"]
    fn roi_hash_copy_reduction_benchmark() {
        let image = DynamicImage::ImageRgba8(image::RgbaImage::from_fn(1920, 1080, |x, y| {
            image::Rgba([x as u8, y as u8, (x + y) as u8, 255])
        }));
        let scenarios = [
            ("primary", vec![Rect::new(0, 540, 1920, 540)]),
            (
                "secondary",
                vec![
                    PASSIVE_SECONDARY_TITLE,
                    Rect::new(600, 200, 600, 100),
                    Rect::new(600, 170, 300, 30),
                    Rect::new(600, 500, 600, 100),
                    Rect::new(600, 470, 300, 30),
                ],
            ),
        ];
        for (name, rects) in scenarios {
            assert_region_hash_equivalent(&image, &rects);
            let iterations = 200;
            let run = |copy: bool| {
                let started = Instant::now();
                for _ in 0..iterations {
                    let mut hash = std::collections::hash_map::DefaultHasher::new();
                    for &rect in &rects {
                        if copy {
                            reference_hash_region(std::hint::black_box(&image), rect, &mut hash)
                                .unwrap();
                        } else {
                            hash_region(std::hint::black_box(&image), rect, &mut hash).unwrap();
                        }
                    }
                    std::hint::black_box(hash.finish());
                }
                started.elapsed()
            };
            let crop = run(true);
            let roi = run(false);
            let copied_bytes: u64 = rects
                .iter()
                .map(|rect| u64::from(rect.width) * u64::from(rect.height) * 4)
                .sum();
            println!(
                "ROI hash {name}: iterations={iterations}, crop={crop:?}, roi={roi:?}, avoided_copy_bytes_per_iteration={copied_bytes}"
            );
        }
    }

    #[test]
    fn unchanged_frames_skip_ocr_until_fallback() {
        let now = Instant::now();
        let mut gate = PassiveScanGate::default();
        let fallback = Duration::from_secs(5);
        let debounce = Duration::from_millis(100);
        assert!(gate.scan_due(1, now, false, fallback, debounce));
        gate.complete(now);
        assert!(!gate.scan_due(1, now + Duration::from_secs(1), false, fallback, debounce));
        assert!(gate.scan_due(1, now + fallback, false, fallback, debounce));
    }

    #[test]
    fn refresh_rejects_old_capture_and_bypasses_unchanged_cache() {
        let requested = Instant::now();
        assert!(!capture_satisfies_refresh(
            requested - Duration::from_millis(1),
            Some(requested)
        ));
        assert!(capture_satisfies_refresh(requested, Some(requested)));
        let mut gate = PassiveScanGate::default();
        assert!(gate.scan_due(7, requested, false, Duration::from_secs(5), Duration::ZERO));
        gate.complete(requested);
        assert!(gate.scan_due(
            7,
            requested,
            true,
            Duration::from_secs(5),
            Duration::from_secs(1)
        ));
    }

    #[test]
    fn changed_frames_debounce_and_failed_scan_stays_due() {
        let now = Instant::now();
        let mut gate = PassiveScanGate::default();
        let fallback = Duration::from_secs(5);
        let debounce = Duration::from_millis(100);
        assert!(gate.scan_due(1, now, false, fallback, debounce));
        gate.complete(now);
        assert!(!gate.scan_due(2, now + debounce, false, fallback, debounce));
        assert!(gate.scan_due(2, now + debounce * 2, false, fallback, debounce));
        assert!(gate.scan_due(2, now + debounce * 3, false, fallback, debounce));
    }

    #[test]
    fn ordinary_and_empty_texts_always_recognize_their_sender_rectangles() {
        let regions = [
            (Rect::new(1, 1, 10, 10), Rect::new(1, 20, 10, 10)),
            (Rect::new(1, 40, 10, 10), Rect::new(1, 60, 10, 10)),
        ];
        let mut visited = Vec::new();
        let messages = recognize_passive_hall(regions, |rect| {
            visited.push(rect.y);
            Ok(match rect.y {
                1 => "ordinary conversation",
                20 => "player(remark)",
                40 => "",
                60 => "second player",
                _ => unreachable!(),
            }
            .into())
        })
        .unwrap();
        assert_eq!(visited, vec![1, 20, 40, 60]);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].sender.as_deref(), Some("remark"));
        assert_eq!(messages[0].raw_sender.as_deref(), Some("player(remark)"));
        assert!(messages[1].text.is_empty());
        assert_eq!(messages[1].sender.as_deref(), Some("second player"));
    }

    #[test]
    fn signature_tracks_title_and_message_but_ignores_unrelated_pixels() {
        let mut pixels = image::RgbaImage::new(1920, 1080);
        let layout = PassiveLayout::Secondary {
            hall: false,
            bubbles: Vec::new(),
            latest: Some(Rect::new(280, 200, 400, 80)),
        };
        let primary = Rect::new(0, 0, 10, 10);
        let first = layout
            .signature(&DynamicImage::ImageRgba8(pixels.clone()), primary)
            .unwrap();
        pixels.put_pixel(1800, 1000, image::Rgba([255, 0, 0, 255]));
        assert_eq!(
            first,
            layout
                .signature(&DynamicImage::ImageRgba8(pixels.clone()), primary)
                .unwrap()
        );
        pixels.put_pixel(650, 40, image::Rgba([255, 0, 0, 255]));
        let title_changed = layout
            .signature(&DynamicImage::ImageRgba8(pixels.clone()), primary)
            .unwrap();
        assert_ne!(first, title_changed);
        pixels.put_pixel(300, 220, image::Rgba([255, 0, 0, 255]));
        assert_ne!(
            title_changed,
            layout
                .signature(&DynamicImage::ImageRgba8(pixels), primary)
                .unwrap()
        );
    }

    #[test]
    fn empty_identity_is_unknown_and_remark_is_effective_sender() {
        assert_eq!(passive_secondary_identity(false, ""), ChatIdentity::Unknown);
        assert_eq!(
            passive_secondary_identity(true, ""),
            ChatIdentity::SecondaryHall
        );
        assert_eq!(passive_sender("player(remark)"), Some("remark".into()));
        assert_eq!(
            passive_secondary_identity(false, "player(remark)"),
            ChatIdentity::Friend("remark".into())
        );
    }
}
