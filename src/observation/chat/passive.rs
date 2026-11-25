//! Cross-platform passive chat layout recognition.
//!
//! Production layout/identity layer used by the shared chat observer. It lives in
//! `observation::chat` (not `composition`) so the manifest replay tests can build and run
//! on Linux; `composition` stays Windows-only. The moved code is unchanged.

use std::hash::{Hash, Hasher};

use anyhow::Result;
use image::DynamicImage;

use super::{
    ChatIdentity, RecognizedChatMessage, ResolvedTemplateArgs, SecondaryHallBubble,
    latest_incoming_bubble_rect, scan_chat_markers, secondary_hall_bubbles,
};
use crate::config::AppConfig;
use crate::geometry::{Rect, crop_canvas};
use crate::runtime::ocr::{OcrPriority, OcrRuntimeHandle};
use crate::runtime::ui::UiStateKind;
use crate::ui::frame::Frame;
use crate::ui::template::best_template_hit;

pub(crate) const SECONDARY_HALL_FALLBACK_SENDER: &str = "二级大厅";
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SecondarySenderIdentity {
    pub(crate) nickname: String,
    pub(crate) remark: Option<String>,
}

impl SecondarySenderIdentity {
    pub(crate) fn effective_name(&self) -> &str {
        self.remark
            .as_deref()
            .filter(|remark| !remark.trim().is_empty())
            .unwrap_or(&self.nickname)
    }
}

/// 解析发言人名字里的「游戏内备注」：`昵称(备注)` / `昵称（备注）`。
///
/// **括号必须跨类型配对**：半角 `()` 与全角 `（）` 在游戏里是同一个语义，OCR 却经常
/// 一半读半角一半读全角（真实样本 `昵称A(备注A）`）。旧实现把两种括号当两套
/// 独立配对，混用就解析不出备注，sender 保留整串 → 身份精确匹配落空 → 权限掉回
/// User 下界。这是**稳定失败**（不会自愈）：同一玩家的名字每次都这样读。
///
/// 因此先找最右侧的**任意**闭括号，再从它左边找最近的**任意**开括号。
/// 只在解析层放宽，`remark_key` 的精确匹配语义不动。
pub(crate) fn parse_secondary_sender_identity(value: &str) -> SecondarySenderIdentity {
    const BRACKETS: [(char, char); 2] = [('(', ')'), ('（', '）')];
    let value = value.trim();
    // 最右侧闭括号：取所有闭括号里位置最后的那一个。
    let rightmost_close = BRACKETS
        .iter()
        .filter_map(|(_, close)| value.rfind(*close))
        .max();
    // 从它左边找最近的任意开括号（半角/全角之一定义了字符串里最靠右的那对）。
    let pair = rightmost_close.and_then(|close_index| {
        BRACKETS
            .iter()
            .filter_map(|(open, _)| {
                value[..close_index]
                    .rfind(*open)
                    .map(|open_index| (close_index, open_index, open.len_utf8()))
            })
            .max_by_key(|(_, open_index, _)| *open_index)
    });
    if let Some((close_index, open_index, open_len)) = pair {
        let remark = value[open_index + open_len..close_index].trim();
        if !remark.is_empty() {
            return SecondarySenderIdentity {
                nickname: value[..open_index].trim().to_string(),
                remark: Some(remark.to_string()),
            };
        }
    }
    SecondarySenderIdentity {
        nickname: value.to_string(),
        remark: None,
    }
}

pub(crate) enum PassiveLayout {
    Primary(Vec<crate::ui::template::TemplateHit>),
    Secondary {
        hall: bool,
        selected: Option<Rect>,
        bubbles: Vec<SecondaryHallBubble>,
        latest: Option<Rect>,
    },
    NoChat,
}

