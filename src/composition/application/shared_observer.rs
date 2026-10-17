use super::*;
use crate::observation::chat::PassiveScanGate;
use crate::observation::chat::{
    ChatIdentity, PassiveLayout, passive_layout, recognize_passive_secondary,
};
use std::sync::mpsc::{self, RecvTimeoutError};

const OBSERVER_WAIT: Duration = Duration::from_millis(100);
// Test-only diagnostic crop and ROI microbenchmark coordinate; never identity evidence.
#[cfg(test)]
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
        let timeline = self.ui.chat_timeline.clone();
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
                                let _ = timeline.report_failure(format!(
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
                                timeline.report_failure("passive frame subscription disconnected");
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
                            let _ = timeline.report_failure(format!(
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
                        let refresh = timeline.pending_refresh()?;
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
                        // 变化判据用 v5.9.4 的「均值差 / 变化比例」而不是精确像素哈希：
                        // 聊天区透出 3D 场景、粒子、半透明底纹的轻微抖动会被阈值吸收；
                        // 真实消息（文字出现）差异远大于阈值，仍能及时触发。
                        let fingerprint = crate::ui::change_detection::rect_chat_change_fingerprint(
                            &frame.image,
                            config.screen.chat_rect.into(),
                        )?;
                        hash_time = finish_observer_stage(&mut stage_started);
                        let now = Instant::now();
                        let scan_due = gate.scan_due(
                            &fingerprint,
                            now,
                            refresh.is_some(),
                            Duration::from_millis(live.timing.chat_scan.fallback_ms.max(1)),
                            Duration::from_millis(live.timing.chat_scan.change_debounce_ms),
                            config.ocr.change_mean_threshold,
                            config.ocr.change_pixel_threshold,
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
                                crate::debug_read_only::observer().cache_frame(published.clone());
                                timeline.publish(
                                    frame.captured_at,
                                    ChatIdentity::PrimaryHall,
                                    messages,
                                )?;
                            }
                            PassiveLayout::Secondary {
                                hall,
                                selected,
                                bubbles,
                                latest,
                            } => {
                                let started = Instant::now();
                                let (identity, messages) = recognize_passive_secondary(
                                    &ocr, &frame.image, &config, hall, selected, &bubbles, latest,
                                )?;
                                monitor.publish_chat_scan(ChatScanTelemetry {
                                    marker_count: messages.len(),
                                    lines: messages
                                        .iter()
                                        .map(|message| redacted_chat_text(&message.text).to_string())
                                        .collect(),
                                    marker_ms: 0,
                                    ocr_ms: started.elapsed().as_millis(),
                                    total_ms: started.elapsed().as_millis(),
                                    scope: "shared-secondary",
                                });
                                ocr_time = finish_observer_stage(&mut stage_started);
                                crate::debug_read_only::observer().cache_frame(published.clone());
                                timeline.publish(frame.captured_at, identity, messages)?;
                            }
                            PassiveLayout::NoChat => {
                                timeline.publish_no_chat(
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
                            timeline.report_failure(format!("passive chat recognition: {error:#}"));
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


fn capture_satisfies_refresh(captured_at: Instant, refresh: Option<Instant>) -> bool {
    refresh.is_none_or(|requested_at| captured_at >= requested_at)
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
    fn refresh_rejects_old_capture_and_bypasses_unchanged_cache() {
        let requested = Instant::now();
        assert!(!capture_satisfies_refresh(
            requested - Duration::from_millis(1),
            Some(requested)
        ));
        assert!(capture_satisfies_refresh(requested, Some(requested)));
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
    fn signature_tracks_selected_row_and_message_but_ignores_unrelated_pixels() {
        let mut pixels = image::RgbaImage::new(1920, 1080);
        let layout = PassiveLayout::Secondary {
            hall: false,
            selected: Some(Rect::new(80, 228, 170, 67)),
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
        pixels.put_pixel(100, 260, image::Rgba([255, 0, 0, 255]));
        let selection_changed = layout
            .signature(&DynamicImage::ImageRgba8(pixels.clone()), primary)
            .unwrap();
        assert_ne!(first, selection_changed);
        pixels.put_pixel(300, 220, image::Rgba([255, 0, 0, 255]));
        assert_ne!(
            selection_changed,
            layout
                .signature(&DynamicImage::ImageRgba8(pixels), primary)
                .unwrap()
        );
    }

    #[test]
    fn empty_identity_is_unknown_and_remark_is_effective_sender() {
        for name in [
            "",
            "公共频道",
            "陌生人消息",
            "添加会话对象",
            "当前大厅",
            "truncated...",
            "partial…",
        ] {
            assert_eq!(
                passive_secondary_identity(false, Some(name)),
                ChatIdentity::Unknown,
                "{name}"
            );
        }
        assert_eq!(
            passive_secondary_identity(false, None),
            ChatIdentity::Unknown
        );
        assert_eq!(
            passive_secondary_identity(true, None),
            ChatIdentity::SecondaryHall
        );
        assert_eq!(passive_sender("player(remark)"), Some("remark".into()));
        assert_eq!(
            passive_secondary_identity(false, Some("player(remark)")),
            ChatIdentity::Friend("remark".into())
        );
    }
}
