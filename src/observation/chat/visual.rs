#[cfg(test)]
use std::path::Path;

use anyhow::Result;
use image::{DynamicImage, GenericImageView};

use super::secondary_hall_vision::confirmed_secondary_hall_bubbles;
use crate::ui::change_detection::{ChangeFingerprint, change_stats, rect_chat_change_fingerprint};
use crate::ui::geometry::Rect;

// 坐标沿用项目固定的 1920x1080 游戏画布；监听模式本身仍是运行期状态。
pub(crate) const SECONDARY_TITLE_RECT: Rect = Rect::new(600, 24, 480, 72);
const MESSAGE_RECT: Rect = Rect::new(250, 90, 1_020, 850);
const MIN_BUBBLE_PIXELS_PER_ROW: usize = 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SecondaryChatIdentity {
    CurrentHall,
    PublicChannel,
    StrangerMessages,
    Friend(String),
    Unknown,
}

pub(crate) fn classify_title(text: &str) -> SecondaryChatIdentity {
    let text = text.trim();
    if text.is_empty() {
        return SecondaryChatIdentity::Unknown;
    }
    if text.contains("当前大厅") {
        return SecondaryChatIdentity::CurrentHall;
    }
    if text.contains("公开频道") || text.contains("公共大厅") {
        return SecondaryChatIdentity::PublicChannel;
    }
    if text.contains("陌生人消息") {
        return SecondaryChatIdentity::StrangerMessages;
    }
    SecondaryChatIdentity::Friend(text.to_string())
}

#[derive(Clone, Debug)]
pub(crate) struct SecondaryHallBubble {
    pub(crate) rect: Rect,
    avatar_rect: Rect,
    sender_rect: Rect,
    sender_fingerprint: ChangeFingerprint,
    fingerprint: ChangeFingerprint,
}

impl SecondaryHallBubble {
    pub(crate) fn avatar_rect(&self) -> Rect {
        self.avatar_rect
    }

    pub(crate) fn sender_rect(&self) -> Rect {
        self.sender_rect
    }
}

pub(crate) fn latest_incoming_bubble_rect(image: &DynamicImage) -> Option<Rect> {
    let region = bounded_rect(image, MESSAGE_RECT)?;
    let mut groups = Vec::new();
    let mut active_start = None;
    let mut active_end = 0_i32;

    for y in region.y..region.bottom() {
        let count = (region.x..region.right())
            .filter(|x| {
                let pixel = image.get_pixel(*x as u32, y as u32).0;
                is_incoming_bubble(pixel[0], pixel[1], pixel[2])
            })
            .count();
        if count >= MIN_BUBBLE_PIXELS_PER_ROW {
            if active_start.is_none() {
                active_start = Some(y);
            }
            active_end = y;
            continue;
        }
        if let Some(start) = active_start.take()
            && active_end - start >= 8
        {
            groups.push((start, active_end));
        }
    }
    if let Some(start) = active_start
        && active_end - start >= 8
    {
        groups.push((start, active_end));
    }
    let (top, bottom) = *groups.last()?;
    let crop_top = (top - 42).max(region.y);
    let crop_bottom = (bottom + 14).min(region.bottom());
    Some(Rect::new(
        region.x + 30,
        crop_top,
        region.width.saturating_sub(80),
        (crop_bottom - crop_top).max(1) as u32,
    ))
}

pub(crate) fn latest_incoming_fingerprint(
    image: &DynamicImage,
) -> Result<Option<ChangeFingerprint>> {
    let Some(rect) = latest_incoming_bubble_rect(image) else {
        return Ok(None);
    };
    rect_chat_change_fingerprint(image, rect).map(Some)
}

pub(crate) fn secondary_hall_bubbles(image: &DynamicImage) -> Result<Vec<SecondaryHallBubble>> {
    confirmed_secondary_hall_bubbles(image)
        .into_iter()
        .map(|bubble| {
            Ok(SecondaryHallBubble {
                fingerprint: rect_chat_change_fingerprint(image, bubble.rect)?,
                sender_fingerprint: rect_chat_change_fingerprint(image, bubble.sender_rect)?,
                rect: bubble.rect,
                avatar_rect: bubble.avatar_rect,
                sender_rect: bubble.sender_rect,
            })
        })
        .collect()
}

pub(crate) fn hall_bubble_sequence_overlap(
    previous: &[SecondaryHallBubble],
    current: &[SecondaryHallBubble],
) -> usize {
    let max_overlap = previous.len().min(current.len());
    for overlap in (1..=max_overlap).rev() {
        let previous_tail = &previous[previous.len() - overlap..];
        let current_head = &current[..overlap];
        if previous_tail
            .iter()
            .zip(current_head)
            .all(|(left, right)| same_hall_bubble(left, right))
        {
            return overlap;
        }
    }
    0
}

