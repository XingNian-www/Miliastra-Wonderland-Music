mod detection;
mod sender_label;

use image::DynamicImage;

use self::detection::{DetectionConfig, Rect as VisionRect, detect_bubbles};
use self::sender_label::{SenderLabelConfig, segment_sender_labels};
use crate::geometry::Rect;

pub(super) struct ConfirmedSecondaryHallBubble {
    pub(super) rect: Rect,
    /// 发言人锚点。**可能缺失**：气泡检测成功但发言人证据关联失败时，
    /// 气泡仍要交付（消息内容比署名更重要），此处为 `None`。
    pub(super) avatar_rect: Option<Rect>,
    pub(super) sender_rect: Option<Rect>,
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
    // **有气泡但发言人证据缺失，不能把整条消息丢掉。**
    //
    // 旧实现 `filter_map(|(bubble, evidence)| evidence.map(...))`：气泡检测到了、
    // 但发言人关联失败（要求 boundary_edges>=24 且 texture_edges>=250，光照/皮肤/
    // 背景一变就失败）的气泡会被**整条静默丢弃**。`recognize_passive_secondary`
    // 只为「private 且 messages 为空」准备 `latest` 单气泡兜底，**大厅没有任何兜底**，
    // 于是整帧零消息、零日志——线上表现就是「大厅几乎不可用」。
    //
    // 现在保留气泡，sender 证据置空：正文照常识别，署名缺省走空串（身份闸会记
    // 带原因的 warn）。只在**完全没检测到气泡**与**检测到但关联失败**之间用
    // 两种不同文案区分，避免把「本来就没消息」误报成故障。
    let total_detected = detected.len();
    let mut orphaned = 0usize;
    let bubbles = detected
        .into_iter()
        .zip(sender_evidence)
        .map(|(bubble, sender_evidence)| {
            let (avatar_rect, sender_rect) = match sender_evidence {
                Some((avatar_rect, sender_rect)) => (Some(avatar_rect), Some(sender_rect)),
                None => {
                    orphaned += 1;
                    (None, None)
                }
            };
            ConfirmedSecondaryHallBubble {
                rect: geometry_rect(bubble.rect),
                avatar_rect,
                sender_rect,
            }
        })
        .collect();
    if total_detected == 0 {
        log::debug!("二级大厅视觉：本帧没有检测到气泡（画面可能没有新消息）");
    } else if orphaned > 0 {
        // 这里**只记 debug**：本函数由 `passive_layout`（界面判断）调用，而界面判断
        // 在外部聊天源（聊天桥）接管期间**仍然照跑**——那些气泡随后会被整帧丢弃，
        // 对它报 warn 只会刷出「接管期间也提示发言人缺失」的假故障。
        // 真正可行动的告警在 `recognize_passive_secondary`：那里只在确实要 OCR 交付时
        // 才执行，接管期间不会跑。
        log::debug!(
            "二级大厅视觉（仅布局阶段，结果可能被丢弃）：{total_detected} 个气泡中有 \
             {orphaned} 个没能关联到发言人证据（头像边界/纹理证据不足）"
        );
    }
    bubbles
}

fn geometry_rect(rect: VisionRect) -> Rect {
    Rect::new(rect.x, rect.y, rect.width, rect.height)
}
