use super::*;
use crate::geometry::Rect;
use crate::observation::chat::{ChatIdentity, ChatTimeline, RecognizedChatMessage};

fn message(text: &str, y: i32) -> RecognizedChatMessage {
    RecognizedChatMessage {
        message_type: "blue".to_string(),
        block: Rect::new(0, y, 100, 10),
        text: text.to_string(),
        sender: None,
        raw_sender: None,
    }
}

fn publish(shared: &ChatTimeline, chat: ChatIdentity, messages: Vec<RecognizedChatMessage>) {
    shared.publish(Instant::now(), chat, messages).unwrap();
}

/// 回归：外部消息的正文是纯正文，命令必须能直接提取。
///
/// 投递层**不再**把昵称拼进正文（昵称在 sender 字段里），所以命令提取
/// 只需看正文首字符，不用在任意位置找 `@`/`#`。
#[test]
fn external_message_with_sender_prefix_still_yields_the_command() {
    let shared = ChatTimeline::new();
    let business = shared.subscribe_business().unwrap();
    shared
        .inject_external(
            Instant::now(),
            crate::observation::chat::VisualSessionId::new(u64::MAX),
            "test",
            vec![external_hall_message(1, "@状态")],
        )
        .unwrap();
    let frames = business.read().unwrap();
    let text = &frames[0].messages[0].text;
    assert!(
        text.ends_with("@状态"),
        "正文应保留完整命令，实际: {text:?}"
    );
    // 与 OCR 同一条提取路径：从 @ 处截断。
    let command = super::secondary_hall_command_text(text);
    assert_eq!(
        command,
        Some("@状态"),
        "命令必须能被正确提取，实际: {command:?}"
    );
}

fn external_hall_message(
    sequence: u64,
    text: &str,
) -> miliastra_kernel::chat_source::ExternalChatMessage {
    external_hall_message_with_hall(sequence, text, None)
}

fn external_hall_message_with_hall(
    sequence: u64,
    text: &str,
    hall_id: Option<u64>,
) -> miliastra_kernel::chat_source::ExternalChatMessage {
    miliastra_kernel::chat_source::ExternalChatMessage {
        sequence,
        sent_at: None,
        channel: miliastra_kernel::chat_source::ChatChannel::current_hall(),
        hall_id,
        sender: miliastra_kernel::chat_source::ExternalSender::new(
            Some("甲".to_string()),
            Some(miliastra_kernel::chat_source::GameUid::from_u32(1001)),
        ),
        to_uid: None,
        text: text.to_string(),
    }
}

/// 构造一个「外部聊天源」语义的决策读者。
///
/// 与 `shared_reader` 的区别就是解耦后的两条判据：
/// - `sequence_floor = Some(_)` → 按 `scope` 匹配，不看屏幕桶；
/// - `baseline_cursor = None` → 不用非单调的哈希序当水位；
/// - `screen_lock` 为空实现 → 哨兵坐标不参与去重。
fn external_reader(
    shared: &ChatTimeline,
    observations: &ChatObservationShared,
    chat: ChatIdentity,
    baseline: Vec<RecognizedChatMessage>,
) -> ChatDecisionReader {
    let observation_session = observations.begin_exclusive().unwrap();
    let subscriber = shared.subscribe().unwrap();
    let refresh = shared.request_refresh().unwrap();
    publish(shared, chat.clone(), baseline);
    let baseline = shared
        .wait_for_snapshot(refresh, Duration::ZERO)
        .unwrap()
        .unwrap();
    // 外部源按业务容器绑定，不按屏幕桶。
    subscriber.bind_scope(baseline.scope.clone()).unwrap();
    ChatDecisionReader {
        listener_mode: ChatListenerMode::Primary,
        subscriber,
        scope: baseline.scope.clone(),
        any_conversation: false,
        settled_conversations: std::collections::HashSet::new(),
        sequence_floor: Some(
            baseline
                .messages
                .iter()
                .map(|message| message.source_sequence)
                .max()
                .unwrap_or(0),
        ),
        chat,
        baseline_at: baseline.captured_at,
        baseline_cursor: None,
        seen: baseline
            .messages
            .into_iter()
            .map(|message| (message.id, message.text))
            .collect(),
        screen_lock: DecisionScreenLock::default(),
        _observation_session: observation_session,
    }
}