pub(crate) fn passive_layout(
    frame: &Frame,
    config: &AppConfig,
    templates: &ResolvedTemplateArgs,
) -> Result<PassiveLayout> {
    // Reuse this frame's classification even before full-screen stabilization.
    // These are two separately evidenced menus, not every Secondary/panel label.
    if passive_ui_excludes_chat(frame.ui_state.as_ref()) {
        return Ok(PassiveLayout::NoChat);
    }
    // Geometry is from this immutable frame, independent of full-screen stability.
    let markers = match frame.marker_hits_for_image() {
        Some(markers) => markers,
        None => scan_chat_markers(&frame.image, templates, config.screen.chat_rect)?,
    };
    if !markers.is_empty() {
        return Ok(PassiveLayout::Primary(markers));
    }
    let bubbles = secondary_hall_bubbles(&frame.image)?;
    let back = best_template_hit(
        &frame.image,
        Some(config.screen.secondary_back_rect.into()),
        &config.templates.secondary_back,
        config.templates.marker_threshold,
    )?
    .is_some();
    if !back && bubbles.is_empty() {
        return Ok(PassiveLayout::NoChat);
    }
    let anchor: Rect = config.screen.secondary_hall_rect.into();
    let friends: Rect = config.invite.friend_list_region.into();
    let hall_search = crate::ui::locator::secondary_hall_search_rect(anchor, friends);
    let hall_hit = best_template_hit(
        &frame.image,
        Some(hall_search),
        &config.templates.secondary_hall,
        config.templates.marker_threshold,
    )?;
    let selected = select_conversation_row(
        &frame.image,
        passive_sidebar_region(config),
        hall_hit.as_ref(),
        friends,
    );
    // A visible hall shortcut does not mean it is the selected conversation.
    let hall = selected
        .zip(hall_hit.as_ref())
        .is_some_and(|(row, hit)| (row.y..row.bottom()).contains(&hit.center().y));
    let latest = latest_incoming_bubble_rect(&frame.image);
    let classified_secondary = matches!(frame.ui_state.as_ref(),
        Some(crate::runtime::ui::UiStateObservation::Classified(state))
            if state.classification().kind() == UiStateKind::Secondary);
    // The back button is shared with menus; it alone does not prove chat geometry.
    if !hall && bubbles.is_empty() && latest.is_none() && !classified_secondary {
        return Ok(PassiveLayout::NoChat);
    }
    Ok(PassiveLayout::Secondary {
        hall,
        selected: selected.map(|row| Rect::new(friends.x, row.y, friends.width, row.height)),
        bubbles,
        latest,
    })
}

impl PassiveLayout {
    /// 旧的变化签名（整段聊天区 + marker 几何的 u64 哈希）。
    /// 2026-10-17 起门控改用 `PassiveScanGate` 的阈值判据，这里只剩测试在用，保留作对照。
    #[allow(dead_code)]
    pub(crate) fn signature(&self, image: &DynamicImage, primary_rect: Rect) -> Result<u64> {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        match self {
            Self::NoChat => 0_u8.hash(&mut hash),
            Self::Primary(markers) => {
                1_u8.hash(&mut hash);
                for marker in markers {
                    marker.kind.hash(&mut hash);
                    (marker.x, marker.y, marker.width, marker.height).hash(&mut hash);
                }
                hash_region(image, primary_rect, &mut hash)?;
            }
            Self::Secondary {
                hall,
                selected,
                bubbles,
                latest,
            } => {
                2_u8.hash(&mut hash);
                hall.hash(&mut hash);
                selected.is_some().hash(&mut hash);
                if let Some(row) = selected {
                    hash_region(image, *row, &mut hash)?;
                }
                if *hall || !bubbles.is_empty() {
                    for bubble in bubbles {
                        hash_region(image, bubble.rect, &mut hash)?;
                        // 发言人区域可能缺失（气泡在、署名证据不足）：缺就只哈希正文，
                        // 不能因为「没有署名」让整帧指纹计算失败。
                        if let Some(sender_rect) = bubble.sender_rect() {
                            hash_region(image, sender_rect, &mut hash)?;
                        }
                    }
                } else if let Some(rect) = latest {
                    hash_region(image, *rect, &mut hash)?;
                }
            }
        }
        Ok(hash.finish())
    }
}

