use image::{DynamicImage, GenericImageView};

use crate::ui::geometry::{Point, Rect};

const MIN_RED_PIXELS_PER_ROW: usize = 2;
const REFERENCE_WIDTH: u32 = 1920;
const REFERENCE_HEIGHT: u32 = 1080;
const REFERENCE_FRIEND_LIST_X: i32 = 80;
const REFERENCE_FRIEND_LIST_Y: i32 = 280;

#[derive(Clone, Copy, Debug)]
pub(crate) struct FriendUnreadLayout {
    search_rect: Rect,
    avatar_left: i32,
    avatar_size: u32,
    avatar_top_scan_padding: i32,
    avatar_outer_inset: i32,
    avatar_boundary_width: i32,
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
    badge_avatar_overlap: i32,
    badge_top_tolerance: i32,
}

impl FriendUnreadLayout {
    pub(crate) fn resolve(
        expected_width: u32,
        expected_height: u32,
        friend_list_region: Rect,
    ) -> Self {
        let scale_x = f64::from(expected_width.max(1)) / f64::from(REFERENCE_WIDTH);
        let scale_y = f64::from(expected_height.max(1)) / f64::from(REFERENCE_HEIGHT);
        let uniform_scale = scale_x.min(scale_y);
        let area_scale = scale_x * scale_y;
        let search_left_offset = scale_i32(54 - REFERENCE_FRIEND_LIST_X, scale_x);
        let search_top_offset = scale_i32(250 - REFERENCE_FRIEND_LIST_Y, scale_y);
        let click_offset = proportional_offset(friend_list_region.width, 70, 170);

        Self {
            search_rect: Rect::new(
                friend_list_region.x + search_left_offset,
                friend_list_region.y + search_top_offset,
                scale_u32(28, scale_x),
                friend_list_region
                    .height
                    .saturating_add(scale_u32(50, scale_y)),
            ),
            avatar_left: friend_list_region.x + scale_i32(20 - REFERENCE_FRIEND_LIST_X, scale_x),
            avatar_size: scale_u32(48, uniform_scale),
            avatar_top_scan_padding: scale_i32(10, scale_y),
            avatar_outer_inset: scale_i32(2, uniform_scale).max(1),
            avatar_boundary_width: scale_i32(4, uniform_scale).max(1),
            min_avatar_boundary_edges: 4,
            min_exclamation_pixels: scale_usize(1, area_scale),
            row_click_x: friend_list_region.x + click_offset,
            min_pixels_per_row: scale_usize(MIN_RED_PIXELS_PER_ROW, scale_x),
            min_width: scale_i32(15, uniform_scale).max(1),
            max_width: scale_i32(24, uniform_scale).max(1),
            min_height: scale_i32(5, uniform_scale).max(1),
            max_height: scale_i32(30, uniform_scale).max(1),
            min_pixels: scale_usize(20, area_scale),
            max_pixels: scale_usize(500, area_scale),
            visibility_radius: scale_i32(14, uniform_scale).max(1),
            min_visibility_pixels: scale_usize(20, area_scale),
            badge_avatar_overlap: scale_i32(4, uniform_scale).max(1),
            badge_top_tolerance: scale_i32(6, uniform_scale).max(1),
        }
    }
}

fn scale_i32(value: i32, scale: f64) -> i32 {
    (f64::from(value) * scale).round() as i32
}

fn scale_u32(value: u32, scale: f64) -> u32 {
    (f64::from(value) * scale).round().max(1.0) as u32
}

fn scale_usize(value: usize, scale: f64) -> usize {
    (value as f64 * scale).round().max(1.0) as usize
}

fn proportional_offset(width: u32, numerator: u32, denominator: u32) -> i32 {
    let rounded = (u64::from(width) * u64::from(numerator) + u64::from(denominator) / 2)
        / u64::from(denominator.max(1));
    rounded.min(u64::from(width.saturating_sub(1))) as i32
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct UnreadFriendHit {
    pub(crate) indicator: Point,
    pub(crate) row_click: Point,
}

pub(crate) fn find_unread_friend_hits(
    image: &DynamicImage,
    layout: &FriendUnreadLayout,
) -> Vec<UnreadFriendHit> {
    detect_friend_unread(image, layout)
}

pub(crate) fn unread_hit_still_visible(
    image: &DynamicImage,
    hit: UnreadFriendHit,
    layout: &FriendUnreadLayout,
) -> bool {
    let radius = layout.visibility_radius.max(0);
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
    red_pixels >= layout.min_visibility_pixels
}

fn detect_friend_unread(image: &DynamicImage, config: &FriendUnreadLayout) -> Vec<UnreadFriendHit> {
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
    config: &FriendUnreadLayout,
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
    config: &FriendUnreadLayout,
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
        let badge_is_right_top_protrusion = badge_rect.right()
            > avatar.right() - config.badge_avatar_overlap
            && badge_rect.x >= avatar.x + config.avatar_size as i32 / 2
            && badge_rect.y >= avatar.y - config.badge_top_tolerance
            && badge_rect.y <= avatar.y + config.avatar_size as i32 / 3;
        badge_is_right_top_protrusion
            && avatar_boundary_edges(
                image,
                avatar,
                config.avatar_outer_inset,
                config.avatar_boundary_width,
            ) >= config.min_avatar_boundary_edges
    })
}