/// 回归：外部源在线时，决策窗口**不需要**等一条新消息才能建基线。
///
/// 接管期间观察线程不发布任何帧，若窗口只能靠「刚好有人说话」完成 refresh，
/// 安静大厅里打开点歌窗口就必然 20s 超时。用服务端序号水位建基线则不受影响。
///
/// 这里直接验证读者语义：水位之前的消息不收，之后的消息收。
#[test]
fn external_reader_uses_sequence_watermark_without_waiting_for_a_frame() {
    let shared = ChatTimeline::new();
    let observations = ChatObservationShared::new();
    let observation_session = observations.begin_exclusive().unwrap();
    let subscriber = shared.subscribe().unwrap();
    let scope = crate::observation::chat::ChatScope::hall();
    subscriber.bind_scope(scope.clone()).unwrap();
    // 窗口打开时的水位是 5：之前的历史都不该进决策。
    let mut reader = ChatDecisionReader {
        listener_mode: ChatListenerMode::Primary,
        subscriber,
        scope,
        any_conversation: false,
        settled_conversations: std::collections::HashSet::new(),
        chat: ChatIdentity::SecondaryHall,
        baseline_at: Instant::now(),
        sequence_floor: Some(5),
        baseline_cursor: None,
        seen: std::collections::HashSet::new(),
        screen_lock: DecisionScreenLock::default(),
        _observation_session: observation_session,
    };
    // 水位内的旧消息与水位后的新消息一起到达。
    shared
        .inject_external(
            Instant::now(),
            crate::observation::chat::VisualSessionId::new(u64::MAX),
            "test",
            vec![
                external_hall_message(3, "@1"),
                external_hall_message(7, "@2"),
            ],
        )
        .unwrap();
    let received = reader.read_messages().unwrap();
    // 正文是纯正文：投递层不再把昵称拼进来，昵称在 sender 字段。
    assert_eq!(
        received.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
        vec!["@2"],
        "只收水位之后的消息：@1(序号3) 是历史，@2(序号7) 才是新决策"
    );
}

