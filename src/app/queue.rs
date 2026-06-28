use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::song_matcher;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct QueueItem {
    pub keyword: String,
    pub source: String,
    pub prefer_accompaniment: bool,
    pub ai_original_text: String,
}

impl Default for QueueItem {
    fn default() -> Self {
        Self {
            keyword: String::new(),
            source: "qqmusic".to_string(),
            prefer_accompaniment: false,
            ai_original_text: String::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct QueueFile {
    items: Vec<QueueItem>,
}

#[derive(Debug)]
pub struct PersistentQueue {
    path: PathBuf,
    max_size: usize,
    items: Vec<QueueItem>,
}

impl PersistentQueue {
    pub fn load(path: PathBuf, max_size: usize) -> Result<Self> {
        let items = if path.exists() {
            let text = fs::read_to_string(&path)
                .with_context(|| format!("read queue state {}", path.display()))?;
            parse_queue_items(&text)
                .with_context(|| format!("parse queue state {}", path.display()))?
        } else {
            Vec::new()
        };
        Ok(Self {
            path,
            max_size,
            items,
        })
    }

    pub fn items(&self) -> &[QueueItem] {
        &self.items
    }

    pub fn front(&self) -> Option<&QueueItem> {
        self.items.first()
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
            song_matcher::same_song_query(&item.keyword, keyword)
                && normalize_source(&item.source) == source
                && item.prefer_accompaniment == prefer_accompaniment
        })
    }

    pub fn push(&mut self, item: QueueItem) -> Result<bool> {
        if self.is_full() {
            return Ok(false);
        }
        self.items.push(QueueItem {
            source: normalize_source(&item.source),
            prefer_accompaniment: item.prefer_accompaniment,
            keyword: item.keyword,
            ai_original_text: item.ai_original_text,
        });
        self.save()?;
        Ok(true)
    }

    pub fn shift(&mut self) -> Result<Option<QueueItem>> {
        if self.items.is_empty() {
            return Ok(None);
        }
        let item = self.items.remove(0);
        self.save()?;
        Ok(Some(item))
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

        let mut removed = Vec::new();
        for index in picked {
            let item = self.items.remove(index);
            removed.push((index + 1, item));
        }
        removed.reverse();
        if !removed.is_empty() {
            self.save()?;
        }
        Ok(removed)
    }

    pub fn clear(&mut self) -> Result<usize> {
        let count = self.items.len();
        if count > 0 {
            self.items.clear();
            self.save()?;
        }
        Ok(count)
    }

    pub fn save(&self) -> Result<()> {
        ensure_parent(&self.path)?;
        let text = serde_json::to_string_pretty(&QueueFile {
            items: self.items.clone(),
        })?;
        fs::write(&self.path, text)
            .with_context(|| format!("write queue state {}", self.path.display()))
    }
}

fn parse_queue_items(text: &str) -> Result<Vec<QueueItem>> {
    if let Ok(file) = serde_json::from_str::<QueueFile>(text) {
        return Ok(file.items);
    }
    if let Ok(items) = serde_json::from_str::<Vec<QueueItem>>(text) {
        return Ok(items);
    }
    let value: serde_json::Value = serde_json::from_str(text)?;
    if let Some(queue) = value.get("queue") {
        return serde_json::from_value(queue.clone()).context("parse queue array");
    }
    serde_json::from_value(value).context("parse queue state")
}

fn normalize_source(source: &str) -> String {
    if source == "netease" {
        "netease".to_string()
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
