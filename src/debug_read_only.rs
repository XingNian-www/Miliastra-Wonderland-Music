//! Side-channel evidence for active reads and successfully enqueued formal workflows.
//! No input, captures, OCR, PNG encoding or pixel hashing runs on a producer hook.
use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::debug_events::DebugEventBus;
use crate::features::command::{ModuleCommand, RoutedCommand};
use crate::runtime::scheduler::FormalTaskSnapshot;
use crate::runtime::task_engine::TaskEngineProjection;
use crate::runtime::ui::CapturedFrame;

const MAX_FRAMES: usize = 8;
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 8;
const MAX_ENTRY_BYTES: usize = 32 * 1024 * 1024;
const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_STATUS_RECORDS: usize = 60;
const MAX_HISTORY: usize = 32;
const STATUS_HISTORY_RESERVE: usize = (MAX_HISTORY + 1) * 32 * 1024;
const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ReadOnlyState {
    #[default]
    Active,
    Push,
}

#[derive(Clone)]
struct SourceFrame {
    frame: Arc<CapturedFrame>,
    captured_at_wall_millis: u64,
    bytes: usize,
}

impl SourceFrame {
    fn new(frame: Arc<CapturedFrame>) -> Self {
        let bytes = frame.image().as_bytes().len();
        // Instant cannot be serialized; retain it for exact matching and estimate wall time.
        let captured_at_wall_millis = now_ms().saturating_sub(
            frame
                .captured_at()
                .elapsed()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
        );
        Self {
            frame,
            captured_at_wall_millis,
            bytes,
        }
    }
}

#[derive(Clone)]
struct Entry {
    id: String,
    kind: &'static str,
    task_id: Option<u64>,
    recorded_at_ms: u64,
    task: Value,
    status: Value,
    redactions: Vec<String>,
    history: VecDeque<Value>,
    source: Option<SourceFrame>,
    missing_reason: Option<&'static str>,
    bytes: usize,
}

impl Entry {
    fn summary(&self) -> Value {
        json!({"id": self.id, "kind": self.kind, "taskId": self.task_id,
            "schemaVersion": SCHEMA_VERSION, "appVersion": env!("CARGO_PKG_VERSION"),
            "recordedAtMs": self.recorded_at_ms, "status": self.status,
            "sourceFrameId": self.source.as_ref().map(|s| s.frame.id()),
            "missingReason": self.missing_reason,
            "evidenceUrl": format!("/debug/read-only/evidence?id={}", self.id)})
    }
}

#[derive(Default)]
struct ObserverData {
    enabled: bool,
    mode: ReadOnlyState,
    frames: VecDeque<SourceFrame>,
    frame_bytes: usize,
    entries: VecDeque<Entry>,
    entry_bytes: usize,
    next_id: u64,
    dropped_frames: u64,
    dropped_entries: u64,
    rejected_frames: u64,
    generation: Option<u64>,
    // Enqueue publishes its projection before the application receives the receipt.
    // Keep bounded latest snapshots so record_trigger can reconcile that race.
    statuses: HashMap<u64, Value>,
}

pub(crate) struct ReadOnlyObserver {
    data: Mutex<ObserverData>,
    events: DebugEventBus,
}

impl Default for ReadOnlyObserver {
    fn default() -> Self {
        Self {
            data: Mutex::new(ObserverData::default()),
            events: DebugEventBus::new(),
        }
    }
}

pub(crate) fn observer() -> &'static ReadOnlyObserver {
    static OBSERVER: OnceLock<ReadOnlyObserver> = OnceLock::new();
    OBSERVER.get_or_init(ReadOnlyObserver::default)
}

