use std::sync::{Arc, Mutex};

use anyhow::Result;
use image::{DynamicImage, GenericImageView};
use serde::Serialize;

use super::change_detection::{ChangeFingerprint, change_stats, rect_chat_change_fingerprint};
use super::geometry::{Point, Rect};

// 坐标沿用项目固定的 1920x1080 游戏画布；监听模式本身仍是运行期状态。
pub(super) const SECONDARY_TITLE_RECT: Rect = Rect::new(600, 24, 480, 72);
const FRIEND_UNREAD_RECT: Rect = Rect::new(46, 250, 48, 650);
const MESSAGE_RECT: Rect = Rect::new(250, 90, 1_020, 850);
const HALL_BUBBLE_SEARCH_RECT: Rect = Rect::new(390, 87, 790, 870);
const HALL_BUBBLE_ANCHOR_LEFT: i32 = 415;
const HALL_BUBBLE_ANCHOR_RIGHT: i32 = 470;
const MIN_HALL_BUBBLE_ANCHOR_PIXELS_PER_ROW: usize = 10;
const MIN_HALL_BUBBLE_HEIGHT: i32 = 45;
const MAX_HALL_BUBBLE_HEIGHT: i32 = 180;
const MIN_HALL_BUBBLE_WIDTH: i32 = 60;
const MAX_HALL_BUBBLE_WIDTH: i32 = 750;
const MIN_RED_PIXELS_PER_ROW: usize = 2;
const MIN_BUBBLE_PIXELS_PER_ROW: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ChatListenerMode {
    Primary,
    Secondary,
}