/// 端到端回归：外部源在线时，决策窗口必须能在**没有新消息**的情况下就绪，
/// 且随后能收到注入的决策消息。
///
/// 这条覆盖用户报告的「点歌报错：等待共享聊天确认基线超时」。
#[test]
fn external_decision_window_is_ready_without_any_incoming_message() {
    let shared = ChatTimeline::new();
    let observations = ChatObservationShared::new();
    let observation_session = observations.begin_exclusive().unwrap();
    let subscriber = shared
        .subscribe_scoped(crate::observation::chat::ChatScope::hall())
        .unwrap();
    let mut reader = ChatDecisionReader {
        listener_mode: ChatListenerMode::Primary,
        subscriber,
        scope: crate::observation::chat::ChatScope::hall(),
        any_conversation: false,
        settled_conversations: std::collections::HashSet::new(),
        chat: ChatIdentity::PrimaryHall,
        baseline_at: Instant::now(),
        sequence_floor: Some(4),
        baseline_cursor: None,
        seen: std::collections::HashSet::new(),
        screen_lock: DecisionScreenLock::default(),
        _observation_session: observation_session,
    };
    // 窗口已就绪：不需要任何帧。此刻读应当为空（没有新消息）。
    assert!(reader.read_messages().unwrap().is_empty());
    // 用户在窗口内回 @1（序号 5，晚于水位 4）。
    shared
        .inject_external(
            Instant::now(),
            crate::observation::chat::VisualSessionId::new(u64::MAX),
            "test",
            vec![external_hall_message(5, "@1")],
        )
        .unwrap();
    let got = reader.read_messages().unwrap();
    assert_eq!(got.len(), 1, "窗口必须收到 @1，实际收到 {} 条", got.len());
    assert!(got[0].text.contains("@1"));
}
/// 回归：外部源的大厅消息**带 hallId** 时，决策窗口也必须收得到。
///
/// 这是用户报告「点歌报错：等待共享聊天确认基线超时」的一个隐蔽成因：
/// 窗口绑的是 `ChatScope::hall()`（`Hall{None}`），而外部消息带 `hallId`
/// （`Hall{Some(id)}`）。若按严格相等比较，两者永不相等，整窗收不到任何消息。
/// 之前的夹具都用 `hall_id: None`，恰好把这个问题掩盖了。
#[test]
fn external_decision_window_receives_messages_carrying_a_hall_id() {
    let shared = ChatTimeline::new();
    let observations = ChatObservationShared::new();
    let observation_session = observations.begin_exclusive().unwrap();
    let subscriber = shared
        .subscribe_scoped(crate::observation::chat::ChatScope::hall())
        .unwrap();
    let mut reader = ChatDecisionReader {
        listener_mode: ChatListenerMode::Primary,
        subscriber,
        scope: crate::observation::chat::ChatScope::hall(),
        any_conversation: false,
        settled_conversations: std::collections::HashSet::new(),
        chat: ChatIdentity::PrimaryHall,
        baseline_at: Instant::now(),
        sequence_floor: Some(10),
        baseline_cursor: None,
        seen: std::collections::HashSet::new(),
        screen_lock: DecisionScreenLock::default(),
        _observation_session: observation_session,
    };
    shared
        .inject_external(
            Instant::now(),
            crate::observation::chat::VisualSessionId::new(u64::MAX),
            "test",
            vec![external_hall_message_with_hall(11, "@1", Some(9527))],
        )
        .unwrap();
    let got = reader.read_messages().unwrap();
    assert_eq!(
        got.len(),
        1,
        "带 hallId 的大厅消息也必须进决策窗口，实际收到 {} 条",
        got.len()
    );
    assert!(got[0].text.contains("@1"));
}
/// 回归：外部源窗口在**没有新消息**时也能就绪，且之后能收到决策输入。
///
/// 这是用户报告的「点歌报错：等待共享聊天确认基线超时」的直接覆盖。
/// 外部接管期间观察线程不发布帧，若窗口只能靠「刚好有人说话」推动，
/// 安静大厅里必然超时。
#[test]
fn external_window_is_ready_then_receives_the_decision() {
    let shared = ChatTimeline::new();
    let observations = ChatObservationShared::new();
    let observation_session = observations.begin_exclusive().unwrap();
    let subscriber = shared
        .subscribe_scoped(crate::observation::chat::ChatScope::hall())
        .unwrap();
    // 窗口打开：水位 10，不需要任何帧。
    let mut reader = ChatDecisionReader {
        listener_mode: ChatListenerMode::Primary,
        subscriber,
        scope: crate::observation::chat::ChatScope::hall(),
        any_conversation: false,
        settled_conversations: std::collections::HashSet::new(),
        chat: ChatIdentity::SecondaryHall,
        baseline_at: Instant::now(),
        sequence_floor: Some(10),
        baseline_cursor: None,
        seen: std::collections::HashSet::new(),
        screen_lock: DecisionScreenLock::default(),
        _observation_session: observation_session,
    };
    // 就绪即可读，不该阻塞。
    assert!(reader.read_messages().unwrap().is_empty());
    // 用户在窗口内回 @1。
    shared
        .inject_external(
            Instant::now(),
            crate::observation::chat::VisualSessionId::new(u64::MAX),
            "test",
            vec![external_hall_message(11, "@1")],
        )
        .unwrap();
    let got = reader.read_messages().unwrap();
    assert_eq!(got.len(), 1, "窗口必须收到 @1，实际 {} 条", got.len());
    assert!(got[0].text.ends_with("@1"), "实际: {:?}", got[0].text);
}
/// 回归：外部消息补了「昵称：」前缀后，`#` 命令仍能被提取。
///
/// `secondary_hall_command_text` 原本要求 `#` 在行首；外部源统一补前缀后
/// `#` 就跑到行中间了，若坚持行首判断，`#状态`/`#塔罗牌`/`#手牌` 会整批失效。
#[test]
fn external_message_with_sender_prefix_still_yields_hash_commands() {
    let shared = ChatTimeline::new();
    let business = shared.subscribe_business().unwrap();
    shared
        .inject_external(
            Instant::now(),
            crate::observation::chat::VisualSessionId::new(u64::MAX),
            "test",
            vec![external_hall_message(1, "#状态")],
        )
        .unwrap();
    let frames = business.read().unwrap();
    let text = &frames[0].messages[0].text;
    assert!(
        text.contains("#状态"),
        "正文里必须保留完整命令，实际: {text:?}"
    );
    assert_eq!(
        super::secondary_hall_command_text(text),
        Some("#状态"),
        "# 命令必须能被提取，实际正文: {text:?}"
    );
}
/// 回归：窗口在 OCR 下打开、随后外部源接管，仍要收到外部帧。
///
/// 若匹配也按 `chat` 比，外部帧（chat=None）
/// 会被整批拒绝，窗口一条收不到直到超时。归属必须一律按 scope 判。
#[test]
fn screen_opened_window_still_receives_frames_after_external_takeover() {
    let shared = ChatTimeline::new();
    let observations = ChatObservationShared::new();
    let observation_session = observations.begin_exclusive().unwrap();
    let subscriber = shared.subscribe().unwrap();
    // 用 OCR 的基线开窗：sequence_floor=None、绑二级大厅桶。
    let baseline = shared
        .publish(
            Instant::now(),
            ChatIdentity::SecondaryHall,
            vec![message("候选", 10)],
        )
        .unwrap();
    subscriber
        .bind_chat(ChatIdentity::SecondaryHall, baseline.visual_session)
        .unwrap();
    let mut reader = ChatDecisionReader {
        listener_mode: ChatListenerMode::Primary,
        subscriber,
        scope: baseline.scope.clone(),
        any_conversation: false,
        chat: ChatIdentity::SecondaryHall,
        settled_conversations: std::collections::HashSet::new(),
        baseline_at: baseline.captured_at,
        sequence_floor: None,
        baseline_cursor: Some((
            baseline.visual_session,
            baseline
                .messages
                .iter()
                .map(|m| m.id.bubble_sequence.get())
                .max()
                .unwrap_or(0),
        )),
        seen: baseline
            .messages
            .into_iter()
            .map(|m| (m.id, m.text))
            .collect(),
        screen_lock: DecisionScreenLock::default(),
        _observation_session: observation_session,
    };
    // 外部源接管，注入一条大厅消息。
    shared
        .inject_external(
            Instant::now(),
            crate::observation::chat::VisualSessionId::new(u64::MAX),
            "test",
            vec![external_hall_message(5, "@1")],
        )
        .unwrap();
    let got = reader.read_messages().unwrap();
    assert_eq!(got.len(), 1, "接管后外部帧必须送达，实际 {} 条", got.len());
}
/// 回归：多会话窗口首次见到某会话时只登记、不交付。
///
/// 该会话的整屏气泡都是「窗口打开后才渲染出来的历史」，时间戳晚于基线，
/// 照单全收会把首屏历史当成新决策（MUST_READ 第 2 节被绕过）。
/// 这条判据曾写反（`!insert()`），导致首帧泄漏、第二帧起永久丢弃，
/// 而任何测试都没覆盖到——所以这里同时固定两个方向。
#[test]
fn multiple_conversations_skips_first_frame_then_accepts_later_ones() {
    let shared = ChatTimeline::new();
    let observations = ChatObservationShared::new();
    let observation_session = observations.begin_exclusive().unwrap();
    let subscriber = shared.subscribe().unwrap();
    let baseline = shared
        .publish(
            Instant::now(),
            ChatIdentity::PrimaryHall,
            vec![message("基线", 10)],
        )
        .unwrap();
    let mut reader = ChatDecisionReader {
        listener_mode: ChatListenerMode::Primary,
        subscriber,
        scope: baseline.scope.clone(),
        any_conversation: true,
        chat: ChatIdentity::PrimaryHall,
        settled_conversations: std::collections::HashSet::new(),
        baseline_at: baseline.captured_at,
        sequence_floor: None,
        baseline_cursor: None,
        seen: baseline
            .messages
            .into_iter()
            .map(|m| (m.id, m.text))
            .collect(),
        screen_lock: DecisionScreenLock::default(),
        _observation_session: observation_session,
    };
    // 首次见到这个会话：整屏历史，必须跳过。
    publish(
        &shared,
        ChatIdentity::Friend("甲".into()),
        vec![message("@确认", 20)],
    );
    assert!(
        reader.read_messages().unwrap().is_empty(),
        "首次见到某会话时，它的整屏历史不得作为决策输入"
    );
    // 第二次见到：这才是真正的回复。
    publish(
        &shared,
        ChatIdentity::Friend("甲".into()),
        vec![message("@确认", 40)],
    );
    let got = reader.read_messages().unwrap();
    assert_eq!(got.len(), 1, "第二帧起必须正常交付，实际 {} 条", got.len());
}
/// 回归：外部源开窗后掉线、OCR 回落，窗口不能在回落瞬间开始漏帧。
///
/// 基线来自外部源时，游标是服务端序号水位，与是哪块面板无关。
/// 若仍按开窗时猜的 `chat` 过滤，实际驻留面板与开窗时不同就会整窗漏帧到超时。
#[test]
fn external_opened_window_keeps_receiving_after_ocr_fallback() {
    let shared = ChatTimeline::new();
    let observations = ChatObservationShared::new();
    let observation_session = observations.begin_exclusive().unwrap();
    let subscriber = shared
        .subscribe_scoped(crate::observation::chat::ChatScope::hall())
        .unwrap();
    // 外部开窗：sequence_floor=Some(_)，chat 只是开窗时猜的面板。
    let mut reader = ChatDecisionReader {
        listener_mode: ChatListenerMode::Primary,
        subscriber,
        scope: crate::observation::chat::ChatScope::hall(),
        any_conversation: false,
        chat: ChatIdentity::SecondaryHall,
        settled_conversations: std::collections::HashSet::new(),
        baseline_at: Instant::now(),
        sequence_floor: Some(0),
        baseline_cursor: None,
        seen: std::collections::HashSet::new(),
        screen_lock: DecisionScreenLock::default(),
        _observation_session: observation_session,
    };
    // 外部源掉线，OCR 接管，且拍到的是一级面板（与开窗时猜的二级不同）。
    publish(
        &shared,
        ChatIdentity::PrimaryHall,
        vec![message("甲：@1", 20)],
    );
    let got = reader.read_messages().unwrap();
    assert!(
        !got.is_empty(),
        "回落 OCR 后窗口仍须收到消息，不能因面板号不同而漏帧"
    );
}
/// 回归：多会话窗口下，**外部帧**（chat=None）的首帧必须直接交付。
///
/// 首帧防护是为 OCR 的「整屏历史」写的——那是窗口打开后重绘出来的旧气泡。
/// 外部聊天源每条消息只投递一次且是真实事件，首帧就是决策输入本身；
/// 若也跳过，这一次确认被永久吞掉，窗口只能等到超时。
#[test]
fn multiple_conversations_delivers_first_external_frame() {
    let shared = ChatTimeline::new();
    let observations = ChatObservationShared::new();
    let observation_session = observations.begin_exclusive().unwrap();
    let subscriber = shared
        .subscribe_scoped(crate::observation::chat::ChatScope::hall())
        .unwrap();
    let mut reader = ChatDecisionReader {
        listener_mode: ChatListenerMode::Primary,
        subscriber,
        scope: crate::observation::chat::ChatScope::hall(),
        any_conversation: true,
        chat: ChatIdentity::SecondaryHall,
        settled_conversations: std::collections::HashSet::new(),
        baseline_at: Instant::now(),
        sequence_floor: Some(0),
        baseline_cursor: None,
        seen: std::collections::HashSet::new(),
        screen_lock: DecisionScreenLock::default(),
        _observation_session: observation_session,
    };
    shared
        .inject_external(
            Instant::now(),
            crate::observation::chat::VisualSessionId::new(u64::MAX),
            "test",
            vec![external_hall_message(1, "@确认")],
        )
        .unwrap();
    let got = reader.read_messages().unwrap();
    assert_eq!(
        got.len(),
        1,
        "外部帧每条只投递一次，首帧跳过等于永久吞掉真实确认；实际 {} 条",
        got.len()
    );
}