impl ReadOnlyObserver {
    #[cfg(test)]
    pub(crate) fn poison_for_test(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = self.data.lock().unwrap();
            panic!("test observer failure");
        }));
    }

    pub(crate) fn enabled(&self) -> bool {
        self.data.lock().map(|d| d.enabled).unwrap_or(false)
    }

    pub(crate) fn state(&self) -> ReadOnlyState {
        self.data.lock().map(|d| d.mode).unwrap_or_default()
    }

    pub(crate) fn push_enabled(&self) -> bool {
        self.data
            .lock()
            .map(|d| d.enabled && d.mode == ReadOnlyState::Push)
            .unwrap_or(false)
    }

    pub(crate) fn state_snapshot(&self) -> Value {
        match self.data.lock() {
            Ok(data) => self.describe(&data),
            Err(_) => json!({"enabled": false, "state": "active", "error": "observer unavailable"}),
        }
    }

    pub(crate) fn getstate(&self) -> Value {
        self.state_snapshot()
    }

    pub(crate) fn set_state(&self, state: ReadOnlyState) -> Value {
        self.set_enabled_state(self.enabled(), state)
    }

    pub(crate) fn set_enabled_state(&self, enabled: bool, state: ReadOnlyState) -> Value {
        let Ok(mut data) = self.data.lock() else {
            return json!({"enabled": false, "state": "active", "error": "observer unavailable"});
        };
        if data.enabled != enabled || data.mode != state {
            data.frames.clear();
            data.frame_bytes = 0;
            data.statuses.clear();
            data.generation = None;
        }
        data.enabled = enabled;
        data.mode = state;
        // Switching mode never publishes a system event to the workflow stream.
        self.describe(&data)
    }

    fn describe(&self, data: &ObserverData) -> Value {
        json!({"enabled": data.enabled, "state": data.mode,
            "schemaVersion": SCHEMA_VERSION, "appVersion": env!("CARGO_PKG_VERSION"),
            "latestSequence": self.events.latest_sequence(),
            "memory": {"frames": data.frames.len(), "frameBytes": data.frame_bytes,
                "entries": data.entries.len(), "entryBytes": data.entry_bytes},
            "limits": {"frames": MAX_FRAMES, "frameBytes": MAX_FRAME_BYTES,
                "entries": MAX_ENTRIES, "entryBytes": MAX_ENTRY_BYTES},
            "droppedFrames": data.dropped_frames, "droppedEntries": data.dropped_entries,
            "rejectedFrames": data.rejected_frames})
    }

    pub(crate) fn bus(&self) -> &DebugEventBus {
        &self.events
    }

    pub(crate) fn list_after(&self, since: u64) -> Value {
        let events = self.events.replay_after(since);
        let first = events.first().map(|e| e.sequence);
        json!({"events": events.iter().map(|event| json!({"sequence": event.sequence,
            "kind": event.kind, "atMs": event.at_ms, "payload": event.payload})).collect::<Vec<_>>(),
            "latestSequence": self.events.latest_sequence(),
            "gap": first.is_some_and(|sequence| sequence > since.saturating_add(1)),
            "retention": self.state_snapshot()})
    }

    pub(crate) fn cache_frame(&self, frame: Arc<CapturedFrame>) {
        let Ok(mut data) = self.data.lock() else {
            return;
        };
        if !data.enabled || data.mode != ReadOnlyState::Push {
            return;
        }
        if data
            .frames
            .iter()
            .any(|s| s.frame.id() == frame.id() && s.frame.captured_at() == frame.captured_at())
        {
            return;
        }
        let source = SourceFrame::new(frame);
        if source.bytes > MAX_FRAME_BYTES {
            data.rejected_frames += 1;
            return;
        }
        while data.frames.len() >= MAX_FRAMES
            || data.frame_bytes.saturating_add(source.bytes) > MAX_FRAME_BYTES
        {
            if let Some(old) = data.frames.pop_front() {
                data.frame_bytes = data.frame_bytes.saturating_sub(old.bytes);
                data.dropped_frames += 1;
            } else {
                break;
            }
        }
        data.frame_bytes += source.bytes;
        data.frames.push_back(source);
    }

    pub(crate) fn record_active(
        &self,
        frame: Arc<CapturedFrame>,
        payload: Value,
    ) -> Result<String, String> {
        let redactions = sensitive_strings(&payload);
        let task = redact_strings(sanitize_payload(payload), &redactions);
        let payload_bytes = serde_json::to_vec(&task).map_err(|e| e.to_string())?.len();
        if payload_bytes > MAX_PAYLOAD_BYTES {
            return Err("active payload exceeds 64 KiB".into());
        }
        let source = SourceFrame::new(frame);
        let bytes = source
            .bytes
            .saturating_add(payload_bytes)
            .saturating_add(redactions.iter().map(String::len).sum::<usize>())
            .saturating_add(4096);
        if bytes > MAX_ENTRY_BYTES {
            return Err("active evidence exceeds 32 MiB".into());
        }
        let mut data = self.data.lock().map_err(|_| "observer unavailable")?;
        if !data.enabled || data.mode != ReadOnlyState::Active {
            return Err("active read requires enabled active state".into());
        }
        let id = next_id(&mut data);
        let entry = Entry {
            id: id.clone(),
            kind: "active",
            task_id: None,
            recorded_at_ms: now_ms(),
            task,
            status: Value::Null,
            redactions,
            history: VecDeque::new(),
            source: Some(source),
            missing_reason: None,
            bytes,
        };
        insert_entry(&mut data, entry);
        Ok(id)
    }

    pub(crate) fn record_trigger(
        &self,
        task_id: u64,
        payload: Value,
        captured_at: Option<Instant>,
    ) {
        self.record_flow(Some(task_id), payload, captured_at, None);
    }

    pub(crate) fn record_immediate(
        &self,
        payload: Value,
        captured_at: Option<Instant>,
        result: String,
    ) {
        let completion = json!({"status": "completed", "result": result});
        self.record_flow(None, payload, captured_at, Some(completion));
    }

    fn record_flow(
        &self,
        task_id: Option<u64>,
        payload: Value,
        captured_at: Option<Instant>,
        completion: Option<Value>,
    ) {
        let Ok(mut data) = self.data.lock() else {
            return;
        };
        if !data.enabled || data.mode != ReadOnlyState::Push {
            return;
        }
        if task_id.is_some() && data.entries.iter().any(|e| e.task_id == task_id) {
            return;
        }
        let redactions = sensitive_strings(&payload);
        let mut task = redact_strings(sanitize_payload(payload), &redactions);
        // Source policy survives payload-budget truncation; oversized metadata must
        // never turn an explicitly private or nonvisual source into a raw screenshot.
        let private_source = task.get("sourceFrameOmitted").is_some()
            || task.pointer("/sourceCommand/sourceFrameOmitted").is_some();
        let non_visual_source = task.get("sourceKind").and_then(Value::as_str)
            == Some("nonVisualInput")
            || task
                .pointer("/sourceCommand/sourceKind")
                .and_then(Value::as_str)
                == Some("nonVisualInput");
        let mut payload_bytes = serde_json::to_vec(&task)
            .map(|v| v.len())
            .unwrap_or(MAX_PAYLOAD_BYTES + 1);
        if payload_bytes > MAX_PAYLOAD_BYTES {
            task = json!({"kind": task.get("kind"), "parametersOmitted": "payload exceeds 64 KiB"});
            payload_bytes = 1024;
        }
        // Only equality with the OCR source Instant is valid. Never substitute latest_frame.
        let mut source = captured_at.filter(|_| !private_source).and_then(|at| {
            data.frames
                .iter()
                .find(|s| s.frame.captured_at() == at)
                .cloned()
                // A long-running conversion may outlive the rolling capture cache.
                // Reuse only a retained evidence frame with the exact same Instant.
                .or_else(|| {
                    data.entries.iter().find_map(|entry| {
                        entry
                            .source
                            .as_ref()
                            .filter(|s| s.frame.captured_at() == at)
                            .cloned()
                    })
                })
        });
        let mut missing_reason = if private_source {
            Some("private_source_frame_omitted")
        } else if source.is_some() {
            None
        } else if non_visual_source {
            Some("non_visual_input_has_no_source_frame")
        } else if captured_at.is_none() {
            Some("flow_has_no_source_capture")
        } else {
            Some("exact_source_frame_not_retained")
        };
        let overhead = payload_bytes
            .saturating_add(redactions.iter().map(String::len).sum::<usize>())
            .saturating_add(STATUS_HISTORY_RESERVE);
        if source
            .as_ref()
            .is_some_and(|s| s.bytes.saturating_add(overhead) > MAX_ENTRY_BYTES)
        {
            source = None;
            missing_reason = Some("source_frame_exceeds_evidence_budget");
        }
        let bytes = source
            .as_ref()
            .map_or(0, |s| s.bytes)
            .saturating_add(overhead);
        let id = next_id(&mut data);
        let immediate = completion.is_some();
        let initial = completion.unwrap_or_else(|| json!({"id": task_id, "status": "queued"}));
        let latest_status = task_id.and_then(|id| data.statuses.get(&id).cloned());
        let mut entry = Entry {
            id,
            kind: if immediate { "immediate" } else { "trigger" },
            task_id,
            recorded_at_ms: now_ms(),
            task,
            status: redact_strings(initial, &redactions),
            redactions,
            history: VecDeque::new(),
            source,
            missing_reason,
            bytes,
        };
        self.publish_entry(&mut entry, "readonly.triggered");
        if let Some(status) = latest_status {
            entry.status = redact_strings(status, &entry.redactions);
            self.publish_entry(&mut entry, "readonly.task.updated");
        }
        insert_entry(&mut data, entry);
    }

    pub(crate) fn record_status(&self, projection: &TaskEngineProjection) {
        let Ok(mut data) = self.data.lock() else {
            return;
        };
        if !data.enabled || data.mode != ReadOnlyState::Push {
            return;
        }
        if data
            .generation
            .is_some_and(|generation| projection.generation <= generation)
        {
            return;
        }
        data.generation = Some(projection.generation);
        data.statuses = projection
            .scheduler
            .tasks()
            .iter()
            .take(MAX_STATUS_RECORDS)
            .map(|snapshot| (snapshot.id, workflow_status(snapshot)))
            .collect();
        // Do not emit whole projections: deferred chat, untracked tasks and elapsed ticks are silent.
        let updates: Vec<_> = data
            .entries
            .iter()
            .filter_map(|entry| {
                let task_id = entry.task_id?;
                let status =
                    redact_strings(data.statuses.get(&task_id)?.clone(), &entry.redactions);
                (entry.status != status).then(|| (entry.id.clone(), status))
            })
            .collect();
        for (id, status) in updates {
            if let Some(entry) = data.entries.iter_mut().find(|entry| entry.id == id) {
                entry.status = status;
                self.publish_entry(entry, "readonly.task.updated");
            }
        }
    }

    fn publish_entry(&self, entry: &mut Entry, kind: &str) {
        let event = self.events.publish(kind, entry.summary());
        entry
            .history
            .push_back(json!({"sequence": event.sequence, "kind": event.kind,
            "atMs": event.at_ms, "status": entry.status}));
        while entry.history.len() > MAX_HISTORY {
            entry.history.pop_front();
        }
    }

    pub(crate) fn read_entry(&self, id: &str) -> Result<Value, String> {
        let entry = {
            let data = self.data.lock().map_err(|_| "observer unavailable")?;
            match data.entries.iter().find(|entry| entry.id == id) {
                Some(entry) => entry.clone(),
                None => {
                    let sequence = id
                        .strip_prefix("readonly-")
                        .and_then(|s| s.parse::<u64>().ok());
                    return Err(if sequence.is_some_and(|s| s > 0 && s <= data.next_id) {
                        "evidence_expired: bounded evidence retention evicted this entry"
                    } else {
                        "evidence_not_found"
                    }
                    .into());
                }
            }
        };
        // Expensive work is intentionally outside all producer and observer locks.
        let source = match entry.source.as_ref() {
            Some(source) => {
                let image = source.frame.image();
                let mut png = Cursor::new(Vec::new());
                image
                    .write_to(&mut png, image::ImageFormat::Png)
                    .map_err(|e| e.to_string())?;
                json!({"frameId": source.frame.id(), "capturedAtWallMillis": source.captured_at_wall_millis,
                    "capturedAtWallMillisEstimated": true,
                    "ageMillis": source.frame.captured_at().elapsed().as_millis(),
                    "canvas": "rawGame", "width": image.width(), "height": image.height(),
                    "pixelFormat": format!("{:?}", image.color()),
                    "pixelSha256": format!("{:x}", Sha256::digest(image.as_bytes())),
                    "uiState": source.frame.ui_state().map(|state| state.diagnostic()),
                    "imagePngBase64": crate::debug_http_service::base64_encode(png.get_ref())})
            }
            None => Value::Null,
        };
        let mut result = entry.summary();
        result["task"] = entry.task;
        result["history"] = json!(entry.history);
        result["source"] = source;
        result["replay"] = json!({"schemaVersion": SCHEMA_VERSION,
            "kind": "evidence_only", "executesBusinessActions": false,
            "parametersAreRedacted": true, "canvas": "rawGame"});
        Ok(result)
    }
}

