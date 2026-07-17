mod detection;
mod sender_label;

use image::DynamicImage;

use self::detection::{DetectionConfig, Rect as VisionRect, detect_bubbles};
use self::sender_label::{SenderLabelConfig, segment_sender_labels};
use crate::ui::geometry::Rect;

pub(super) struct ConfirmedSecondaryHallBubble {
    pub(super) rect: Rect,
    pub(super) avatar_rect: Rect,
    pub(super) sender_rect: Rect,
}

pub(super) fn confirmed_secondary_hall_bubbles(
    image: &DynamicImage,
) -> Vec<ConfirmedSecondaryHallBubble> {
    let detected = detect_bubbles(image, &DetectionConfig::default());
    let senders = segment_sender_labels(image, &detected, &SenderLabelConfig::default());
    let mut sender_evidence = vec![None; detected.len()];
    for sender in senders {
        for index in sender.bubble_indices {
            if let Some(slot) = sender_evidence.get_mut(index) {
                *slot = Some((
                    geometry_rect(sender.avatar_rect),
                    geometry_rect(sender.sender_rect),
                ));
            }
        }
    }
    detected
        .into_iter()
        .zip(sender_evidence)
        .filter_map(|(bubble, sender_evidence)| {
            sender_evidence.map(|(avatar_rect, sender_rect)| ConfirmedSecondaryHallBubble {
                rect: geometry_rect(bubble.rect),
                avatar_rect,
                sender_rect,
            })
        })
        .collect()
}

fn geometry_rect(rect: VisionRect) -> Rect {
    Rect::new(rect.x, rect.y, rect.width, rect.height)
}
