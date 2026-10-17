//! Pure-state/UTF-8 regressions: no synthetic visual frames or game input.
use super::*;
use crate::runtime::scheduler::FormalSchedulerSnapshot;

fn observer_push() -> ReadOnlyObserver {
    let observer = ReadOnlyObserver::default();
    observer.set_enabled_state(true, ReadOnlyState::Push);
    observer
}

fn formal(id: u64, status: &str, result: Option<String>) -> FormalTaskSnapshot {
    FormalTaskSnapshot {
        id,
        label: "private label must never escape".into(),
        status: status.into(),
        queued_at_ms: 1,
        started_at_ms: Some(2),
        finished_at_ms: (status == "completed").then_some(3),
        elapsed_ms: 0,
        cancellation_requested: false,
        result,
    }
}

fn section_all(observer: &ReadOnlyObserver, id: &str, section: &str, budget: usize) -> Value {
    let mut cursor = 0;
    let mut revision = None;
    let mut text = String::new();
    loop {
        let response = observer
            .read_section(id, section, cursor, 7, budget, revision)
            .unwrap();
        assert_eq!(response["ok"], true, "{response}");
        assert!(serde_json::to_vec(&response).unwrap().len() <= budget);
        let page = &response["evidence"]["page"];
        let current_revision = page["revision"].as_u64().unwrap();
        if let Some(revision) = revision {
            assert_eq!(current_revision, revision);
        }
        revision = Some(current_revision);
        assert_eq!(page["cursor"], cursor);
        for item in page["items"].as_array().unwrap() {
            assert_eq!(item["section"], section);
            assert_eq!(item["offset"], text.len());
            text.push_str(item["text"].as_str().unwrap());
        }
        let next = page["nextCursor"].as_u64().unwrap() as usize;
        assert_eq!(next, text.len());
        if page["done"] == true {
            break;
        }
        assert!(next > cursor);
        cursor = next;
    }
    serde_json::from_str(&text).unwrap()
}

#[test]
fn large_unicode_sections_roundtrip_fixed_revision_and_whole_envelope() {
    let observer = observer_push();
    let original = "汉字🙂é\"\\\n".repeat(3000);
    let task = json!({"kind": "largeUTF8", "parameters": {"text": original}, "ocr": {
        "text": "@点歌晴天🙂\n第二行", "boxes": [{"text": "汉字", "x": 1}]}});
    assert!(serde_json::to_vec(&task).unwrap().len() <= MAX_PAYLOAD_BYTES);
    observer.record_trigger(1, task.clone(), None);
    let parsed = section_all(&observer, "readonly-1", "parameters", 701);
    assert_eq!(parsed["task"]["parameters"], task["parameters"]);
    assert!(parsed["task"].get("ocr").is_none());
    assert_eq!(
        section_all(&observer, "readonly-1", "ocr", 701),
        task["ocr"]
    );
    let metadata = section_all(&observer, "readonly-1", "metadata", 701);
    assert!(metadata.get("task").is_none());
    assert!(metadata.get("history").is_none());
    assert!(!metadata.to_string().contains("imagePngBase64"));
    let history = section_all(&observer, "readonly-1", "history", 701);
    assert_eq!(history["history"].as_array().unwrap().len(), 1);
}

#[test]
fn section_tiny_budget_no_advance_and_invalid_boundary_revision() {
    let observer = observer_push();
    observer.record_trigger(1, json!({"text": "汉字🙂"}), None);
    let tiny = observer
        .read_section("readonly-1", "parameters", 0, 10, 1, None)
        .unwrap();
    assert_eq!(tiny["error"], "page_budget_too_small");
    assert_eq!(tiny["nextCursor"], 0);
    let page = observer
        .read_section("readonly-1", "parameters", 0, 10, 8192, None)
        .unwrap();
    let fragment = page["evidence"]["page"]["items"][0]["text"]
        .as_str()
        .unwrap();
    let invalid = fragment.find('汉').unwrap() + 1;
    assert!(
        observer
            .read_section("readonly-1", "parameters", invalid, 10, 8192, None)
            .unwrap_err()
            .starts_with("invalid_cursor")
    );
    let revision = page["evidence"]["revision"].as_u64().unwrap();
    observer.record_formal_status(&formal(1, "completed", Some("done".into())));
    assert!(
        observer
            .read_section("readonly-1", "parameters", 0, 10, 8192, Some(revision))
            .unwrap_err()
            .starts_with("revision_conflict")
    );
}