fn workflow_status(snapshot: &FormalTaskSnapshot) -> Value {
    // label contains raw chat text (including invitation passwords); never publish it.
    json!({"id": snapshot.id, "status": snapshot.status,
        "queuedAtMs": snapshot.queued_at_ms, "startedAtMs": snapshot.started_at_ms,
        "finishedAtMs": snapshot.finished_at_ms,
        "cancellationRequested": snapshot.cancellation_requested,
        "result": snapshot.result})
}

fn next_id(data: &mut ObserverData) -> String {
    data.next_id = data.next_id.saturating_add(1);
    format!("readonly-{}", data.next_id)
}

fn insert_entry(data: &mut ObserverData, entry: Entry) {
    while data.entries.len() >= MAX_ENTRIES
        || data.entry_bytes.saturating_add(entry.bytes) > MAX_ENTRY_BYTES
    {
        if let Some(old) = data.entries.pop_front() {
            data.entry_bytes = data.entry_bytes.saturating_sub(old.bytes);
            data.dropped_entries += 1;
        } else {
            break;
        }
    }
    data.entry_bytes += entry.bytes;
    data.entries.push_back(entry);
}

/// The source is captured from the routed input, before conversion into another task.
/// Do not serialize application/AI/configuration contexts into this operational allowlist.
pub(crate) fn command_parameters(parsed: &RoutedCommand) -> Value {
    let captured_at = command_capture(parsed);
    let mut payload = json!({"kind": "command", "parametersVersion": 1,
        "raw": parsed.raw, "matched": parsed.matched, "userCommand": parsed.user_command,
        "originalText": parsed.observation.message_text,
        "originalTextMissingReason": parsed.observation.message_text.is_none()
            .then_some("routed_input_has_no_original_message_text"),
        "nickname": parsed.username, "messageType": parsed.message_type,
        "authority": format!("{:?}", parsed.authority),
        "messageY": parsed.observation.message_y,
        "observationFrameId": parsed.observation.frame_id.map(|id| id.get()),
        "messageId": parsed.observation.message_id.as_ref().map(|id| json!({
            "visualSession": id.visual_session.get(),
            "bubbleSequence": id.bubble_sequence.get(), "chat": format!("{:?}", id.chat)})),
        "observationHasCapture": captured_at.is_some(),
        "sourceKind": if matches!(parsed.message_type.as_str(), "blue" | "pink") {
            "visualChat"
        } else { "nonVisualInput" },
        "capturedAtMs": captured_at.map(|at| now_ms().saturating_sub(
            at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)),
        "captureTimeEncoding": "wall_time_estimate_exact_instant_retained_in_process"});
    payload["commandFamily"] = json!(match &parsed.command {
        ModuleCommand::SongRequest(_) => "songRequest",
        ModuleCommand::Playback(_) => "playback",
        ModuleCommand::Hall(_) => "hall",
        ModuleCommand::Administration(_) => "administration",
        ModuleCommand::IdiomChain(_) => "idiomChain",
        ModuleCommand::CardGame(_) => "cardGame",
        ModuleCommand::TurtleSoup(_) => "turtleSoup",
        ModuleCommand::Tarot(_) => "tarot",
        ModuleCommand::Playlist(_) => "playlist",
        ModuleCommand::Undercover(_) => "undercover",
        ModuleCommand::Invite(_) => "invite",
        ModuleCommand::Moderation(_) => "moderation",
        ModuleCommand::CustomWorkflow(_) => "customWorkflow",
    });
    let private_reason =
        match &parsed.command {
            ModuleCommand::Moderation(_) if parsed.message_type == "pink" => {
                Some("private moderation input")
            }
            ModuleCommand::Tarot(crate::features::tarot::TarotCommand::Draw {
                ai: true, ..
            }) if parsed.message_type == "pink" => Some("private AI input"),
            ModuleCommand::Undercover(
                crate::features::undercover::UndercoverCommand::Vote(_)
                | crate::features::undercover::UndercoverCommand::Abstain
                | crate::features::undercover::UndercoverCommand::Reveal,
            ) => Some("private game input"),
            _ => None,
        };
    if let Some(reason) = private_reason {
        payload["parametersOmitted"] = json!(reason);
        payload["sourceTextOmitted"] = json!(reason);
        payload["sourceFrameOmitted"] = json!(reason);
        // Keep omission explicit. Do not replace original input with invented text.
        payload["raw"] = Value::Null;
        payload["userCommand"] = Value::Null;
        payload["originalText"] = Value::Null;
        payload["privateRawSecret"] = json!(parsed.raw);
        payload["privateCommandSecret"] = json!(parsed.user_command);
        payload["privateTextSecret"] = json!(parsed.observation.message_text);
        // Values are collected before sanitization, then removed from task labels,
        // results and history. They are never present in exported evidence.
        if let ModuleCommand::Moderation(command) = &parsed.command {
            payload["privateContextSecret"] = json!(command.uid);
        }
        if let ModuleCommand::Tarot(crate::features::tarot::TarotCommand::Draw {
            question, ..
        }) = &parsed.command
        {
            payload["privateQuestionSecret"] = json!(question);
        }
    } else if matches!(parsed.command, ModuleCommand::Moderation(_)) {
        // A public trigger stays reproducible. Internal moderation state is never
        // serialized, independently of whether the originating message is public.
        payload["parametersOmitted"] = json!("private moderation context");
    } else {
        payload["parameters"] = public_game_actor_fields(json!(parsed.command));
    }
    payload
}

