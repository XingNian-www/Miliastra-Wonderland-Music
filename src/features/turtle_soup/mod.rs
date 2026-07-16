use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::runtime::timer::{DeadlineKind, DeadlineModule, DeadlineToken};

pub(crate) mod repository;
mod service;

pub(crate) use repository::{TurtleSoupAppendReceipt, TurtleSoupSubmission};
pub(crate) use service::{
    QuestionSubmitOutcome, SecondaryOcrObservation, SecondaryOcrStability, TurtleSoupAiCompletion,
    TurtleSoupAiCompletionPort, TurtleSoupCommand, TurtleSoupCommandOutcome, TurtleSoupConfig,
    TurtleSoupQuestion, TurtleSoupService, TurtleSoupSnapshot, TurtleSoupWorkerRuntime,
    parse_question_message,
};

impl TurtleSoupCommand {
    pub(crate) fn parse_start(payload: &str) -> Option<Self> {
        (crate::features::chat_text::compact_command(payload) == "海龟汤").then_some(Self::Start)
    }

    pub(crate) fn parse_hall(payload: &str) -> Option<Self> {
        match crate::features::chat_text::compact_command(payload).as_str() {
            "状态" => Some(Self::Status),
            "结束" => Some(Self::End),
            _ => None,
        }
    }

    pub(crate) fn lock_key(&self) -> &'static str {
        match self {
            Self::Start => "turtle_soup:start",
            Self::Status => "turtle_soup:status",
            Self::End => "turtle_soup:end",
        }
    }

    pub(crate) fn same_request(&self, other: &Self) -> bool {
        self == other
    }
}

#[derive(Debug)]
pub struct TurtleSoupDeadlineModule;

impl DeadlineModule for TurtleSoupDeadlineModule {
    const NAME: &'static str = "turtle-soup";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TurtleSoupDeadlineKind {
    SessionMaximum,
    SessionIdle,
}

impl DeadlineKind for TurtleSoupDeadlineKind {
    type Module = TurtleSoupDeadlineModule;
}

pub type TurtleSoupDeadlineToken = DeadlineToken<TurtleSoupDeadlineKind>;

const DELIVERY_ATTEMPTS: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurtleSoupDeliveryPurpose {
    Opening,
    SurfaceRepeat,
    Judgment,
    Settlement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TurtleSoupDelivery {
    pub(crate) generation: u64,
    pub(crate) purpose: TurtleSoupDeliveryPurpose,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurtleSoupDeliveryOutcome {
    Added,
    DroppedEarlierMessage,
    Rejected,
}

pub(crate) trait TurtleSoupDeliveryPort {
    fn deliver_turtle_soup(
        &mut self,
        intent: TurtleSoupDeliveryIntent,
    ) -> Result<TurtleSoupDeliveryOutcome>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TurtleSoupDeliveryIntent {
    messages: Vec<String>,
    generation: u64,
    purpose: TurtleSoupDeliveryPurpose,
}

impl TurtleSoupDeliveryIntent {
    pub(crate) fn new(
        messages: Vec<String>,
        generation: u64,
        purpose: TurtleSoupDeliveryPurpose,
    ) -> Result<Self> {
        if messages.is_empty() {
            bail!("海龟汤投递不能为空");
        }
        Ok(Self {
            messages,
            generation,
            purpose,
        })
    }

    pub(crate) fn is_urgent(&self) -> bool {
        matches!(
            self.purpose,
            TurtleSoupDeliveryPurpose::Opening | TurtleSoupDeliveryPurpose::Settlement
        )
    }

    pub(crate) fn is_protected(&self) -> bool {
        !matches!(self.purpose, TurtleSoupDeliveryPurpose::Judgment)
    }

    pub(crate) fn max_attempts(&self) -> u8 {
        DELIVERY_ATTEMPTS
    }

    pub(crate) fn into_parts(self) -> (Vec<String>, TurtleSoupDelivery) {
        (
            self.messages,
            TurtleSoupDelivery {
                generation: self.generation,
                purpose: self.purpose,
            },
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TurtleSoupPuzzle {
    pub(crate) id: String,
    #[serde(rename = "标题")]
    pub(crate) title: String,
    #[serde(rename = "汤面")]
    pub(crate) surface: String,
    #[serde(rename = "汤底")]
    pub(crate) bottom: String,
    #[serde(rename = "裁决备注")]
    pub(crate) adjudication_notes: String,
    #[serde(rename = "启用")]
    pub(crate) enabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WrappedQuestionBank {
    #[serde(rename = "题目")]
    questions: Vec<TurtleSoupPuzzle>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum QuestionBankFile {
    Wrapped(WrappedQuestionBank),
    List(Vec<TurtleSoupPuzzle>),
}

pub(crate) fn load_question_bank(path: &Path) -> Result<Vec<TurtleSoupPuzzle>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("读取海龟汤题库失败: {}", path.display()))?;
    parse_question_bank(&text, path)
}

pub(crate) fn parse_question_bank(text: &str, path: &Path) -> Result<Vec<TurtleSoupPuzzle>> {
    let file: QuestionBankFile = serde_yaml::from_str(text)
        .with_context(|| format!("解析海龟汤题库失败: {}", path.display()))?;
    let mut questions = match file {
        QuestionBankFile::Wrapped(file) => file.questions,
        QuestionBankFile::List(questions) => questions,
    };
    if questions.is_empty() {
        bail!("海龟汤题库没有题目: {}", path.display());
    }
    let mut ids = HashSet::new();
    for (index, puzzle) in questions.iter_mut().enumerate() {
        let number = index + 1;
        if puzzle.id.trim().is_empty() {
            bail!("海龟汤题库第 {} 题 id 不能为空", number);
        }
        if puzzle.title.trim().is_empty() {
            bail!("海龟汤题库第 {} 题标题不能为空", number);
        }
        if puzzle.surface.trim().is_empty() {
            bail!("海龟汤题库第 {} 题汤面不能为空", number);
        }
        if puzzle.bottom.trim().is_empty() {
            bail!("海龟汤题库第 {} 题汤底不能为空", number);
        }
        puzzle.id = puzzle.id.trim().to_string();
        puzzle.title = puzzle.title.trim().to_string();
        if !ids.insert(puzzle.id.clone()) {
            bail!("海龟汤题库存在重复 ID: {}", puzzle.id);
        }
    }
    Ok(questions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_intent_owns_turtle_soup_scheduling_policy() {
        let opening = TurtleSoupDeliveryIntent::new(
            vec!["汤面1/1：测试".to_string()],
            7,
            TurtleSoupDeliveryPurpose::Opening,
        )
        .unwrap();
        assert!(opening.is_urgent());
        assert!(opening.is_protected());
        assert_eq!(opening.max_attempts(), 3);

        let judgment = TurtleSoupDeliveryIntent::new(
            vec!["[玩家]的问题回复：是".to_string()],
            7,
            TurtleSoupDeliveryPurpose::Judgment,
        )
        .unwrap();
        assert!(!judgment.is_urgent());
        assert!(!judgment.is_protected());
        assert_eq!(judgment.max_attempts(), 3);

        assert!(
            TurtleSoupDeliveryIntent::new(Vec::new(), 7, TurtleSoupDeliveryPurpose::Settlement,)
                .is_err()
        );
    }
}
