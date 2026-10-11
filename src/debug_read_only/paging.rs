//! HTTP envelopes and cursor rules for the read-only observer.
//!
//! Section cursors are UTF-8 byte offsets into a stable serialized JSON value.
//! Concatenate items[].text, then parse JSON to restore the complete section.
//! Each item is at most 1024 UTF-8 bytes; a page may shorten its last fragment
//! to fit. The entire success envelope counts against maxBytes. Error envelopes
//! report an unchanged cursor even when maxBytes cannot hold the minimum error.
//! Event cursors are producer sequences. Filtering scans only past delivered
//! matches: it never advances beyond a matching event that did not fit.
use super::*;

const MAX_PAGE_BYTES: usize = 65536;
const MAX_PAGE_ITEMS: usize = 100;
const FRAGMENT_BYTES: usize = 1024;

fn wire_size(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |v| v.len())
}

fn budget_error(instance: &str, cursor: u64, minimum: usize) -> Value {
    json!({"ok": false, "error": "page_budget_too_small", "instance": instance,
        "cursor": cursor, "nextCursor": cursor, "minimumBytes": minimum})
}

fn page_limits(limit: usize, bytes: usize) -> Result<(usize, usize), String> {
    if limit == 0 {
        return Err("invalid_limit: must be positive".into());
    }
    Ok((limit.min(MAX_PAGE_ITEMS), bytes.min(MAX_PAGE_BYTES)))
}

fn section_envelope(
    entry: &Entry,
    section: &str,
    cursor: usize,
    next: usize,
    total: usize,
    items: &[Value],
) -> Value {
    json!({"ok": true, "evidence": {"id": entry.id, "instance": entry.instance,
        "section": section, "revision": entry.revision,
        "page": {"encoding": "json-utf8", "items": items,
            "cursor": cursor, "nextCursor": next, "done": next == total,
            "revision": entry.revision, "totalBytes": total}}})
}

impl ReadOnlyObserver {
    /// No capture, OCR, image encoding, hashing or publication occurs here.
    pub(crate) fn summary_snapshot(&self) -> Value {
        let Ok(data) = self.data.lock() else {
            return json!({"instance": self.instance, "error": "observer unavailable"});
        };
        json!({"instance": self.instance, "revision": data.revision,
            "latestSequence": self.events.latest_sequence(),
            "taskCount": data.entries.iter().filter(|e| e.task_id.is_some()).count(),
            "tasks": data.entries.iter().rev().filter(|e| e.task_id.is_some())
                .take(5).map(Entry::summary).collect::<Vec<_>>(),
            "loss": loss_snapshot(&data), "retention": self.describe(&data)})
    }

