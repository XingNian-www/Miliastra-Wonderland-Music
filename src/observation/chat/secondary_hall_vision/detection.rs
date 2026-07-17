use image::{DynamicImage, GenericImageView};

const MAX_RIGHT_EDGE_GAP: i32 = 8;
const STRONG_ROW_PERCENT: usize = 45;
const MAX_INTERNAL_WEAK_ROWS: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Rect {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) width: u32,
    pub(super) height: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub const fn right(self) -> i32 {
        self.x + self.width as i32
    }

    pub const fn bottom(self) -> i32 {
        self.y + self.height as i32
    }
}

#[derive(Clone, Debug)]
pub(super) struct DetectionConfig {
    search_rect: Rect,
    anchor_left: i32,
    anchor_right: i32,
    min_anchor_pixels_per_row: usize,
    min_height: i32,
    max_height: i32,
    min_width: i32,
    max_width: i32,
    left_padding: i32,
    min_dark_fill_ratio: f32,
    boundary_margin: i32,
}

impl Default for DetectionConfig {
    fn default() -> Self {
        Self {
            search_rect: Rect::new(390, 87, 790, 870),
            anchor_left: 415,
            anchor_right: 470,
            min_anchor_pixels_per_row: 10,
            min_height: 50,
            max_height: 180,
            min_width: 60,
            max_width: 750,
            left_padding: 5,
            min_dark_fill_ratio: 0.35,
            boundary_margin: 1,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct DetectedBubble {
    pub(super) rect: Rect,
}

#[derive(Clone, Debug)]
struct BubbleCandidate {
    rect: Rect,
    accepted: bool,
}

pub(super) fn detect_bubbles(
    image: &DynamicImage,
    config: &DetectionConfig,
) -> Vec<DetectedBubble> {
    let region = config.search_rect;
    let anchor_left = config.anchor_left.max(region.x);
    let anchor_right = config.anchor_right.min(region.right());
    if region.x < 0
        || region.y < 0
        || region.right() > image.width() as i32
        || region.bottom() > image.height() as i32
        || anchor_right <= anchor_left
    {
        return Vec::new();
    }

    let mut row_groups = Vec::new();
    let mut group_start = None;
    let mut group_end = region.y;
    for y in region.y..region.bottom() {
        let anchor_pixels = (anchor_left..anchor_right)
            .filter(|x| is_incoming_pixel(image.get_pixel(*x as u32, y as u32).0))
            .count();
        if anchor_pixels >= config.min_anchor_pixels_per_row {
            group_start.get_or_insert(y);
            group_end = y;
        } else if let Some(start) = group_start.take() {
            row_groups.push((start, group_end));
        }
    }
    if let Some(start) = group_start {
        row_groups.push((start, group_end));
    }

    let mut candidates = row_groups
        .into_iter()
        .filter_map(|(top, bottom)| detect_group(image, config, top, bottom))
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| (candidate.rect.y, candidate.rect.x));
    candidates
        .into_iter()
        .filter(|candidate| candidate.accepted)
        .map(|candidate| DetectedBubble {
            rect: candidate.rect,
        })
        .collect()
}

fn detect_group(
    image: &DynamicImage,
    config: &DetectionConfig,
    initial_top: i32,
    initial_bottom: i32,
) -> Option<BubbleCandidate> {
    let left = (config.anchor_left - config.left_padding).max(config.search_rect.x);
    let provisional_right = detect_right_edge(image, config, initial_top, initial_bottom)?;
    let (top, bottom) = trim_false_vertical_bridge(
        image,
        config,
        left,
        provisional_right,
        initial_top,
        initial_bottom,
    );
    let right = detect_right_edge(image, config, top, bottom)?;
    let height = bottom - top + 1;
    let width = right - left + 1;

    let rect = Rect::new(left, top, width as u32, height as u32);
    let dark_pixels = (rect.y..rect.bottom())
        .flat_map(|y| (rect.x..rect.right()).map(move |x| (x, y)))
        .filter(|(x, y)| is_incoming_pixel(image.get_pixel(*x as u32, *y as u32).0))
        .count();
    let dark_fill_ratio = dark_pixels as f32 / (rect.width * rect.height).max(1) as f32;
    let size_valid = (config.min_width..=config.max_width).contains(&width)
        && (config.min_height..=config.max_height).contains(&height);
    let clear_of_boundary = top - config.search_rect.y >= config.boundary_margin
        && config.search_rect.bottom() - bottom > config.boundary_margin;
    Some(BubbleCandidate {
        rect,
        accepted: size_valid && clear_of_boundary && dark_fill_ratio >= config.min_dark_fill_ratio,
    })
}

fn detect_right_edge(
    image: &DynamicImage,
    config: &DetectionConfig,
    top: i32,
    bottom: i32,
) -> Option<i32> {
    let height = bottom - top + 1;
    let min_column_pixels = (height as usize).div_ceil(4);
    let column_support = |x: i32| {
        (top..=bottom)
            .filter(|y| is_incoming_pixel(image.get_pixel(x as u32, *y as u32).0))
            .count()
    };
    let scan_start = (config.anchor_left.max(config.search_rect.x)
        ..config.anchor_right.min(config.search_rect.right()))
        .find(|x| column_support(*x) >= min_column_pixels)?;
    let mut right = scan_start;
    let mut gap = 0;
    for x in scan_start + 1..config.search_rect.right() {
        if column_support(x) >= min_column_pixels {
            right = x;
            gap = 0;
        } else {
            gap += 1;
            if gap > MAX_RIGHT_EDGE_GAP {
                break;
            }
        }
    }
    Some(right)
}

fn trim_false_vertical_bridge(
    image: &DynamicImage,
    config: &DetectionConfig,
    left: i32,
    right: i32,
    top: i32,
    bottom: i32,
) -> (i32, i32) {
    let row_support = (top..=bottom)
        .map(|y| {
            (left..=right)
                .filter(|x| is_incoming_pixel(image.get_pixel(*x as u32, y as u32).0))
                .count()
        })
        .collect::<Vec<_>>();
    let Some(max_support) = row_support.iter().copied().max() else {
        return (top, bottom);
    };
    let strong_threshold = (max_support * STRONG_ROW_PERCENT).div_ceil(100);
    let strong_rows = row_support
        .iter()
        .enumerate()
        .filter_map(|(index, support)| (*support >= strong_threshold).then_some(index))
        .collect::<Vec<_>>();
    let Some(first) = strong_rows.first().copied() else {
        return (top, bottom);
    };

    let mut clusters = Vec::new();
    let mut cluster_start = first;
    let mut previous = first;
    for current in strong_rows.into_iter().skip(1) {
        if current - previous - 1 > MAX_INTERNAL_WEAK_ROWS {
            clusters.push((cluster_start, previous));
            cluster_start = current;
        }
        previous = current;
    }
    clusters.push((cluster_start, previous));
    if clusters.len() == 1 {
        return (top, bottom);
    }

    let selected = clusters
        .iter()
        .enumerate()
        .max_by_key(|(index, (start, end))| {
            let candidate_top = if *index == 0 { 0 } else { *start };
            let candidate_bottom = if *index + 1 == clusters.len() {
                row_support.len() - 1
            } else {
                *end
            };
            let height = candidate_bottom - candidate_top + 1;
            (height >= config.min_height as usize, end - start + 1)
        })
        .map(|(index, cluster)| (index, *cluster));
    let Some((index, (start, end))) = selected else {
        return (top, bottom);
    };
    let selected_top = if index == 0 { top } else { top + start as i32 };
    let selected_bottom = if index + 1 == clusters.len() {
        bottom
    } else {
        top + end as i32
    };
    (selected_top, selected_bottom)
}

pub(crate) fn is_incoming_pixel(pixel: [u8; 4]) -> bool {
    let [red, green, blue, _] = pixel;
    (58..=72).contains(&red)
        && (64..=82).contains(&green)
        && (76..=104).contains(&blue)
        && blue >= red.saturating_add(14)
}
