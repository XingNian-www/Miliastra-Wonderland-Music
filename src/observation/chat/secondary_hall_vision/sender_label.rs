use image::{DynamicImage, GenericImageView};

use super::detection::{DetectedBubble, Rect};

const AVATAR_BOUNDARY_STEP_DEGREES: i32 = 5;
const AVATAR_BOUNDARY_COLOR_DELTA: u8 = 20;
const AVATAR_TEXTURE_COLOR_DELTA: u8 = 24;
const MAX_AVATAR_ANCHOR_OFFSET: i32 = 6;
const MIN_VISIBLE_AVATAR_BOUNDARY_SAMPLES: usize = 24;
const SENDER_MAX_FULLWIDTH_CHARS: u32 = 18;
const SENDER_FULLWIDTH_CHAR_WIDTH: u32 = 21;
const SENDER_HORIZONTAL_PADDING: u32 = 8;

#[derive(Clone, Copy, Debug)]
struct AvatarAnchorScore {
    line_y: i32,
    boundary_edges: usize,
    visible_samples: usize,
    texture_edges: usize,
    visible_texture_samples: usize,
    boundary_supported: bool,
}

#[derive(Clone, Copy, Debug)]
struct AvatarTextureSampleRow {
    relative_y: i32,
    left_index: usize,
    right_index: usize,
}

struct AvatarTextureScan {
    row_prefixes: Vec<usize>,
    prefix_width: usize,
    visible_bottom: i32,
    sample_rows: Vec<AvatarTextureSampleRow>,
    total_samples: usize,
}

impl AvatarTextureScan {
    fn new(image: &DynamicImage, config: &SenderLabelConfig) -> Self {
        let size = config.avatar_size as i32;
        let center_x = config.avatar_left + size / 2;
        let radius_squared = (size / 2).pow(2);
        let x_coordinates = (config.avatar_left.max(0)
            ..(config.avatar_left + size).min(image.width() as i32 - 1))
            .step_by(2)
            .collect::<Vec<_>>();
        let sample_rows = (0..size)
            .step_by(2)
            .filter_map(|relative_y| {
                let dy = relative_y - size / 2;
                let mut supported = x_coordinates.iter().enumerate().filter_map(|(index, x)| {
                    let dx = *x - center_x;
                    (dx * dx + dy * dy <= radius_squared
                        && (dx + 1) * (dx + 1) + dy * dy <= radius_squared
                        && dx * dx + (dy + 1) * (dy + 1) <= radius_squared)
                        .then_some(index)
                });
                let left_index = supported.next()?;
                let right_index = supported.next_back().unwrap_or(left_index) + 1;
                Some(AvatarTextureSampleRow {
                    relative_y,
                    left_index,
                    right_index,
                })
            })
            .collect::<Vec<_>>();
        let total_samples = sample_rows
            .iter()
            .map(|row| row.right_index - row.left_index)
            .sum();
        let visible_bottom = config.search_rect.bottom().min(image.height() as i32);
        let prefix_width = x_coordinates.len() + 1;
        let mut row_prefixes = vec![0; visible_bottom.max(0) as usize * prefix_width];
        for y in 0..(visible_bottom - 1).max(0) {
            let row_start = y as usize * prefix_width;
            for (index, x) in x_coordinates.iter().enumerate() {
                let pixel = image.get_pixel(*x as u32, y as u32).0;
                let right = image.get_pixel((*x + 1) as u32, y as u32).0;
                let below = image.get_pixel(*x as u32, (y + 1) as u32).0;
                let is_edge = color_delta(pixel, right).max(color_delta(pixel, below))
                    >= AVATAR_TEXTURE_COLOR_DELTA;
                row_prefixes[row_start + index + 1] =
                    row_prefixes[row_start + index] + usize::from(is_edge);
            }
        }
        Self {
            row_prefixes,
            prefix_width,
            visible_bottom,
            sample_rows,
            total_samples,
        }
    }