pub(crate) fn hash_region(image: &DynamicImage, rect: Rect, hash: &mut impl Hasher) -> Result<()> {
    (rect.x, rect.y, rect.width, rect.height).hash(hash);
    let right = i64::from(rect.x) + i64::from(rect.width);
    let bottom = i64::from(rect.y) + i64::from(rect.height);
    if rect.x < 0
        || rect.y < 0
        || right > i64::from(image.width())
        || bottom > i64::from(image.height())
    {
        anyhow::bail!(
            "crop rect {},{},{},{} outside image {}x{}",
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            image.width(),
            image.height()
        );
    }
    let bytes_per_pixel = usize::from(image.color().bytes_per_pixel());
    let row_bytes = rect.width as usize * bytes_per_pixel;
    // Preserve the byte-slice Hash length prefix, then stream the same row-major bytes.
    (row_bytes * rect.height as usize).hash(hash);
    if row_bytes == 0 || rect.height == 0 {
        return Ok(());
    }
    let stride = image.width() as usize * bytes_per_pixel;
    let bytes = image.as_bytes();
    for y in rect.y as usize..bottom as usize {
        let start = y * stride + rect.x as usize * bytes_per_pixel;
        hash.write(&bytes[start..start + row_bytes]);
    }
    Ok(())
}

pub(crate) fn passive_ui_excludes_chat(
    observation: Option<&crate::runtime::ui::UiStateObservation>,
) -> bool {
    let Some(crate::runtime::ui::UiStateObservation::Classified(state)) = observation else {
        return false;
    };
    let classification = state.classification();
    match classification.kind() {
        UiStateKind::MainMenu => matches!(
            classification.evidence().final_rule(),
            "main_menu_template" | "paimon_menu_grid"
        ),
        UiStateKind::Secondary => matches!(
            classification.label(),
            "secondary:qianxing_panel" | "secondary:qianxing_map"
        ),
        _ => false,
    }
}

pub(crate) fn passive_sidebar_region(config: &AppConfig) -> Rect {
    let friends: Rect = config.invite.friend_list_region.into();
    // Scrolling can move the selected friend above the active search's y=280.
    // Include the whole sidebar below Back, while retaining its configured width.
    let back: Rect = config.screen.secondary_back_rect.into();
    let top = back.bottom();
    Rect::new(
        friends.x.saturating_sub(70),
        top,
        friends.width.saturating_add(110),
        (friends.bottom() - top).max(0) as u32,
    )
}

/// 从侧栏亮带里挑「最可能是当前选中会话」的那一条（**只读**，不点击）。
///
/// 旧实现要求**恰好一条**亮带，多一条就 `None` → `ChatIdentity::Unknown` →
/// 命令被身份闸丢弃、未读任务永远等不到 `Friend(_)` 而死循环。侧栏行数多、
/// 有红点行、头像亮时出现 ≥2 条亮带是常态，所以这里多候选择优。
///
/// 判据（先验，不靠 OCR 猜）：
/// 1. **大厅快捷入口所在那条**：若大厅模板命中点落在某条亮带内，那条就是
///    「当前大厅」行——这是唯一有像素级外部证据的选择，优先返回它。
///    （它同时让 `hall` 判定照旧成立，行为与旧实现一致。）
/// 2. 否则取**最高**的那条（选中行是整行色带；细亮带多是头像/红点误报）。
/// 3. 高度相同时取**会话列表区内、最靠上**的那条（列表自上而下排版）。
fn select_conversation_row(
    image: &DynamicImage,
    sidebar: Rect,
    hall_hit: Option<&crate::ui::template::TemplateHit>,
    friends: Rect,
) -> Option<Rect> {
    let rows = crate::ui::routines::highlighted_conversation_row_candidates(image, sidebar);
    if rows.is_empty() {
        return None;
    }
    if let Some(center) = hall_hit.map(|hit| hit.center()) {
        if let Some(row) = rows
            .iter()
            .find(|row| (row.y..row.bottom()).contains(&center.y))
        {
            return Some(*row);
        }
    }
    // 候选已按「高者优先」排序；这里只在同高时按「落在会话列表区、位置更靠上」微调。
    rows.iter().copied().min_by(|left, right| {
        right
            .height
            .cmp(&left.height)
            .then_with(|| {
                let in_list = |row: &Rect| (row.y..row.bottom()).contains(&friends.y);
                in_list(right).cmp(&in_list(left))
            })
            .then_with(|| left.y.cmp(&right.y))
    })
}

