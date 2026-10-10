use super::*;
use crate::observation::chat::{ChatIdentity, SharedChatInput, SharedChatOcr};
use crate::ui::geometry::Rect;

fn message(text: &str, y: i32) -> SharedChatInput {
    SharedChatInput {
        message_type: "blue".to_string(),
        block: Rect::new(0, y, 100, 10),
        text: text.to_string(),
        sender: None,
        raw_sender: None,
    }
}

fn publish(shared: &SharedChatOcr, chat: ChatIdentity, messages: Vec<SharedChatInput>) {
    shared.publish(Instant::now(), chat, messages).unwrap();
}

fn shared_reader(
    shared: &SharedChatOcr,
    observations: &ChatObservationShared,
    chat: ChatIdentity,
    baseline: Vec<SharedChatInput>,
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
        let shared = SharedChatOcr::new();
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
        let shared = SharedChatOcr::new();
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
    let shared = SharedChatOcr::new();
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
fn shared_reader_reports_failure_and_chat_change_without_waiting_for_timeout() {
    for change_chat in [false, true] {
        let shared = SharedChatOcr::new();
        let observations = ChatObservationShared::new();
        let mut reader = shared_reader(
            &shared,
            &observations,
            ChatIdentity::PrimaryHall,
            vec![message("anchor", 0)],
        );
        if change_chat {
            publish(&shared, ChatIdentity::SecondaryHall, Vec::new());
        } else {
            shared.report_failure("capture unavailable").unwrap();
        }
        let started = Instant::now();
        assert!(reader.wait_for_input(500, Duration::from_secs(30)).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(reader.read_messages().is_err());
        drop(reader);
        let mut next = shared_reader(
            &shared,
            &observations,
            ChatIdentity::PrimaryHall,
            vec![message("anchor", 0)],
        );
        assert!(next.read_messages().unwrap().is_empty());
    }
}

#[test]
fn shared_reader_accepts_corrected_text_for_a_new_bubble() {
    let shared = SharedChatOcr::new();
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
    let shared = SharedChatOcr::new();
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