fn avatar_boundary_edges(
    image: &DynamicImage,
    rect: Rect,
    outer_inset: i32,
    boundary_width: i32,
) -> usize {
    let Some(rect) = bounded_rect(image, rect) else {
        return 0;
    };
    let size = rect.width.min(rect.height) as i32;
    if size < 8 {
        return 0;
    }
    let center_x = rect.x + size / 2;
    let center_y = rect.y + size / 2;
    let outer_radius = size / 2 - outer_inset;
    let inner_radius = (outer_radius - boundary_width).max(1);
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

        let hits = find_unread_friend_hits(&DynamicImage::ImageRgba8(image), &default_layout());

        assert_eq!(hits.len(), 1);
        assert!((hits[0].row_click.y - 310).abs() <= 1);
        assert_eq!(hits[0].row_click.x, 150);
    }

    #[test]
    fn scales_detection_and_click_coordinates_for_1280_by_720() {
        let friend_list = Rect::new(53, 187, 113, 400);
        let layout = FriendUnreadLayout::resolve(1280, 720, friend_list);
        let mut image = RgbaImage::from_pixel(1280, 720, Rgba([35, 40, 55, 255]));
        draw_friend_avatar_at(&mut image, 13, 200, 32);
        draw_unread_badge(&mut image, Rect::new(37, 200, 13, 13));

        let hits = find_unread_friend_hits(&DynamicImage::ImageRgba8(image), &layout);

        assert_eq!(layout.search_rect.x, 36);
        assert_eq!(layout.search_rect.y, 167);
        assert_eq!(layout.search_rect.width, 19);
        assert_eq!(layout.search_rect.height, 433);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].row_click.x, 100);
        assert_eq!(hits[0].row_click.y, 206);
    }

    #[test]
    fn follows_a_moved_friend_list_region() {
        let layout = FriendUnreadLayout::resolve(1920, 1080, Rect::new(300, 350, 170, 600));
        let mut image = test_image();
        draw_friend_avatar_at(&mut image, 240, 370, 48);
        draw_unread_badge(&mut image, Rect::new(276, 370, 20, 20));

        let hits = find_unread_friend_hits(&DynamicImage::ImageRgba8(image), &layout);

        assert_eq!(layout.search_rect.x, 274);
        assert_eq!(layout.search_rect.y, 320);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].row_click.x, 370);
        assert_eq!(hits[0].row_click.y, 380);
    }

    #[test]
    fn keeps_click_inside_a_narrow_friend_list_region() {
        let friend_list = Rect::new(80, 280, 20, 600);
        let layout = FriendUnreadLayout::resolve(1920, 1080, friend_list);

        assert!(layout.row_click_x >= friend_list.x);
        assert!(layout.row_click_x < friend_list.right());
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

        assert!(
            find_unread_friend_hits(&DynamicImage::ImageRgba8(image), &default_layout()).is_empty()
        );
    }

    #[test]
    fn rejects_a_badge_without_avatar_structure() {
        let mut image = test_image();
        draw_unread_badge(&mut image, Rect::new(56, 300, 20, 20));

        assert!(
            find_unread_friend_hits(&DynamicImage::ImageRgba8(image), &default_layout()).is_empty()
        );
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

        assert!(
            find_unread_friend_hits(&DynamicImage::ImageRgba8(image), &default_layout()).is_empty()
        );
    }

    #[test]
    fn confirms_that_a_detected_badge_is_still_visible() {
        let mut image = test_image();
        draw_friend_avatar(&mut image, 300);
        draw_unread_badge(&mut image, Rect::new(56, 300, 20, 20));
        let image = DynamicImage::ImageRgba8(image);
        let layout = default_layout();
        let hit = find_unread_friend_hits(&image, &layout)[0];

        assert!(unread_hit_still_visible(&image, hit, &layout));
    }

    fn default_layout() -> FriendUnreadLayout {
        FriendUnreadLayout::resolve(1920, 1080, Rect::new(80, 280, 170, 600))
    }

    fn test_image() -> RgbaImage {
        RgbaImage::from_pixel(1920, 1080, Rgba([35, 40, 55, 255]))
    }

    fn draw_friend_avatar(image: &mut RgbaImage, top: i32) {
        draw_friend_avatar_at(image, 20, top, 48);
    }

    fn draw_friend_avatar_at(image: &mut RgbaImage, left: i32, top: i32, size: u32) {
        let size = size as i32;
        let center_x = left + size / 2;
        let center_y = top + size / 2;
        let radius = size / 2 - 2;
        for y in top..top + size {
            for x in left..left + size {
                let dx = x - center_x;
                let dy = y - center_y;
                if dx * dx + dy * dy <= radius.pow(2) {
                    image.put_pixel(x as u32, y as u32, Rgba([190, 180, 170, 255]));
                }
            }
        }
    }

    fn draw_unread_badge(image: &mut RgbaImage, rect: Rect) {
        fill_rect(image, rect, Rgba([230, 62, 80, 255]));
        let stroke_width = ((rect.width * 3 + 10) / 20).max(1) as i32;
        let stroke_left = rect.x + (rect.width as i32 - stroke_width) / 2;
        let line_top = rect.y + ((rect.height * 4 + 10) / 20) as i32;
        let line_bottom = rect.y + ((rect.height * 13 + 10) / 20) as i32;
        let dot_top = rect.y + ((rect.height * 15 + 10) / 20) as i32;
        let dot_bottom = rect.y + ((rect.height * 18 + 10) / 20) as i32;
        for y in line_top..line_bottom {
            for x in stroke_left..stroke_left + stroke_width {
                image.put_pixel(x as u32, y as u32, Rgba([245, 245, 245, 255]));
            }
        }
        for y in dot_top..dot_bottom {
            for x in stroke_left..stroke_left + stroke_width {
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
