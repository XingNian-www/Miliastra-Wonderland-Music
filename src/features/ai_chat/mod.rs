//! AI 聊天与轻量记忆。
//!
//! 设计约束：
//! - 聊天输入来自 OCR，属于不可信外部文本。关键词触发的闲聊不是 `@` 命令，
//!   因此不走命令信封；只有 AI 主动产出的命令才会被复核后执行。
//! - 记忆按玩家个人归属，与歌单/塔罗一致使用规范化昵称（备注名归并后的名字）。
//! - 记忆只用 SQLite 单库单表，不引入向量检索或外部服务；容量按 token 估算裁剪。
//! - 未配置 `ai.api_key` 或未启用时整段功能静默关闭：不回复、不消耗调用。
//! - AI 输出的命令按**提问者本人**的权限复核后执行，绝不提升权限。
//!
//! 目录化后的职责划分（这是本目录唯一的"结构文档"，子模块不再各写一遍）：
//! - `budget`：全部容量预算与压缩水位。它们互相牵制（压缩触发线必须低于兜底
//!   删除线、读取窗口必须小于落库预算），分散到各子模块后极易单独改坏其中一条；
//! - `config`：`ai_chat` 配置段与校验、关键词触发判定、内置协议块与提示词默认值；
//! - `vision`：识图配置、内存 JPEG 编码与「这轮要不要带图」的判据；
//! - `memory`：记忆存储（原文/事实/摘要/大厅行）、压缩、遗忘与事实抽取提示词；
//! - `prompt`：system 提示词拼装、用户消息尾部、历史注入与回复渲染；
//! - `tools`：AI 工具声明（`execute_game_command` / `look` / `speak`）与参数校验；
//! - `service`：触发冷却闸门 `TriggerGate`。
//!
//! 依赖方向是单向的：`budget` → `config`/`vision`/`memory`/`prompt`/`tools` →
//! `service`；`mod` 只做再导出，不含逻辑。因此任何一层都不会反向依赖上层，
//! 也不存在 `config` ↔ `tools` 的循环（`configured_tool_prompt` 这类"空串回退
//! 默认"的取值规则属于配置语义，放在 `config`，由 `tools` 单向引用）。

// 消费方在 `composition`（仅 Windows 编译）与 `features::ai_chat_preview`，因此
// Linux 测试构建下本模块大量公开项会被判为 dead_code；再导出门面里也有一部分
// 当前没有调用方（保留是为了不因拆分收窄 API）。两类都是刻意为之，显式放行，
// 避免真实告警被淹没。
#![allow(dead_code, unused_imports)]

mod budget;
mod config;
mod memory;
mod prompt;
mod service;
mod tools;
mod vision;

#[cfg(test)]
mod tests;

// 对外稳定路径：`crate::features::ai_chat::X` 与拆分前逐项一致，调用方零修改。
//
// `features` 是 crate 私有模块，所以这里只是把原有的可见性分层原样保留：
// 配置/识图类型为 `pub`，其余保持 `pub(crate)`。语气上分成三段，便于核对。
//
// 再导出集合 = 拆分前的完整公开面。消费方（`composition::application::{ai_chat_commands,
// listener,delivery}`、`features::ai_chat_preview`、`config::types`）只用到其中一部分，
// 其余保留是为了不因拆分而收窄 API——收窄会让后续 wave 想用时被迫穿透到子模块内部。

/// 配置与识图类型：拆分前是 `pub`，这里保持 `pub`。
pub use config::{
    AiChatAccess, AiChatConfig, AiChatProtocols, AiChatToolDescriptions, AiChatToolPrompts,
    AiReplyConfig, AiReplyMode, TriggerMode,
};
pub use vision::{AiChatVisionConfig, VisionCaptureScope, VisionMode};

/// 配置层的命令/触发类型与内置文案。
pub(crate) use config::{
    AiChatCommand, HallSpeakerRole, KeywordTrigger, TriggerSkip, keyword_trigger, summary_prompt,
};

/// 容量预算常量（含跨模块消费的 `FACT_EXTRACT_*` / `SUMMARY_*`）。
pub(crate) use budget::{
    ACTOR_FACTS_BUDGET, ACTOR_HISTORY_INJECT_TOKEN_BUDGET, ACTOR_SUMMARY_BUDGET,
    ACTOR_TOKEN_BUDGET, BOT_TOKEN_BUDGET, DEFAULT_COOLDOWN_SECONDS, FACT_EXTRACT_MAX_TOKENS,
    FACT_EXTRACT_TIMEOUT, HALL_BOT_READ_MAX_LINES, HALL_BOT_READ_TOKEN_BUDGET, HALL_SUMMARY_BUDGET,
    HALL_TOKEN_BUDGET, MAX_FACT_CHARS, MAX_FACTS_PER_ACTOR, MAX_QUESTION_CHARS, SUMMARY_MAX_TOKENS,
};

/// 记忆存储与压缩。
pub(crate) use memory::{
    ActorMemorySummary, AiChatMemory, ChatContext, CompressionPlan, HallContext, StoredTurn,
    estimate_tokens, fact_extraction_prompt, parse_fact_update, parse_summary,
};

/// 提示词拼装、历史注入与回复渲染。
pub(crate) use prompt::{
    AiChatAnswer, AiChatReply, MAX_ANSWER_WIDTH, PromptBlock, PromptBlockKind, PromptInput,
    build_prompt, conversation_history, format_ai_name_pairs, injected_conversation_history,
    parse_reply, render_reply, response_system_prompt, response_system_prompt_parts,
    response_system_prompt_with_tools, sanitize_action, sanitize_answer,
};

/// 触发冷却闸门。
pub(crate) use service::TriggerGate;

/// 工具 schema、参数解析与工具提示词。
pub(crate) use tools::{
    AiChatExecution, COMMAND_TOOL, LOOK_TOOL, MAX_TOOL_ROUNDS, SPEAK_TOOL,
    VISION_NAVIGATION_TIMEOUT_MS, command_tools, look_system_prompt, look_tool, parse_execution,
    parse_look_arguments, parse_speak_arguments, parse_speak_arguments_for_model,
    speak_tool_for_model,
};

/// 识图判据与编码。
pub(crate) use vision::{
    auto_image_enabled, encode_vision_image, look_quota_allows, look_tool_available,
};