#[test]
fn age_changes_do_not_change_revision_or_serialized_metadata_section() {
    let observer = observer_push();
    observer.record_trigger(1, json!({}), None);
    {
        let mut data = observer.data.lock().unwrap();
        data.entries[0].source = Some(SourceIdentity {
            captured_at: Instant::now(),
            metadata: json!({"frameId": 7, "canvas": "rawGame"}),
        });
    }
    let first = observer
        .read_section("readonly-1", "metadata", 0, 100, 8192, None)
        .unwrap();
    let age_before = observer.entry_metadata("readonly-1").unwrap()["source"]["ageMillis"]
        .as_u64()
        .unwrap();
    observer.data.lock().unwrap().entries[0]
        .source
        .as_mut()
        .unwrap()
        .captured_at = Instant::now() - std::time::Duration::from_secs(60);
    assert!(
        observer.entry_metadata("readonly-1").unwrap()["source"]["ageMillis"]
            .as_u64()
            .unwrap()
            > age_before
    );
    let revision = first["evidence"]["revision"].as_u64().unwrap();
    assert_eq!(
        observer
            .read_section("readonly-1", "metadata", 0, 100, 8192, Some(revision))
            .unwrap(),
        first
    );
}

#[test]
fn image_loss_keeps_metadata_and_terminal_from_complete_formal_source() {
    let observer = observer_push();
    observer.record_trigger(1, json!({"kind": "command"}), None);
    {
        let mut data = observer.data.lock().unwrap();
        data.entries[0].image_retained = true;
        data.entries[0].source = Some(SourceIdentity {
            captured_at: Instant::now(),
            metadata: json!({"frameId": 77}),
        });
        mark_image_evicted(&mut data, "readonly-1");
        refresh_entries(&mut data);
    }
    let old_revision = observer.entry_metadata("readonly-1").unwrap()["revision"]
        .as_u64()
        .unwrap();
    let mut all: Vec<_> = (2..=1101)
        .rev()
        .map(|id| formal(id, "queued", None))
        .collect();
    all.push(formal(
        1,
        "completed",
        Some("terminal outside display and cache".into()),
    ));
    observer.record_status(&TaskEngineProjection {
        generation: 1,
        scheduler: FormalSchedulerSnapshot::default(),
        formal_statuses: all,
        dropped_formal_statuses: 0,
        diagnostics: Arc::from([]),
    });
    let metadata = observer.entry_metadata("readonly-1").unwrap();
    assert_eq!(metadata["status"]["status"], "completed");
    assert_eq!(
        metadata["status"]["result"],
        "terminal outside display and cache"
    );
    assert_eq!(metadata["sourceFrameId"], 77);
    assert_eq!(metadata["imageRetained"], false);
    assert_eq!(metadata["missingReason"], "source_frame_evicted");
    assert!(metadata["revision"].as_u64().unwrap() > old_revision);
    assert!(
        observer
            .read_image("readonly-1")
            .unwrap_err()
            .contains("source_frame_evicted")
    );
    assert_eq!(
        observer.summary_snapshot()["loss"]["preReceiptStatusesOmitted"],
        101
    );
    assert_eq!(
        observer.read_entry("readonly-1").unwrap()["status"]["status"],
        "completed"
    );
}