    fn observation(&self, line_y: i32, config: &SenderLabelConfig) -> (usize, usize) {
        let top = line_y + config.avatar_top_offset;
        self.sample_rows
            .iter()
            .filter_map(|sample_row| {
                let y = top + sample_row.relative_y;
                if y < 0 || y >= self.visible_bottom - 1 {
                    return None;
                }
                let row_start = y as usize * self.prefix_width;
                let edges = self.row_prefixes[row_start + sample_row.right_index]
                    - self.row_prefixes[row_start + sample_row.left_index];
                Some((edges, sample_row.right_index - sample_row.left_index))
            })
            .fold((0, 0), |(edges, samples), observation| {
                (edges + observation.0, samples + observation.1)
            })
    }
}

#[derive(Clone, Debug)]
pub(super) struct SenderLabelConfig {
    search_rect: Rect,
    avatar_left: i32,
    avatar_size: u32,
    avatar_top_offset: i32,
    min_avatar_boundary_edges: usize,
    min_avatar_texture_edges: usize,
    sender_left: i32,
    sender_width: u32,
    sender_top_offset: i32,
    sender_height: u32,
}

impl Default for SenderLabelConfig {
    fn default() -> Self {
        Self {
            search_rect: Rect::new(410, 87, 420, 870),
            avatar_left: 302,
            avatar_size: 88,
            avatar_top_offset: 4,
            min_avatar_boundary_edges: 24,
            min_avatar_texture_edges: 250,
            sender_left: 410,
            sender_width: SENDER_MAX_FULLWIDTH_CHARS * SENDER_FULLWIDTH_CHAR_WIDTH
                + SENDER_HORIZONTAL_PADDING * 2,
            sender_top_offset: -10,
            sender_height: 36,
        }
    }
}

#[derive(Clone, Debug)]
struct DetectedAvatarAnchor {
    rect: Rect,
}

#[derive(Clone, Debug)]
pub(super) struct DetectedSenderLabel {
    pub(super) avatar_rect: Rect,
    pub(super) sender_rect: Rect,
    pub(super) bubble_indices: Vec<usize>,
}

pub(super) fn segment_sender_labels(
    image: &DynamicImage,
    bubbles: &[DetectedBubble],
    config: &SenderLabelConfig,
) -> Vec<DetectedSenderLabel> {
    let avatar_anchors = detect_avatar_anchors(image, config);
    associate_bubbles(image, bubbles, config, &avatar_anchors)
}

