#[cfg(test)]
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::{TurtleSoupPuzzle, load_question_bank, parse_question_bank};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TurtleSoupSubmission {
    pub(crate) title: String,
    pub(crate) surface: String,
    pub(crate) bottom: String,
    #[serde(default)]
    pub(crate) adjudication_notes: String,
    #[serde(default = "default_enabled")]
    pub(crate) enabled: bool,
}

fn default_enabled() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TurtleSoupAppendReceipt {
    pub(crate) id: String,
    pub(crate) position: usize,
    pub(crate) total: usize,
}

#[derive(Clone)]
pub(crate) struct TurtleSoupBankStore {
    path: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

impl TurtleSoupBankStore {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn append(
        &self,
        submission: TurtleSoupSubmission,
    ) -> Result<TurtleSoupAppendReceipt> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| anyhow!("海龟汤题库写入锁已损坏"))?;
        let mut questions = if self.path.exists() {
            load_question_bank(&self.path)?
        } else {
            Vec::new()
        };
        let id = next_question_id(&questions);
        questions.push(TurtleSoupPuzzle {
            id: id.clone(),
            title: required_text("标题", submission.title)?,
            surface: required_text("汤面", submission.surface)?,
            bottom: required_text("汤底", submission.bottom)?,
            adjudication_notes: submission.adjudication_notes.trim().to_string(),
            enabled: submission.enabled,
        });

        let text = serde_yaml::to_string(&WrappedQuestionBankRef {
            questions: &questions,
        })
        .context("序列化海龟汤题库失败")?;
        parse_question_bank(&text, &self.path).context("写入前校验海龟汤题库失败")?;
        atomic_write(&self.path, text.as_bytes())?;
        Ok(TurtleSoupAppendReceipt {
            id,
            position: questions.len(),
            total: questions.len(),
        })
    }
}

#[derive(Serialize)]
struct WrappedQuestionBankRef<'a> {
    #[serde(rename = "题目")]
    questions: &'a [TurtleSoupPuzzle],
}

fn required_text(label: &str, value: String) -> Result<String> {
    let value = value.trim().to_string();
    if value.is_empty() {
        bail!("海龟汤{}不能为空", label);
    }
    Ok(value)
}

fn next_question_id(questions: &[TurtleSoupPuzzle]) -> String {
    let mut sequence = questions.len().saturating_add(1);
    loop {
        let id = format!("soup-{sequence:04}");
        if questions.iter().all(|question| question.id != id) {
            return id;
        }
        sequence = sequence.saturating_add(1);
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    crate::adapters::file_store::write_atomic(path, bytes, "海龟汤题库")
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn appends_questions_in_receipt_order_and_assigns_ids() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("mwm-turtle-bank-{suffix}.yaml"));
        let store = TurtleSoupBankStore::new(path.clone());

        let first = store
            .append(submission("第一题", "第一面", "第一底"))
            .unwrap();
        let second = store
            .append(submission("第二题", "第二面", "第二底"))
            .unwrap();

        assert_eq!(
            first,
            TurtleSoupAppendReceipt {
                id: "soup-0001".to_string(),
                position: 1,
                total: 1
            }
        );
        assert_eq!(
            second,
            TurtleSoupAppendReceipt {
                id: "soup-0002".to_string(),
                position: 2,
                total: 2
            }
        );
        let questions = load_question_bank(&path).unwrap();
        assert_eq!(
            questions
                .iter()
                .map(|item| item.title.as_str())
                .collect::<Vec<_>>(),
            vec!["第一题", "第二题"]
        );
    }

    #[test]
    fn rejects_missing_required_content_without_creating_a_file() {
        let path = std::env::temp_dir().join("mwm-turtle-bank-invalid.yaml");
        let _ = fs::remove_file(&path);
        let store = TurtleSoupBankStore::new(path.clone());
        let error = store.append(submission("", "汤面", "汤底")).unwrap_err();

        assert!(error.to_string().contains("标题不能为空"));
        assert!(!path.exists());
    }

    fn submission(title: &str, surface: &str, bottom: &str) -> TurtleSoupSubmission {
        TurtleSoupSubmission {
            title: title.to_string(),
            surface: surface.to_string(),
            bottom: bottom.to_string(),
            adjudication_notes: "只依据完整汤底裁决".to_string(),
            enabled: true,
        }
    }
}
