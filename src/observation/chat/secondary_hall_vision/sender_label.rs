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
/// 发言人名字按亮像素收紧时的相对亮度阈值（高于本行中位数的量）。
const SENDER_TEXT_LUMINANCE_DELTA: u32 = 40;
/// 末尾至少这么多列「空白」才认定文字已结束（名字内部字距远小于它）。
const SENDER_TRAILING_GAP_PX: usize = 24;
/// 收紧后右端再留一点余量，避免切在最后一笔上。
const SENDER_TRAILING_PADDING_PX: i32 = 8;
/// 收紧后的最小宽度；比它更窄说明判断不可信，退回配置宽度。
///
/// 取 24 而不是「一个名字的典型宽度」：两字昵称（`昵称D`、`昵称E`）在真实帧上
/// 只有 ~40px 宽，阈值定高会把**短名**错误地判成不可信而退回 394px 固定宽，
/// 背景照样被吃进 OCR。真正的可信度由「文字列数」与「末尾空白长度」两个条件
/// 保证（见 [`tight_sender_right`]）。
const SENDER_MIN_TEXT_WIDTH_PX: u32 = 24;
/// 至少要这么多列被判为文字，才认为「这一行确实有名字」。
/// 单列亮点（噪点、边框反光）不算。
const SENDER_MIN_TEXT_COLUMNS: usize = 6;

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