/// 从高亮会话行的 OCR 行里取**会话名**：最上面一条非空行，忽略其余。
///
/// 旧实现要求「恰好一行」，多出一行（末尾消息预览、未读数、时间戳）就整体判
/// `None` → `ChatIdentity::Unknown` → 命令被身份闸丢弃。真实好友私聊截图的高亮行
/// 常带预览，这是「进去后命令不生效」的直接上游。
///
/// 为什么取**首行**而不是「最长行/最像名字的行」：侧栏行自上而下排版，名字恒在
/// 第一行，预览/状态在下面。按内容猜（长度、字符集）会把「预览恰好更长」判成名字，
/// 让身份变成错误玩家——那比 Unknown 更危险（会把命令授权给错的人）。
fn conversation_name_from_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> Option<String> {
    lines
        .into_iter()
        .map(str::trim)
        .find(|text| !text.is_empty())
        .map(str::to_owned)
}

/// 读**左侧好友列表**里所有可见名字（二级大厅的权限下界依据）。
///
/// 用户规则：「只要是左侧列表中的都是好友」。所以大厅里发言人的名字若出现在
/// 左侧列表，就至少是 Friend 级——即便身份映射里没登记。
///
/// **失败一律返回空集合**（= 所有人都退回原有下界，没人因此失去权限）。
/// 空集合与「列表里确实没人」在语义上等价，而后者只会让持有人更少，不会更危险：
/// 回调路径是「未登记 → User」，不是「未登记 → 放行」。
///
/// 匹配用 [`crate::text::fold_wide`]：**只折全角，保留空白与大小写**。
///
/// 与 `identity::resolve`（#4）同一比较口径。**刻意不用
/// `normalize_comparison_text`**：它会丢空白，于是 `A B` 与 `AB` 会撞成同一个键，
/// 陌生人可能因为撞名拿到 Friend 下界。虽然那只是下界（拿不到 Admin/Owner），
/// 但身份相关的匹配一律宁严勿宽——宁可「OCR 多读一个空格 → 匹配失败 → 退回 User」
/// 这种**少给**，也不要「少读一个空格 → 撞上别人」这种**多给**。
fn sidebar_name_set(
    ocr: &OcrRuntimeHandle,
    image: &DynamicImage,
    config: &AppConfig,
) -> std::collections::BTreeSet<String> {
    let region = passive_sidebar_region(config);
    let Ok(crop) = crop_canvas(image, region) else {
        return Default::default();
    };
    let Ok(lines) = ocr.recognize_lines(crop, OcrPriority::UiConfirmation) else {
        log::debug!("二级大厅好友列表 OCR 失败：本轮不做 Friend 下界推断");
        return Default::default();
    };
    let names = lines
        .iter()
        .map(|line| crate::text::fold_wide(line.text.trim()))
        .filter(|name| !name.is_empty())
        .collect::<std::collections::BTreeSet<_>>();
    if names.is_empty() {
        log::debug!("二级大厅好友列表读空：本轮不做 Friend 下界推断");
    }
    names
}