/// 回归：外部源同一决策文本连发两次，第二次必须仍然生效。
///
/// 外部消息的 block 是统一哨兵坐标，`DecisionScreenLock::default()` 会按
/// (文本, bottom) 把第二次判成「已消费」——但外部每条消息只投递一次，
/// 重复文本就是用户真的又发了一次（如连续两次 `@换一批`）。
#[test]
fn external_repeated_decision_text_is_accepted_twice() {
    use crate::observation::chat::ChatMessage as Observed;
    use crate::observation::decision::DecisionScreenLock;
    let sentinel = crate::geometry::Rect::new(0, i32::MIN / 2, 1, 1);
    let msg = Observed {
        message_type: "blue".to_string(),
        block: sentinel,
        text: "@换一批".to_string(),
        sender: None,
    };
    let mut lock = DecisionScreenLock::inert();
    assert!(lock.accept_once(&msg), "外部源第一次决策必须生效");
    assert!(lock.accept_once(&msg), "外部源第二次同文本决策也必须生效");
}
/// 回归：外部源的决策读者必须按业务范围收消息，不能按屏幕桶。
///
/// 外部大厅消息恒被映射成 `SecondaryHall`，而一级值守下的窗口期望 `PrimaryHall`。
/// 若按 chat 匹配，用户发的 `@1` 会被判成「另一个聊天」而永远收不到，
/// 表现为「等待共享聊天确认基线超时」。
#[test]
fn external_decision_reader_matches_by_scope_not_screen_bucket() {
    let shared = ChatTimeline::new();
    let observations = ChatObservationShared::new();
    // 窗口以一级大厅为宿主打开，但外部消息落在 SecondaryHall 桶里。
    let mut reader = external_reader(
        &shared,
        &observations,
        ChatIdentity::PrimaryHall,
        vec![message("基线", 40)],
    );
    shared
        .inject_external(
            Instant::now(),
            crate::observation::chat::VisualSessionId::new(u64::MAX),
            "test",
            vec![external_hall_message(2, "@1")],
        )
        .unwrap();
    let received = reader.read_messages().unwrap();
    assert!(
        received.iter().any(|m| m.text.contains("@1")),
        "外部源的决策消息必须按 scope 收进来，实际收到: {:?}",
        received.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
}

fn shared_reader(
    shared: &ChatTimeline,
    observations: &ChatObservationShared,
    chat: ChatIdentity,
    baseline: Vec<RecognizedChatMessage>,
) -> ChatDecisionReader {
    let observation_session = observations.begin_exclusive().unwrap();
    let subscriber = shared.subscribe().unwrap();
    let refresh = shared.request_refresh().unwrap();
    publish(shared, chat.clone(), baseline);
    let baseline = shared
        .wait_for_snapshot(refresh, Duration::ZERO)
        .unwrap()
        .unwrap();
    subscriber
        .bind_chat(chat.clone(), baseline.visual_session)
        .unwrap();
    let baseline_cursor = Some((
        baseline.visual_session,
        baseline
            .messages
            .iter()
            .map(|message| message.id.bubble_sequence.get())
            .max()
            .unwrap_or(0),
    ));
    ChatDecisionReader {
        listener_mode: ChatListenerMode::Primary,
        subscriber,
        scope: baseline.scope.clone(),
        any_conversation: false,
        settled_conversations: std::collections::HashSet::new(),
        sequence_floor: None,
        chat,
        baseline_at: baseline.captured_at,
        baseline_cursor,
        seen: baseline
            .messages
            .into_iter()
            .map(|message| (message.id, message.text))
            .collect(),
        screen_lock: DecisionScreenLock::default(),
        _observation_session: observation_session,
    }
}

#[test]
fn shared_decision_subscribers_preserve_business_delivery_in_both_halls() {
    for chat in [ChatIdentity::PrimaryHall, ChatIdentity::SecondaryHall] {
        let shared = ChatTimeline::new();
        let observations = ChatObservationShared::new();
        let business = shared.subscribe().unwrap();
        let mut reader = shared_reader(
            &shared,
            &observations,
            chat.clone(),
            vec![message("anchor", 0)],
        );
        business.read().unwrap();
        publish(
            &shared,
            chat,
            vec![message("anchor", 0), message("user: @skip", 20)],
        );
        let snapshots = business.read().unwrap();
        let fresh = snapshots[0]
            .messages
            .iter()
            .find(|message| message.text == "user: @skip")
            .unwrap();
        assert!(fresh.is_new);
        assert!(shared.acknowledge_business(&fresh.id).unwrap());
        reader.wait_for_input(500, Duration::from_secs(30)).unwrap();
        let messages = reader.read_messages().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "user: @skip");
        assert!(reader.accept_once(&messages[0]));
        assert!(!reader.accept_once(&messages[0]));
        assert!(reader.read_messages().unwrap().is_empty());
    }
}

