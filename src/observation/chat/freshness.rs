//! 命令新老判定：把每帧无状态气泡序列对齐到上一帧，只有底部追加区才是新命令。
//!
//! 与 CommandLock 的区别：锁按“文本 + 绝对 y”记忆，无法区分“同一气泡滚动”与
//! “新命令落在同一槽位”；本模块用整屏有序序列的**对角对齐**判定“相对上一帧新追加
//! 在底部的是哪些”，并对视图不连续一律保守判旧。

use std::collections::HashMap;

use crate::observation::chat::RecognizedChatMessage;
use crate::text::{OCR_TRAILING_NOISE, normalize_chat_text};

/// 锚点最少长度：低于此值认为对齐不成立，保守判旧。
const MIN_ANCHOR: usize = 1;
const POS_TOL: i32 = 8;
const EDIT_LIMIT: usize = 4;
const EDIT_MAX_CHARS: usize = 96;

/// 基线的种类，决定首屏命令如何处理。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BaselineKind {
    /// 进程启动/重启（含改配置重启）：首屏命令全部无视。
    AppStart,
    /// 进入大厅/世界（有过场加载）：首屏粉色无视、蓝色交付。
    HallWorldSwitch,
    /// 临时切界面后返回：全部无视（场景 3）。
    PanelReturn,
}

