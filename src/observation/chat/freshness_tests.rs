//! freshness 的回归/属性/边界测试（独立于模块内的单元测试，便于并行维护）。
//!
//! 覆盖范围：
//! - 确定性 PRNG 生成的随机有序帧序（不变量 + 无墙钟可复现）；
//! - OCR/颜色/发送者/坐标/滚动等边界；
//! - technical-docs/design/evidence/replay.json 的 6 组真帧用例
//!   （只保留文本序，y 用行网格合成，与 README 的“约 5 行窗口”一致）。
//!
//! 这里只驱动 SequenceFreshness 纯状态机，不改动生产逻辑，也不引入墙钟或系统随机。

use std::collections::HashMap;

use super::freshness::{
    BaselineKind, FrameDecision, FreshFrame, Freshness, SequenceFreshness, ViewPhase,
};
use super::RecognizedChatMessage;
use crate::text::normalize_chat_text;
use crate::ui::geometry::Rect;

/// 合成行距：与真帧约 5 行窗口的量级一致。
const ROW: i32 = 30;
const SENDER: &str = "2#BOT|勿加";

fn bubble_at(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    text: &str,
    kind: &str,
    sender: &str,
) -> RecognizedChatMessage {
    RecognizedChatMessage {
        text: text.to_string(),
        sender: Some(sender.to_string()),
        raw_sender: None,
        message_type: kind.to_string(),
        block: Rect::new(x, y, width, height),
    }
}

fn bubble(y: i32, text: &str, kind: &str) -> RecognizedChatMessage {
    bubble_at(0, y, 400, 28, text, kind, SENDER)
}

/// 合成一帧：第 i 条 y = i*ROW（自上而下）。
fn frame(texts: &[&str]) -> Vec<RecognizedChatMessage> {
    texts
        .iter()
        .enumerate()
        .map(|(i, text)| bubble(i as i32 * ROW, text, "blue"))
        .collect()
}

fn observe(
    state: &mut SequenceFreshness,
    baseline: Option<BaselineKind>,
    bubbles: &[RecognizedChatMessage],
) -> FrameDecision {
    state.observe(&FreshFrame {
        trusted: true,
        baseline,
        bubbles,
    })
}

fn observe_texts(
    state: &mut SequenceFreshness,
    baseline: Option<BaselineKind>,
    texts: &[&str],
) -> FrameDecision {
    let bubbles = frame(texts);
    observe(state, baseline, &bubbles)
}

fn new_indices(decision: &FrameDecision) -> Vec<usize> {
    decision
        .freshness
        .iter()
        .enumerate()
        .filter(|(_, freshness)| **freshness == Freshness::New)
        .map(|(index, _)| index)
        .collect()
}

/// 断言“新”的恰好是这些下标；其余为 Seen/Unknown（即不算新）。
fn assert_new_at(decision: &FrameDecision, expected: &[usize]) {
    assert_eq!(
        new_indices(decision),
        expected,
        "unexpected freshness: {:?}",
        decision.freshness
    );
}

/// 以文本定位期望为“新”的条目；同文本取最靠下的一条（C1' 语义）。
fn assert_new_texts(
    decision: &FrameDecision,
    bubbles: &[RecognizedChatMessage],
    expected_texts: &[&str],
) {
    let expected: Vec<usize> = expected_texts
        .iter()
        .map(|text| {
            bubbles
                .iter()
                .rposition(|b| b.text == *text)
                .unwrap_or_else(|| panic!("frame does not contain {text:?}"))
        })
        .collect();
    assert_eq!(
        new_indices(decision),
        expected,
        "unexpected freshness: {:?}",
        decision.freshness
    );
}

fn is_command(text: &str) -> bool {
    matches!(
        text.trim_start().chars().next(),
        Some('@' | '＠' | '#' | '＃' | '&' | '＆')
    )
}

fn key_of(b: &RecognizedChatMessage) -> String {
    format!(
        "{}\u{1}{}\u{1}{}",
        normalize_chat_text(b.sender.as_deref().unwrap_or("")),
        normalize_chat_text(&b.text),
        b.message_type
    )
}

// ---------------------------------------------------------------------------
// 确定性 PRNG（SplitMix64）：无外部依赖、无墙钟、同 seed 必同序列。
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        assert!(n > 0);
        (self.next_u64() % n as u64) as usize
    }

    fn range_i32(&mut self, lo: i32, hi: i32) -> i32 {
        assert!(lo <= hi);
        lo + (self.next_u64() % (hi - lo + 1) as u64) as i32
    }
}

