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
/// - `from_screen = false` → 按 `scope` 匹配，不看屏幕桶；
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
    subscriber
        .bind_scope(baseline.scope.clone(), baseline.visual_session)
        .unwrap();
    ChatDecisionReader {
        listener_mode: ChatListenerMode::Primary,
        subscriber,
        scope: baseline.scope.clone(),
        from_screen: false,
        any_conversation: false,
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
    subscriber
        .bind_scope(
            scope.clone(),
            crate::observation::chat::VisualSessionId::new(0),
        )
        .unwrap();
    // 窗口打开时的水位是 5：之前的历史都不该进决策。
    let mut reader = ChatDecisionReader {
        listener_mode: ChatListenerMode::Primary,
        subscriber,
        scope,
        from_screen: false,
        any_conversation: false,
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
        from_screen: false,
        any_conversation: false,
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
        from_screen: false,
        any_conversation: false,
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
        from_screen: true,
        any_conversation: false,
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
