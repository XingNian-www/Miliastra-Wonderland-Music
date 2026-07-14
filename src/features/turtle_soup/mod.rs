use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub(crate) mod repository;

#[derive(Clone, Debug, Deserialize, Serialize)]
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