const CMD_PREFIXES: [&str; 6] = ["@", "＠", "#", "＃", "&", "＆"];
const KINDS: [&str; 3] = ["blue", "pink", "yellow"];

/// 随机一条逻辑消息（文本, 颜色）。文本复用少量键，刻意制造同文本重复。
fn random_message(rng: &mut Rng) -> (String, String) {
    let kind = KINDS[rng.below(KINDS.len())].to_string();
    let text = match rng.below(10) {
        0 => String::new(),
        1..=5 => format!(
            "{}{}",
            CMD_PREFIXES[rng.below(CMD_PREFIXES.len())],
            rng.below(3)
        ),
        6 => format!("闲聊{}", rng.below(4)),
        7 => "多行\n第二行".to_string(),
        8 => "很".repeat(rng.below(20)),
        _ => format!("聊{}", rng.below(5)),
    };
    (text, kind)
}

/// 一帧 = 逻辑消息列表的末 window 条，整体加 offset 的滚动位移。
fn frame_from(
    logical: &[(String, String)],
    window: usize,
    offset: i32,
) -> Vec<RecognizedChatMessage> {
    let start = logical.len().saturating_sub(window);
    logical[start..]
        .iter()
        .enumerate()
        .map(|(i, (text, kind))| {
            bubble(i as i32 * ROW + offset, text.as_str(), kind.as_str())
        })
        .collect()
}

fn frame_snapshot(bubbles: &[RecognizedChatMessage]) -> Vec<(String, String, i32)> {
    bubbles
        .iter()
        .map(|b| (b.text.clone(), b.message_type.clone(), b.block.y))
        .collect()
}

/// 每次 observe 的完整可比较结果（用于确定性断言）。
#[derive(Clone, Debug, PartialEq, Eq)]
struct DecisionTrace {
    freshness: Vec<Freshness>,
    matched: Vec<Option<usize>>,
    phase: ViewPhase,
    scroll_delta: Option<i32>,
}

impl DecisionTrace {
    fn of(decision: &FrameDecision) -> Self {
        Self {
            freshness: decision.freshness.clone(),
            matched: decision.matched.clone(),
            phase: decision.phase,
            scroll_delta: decision.scroll_delta,
        }
    }
}

/// 对常规帧（无基线、可信）检查不变量：
/// G1（cur[0] 不 New）、相同连续帧不 New、C1'（同键最多最下一条 New）。
fn check_regular_frame(
    seed: u64,
    step: usize,
    decision: &FrameDecision,
    bubbles: &[RecognizedChatMessage],
    prev: &[(String, String, i32)],
) {
    if bubbles.is_empty() {
        return;
    }
    assert_ne!(
        decision.freshness[0],
        Freshness::New,
        "G1 violated seed={seed} step={step}"
    );

    if prev == frame_snapshot(bubbles).as_slice() {
        assert!(
            decision
                .freshness
                .iter()
                .all(|freshness| *freshness != Freshness::New),
            "identical consecutive frame produced New seed={seed} step={step} freshness={:?}",
            decision.freshness
        );
    }

    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, bubble) in bubbles.iter().enumerate() {
        if is_command(&bubble.text) {
            groups.entry(key_of(bubble)).or_default().push(index);
        }
    }
    for (key, indices) in groups {
        let newly: Vec<usize> = indices
            .iter()
            .copied()
            .filter(|index| decision.freshness[*index] == Freshness::New)
            .collect();
        assert!(
            newly.len() <= 1,
            "C1' violated: key={key:?} seed={seed} step={step} freshness={:?}",
            decision.freshness
        );
        if let Some(&only) = newly.first() {
            assert_eq!(
                only,
                *indices.last().unwrap(),
                "C1' must pick the bottom-most occurrence key={key:?} seed={seed} step={step}"
            );
        }
    }
}