#[test]
fn entries_task_filter_and_budget_never_cross_an_unsent_matching_event() {
    let observer = observer_push();
    observer.record_trigger(99, json!({}), None); // sequence 1, unmatched
    observer.record_trigger(7, json!({}), None); // sequence 2, matched
    observer.record_formal_status(&formal(99, "running", None)); // sequence 3, unmatched
    observer.record_formal_status(&formal(7, "running", Some("汉🙂".repeat(800)))); // sequence 4, matched
    observer.record_trigger(88, json!({}), None); // sequence 5, unmatched
    let one = observer
        .list_page(0, 1, 8192, Some(7), Some(observer.instance()))
        .unwrap();
    let budget = serde_json::to_vec(&one).unwrap().len() + 2;
    let prefix = observer
        .list_page(0, 100, budget, Some(7), Some(observer.instance()))
        .unwrap();
    assert_eq!(prefix["ok"], true);
    assert!(serde_json::to_vec(&prefix).unwrap().len() <= budget);
    assert_eq!(prefix["entries"]["events"].as_array().unwrap().len(), 1);
    assert_eq!(prefix["entries"]["events"][0]["sequence"], 2);
    assert_eq!(prefix["entries"]["nextCursor"], 3);
    assert_eq!(prefix["entries"]["hasMore"], true);
    let rest = observer
        .list_page(3, 10, 8192, Some(7), Some(observer.instance()))
        .unwrap();
    assert_eq!(rest["entries"]["events"][0]["sequence"], 4);
    assert_eq!(rest["entries"]["nextCursor"], 5);
    let tiny = observer
        .list_page(0, 10, 1, Some(7), Some(observer.instance()))
        .unwrap();
    assert_eq!(tiny["nextCursor"], 0);
    assert_eq!(tiny["error"], "page_budget_too_small");
    let absent = observer
        .list_page(0, 10, 8192, Some(123), Some(observer.instance()))
        .unwrap();
    assert_eq!(absent["entries"]["nextCursor"], 5);
    assert!(absent["entries"]["events"].as_array().unwrap().is_empty());
}