impl ChatListenerMode {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Primary => "一级监听",
            Self::Secondary => "二级监听",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ChatListenerSnapshot {
    pub(super) mode: ChatListenerMode,
    pub(super) pending_mode: Option<ChatListenerMode>,
    pub(super) initial_unread_clear: bool,
    pub(super) unread_task_pending: bool,
    pub(super) hall_round_required: bool,
}

#[derive(Clone)]
pub(super) struct ChatListenerShared {
    state: Arc<Mutex<ChatListenerState>>,
}

#[derive(Debug)]
struct ChatListenerState {
    mode: ChatListenerMode,
    pending_mode: Option<ChatListenerMode>,
    initial_unread_clear: bool,
    unread_task_pending: bool,
    hall_round_required: bool,
}

impl ChatListenerShared {
    pub(super) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ChatListenerState {
                mode: ChatListenerMode::Primary,
                pending_mode: None,
                initial_unread_clear: false,
                unread_task_pending: false,
                hall_round_required: false,
            })),
        }
    }

    pub(super) fn snapshot(&self) -> ChatListenerSnapshot {
        self.state.lock().map_or_else(
            |_| ChatListenerSnapshot {
                mode: ChatListenerMode::Primary,
                pending_mode: None,
                initial_unread_clear: false,
                unread_task_pending: false,
                hall_round_required: false,
            },
            |state| ChatListenerSnapshot {
                mode: state.mode,
                pending_mode: state.pending_mode,
                initial_unread_clear: state.initial_unread_clear,
                unread_task_pending: state.unread_task_pending,
                hall_round_required: state.hall_round_required,
            },
        )
    }

    pub(super) fn request_mode(&self, target: ChatListenerMode) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.pending_mode.is_some() || state.mode == target {
            return false;
        }
        state.pending_mode = Some(target);
        true
    }

    pub(super) fn complete_mode_switch(&self, mode: ChatListenerMode) {
        if let Ok(mut state) = self.state.lock() {
            state.mode = mode;
            state.pending_mode = None;
            state.initial_unread_clear = mode == ChatListenerMode::Secondary;
            state.unread_task_pending = false;
            state.hall_round_required = false;
        }
    }

    pub(super) fn cancel_mode_request(&self, target: ChatListenerMode) {
        if let Ok(mut state) = self.state.lock() {
            if state.pending_mode == Some(target) {
                state.pending_mode = None;
            }
        }
    }

    pub(super) fn fail_mode_switch_to_primary(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.mode = ChatListenerMode::Primary;
            state.pending_mode = None;
            state.initial_unread_clear = false;
            state.unread_task_pending = false;
            state.hall_round_required = false;
        }
    }

    pub(super) fn claim_unread_task(&self) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.mode != ChatListenerMode::Secondary || state.unread_task_pending {
            return false;
        }
        state.unread_task_pending = true;
        true
    }

    pub(super) fn finish_unread_task(&self, processed_message: bool) {
        if let Ok(mut state) = self.state.lock() {
            state.unread_task_pending = false;
            if !state.initial_unread_clear && processed_message {
                state.hall_round_required = true;
            }
        }
    }

    pub(super) fn release_unread_task(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.unread_task_pending = false;
        }
    }

    pub(super) fn finish_initial_unread_clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            if state.mode == ChatListenerMode::Secondary && !state.unread_task_pending {
                state.initial_unread_clear = false;
                state.hall_round_required = true;
            }
        }
    }

    pub(super) fn finish_hall_round(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.hall_round_required = false;
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SecondaryChatIdentity {
    CurrentHall,
    PublicChannel,
    StrangerMessages,
    Friend(String),
    Unknown,
}

pub(super) fn classify_title(text: &str) -> SecondaryChatIdentity {
    let text = text.trim();
    if text.is_empty() {
        return SecondaryChatIdentity::Unknown;
    }
    if text.contains("当前大厅") {
        return SecondaryChatIdentity::CurrentHall;
    }
    if text.contains("公开频道") {
        return SecondaryChatIdentity::PublicChannel;
    }
    if text.contains("陌生人消息") {
        return SecondaryChatIdentity::StrangerMessages;
    }
    SecondaryChatIdentity::Friend(text.to_string())
}

#[derive(Clone, Copy, Debug)]
pub(super) struct UnreadFriendHit {
    pub(super) indicator: Point,
    pub(super) row_click: Point,
}

#[derive(Clone, Debug)]
pub(super) struct SecondaryHallBubble {
    pub(super) rect: Rect,
    fingerprint: ChangeFingerprint,
}

pub(super) fn find_unread_friend_hits(image: &DynamicImage) -> Vec<UnreadFriendHit> {
    let Some(region) = bounded_rect(image, FRIEND_UNREAD_RECT) else {
        return Vec::new();
    };
    let mut groups = Vec::new();
    let mut active_start = None;
    let mut active_end = 0_i32;
    let mut active_pixels = 0usize;
    let mut active_x_total = 0_i64;

    for y in region.y..region.bottom() {
        let mut pixels = 0usize;
        let mut x_total = 0_i64;
        for x in region.x..region.right() {
            let pixel = image.get_pixel(x as u32, y as u32).0;
            if is_unread_red(pixel[0], pixel[1], pixel[2]) {
                pixels += 1;
                x_total += i64::from(x);
            }
        }
        if pixels >= MIN_RED_PIXELS_PER_ROW {
            if active_start.is_none() {
                active_start = Some(y);
            }
            active_end = y;
            active_pixels += pixels;
            active_x_total += x_total;
            continue;
        }
        if let Some(start) = active_start.take() {
            push_unread_group(
                &mut groups,
                start,
                active_end,
                active_pixels,
                active_x_total,
            );
            active_pixels = 0;
            active_x_total = 0;
        }
    }
    if let Some(start) = active_start {
        push_unread_group(
            &mut groups,
            start,
            active_end,
            active_pixels,
            active_x_total,
        );
    }
    groups
}

pub(super) fn unread_hit_still_visible(image: &DynamicImage, hit: UnreadFriendHit) -> bool {
    let radius = 14_i32;
    let left = (hit.indicator.x - radius).max(0);
    let top = (hit.indicator.y - radius).max(0);
    let right = (hit.indicator.x + radius).min(image.width() as i32 - 1);
    let bottom = (hit.indicator.y + radius).min(image.height() as i32 - 1);
    let mut red_pixels = 0usize;
    for y in top..=bottom {
        for x in left..=right {
            let pixel = image.get_pixel(x as u32, y as u32).0;
            if is_unread_red(pixel[0], pixel[1], pixel[2]) {
                red_pixels += 1;
            }
        }
    }
    red_pixels >= 20
}

pub(super) fn latest_incoming_bubble_rect(image: &DynamicImage) -> Option<Rect> {
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
        if let Some(start) = active_start.take() {
            if active_end - start >= 8 {
                groups.push((start, active_end));
            }
        }
    }
    if let Some(start) = active_start {
        if active_end - start >= 8 {
            groups.push((start, active_end));
        }
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

pub(super) fn latest_incoming_fingerprint(
    image: &DynamicImage,
) -> Result<Option<ChangeFingerprint>> {
    let Some(rect) = latest_incoming_bubble_rect(image) else {
        return Ok(None);
    };
    rect_chat_change_fingerprint(image, rect).map(Some)
}

pub(super) fn secondary_hall_bubbles(image: &DynamicImage) -> Result<Vec<SecondaryHallBubble>> {
    let Some(region) = bounded_rect(image, HALL_BUBBLE_SEARCH_RECT) else {
        return Ok(Vec::new());
    };
    let anchor_left = HALL_BUBBLE_ANCHOR_LEFT.max(region.x);
    let anchor_right = HALL_BUBBLE_ANCHOR_RIGHT.min(region.right());
    if anchor_right <= anchor_left {
        return Ok(Vec::new());
    }

    let mut groups = Vec::new();
    let mut start = None;
    let mut end = region.y;
    for y in region.y..region.bottom() {
        let anchor_pixels = (anchor_left..anchor_right)
            .filter(|x| {
                let pixel = image.get_pixel(*x as u32, y as u32).0;
                is_incoming_bubble(pixel[0], pixel[1], pixel[2])
            })
            .count();
        if anchor_pixels >= MIN_HALL_BUBBLE_ANCHOR_PIXELS_PER_ROW {
            if start.is_none() {
                start = Some(y);
            }
            end = y;
        } else if let Some(top) = start.take() {
            push_hall_bubble_group(&mut groups, image, region, top, end);
        }
    }
    if let Some(top) = start {
        push_hall_bubble_group(&mut groups, image, region, top, end);
    }

    groups
        .into_iter()
        .map(|rect| {
            Ok(SecondaryHallBubble {
                fingerprint: rect_chat_change_fingerprint(image, rect)?,
                rect,
            })
        })
        .collect()
}

pub(super) fn hall_bubble_sequence_overlap(
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

fn push_hall_bubble_group(
    groups: &mut Vec<Rect>,
    image: &DynamicImage,
    region: Rect,
    top: i32,
    bottom: i32,
) {
    let height = bottom - top + 1;
    if !(MIN_HALL_BUBBLE_HEIGHT..=MAX_HALL_BUBBLE_HEIGHT).contains(&height) {
        return;
    }

    let anchor_left = HALL_BUBBLE_ANCHOR_LEFT.max(region.x);
    let anchor_right = HALL_BUBBLE_ANCHOR_RIGHT.min(region.right());
    let mut right_edges = Vec::new();
    for y in top..=bottom {
        let anchor_pixels = (anchor_left..anchor_right)
            .filter(|x| {
                let pixel = image.get_pixel(*x as u32, y as u32).0;
                is_incoming_bubble(pixel[0], pixel[1], pixel[2])
            })
            .count();
        if anchor_pixels < MIN_HALL_BUBBLE_ANCHOR_PIXELS_PER_ROW {
            continue;
        }
        let right = (anchor_right..region.right()).rev().find(|x| {
            let pixel = image.get_pixel(*x as u32, y as u32).0;
            is_incoming_bubble(pixel[0], pixel[1], pixel[2])
        });
        if let Some(right) = right {
            right_edges.push(right);
        }
    }
    if right_edges.is_empty() {
        return;
    }
    right_edges.sort_unstable();
    let percentile_index = (right_edges.len() - 1) * 9 / 10;
    let right = right_edges[percentile_index];
    let left = (HALL_BUBBLE_ANCHOR_LEFT - 5).max(region.x);
    let width = right - left + 1;
    if !(MIN_HALL_BUBBLE_WIDTH..=MAX_HALL_BUBBLE_WIDTH).contains(&width) {
        return;
    }
    groups.push(Rect::new(left, top, width as u32, height as u32));
}

fn same_hall_bubble(left: &SecondaryHallBubble, right: &SecondaryHallBubble) -> bool {
    let stats = change_stats(&left.fingerprint, &right.fingerprint);
    stats.mean_abs_diff < 0.8 && stats.changed_ratio < 0.01
}

fn push_unread_group(
    groups: &mut Vec<UnreadFriendHit>,
    start: i32,
    end: i32,
    pixels: usize,
    x_total: i64,
) {
    let height = end - start + 1;
    if !(5..=30).contains(&height) || !(20..=500).contains(&pixels) {
        return;
    }
    let center_y = start + height / 2;
    let center_x = (x_total / pixels.max(1) as i64) as i32;
    groups.push(UnreadFriendHit {
        indicator: Point::new(center_x, center_y),
        row_click: Point::new(150, center_y),
    });
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

fn is_unread_red(red: u8, green: u8, blue: u8) -> bool {
    red >= 175 && green <= 125 && blue <= 135 && red >= green.saturating_add(65)
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
            classify_title("陌生人消息"),
            SecondaryChatIdentity::StrangerMessages
        );
        assert_eq!(
            classify_title("难以识别的昵称"),
            SecondaryChatIdentity::Friend("难以识别的昵称".to_string())
        );
    }

    #[test]
    fn finds_red_unread_indicator_in_friend_column() {
        let mut image = RgbaImage::new(1920, 1080);
        for y in 300..314 {
            for x in 60..74 {
                image.put_pixel(x, y, Rgba([230, 62, 80, 255]));
            }
        }
        let hits = find_unread_friend_hits(&DynamicImage::ImageRgba8(image));
        assert_eq!(hits.len(), 1);
        assert!((hits[0].row_click.y - 306).abs() <= 1);
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
        let rect =
            latest_incoming_bubble_rect(&DynamicImage::ImageRgba8(image)).expect("latest bubble");
        assert!(rect.y >= 670);
        assert!(rect.bottom() >= 790);
    }

    #[test]
    fn finds_each_complete_hall_bubble_without_title_space() {
        let mut image = RgbaImage::new(1920, 1080);
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
    fn finds_new_hall_bubbles_by_suffix_prefix_overlap() {
        fn bubble(value: u8) -> SecondaryHallBubble {
            SecondaryHallBubble {
                rect: Rect::new(415, 300, 200, 54),
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
}