pub(crate) fn hall_bubble_sequence_is_retained_prefix(
    previous: &[SecondaryHallBubble],
    current: &[SecondaryHallBubble],
) -> bool {
    !current.is_empty()
        && current.len() < previous.len()
        && previous[..current.len()]
            .iter()
            .zip(current)
            .all(|(left, right)| same_hall_bubble(left, right))
}

pub(crate) fn hall_bubble_sequences_stable(
    previous: &[SecondaryHallBubble],
    current: &[SecondaryHallBubble],
) -> bool {
    previous.len() == current.len()
        && previous
            .iter()
            .zip(current)
            .all(|(left, right)| same_hall_bubble(left, right))
}

fn rect_identity(rect: Rect) -> (i32, i32, u32, u32) {
    (rect.x, rect.y, rect.width, rect.height)
}

fn same_hall_bubble(left: &SecondaryHallBubble, right: &SecondaryHallBubble) -> bool {
    rect_identity(left.sender_rect) == rect_identity(right.sender_rect)
        && fingerprints_match(&left.fingerprint, &right.fingerprint)
        && fingerprints_match(&left.sender_fingerprint, &right.sender_fingerprint)
}

fn fingerprints_match(left: &ChangeFingerprint, right: &ChangeFingerprint) -> bool {
    let stats = change_stats(left, right);
    stats.mean_abs_diff < 0.8 && stats.changed_ratio < 0.01
}

fn bounded_rect(image: &DynamicImage, rect: Rect) -> Option<Rect> {
    let right = rect.right().min(image.width() as i32);
    let bottom = rect.bottom().min(image.height() as i32);
    if rect.x < 0 || rect.y < 0 || right <= rect.x || bottom <= rect.y {
        return None;
    }
    Some(Rect::new(
        rect.x,
        rect.y,
        (right - rect.x) as u32,
        (bottom - rect.y) as u32,
    ))
}

fn is_incoming_bubble(red: u8, green: u8, blue: u8) -> bool {
    (58..=72).contains(&red)
        && (64..=82).contains(&green)
        && (76..=104).contains(&blue)
        && blue >= red.saturating_add(14)
}

#[cfg(test)]
mod tests {
    use image::{Rgba, RgbaImage};

    use super::*;

    #[test]
    fn classifies_known_titles_before_friend_titles() {
        assert_eq!(
            classify_title("当前大厅"),
            SecondaryChatIdentity::CurrentHall
        );
        assert_eq!(
            classify_title("公开频道"),
            SecondaryChatIdentity::PublicChannel
        );
        assert_eq!(
            classify_title("公共大厅"),
            SecondaryChatIdentity::PublicChannel
        );
        assert_eq!(
            classify_title("陌生人消息"),
            SecondaryChatIdentity::StrangerMessages
        );
        assert_eq!(
            classify_title("难以识别的昵称"),
            SecondaryChatIdentity::Friend("难以识别的昵称".to_string())
        );
    }

    #[test]
    fn finds_lowest_dark_bubble_only() {
        let mut image = RgbaImage::new(1920, 1080);
        for (top, bottom) in [(300, 340), (720, 780)] {
            for y in top..bottom {
                for x in 420..760 {
                    image.put_pixel(x, y, Rgba([62, 71, 89, 255]));
                }
            }
        }
        let image = DynamicImage::ImageRgba8(image);
        let rect = latest_incoming_bubble_rect(&image).expect("latest bubble");
        assert!(rect.y >= 670);
        assert!(rect.bottom() >= 790);
    }

    #[test]
    fn finds_each_complete_hall_bubble_without_title_space() {
        let mut image = RgbaImage::new(1920, 1080);
        fill_rect(
            &mut image,
            Rect::new(418, 260, 61, 18),
            Rgba([195, 193, 185, 255]),
        );
        draw_avatar(&mut image, 260);
        for (top, bottom, right) in [(300, 354, 760), (380, 434, 680), (460, 514, 820)] {
            for y in top..bottom {
                for x in 415..right {
                    image.put_pixel(x, y, Rgba([62, 71, 89, 255]));
                }
            }
        }
        for y in 220..250 {
            for x in 415..570 {
                image.put_pixel(x, y, Rgba([62, 71, 89, 255]));
            }
        }

        let bubbles = secondary_hall_bubbles(&DynamicImage::ImageRgba8(image))
            .expect("hall bubble detection");
        assert_eq!(bubbles.len(), 3);
        assert_eq!(bubbles[0].rect.y, 300);
        assert_eq!(bubbles[1].rect.y, 380);
        assert_eq!(bubbles[2].rect.y, 460);
    }

