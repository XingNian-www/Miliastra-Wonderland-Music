use anyhow::{Result, anyhow};
use miliastra_playback::PlayableTrack;
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

impl QueueItem {
    /// 判定两个队列项是否构成重复，供队列去重与 HTTP 契约复用同一策略。
    ///
    /// 统一规则：
    /// - 双方都是结构化曲目：精确比较 TrackKey，避免不同版本/不同音源误判；
    /// - 其余形态（至少一方是待解析项）：比较查询特征（结构化项取曲目标题、
    ///   待解析项取 keyword），要求模糊匹配且音源、伴奏偏好一致。
    ///   这样结构化曲目与待解析项交叉形态不会漏去重。
    pub(crate) fn duplicates_with(&self, other: &QueueItem) -> bool {
        match (&self.track, &other.track) {
            (Some(left), Some(right)) => left.track_ref.key == right.track_ref.key,
            _ => {
                let left_query = self
                    .track
                    .as_ref()
                    .map(|track| track.metadata.title.as_str())
                    .unwrap_or(self.keyword.as_str());
                let right_query = other
                    .track
                    .as_ref()
                    .map(|track| track.metadata.title.as_str())
                    .unwrap_or(other.keyword.as_str());
                matcher::same_song_query(left_query, right_query)
                    && normalize_source(&self.source) == normalize_source(&other.source)
                    && self.prefer_accompaniment == other.prefer_accompaniment
            }
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

    /// 用共享请求状态存储中的最新队列快照覆盖内存缓存。
    /// 供共享存储内的原子事务（如确认+出队）落盘后同步缓存使用。
    pub(crate) fn sync_snapshot(&mut self, next_id: u64, items: Vec<QueueItem>) {
        self.next_id = next_id;
        self.items = items;
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

    pub fn contains_duplicate(&self, item: &QueueItem) -> bool {
        self.items
            .iter()
            .any(|existing| existing.duplicates_with(item))
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
    use crate::features::playback::{test_candidate, test_track};

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

    #[test]
    fn pending_items_match_by_query_features() {
        let mut queue = PersistentQueue::new_for_test(5).unwrap();
        queue
            .push(QueueItem {
                keyword: "晴天".to_string(),
                source: "netease".to_string(),
                ..QueueItem::default()
            })
            .unwrap();

        // 同 keyword + 同音源判重；音源不同不判重。
        assert!(queue.contains_duplicate(&QueueItem {
            keyword: "晴天".to_string(),
            source: "netease".to_string(),
            ..QueueItem::default()
        }));
        assert!(!queue.contains_duplicate(&QueueItem {
            keyword: "晴天".to_string(),
            source: "qqmusic".to_string(),
            ..QueueItem::default()
        }));
        assert!(!queue.contains_duplicate(&QueueItem {
            keyword: "稻香".to_string(),
            source: "netease".to_string(),
            ..QueueItem::default()
        }));
    }

    #[test]
    fn structured_track_and_pending_query_are_cross_detected_as_duplicates() {
        let mut queue = PersistentQueue::new_for_test(5).unwrap();
        // 队列中先有结构化曲目（keyword 为曲目标题）。
        queue
            .push(QueueItem {
                keyword: "晴天".to_string(),
                source: "netease".to_string(),
                track: Some(test_track("miliastra://track/netease/42", "晴天 - 周杰伦")),
                ..QueueItem::default()
            })
            .unwrap();

        // 待解析项用相同查询特征：命中结构化曲目，不再漏去重。
        assert!(queue.contains_duplicate(&QueueItem {
            keyword: "晴天".to_string(),
            source: "netease".to_string(),
            ..QueueItem::default()
        }));
        // 模糊子串同样命中。
        assert!(queue.contains_duplicate(&QueueItem {
            keyword: "周杰伦 晴天 现场".to_string(),
            source: "netease".to_string(),
            ..QueueItem::default()
        }));
        // 音源不同不判重。
        assert!(!queue.contains_duplicate(&QueueItem {
            keyword: "晴天".to_string(),
            source: "qqmusic".to_string(),
            ..QueueItem::default()
        }));
        // 伴奏偏好不同不判重。
        assert!(!queue.contains_duplicate(&QueueItem {
            keyword: "晴天".to_string(),
            source: "netease".to_string(),
            prefer_accompaniment: true,
            ..QueueItem::default()
        }));
    }

    #[test]
    fn pending_query_and_structured_track_are_cross_detected_as_duplicates() {
        let mut queue = PersistentQueue::new_for_test(5).unwrap();
        // 队列中先有待解析项。
        queue
            .push(QueueItem {
                keyword: "晴天".to_string(),
                source: "netease".to_string(),
                ..QueueItem::default()
            })
            .unwrap();

        // 新结构化曲目标题与待解析项匹配：判重。
        assert!(queue.contains_duplicate(&QueueItem {
            keyword: "晴天".to_string(),
            source: "netease".to_string(),
            track: Some(test_track("miliastra://track/netease/42", "晴天 - 周杰伦")),
            ..QueueItem::default()
        }));
        // 标题不匹配不判重。
        assert!(!queue.contains_duplicate(&QueueItem {
            keyword: "稻香".to_string(),
            source: "netease".to_string(),
            track: Some(test_track("miliastra://track/netease/43", "稻香 - 周杰伦")),
            ..QueueItem::default()
        }));
    }

    #[test]
    fn structured_tracks_compare_exact_key_not_title() {
        let mut queue = PersistentQueue::new_for_test(5).unwrap();
        queue
            .push(QueueItem {
                keyword: "晴天".to_string(),
                source: "netease".to_string(),
                track: Some(test_track("miliastra://track/netease/42", "晴天 - 周杰伦")),
                ..QueueItem::default()
            })
            .unwrap();

        // 同一 TrackKey 判重（即使标题写法不同）。
        assert!(queue.contains_duplicate(&QueueItem {
            keyword: "晴天".to_string(),
            source: "netease".to_string(),
            track: Some(test_track(
                "miliastra://track/netease/42",
                "晴天 现场 - 周杰伦"
            )),
            ..QueueItem::default()
        }));
        // 同标题不同 id：不同版本，不判重。
        assert!(!queue.contains_duplicate(&QueueItem {
            keyword: "晴天".to_string(),
            source: "netease".to_string(),
            track: Some(test_track("miliastra://track/netease/43", "晴天 - 周杰伦")),
            ..QueueItem::default()
        }));
    }
}
