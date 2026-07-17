use image::{DynamicImage, GenericImageView};

use crate::ui::geometry::{Point, Rect};

const MIN_RED_PIXELS_PER_ROW: usize = 2;

#[derive(Clone, Debug)]
struct FriendUnreadConfig {
    search_rect: Rect,
    avatar_left: i32,
    avatar_size: u32,
    avatar_top_scan_padding: i32,
    min_avatar_boundary_edges: usize,
    min_exclamation_pixels: usize,
    row_click_x: i32,
    min_pixels_per_row: usize,
    min_width: i32,
    max_width: i32,
    min_height: i32,
    max_height: i32,
    min_pixels: usize,
    max_pixels: usize,
    visibility_radius: i32,
    min_visibility_pixels: usize,
}

impl Default for FriendUnreadConfig {
    fn default() -> Self {
        Self {
            search_rect: Rect::new(54, 250, 28, 650),
            avatar_left: 20,
            avatar_size: 48,
            avatar_top_scan_padding: 10,
            min_avatar_boundary_edges: 4,
            min_exclamation_pixels: 1,
            row_click_x: 150,
            min_pixels_per_row: MIN_RED_PIXELS_PER_ROW,
            min_width: 15,
            max_width: 24,
            min_height: 5,
            max_height: 30,
            min_pixels: 20,
            max_pixels: 500,
            visibility_radius: 14,
            min_visibility_pixels: 20,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct UnreadFriendHit {
    pub(crate) indicator: Point,
    pub(crate) row_click: Point,
}

pub(crate) fn find_unread_friend_hits(image: &DynamicImage) -> Vec<UnreadFriendHit> {
    detect_friend_unread(image, &FriendUnreadConfig::default())
}

pub(crate) fn unread_hit_still_visible(image: &DynamicImage, hit: UnreadFriendHit) -> bool {
    let config = FriendUnreadConfig::default();
    let radius = config.visibility_radius.max(0);
    let left = (hit.indicator.x - radius).max(0);
    let top = (hit.indicator.y - radius).max(0);
    let right = (hit.indicator.x + radius).min(image.width() as i32 - 1);
    let bottom = (hit.indicator.y + radius).min(image.height() as i32 - 1);
    let red_pixels = (top..=bottom)
        .flat_map(|y| (left..=right).map(move |x| (x, y)))
        .filter(|(x, y)| {
            let pixel = image.get_pixel(*x as u32, *y as u32).0;
            is_unread_red(pixel[0], pixel[1], pixel[2])
        })
        .count();
    red_pixels >= config.min_visibility_pixels
}

fn detect_friend_unread(image: &DynamicImage, config: &FriendUnreadConfig) -> Vec<UnreadFriendHit> {
    let Some(region) = bounded_rect(image, config.search_rect) else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    let mut active_start = None;
    let mut active_end = 0_i32;
    let mut active_pixels = 0usize;
    let mut active_x_total = 0_i64;
    let mut active_left = region.right();
    let mut active_right = region.x;

    for y in region.y..region.bottom() {
        let mut pixels = 0usize;
        let mut x_total = 0_i64;
        let mut row_left = region.right();
        let mut row_right = region.x;
        for x in region.x..region.right() {
            let pixel = image.get_pixel(x as u32, y as u32).0;
            if is_unread_red(pixel[0], pixel[1], pixel[2]) {
                pixels += 1;
                x_total += i64::from(x);
                row_left = row_left.min(x);
                row_right = row_right.max(x);
            }
        }
        if pixels >= config.min_pixels_per_row {
            active_start.get_or_insert(y);
            active_end = y;
            active_pixels += pixels;
            active_x_total += x_total;
            active_left = active_left.min(row_left);
            active_right = active_right.max(row_right);
            continue;
        }
        if let Some(start) = active_start.take() {
            push_unread_group(
                &mut hits,
                image,
                config,
                ActiveGroup {
                    start,
                    end: active_end,
                    pixels: active_pixels,
                    x_total: active_x_total,
                    left: active_left,
                    right: active_right,
                },
            );
            active_pixels = 0;
            active_x_total = 0;
            active_left = region.right();
            active_right = region.x;
        }
    }
    if let Some(start) = active_start {
        push_unread_group(
            &mut hits,
            image,
            config,
            ActiveGroup {
                start,
                end: active_end,
                pixels: active_pixels,
                x_total: active_x_total,
                left: active_left,
                right: active_right,
            },
        );
    }
    hits
}

struct ActiveGroup {
    start: i32,
    end: i32,
    pixels: usize,
    x_total: i64,
    left: i32,
    right: i32,
}

fn push_unread_group(
    hits: &mut Vec<UnreadFriendHit>,
    image: &DynamicImage,
    config: &FriendUnreadConfig,
    group: ActiveGroup,
) {
    let height = group.end - group.start + 1;
    let width = group.right - group.left + 1;
    let rect = Rect::new(group.left, group.start, width.max(1) as u32, height as u32);
    if !(config.min_width..=config.max_width).contains(&width)
        || !(config.min_height..=config.max_height).contains(&height)
        || !(config.min_pixels..=config.max_pixels).contains(&group.pixels)
        || !badge_protrudes_from_avatar(image, config, rect)
        || !has_unread_exclamation(image, rect, config.min_exclamation_pixels)
    {
        return;
    }
    let center_y = group.start + height / 2;
    let center_x = (group.x_total / group.pixels.max(1) as i64) as i32;
    hits.push(UnreadFriendHit {
        indicator: Point::new(center_x, center_y),
        row_click: Point::new(config.row_click_x, center_y),
    });
}

fn badge_protrudes_from_avatar(
    image: &DynamicImage,
    config: &FriendUnreadConfig,
    badge_rect: Rect,
) -> bool {
    let scan_start = badge_rect.y - config.avatar_top_scan_padding;
    let scan_end = badge_rect.bottom() + config.avatar_top_scan_padding;
    (scan_start..=scan_end).any(|top| {
        let avatar = Rect::new(
            config.avatar_left,
            top,
            config.avatar_size,
            config.avatar_size,
        );
        let badge_is_right_top_protrusion = badge_rect.right() > avatar.right() - 4
            && badge_rect.x >= avatar.x + config.avatar_size as i32 / 2
            && badge_rect.y >= avatar.y - 6
            && badge_rect.y <= avatar.y + config.avatar_size as i32 / 3;
        badge_is_right_top_protrusion
            && avatar_boundary_edges(image, avatar) >= config.min_avatar_boundary_edges
    })
}

fn avatar_boundary_edges(image: &DynamicImage, rect: Rect) -> usize {
    let Some(rect) = bounded_rect(image, rect) else {
        return 0;
    };
    let size = rect.width.min(rect.height) as i32;
    if size < 8 {
        return 0;
    }
    let center_x = rect.x + size / 2;
    let center_y = rect.y + size / 2;
    let outer_radius = size / 2 - 2;
    let inner_radius = (outer_radius - 4).max(1);
    let samples = [
        (-3, -10),
        (3, -10),
        (-8, -7),
        (8, -7),
        (-10, -3),
        (10, -3),
        (-10, 3),
        (10, 3),
        (-8, 7),
        (8, 7),
        (-3, 10),
        (3, 10),
    ];
    samples
        .into_iter()
        .filter(|(dx, dy)| {
            let inner_x = center_x + dx * inner_radius / 10;
            let inner_y = center_y + dy * inner_radius / 10;
            let outer_x = center_x + dx * outer_radius / 10;
            let outer_y = center_y + dy * outer_radius / 10;
            if inner_x < 0
                || inner_y < 0
                || outer_x < 0
                || outer_y < 0
                || inner_x >= image.width() as i32
                || outer_x >= image.width() as i32
                || inner_y >= image.height() as i32
                || outer_y >= image.height() as i32
            {
                return false;
            }
            let inner = image.get_pixel(inner_x as u32, inner_y as u32).0;
            let outer = image.get_pixel(outer_x as u32, outer_y as u32).0;
            color_distance(inner, outer) >= 35
        })
        .count()
}

fn has_unread_exclamation(image: &DynamicImage, rect: Rect, min_pixels: usize) -> bool {
    let Some(rect) = bounded_rect(image, rect) else {
        return false;
    };
    let pixels = (rect.y..rect.bottom())
        .flat_map(|y| (rect.x..rect.right()).map(move |x| (x, y)))
        .filter(|(x, y)| {
            let pixel = image.get_pixel(*x as u32, *y as u32).0;
            is_exclamation_pixel(pixel[0], pixel[1], pixel[2])
        })
        .count();
    pixels >= min_pixels
}

fn color_distance(left: [u8; 4], right: [u8; 4]) -> u16 {
    u16::from(left[0].abs_diff(right[0]))
        + u16::from(left[1].abs_diff(right[1]))
        + u16::from(left[2].abs_diff(right[2]))
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

fn is_exclamation_pixel(red: u8, green: u8, blue: u8) -> bool {
    red >= 180
        && green >= 180
        && blue >= 180
        && red.abs_diff(green) <= 70
        && red.abs_diff(blue) <= 70
}

#[cfg(test)]
mod tests {
    use image::{Rgba, RgbaImage};

    use super::*;

    #[test]
    fn finds_a_badge_attached_to_a_circular_friend_avatar() {
        let mut image = test_image();
        draw_friend_avatar(&mut image, 300);
        draw_unread_badge(&mut image, Rect::new(56, 300, 20, 20));

        let hits = find_unread_friend_hits(&DynamicImage::ImageRgba8(image));

        assert_eq!(hits.len(), 1);
        assert!((hits[0].row_click.y - 310).abs() <= 1);
        assert_eq!(hits[0].row_click.x, 150);
    }

    #[test]
    fn rejects_red_shapes_with_the_wrong_geometry() {
        let mut image = test_image();
        fill_rect(
            &mut image,
            Rect::new(54, 300, 28, 80),
            Rgba([230, 62, 80, 255]),
        );
        fill_rect(
            &mut image,
            Rect::new(54, 701, 5, 24),
            Rgba([230, 62, 80, 255]),
        );

        assert!(find_unread_friend_hits(&DynamicImage::ImageRgba8(image)).is_empty());
    }

    #[test]
    fn rejects_a_badge_without_avatar_structure() {
        let mut image = test_image();
        draw_unread_badge(&mut image, Rect::new(56, 300, 20, 20));

        assert!(find_unread_friend_hits(&DynamicImage::ImageRgba8(image)).is_empty());
    }

    #[test]
    fn rejects_a_red_badge_without_the_white_exclamation() {
        let mut image = test_image();
        draw_friend_avatar(&mut image, 300);
        fill_rect(
            &mut image,
            Rect::new(56, 300, 20, 20),
            Rgba([230, 62, 80, 255]),
        );

        assert!(find_unread_friend_hits(&DynamicImage::ImageRgba8(image)).is_empty());
    }

    #[test]
    fn confirms_that_a_detected_badge_is_still_visible() {
        let mut image = test_image();
        draw_friend_avatar(&mut image, 300);
        draw_unread_badge(&mut image, Rect::new(56, 300, 20, 20));
        let image = DynamicImage::ImageRgba8(image);
        let hit = find_unread_friend_hits(&image)[0];

        assert!(unread_hit_still_visible(&image, hit));
    }

    fn test_image() -> RgbaImage {
        RgbaImage::from_pixel(1920, 1080, Rgba([35, 40, 55, 255]))
    }

    fn draw_friend_avatar(image: &mut RgbaImage, top: i32) {
        let center_x = 44_i32;
        let center_y = top + 24;
        for y in top..top + 48 {
            for x in 20..68 {
                let dx = x - center_x;
                let dy = y - center_y;
                if dx * dx + dy * dy <= 22_i32.pow(2) {
                    image.put_pixel(x as u32, y as u32, Rgba([190, 180, 170, 255]));
                }
            }
        }
    }

    fn draw_unread_badge(image: &mut RgbaImage, rect: Rect) {
        fill_rect(image, rect, Rgba([230, 62, 80, 255]));
        for y in rect.y + 4..rect.y + 13 {
            for x in rect.x + 9..rect.x + 12 {
                image.put_pixel(x as u32, y as u32, Rgba([245, 245, 245, 255]));
            }
        }
        for y in rect.y + 15..rect.y + 18 {
            for x in rect.x + 9..rect.x + 12 {
                image.put_pixel(x as u32, y as u32, Rgba([245, 245, 245, 255]));
            }
        }
    }

    fn fill_rect(image: &mut RgbaImage, rect: Rect, color: Rgba<u8>) {
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                image.put_pixel(x as u32, y as u32, color);
            }
        }
    }
}
