//! AI 聊天「装配预览」（dry-run）。
//!
//! 只做本地拼装：不写配置、不写记忆、不写 trace、不调用模型。
//! 与 `/ai/trace` 的分工：trace 看已经发生过的请求；预览看“现在如果发生一轮会拼成什么”。

use std::borrow::Cow;

use serde_json::json;

use crate::config::AppConfig;
use crate::features::ai_chat::{
    self, AiChatMemory, BOT_HISTORY_READ_BUDGET, ChatContext, HallContext, PromptBlock,
    PromptBlockKind, PromptInput,
};

fn estimate_tokens(chars: usize) -> usize {
    // 粗估：中文约 1 字 0.7 token，英文/标点约 0.25。这里只用于预览展示。
    (chars as f64 * 0.7).round() as usize
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn block_json(block: &PromptBlock) -> serde_json::Value {
    let chars = block.text.chars().count();
    json!({
        "name": block.name,
        "kind": match block.kind {
            PromptBlockKind::System => "system",
            PromptBlockKind::Tail => "tail",
        },
        "source": block.source.as_ref(),
        "chars": chars,
        "tokens": estimate_tokens(chars),
    })
}

/// 生成预览 JSON。
pub(crate) fn render_chat_preview(
    config: &AppConfig,
    memory: Option<&AiChatMemory>,
    asker: &str,
    question: &str,
) -> anyhow::Result<String> {
    let chat = &config.ai_chat;
    let look_enabled = ai_chat::look_tool_available(chat);
    let speak_enabled = config.tts.enabled
        && config.tts.trigger_scope == crate::features::tts::TtsTriggerScope::AiChat
        && !config.tts.api_key.trim().is_empty();
    let command_enabled = chat.allow_commands;

    let mut system_parts = ai_chat::response_system_prompt_parts(
        chat,
        look_enabled,
        speak_enabled,
        command_enabled,
    );
    if config.emoji.ai_enabled && !config.emoji.ai_allowed.is_empty() {
        let catalog = crate::features::emoji::ai_prompt_catalog(&config.emoji.ai_allowed);
        system_parts.push(PromptBlock {
            name: "emoji 表情目录",
            kind: PromptBlockKind::System,
            source: Cow::Borrowed("emoji.ai_allowed"),
            text: format!(
                "{catalog}\n想发表情时只能从上面清单里选，并把标签原样写进回复；不要编造不存在的表情。\n表情标签只能放在整条回复的末尾，不要插在句子中间，否则聊天里会被拆成多条消息。"
            ),
        });
    }
    let system = system_parts
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let (context, hall) = match memory {
        Some(memory) => (
            memory.context(asker, BOT_HISTORY_READ_BUDGET)?,
            memory.hall_context()?,
        ),
        None => (
            ChatContext {
                facts: Vec::new(),
                summary: None,
                turns: Vec::new(),
                bot_history: Vec::new(),
            },
            HallContext::default(),
        ),
    };
    let allowed_commands: Vec<String> = Vec::new();
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let tail = ai_chat::build_prompt(&PromptInput {
        config: chat,
        question,
        context: &context,
        allowed_commands: &allowed_commands,
        asker,
        hall: &hall,
        now_unix,
    });

    let mut tools: Vec<&str> = Vec::new();
    if command_enabled {
        tools.push("execute_game_command");
    }
    if speak_enabled {
        tools.push("speak");
    }
    if look_enabled {
        tools.push("look");
    }
    let tools_json = serde_json::to_string(&tools)?;


    let system_chars = system.chars().count();
    let tail_chars = tail.chars().count();
    let stable_prefix_bytes = system.len() + tools_json.len();
    let mut blocks: Vec<serde_json::Value> = system_parts.iter().map(block_json).collect();
    blocks.push(json!({
        "name": "本轮问题 + 系统补充段",
        "kind": "tail",
        "source": "code(build_prompt)",
        "chars": tail_chars,
        "tokens": estimate_tokens(tail_chars),
    }));

    let mut warnings: Vec<String> = Vec::new();
    if !memory.is_some() {
        warnings.push("记忆库未装配：预览中的个人/大厅记忆为空".to_string());
    }
    warnings.push("预览未计算该玩家可用命令：尾部按空命令列表展示".to_string());
    if chat.system_prompt.trim().is_empty() {
        warnings.push("system_prompt 为空，正在使用内置默认人设".to_string());
    }
    if stable_prefix_bytes < 1024 {
        warnings.push("稳定前缀不足 1024 字节，部分 provider 不会启用前缀缓存".to_string());
    }

    let payload = json!({
        "ok": true,
        "draft": true,
        "system": system,
        "tail": tail,
        "tools": tools,
        "blocks": blocks,
        "totals": {
            "systemChars": system_chars,
            "tailChars": tail_chars,
            "stablePrefixBytes": stable_prefix_bytes,
            "tailTokens": estimate_tokens(tail_chars),
            "prefixFingerprint": format!("{:016x}", fnv1a(format!("{system}{tools_json}").as_bytes())),
        },
        "warnings": warnings,
    });
    Ok(serde_json::to_string_pretty(&payload)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_system_matches_runtime_assembly() {
        let config = AppConfig::default();
        let chat = &config.ai_chat;
        let look = ai_chat::look_tool_available(chat);
        let speak = config.tts.enabled
            && config.tts.trigger_scope == crate::features::tts::TtsTriggerScope::AiChat
            && !config.tts.api_key.trim().is_empty();
        let expected = ai_chat::response_system_prompt_with_tools(chat, look, speak, chat.allow_commands);
        let payload = render_chat_preview(&config, None, "预览玩家", "问题").unwrap();
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["system"].as_str().unwrap(), expected);
        assert!(value["tail"].as_str().unwrap().contains("问题"));
        assert!(value["tools"].is_array());
        assert!(value["blocks"].as_array().unwrap().len() >= 7);
        assert_eq!(value["ok"], true);
    }

    #[test]
    fn preview_reports_draft_and_warnings() {
        let config = AppConfig::default();
        let payload = render_chat_preview(&config, None, "预览玩家", "问题").unwrap();
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["draft"], true);
        assert!(!value["warnings"].as_array().unwrap().is_empty());
        assert!(value["totals"]["stablePrefixBytes"].as_u64().unwrap() > 0);
        assert!(!value["totals"]["prefixFingerprint"].as_str().unwrap().is_empty());
    }
}