/// 跑完一条随机帧序，返回全部判决；同时在其中断言所有不变量。
fn fuzz_run(seed: u64) -> Vec<DecisionTrace> {
    let mut rng = Rng::new(seed);
    let mut state = SequenceFreshness::default();
    let mut trace = Vec::new();
    let window = 2 + rng.below(4); // 2..=5 行窗口
    let mut logical: Vec<(String, String)> = Vec::new();
    for _ in 0..(2 + rng.below(4)) {
        logical.push(random_message(&mut rng));
    }

    // 第一帧必带基线事件；该 chat 的“第一个基线”一律按 AppStart（全 Unknown）。
    let first_baseline = match rng.below(3) {
        0 => BaselineKind::AppStart,
        1 => BaselineKind::HallWorldSwitch,
        _ => BaselineKind::PanelReturn,
    };
    let mut offset = rng.range_i32(-60, 60);
    {
        let bubbles = frame_from(&logical, window, offset);
        let decision = observe(&mut state, Some(first_baseline), &bubbles);
        assert!(
            decision
                .freshness
                .iter()
                .all(|freshness| *freshness == Freshness::Unknown),
            "first baseline must be all Unknown seed={seed}"
        );
        trace.push(DecisionTrace::of(&decision));
    }
    let mut prev = frame_snapshot(&frame_from(&logical, window, offset));

    for step in 0..48 {
        let action = rng.below(12);
        let mut after_view_disappear = false;
        match action {
            0..=6 => {
                let appended = rng.below(4);
                for _ in 0..appended {
                    logical.push(random_message(&mut rng));
                }
            }
            7 => { /* 不追加也不改 offset：制造“完全相同的连续帧” */ }
            8 => {
                state.mark_view_disappeared();
                after_view_disappear = true;
            }
            9 => {
                // 不可信帧：保守全 Unknown，并重建基准。
                let bubbles = frame_from(&logical, window, rng.range_i32(-60, 60));
                let decision = state.observe(&FreshFrame {
                    trusted: false,
                    baseline: None,
                    bubbles: &bubbles,
                });
                assert!(
                    decision
                        .freshness
                        .iter()
                        .all(|freshness| *freshness == Freshness::Unknown),
                    "untrusted frame must be all Unknown seed={seed} step={step}"
                );
                trace.push(DecisionTrace::of(&decision));
                prev = frame_snapshot(&bubbles);
                offset = rng.range_i32(-60, 60);
                continue;
            }
            10 => {
                // PanelReturn：即使画面连续也全 Unknown。
                let bubbles = frame_from(&logical, window, rng.range_i32(-60, 60));
                let decision = observe(&mut state, Some(BaselineKind::PanelReturn), &bubbles);
                assert!(
                    decision
                        .freshness
                        .iter()
                        .all(|freshness| *freshness == Freshness::Unknown),
                    "PanelReturn must be all Unknown seed={seed} step={step}"
                );
                trace.push(DecisionTrace::of(&decision));
                prev = frame_snapshot(&bubbles);
                offset = rng.range_i32(-60, 60);
                continue;
            }
            _ => {
                // 已初始化后的 HallWorldSwitch：粉旧蓝新。
                let bubbles = frame_from(&logical, window, rng.range_i32(-60, 60));
                let decision = observe(&mut state, Some(BaselineKind::HallWorldSwitch), &bubbles);
                for (index, bubble) in bubbles.iter().enumerate() {
                    let expected = if bubble.message_type == "blue" {
                        Freshness::New
                    } else {
                        Freshness::Unknown
                    };
                    assert_eq!(
                        decision.freshness[index], expected,
                        "HallWorldSwitch color handling seed={seed} step={step}"
                    );
                }
                trace.push(DecisionTrace::of(&decision));
                prev = frame_snapshot(&bubbles);
                offset = rng.range_i32(-60, 60);
                continue;
            }
        }

        if action <= 6 && rng.below(2) == 0 {
            offset = rng.range_i32(-60, 60);
        }
        let bubbles = frame_from(&logical, window, offset);
        let decision = observe(&mut state, None, &bubbles);
        if after_view_disappear {
            assert!(
                decision
                    .freshness
                    .iter()
                    .all(|freshness| *freshness == Freshness::Unknown),
                "frame after mark_view_disappeared must be all Unknown seed={seed} step={step}"
            );
        }
        check_regular_frame(seed, step, &decision, &bubbles, &prev);
        trace.push(DecisionTrace::of(&decision));
        prev = frame_snapshot(&bubbles);
    }

    trace
}

// ---------------------------------------------------------------------------
// 1. 属性 / 模糊 / 确定性
// ---------------------------------------------------------------------------

#[test]
fn fuzz_invariants_hold_for_random_sequences() {
    for seed in 1..=64u64 {
        let _ = fuzz_run(seed);
    }
}

