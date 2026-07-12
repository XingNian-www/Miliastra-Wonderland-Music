use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::turtle_soup::{TurtleSoupPuzzle, load_question_bank, parse_question_bank};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct TurtleSoupSubmission {
    pub(super) title: String,
    pub(super) surface: String,
    pub(super) bottom: String,
    #[serde(default)]
    pub(super) adjudication_notes: String,
    #[serde(default = "default_enabled")]
    pub(super) enabled: bool,
}

fn default_enabled() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct TurtleSoupAppendReceipt {
    pub(super) id: String,
    pub(super) position: usize,
    pub(super) total: usize,
}

#[derive(Clone)]
pub(super) struct TurtleSoupBankStore {
    path: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

impl TurtleSoupBankStore {
    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            path,
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub(super) fn append(
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
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建海龟汤题库目录失败: {}", parent.display()))?;
    }
    let temporary = temporary_path(path);
    let mut file = fs::File::create(&temporary)
        .with_context(|| format!("创建海龟汤题库临时文件失败: {}", temporary.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("写入海龟汤题库临时文件失败: {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("同步海龟汤题库临时文件失败: {}", temporary.display()))?;
    drop(file);
    replace_file(&temporary, path)
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = OsString::from(".");
    name.push(
        path.file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("turtle-soup")),
    );
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(windows)]
fn replace_file(temporary: &Path, target: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    let temporary = temporary
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            PCWSTR(temporary.as_ptr()),
            PCWSTR(target.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .with_context(|| "替换海龟汤题库失败")
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, target: &Path) -> Result<()> {
    fs::rename(temporary, target).with_context(|| {
        format!(
            "替换海龟汤题库失败: {} -> {}",
            temporary.display(),
            target.display()
        )
    })
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