pub(crate) fn recognize_passive_secondary(
    ocr: &OcrRuntimeHandle,
    image: &DynamicImage,
    config: &AppConfig,
    hall: bool,
    selected: Option<Rect>,
    bubbles: &[SecondaryHallBubble],
    latest: Option<Rect>,
) -> Result<(ChatIdentity, Vec<RecognizedChatMessage>)> {
    let selected_name = if !hall {
        selected
            .map(|row| -> Result<Option<String>> {
                let lines =
                    ocr.recognize_lines(crop_canvas(image, row)?, OcrPriority::ChatObservation)?;
                Ok(conversation_name_from_lines(
                    lines.iter().map(|line| line.text.as_str()),
                ))
            })
            .transpose()?
            .flatten()
    } else {
        None
    };
    let identity = passive_secondary_identity(hall, selected_name.as_deref());
    // Secondary bubbles have no primary marker color. Preserve the established
    // channel mapping: selected private conversation => pink; current hall => blue.
    // Unknown rows do not acquire either channel's authorization semantics.
    let message_type = match &identity {
        ChatIdentity::Friend(_) => "pink",
        ChatIdentity::SecondaryHall => "blue",
        _ => "unknown",
    };
    // 只有**大厅**帧需要「左侧列表 = 好友」的下界推断：私聊帧本来就是 Friend，
    // 而 Unknown 帧不派发业务。列表 OCR 只在大厅帧发生，省掉每次私聊的两倍 OCR 成本。
    let friend_list = if hall {
        sidebar_name_set(ocr, image, config)
    } else {
        Default::default()
    };
    // 署名缺失时仍交付正文（旧实现是整条丢掉，大厅又无单气泡兜底 → 整帧零消息）。
    // 告警只在**这里**发一次，不在布局阶段发：布局阶段（`passive_layout`）在聊天桥
    // 接管期间照跑，那一帧的气泡随后会被整帧丢弃，报警只会刷出假故障。
    let orphaned_senders = bubbles
        .iter()
        .filter(|bubble| bubble.sender_rect().is_none())
        .count();
    let mut messages = recognize_passive_hall(
        bubbles
            .iter()
            .map(|bubble| (bubble.rect, bubble.sender_rect())),
        |rect| passive_text(ocr, image, rect, config.ocr.same_line_y_tolerance),
    )?;
    if orphaned_senders > 0 {
        log::warn!(
            "二级大厅视觉：{} 个气泡中有 {orphaned_senders} 个没能关联到发言人证据（头像边界/纹理证据不足），这些气泡仍按正文交付，但署名为空——身份/权限会因此退回来源下界；若持续出现请核对该分辨率的头像定位常量",
            bubbles.len()
        );
    }
    for message in &mut messages {
        message.message_type = message_type.into();
        // 「左侧列表里的名字 = 好友」：只有大厅帧才有意义（私聊已是 Friend）。
        // 匹配走 fold_wide（只折全角、保留空白），与侧栏 OCR 同一口径。
        message.sender_in_friend_list = !friend_list.is_empty()
            && message
                .sender
                .as_deref()
                .is_some_and(|sender| friend_list.contains(&crate::text::fold_wide(sender)));
    }
    if messages.is_empty() && !hall {
        if let Some(block) = latest {
            let raw_sender = matches!(&identity, ChatIdentity::Friend(_))
                .then(|| selected_name.clone())
                .flatten();
            messages.push(RecognizedChatMessage {
                text: passive_text(ocr, image, block, config.ocr.same_line_y_tolerance)?,
                sender: raw_sender.as_deref().and_then(passive_sender),
                raw_sender,
                message_type: message_type.into(),
                block,
                // 这条单气泡兜底属于私聊（`!hall`），本来就是 pink/Friend，
                // 不需要侧栏下界推断。
                sender_in_friend_list: false,
            });
        }
    }
    Ok((identity, messages))
}

pub(crate) fn passive_text(
    ocr: &OcrRuntimeHandle,
    image: &DynamicImage,
    rect: Rect,
    tolerance: i32,
) -> Result<String> {
    Ok(ocr.merged_text(
        crop_canvas(image, rect)?,
        tolerance,
        OcrPriority::ChatObservation,
    )?)
}