#[test]
fn fuzz_same_seed_same_output_without_wall_clock() {
    for seed in [0u64, 1, 7, 42, 0xDEAD_BEEF, 0x1234_5678_9ABC_DEF0] {
        let first = fuzz_run(seed);
        let second = fuzz_run(seed);
        assert_eq!(
            first, second,
            "freshness decisions must be deterministic for seed={seed}"
        );
    }
}

#[test]
fn first_baseline_of_every_kind_is_all_unknown() {
    for kind in [
        BaselineKind::AppStart,
        BaselineKind::HallWorldSwitch,
        BaselineKind::PanelReturn,
    ] {
        let mut state = SequenceFreshness::default();
        let decision = observe_texts(&mut state, Some(kind), &["@状态", "闲聊"]);
        assert!(
            decision
                .freshness
                .iter()
                .all(|freshness| *freshness == Freshness::Unknown),
            "first baseline {kind:?} must be all Unknown"
        );
    }
}

#[test]
fn panel_return_is_all_unknown_even_with_appended_command() {
    let mut state = SequenceFreshness::default();
    let _ = observe_texts(&mut state, Some(BaselineKind::AppStart), &["A", "B"]);
    let decision = observe_texts(&mut state, Some(BaselineKind::PanelReturn), &["A", "B", "@新"]);
    assert!(decision
        .freshness
        .iter()
        .all(|freshness| *freshness == Freshness::Unknown));
}

#[test]
fn mark_view_disappeared_forces_next_frame_unknown_then_recovers() {
    let mut state = SequenceFreshness::default();
    let _ = observe_texts(&mut state, Some(BaselineKind::AppStart), &["A", "B", "C"]);
    state.mark_view_disappeared();
    let decision = observe_texts(&mut state, None, &["B", "C", "@新"]);
    assert!(decision
        .freshness
        .iter()
        .all(|freshness| *freshness == Freshness::Unknown));
    assert_eq!(decision.phase, ViewPhase::Discontinuity);

    // 基准已重建为上一帧；再追加一条应恢复判新。
    let recovered = observe_texts(&mut state, None, &["C", "@新", "@再新"]);
    assert_new_at(&recovered, &[2]);
}

#[test]
fn first_bubble_is_never_new_on_a_regular_frame() {
    let mut state = SequenceFreshness::default();
    let _ = observe_texts(&mut state, Some(BaselineKind::HallWorldSwitch), &["A", "B", "C"]);
    let decision = observe_texts(&mut state, None, &["B", "C", "@新"]);
    assert_ne!(decision.freshness[0], Freshness::New);
    assert_new_at(&decision, &[2]);
}

#[test]
fn absent_view_consumes_cold_baseline_so_first_blue_is_new() {
    // 聊天区一直不存在 => 首屏没有命令，冷基线消费；首个 HallWorldSwitch 基线按 blue New 处理。
    let mut state = SequenceFreshness::default();
    state.mark_view_absent();
    let decision = observe_texts(&mut state, Some(BaselineKind::HallWorldSwitch), &["@新"]);
    assert_eq!(decision.freshness[0], Freshness::New);
}

#[test]
fn cold_baseline_without_absent_view_still_suppresses_first_blue() {
    // 对照：没有“聊天区不存在”，第一个基线仍是 AppStart 全旧。
    let mut state = SequenceFreshness::default();
    let decision = observe_texts(&mut state, Some(BaselineKind::HallWorldSwitch), &["@旧"]);
    assert_eq!(decision.freshness[0], Freshness::Unknown);
}

#[test]
fn hall_switch_baseline_can_mark_the_top_blue_bubble_new() {
    // 记录 G1 的例外：HallWorldSwitch 基线的蓝色首屏按业务规则交付。
    // 先让该 chat 有过观测，否则“第一个基线”按 AppStart 一律全 Unknown。
    let mut state = SequenceFreshness::default();
    let _ = observe_texts(&mut state, Some(BaselineKind::AppStart), &["种子"]);
    let bubbles = vec![
        bubble_at(0, 0, 400, 28, "@蓝", "blue", SENDER),
        bubble_at(0, 30, 400, 28, "@粉", "pink", SENDER),
    ];
    let decision = observe(&mut state, Some(BaselineKind::HallWorldSwitch), &bubbles);
    assert_eq!(
        decision.freshness,
        vec![Freshness::New, Freshness::Unknown]
    );
}