#[test]
fn shared_reader_filters_baseline_revisions_and_accepts_new_duplicate_rows() {
    for chat in [ChatIdentity::PrimaryHall, ChatIdentity::SecondaryHall] {
        let shared = ChatTimeline::new();
        let observations = ChatObservationShared::new();
        let mut reader = shared_reader(
            &shared,
            &observations,
            chat.clone(),
            vec![message("anchor", 0), message("user: @confirn", 20)],
        );
        publish(
            &shared,
            chat.clone(),
            vec![message("anchor", 0), message("user: @confirm", 20)],
        );
        assert!(reader.read_messages().unwrap().is_empty());
        publish(
            &shared,
            chat,
            vec![
                message("anchor", 0),
                message("user: @confirm", 20),
                message("user: @confirm", 40),
            ],
        );
        let fresh = reader.read_messages().unwrap();
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].text, "user: @confirm");
    }
}

#[test]
fn shared_reader_receives_input_under_formal_ownership_without_scan_permit() {
    let coordinator = UiCoordinator::new();
    let _formal = coordinator.acquire_formal();
    let shared = ChatTimeline::new();
    let observations = ChatObservationShared::new();
    let mut reader = shared_reader(
        &shared,
        &observations,
        ChatIdentity::PrimaryHall,
        vec![message("anchor", 0)],
    );
    assert!(coordinator.try_acquire_scan().is_none());
    publish(
        &shared,
        ChatIdentity::PrimaryHall,
        vec![message("anchor", 0), message("user: @AI", 20)],
    );
    assert_eq!(reader.read_messages().unwrap().len(), 1);
    assert!(coordinator.try_acquire_scan().is_none());
    assert!(observations.begin_exclusive().is_err());
    drop(reader);
    assert!(observations.begin_exclusive().is_ok());
    assert!(coordinator.try_acquire_scan().is_none());
}