fn detect_avatar_anchors(
    image: &DynamicImage,
    config: &SenderLabelConfig,
) -> Vec<DetectedAvatarAnchor> {
    let boundary_offsets = avatar_boundary_offsets(config);
    let total_boundary_samples = boundary_offsets.len();
    let texture_scan = AvatarTextureScan::new(image, config);
    let last_line_y =
        config.search_rect.bottom() - config.avatar_top_offset - (config.avatar_size as i32 / 4);
    let observations = (config.search_rect.y..=last_line_y)
        .map(|line_y| {
            let (boundary_edges, visible_samples) =
                avatar_boundary_observation(image, line_y, config, &boundary_offsets);
            let boundary_supported = visible_samples >= MIN_VISIBLE_AVATAR_BOUNDARY_SAMPLES
                && boundary_edges
                    >= scaled_boundary_threshold(config, visible_samples, total_boundary_samples);
            let (texture_edges, visible_texture_samples) = texture_scan.observation(line_y, config);
            let texture_supported = visible_texture_samples > 0
                && texture_edges
                    >= scaled_texture_threshold(
                        config,
                        visible_texture_samples,
                        texture_scan.total_samples,
                    );
            (
                AvatarAnchorScore {
                    line_y,
                    boundary_edges,
                    visible_samples,
                    texture_edges,
                    visible_texture_samples,
                    boundary_supported,
                },
                texture_supported,
            )
        })
        .collect::<Vec<_>>();
    let mut texture_scores = Vec::new();
    let mut run_start = None;
    for index in 0..=observations.len() {
        let supported = observations
            .get(index)
            .is_some_and(|(_, texture_supported)| *texture_supported);
        if supported {
            run_start.get_or_insert(index);
        } else if let Some(start) = run_start.take() {
            texture_scores.extend(localized_texture_run_scores(
                &observations,
                start,
                index,
                config,
                texture_scan.total_samples,
            ));
        }
    }

    let minimum_separation = config.avatar_size as i32;
    let mut boundary_scores = observations
        .iter()
        .map(|(score, _)| *score)
        .filter(|score| score.boundary_supported)
        .collect::<Vec<_>>();
    boundary_scores.sort_by(|left, right| {
        let left_normalized = left.boundary_edges * total_boundary_samples / left.visible_samples;
        let right_normalized =
            right.boundary_edges * total_boundary_samples / right.visible_samples;
        right_normalized
            .cmp(&left_normalized)
            .then_with(|| left.line_y.cmp(&right.line_y))
    });
    let mut selected = Vec::new();
    for score in boundary_scores {
        if selected.iter().any(|selected_score: &AvatarAnchorScore| {
            (score.line_y - selected_score.line_y).abs() < minimum_separation
        }) {
            continue;
        }
        selected.push(score);
    }

    texture_scores.sort_by(|left, right| {
        let left_key = avatar_texture_key(left, texture_scan.total_samples);
        let right_key = avatar_texture_key(right, texture_scan.total_samples);
        right_key
            .cmp(&left_key)
            .then_with(|| left.line_y.cmp(&right.line_y))
    });
    let mut selected_texture = Vec::new();
    for score in texture_scores {
        if selected_texture
            .iter()
            .any(|selected_score: &AvatarAnchorScore| {
                (score.line_y - selected_score.line_y).abs() < minimum_separation
            })
        {
            continue;
        }
        selected_texture.push(score);
    }
    for texture_score in selected_texture {
        let overlapping_boundary = selected
            .iter()
            .enumerate()
            .filter(|(_, boundary_score)| {
                (texture_score.line_y - boundary_score.line_y).abs() < minimum_separation
            })
            .min_by_key(|(_, boundary_score)| (texture_score.line_y - boundary_score.line_y).abs());
        if let Some((index, boundary_score)) = overlapping_boundary {
            let strong_texture_threshold = scaled_texture_threshold(
                config,
                texture_score.visible_texture_samples,
                texture_scan.total_samples,
            )
            // Replacing a circular peak requires texture confidence at least as
            // high as a perfect boundary observation under the current config.
            .saturating_mul(total_boundary_samples)
            .div_ceil(config.min_avatar_boundary_edges);
            if (texture_score.line_y - boundary_score.line_y).abs() > MAX_AVATAR_ANCHOR_OFFSET
                && texture_score.texture_edges >= strong_texture_threshold
            {
                selected[index] = texture_score;
            }
        } else {
            selected.push(texture_score);
        }
    }
    selected.sort_by_key(|score| score.line_y);
    selected
        .into_iter()
        .map(|score| DetectedAvatarAnchor {
            rect: avatar_rect(score.line_y, config),
        })
        .collect()
}