    #[test]
    fn fixed_sender_region_is_attached_to_an_incoming_hall_bubble() {
        let mut image = RgbaImage::new(1920, 1080);
        fill_rect(
            &mut image,
            Rect::new(418, 200, 61, 18),
            Rgba([195, 193, 185, 255]),
        );
        draw_avatar(&mut image, 200);
        fill_rect(
            &mut image,
            Rect::new(410, 240, 190, 60),
            Rgba([62, 71, 89, 255]),
        );

        let bubbles = secondary_hall_bubbles(&DynamicImage::ImageRgba8(image))
            .expect("hall bubble observation");

        assert_eq!(bubbles.len(), 1);
        let sender = bubbles[0].sender_rect();
        assert_eq!((sender.x, sender.width, sender.height), (410, 394, 36));
        assert!(sender.y <= 200);
        assert!(sender.bottom() >= 218);
    }

    #[test]
    fn real_secondary_chat_only_exposes_left_incoming_avatars() {
        let image = image::open(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/ui/secondary-chat-scrolled-1920x1080.jpg"),
        )
        .expect("load secondary chat fixture");

        let bubbles = secondary_hall_bubbles(&image).expect("detect incoming avatar anchors");

        assert!(!bubbles.is_empty());
        assert!(bubbles.iter().all(|bubble| {
            let avatar = bubble.avatar_rect();
            avatar.x == 302 && avatar.right() <= 390
        }));
    }

    #[test]
    fn sender_label_loading_changes_the_fixed_region_fingerprint() {
        let mut image = RgbaImage::new(1920, 1080);
        draw_avatar(&mut image, 200);
        fill_rect(
            &mut image,
            Rect::new(410, 240, 190, 60),
            Rgba([62, 71, 89, 255]),
        );

        let without_label = secondary_hall_bubbles(&DynamicImage::ImageRgba8(image.clone()))
            .expect("hall bubble observation without sender label");
        assert_eq!(without_label.len(), 1);

        fill_rect(
            &mut image,
            Rect::new(418, 200, 61, 18),
            Rgba([195, 193, 185, 255]),
        );
        let with_label = secondary_hall_bubbles(&DynamicImage::ImageRgba8(image))
            .expect("hall bubble observation with sender label");
        assert_eq!(with_label.len(), 1);
        assert!(!hall_bubble_sequences_stable(&without_label, &with_label));
    }

    #[test]
    fn each_avatar_starts_a_sender_group_without_scanning_label_pixels() {
        let mut image = RgbaImage::new(1920, 1080);
        fill_rect(
            &mut image,
            Rect::new(418, 200, 61, 18),
            Rgba([195, 193, 185, 255]),
        );
        draw_avatar(&mut image, 200);
        fill_rect(
            &mut image,
            Rect::new(410, 240, 190, 60),
            Rgba([62, 71, 89, 255]),
        );
        draw_avatar(&mut image, 400);
        fill_rect(
            &mut image,
            Rect::new(410, 500, 190, 60),
            Rgba([62, 71, 89, 255]),
        );

        let bubbles = secondary_hall_bubbles(&DynamicImage::ImageRgba8(image))
            .expect("hall bubble observation");

        assert_eq!(bubbles.len(), 2);
        assert_eq!(bubbles[0].rect.y, 240);
        assert_eq!(bubbles[1].rect.y, 500);
        assert_ne!(bubbles[0].sender_rect().y, bubbles[1].sender_rect().y);
    }

    #[test]
    fn fixed_sender_region_covers_remark_and_is_shared_by_continuation_bubbles() {
        let mut image = RgbaImage::new(1920, 1080);
        fill_rect(
            &mut image,
            Rect::new(418, 200, 55, 18),
            Rgba([195, 193, 185, 255]),
        );
        fill_rect(
            &mut image,
            Rect::new(482, 200, 6, 18),
            Rgba([195, 193, 185, 255]),
        );
        fill_rect(
            &mut image,
            Rect::new(495, 200, 80, 18),
            Rgba([108, 146, 211, 255]),
        );
        fill_rect(
            &mut image,
            Rect::new(582, 200, 6, 18),
            Rgba([195, 193, 185, 255]),
        );
        draw_avatar(&mut image, 200);
        fill_rect(
            &mut image,
            Rect::new(410, 240, 190, 60),
            Rgba([62, 71, 89, 255]),
        );
        fill_rect(
            &mut image,
            Rect::new(410, 306, 220, 60),
            Rgba([62, 71, 89, 255]),
        );

        let bubbles = secondary_hall_bubbles(&DynamicImage::ImageRgba8(image))
            .expect("hall bubble observation");

        assert_eq!(bubbles.len(), 2);
        for bubble in bubbles {
            let sender = bubble.sender_rect();
            assert_eq!((sender.x, sender.width, sender.height), (410, 394, 36));
            assert!(sender.y <= 200);
            assert!(sender.right() >= 588);
        }
    }

