use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use image::DynamicImage;
use image::GenericImageView;
use image::imageops::FilterType;

use crate::runtime::ui::{CapturedFrame, UiStateObservation};
use crate::ui::atoms::GameUi;
use crate::ui::template::TemplateHit;

#[derive(Clone, Debug)]
pub(crate) struct Canvas {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) resize: bool,
}

#[derive(Debug)]
pub(crate) struct Frame {
    pub(crate) image: Arc<DynamicImage>,
    pub(crate) captured_at: Instant,
    pub(crate) ui_state: Option<UiStateObservation>,
}

impl Frame {
    /// 返回 [`Self::image`] 坐标系中的标记命中位置。
    ///
    /// UI 状态分类器可能在查找标记前归一化截图。只有 OCR 使用同一坐标系时才能复用命中位置，否则监听器必须重新执行常规标记搜索。
    pub(crate) fn marker_hits_for_image(&self) -> Option<Vec<TemplateHit>> {
        let UiStateObservation::Classified(state) = self.ui_state.as_ref()? else {
            return None;
        };
        let probe = state.classification().evidence().marker_probe()?;
        let hits = probe.marker_hits();
        // 未知/过渡分类会记录空探测，这不是可复用证据；调用方必须重新执行常规标记搜索，不能将其当成确定的空聊天扫描。
        if hits.is_empty() || probe.coordinate_size() != (self.image.width(), self.image.height()) {
            return None;
        }
        Some(hits.to_vec())
    }
}

#[derive(Default)]
pub(crate) struct LatestFrameCache {
    image: Option<Arc<DynamicImage>>,
    valid: bool,
}

impl LatestFrameCache {
    pub(crate) fn store(&mut self, image: Arc<DynamicImage>) {
        self.image = Some(image);
        self.valid = true;
    }

    pub(crate) fn invalidate(&mut self) {
        self.valid = false;
    }

    pub(crate) fn image(&self) -> Option<Arc<DynamicImage>> {
        self.valid.then(|| self.image.clone()).flatten()
    }
}

pub(crate) fn load_frame(canvas: &Canvas, game_ui: &GameUi) -> Result<Frame> {
    let started = Instant::now();
    let image = Arc::new(game_ui.capture()?);
    let captured_at = Instant::now();
    let image = normalize_frame(image, canvas, started);
    Ok(Frame {
        image,
        captured_at,
        ui_state: None,
    })
}

pub(crate) fn from_captured_frame(frame: &CapturedFrame, canvas: &Canvas) -> Frame {
    let started = Instant::now();
    let source = frame.image_arc();
    Frame {
        image: normalize_frame(source, canvas, started),
        captured_at: frame.captured_at(),
        ui_state: frame.ui_state().cloned(),
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use image::DynamicImage;

    use super::Frame;
    use crate::runtime::ui::{
        UiEvidenceRect, UiMarkerProbeEvidence, UiStateClassification, UiStateEvidence, UiStateKind,
        UiStateTracker,
    };
    use crate::ui::template::TemplateHit;

    #[test]
    fn marker_hits_are_reused_in_the_classifier_coordinate_space() {
        // 分类器会先将 4K 截图归一化到配置的 1920x1080 画布，再进行扫描。命中位置相对该归一化坐标系中的聊天裁剪区，不能再次缩放。
        let classifier_hit = TemplateHit {
            kind: "blue".to_owned(),
            x: 48,
            y: 270,
            width: 20,
            height: 10,
            score: 0.98,
        };
        let classification = UiStateClassification::with_evidence(
            UiStateKind::Primary,
            "primary:marker",
            UiStateEvidence::new(
                Vec::new(),
                Some(UiMarkerProbeEvidence::new(
                    UiEvidenceRect::new(0, 0, 1920, 1080),
                    (1920, 1080),
                    1,
                    0,
                    0,
                    vec![classifier_hit],
                )),
                "primary_chat_markers",
            ),
        );
        let observation = UiStateTracker::new(1).observe(1, classification);
        let frame = Frame {
            image: Arc::new(DynamicImage::new_rgba8(1920, 1080)),
            captured_at: Instant::now(),
            ui_state: Some(observation),
        };

        let marker = frame
            .marker_hits_for_image()
            .expect("marker evidence is available")
            .pop()
            .expect("one marker hit");

        assert_eq!((marker.x, marker.y), (48, 270));
        assert_eq!((marker.width, marker.height), (20, 10));
        assert_eq!(marker.kind, "blue");
        assert_eq!(marker.score, 0.98);
    }

    #[test]
    fn marker_hits_fall_back_when_ocr_uses_a_different_coordinate_space() {
        let classification = UiStateClassification::with_evidence(
            UiStateKind::Primary,
            "primary:marker",
            UiStateEvidence::new(
                Vec::new(),
                Some(UiMarkerProbeEvidence::new(
                    UiEvidenceRect::new(0, 0, 1920, 1080),
                    (1920, 1080),
                    1,
                    0,
                    0,
                    vec![TemplateHit {
                        kind: "blue".to_owned(),
                        x: 48,
                        y: 270,
                        width: 20,
                        height: 10,
                        score: 0.98,
                    }],
                )),
                "primary_chat_markers",
            ),
        );
        let observation = UiStateTracker::new(1).observe(1, classification);
        let frame = Frame {
            image: Arc::new(DynamicImage::new_rgba8(1280, 720)),
            captured_at: Instant::now(),
            ui_state: Some(observation),
        };

        assert!(frame.marker_hits_for_image().is_none());
    }

    #[test]
    fn empty_marker_probe_falls_back_to_the_normal_marker_search() {
        let classification = UiStateClassification::with_evidence(
            UiStateKind::Unknown,
            "unknown:no-marker",
            UiStateEvidence::new(
                Vec::new(),
                Some(UiMarkerProbeEvidence::new(
                    UiEvidenceRect::new(0, 0, 1920, 1080),
                    (1920, 1080),
                    0,
                    0,
                    0,
                    Vec::new(),
                )),
                "no_reliable_anchor",
            ),
        );
        let observation = UiStateTracker::new(1).observe(1, classification);
        let frame = Frame {
            image: Arc::new(DynamicImage::new_rgba8(1920, 1080)),
            captured_at: Instant::now(),
            ui_state: Some(observation),
        };

        assert!(frame.marker_hits_for_image().is_none());
    }
}
