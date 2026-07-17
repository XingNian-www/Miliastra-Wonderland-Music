use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::features::command::{
    CommandAuthority, CommandEnvelope, CommandPrefix, FeatureCommandMatch,
};
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

pub(crate) trait TurtleSoupApplicationPort {
    fn handle_hall_command(
        &mut self,
        player: &str,
        command: &TurtleSoupCommand,
    ) -> Result<TurtleSoupCommandOutcome>;
    fn handle_friend_command(
        &mut self,
        player: &str,
        command: &TurtleSoupCommand,
    ) -> Result<TurtleSoupCommandOutcome>;
    fn submit_question(&mut self, question: TurtleSoupQuestion) -> Result<QuestionSubmitOutcome>;
    fn send_current_hall(&mut self, message: &str) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TurtleSoupApplication;

impl TurtleSoupApplication {
    pub(crate) fn execute_command<P: TurtleSoupApplicationPort + ?Sized>(
        &self,
        raw_command: &str,
        player: &str,
        from_friend: bool,
        command: &TurtleSoupCommand,
        port: &mut P,
    ) -> Result<()> {
        let outcome = if from_friend {
            port.handle_friend_command(player, command)?
        } else {
            port.handle_hall_command(player, command)?
        };
        if let Some(reply) = outcome.immediate_reply {
            port.send_current_hall(&reply)?;
        }
        log::info!(
            "海龟汤命令已处理: command={} action={}",
            raw_command,
            outcome.action
        );
        Ok(())
    }

    pub(crate) fn submit_question<P: TurtleSoupApplicationPort + ?Sized>(
        &self,
        question: TurtleSoupQuestion,
        port: &mut P,
    ) -> Result<bool> {
        match port.submit_question(question)? {
            QuestionSubmitOutcome::Ignored => Ok(false),
            QuestionSubmitOutcome::Queued { request_id } => {
                log::info!("海龟汤提问已进入 AI 队列: request_id={}", request_id);
                Ok(true)
            }
            QuestionSubmitOutcome::Reply(reply) => {
                port.send_current_hall(&reply)?;
                Ok(true)
            }
        }
    }
}

pub(crate) enum TurtleSoupMutationIntent {
    Start { puzzle_id: Option<String> },
    End,
    AppendPuzzle(TurtleSoupSubmission),
}

pub(crate) enum TurtleSoupMutationOutcome {
    Started(TurtleSoupSnapshot),
    Ended {
        ended: bool,
        snapshot: TurtleSoupSnapshot,
    },
    PuzzleAppended(TurtleSoupAppendReceipt),
}

impl TurtleSoupCommand {
    pub(crate) fn claims_start_chat(envelope: &CommandEnvelope) -> bool {
        envelope.prefix() == CommandPrefix::Hash
            && envelope.authority() == CommandAuthority::HallMember
            && crate::features::chat_text::compact_command(envelope.command_text()) == "海龟汤"
    }

    pub(crate) fn claims_active_chat(envelope: &CommandEnvelope) -> bool {
        envelope.prefix() == CommandPrefix::Hash
            && envelope.authority() == CommandAuthority::HallMember
            && matches!(
                crate::features::chat_text::compact_command(envelope.command_text()).as_str(),
                "状态" | "结束"
            )
    }

    pub(crate) fn parse_start_chat(
        envelope: &CommandEnvelope,
    ) -> Option<FeatureCommandMatch<Self>> {
        if !Self::claims_start_chat(envelope) {
            return None;
        }
        Self::parse_start(envelope.command_text())
            .map(|command| FeatureCommandMatch::new("#", envelope.command_text(), command))
    }

    pub(crate) fn parse_active_chat(
        envelope: &CommandEnvelope,
    ) -> Option<FeatureCommandMatch<Self>> {
        if !Self::claims_active_chat(envelope) {
            return None;
        }
        Self::parse_hall(envelope.command_text())
            .map(|command| FeatureCommandMatch::new("#", envelope.command_text(), command))
    }

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

pub(crate) fn load_question_bank(path: &Path) -> Result<Vec<TurtleSoupPuzzle>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("读取海龟汤题库失败: {}", path.display()))?;
    parse_question_bank(&text, path)
}

pub(crate) fn parse_question_bank(text: &str, path: &Path) -> Result<Vec<TurtleSoupPuzzle>> {
    let file: WrappedQuestionBank = serde_yaml::from_str(text)
        .with_context(|| format!("解析海龟汤题库失败: {}", path.display()))?;
    let mut questions = file.questions;
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
    fn question_bank_requires_the_current_wrapped_shape() {
        let error = parse_question_bank(
            r#"
- id: old-001
  标题: 测试
  汤面: 测试汤面
  汤底: 测试汤底
  裁决备注: 测试备注
  启用: true
"#,
            Path::new("turtle_soup.yaml"),
        )
        .expect_err("a bare question list is not the current bank format");

        assert!(error.to_string().contains("解析海龟汤题库失败"));
    }

    struct CommandPort {
        replies: Vec<String>,
    }

    impl TurtleSoupApplicationPort for CommandPort {
        fn handle_hall_command(
            &mut self,
            _player: &str,
            _command: &TurtleSoupCommand,
        ) -> Result<TurtleSoupCommandOutcome> {
            Ok(TurtleSoupCommandOutcome {
                action: "status",
                immediate_reply: Some("海龟汤正在进行".to_string()),
            })
        }

        fn handle_friend_command(
            &mut self,
            _player: &str,
            _command: &TurtleSoupCommand,
        ) -> Result<TurtleSoupCommandOutcome> {
            unreachable!("hall command")
        }

        fn submit_question(
            &mut self,
            _question: TurtleSoupQuestion,
        ) -> Result<QuestionSubmitOutcome> {
            unreachable!("command test")
        }

        fn send_current_hall(&mut self, message: &str) -> Result<()> {
            self.replies.push(message.to_string());
            Ok(())
        }
    }

    #[test]
    fn command_application_delivers_the_immediate_reply() {
        let mut port = CommandPort {
            replies: Vec::new(),
        };

        TurtleSoupApplication
            .execute_command(
                "#状态",
                "Alice",
                false,
                &TurtleSoupCommand::Status,
                &mut port,
            )
            .expect("turtle soup command");

        assert_eq!(port.replies, ["海龟汤正在进行"]);
    }

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