fn localized_texture_run_scores(
    observations: &[(AvatarAnchorScore, bool)],
    run_start: usize,
    run_end: usize,
    config: &SenderLabelConfig,
    total_texture_samples: usize,
) -> Vec<AvatarAnchorScore> {
    let peak_radius = (config.avatar_size as usize / 2).max(1);
    let mut peak_plateaus = Vec::new();
    let mut plateau_start = None;
    for index in run_start..=run_end {
        let is_peak = if index == run_end {
            false
        } else {
            let window_start = index.saturating_sub(peak_radius).max(run_start);
            let window_end = (index + peak_radius + 1).min(run_end);
            let response =
                normalized_texture_response(&observations[index].0, total_texture_samples);
            let window_max = observations[window_start..window_end]
                .iter()
                .map(|(score, _)| normalized_texture_response(score, total_texture_samples))
                .max()
                .unwrap_or_default();
            let left_valley = observations[window_start..index]
                .iter()
                .map(|(score, _)| normalized_texture_response(score, total_texture_samples))
                .min();
            let right_valley = observations[index + 1..window_end]
                .iter()
                .map(|(score, _)| normalized_texture_response(score, total_texture_samples))
                .min();
            response == window_max
                && left_valley.is_none_or(|valley| response > valley)
                && right_valley.is_none_or(|valley| response > valley)
        };
        if is_peak {
            plateau_start.get_or_insert(index);
        } else if let Some(start) = plateau_start.take() {
            peak_plateaus.push(start + (index - start - 1) / 2);
        }
    }

    peak_plateaus.sort_by(|left, right| {
        normalized_texture_response(&observations[*right].0, total_texture_samples)
            .cmp(&normalized_texture_response(
                &observations[*left].0,
                total_texture_samples,
            ))
            .then_with(|| left.cmp(right))
    });
    let minimum_separation = config.avatar_size as usize;
    let mut peaks = Vec::new();
    for peak in peak_plateaus {
        if peaks
            .iter()
            .any(|selected: &usize| peak.abs_diff(*selected) < minimum_separation)
        {
            continue;
        }
        peaks.push(peak);
    }
    peaks.sort_unstable();
    if peaks.is_empty() {
        return Vec::new();
    }

    let valleys = peaks
        .windows(2)
        .map(|pair| {
            let minimum = observations[pair[0]..=pair[1]]
                .iter()
                .map(|(score, _)| normalized_texture_response(score, total_texture_samples))
                .min()
                .unwrap_or_default();
            let mut valley_indices = (pair[0]..=pair[1]).filter(|index| {
                normalized_texture_response(&observations[*index].0, total_texture_samples)
                    == minimum
            });
            let first = valley_indices.next().unwrap_or(pair[0]);
            let last = valley_indices.next_back().unwrap_or(first);
            first + (last - first) / 2
        })
        .collect::<Vec<_>>();
    let maximum_lobe_width = config.avatar_size.saturating_mul(2) as usize;
    peaks
        .iter()
        .enumerate()
        .filter_map(|(position, _)| {
            let left = position
                .checked_sub(1)
                .map_or(run_start, |index| valleys[index]);
            let right = valleys.get(position).copied().unwrap_or(run_end - 1);
            (right - left < maximum_lobe_width).then(|| observations[left + (right - left) / 2].0)
        })
        .collect()
}

fn avatar_rect(line_y: i32, config: &SenderLabelConfig) -> Rect {
    Rect::new(
        config.avatar_left,
        line_y + config.avatar_top_offset,
        config.avatar_size,
        config.avatar_size,
    )
}

