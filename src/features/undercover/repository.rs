use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::{UndercoverWordPair, normalized_word_component, unordered_word_key};

#[derive(Clone)]
pub(crate) struct UndercoverBankStore {
    bank_path: PathBuf,
    used_path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl UndercoverBankStore {
    pub(crate) fn new(bank_path: PathBuf, used_path: PathBuf) -> Self {
        Self {
            bank_path,
            used_path,
            lock: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn consume_random(&self, seed: u64) -> Result<UndercoverWordPair> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("谁是卧底词库写入锁已损坏"))?;
        let candidates = load_candidates(&self.bank_path)?;
        let mut used = load_used_pairs(&self.used_path)?;
        let available = candidates
            .into_iter()
            .filter(|pair| !used.contains(&pair.unordered_key()))
            .collect::<Vec<_>>();
        if available.is_empty() {
            bail!("谁是卧底词库已耗尽");
        }
        let pair = available[(mix_seed(seed) as usize) % available.len()].clone();
        used.insert(pair.unordered_key());
        save_used_pairs(&self.used_path, &used)?;
        Ok(pair)
    }
}

#[derive(Deserialize)]
struct WordBankFile {
    #[serde(rename = "词组", default)]
    entries: Vec<WordEntry>,
}

#[derive(Deserialize)]
struct WordEntry {
    #[serde(rename = "平民词")]
    civilian: Option<String>,
    #[serde(rename = "卧底词")]
    undercover: Option<String>,
    #[serde(rename = "启用", default = "default_enabled")]
    enabled: bool,
}

fn default_enabled() -> bool {
    true
}

fn load_candidates(path: &Path) -> Result<Vec<UndercoverWordPair>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("读取谁是卧底词库失败: {}", path.display()))?;
    let file: WordBankFile = serde_yaml::from_str(&text)
        .with_context(|| format!("解析谁是卧底词库失败: {}", path.display()))?;
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    for entry in file.entries {
        if !entry.enabled {
            continue;
        }
        let Some(civilian) = entry.civilian.map(|word| word.trim().to_string()) else {
            continue;
        };
        let Some(undercover) = entry.undercover.map(|word| word.trim().to_string()) else {
            continue;
        };
        if civilian.is_empty()
            || undercover.is_empty()
            || normalized_word_component(&civilian) == normalized_word_component(&undercover)
        {
            continue;
        }
        let key = unordered_word_key(&civilian, &undercover);
        if seen.insert(key) {
            candidates.push(UndercoverWordPair::new(civilian, undercover));
        }
    }
    if candidates.is_empty() {
        bail!("谁是卧底词库没有可用词对");
    }
    Ok(candidates)
}

#[derive(Default, Deserialize, Serialize)]
struct UsedPairFile {
    #[serde(rename = "已用词对", default)]
    pairs: Vec<[String; 2]>,
}

fn load_used_pairs(path: &Path) -> Result<HashSet<String>> {
    if !path.exists() {
        return Ok(HashSet::new());
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("读取谁是卧底永久使用记录失败: {}", path.display()))?;
    let file: UsedPairFile = serde_yaml::from_str(&text)
        .with_context(|| format!("解析谁是卧底永久使用记录失败: {}", path.display()))?;
    Ok(file
        .pairs
        .into_iter()
        .map(|[left, right]| unordered_word_key(&left, &right))
        .collect())
}

fn save_used_pairs(path: &Path, keys: &HashSet<String>) -> Result<()> {
    let mut pairs = keys
        .iter()
        .filter_map(|key| {
            key.split_once('\0')
                .map(|(left, right)| [left.to_string(), right.to_string()])
        })
        .collect::<Vec<_>>();
    pairs.sort();
    let text =
        serde_yaml::to_string(&UsedPairFile { pairs }).context("序列化谁是卧底永久使用记录失败")?;
    atomic_write(path, text.as_bytes())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    crate::adapters::file_store::write_atomic(path, bytes, "谁是卧底记录")
}

fn mix_seed(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn consumes_unordered_word_pairs_permanently_and_skips_reverse_duplicates() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mwm-undercover-bank-{suffix}"));
        fs::create_dir_all(&dir).unwrap();
        let bank_path = dir.join("undercover.yaml");
        let used_path = dir.join("used.yaml");
        fs::write(
            &bank_path,
            r#"
词组:
  - 平民词: 苹果
    卧底词: 梨
    启用: true
  - 平民词: 梨
    卧底词: 苹果
    启用: true
  - 平民词: 猫
    卧底词: 狗
    启用: true
"#,
        )
        .unwrap();
        let store = UndercoverBankStore::new(bank_path, used_path.clone());

        let first = store.consume_random(1).unwrap();
        assert!(used_path.exists(), "used record is durable before return");
        let second = store.consume_random(2).unwrap();
        let keys = [first, second]
            .map(|pair| pair.unordered_key())
            .into_iter()
            .collect::<std::collections::HashSet<_>>();

        assert_eq!(keys.len(), 2);
        assert!(
            store
                .consume_random(3)
                .unwrap_err()
                .to_string()
                .contains("耗尽")
        );
    }

    #[test]
    fn corrupted_usage_record_fails_closed_without_overwriting_it() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mwm-undercover-corrupt-{suffix}"));
        fs::create_dir_all(&dir).unwrap();
        let bank_path = dir.join("undercover.yaml");
        let used_path = dir.join("used.yaml");
        fs::write(
            &bank_path,
            "词组:\n  - 平民词: 苹果\n    卧底词: 梨\n    启用: true\n",
        )
        .unwrap();
        fs::write(&used_path, "已用词对: [").unwrap();
        let store = UndercoverBankStore::new(bank_path, used_path.clone());

        let error = store.consume_random(1).unwrap_err();

        assert!(error.to_string().contains("解析谁是卧底永久使用记录失败"));
        assert_eq!(fs::read_to_string(used_path).unwrap(), "已用词对: [");
    }
}