pub(crate) fn recognize_passive_hall(
    regions: impl IntoIterator<Item = (Rect, Option<Rect>)>,
    mut recognize: impl FnMut(Rect) -> Result<String>,
) -> Result<Vec<RecognizedChatMessage>> {
    regions
        .into_iter()
        .map(|(block, sender_rect)| {
            let text = recognize(block)?;
            // 没有署名区域：保留消息（正文比署名重要），sender/raw_sender 留空，
            // 让身份闸按「无身份」处理并记带原因的 warn。
            let Some(sender_rect) = sender_rect else {
                return Ok(RecognizedChatMessage {
                    text,
                    sender: None,
                    raw_sender: None,
                    message_type: "blue".into(),
                    block,
                    sender_in_friend_list: false,
                });
            };
            let raw_sender = recognize(sender_rect)?;
            Ok(RecognizedChatMessage {
                text,
                sender: passive_sender(&raw_sender),
                raw_sender: Some(raw_sender),
                message_type: "blue".into(),
                block,
                sender_in_friend_list: false,
            })
        })
        .collect()
}

pub(crate) fn passive_sender(raw: &str) -> Option<String> {
    let identity = parse_secondary_sender_identity(raw);
    let effective = identity.effective_name().trim();
    (!effective.is_empty()).then(|| effective.to_owned())
}