#[test]
fn empty_frame_is_unknown_and_preserves_alignment() {
    let mut state = SequenceFreshness::default();
    let _ = observe_texts(&mut state, Some(BaselineKind::AppStart), &["A", "B"]);
    let empty = observe_texts(&mut state, None, &[]);
    assert!(empty.freshness.is_empty());
    let decision = observe_texts(&mut state, None, &["A", "B", "@新"]);
    assert_new_at(&decision, &[2]);
}

#[test]
fn identical_consecutive_frames_have_no_new() {
    let mut state = SequenceFreshness::default();
    let _ = observe_texts(&mut state, Some(BaselineKind::AppStart), &["A", "B", "@C"]);
    let decision = observe_texts(&mut state, None, &["A", "B", "@C"]);
    assert!(
        decision
            .freshness
            .iter()
            .all(|freshness| *freshness == Freshness::Seen),
        "identical frame: {:?}",
        decision.freshness
    );
}

#[test]
fn appended_same_key_only_bottom_most_is_new() {
    let mut state = SequenceFreshness::default();
    let _ = observe_texts(&mut state, Some(BaselineKind::AppStart), &["A", "B", "C"]);
    let decision = observe_texts(&mut state, None, &["B", "C", "@K", "@K", "@K"]);
    assert_eq!(
        decision.freshness,
        vec![
            Freshness::Seen,
            Freshness::Seen,
            Freshness::Seen,
            Freshness::Seen,
            Freshness::New
        ]
    );
}

#[test]
fn untrusted_frame_is_all_unknown_and_rebuilds_the_baseline() {
    let mut state = SequenceFreshness::default();
    let _ = observe_texts(&mut state, Some(BaselineKind::AppStart), &["A", "B"]);
    let bubbles = frame(&["A", "B", "@新"]);
    let decision = state.observe(&FreshFrame {
        trusted: false,
        baseline: None,
        bubbles: &bubbles,
    });
    assert!(decision
        .freshness
        .iter()
        .all(|freshness| *freshness == Freshness::Unknown));
    assert_eq!(decision.phase, ViewPhase::Discontinuity);

    let same = observe_texts(&mut state, None, &["A", "B", "@新"]);
    assert!(same
        .freshness
        .iter()
        .all(|freshness| *freshness == Freshness::Seen));
    let more = observe_texts(&mut state, None, &["B", "@新", "@再"]);
    assert_new_at(&more, &[2]);
}

// ---------------------------------------------------------------------------
// 2. 边界
// ---------------------------------------------------------------------------

#[test]
fn super_long_wrapped_bubble_still_anchors() {
    // 超长换行 → block.height 很大，但锚点只看 y/文本/颜色/发送者。
    let long = "这是一条很长的消息\n第二行还很长\n第三行继续很长继续很长";
    let mut state = SequenceFreshness::default();
    let seeded = vec![
        bubble_at(0, 100, 400, 120, long, "blue", SENDER),
        bubble(220, "B", "blue"),
        bubble(250, "C", "blue"),
    ];
    let _ = observe(&mut state, Some(BaselineKind::AppStart), &seeded);
    let current = vec![
        bubble_at(0, 70, 400, 120, long, "blue", SENDER),
        bubble(190, "B", "blue"),
        bubble(220, "C", "blue"),
        bubble(250, "@新", "blue"),
    ];
    let decision = observe(&mut state, None, &current);
    assert_new_at(&decision, &[3]);
}

#[test]
fn empty_text_bubble_is_a_tolerated_anchor() {
    let mut state = SequenceFreshness::default();
    let _ = observe_texts(&mut state, Some(BaselineKind::AppStart), &["", "A"]);
    let decision = observe_texts(&mut state, None, &["", "A", "@新"]);
    assert_new_at(&decision, &[2]);
}

#[test]
fn yellow_color_is_part_of_the_anchor_identity() {
    let mut state = SequenceFreshness::default();
    let seeded = vec![
        bubble_at(0, 100, 400, 28, "黄1", "yellow", SENDER),
        bubble_at(0, 130, 400, 28, "黄2", "yellow", SENDER),
    ];
    let _ = observe(&mut state, Some(BaselineKind::AppStart), &seeded);
    let current = vec![
        bubble_at(0, 70, 400, 28, "黄1", "yellow", SENDER),
        bubble_at(0, 100, 400, 28, "黄2", "yellow", SENDER),
        bubble_at(0, 130, 400, 28, "@蓝", "blue", SENDER),
    ];
    let decision = observe(&mut state, None, &current);
    assert_new_at(&decision, &[2]);
}

