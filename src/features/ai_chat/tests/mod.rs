//! AI 聊天与记忆的单元测试，按子域分文件。
//!
//! 与生产模块一一对应：`config` / `memory` / `prompt` / `reply` / `service` /
//! `tools` / `vision`；`support` 只放共用夹具（配置、内存库）。
//!
//! `use super::*;` 把整个 `ai_chat` 的稳定路径带进本模块命名空间，子模块再
//! `use super::*` 就能拿到与拆分前完全相同的符号集——测试里不用写长路径。

use crate::features::command::CommandEnvelope;
use crate::features::identity::AiNamePair;
use image::DynamicImage;
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;

use super::budget::{
    ACTOR_READ_MIN_TURNS, ACTOR_READ_TOKEN_BUDGET, COMPRESS_TARGET_PERCENT,
    COMPRESS_TRIGGER_PERCENT, HALL_READ_BUDGET,
};
use super::config::{
    ACTION_PROTOCOL, IDENTITY_PROTOCOL, MEMORY_PROTOCOL, REPLY_PROTOCOL, TIME_PROTOCOL,
    TOOL_PROTOCOL, default_answer_prompt, default_command_tool_prompt, default_look_tool_prompt,
    default_speak_tool_prompt,
};
use super::memory::actor_turn_read_start;
use super::prompt::injected_turns;
use super::*;

mod support;

mod config;
mod memory;
mod prompt;
mod reply;
mod service;
mod tools;
mod vision;
