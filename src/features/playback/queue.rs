use anyhow::{Result, anyhow};
use miliastra_playback::{PlayableTrack, TrackKey};
use serde::{Deserialize, Serialize};

use crate::features::song_request::SearchCandidate;

use super::matcher;
#[cfg(test)]
use super::state::RequestStateStore;
use super::state::SharedRequestStateStore;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueItem {
    pub id: u64,
    pub keyword: String,
    pub source: String,
    pub prefer_accompaniment: bool,
    pub ai_original_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<PlayableTrack>,
    pub friend_username: String,
    #[serde(default)]
    pub requester: String,
    pub dedup_bypass: bool,
    #[serde(default)]
    pub candidate_snapshot: Vec<SearchCandidate>,
}

impl Default for QueueItem {
    fn default() -> Self {
        Self {
            id: 0,
            keyword: String::new(),
            source: "qqmusic".to_string(),
            prefer_accompaniment: false,
            ai_original_text: String::new(),
            track: None,
            friend_username: String::new(),
            requester: String::new(),
            dedup_bypass: false,
            candidate_snapshot: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct PersistentQueue {
    max_size: usize,
    next_id: u64,
    items: Vec<QueueItem>,
    request_store: SharedRequestStateStore,
}

impl PersistentQueue {
    pub(crate) fn from_request_store(
        request_store: SharedRequestStateStore,
        max_size: usize,
    ) -> Result<Self> {
        let (next_id, items) = request_store
            .lock()
            .map_err(|_| anyhow::anyhow!("请求状态存储锁已中毒"))?
            .queue_snapshot();
        Ok(Self {
            max_size,
            next_id,
            items,
            request_store,
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(max_size: usize) -> Result<Self> {
        Self::from_request_store(RequestStateStore::new_for_test(), max_size)
    }

    pub fn items(&self) -> &[QueueItem] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.items.len() >= self.max_size
    }

    pub fn has_duplicate(&self, keyword: &str, source: &str, prefer_accompaniment: bool) -> bool {
        let source = normalize_source(source);
        self.items.iter().any(|item| {
            item.track.is_none()
                && matcher::same_song_query(&item.keyword, keyword)
                && normalize_source(&item.source) == source
                && item.prefer_accompaniment == prefer_accompaniment
        })
    }

    pub fn has_duplicate_track(&self, key: &TrackKey) -> bool {
        self.items.iter().any(|item| {
            item.track
                .as_ref()
                .is_some_and(|track| &track.track_ref.key == key)
        })
    }

    pub fn push(&mut self, item: QueueItem) -> Result<bool> {
        if self.is_full() {
            return Ok(false);
        }
        let mut items = self.items.clone();
        let id = self.next_id;
        let next_id = self.next_id.wrapping_add(1).max(1);
        items.push(QueueItem {
            id,
            source: normalize_source(&item.source),
            prefer_accompaniment: item.prefer_accompaniment,
            keyword: item.keyword,
            ai_original_text: item.ai_original_text,
            track: item.track,
            friend_username: item.friend_username,
            requester: item.requester,
            dedup_bypass: item.dedup_bypass,
            candidate_snapshot: item.candidate_snapshot,
        });
        self.save_state(&items, next_id)?;
        self.items = items;
        self.next_id = next_id;
        Ok(true)
    }

    pub fn remove_indexes(&mut self, indexes: &[usize]) -> Result<Vec<(usize, QueueItem)>> {
        let mut picked = indexes
            .iter()
            .copied()
            .filter(|index| *index < self.items.len())
            .collect::<Vec<_>>();
        picked.sort_unstable();
        picked.dedup();
        picked.sort_unstable_by(|left, right| right.cmp(left));

        let mut items = self.items.clone();
        let mut removed = Vec::new();
        for index in picked {
            let item = items.remove(index);
            removed.push((index + 1, item));
        }
        removed.reverse();
        if !removed.is_empty() {
            self.save_state(&items, self.next_id)?;
            self.items = items;
        }
        Ok(removed)
    }

    pub fn remove_id(&mut self, id: u64) -> Result<Option<(usize, QueueItem)>> {
        let Some(index) = self.items.iter().position(|item| item.id == id) else {
            return Ok(None);
        };
        Ok(self.remove_indexes(&[index])?.into_iter().next())
    }

    pub fn clear(&mut self) -> Result<usize> {
        let count = self.items.len();
        if count > 0 {
            self.save_state(&[], self.next_id)?;
            self.items.clear();
        }
        Ok(count)
    }

    fn save_state(&self, items: &[QueueItem], next_id: u64) -> Result<()> {
        self.request_store
            .lock()
            .map_err(|_| anyhow!("请求状态存储锁已中毒"))?
            .update(|snapshot| {
                snapshot.queue = items.to_vec();
                snapshot.next_queue_item_id = next_id;
                true
            })
            .map(|_| ())
    }
}

fn normalize_source(source: &str) -> String {
    if source.trim().is_empty() {
        String::new()
    } else if matches!(source, "qqmusic" | "netease" | "bilibili") {
        source.to_string()
    } else {
        "qqmusic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::playback::test_candidate;

    #[test]
    fn push_assigns_stable_ids_and_normalizes_source() {
        let mut queue = PersistentQueue::new_for_test(5).unwrap();
        let added = queue
            .push(QueueItem {
                keyword: "song name".to_string(),
                source: "netease".to_string(),
                candidate_snapshot: vec![test_candidate(
                    "song name - artist",
                    "miliastra://track/netease/42",
                )],
                ..QueueItem::default()
            })
            .unwrap();

        assert!(added);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.items()[0].id, 1);
        assert_eq!(queue.items()[0].keyword, "song name");
        assert_eq!(queue.items()[0].source, "netease");
        assert_eq!(
            queue.items()[0].candidate_snapshot,
            vec![test_candidate(
                "song name - artist",
                "miliastra://track/netease/42",
            )]
        );
    }

    #[test]
    fn remove_by_id_is_stable_after_front_item_is_shifted() {
        let mut queue = PersistentQueue::new_for_test(5).unwrap();

        for keyword in ["first", "second", "third"] {
            queue
                .push(QueueItem {
                    keyword: keyword.to_string(),
                    ..QueueItem::default()
                })
                .unwrap();
        }
        let third_id = queue.items()[2].id;

        assert_eq!(queue.remove_indexes(&[0]).unwrap()[0].1.keyword, "first");
        let removed = queue.remove_id(third_id).unwrap().unwrap();

        assert_eq!(removed.0, 2);
        assert_eq!(removed.1.keyword, "third");
        assert_eq!(queue.items().len(), 1);
        assert_eq!(queue.items()[0].keyword, "second");
    }
}