#[test]
fn entries_prefix_paging_unified_instance_restart_reset_and_retention_gap() {
    let observer = observer_push();
    let instance = observer.instance().to_owned();
    for id in 1..=270 {
        observer.record_trigger(id, json!({}), None);
    }
    let mut since = 0;
    let mut sequences = Vec::new();
    loop {
        let page = observer
            .list_page(since, 3, 8192, None, Some(&instance))
            .unwrap();
        assert_eq!(page["ok"], true);
        assert_eq!(page["entries"]["instance"], instance);
        if since == 0 {
            assert_eq!(page["entries"]["gap"], true);
        }
        sequences.extend(
            page["entries"]["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["sequence"].as_u64().unwrap()),
        );
        since = page["entries"]["nextCursor"].as_u64().unwrap();
        if page["entries"]["hasMore"] == false {
            break;
        }
    }
    assert_eq!(sequences, (15..=270).collect::<Vec<_>>());
    assert_eq!(observer.getstate()["instance"], instance);
    assert_eq!(observer.summary_snapshot()["instance"], instance);
    assert_eq!(
        observer.entry_metadata("readonly-270").unwrap()["instance"],
        instance
    );
    observer.set_enabled_state(true, ReadOnlyState::Active);
    assert_eq!(observer.instance(), instance);
    let reset = observer
        .list_page(269, 1, 8192, None, Some("previous process"))
        .unwrap();
    assert_eq!(reset["entries"]["reset"], true);
    assert_eq!(reset["entries"]["gap"], true);
    assert_eq!(reset["entries"]["cursor"], 0);
    assert_eq!(reset["entries"]["events"][0]["sequence"], 15);
    assert_eq!(
        observer
            .list_page(99999, 1, 8192, None, Some(&instance))
            .unwrap()["entries"]["reset"],
        true
    );
    assert_ne!(ReadOnlyObserver::default().instance(), instance);
}

#[test]
fn bounded_metadata_eviction_exposes_unfinished_loss_and_ids_do_not_cycle() {
    let observer = observer_push();
    for id in 1..=270 {
        observer.record_trigger(id, json!({}), None);
    }
    let state = observer.getstate();
    assert_eq!(state["memory"]["entries"], 256);
    assert_eq!(state["loss"]["metadataEvicted"], 14);
    assert_eq!(state["loss"]["unfinishedMetadataEvicted"], 14);
    assert_eq!(state["loss"]["recent"][0]["id"], "readonly-1");
    assert!(
        observer
            .entry_metadata("readonly-1")
            .unwrap_err()
            .contains("unfinished")
    );
    assert_eq!(
        observer.entry_metadata("readonly-270").unwrap()["id"],
        "readonly-270"
    );
    observer.record_trigger(271, json!({}), None);
    assert_eq!(
        observer.entry_metadata("readonly-271").unwrap()["id"],
        "readonly-271"
    );
    let exhausted = observer_push();
    exhausted.data.lock().unwrap().next_id = u64::MAX - 1;
    exhausted.record_trigger(1, json!({}), None);
    exhausted.record_trigger(2, json!({}), None);
    assert_eq!(exhausted.bus().latest_sequence(), 1);
    assert_eq!(exhausted.data.lock().unwrap().next_id, u64::MAX);
    assert!(
        exhausted
            .entry_metadata(&format!("readonly-{}", u64::MAX))
            .is_ok()
    );
}

#[test]
fn metadata_byte_budget_and_recent_loss_are_bounded() {
    let observer = observer_push();
    for id in 1..=70 {
        observer.record_trigger(id, json!({}), None);
    }
    {
        let mut data = observer.data.lock().unwrap();
        let large = json!({"status": "running", "result": "x".repeat(15000)});
        for entry in &mut data.entries {
            entry.status = large.clone();
            entry.history = (0..MAX_HISTORY)
                .map(|n| json!({"sequence": n, "status": large}))
                .collect();
        }
        refresh_entries(&mut data);
        assert!(data.entry_bytes <= MAX_ENTRY_BYTES);
        assert!(data.entries.len() < 70);
        assert!(data.lost_unfinished > 0);
        assert_eq!(data.losses.len(), MAX_HISTORY);
        assert_eq!(
            data.entry_bytes,
            data.entries.iter().map(|e| e.bytes).sum::<usize>()
        );
    }
}

#[test]
fn history_drop_counts_revision_and_summary_result_preview_are_explicit() {
    let observer = observer_push();
    observer.record_trigger(1, json!({}), None);
    for n in 0..40 {
        observer.record_formal_status(&formal(1, "running", Some(format!("step{n}"))));
    }
    let evidence = observer.read_entry("readonly-1").unwrap();
    assert_eq!(evidence["history"].as_array().unwrap().len(), 32);
    assert_eq!(evidence["historyDropped"], 9);
    assert_eq!(
        observer.bus().replay_after(0).last().unwrap().payload["historyDropped"],
        9
    );
    assert_eq!(
        section_all(&observer, "readonly-1", "history", 1024)["historyDropped"],
        9
    );
    let result = "汉🙂".repeat(1200);
    observer.record_formal_status(&formal(1, "completed", Some(result.clone())));
    let summary = observer.summary_snapshot();
    assert_eq!(summary["tasks"][0]["status"]["resultTruncated"], true);
    assert_eq!(summary["tasks"][0]["status"]["resultBytes"], result.len());
    assert!(
        summary["tasks"][0]["status"]["result"]
            .as_str()
            .unwrap()
            .len()
            <= 1024
    );
    assert_eq!(
        section_all(&observer, "readonly-1", "metadata", 1024)["status"]["result"],
        result
    );
    let before = observer.bus().latest_sequence();
    let mut same = formal(1, "completed", Some(result));
    same.elapsed_ms = 999999;
    observer.record_formal_status(&same);
    assert_eq!(observer.bus().latest_sequence(), before);
}

#[test]
fn privacy_legacy_compatibility_modes_and_oversized_status_loss() {
    let observer = observer_push();
    observer.record_trigger(
        1,
        json!({"token": "secret-value", "text": "copy secret-value", "ocr": "secret-value"}),
        None,
    );
    observer.record_formal_status(&formal(1, "completed", Some("done secret-value".into())));
    for section in ["metadata", "ocr", "parameters", "history"] {
        assert!(
            !section_all(&observer, "readonly-1", section, 1024)
                .to_string()
                .contains("secret-value")
        );
    }
    let old = observer.read_entry("readonly-1").unwrap();
    assert!(old.get("task").is_some() && old.get("history").is_some());
    assert!(!old.to_string().contains("secret-value"));
    observer.record_formal_status(&formal(
        1,
        "completed",
        Some("x".repeat(MAX_STATUS_BYTES * 3)),
    ));
    assert!(observer.entry_metadata("readonly-1").unwrap()["status"]["resultOmitted"].is_string());
    assert_eq!(observer.summary_snapshot()["loss"]["oversizedStatuses"], 1);
    let before = observer.bus().latest_sequence();
    observer.set_enabled_state(false, ReadOnlyState::Push);
    observer.record_formal_status(&formal(1, "failed", None));
    observer.record_trigger(2, json!({}), None);
    assert_eq!(observer.bus().latest_sequence(), before);
    observer.set_enabled_state(true, ReadOnlyState::Active);
    observer.record_formal_status(&formal(1, "failed", None));
    observer.record_trigger(2, json!({}), None);
    assert_eq!(observer.bus().latest_sequence(), before);
    assert_eq!(
        observer.entry_metadata("readonly-1").unwrap()["status"]["status"],
        "completed"
    );
}

#[test]
fn formal_source_terminal_loss_visible_without_publication() {
    let observer = observer_push();
    observer.record_status(&TaskEngineProjection {
        generation: 1,
        scheduler: FormalSchedulerSnapshot::default(),
        formal_statuses: Vec::new(),
        dropped_formal_statuses: 42,
        diagnostics: Arc::from([]),
    });
    assert_eq!(
        observer.summary_snapshot()["loss"]["droppedFormalStatuses"],
        42
    );
    assert_eq!(observer.bus().latest_sequence(), 0);
}

struct ObserverSink(Arc<ReadOnlyObserver>);
impl crate::runtime::task_engine::TaskEngineStateSink for ObserverSink {
    fn publish_task_engine(&self, projection: TaskEngineProjection) {
        self.0.record_status(&projection);
    }
}

struct NoUiWork;
impl crate::runtime::scheduler::FormalTaskWork for NoUiWork {
    fn execute(
        self: Box<Self>,
        _: crate::runtime::scheduler::FormalTaskCancellationToken,
    ) -> crate::runtime::scheduler::FormalTaskExecutionOutcome {
        panic!("test must not execute a business workflow");
    }
    fn cancel(self: Box<Self>) {}
}

fn enqueue(engine: &crate::runtime::task_engine::TaskEngineHandle) -> u64 {
    use crate::runtime::scheduler::{FormalTaskEnqueueOutcome, FormalTaskSubmission};
    match engine
        .enqueue_formal(FormalTaskSubmission::new(
            "@点歌晴天",
            None,
            false,
            Box::new(NoUiWork),
        ))
        .unwrap()
    {
        FormalTaskEnqueueOutcome::Queued(receipt) => receipt.task_id,
        FormalTaskEnqueueOutcome::Duplicate => panic!("unexpected duplicate"),
    }
}

#[test]
fn actual_task_engine_sink_delivers_old_terminal_outside_thirty_display_rows() {
    use crate::runtime::scheduler::FormalTaskCompletion;
    use crate::runtime::task_engine::TaskEngineHandle;
    let observer = Arc::new(observer_push());
    let engine = TaskEngineHandle::new(Some(Arc::new(ObserverSink(observer.clone()))));
    let first = enqueue(&engine);
    observer.record_trigger(first, json!({"parameters": {"keyword": "晴天"}}), None);
    let lease = engine.take_next_formal().unwrap().unwrap();
    assert_eq!(lease.task_id(), first);
    for _ in 0..30 {
        let id = enqueue(&engine);
        observer.record_trigger(id, json!({"kind": "command"}), None);
    }
    assert_eq!(engine.snapshot().unwrap().tasks().len(), 30);
    assert!(
        engine
            .snapshot()
            .unwrap()
            .tasks()
            .iter()
            .all(|s| s.id != first)
    );
    assert_eq!(
        observer.entry_metadata("readonly-1").unwrap()["status"]["status"],
        "running"
    );
    {
        let mut data = observer.data.lock().unwrap();
        // Pure lifecycle marker: actual PNG eviction is covered by the ignored real fixture.
        mark_image_evicted(&mut data, "readonly-1");
        refresh_entries(&mut data);
    }
    engine
        .complete_formal(
            first,
            FormalTaskCompletion::Failed("formal source terminal".into()),
        )
        .unwrap();
    assert!(
        engine
            .snapshot()
            .unwrap()
            .tasks()
            .iter()
            .all(|s| s.id != first)
    );
    assert_eq!(
        engine.formal_task_snapshot(first).unwrap().unwrap().status,
        "failed"
    );
    let evidence = observer.read_entry("readonly-1").unwrap();
    assert_eq!(evidence["status"]["status"], "failed");
    assert_eq!(evidence["status"]["result"], "formal source terminal");
    assert_eq!(evidence["missingReason"], "source_frame_evicted");
    assert_eq!(
        evidence["history"].as_array().unwrap().last().unwrap()["status"]["status"],
        "failed"
    );
    assert_eq!(observer.summary_snapshot()["taskCount"], 31);
    assert_eq!(
        observer.summary_snapshot()["tasks"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
}

#[test]
fn actual_task_engine_sink_reconciles_terminal_before_trigger_receipt() {
    use crate::runtime::scheduler::FormalTaskCompletion;
    use crate::runtime::task_engine::TaskEngineHandle;
    let observer = Arc::new(observer_push());
    let engine = TaskEngineHandle::new(Some(Arc::new(ObserverSink(observer.clone()))));
    let id = enqueue(&engine);
    let _lease = engine.take_next_formal().unwrap().unwrap();
    engine
        .complete_formal(id, FormalTaskCompletion::Failed("before receipt".into()))
        .unwrap();
    assert_eq!(
        observer.bus().latest_sequence(),
        0,
        "untracked formal source remains silent"
    );
    observer.record_trigger(id, json!({"kind": "command"}), None);
    let events = observer.bus().replay_after(0);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].kind, "readonly.triggered");
    assert_eq!(events[1].kind, "readonly.task.updated");
    assert_eq!(events[1].payload["status"]["result"], "before receipt");
    assert_eq!(
        observer.read_entry("readonly-1").unwrap()["status"]["status"],
        "failed"
    );
    observer.record_trigger(id, json!({}), None);
    assert_eq!(observer.bus().latest_sequence(), 2);
}

#[test]
fn newest_pre_receipt_terminal_is_cached_with_more_than_one_thousand_source_ids() {
    let observer = observer_push();
    // Deliberately ascending source order: reconciliation must choose newest IDs.
    let statuses = (1..=1001)
        .map(|id| formal(id, "completed", Some(format!("done-{id}"))))
        .collect();
    observer.record_status(&TaskEngineProjection {
        generation: 1,
        scheduler: FormalSchedulerSnapshot::default(),
        formal_statuses: statuses,
        dropped_formal_statuses: 0,
        diagnostics: Arc::from([]),
    });
    observer.record_trigger(1001, json!({}), None);
    assert_eq!(
        observer.read_entry("readonly-1").unwrap()["status"]["result"],
        "done-1001"
    );
    assert_eq!(
        observer.data.lock().unwrap().statuses.len(),
        MAX_STATUS_RECORDS
    );
    assert_eq!(
        observer.summary_snapshot()["loss"]["preReceiptStatusesOmitted"],
        1
    );
}

#[test]
fn versioned_lookup_cannot_regress_tasks_or_suppress_other_task_projections_and_restore_is_allowed()
{
    let observer = observer_push();
    observer.record_trigger(1, json!({}), None);
    observer.record_trigger(2, json!({}), None);
    observer.record_formal_status_at(20, &formal(1, "running", Some("lookup20".into())));
    observer.record_status(&TaskEngineProjection {
        generation: 19,
        scheduler: FormalSchedulerSnapshot::default(),
        formal_statuses: vec![
            formal(1, "queued", None),
            formal(2, "running", Some("projection19".into())),
        ],
        dropped_formal_statuses: 0,
        diagnostics: Arc::from([]),
    });
    assert_eq!(
        observer.read_entry("readonly-1").unwrap()["status"]["result"],
        "lookup20"
    );
    assert_eq!(
        observer.read_entry("readonly-2").unwrap()["status"]["result"],
        "projection19"
    );
    let before = observer.bus().latest_sequence();
    observer.record_formal_status_at(18, &formal(2, "failed", Some("stale global lookup".into())));
    observer.record_formal_status_at(
        19,
        &formal(1, "failed", Some("stale per-entry lookup".into())),
    );
    assert_eq!(observer.bus().latest_sequence(), before);
    let mut restored = formal(1, "queued", None);
    restored.started_at_ms = None;
    observer.record_status(&TaskEngineProjection {
        generation: 20,
        scheduler: FormalSchedulerSnapshot::default(),
        formal_statuses: vec![restored, formal(2, "running", Some("projection19".into()))],
        dropped_formal_statuses: 0,
        diagnostics: Arc::from([]),
    });
    assert_eq!(
        observer.read_entry("readonly-1").unwrap()["status"]["status"],
        "queued"
    );
    assert_eq!(observer.bus().latest_sequence(), before + 1);
}

#[test]
fn immediate_failure_status_is_retained_and_published() {
    let observer = observer_push();
    observer.record_immediate_with_status(
        json!({"kind": "command", "flow": "immediatePlayback"}),
        None,
        "failed",
        "handler_error; effects_may_be_partial: pause".into(),
    );
    let evidence = observer.read_entry("readonly-1").unwrap();
    assert_eq!(evidence["status"]["status"], "failed");
    assert_eq!(
        evidence["status"]["result"],
        "handler_error; effects_may_be_partial: pause"
    );
    assert_eq!(
        observer.bus().replay_after(0)[0].payload["status"]["status"],
        "failed"
    );
}

#[test]
fn queued_metadata_enrichment_is_silent_but_state_restore_and_terminal_are_published() {
    let observer = observer_push();
    observer.record_trigger(1, json!({}), None);
    let before_revision = observer.entry_metadata("readonly-1").unwrap()["revision"]
        .as_u64()
        .unwrap();
    let mut queued = formal(1, "queued", None);
    queued.started_at_ms = None;
    observer.record_formal_status_at(1, &queued);
    assert_eq!(observer.bus().latest_sequence(), 1);
    let evidence = observer.read_entry("readonly-1").unwrap();
    assert_eq!(evidence["status"]["queuedAtMs"], 1);
    assert_eq!(evidence["history"].as_array().unwrap().len(), 1);
    assert!(evidence["revision"].as_u64().unwrap() > before_revision);
    observer.record_formal_status_at(2, &formal(1, "running", None));
    observer.record_formal_status_at(3, &queued);
    observer.record_formal_status_at(4, &formal(1, "completed", Some("done".into())));
    assert_eq!(observer.bus().latest_sequence(), 4);
    assert_eq!(
        observer.bus().replay_after(0)[2].payload["status"]["status"],
        "queued"
    );

    let observer = observer_push();
    observer.record_status(&TaskEngineProjection {
        generation: 1,
        scheduler: FormalSchedulerSnapshot::default(),
        formal_statuses: vec![queued],
        dropped_formal_statuses: 0,
        diagnostics: Arc::from([]),
    });
    observer.record_trigger(1, json!({}), None);
    assert_eq!(observer.bus().latest_sequence(), 1);
    assert_eq!(
        observer.bus().replay_after(0)[0].payload["status"]["queuedAtMs"],
        1
    );
}