    pub(crate) fn read_section(
        &self,
        id: &str,
        section: &str,
        cursor: usize,
        limit: usize,
        max_bytes: usize,
        revision: Option<u64>,
    ) -> Result<Value, String> {
        let (limit, max_bytes) = page_limits(limit, max_bytes)?;
        let (entry, _) = self.snapshot_entry(id)?;
        if revision.is_some_and(|revision| revision != entry.revision) {
            return Err(format!(
                "revision_conflict: current={}, cursor={cursor}",
                entry.revision
            ));
        }
        let value = match section {
            "metadata" => {
                let mut metadata = entry_metadata_value(&entry, false);
                metadata["status"] = entry.status.clone();
                metadata
            }
            "ocr" => entry.task.get("ocr").cloned().unwrap_or(Value::Null),
            "parameters" => {
                let mut task = entry.task.clone();
                if let Some(fields) = task.as_object_mut() {
                    fields.remove("ocr");
                }
                json!({"task": task})
            }
            "history" => {
                json!({"status": entry.status, "history": entry.history, "historyDropped": entry.history_dropped})
            }
            _ => return Err("invalid_section: metadata|ocr|parameters|history".into()),
        };
        let text = serde_json::to_string(&value).map_err(|e| e.to_string())?;
        if cursor > text.len() || !text.is_char_boundary(cursor) {
            return Err(format!(
                "invalid_cursor: UTF-8 boundary required, cursor={cursor}"
            ));
        }
        let mut next = cursor;
        let mut items = Vec::new();
        while next < text.len() && items.len() < limit {
            let end = utf8_floor(&text, next.saturating_add(FRAGMENT_BYTES));
            let boundaries: Vec<usize> = text[next..end]
                .char_indices()
                .map(|(offset, character)| next + offset + character.len_utf8())
                .collect();
            let mut low = 0;
            let mut high = boundaries.len();
            let mut best = None;
            // JSON escapes count too; character boundaries make every fragment reversible.
            while low < high {
                let mid = low + (high - low) / 2;
                let stop = boundaries[mid];
                let mut candidate = items.clone();
                candidate
                    .push(json!({"section": section, "offset": next, "text": &text[next..stop]}));
                let envelope =
                    section_envelope(&entry, section, cursor, stop, text.len(), &candidate);
                if wire_size(&envelope) <= max_bytes {
                    best = Some((stop, candidate));
                    low = mid + 1;
                } else {
                    high = mid;
                }
            }
            let Some((stop, candidate)) = best else {
                if items.is_empty() {
                    let stop = boundaries[0];
                    let first =
                        [json!({"section": section, "offset": next, "text": &text[next..stop]})];
                    let minimum = wire_size(&section_envelope(
                        &entry,
                        section,
                        cursor,
                        stop,
                        text.len(),
                        &first,
                    ));
                    return Ok(budget_error(&self.instance, cursor as u64, minimum));
                }
                break;
            };
            next = stop;
            items = candidate;
        }
        let envelope = section_envelope(&entry, section, cursor, next, text.len(), &items);
        if wire_size(&envelope) > max_bytes {
            return Ok(budget_error(
                &self.instance,
                cursor as u64,
                wire_size(&envelope),
            ));
        }
        Ok(envelope)
    }

    pub(crate) fn list_page(
        &self,
        since: u64,
        limit: usize,
        max_bytes: usize,
        task_id: Option<u64>,
        instance: Option<&str>,
    ) -> Result<Value, String> {
        let (limit, max_bytes) = page_limits(limit, max_bytes)?;
        // Producers take data before publishing: metadata loss and replay form one snapshot.
        let data = self.data.lock().map_err(|_| "observer unavailable")?;
        let events = self.events.replay_after(0);
        let latest = events
            .last()
            .map_or_else(|| self.events.latest_sequence(), |e| e.sequence);
        let reset = instance.is_some_and(|old| old != self.instance) || since > latest;
        let start = if reset { 0 } else { since };
        let oldest = events.first().map(|event| event.sequence);
        let gap = reset || oldest.is_some_and(|old| old > start.saturating_add(1));
        let mut loss = loss_snapshot(&data);
        // Recent tombstones are available in summary/mode; paging keeps only counters.
        loss.as_object_mut().unwrap().remove("recent");
        let envelope = |items: &[Value], next: u64, more: bool| {
            json!({"ok": true,
            "entries": {"events": items, "cursor": start, "nextCursor": next,
                "hasMore": more, "instance": self.instance, "revision": data.revision,
                "latestSequence": latest, "oldestSequence": oldest,
                "gap": gap, "reset": reset, "taskId": task_id, "loss": loss}})
        };
        let mut items = Vec::new();
        let mut next = start;
        let mut more = false;
        for event in events.iter().filter(|e| e.sequence > start) {
            let matches = task_id.is_none_or(|id| event.payload["taskId"].as_u64() == Some(id));
            if !matches {
                next = event.sequence;
                continue;
            }
            if items.len() == limit {
                more = true;
                break;
            }
            let mut candidate = items.clone();
            candidate.push(json!({"sequence": event.sequence, "kind": event.kind,
                "atMs": event.at_ms, "payload": event.payload}));
            // Conservatively reserve hasMore=true for a prefix that may stop here.
            let page = envelope(&candidate, event.sequence, true);
            if wire_size(&page) > max_bytes {
                if items.is_empty() {
                    // Nothing delivered: retain the original caller cursor even if
                    // filtering has scanned earlier nonmatching producer sequences.
                    return Ok(budget_error(&self.instance, since, wire_size(&page)));
                }
                more = true;
                break;
            }
            items = candidate;
            next = event.sequence;
        }
        let page = envelope(&items, next, more);
        if wire_size(&page) > max_bytes {
            return Ok(budget_error(&self.instance, since, wire_size(&page)));
        }
        Ok(page)
    }
}