#[test]
fn every_command_prefix_counts_as_a_candidate_for_c1() {
    // 半角/全角 @ # 与 & 映射前缀都算命令候选：两条同键时只最下为 New。
    for prefix in ["@", "＠", "#", "＃", "&", "＆"] {
        let mut state = SequenceFreshness::default();
        let _ = observe_texts(&mut state, Some(BaselineKind::AppStart), &["A", "B"]);
        let command = format!("{prefix}K");
        let texts = ["B", command.as_str(), command.as_str()];
        let decision = observe_texts(&mut state, None, &texts);
        assert_new_at(&decision, &[2]);
    }
    // 半角 & 与全角 ＆ 经 normalize_chat_text 折叠为同一个键，
    // 因此仍按 C1' 只认最下一条。
    let mut state = SequenceFreshness::default();
    let _ = observe_texts(&mut state, Some(BaselineKind::AppStart), &["A", "B"]);
    let decision = observe_texts(&mut state, None, &["B", "&K", "＆K"]);
    assert_new_at(&decision, &[2]);
}

#[test]
fn same_frame_three_identical_only_bottom_is_new() {
    let mut state = SequenceFreshness::default();
    let _ = observe_texts(&mut state, Some(BaselineKind::AppStart), &["A", "B"]);
    let decision = observe_texts(&mut state, None, &["B", "#同", "#同", "#同"]);
    assert_eq!(
        decision.freshness,
        vec![
            Freshness::Seen,
            Freshness::Seen,
            Freshness::Seen,
            Freshness::New
        ]
    );
}

#[test]
fn mid_list_small_ocr_revision_keeps_the_anchor() {
    let mut state = SequenceFreshness::default();
    let _ = observe_texts(
        &mut state,
        Some(BaselineKind::AppStart),
        &["标点测试，", "B", "C", "D"],
    );
    // 中段首条的标点被 OCR 修订（尾部噪声增加），仍应锚定。
    let decision = observe_texts(
        &mut state,
        None,
        &["标点测试，。！？；：", "B", "C", "D", "@新"],
    );
    assert_new_at(&decision, &[4]);
}

#[test]
fn mid_list_large_ocr_revision_breaks_the_anchor() {
    let mut state = SequenceFreshness::default();
    let _ = observe_texts(&mut state, Some(BaselineKind::AppStart), &["AAAA", "B", "C"]);
    let decision = observe_texts(&mut state, None, &["ZZZZ", "Bx", "Cx"]);
    assert!(
        decision
            .freshness
            .iter()
            .all(|freshness| *freshness == Freshness::Unknown),
        "large revision must fall back to Unknown: {:?}",
        decision.freshness
    );
}

#[test]
fn sender_change_breaks_the_anchor() {
    let mut state = SequenceFreshness::default();
    let seeded = vec![bubble_at(0, 100, 400, 28, "同一句", "blue", "甲")];
    let _ = observe(&mut state, Some(BaselineKind::AppStart), &seeded);
    let current = vec![bubble_at(0, 100, 400, 28, "同一句", "blue", "乙")];
    let decision = observe(&mut state, None, &current);
    assert!(decision
        .freshness
        .iter()
        .all(|freshness| *freshness == Freshness::Unknown));
}

#[test]
fn color_change_breaks_the_anchor() {
    let mut state = SequenceFreshness::default();
    let seeded = vec![bubble_at(0, 100, 400, 28, "同一句", "yellow", SENDER)];
    let _ = observe(&mut state, Some(BaselineKind::AppStart), &seeded);
    let current = vec![bubble_at(0, 100, 400, 28, "同一句", "blue", SENDER)];
    let decision = observe(&mut state, None, &current);
    assert!(decision
        .freshness
        .iter()
        .all(|freshness| *freshness == Freshness::Unknown));
}

#[test]
fn x_and_size_change_still_anchor() {
    let mut state = SequenceFreshness::default();
    let seeded = vec![
        bubble_at(0, 100, 400, 28, "m1", "blue", SENDER),
        bubble_at(0, 130, 400, 28, "m2", "blue", SENDER),
    ];
    let _ = observe(&mut state, Some(BaselineKind::AppStart), &seeded);
    // x / width / height 全变，只有 y 与文本/颜色/发送者保持 → 仍锚定。
    let current = vec![
        bubble_at(50, 100, 520, 144, "m1", "blue", SENDER),
        bubble_at(20, 130, 300, 20, "m2", "blue", SENDER),
        bubble_at(0, 160, 400, 28, "@新", "blue", SENDER),
    ];
    let decision = observe(&mut state, None, &current);
    assert_new_at(&decision, &[2]);
}