#[test]
fn shared_reader_ignores_long_foreign_state_but_reports_observation_failure() {
    let shared = ChatTimeline::new();
    let observations = ChatObservationShared::new();
    let mut reader = shared_reader(
        &shared,
        &observations,
        ChatIdentity::PrimaryHall,
        vec![message("anchor", 0)],
    );
    for _ in 0..64 {
        publish(&shared, ChatIdentity::PublicChannel, Vec::new());
    }
    assert!(reader.read_messages().unwrap().is_empty());
    publish(
        &shared,
        ChatIdentity::PrimaryHall,
        vec![message("anchor", 0), message("user: @confirm", 20)],
    );
    assert_eq!(reader.read_messages().unwrap()[0].text, "user: @confirm");
    drop(reader);

    let failing = ChatTimeline::new();
    let failure_observations = ChatObservationShared::new();
    let mut failed_reader = shared_reader(
        &failing,
        &failure_observations,
        ChatIdentity::PrimaryHall,
        vec![message("anchor", 0)],
    );
    let started = Instant::now();
    failing.report_failure("capture unavailable").unwrap();
    assert!(
        failed_reader
            .wait_for_input(500, Duration::from_secs(30))
            .is_err()
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(failed_reader.read_messages().is_err());
}

#[test]
fn shared_reader_accepts_corrected_text_for_a_new_bubble() {
    let shared = ChatTimeline::new();
    let observations = ChatObservationShared::new();
    let mut reader = shared_reader(
        &shared,
        &observations,
        ChatIdentity::PrimaryHall,
        vec![message("anchor", 0)],
    );
    publish(
        &shared,
        ChatIdentity::PrimaryHall,
        vec![message("anchor", 0), message("user: @confirn", 20)],
    );
    assert_eq!(reader.read_messages().unwrap()[0].text, "user: @confirn");
    publish(
        &shared,
        ChatIdentity::PrimaryHall,
        vec![message("anchor", 0), message("user: @confirm", 20)],
    );
    assert_eq!(reader.read_messages().unwrap()[0].text, "user: @confirm");
    assert!(reader.read_messages().unwrap().is_empty());
}

#[test]
fn shared_reader_does_not_replay_pending_messages_from_its_fresh_baseline() {
    let shared = ChatTimeline::new();
    let observations = ChatObservationShared::new();
    publish(
        &shared,
        ChatIdentity::PrimaryHall,
        vec![message("anchor", 0)],
    );
    publish(
        &shared,
        ChatIdentity::PrimaryHall,
        vec![message("anchor", 0), message("old pending", 20)],
    );
    let mut reader = shared_reader(
        &shared,
        &observations,
        ChatIdentity::PrimaryHall,
        vec![message("anchor", 0), message("old pending", 20)],
    );
    publish(
        &shared,
        ChatIdentity::PrimaryHall,
        vec![
            message("anchor", 0),
            message("old pending", 20),
            message("new input", 40),
        ],
    );
    let messages = reader.read_messages().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text, "new input");
}