pub(crate) fn passive_secondary_identity(hall: bool, selected_name: Option<&str>) -> ChatIdentity {
    if hall {
        return ChatIdentity::SecondaryHall;
    }
    match selected_name.and_then(passive_sender) {
        Some(name)
            if !name.ends_with(['…', '⋯'])
                && !name.ends_with("...")
                && !matches!(
                    name.as_str(),
                    "\u{597d}\u{53cb}"
                        | "\u{804a}\u{5929}"
                        | "\u{5f53}\u{524d}\u{5927}\u{5385}"
                        | "\u{5927}\u{5385}"
                        | "\u{964c}\u{751f}\u{4eba}"
                        | "\u{4e16}\u{754c}"
                        | "公共频道"
                        | "陌生人消息"
                        | "添加会话对象"
                ) =>
        {
            ChatIdentity::Friend(name.into())
        }
        _ => ChatIdentity::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 括号**跨类型**配对：半角/全角混用也必须解析出备注。
    ///
    /// 真实样本 `昵称A(备注A）` 是症状1的触发源（9 条真实帧都由它引起）：
    /// 旧实现把 `()` 与 `（）` 当两套独立配对，混用就解析不出备注 → sender 保留
    /// 整串 → 身份精确匹配落空 → 权限掉回 User 下界，且**稳定失败**（不会自愈）。
    #[test]
    fn sender_identity_parses_every_bracket_combination() {
        // (半角开, 半角闭)
        let identity = parse_secondary_sender_identity("昵称A(备注A)");
        assert_eq!(identity.nickname, "昵称A");
        assert_eq!(identity.remark.as_deref(), Some("备注A"));
        assert_eq!(identity.effective_name(), "备注A");

        // (半角开, 全角闭) —— 真实线上样本
        let identity = parse_secondary_sender_identity("昵称A(备注A）");
        assert_eq!(identity.nickname, "昵称A");
        assert_eq!(identity.remark.as_deref(), Some("备注A"));
        assert_eq!(identity.effective_name(), "备注A");

        // (全角开, 半角闭)
        let identity = parse_secondary_sender_identity("昵称A（备注A)");
        assert_eq!(identity.nickname, "昵称A");
        assert_eq!(identity.remark.as_deref(), Some("备注A"));
        assert_eq!(identity.effective_name(), "备注A");

        // (全角开, 全角闭)
        let identity = parse_secondary_sender_identity("昵称A（备注A）");
        assert_eq!(identity.nickname, "昵称A");
        assert_eq!(identity.remark.as_deref(), Some("备注A"));
        assert_eq!(identity.effective_name(), "备注A");
    }

    /// 无括号 / 未闭合 / 空备注时，整串就是身份键（不能凭空造备注）。
    #[test]
    fn sender_identity_without_a_complete_pair_keeps_the_whole_string() {
        for raw in [
            "PlayerB",
            "昵称C",
            "2#BOT|勿加",
            "玩家(未闭合",
            "玩家）未闭合（",
        ] {
            let identity = parse_secondary_sender_identity(raw);
            assert_eq!(identity.remark, None, "raw={raw}");
            assert_eq!(identity.effective_name(), raw, "raw={raw}");
        }
    }

    /// 最右侧配对优先：名字里自带括号时，外层备注才是身份键。
    #[test]
    fn sender_identity_uses_the_rightmost_pair() {
        let identity = parse_secondary_sender_identity("玩家(自带内容)(大厅备注)");
        assert_eq!(identity.nickname, "玩家(自带内容)");
        assert_eq!(identity.effective_name(), "大厅备注");

        // 混用括号时同样只看最右那一对。
        let identity = parse_secondary_sender_identity("玩家（自带内容)(大厅备注）");
        assert_eq!(identity.effective_name(), "大厅备注");
    }

    /// 「左侧列表里的名字 = 好友」的匹配口径：与 `identity::resolve` 同为
    /// **fold_wide**（只折全角、保留空白与大小写），且失败时为空集。
    ///
    /// 这里只验证**判定函数**的语义（纯逻辑），真实列表 OCR 在整帧识别里做。
    #[test]
    fn friend_list_membership_folds_width_but_keeps_whitespace() {
        let sidebar = ["PlayerB", "昵称D", "昵称E°"]
            .into_iter()
            .map(crate::text::fold_wide)
            .collect::<std::collections::BTreeSet<_>>();
        assert!(!sidebar.is_empty());

        // 全角/半角差异被折平 → 同一个键（OCR 常见抖动，必须命中）。
        for sender in ["PlayerB", "ＰｌａｙｅｒＢ"] {
            assert!(
                sidebar.contains(&crate::text::fold_wide(sender)),
                "sender={sender} 应判为左侧列表成员（全角折半角）"
            );
        }
        // 首尾空白由调用方 trim 掉，不构成差异。
        assert!(sidebar.contains(&crate::text::fold_wide(" PlayerB ".trim())));

        // **但内部空白不被抹平**：`A B` 与 `AB` 是两个人。
        // 宁可变「少给」（匹配不上 → 退回 User），也不能「多给」（撞名拿到 Friend）。
        let spaced = ["A B"]
            .into_iter()
            .map(crate::text::fold_wide)
            .collect::<std::collections::BTreeSet<_>>();
        assert!(!spaced.contains(&crate::text::fold_wide("AB")));
        assert!(!spaced.contains(&crate::text::fold_wide("A  B")));
        assert!(spaced.contains(&crate::text::fold_wide("A B")));

        // 不在列表里的人不能误判（未登记 → 仍走 User 下界）。
        for sender in ["昵称C", "2#BOT|勿加"] {
            assert!(
                !sidebar.contains(&crate::text::fold_wide(sender)),
                "sender={sender} 不应判为左侧列表成员"
            );
        }

        // 列表读空（OCR 失败）→ 空集合 → 谁都不算好友，退回原下界。
        let empty: std::collections::BTreeSet<String> = Default::default();
        assert!(empty.is_empty());
    }

    /// 高亮行 OCR 多行时取**首行**作会话名（忽略预览/状态行）。
    ///
    /// 旧实现要求恰好一行，多行就 `None` → `Unknown` → 好友私聊命令被丢弃。
    #[test]
    fn conversation_name_takes_the_top_non_empty_line() {
        assert_eq!(
            conversation_name_from_lines(["昵称E°", "今晚一起玩吗"]).as_deref(),
            Some("昵称E°")
        );
        assert_eq!(
            conversation_name_from_lines(["   ", "", "昵称D", "预览"]).as_deref(),
            Some("昵称D")
        );
        assert_eq!(conversation_name_from_lines(["  "]), None);
        assert_eq!(conversation_name_from_lines(Vec::<&str>::new()), None);
        // 恰好一行仍然正确（旧行为不能退化）。
        assert_eq!(
            conversation_name_from_lines(["PlayerB"]).as_deref(),
            Some("PlayerB")
        );
    }
}