#[test]
fn scroll_in_either_direction_still_anchors() {
    // 内容整体下移：delta > 0。
    let mut state = SequenceFreshness::default();
    let _ = observe_texts(&mut state, Some(BaselineKind::AppStart), &["A", "B", "C"]);
    let down = vec![
        bubble(130, "A", "blue"),
        bubble(160, "B", "blue"),
        bubble(190, "C", "blue"),
        bubble(220, "@新", "blue"),
    ];
    let decision = observe(&mut state, None, &down);
    assert_new_at(&decision, &[3]);

    // 内容整体上移且顶部滚出一条：delta < 0。
    let mut state = SequenceFreshness::default();
    let seeded = vec![
        bubble(100, "A", "blue"),
        bubble(130, "B", "blue"),
        bubble(160, "C", "blue"),
    ];
    let _ = observe(&mut state, Some(BaselineKind::AppStart), &seeded);
    let up = vec![
        bubble(100, "B", "blue"),
        bubble(130, "C", "blue"),
        bubble(160, "@新", "blue"),
    ];
    let decision = observe(&mut state, None, &up);
    assert_new_at(&decision, &[2]);
}

#[test]
fn appending_a_whole_window_leaves_no_common_anchor() {
    let mut state = SequenceFreshness::default();
    let _ = observe_texts(
        &mut state,
        Some(BaselineKind::AppStart),
        &["A", "B", "C", "D", "E"],
    );
    let decision = observe_texts(&mut state, None, &["V", "W", "X", "Y", "Z"]);
    assert!(
        decision
            .freshness
            .iter()
            .all(|freshness| *freshness == Freshness::Unknown),
        "no common anchor must be all Unknown: {:?}",
        decision.freshness
    );
    assert_eq!(decision.phase, ViewPhase::Discontinuity);
}

// ---------------------------------------------------------------------------
// 3. evidence/replay.json 的 6 组真帧用例（文本序 + 合成 y）
// ---------------------------------------------------------------------------

#[test]
fn replay_case1_4fillers_scrolled() {
    let mut state = SequenceFreshness::default();
    // 真帧 00-before（README）：甲 乙 丙 丁 @状态，作为首个基线。
    let seed = observe_texts(
        &mut state,
        Some(BaselineKind::AppStart),
        &["甲", "乙", "丙", "丁", "@状态"],
    );
    assert!(seed
        .freshness
        .iter()
        .all(|freshness| *freshness == Freshness::Unknown));

    // 01-first-AA：乙 丙 丁 @状态 @AA → 仅底部 @AA 新。
    let first = frame(&["乙", "丙", "丁", "@状态", "@AA"]);
    let decision = observe(&mut state, None, &first);
    assert_new_texts(&decision, &first, &["@AA"]);

    // 02-four-fillers：@AA 一 二 三 四 → 四条填充为新。
    let fillers = frame(&["@AA", "一", "二", "三", "四"]);
    let decision = observe(&mut state, None, &fillers);
    assert_new_texts(&decision, &fillers, &["一", "二", "三", "四"]);

    // 03-second-AA：一 二 三 四 @AA → 首条已滚出，底部 @AA 新。
    let second = frame(&["一", "二", "三", "四", "@AA"]);
    let decision = observe(&mut state, None, &second);
    assert_new_texts(&decision, &second, &["@AA"]);
}

#[test]
fn replay_case1_same_frame_identical() {
    let mut state = SequenceFreshness::default();
    // 合成前驱：顶部 @CC 与三条填充已在屏上（真帧 04 之前的状态 README 未给）。
    let seed = observe_texts(
        &mut state,
        Some(BaselineKind::AppStart),
        &["@CC", "戊", "己", "庚"],
    );
    assert!(seed
        .freshness
        .iter()
        .all(|freshness| *freshness == Freshness::Unknown));

    // 04-both-CC：同帧两条同文本 @CC，仅最底部一条为 New。
    let both = frame(&["@CC", "戊", "己", "庚", "@CC"]);
    let decision = observe(&mut state, None, &both);
    assert_eq!(new_indices(&decision), vec![4]);
    assert_eq!(decision.freshness[0], Freshness::Seen);
}