/// 回归：外部消息正文里的 `@` 不能把闲聊变成命令。
///
/// 曾经的实现让投递层把「昵称：」拼进正文，命令提取只好在任意位置找 `@`，
/// 于是「今天天气不错@某人」被从 `@` 处截断成命令 `@某人` 进入派发。
/// 现在正文是纯正文，提取只认首字符。
#[test]
fn external_chatter_mentioning_at_is_not_truncated_into_a_command() {
    let shared = ChatTimeline::new();
    let observations = ChatObservationShared::new();
    let observation_session = observations.begin_exclusive().unwrap();
    let scope = crate::observation::chat::ChatScope::hall();
    let subscriber = shared.subscribe_scoped(scope.clone()).unwrap();
    let mut reader = ChatDecisionReader {
        listener_mode: ChatListenerMode::Primary,
        subscriber,
        scope,
        any_conversation: false,
        settled_conversations: std::collections::HashSet::new(),
        chat: ChatIdentity::PrimaryHall,
        baseline_at: Instant::now(),
        sequence_floor: Some(1),
        baseline_cursor: None,
        seen: std::collections::HashSet::new(),
        screen_lock: DecisionScreenLock::default(),
        _observation_session: observation_session,
    };
    shared
        .inject_external(
            Instant::now(),
            crate::observation::chat::VisualSessionId::new(u64::MAX),
            "test",
            vec![external_hall_message(2, "今天天气不错@某人")],
        )
        .unwrap();
    let received = reader.read_messages().unwrap();
    assert_eq!(
        received.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
        vec!["今天天气不错@某人"],
        "正文必须原样交付：不补前缀，也不从 @ 处截断"
    );
}