// 观察线程每帧复用的大块前缀和缓冲区(约 390KB),避免反复分配。
thread_local! {
    static ROW_PREFIXES_SCRATCH: std::cell::RefCell<Vec<usize>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

struct AvatarTextureScan {
    row_prefixes: Vec<usize>,
    prefix_width: usize,
    visible_bottom: i32,
    sample_rows: Vec<AvatarTextureSampleRow>,
    total_samples: usize,
}

impl Drop for AvatarTextureScan {
    fn drop(&mut self) {
        // 把缓冲区归还线程局部缓存,供下一帧复用(保留已分配容量)。
        // 用 `<=` 而不是 `<`：取用时缓存被 `mem::take` 清成零容量，若这里只认
        // 「缓存更小」，容量相等的归还永远不发生，第二帧之后每帧都要重新分配。
        // 归还前清空长度，让下一帧的 `resize` 正常补零，避免残留上一帧的旧值。
        ROW_PREFIXES_SCRATCH.with(|cell| {
            let mut scratch = cell.borrow_mut();
            if scratch.capacity() <= self.row_prefixes.capacity() {
                self.row_prefixes.clear();
                *scratch = std::mem::take(&mut self.row_prefixes);
            }
        });
    }
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
        let needed = visible_bottom.max(0) as usize * prefix_width;
        // `mem::take` 直接转让这块堆内存；`drain(..).collect()` 会新建一个 Vec
        // 再把元素逐个数拷过去，线程局部缓存里的容量反而永远用不上。
        let mut row_prefixes: Vec<usize> =
            ROW_PREFIXES_SCRATCH.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
        row_prefixes.resize(needed, 0);
        for y in 0..(visible_bottom - 1).max(0) {
            let row_start = y as usize * prefix_width;
            // resize 复用旧值时行首必须清零(前缀和基线)。
            row_prefixes[row_start] = 0;
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
            // 替换圆形峰值时，纹理置信度至少要达到当前配置下完整边界观测的水平。
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
            // 头像贴到视口顶部时，它**上方**那行名字会被裁掉：读出来的名字不可信，
            // 一旦拿它去归属气泡就会产生错误署名（比没有署名更糟——权限/歌单会记到
            // 别人头上）。这种头像整体跳过，它的气泡保持无署名。
            //
            // 判定用**未裁剪**的标签顶：一旦被 `.max(0)` 夹到 0 就分不清「本来就在顶部」
            // 和「被裁掉了」。
            let sender_top_raw = anchor.rect.y + config.sender_top_offset;
            if sender_top_raw < config.search_rect.y {
                return None;
            }
            let sender_top = sender_top_raw.max(0);
            let configured_right =
                (config.sender_left + config.sender_width as i32).min(image.width() as i32);
            let sender_bottom =
                (sender_top + config.sender_height as i32).min(image.height() as i32);
            if configured_right <= config.sender_left || sender_bottom <= sender_top {
                return None;
            }
            // 固定宽度会吃进右侧 3D 场景：场景里的名牌同样是白色文字，OCR 会把
            // `2#BOT|勿加` 读成 `2#BOT|勿加安娜`（README 人工核验过）。按本行
            // 实际亮像素把右边界收紧到文字末尾，读不到可信文字时**保持原宽**
            // （宁可多吃背景，也不能把窄名字裁没）。
            let sender_right =
                tight_sender_right(image, config, sender_top, sender_bottom, configured_right);
            let sender_rect = Rect::new(
                config.sender_left,
                sender_top,
                (sender_right - config.sender_left) as u32,
                (sender_bottom - sender_top) as u32,
            );
            // 归属窗口 = `[本头像顶, 下一个头像顶)`，**不是**「标签框底 → 下一个标签框顶」。
            //
            // 标签框是固定的 36px 高，而名字实际只占约 20px，所以「标签框底」
            // （= 头像顶 + 26px）落在**头像内部**。游戏排版里每组的第一条气泡紧贴
            // 头像下沿，气泡顶常常只比头像顶低 10~15px，正好被这个下界排除：
            // 实测真图组0 气泡顶 193 / 窗口起点 207（差 14px）→ 首条气泡永远丢署名，
            // 而头像证据本身是充足的（boundary 72/24、texture 588/250）。
            //
            // 用头像顶做下界后，相邻两组首尾相接、不重不漏；比该头像更早的气泡
            // 仍归上一组（或第一组之外不归属），不会串到别人头上。
            let first_bubble_y = anchor.rect.y;
            let next_label_y = avatar_anchors
                .get(position + 1)
                .map(|anchor| anchor.rect.y)
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

/// 按**本行实际亮像素**把发言人名字的右边界收紧到文字末尾。
///
/// 背景：`sender_width` 是 18 个全角字符的固定预算（394px）。名字短时，右端会
/// 一路吃进 3D 场景；场景里的白色名牌同样是高亮文字，OCR 会把
/// `2#BOT|勿加` 读成 `2#BOT|勿加安娜`（README:110 人工核验）。
///
/// 判据与界：
/// - 用本行中位数亮度做自适应阈值（`+SENDER_TEXT_LUMINANCE_DELTA`），不假设绝对亮度；
/// - 只在**末尾连续空白之后**裁，且只在空白宽度足够大
///   （`SENDER_TRAILING_GAP_PX`）时才裁——名字内部字间距、名字末尾的小图标都不会被切；
/// - 任何异常（整行读不到文字、裁完过窄、裁完只剩很小一段）都**保持原宽**：
///   宁可多吃背景，也绝不把窄名字裁没。
fn tight_sender_right(
    image: &DynamicImage,
    config: &SenderLabelConfig,
    top: i32,
    bottom: i32,
    configured_right: i32,
) -> i32 {
    let left = config.sender_left;
    if configured_right <= left || bottom <= top {
        return configured_right;
    }
    let mut luminance = Vec::with_capacity((configured_right - left) as usize);
    for x in left..configured_right {
        let mut sum = 0u32;
        let mut rows = 0u32;
        for y in top..bottom {
            let pixel = image.get_pixel(x as u32, y as u32).0;
            // Rec.601 亮度；只用于「亮/暗」二分，不需要色彩精度。
            let luma =
                (u32::from(pixel[0]) * 299 + u32::from(pixel[1]) * 587 + u32::from(pixel[2]) * 114)
                    / 1000;
            sum += luma;
            rows += 1;
        }
        luminance.push(if rows == 0 { 0 } else { sum / rows });
    }
    let mut sorted = luminance.clone();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    let threshold = median.saturating_add(SENDER_TEXT_LUMINANCE_DELTA);
    let is_text: Vec<bool> = luminance.iter().map(|value| *value > threshold).collect();
    let Some(last_text) = is_text.iter().rposition(|is_text| *is_text) else {
        // 整行没有可信文字：保持原宽，让 OCR 去处理（可能整段是低对比文字）。
        return configured_right;
    };
    let text_columns = is_text.iter().filter(|is_text| **is_text).count();
    if text_columns < SENDER_MIN_TEXT_COLUMNS {
        return configured_right;
    }
    let trailing_gap = is_text.len() - 1 - last_text;
    if trailing_gap < SENDER_TRAILING_GAP_PX {
        return configured_right;
    }
    let candidate = left + last_text as i32 + SENDER_TRAILING_PADDING_PX;
    if candidate - left < SENDER_MIN_TEXT_WIDTH_PX as i32 {
        return configured_right;
    }
    candidate.min(configured_right)
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
    fn associates_a_bubble_that_sits_just_below_its_avatar() {
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

    /// 回归：每组**第一条**气泡紧贴头像下沿时也必须拿到署名。
    ///
    /// 旧实现把归属窗口下界设成「标签框底」= 头像顶 + 26px。标签框固定 36px 高、
    /// 名字实际只占约 20px，于是这个下界落在**头像内部**；而游戏排版里每组第一条
    /// 气泡的顶边通常只比头像顶低 10~15px → 永远落在窗口之前、永远没有署名。
    /// 真图实测：头像 y=181 → 窗口起点 207，气泡顶 193（差 14px）→ 丢署名；
    /// 而同一帧头像证据是充足的（boundary 72/24、texture 588/250），
    /// 所以线上那条「头像证据不足」的告警其实一直在误导排查方向。
    #[test]
    fn first_bubble_touching_the_avatar_still_gets_its_sender() {
        let image = DynamicImage::new_rgba8(1920, 1080);
        let config = SenderLabelConfig::default();
        let anchors = vec![
            DetectedAvatarAnchor {
                rect: Rect::new(302, 181, 88, 88),
            },
            DetectedAvatarAnchor {
                rect: Rect::new(302, 367, 88, 88),
            },
        ];
        let bubbles = vec![
            // 紧贴头像下沿（旧实现下界 207，这里 193 → 差 14px 落空）
            DetectedBubble {
                rect: Rect::new(410, 193, 284, 157),
            },
            // 第二组：正常间距
            DetectedBubble {
                rect: Rect::new(410, 400, 256, 65),
            },
        ];

        let labels = associate_bubbles(&image, &bubbles, &config, &anchors);

        assert_eq!(labels.len(), 2, "两个头像都应各自关联到气泡");
        assert_eq!(
            labels[0].bubble_indices,
            vec![0],
            "第一条气泡属于第一个头像"
        );
        assert_eq!(
            labels[1].bubble_indices,
            vec![1],
            "第二条气泡属于第二个头像"
        );
    }

    /// 边界安全：比第一个头像更早的气泡不归属任何人，绝不串到第一个发言人头上。
    #[test]
    fn a_bubble_above_the_first_avatar_is_never_attributed_to_it() {
        let image = DynamicImage::new_rgba8(1920, 1080);
        let config = SenderLabelConfig::default();
        let anchors = vec![DetectedAvatarAnchor {
            rect: Rect::new(302, 181, 88, 88),
        }];
        // 头像之上（画面顶部那条头像被搜索区裁掉的消息）
        let bubbles = vec![DetectedBubble {
            rect: Rect::new(410, 110, 300, 60),
        }];

        let labels = associate_bubbles(&image, &bubbles, &config, &anchors);

        assert!(
            labels.is_empty(),
            "头像之上没有锚点的气泡不得归给该发言人：{labels:?}"
        );
    }

    #[test]
    fn avatar_texture_scan_reuses_the_thread_local_prefix_buffer_across_frames() {
        let image = DynamicImage::new_rgba8(1920, 1080);
        let config = SenderLabelConfig::default();

        let first = AvatarTextureScan::new(&image, &config);
        let first_ptr = first.row_prefixes.as_ptr();
        let first_capacity = first.row_prefixes.capacity();
        assert!(first_capacity > 0);
        drop(first);

        let cached_capacity = ROW_PREFIXES_SCRATCH.with(|cell| cell.borrow().capacity());
        assert!(
            cached_capacity >= first_capacity,
            "扫描结束必须把缓冲区归还线程局部缓存，否则每帧都要重新分配"
        );

        let second = AvatarTextureScan::new(&image, &config);
        assert_eq!(
            second.row_prefixes.as_ptr(),
            first_ptr,
            "第二帧必须复用第一帧的缓冲区，不能重新分配"
        );
        assert_eq!(second.row_prefixes.capacity(), first_capacity);

        // 连续多帧仍然复用同一块内存。
        drop(second);
        let third = AvatarTextureScan::new(&image, &config);
        assert_eq!(third.row_prefixes.as_ptr(), first_ptr);
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
