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

pub(crate) fn parse_secondary_sender_identity(value: &str) -> SecondarySenderIdentity {
    let value = value.trim();
    let mut rightmost_pair = None;
    for (open, close) in [('(', ')'), ('（', '）')] {
        let Some(close_index) = value.rfind(close) else {
            continue;
        };
        let Some(open_index) = value[..close_index].rfind(open) else {
            continue;
        };
        if rightmost_pair
            .as_ref()
            .is_none_or(|(best_close, _, _)| close_index > *best_close)
        {
            rightmost_pair = Some((close_index, open_index, open.len_utf8()));
        }
    }
    if let Some((close_index, open_index, open_len)) = rightmost_pair {
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
    let selected = crate::ui::routines::highlighted_conversation_row(
        &frame.image,
        passive_sidebar_region(config),
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
                        hash_region(image, bubble.sender_rect(), &mut hash)?;
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
            "main_menu_template" | "paimon_menu_panel_template"
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
                let names = lines
                    .iter()
                    .map(|line| line.text.trim())
                    .filter(|text| !text.is_empty())
                    .collect::<Vec<_>>();
                // More than one OCR name in the actual highlight is ambiguous.
                Ok(match names.as_slice() {
                    [name] => Some((*name).to_owned()),
                    _ => None,
                })
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
    let mut messages = recognize_passive_hall(
        bubbles
            .iter()
            .map(|bubble| (bubble.rect, bubble.sender_rect())),
        |rect| passive_text(ocr, image, rect, config.ocr.same_line_y_tolerance),
    )?;
    for message in &mut messages {
        message.message_type = message_type.into();
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
    regions: impl IntoIterator<Item = (Rect, Rect)>,
    mut recognize: impl FnMut(Rect) -> Result<String>,
) -> Result<Vec<RecognizedChatMessage>> {
    regions
        .into_iter()
        .map(|(block, sender_rect)| {
            let text = recognize(block)?;
            let raw_sender = recognize(sender_rect)?;
            Ok(RecognizedChatMessage {
                text,
                sender: passive_sender(&raw_sender),
                raw_sender: Some(raw_sender),
                message_type: "blue".into(),
                block,
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