impl BaselineKind {
    /// 由现有 reset_chat_observation_baseline(reason) 的原因串映射；未知原因保守按返回处理。
    pub(crate) fn from_reason(reason: &str) -> Self {
        if reason.contains("千星") || reason.contains("大厅") {
            Self::HallWorldSwitch
        } else {
            Self::PanelReturn
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ViewPhase {
    Idle,
    Baseline,
    Streaming,
    Discontinuity,
}

impl Default for ViewPhase {
    fn default() -> Self {
        ViewPhase::Idle
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Freshness {
    New,
    Seen,
    Unknown,
}

/// 无状态每帧输入。
pub(crate) struct FreshFrame<'a> {
    pub(crate) trusted: bool,
    /// 仅在该帧是某个基线事件的第一帧时给出。
    pub(crate) baseline: Option<BaselineKind>,
    /// 按 block.y 升序的全部气泡（含非命令气泡，它们是锚点）。
    pub(crate) bubbles: &'a [RecognizedChatMessage],
}

pub(crate) struct FrameDecision {
    pub(crate) freshness: Vec<Freshness>,
    /// 每个气泡匹配到的上一帧索引；None 表示尾部新增。
    pub(crate) matched: Vec<Option<usize>>,
    pub(crate) phase: ViewPhase,
    pub(crate) scroll_delta: Option<i32>,
}

#[derive(Clone)]
struct Snapshot {
    y: i32,
    message_type: String,
    sender: Option<String>,
    text: String,
}

impl Snapshot {
    fn of(bubble: &RecognizedChatMessage) -> Self {
        Self {
            y: bubble.block.y,
            message_type: bubble.message_type.clone(),
            sender: bubble.sender.clone(),
            text: bubble.text.clone(),
        }
    }

    fn key(&self) -> String {
        format!(
            "{}\u{1}{}\u{1}{}",
            normalize_chat_text(self.sender.as_deref().unwrap_or("")),
            normalize_chat_text(&self.text),
            self.message_type
        )
    }
}

/// 每个聊天一份的新鲜度状态：跨 Tracker 重建存活（放在 State 层）。
#[derive(Default)]
pub(crate) struct SequenceFreshness {
    phase: ViewPhase,
    initialized: bool,
    /// 上一次观测到聊天不可见；下一帧作不连续处理（保守）。
    pending_discontinuity: bool,
    prev: Vec<Snapshot>,
}

impl SequenceFreshness {
    /// 聊天不可见（publish_no_chat）：不推进对齐，下一帧按不连续处理。
    pub(crate) fn mark_view_disappeared(&mut self) {
        self.pending_discontinuity = true;
    }

    pub(crate) fn observe(&mut self, frame: &FreshFrame<'_>) -> FrameDecision {
        let len = frame.bubbles.len();
        let all = |phase: ViewPhase, freshness: Freshness| FrameDecision {
            freshness: vec![freshness; len],
            matched: vec![None; len],
            phase,
            scroll_delta: None,
        };

        // 1) 基线事件：本帧按种类处理，并重建对齐基准。
        if let Some(kind) = frame.baseline {
            // 该 chat 的第一个基线（进程启动/重启后首见）一律全旧；
            // 之后的基线才按原因区分为“进入大厅/世界”与“切界面返回”。
            let kind = if self.initialized { kind } else { BaselineKind::AppStart };
            let phase = if kind == BaselineKind::AppStart {
                ViewPhase::Baseline
            } else {
                ViewPhase::Streaming
            };
            let mut decision = all(phase, Freshness::Unknown);
            if kind == BaselineKind::HallWorldSwitch {
                // 命令来源只有 blue（大厅）与 pink（好友）；金色/黄色 marker 仅用于 OCR 分块，不在此列。
                for (slot, bubble) in decision.freshness.iter_mut().zip(frame.bubbles) {
                    if bubble.message_type == "blue" {
                        *slot = Freshness::New;
                    }
                }
            }
            self.phase = phase;
            self.prev = frame.bubbles.iter().map(Snapshot::of).collect();
            self.initialized = true;
            self.pending_discontinuity = false;
            return decision;
        }

        // 2) 空帧：不构成追加证据。
        if len == 0 {
            let phase = if self.initialized {
                ViewPhase::Streaming
            } else {
                ViewPhase::Baseline
            };
            self.phase = phase;
            return all(phase, Freshness::Unknown);
        }
        // 3) 不可信帧 / 不连续：保守判旧并重建基准。
        if !frame.trusted || !self.initialized || self.pending_discontinuity {
            self.phase = ViewPhase::Discontinuity;
            self.prev = frame.bubbles.iter().map(Snapshot::of).collect();
            self.initialized = true;
            self.pending_discontinuity = false;
            return all(ViewPhase::Discontinuity, Freshness::Unknown);
        }

        // 4) 对角对齐：锚点必须以 cur[0] 起、止于 prev[n-1]、零跳过、长度 >= MIN_ANCHOR。
        let n = self.prev.len();
        let m = len;
        let mut best: Option<(usize, i32)> = None;
        for k in (MIN_ANCHOR..=n.min(m)).rev() {
            let base = n - k;
            let delta = frame.bubbles[0].block.y - self.prev[base].y;
            if (0..k).all(|i| bubble_matches(&self.prev[base + i], &frame.bubbles[i], delta)) {
                best = Some((k, delta));
                break;
            }
        }
        let Some((k, delta)) = best else {
            self.phase = ViewPhase::Discontinuity;
            self.prev = frame.bubbles.iter().map(Snapshot::of).collect();
            return all(ViewPhase::Discontinuity, Freshness::Unknown);
        };

        let mut decision = FrameDecision {
            freshness: vec![Freshness::Seen; m],
            matched: vec![None; m],
            phase: ViewPhase::Streaming,
            scroll_delta: Some(delta),
        };
        for i in 0..k {
            decision.matched[i] = Some(n - k + i);
        }
        // 尾部 = 底部追加区，默认全部为新；C1'：同键只认最靠下那条。
        let mut last_by_key: HashMap<String, usize> = HashMap::new();
        for i in k..m {
            let snapshot = Snapshot::of(&frame.bubbles[i]);
            if is_command_candidate(&snapshot.text) {
                last_by_key.insert(snapshot.key(), i);
            }
        }
        for i in k..m {
            let snapshot = Snapshot::of(&frame.bubbles[i]);
            let candidate = is_command_candidate(&snapshot.text);
            let bottom_most = last_by_key.get(&snapshot.key()) == Some(&i);
            decision.freshness[i] = if !candidate || bottom_most {
                Freshness::New
            } else {
                Freshness::Seen
            };
        }

        self.phase = ViewPhase::Streaming;
        self.prev = frame.bubbles.iter().map(Snapshot::of).collect();
        decision
    }
}

fn bubble_matches(prev: &Snapshot, cur: &RecognizedChatMessage, delta: i32) -> bool {
    if prev.message_type != cur.message_type {
        return false;
    }
    if let (Some(a), Some(b)) = (prev.sender.as_deref(), cur.sender.as_deref()) {
        if !a.is_empty()
            && !b.is_empty()
            && normalize_chat_text(a) != normalize_chat_text(b)
        {
            return false;
        }
    }
    if (cur.block.y - prev.y - delta).abs() > POS_TOL {
        return false;
    }
    // 命令候选必须精确匹配：命令短、分支多，允许编辑距离会把 &M3 与 &M4 误并成同一气泡。
    // 只有非命令（闲聊/回复）才允许有界编辑距离的近似匹配。
    if is_command_candidate(&prev.text) || is_command_candidate(&cur.text) {
        command_text_equal(&prev.text, &cur.text)
    } else {
        text_similar(&prev.text, &cur.text)
    }
}

/// 命令候选的精确匹配：归一化（折全角、去空白）后去掉尾部 OCR 噪声，必须完全相等。
fn command_text_equal(left: &str, right: &str) -> bool {
    let a = normalize_chat_text(left);
    let b = normalize_chat_text(right);
    a.trim_end_matches(OCR_TRAILING_NOISE) == b.trim_end_matches(OCR_TRAILING_NOISE)
}

/// 文本是否可能是同一条气泡的两次识别（相等 / 尾部噪声 / 截断 / 有界编辑距离）。
fn text_similar(left: &str, right: &str) -> bool {
    let a = normalize_chat_text(left);
    let b = normalize_chat_text(right);
    if a.is_empty() || b.is_empty() {
        return true;
    }
    if a == b {
        return true;
    }
    let ta = a.trim_end_matches(OCR_TRAILING_NOISE);
    let tb = b.trim_end_matches(OCR_TRAILING_NOISE);
    if ta == tb || ta.starts_with(tb) || tb.starts_with(ta) {
        return true;
    }
    // 编辑距离上限按较短文本长度缩放：短文本必须几乎完全一致，否则 `A` 与 `B`
    // 会因距离 1 被当成同一气泡，锚点会把新增行一并吞入。
    let allowed = EDIT_LIMIT.min(ta.chars().count().min(tb.chars().count()) / 3);
    allowed > 0 && bounded_levenshtein(&ta, &tb, allowed)
}

fn bounded_levenshtein(a: &str, b: &str, limit: usize) -> bool {
    let a: Vec<char> = a.chars().take(EDIT_MAX_CHARS).collect();
    let b: Vec<char> = b.chars().take(EDIT_MAX_CHARS).collect();
    if a.len().abs_diff(b.len()) > limit {
        return false;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        let mut row_min = cur[0];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
            row_min = row_min.min(cur[j + 1]);
        }
        if row_min > limit {
            return false;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()] <= limit
}

/// 首字符是否为 @/#（全角亦可）。
fn is_command_candidate(text: &str) -> bool {
    matches!(
        text.trim_start().chars().next(),
        Some('@' | '＠' | '#' | '＃' | '&' | '＆')
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::geometry::Rect;

    fn bubble(y: i32, text: &str) -> RecognizedChatMessage {
        RecognizedChatMessage {
            text: text.to_string(),
            sender: Some("2#BOT|勿加".to_string()),
            raw_sender: None,
            message_type: "blue".to_string(),
            block: Rect::new(0, y, 400, 28),
        }
    }

    fn rows(items: &[(&str, i32)]) -> Vec<RecognizedChatMessage> {
        items.iter().map(|(text, y)| bubble(*y, text)).collect()
    }

    fn observe(
        state: &mut SequenceFreshness,
        baseline: Option<BaselineKind>,
        items: &[(&str, i32)],
    ) -> FrameDecision {
        let bubbles = rows(items);
        state.observe(&FreshFrame {
            trusted: true,
            baseline,
            bubbles: &bubbles,
        })
    }

    fn new_of(decision: &FrameDecision, index: usize) -> Freshness {
        decision.freshness[index]
    }

    #[test]
    fn app_start_ignores_first_screen() {
        let mut state = SequenceFreshness::default();
        let d = observe(
            &mut state,
            Some(BaselineKind::AppStart),
            &[("@状态", 100), ("闲聊", 130)],
        );
        assert!(d.freshness.iter().all(|f| *f == Freshness::Unknown));
    }

    #[test]
    fn hall_switch_delivers_blue_and_hides_pink() {
        let mut state = SequenceFreshness::default();
        // 该 chat 的第一个基线一律 AppStart（全旧），先把状态初始化。
        let seed = [bubble(10, "seed")];
        let _ = state.observe(&FreshFrame {
            trusted: true,
            baseline: Some(BaselineKind::AppStart),
            bubbles: &seed,
        });
        let bubbles = vec![
            {
                let mut b = bubble(100, "@状态");
                b.message_type = "pink".to_string();
                b
            },
            bubble(130, "@状态"),
        ];
        let d = state.observe(&FreshFrame {
            trusted: true,
            baseline: Some(BaselineKind::HallWorldSwitch),
            bubbles: &bubbles,
        });
        assert_eq!(d.freshness[0], Freshness::Unknown, "粉色首屏必须无视");
        assert_eq!(d.freshness[1], Freshness::New, "蓝色首屏照常交付");
    }

    #[test]
    fn appended_at_bottom_is_new() {
        let mut state = SequenceFreshness::default();
        let _ = observe(
            &mut state,
            Some(BaselineKind::HallWorldSwitch),
            &[("A", 100), ("B", 130), ("C", 160), ("D", 190), ("E", 220)],
        );
        // 整屏上滚 30px，底部新增 F。
        let d = observe(
            &mut state,
            None,
            &[("B", 70), ("C", 100), ("D", 130), ("E", 160), ("F", 190)],
        );
        assert_eq!(new_of(&d, 4), Freshness::New, "底部 F 应为新");
        assert!(d.freshness[..4].iter().all(|f| *f == Freshness::Seen));
    }

    #[test]
    fn same_frame_identical_only_bottom_is_new() {
        let mut state = SequenceFreshness::default();
        let _ = observe(
            &mut state,
            Some(BaselineKind::HallWorldSwitch),
            &[("A", 100), ("B", 130), ("C", 160)],
        );
        // 上一帧末两条 B/C 保留，尾部两条同文本 @CC。
        let d = observe(
            &mut state,
            None,
            &[("B", 130), ("C", 160), ("@CC", 190), ("@CC", 220)],
        );
        assert_eq!(new_of(&d, 2), Freshness::Seen, "同键上方那条必须判旧");
        assert_eq!(new_of(&d, 3), Freshness::New, "同键最靠下那条才是新");
    }

    #[test]
    fn panel_return_is_all_old() {
        let mut state = SequenceFreshness::default();
        let _ = observe(
            &mut state,
            Some(BaselineKind::HallWorldSwitch),
            &[("A", 100), ("B", 130)],
        );
        let d = observe(
            &mut state,
            Some(BaselineKind::PanelReturn),
            &[("@状态", 100), ("m1", 130)],
        );
        assert!(d.freshness.iter().all(|f| *f == Freshness::Unknown));
    }

    #[test]
    fn no_common_anchor_is_all_old() {
        let mut state = SequenceFreshness::default();
        let _ = observe(
            &mut state,
            Some(BaselineKind::HallWorldSwitch),
            &[("A", 100), ("B", 130), ("C", 160)],
        );
        // 两次观测之间追加超过窗口行数：无共同锚，保守判旧。
        let d = observe(
            &mut state,
            None,
            &[("X", 100), ("Y", 130), ("Z", 160)],
        );
        assert!(d.freshness.iter().all(|f| *f == Freshness::Unknown));
    }

    #[test]
    fn ocr_noise_revision_still_anchors() {
        let mut state = SequenceFreshness::default();
        let _ = observe(
            &mut state,
            Some(BaselineKind::HallWorldSwitch),
            &[("标点测试，", 100), ("B", 130), ("C", 160)],
        );
        let d = observe(
            &mut state,
            None,
            &[("标点测试，。！？；：", 100), ("B", 130), ("C", 160), ("D", 190)],
        );
        assert_eq!(new_of(&d, 3), Freshness::New);
        assert_eq!(new_of(&d, 0), Freshness::Seen);
    }
}