#[test]
fn replay_case2_reply_between() {
    let mut state = SequenceFreshness::default();
    // 01-first-BB 作为基线。
    let seed = observe_texts(
        &mut state,
        Some(BaselineKind::AppStart),
        &["三", "四", "@AA", "@BB"],
    );
    assert!(seed
        .freshness
        .iter()
        .all(|freshness| *freshness == Freshness::Unknown));

    // 02-reply：三 四 @AA @BB 已处理 → 只有“已处理”新。
    let reply = frame(&["三", "四", "@AA", "@BB", "已处理"]);
    let decision = observe(&mut state, None, &reply);
    assert_new_texts(&decision, &reply, &["已处理"]);

    // 03-second-BB：四 @AA @BB 已处理 @BB → 中隔回复，仅底部 @BB 新。
    let second = frame(&["四", "@AA", "@BB", "已处理", "@BB"]);
    let decision = observe(&mut state, None, &second);
    assert_eq!(new_indices(&decision), vec![4]);
}

#[test]
fn replay_case3_view_return_is_all_old() {
    let texts = ["四", "@AA", "@BB", "已处理", "@BB"];

    // 路径一：PanelReturn 基线（目标窗口恢复）→ 全部无视。
    let mut state = SequenceFreshness::default();
    let seed = observe_texts(&mut state, Some(BaselineKind::AppStart), &texts);
    assert!(seed
        .freshness
        .iter()
        .all(|freshness| *freshness == Freshness::Unknown));
    let decision = observe_texts(&mut state, Some(BaselineKind::PanelReturn), &texts);
    assert!(decision
        .freshness
        .iter()
        .all(|freshness| *freshness == Freshness::Unknown));

    // 路径二：视图消失标记 → 下一帧保守全 Unknown。
    let mut state = SequenceFreshness::default();
    let _ = observe_texts(&mut state, Some(BaselineKind::AppStart), &texts);
    state.mark_view_disappeared();
    let decision = observe_texts(&mut state, None, &texts);
    assert!(decision
        .freshness
        .iter()
        .all(|freshness| *freshness == Freshness::Unknown));
}

/// replay.json 中 s016 的长英文行（step-02 里的占位即指这一条）。
const LONG_EN: &str =
    "This is a fairly long English sentence ...y abcdefghijklmnopqrstuvwxyz01234 56789";

#[test]
fn replay_s016_scroll() {
    let mut state = SequenceFreshness::default();
    // step-00：无新增。
    let seed = observe_texts(
        &mut state,
        Some(BaselineKind::AppStart),
        &["前后有空格", LONG_EN],
    );
    assert!(seed
        .freshness
        .iter()
        .all(|freshness| *freshness == Freshness::Unknown));

    // step-02：长英文行 + 滚1 + 滚2 → 两条滚动为新。
    let step02 = frame(&[LONG_EN, "滚1", "滚2"]);
    let decision = observe(&mut state, None, &step02);
    assert_new_texts(&decision, &step02, &["滚1", "滚2"]);

    // step-06：靠 滚2 共同锚对齐，追加的 滚3..滚6 为新。
    let step06 = frame(&["滚2", "滚3", "滚4", "滚5", "滚6"]);
    let decision = observe(&mut state, None, &step06);
    assert_new_texts(&decision, &step06, &["滚3", "滚4", "滚5", "滚6"]);
}

#[test]
fn replay_s017_duplicate_adjacent() {
    let mut state = SequenceFreshness::default();
    let seed = observe_texts(
        &mut state,
        Some(BaselineKind::AppStart),
        &["滚2", "滚3", "滚4", "滚5", "滚6"],
    );
    assert!(seed
        .freshness
        .iter()
        .all(|freshness| *freshness == Freshness::Unknown));

    // dup-1：标点行首次出现 → 仅它新。
    let dup1 = frame(&["滚3", "滚4", "滚5", "滚6", "标点测试，。！？；："]);
    let decision = observe(&mut state, None, &dup1);
    assert_eq!(new_indices(&decision), vec![4]);

    // dup-2：相邻两条完全相同 → 上一帧那条已锚定，只有底部新。
    let dup2 = frame(&[
        "滚4",
        "滚5",
        "滚6",
        "标点测试，。！？；：",
        "标点测试，。！？；：",
    ]);
    let decision = observe(&mut state, None, &dup2);
    assert_eq!(new_indices(&decision), vec![4]);
    assert_eq!(decision.freshness[3], Freshness::Seen);
}