// These are game actor fields in typed module commands, not account/configuration
// usernames. Export them under explicit public aliases without relaxing the generic
// credential sanitizer (arbitrary username fields are still redacted).
fn public_game_actor_fields(value: Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| {
                    let key = match key.as_str() {
                        "username" => "publicUsername".to_owned(),
                        "display_name" => "publicDisplayName".to_owned(),
                        "requester" => "publicRequester".to_owned(),
                        "player" => "publicPlayer".to_owned(),
                        "friend_username" => "publicFriendUsername".to_owned(),
                        _ => key,
                    };
                    (key, public_game_actor_fields(value))
                })
                .collect(),
        ),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(public_game_actor_fields).collect())
        }
        other => other,
    }
}

/// Console/HTTP inputs can carry a business timestamp, but it is not a game capture.
pub(crate) fn command_capture(parsed: &RoutedCommand) -> Option<Instant> {
    matches!(parsed.message_type.as_str(), "blue" | "pink")
        .then_some(parsed.observation.captured_at)
        .flatten()
}

pub(crate) fn record_command_immediate(parsed: &RoutedCommand, flow: &str, result: String) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if !observer().push_enabled() {
            return;
        }
        let mut payload = command_parameters(parsed);
        payload["flow"] = json!(flow);
        observer().record_immediate(payload, command_capture(parsed), result);
    }));
}