fn associate_bubbles(
    image: &DynamicImage,
    bubbles: &[DetectedBubble],
    config: &SenderLabelConfig,
    avatar_anchors: &[DetectedAvatarAnchor],
) -> Vec<DetectedSenderLabel> {
    avatar_anchors
        .iter()
        .enumerate()
        .filter_map(|(position, anchor)| {
            let sender_top = (anchor.rect.y + config.sender_top_offset).max(0);
            let sender_right =
                (config.sender_left + config.sender_width as i32).min(image.width() as i32);
            let sender_bottom =
                (sender_top + config.sender_height as i32).min(image.height() as i32);
            if sender_right <= config.sender_left || sender_bottom <= sender_top {
                return None;
            }
            let sender_rect = Rect::new(
                config.sender_left,
                sender_top,
                (sender_right - config.sender_left) as u32,
                (sender_bottom - sender_top) as u32,
            );
            let first_bubble_y = sender_rect.bottom();
            let next_label_y = avatar_anchors
                .get(position + 1)
                .map(|anchor| anchor.rect.y + config.sender_top_offset)
                .unwrap_or(config.search_rect.bottom());
            let bubble_indices = bubbles
                .iter()
                .enumerate()
                .filter(|(_, bubble)| {
                    bubble.rect.y >= first_bubble_y && bubble.rect.y < next_label_y
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            (!bubble_indices.is_empty()).then_some(DetectedSenderLabel {
                avatar_rect: anchor.rect,
                sender_rect,
                bubble_indices,
            })
        })
        .collect()
}

fn avatar_boundary_observation(
    image: &DynamicImage,
    line_y: i32,
    config: &SenderLabelConfig,
    offsets: &[(i32, i32, i32, i32)],
) -> (usize, usize) {
    let top = line_y + config.avatar_top_offset;
    let size = config.avatar_size as i32;
    let center_x = config.avatar_left + size / 2;
    let center_y = top + size / 2;
    let visible_bottom = config.search_rect.bottom().min(image.height() as i32);
    let mut edges = 0;
    let mut visible_samples = 0;
    for (inner_x, inner_y, outer_x, outer_y) in offsets {
        let Some(inner) = visible_pixel(
            image,
            center_x + inner_x,
            center_y + inner_y,
            visible_bottom,
        ) else {
            continue;
        };
        let Some(outer) = visible_pixel(
            image,
            center_x + outer_x,
            center_y + outer_y,
            visible_bottom,
        ) else {
            continue;
        };
        visible_samples += 1;
        if color_delta(inner, outer) >= AVATAR_BOUNDARY_COLOR_DELTA {
            edges += 1;
        }
    }
    (edges, visible_samples)
}

fn avatar_boundary_offsets(config: &SenderLabelConfig) -> Vec<(i32, i32, i32, i32)> {
    let size = config.avatar_size as i32;
    let inner_radius = (size / 2 - 4).max(1) as f64;
    let outer_radius = (size / 2 + 3) as f64;
    (0..360)
        .step_by(AVATAR_BOUNDARY_STEP_DEGREES as usize)
        .map(|degrees| {
            let angle = f64::from(degrees).to_radians();
            let cosine = angle.cos();
            let sine = angle.sin();
            (
                (cosine * inner_radius).round() as i32,
                (sine * inner_radius).round() as i32,
                (cosine * outer_radius).round() as i32,
                (sine * outer_radius).round() as i32,
            )
        })
        .collect()
}

fn scaled_boundary_threshold(
    config: &SenderLabelConfig,
    visible_samples: usize,
    total_samples: usize,
) -> usize {
    (config.min_avatar_boundary_edges * visible_samples).div_ceil(total_samples)
}

fn scaled_texture_threshold(
    config: &SenderLabelConfig,
    visible_samples: usize,
    total_samples: usize,
) -> usize {
    if total_samples == 0 {
        return usize::MAX;
    }
    (config.min_avatar_texture_edges * visible_samples).div_ceil(total_samples)
}

fn avatar_texture_key(score: &AvatarAnchorScore, total_texture_samples: usize) -> (usize, usize) {
    (
        normalized_texture_response(score, total_texture_samples),
        score.texture_edges,
    )
}

fn normalized_texture_response(score: &AvatarAnchorScore, total_texture_samples: usize) -> usize {
    score
        .texture_edges
        .saturating_mul(total_texture_samples)
        .checked_div(score.visible_texture_samples)
        .unwrap_or_default()
}

fn visible_pixel(image: &DynamicImage, x: i32, y: i32, visible_bottom: i32) -> Option<[u8; 4]> {
    (x >= 0 && y >= 0 && x < image.width() as i32 && y < visible_bottom)
        .then(|| image.get_pixel(x as u32, y as u32).0)
}

fn color_delta(left: [u8; 4], right: [u8; 4]) -> u8 {
    left[..3]
        .iter()
        .zip(&right[..3])
        .map(|(left, right)| left.abs_diff(*right))
        .max()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn associates_a_first_bubble_at_the_smallest_reviewed_visible_label_gap() {
        let image = DynamicImage::new_rgba8(1920, 1080);
        let config = SenderLabelConfig::default();
        let anchors = vec![
            DetectedAvatarAnchor {
                rect: Rect::new(302, 387, 88, 88),
            },
            DetectedAvatarAnchor {
                rect: Rect::new(302, 531, 88, 88),
            },
        ];
        let bubbles = vec![DetectedBubble {
            rect: Rect::new(418, 414, 629, 98),
        }];

        let labels = associate_bubbles(&image, &bubbles, &config, &anchors);

        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].bubble_indices, vec![0]);
    }

    #[test]
    fn excludes_a_bubble_whose_sender_label_is_clipped_above_the_viewport() {
        let image = DynamicImage::new_rgba8(1920, 1080);
        let config = SenderLabelConfig::default();
        let anchors = vec![
            DetectedAvatarAnchor {
                rect: Rect::new(302, 91, 88, 88),
            },
            DetectedAvatarAnchor {
                rect: Rect::new(302, 200, 88, 88),
            },
        ];
        let bubbles = vec![
            DetectedBubble {
                rect: Rect::new(418, 116, 257, 65),
            },
            DetectedBubble {
                rect: Rect::new(418, 232, 629, 97),
            },
        ];

        let labels = associate_bubbles(&image, &bubbles, &config, &anchors);

        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].bubble_indices, vec![1]);
    }
}
