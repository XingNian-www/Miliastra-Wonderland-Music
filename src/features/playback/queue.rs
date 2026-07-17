use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::matcher;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueItem {
    pub id: u64,
    pub keyword: String,
    pub source: String,
    pub prefer_accompaniment: bool,
    pub ai_original_text: String,
    pub uri: String,
    pub friend_username: String,
    pub dedup_bypass: bool,
}

impl Default for QueueItem {
    fn default() -> Self {
        Self {
            id: 0,
            keyword: String::new(),
            source: "qqmusic".to_string(),
            prefer_accompaniment: false,
            ai_original_text: String::new(),
            uri: String::new(),
            friend_username: String::new(),
            dedup_bypass: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QueueFile {
    next_id: u64,
    items: Vec<QueueItem>,
}

#[derive(Debug)]
pub struct PersistentQueue {
    path: PathBuf,
    max_size: usize,
    next_id: u64,
    items: Vec<QueueItem>,
}

impl PersistentQueue {
    pub fn load(path: PathBuf, max_size: usize) -> Result<Self> {
        let file_exists = path.exists();
        let file = if file_exists {
            let text = fs::read_to_string(&path)
                .with_context(|| format!("read queue state {}", path.display()))?;
            serde_json::from_str(&text)
                .with_context(|| format!("parse queue state {}", path.display()))?
        } else {
            QueueFile {
                next_id: 1,
                items: Vec::new(),
            }
        };
        if file_exists && file.items.iter().any(|item| item.id == 0) {
            bail!("播放队列当前格式要求每个 items[].id 大于 0");
        }
        if file_exists {
            let mut persisted_ids = HashSet::new();
            if file.items.iter().any(|item| !persisted_ids.insert(item.id)) {
                bail!("播放队列当前格式不允许重复的 items[].id");
            }
            let max_item_id = file.items.iter().map(|item| item.id).max().unwrap_or(0);
            if file.next_id == 0 || file.next_id <= max_item_id {
                bail!("播放队列当前格式要求 nextId 大于所有 items[].id");
            }
        }
        Ok(Self {
            path,
            max_size,
            next_id: file.next_id,
            items: file.items,
        })
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
            item.uri.is_empty()
                && matcher::same_song_query(&item.keyword, keyword)
                && normalize_source(&item.source) == source
                && item.prefer_accompaniment == prefer_accompaniment
        })
    }

    pub fn has_duplicate_uri(&self, uri: &str) -> bool {
        let uri = uri.trim();
        !uri.is_empty() && self.items.iter().any(|item| item.uri.trim() == uri)
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
            uri: item.uri,
            friend_username: item.friend_username,
            dedup_bypass: item.dedup_bypass,
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
        ensure_parent(&self.path)?;
        let text = serde_json::to_string_pretty(&QueueFile {
            next_id,
            items: items.to_vec(),
        })?;
        write_atomic(&self.path, &text)
    }
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    crate::adapters::file_store::write_atomic(path, text.as_bytes(), "播放队列")
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

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create state directory {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn push_persists_wrapped_queue_file() {
        let path = temp_queue_path("wrapped");
        let _ = fs::remove_file(&path);

        let mut queue = PersistentQueue::load(path.clone(), 5).unwrap();
        let added = queue
            .push(QueueItem {
                keyword: "song name".to_string(),
                source: "netease".to_string(),
                ..QueueItem::default()
            })
            .unwrap();

        assert!(added);
        assert_eq!(queue.len(), 1);

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"items\""));
        assert!(text.contains("\"nextId\""));

        let loaded = PersistentQueue::load(path.clone(), 5).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded.items()[0].id > 0);
        assert_eq!(loaded.items()[0].keyword, "song name");
        assert_eq!(loaded.items()[0].source, "netease");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn load_rejects_legacy_queue_shapes() {
        let path = temp_queue_path("legacy-shape");
        let _ = fs::remove_file(&path);
        fs::write(
            &path,
            r#"[{"id":1,"keyword":"legacy","source":"qqmusic","preferAccompaniment":false,"aiOriginalText":"","uri":"","friendUsername":"","dedupBypass":false}]"#,
        )
        .unwrap();

        let error = PersistentQueue::load(path.clone(), 5).expect_err("legacy array rejected");
        assert!(error.to_string().contains("parse queue state"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn load_rejects_current_queue_files_without_stable_item_ids() {
        let path = temp_queue_path("missing-stable-id");
        let _ = fs::remove_file(&path);
        fs::write(
            &path,
            r#"{
                "nextId": 1,
                "items": [{
                    "id": 0,
                    "keyword": "song",
                    "source": "qqmusic",
                    "preferAccompaniment": false,
                    "aiOriginalText": "",
                    "uri": "",
                    "friendUsername": "",
                    "dedupBypass": false
                }]
            }"#,
        )
        .unwrap();

        let error = PersistentQueue::load(path.clone(), 5)
            .expect_err("current queue items must already have stable ids");

        assert!(error.to_string().contains("id"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn load_rejects_duplicate_current_queue_item_ids() {
        let path = temp_queue_path("duplicate-stable-id");
        let _ = fs::remove_file(&path);
        let item = |keyword: &str| QueueItem {
            id: 1,
            keyword: keyword.to_string(),
            ..QueueItem::default()
        };
        fs::write(
            &path,
            serde_json::to_string(&QueueFile {
                next_id: 2,
                items: vec![item("first"), item("second")],
            })
            .unwrap(),
        )
        .unwrap();

        let error = PersistentQueue::load(path.clone(), 5)
            .expect_err("current queue item ids must be unique");

        assert!(error.to_string().contains("重复"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn load_rejects_current_queue_files_with_a_stale_next_id() {
        let path = temp_queue_path("stale-next-id");
        let _ = fs::remove_file(&path);
        fs::write(
            &path,
            serde_json::to_string(&QueueFile {
                next_id: 3,
                items: vec![QueueItem {
                    id: 3,
                    keyword: "song".to_string(),
                    ..QueueItem::default()
                }],
            })
            .unwrap(),
        )
        .unwrap();

        let error = PersistentQueue::load(path.clone(), 5)
            .expect_err("nextId must be greater than every persisted item id");

        assert!(error.to_string().contains("nextId"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn remove_by_id_is_stable_after_front_item_is_shifted() {
        let path = temp_queue_path("stable-id-remove");
        let _ = fs::remove_file(&path);
        let mut queue = PersistentQueue::load(path.clone(), 5).unwrap();

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

        let _ = fs::remove_file(path);
    }

    fn temp_queue_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "miliastra-queue-test-{}-{}-{}.json",
            std::process::id(),
            name,
            nanos
        ))
    }
}