    #[test]
    fn finds_new_hall_bubbles_by_suffix_prefix_overlap() {
        fn bubble(value: u8) -> SecondaryHallBubble {
            SecondaryHallBubble {
                rect: Rect::new(415, 300, 200, 54),
                avatar_rect: Rect::new(302, 264, 88, 88),
                sender_rect: Rect::new(418, 260, 61, 18),
                sender_fingerprint: ChangeFingerprint {
                    pixels: vec![25; 104 * 36],
                    width: 104,
                    height: 36,
                },
                fingerprint: ChangeFingerprint {
                    pixels: vec![value; 104 * 36],
                    width: 104,
                    height: 36,
                },
            }
        }

        let previous = vec![bubble(10), bubble(30), bubble(50), bubble(70)];
        let current = vec![bubble(50), bubble(70), bubble(90), bubble(110)];
        assert_eq!(hall_bubble_sequence_overlap(&previous, &current), 2);
    }

    #[test]
    fn sender_region_change_keeps_the_hall_observation_unstable() {
        let fingerprint = ChangeFingerprint {
            pixels: vec![42; 104 * 36],
            width: 104,
            height: 36,
        };
        let sender_fingerprint = ChangeFingerprint {
            pixels: vec![25; 104 * 36],
            width: 104,
            height: 36,
        };
        let first_sender_region = vec![SecondaryHallBubble {
            rect: Rect::new(410, 240, 190, 60),
            avatar_rect: Rect::new(302, 204, 88, 88),
            sender_rect: Rect::new(418, 200, 61, 18),
            sender_fingerprint: sender_fingerprint.clone(),
            fingerprint: fingerprint.clone(),
        }];
        let changed_sender_region = vec![SecondaryHallBubble {
            rect: Rect::new(410, 240, 190, 60),
            avatar_rect: Rect::new(302, 204, 88, 88),
            sender_rect: Rect::new(490, 200, 90, 18),
            sender_fingerprint,
            fingerprint,
        }];

        assert!(!hall_bubble_sequences_stable(
            &first_sender_region,
            &changed_sender_region
        ));
        assert!(hall_bubble_sequences_stable(
            &changed_sender_region,
            &changed_sender_region
        ));
    }

    #[test]
    fn sender_pixels_are_part_of_hall_message_identity_and_stability() {
        let bubble_fingerprint = ChangeFingerprint {
            pixels: vec![42; 104 * 36],
            width: 104,
            height: 36,
        };
        let first = vec![SecondaryHallBubble {
            rect: Rect::new(410, 240, 190, 60),
            avatar_rect: Rect::new(302, 204, 88, 88),
            sender_rect: Rect::new(418, 200, 61, 18),
            sender_fingerprint: ChangeFingerprint {
                pixels: vec![10; 104 * 36],
                width: 104,
                height: 36,
            },
            fingerprint: bubble_fingerprint.clone(),
        }];
        let changed_sender = vec![SecondaryHallBubble {
            rect: Rect::new(410, 240, 190, 60),
            avatar_rect: Rect::new(302, 204, 88, 88),
            sender_rect: Rect::new(418, 200, 61, 18),
            sender_fingerprint: ChangeFingerprint {
                pixels: vec![240; 104 * 36],
                width: 104,
                height: 36,
            },
            fingerprint: bubble_fingerprint,
        }];

        assert!(!hall_bubble_sequences_stable(&first, &changed_sender));
        assert_eq!(hall_bubble_sequence_overlap(&first, &changed_sender), 0);
    }

    fn fill_rect(image: &mut RgbaImage, rect: Rect, color: Rgba<u8>) {
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                image.put_pixel(x as u32, y as u32, color);
            }
        }
    }

    fn draw_avatar(image: &mut RgbaImage, label_y: i32) {
        let left = 302_i32;
        let top = label_y + 4;
        let size = 88_i32;
        let center_x = left + size / 2;
        let center_y = top + size / 2;
        let radius_squared = (size / 2 - 4).pow(2);
        for y in top..top + size {
            for x in left..left + size {
                let dx = x - center_x;
                let dy = y - center_y;
                if dx * dx + dy * dy <= radius_squared {
                    image.put_pixel(x as u32, y as u32, Rgba([220, 220, 220, 255]));
                }
            }
        }
    }
}