/// Only operational parameters belong in this API; identity and credential fields are omitted.
pub(crate) fn sanitize_payload(value: Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| {
                    let sensitive = sensitive_key(&key);
                    (
                        key,
                        if sensitive {
                            json!("[redacted]")
                        } else {
                            sanitize_payload(value)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(sanitize_payload).collect()),
        Value::String(text) => json!(crate::privacy::redacted_chat_text(&text)),
        other => other,
    }
}

fn sensitive_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['_', '-'], "");
    [
        "authorization",
        "token",
        "apikey",
        "configkey",
        "secret",
        "password",
        "cookie",
    ]
    .iter()
    .any(|part| normalized.contains(part))
        || matches!(
            normalized.as_str(),
            "username"
                | "displayname"
                | "requester"
                | "player"
                | "friendusername"
                | "aicontext"
                | "moderationcontext"
                | "systemprompt"
                | "prompt"
                | "airesponse"
        )
}

fn sensitive_strings(value: &Value) -> Vec<String> {
    fn collect_sensitive_values(value: &Value, output: &mut Vec<String>) {
        match value {
            Value::String(text)
                if !text.is_empty()
                    && text.len() <= 2048
                    && output.len() < 64
                    && !output.contains(text) =>
            {
                output.push(text.clone())
            }
            Value::Object(fields) => {
                for value in fields.values() {
                    collect_sensitive_values(value, output);
                }
            }
            Value::Array(items) => {
                for value in items {
                    collect_sensitive_values(value, output);
                }
            }
            _ => {}
        }
    }
    fn collect(value: &Value, output: &mut Vec<String>) {
        match value {
            Value::Object(fields) => {
                for (key, value) in fields {
                    if sensitive_key(key) {
                        if let Some(text) =
                            value.as_str().filter(|s| !s.is_empty() && s.len() <= 2048)
                            && output.len() < 64
                            && !output.iter().any(|v| v == text)
                        {
                            output.push(text.to_owned());
                        } else if !value.is_string() {
                            collect_sensitive_values(value, output);
                        }
                    } else {
                        collect(value, output);
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    collect(item, output);
                }
            }
            _ => {}
        }
    }
    let mut output = Vec::new();
    collect(value, &mut output);
    // Longer identities must be replaced before their substrings.
    output.sort_by_key(|text| std::cmp::Reverse(text.len()));
    output
}

fn redact_strings(value: Value, redactions: &[String]) -> Value {
    match value {
        Value::String(mut text) => {
            for secret in redactions {
                text = text.replace(secret, "[redacted]");
            }
            json!(crate::privacy::redacted_chat_text(&text))
        }
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key, redact_strings(value, redactions)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|v| redact_strings(v, redactions))
                .collect(),
        ),
        other => other,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::scheduler::{
        FormalSchedulerSnapshot, FormalTaskCancellationToken, FormalTaskCompletion,
        FormalTaskEnqueueOutcome, FormalTaskExecutionOutcome, FormalTaskSubmission, FormalTaskWork,
    };
    use crate::runtime::task_engine::TaskEngineHandle;

    #[test]
    fn routed_evidence_keeps_public_actor_but_hides_private_contexts() {
        use crate::features::command::ModuleCommand;
        use crate::features::moderation::{ModerationAction, ModerationCommand};
        let mut source = RoutedCommand::console(
            "拉黑",
            "拉黑UID123456789",
            ModuleCommand::Moderation(ModerationCommand {
                action: ModerationAction::Blacklist,
                uid: "123456789".into(),
                requester: "备注昵称".into(),
            }),
        );
        source.username = "香菜".into();
        source.message_type = "pink".into();
        source.observation.message_text = Some("香菜：@拉黑UID123456789".into());
        let observer = ReadOnlyObserver::default();
        observer.set_enabled_state(true, ReadOnlyState::Push);
        let payload = command_parameters(&source);
        assert_eq!(payload["nickname"], "香菜");
        assert_eq!(payload["commandFamily"], "moderation");
        assert!(payload["raw"].is_null());
        observer.record_immediate(
            payload,
            command_capture(&source),
            "拉黑UID123456789完成".into(),
        );
        let evidence = observer.read_entry("readonly-1").unwrap();
        assert_eq!(evidence["task"]["nickname"], "香菜");
        assert_eq!(
            evidence["task"]["sourceTextOmitted"],
            "private moderation input"
        );
        assert_eq!(evidence["missingReason"], "private_source_frame_omitted");
        assert!(evidence["source"].is_null());
        assert!(!evidence.to_string().contains("123456789"));
        assert!(!evidence.to_string().contains("备注昵称"));
    }

    #[test]
    fn private_ai_contexts_and_tokens_never_leak_through_status() {
        let observer = ReadOnlyObserver::default();
        observer.set_enabled_state(true, ReadOnlyState::Push);
        let payload = json!({"kind": "command", "nickname": "香菜", "parameters": {
            "aiContext": {"prompt": "private-prompt", "response": "private-answer"},
            "moderationContext": {"uid": "private-uid"}, "config_token": "config-secret"}});
        observer.record_immediate(
            payload,
            None,
            "private-prompt private-answer private-uid config-secret".into(),
        );
        let evidence = observer.read_entry("readonly-1").unwrap().to_string();
        assert!(evidence.contains("香菜"));
        for secret in [
            "private-prompt",
            "private-answer",
            "private-uid",
            "config-secret",
        ] {
            assert!(!evidence.contains(secret));
        }
    }

    #[test]
    fn private_source_policy_survives_payload_budget_truncation() {
        let observer = ReadOnlyObserver::default();
        observer.set_enabled_state(true, ReadOnlyState::Push);
        observer.record_trigger(
            10,
            json!({"kind": "converted", "large": "x".repeat(MAX_PAYLOAD_BYTES + 1),
            "sourceCommand": {"sourceFrameOmitted": "private input"}}),
            Some(Instant::now()),
        );
        let evidence = observer.read_entry("readonly-1").unwrap();
        assert_eq!(
            evidence["task"]["parametersOmitted"],
            "payload exceeds 64 KiB"
        );
        assert_eq!(evidence["missingReason"], "private_source_frame_omitted");
        assert!(evidence["source"].is_null());
    }

    #[test]
    fn console_business_timestamp_is_not_a_visual_capture() {
        let source = RoutedCommand::console(
            "next",
            "下一首",
            ModuleCommand::Playback(crate::features::playback::PlaybackCommand::Next),
        );
        assert!(source.observation.captured_at.is_some());
        assert!(command_capture(&source).is_none());
        let observer = ReadOnlyObserver::default();
        observer.set_enabled_state(true, ReadOnlyState::Push);
        observer.record_trigger(9, command_parameters(&source), command_capture(&source));
        let evidence = observer.read_entry("readonly-1").unwrap();
        assert_eq!(
            evidence["missingReason"],
            "non_visual_input_has_no_source_frame"
        );
        assert!(evidence["source"].is_null());
    }

    struct NoUiWork;
    impl FormalTaskWork for NoUiWork {
        fn execute(self: Box<Self>, _: FormalTaskCancellationToken) -> FormalTaskExecutionOutcome {
            FormalTaskExecutionOutcome::Completed(Ok("done".into()))
        }
        fn cancel(self: Box<Self>) {}
    }

    fn projection(engine: &TaskEngineHandle, generation: u64) -> TaskEngineProjection {
        TaskEngineProjection {
            generation,
            scheduler: engine.snapshot().unwrap(),
            diagnostics: Arc::from([]),
        }
    }

    fn enqueue(engine: &TaskEngineHandle) -> u64 {
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
    fn disabled_active_and_ordinary_projections_do_not_publish() {
        let observer = ReadOnlyObserver::default();
        assert!(!observer.enabled());
        assert_eq!(observer.state(), ReadOnlyState::Active);
        observer.record_trigger(1, json!({"kind": "command"}), None);
        let empty = TaskEngineProjection {
            generation: 1,
            scheduler: FormalSchedulerSnapshot::default(),
            diagnostics: Arc::from([]),
        };
        observer.record_status(&empty);
        observer.set_enabled_state(true, ReadOnlyState::Active);
        observer.record_trigger(1, json!({"kind": "command"}), None);
        observer.set_enabled_state(true, ReadOnlyState::Push);
        observer.record_status(&empty);
        assert_eq!(observer.bus().latest_sequence(), 0);
        assert_eq!(observer.state_snapshot()["memory"]["entries"], 0);
    }

    #[test]
    fn reconcile_pretrigger_terminal_snapshot_without_elapsed_tick_events() {
        let observer = ReadOnlyObserver::default();
        observer.set_enabled_state(true, ReadOnlyState::Push);
        let engine = TaskEngineHandle::new(None);
        let id = enqueue(&engine);
        let lease = engine.take_next_formal().unwrap().unwrap();
        assert_eq!(lease.task_id(), id);
        engine
            .complete_formal(id, FormalTaskCompletion::Failed("exact failure".into()))
            .unwrap();
        observer.record_status(&projection(&engine, 3));
        assert_eq!(
            observer.bus().latest_sequence(),
            0,
            "untracked workflows must stay silent"
        );
        observer.record_trigger(
            id,
            json!({"kind": "command", "parameters": {"keyword": "晴天"}}),
            None,
        );
        let events = observer.bus().replay_after(0);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "readonly.triggered");
        assert_eq!(events[1].payload["status"]["status"], "failed");
        assert_eq!(events[1].payload["status"]["result"], "exact failure");
        observer.record_status(&projection(&engine, 4));
        observer.record_trigger(id, json!({"kind": "command"}), None);
        assert_eq!(
            observer.bus().latest_sequence(),
            2,
            "ticks and duplicate receipt must be silent"
        );
        let evidence = observer.read_entry("readonly-1").unwrap();
        assert!(evidence["source"].is_null());
        assert_eq!(evidence["missingReason"], "flow_has_no_source_capture");
        assert_eq!(evidence["history"].as_array().unwrap().len(), 2);
        assert_eq!(evidence["history"][1]["status"]["status"], "failed");
    }

    #[test]
    fn overflow_reports_expired_entries_and_replay_gaps() {
        let observer = ReadOnlyObserver::default();
        observer.set_enabled_state(true, ReadOnlyState::Push);
        for id in 1..=270 {
            observer.record_trigger(id, json!({"kind": "command"}), None);
        }
        assert!(
            observer
                .read_entry("readonly-1")
                .unwrap_err()
                .starts_with("evidence_expired")
        );
        assert_eq!(observer.state_snapshot()["memory"]["entries"], MAX_ENTRIES);
        assert_eq!(
            observer.state_snapshot()["droppedEntries"],
            270 - MAX_ENTRIES
        );
        assert!(
            observer.state_snapshot()["memory"]["entryBytes"]
                .as_u64()
                .unwrap()
                <= MAX_ENTRY_BYTES as u64
        );
        let replay = observer.list_after(0);
        assert_eq!(replay["gap"], true);
        assert_eq!(replay["events"].as_array().unwrap().len(), 256);
        observer.set_enabled_state(false, ReadOnlyState::Push);
        observer.record_trigger(271, json!({"kind": "command"}), None);
        assert_eq!(observer.bus().latest_sequence(), 270);
        assert_eq!(observer.state_snapshot()["memory"]["frames"], 0);
    }

    #[test]
    fn immediate_commands_have_no_formal_id_and_never_capture() {
        let observer = ReadOnlyObserver::default();
        observer.record_immediate(json!({"kind": "command"}), None, "done".into());
        assert_eq!(observer.bus().latest_sequence(), 0);
        observer.set_enabled_state(true, ReadOnlyState::Push);
        observer.record_immediate(
            json!({"kind": "command", "flow": "immediateAdministration",
            "parameters": {"SetCommandsEnabled": {"enabled": false, "username": "测试昵称"}},
            "raw": "禁用测试昵称", "username": "测试昵称"}),
            None,
            "已禁用测试昵称".into(),
        );
        let events = observer.bus().replay_after(0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "readonly.triggered");
        assert!(events[0].payload["taskId"].is_null());
        assert_eq!(events[0].payload["status"]["status"], "completed");
        let evidence = observer.read_entry("readonly-1").unwrap();
        assert_eq!(evidence["kind"], "immediate");
        assert!(evidence["source"].is_null());
        assert_eq!(evidence["history"].as_array().unwrap().len(), 1);
        assert!(!evidence.to_string().contains("测试昵称"));
        observer.set_enabled_state(true, ReadOnlyState::Active);
        observer.record_immediate(json!({"kind": "command"}), None, "done".into());
        assert_eq!(observer.bus().latest_sequence(), 1);
    }

    #[test]
    fn secrets_in_parameters_and_status_are_redacted() {
        let observer = ReadOnlyObserver::default();
        observer.set_enabled_state(true, ReadOnlyState::Push);
        let payload = json!({"kind": "command", "parameters": {"Invite": {
            "username": "测试昵称", "password": "123456", "Authorization": "secret-auth"}}});
        observer.record_trigger(1, payload, None);
        let mut data = observer.data.lock().unwrap();
        let entry = data.entries.front_mut().unwrap();
        entry.status = redact_strings(
            json!({"result": "测试昵称邀请123456执行完成"}),
            &entry.redactions,
        );
        drop(data);
        let evidence = observer.read_entry("readonly-1").unwrap().to_string();
        assert!(!evidence.contains("123456"));
        assert!(!evidence.contains("测试昵称"));
        assert!(!evidence.contains("secret-auth"));
    }

    /// This is opt-in because real screenshots are deliberately kept local and ignored by git.
    /// It checks exact pixel/capture metadata and encoding; it does not claim game OCR acceptance.
    #[test]
    #[ignore = "requires the local real screenshot fixture; no synthetic frames"]
    fn real_source_frame_identity_active_push_and_cache_eviction() {
        use crate::runtime::ui::{CaptureFrame, UiDevice, UiRuntime};
        struct ImageDevice(image::DynamicImage);
        impl UiDevice for ImageDevice {
            fn capture(&mut self) -> anyhow::Result<image::DynamicImage> {
                Ok(self.0.clone())
            }
        }
        let image = image::open(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/ui/secondary-with-hall-row-1920x1080.png"),
        )
        .unwrap();
        let expected_hash = format!("{:x}", Sha256::digest(image.as_bytes()));
        let runtime = UiRuntime::start(ImageDevice(image), 2).unwrap();
        let capture = || {
            Arc::new(
                runtime
                    .handle()
                    .submit_external(CaptureFrame)
                    .unwrap()
                    .wait()
                    .unwrap()
                    .unwrap(),
            )
        };
        let first = capture();
        let second = capture();
        let observer = ReadOnlyObserver::default();
        observer.set_enabled_state(true, ReadOnlyState::Active);
        let active_id = observer
            .record_active(first.clone(), json!({"request": "fresh"}))
            .unwrap();
        assert_eq!(observer.bus().latest_sequence(), 0);
        let active = observer.read_entry(&active_id).unwrap();
        assert_eq!(active["source"]["frameId"], first.id());
        assert_eq!(active["source"]["pixelSha256"], expected_hash);
        assert_eq!(active["source"]["canvas"], "rawGame");
        assert!(
            active["source"]["imagePngBase64"]
                .as_str()
                .unwrap()
                .starts_with("iVBOR")
        );
        observer.set_enabled_state(true, ReadOnlyState::Push);
        assert!(observer.record_active(first.clone(), json!({})).is_err());
        observer.cache_frame(first.clone());
        observer.cache_frame(second.clone());
        observer.record_trigger(1, json!({"kind": "command"}), Some(first.captured_at()));
        let pushed_id = observer.bus().replay_after(0)[0].payload["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let pushed = observer.read_entry(&pushed_id).unwrap();
        assert_eq!(
            pushed["source"]["frameId"],
            first.id(),
            "a newer frame cannot replace the OCR source"
        );
        assert_eq!(pushed["source"]["pixelSha256"], expected_hash);
        use crate::features::moderation::{ModerationAction, ModerationCommand};
        let mut public_moderation = RoutedCommand::console(
            "拉黑",
            "拉黑UID123456789",
            ModuleCommand::Moderation(ModerationCommand {
                action: ModerationAction::Blacklist,
                uid: "123456789".into(),
                requester: "香菜".into(),
            }),
        );
        public_moderation.message_type = "blue".into();
        public_moderation.username = "香菜".into();
        public_moderation.observation.captured_at = Some(first.captured_at());
        public_moderation.observation.message_text = Some("香菜：@拉黑UID123456789".into());
        observer.record_trigger(
            30,
            command_parameters(&public_moderation),
            command_capture(&public_moderation),
        );
        let public = observer.read_entry("readonly-3").unwrap();
        assert_eq!(public["source"]["frameId"], first.id());
        assert_eq!(public["task"]["originalText"], "香菜：@拉黑UID123456789");
        assert_eq!(
            public["task"]["parametersOmitted"],
            "private moderation context"
        );
        assert!(public["task"].get("sourceTextOmitted").is_none());
        public_moderation.message_type = "pink".into();
        observer.record_trigger(
            31,
            command_parameters(&public_moderation),
            command_capture(&public_moderation),
        );
        let private = observer.read_entry("readonly-4").unwrap();
        assert!(private["source"].is_null());
        assert_eq!(private["missingReason"], "private_source_frame_omitted");
        assert!(!private.to_string().contains("123456789"));
        observer.record_trigger(2, json!({"kind": "command"}), Some(Instant::now()));
        let missing_id = observer.bus().replay_after(0).last().unwrap().payload["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let missing = observer.read_entry(&missing_id).unwrap();
        assert!(missing["source"].is_null());
        assert_eq!(missing["missingReason"], "exact_source_frame_not_retained");
        for _ in 0..10 {
            observer.cache_frame(capture());
        }
        assert!(
            observer.state_snapshot()["memory"]["frames"]
                .as_u64()
                .unwrap()
                <= MAX_FRAMES as u64
        );
        assert!(
            observer.state_snapshot()["memory"]["frameBytes"]
                .as_u64()
                .unwrap()
                <= MAX_FRAME_BYTES as u64
        );
        assert!(observer.state_snapshot()["droppedFrames"].as_u64().unwrap() > 0);
        observer.record_trigger(
            99,
            json!({"kind": "convertedAfterVote"}),
            Some(first.captured_at()),
        );
        let late_id = observer.bus().replay_after(0).last().unwrap().payload["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let late = observer.read_entry(&late_id).unwrap();
        assert_eq!(
            late["source"]["frameId"],
            first.id(),
            "retained evidence keeps the exact old source after cache eviction"
        );
        observer.set_enabled_state(false, ReadOnlyState::Push);
        assert_eq!(observer.state_snapshot()["memory"]["frames"], 0);
        if let Some(directory) = std::env::var_os("MILIASTRA_READONLY_EVIDENCE_DIR") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            for (name, value) in [
                ("active.json", &active),
                ("trigger.json", &pushed),
                ("public-moderation.json", &public),
                ("private-moderation.json", &private),
                ("converted-after-cache-eviction.json", &late),
            ] {
                std::fs::write(
                    directory.join(name),
                    serde_json::to_vec_pretty(value).unwrap(),
                )
                .unwrap();
            }
            first.image().save(directory.join("active.png")).unwrap();
            first.image().save(directory.join("trigger.png")).unwrap();
            std::fs::write(
                directory.join("events.json"),
                serde_json::to_vec_pretty(&observer.list_after(0)).unwrap(),
            )
            .unwrap();
            std::fs::write(directory.join("provenance.json"), serde_json::to_vec_pretty(&json!({
                "kind": "test_hook_fixture", "actualBusinessCommandExecuted": false,
                "actualGameCaptureDuringTest": false,
                "fixture": "tests/fixtures/ui/secondary-with-hall-row-1920x1080.png",
                "checks": ["exact capture metadata", "raw pixel hash", "bounded evidence retention"],
                "canvas": "rawGame"})).unwrap()).unwrap();
        }
    }
}
