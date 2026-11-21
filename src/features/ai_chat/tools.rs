//! AI 工具声明与参数校验：`execute_game_command`、`look`、`speak`。
//!
//! 工具 schema 属于请求的**稳定前缀**：不得内嵌任何具体玩家/回合相关内容，
//! 否则每换一个人（或每次权限变化）前缀缓存就整体失效。

use anyhow::Result;
use serde::Deserialize;

use crate::features::tts::{DEFAULT_TTS_MODEL, TtsSpeakOptions, tts_model_capabilities};

use super::config::{AiChatConfig, configured_tool_prompt};

/// 工具参数严格独立于展示文字；不从自然语言回复解析命令。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AiChatExecution {
    pub command: String,
}

pub(crate) const COMMAND_TOOL: &str = "execute_game_command";
pub(crate) const MAX_TOOL_ROUNDS: usize = 3;

/// 二级界面导航截图等待上界；独立于 `vision.timeout_ms`（后者只用于单次截图）。
/// debug 实测约 2.3s，这里给 12s，覆盖设备唤醒 + 观察稳定 + 导航 + 返回。
pub(crate) const VISION_NAVIGATION_TIMEOUT_MS: u64 = 12_000;

/// AI 聊天内置的识图工具名。
pub(crate) const LOOK_TOOL: &str = "look";

/// `look` 工具定义：按需查看当前一级大厅截图；instruction 可选。
const DEFAULT_LOOK_TOOL_DESCRIPTION: &str = "查看当前一级大厅截图，由视觉子模型返回文字转述。主模型是否另收到原图取决于 ai_chat.vision.direct_to_main/当前配置；若本轮已直接收到图，应结合图判断，不要因 look 文案否认画面。主模型的用户本轮问题会一并传给视觉子模型。instruction 写清要从画面拿什么，可省略；示例：instruction=提取聊天区域里的文字。";

pub(crate) fn look_tool(config: &AiChatConfig) -> serde_json::Value {
    let description = configured_tool_prompt(
        &config.tool_descriptions.look,
        DEFAULT_LOOK_TOOL_DESCRIPTION,
    );
    serde_json::json!({
        "type": "function",
        "function": {
            "name": LOOK_TOOL,
            "description": description,
            // instruction 是可选参数，strict:true 会要求 required 覆盖所有字段；
            // 这里关掉 strict，保留“可选 instruction”语义。
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "instruction": {
                        "type": "string",
                        "description": "希望从截图了解什么，需自包含；省略则简要描述画面",
                        "maxLength": 200
                    }
                },
                "additionalProperties": false
            }
        }
    })
}

/// 解析 `look` 工具参数；instruction 可省略。
pub(crate) fn parse_look_arguments(raw: &str) -> Result<Option<String>> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LookArguments {
        #[serde(default)]
        instruction: Option<String>,
    }
    let arguments: LookArguments = serde_json::from_str(raw)?;
    let Some(instruction) = arguments.instruction else {
        return Ok(None);
    };
    let instruction = instruction.trim().to_string();
    if instruction.is_empty() {
        anyhow::bail!("look.instruction 不能为空");
    }
    if instruction.chars().count() > 200 {
        anyhow::bail!("look.instruction 不能超过 200 个字符");
    }
    if instruction.chars().any(char::is_control) {
        anyhow::bail!("look.instruction 不能包含控制字符");
    }
    Ok(Some(instruction))
}

/// `look` 子请求的 system prompt；短小，不自动注入聊天主提示词。
pub(crate) fn look_system_prompt() -> &'static str {
    "你在看一张游戏截图。只根据截图回答用户要求；看不清就说看不清，不要编造。输出一段简短自然语言，不要 Markdown、URL 或游戏命令。"
}

/// AI 聊天内置的语音工具名。只有 AI 聊天会暴露/处理它；普通回复不自动朗读。
pub(crate) const SPEAK_TOOL: &str = "speak";

/// `speak` 工具定义（未知模型安全默认，只暴露 text）。
const DEFAULT_SPEAK_TOOL_DESCRIPTION: &str =
    "让 BOT 用语音说出文本。text 用自然口语，不要 Markdown/URL/命令。";

/// 按当前 TTS 模型能力生成 `speak` 工具 schema；不支持的参数不暴露。
pub(crate) fn speak_tool_for_model(
    model: &str,
    max_chars: usize,
    config: &AiChatConfig,
) -> serde_json::Value {
    let model = if model.trim().is_empty() {
        DEFAULT_TTS_MODEL
    } else {
        model
    };
    let capabilities = tts_model_capabilities(model);
    let mut properties = serde_json::Map::new();
    let text_description = match capabilities.style_mode {
        crate::features::tts::TtsStyleMode::Parentheses => {
            "要朗读的自然口语；可内联 S1 固定括号标签，如 (sad)"
        }
        crate::features::tts::TtsStyleMode::Brackets => {
            "要朗读的自然口语；可内联方括号自然语言 cue，如 [whispers sweetly]"
        }
        crate::features::tts::TtsStyleMode::DramaDirections => {
            "要朗读的自然口语；可内联 [导演指示] 和 <whisper>…</whisper>"
        }
        crate::features::tts::TtsStyleMode::None => "要朗读的自然口语",
    };
    properties.insert(
        "text".to_string(),
        serde_json::json!({
            "type": "string",
            "description": text_description,
            "minLength": 1,
            "maxLength": max_chars.max(1)
        }),
    );
    if capabilities.supports_emotion {
        let description = match capabilities.style_mode {
            crate::features::tts::TtsStyleMode::Parentheses => "S1 固定括号标签词，如 sad",
            crate::features::tts::TtsStyleMode::Brackets => {
                "S2/S2.1 自由自然语言 cue，如 whispers sweetly"
            }
            crate::features::tts::TtsStyleMode::DramaDirections => {
                "Drama 3 导演指示，如 calm, measured storyteller voice"
            }
            crate::features::tts::TtsStyleMode::None => "",
        };
        properties.insert(
            "emotion".to_string(),
            serde_json::json!({"type":"string","description":description,"minLength":1,"maxLength":60}),
        );
    }
    if capabilities.supports_style {
        let style_description = match capabilities.style_mode {
            crate::features::tts::TtsStyleMode::DramaDirections => {
                "Drama 3 导演/表演风格，如 平静克制"
            }
            _ => "自然语言语气/风格，如 温柔",
        };
        properties.insert(
            "style".to_string(),
            serde_json::json!({
                "type":"string",
                "description":style_description,
                "minLength":1,
                "maxLength":60
            }),
        );
    }
    if capabilities.supports_speed {
        properties.insert(
            "speed".to_string(),
            serde_json::json!({
                "type":"number",
                "description":"语速倍率，0.5=慢一半，2.0=快一倍；默认 1.0",
                "minimum":0.5,
                "maximum":2.0
            }),
        );
    }
    if capabilities.supports_volume_db {
        properties.insert(
            "volume_db".to_string(),
            serde_json::json!({
                "type":"number",
                "description":"语音音量增益（dB），-20..20；0=不调整",
                "minimum":-20.0,
                "maximum":20.0
            }),
        );
    }
    let example = match capabilities.style_mode {
        crate::features::tts::TtsStyleMode::Parentheses => {
            "示例：text=我没事, emotion=sad（写成 (sad)）"
        }
        crate::features::tts::TtsStyleMode::Brackets => {
            "示例：text=我没事, emotion=laughing nervously（写成 [laughing nervously]）"
        }
        crate::features::tts::TtsStyleMode::DramaDirections => {
            "示例：text=我没事。[small laugh], emotion=forcing a smile"
        }
        crate::features::tts::TtsStyleMode::None => "示例：text=我没事",
    };
    serde_json::json!({
        "type": "function",
        "function": {
            "name": SPEAK_TOOL,
            "description": format!(
                "{}{example}",
                configured_tool_prompt(&config.tool_descriptions.speak, DEFAULT_SPEAK_TOOL_DESCRIPTION)
            ),
            // text 必填、emotion/style/speed/volume_db 可选；strict:true 要求所有
            // properties 都在 required 里，否则部分 OpenAI 兼容端点会整包 400。
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": serde_json::Value::Object(properties),
                "required": ["text"],
                "additionalProperties": false
            }
        }
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpeakArguments {
    text: String,
    #[serde(default)]
    emotion: Option<String>,
    #[serde(default)]
    style: Option<String>,
    #[serde(default)]
    speed: Option<f32>,
    #[serde(default)]
    volume_db: Option<f32>,
}

/// 解析旧调用（未知模型，仅 text）。
pub(crate) fn parse_speak_arguments(raw: &str) -> Result<String> {
    parse_speak_arguments_for_model(raw, "").map(|(text, _)| text)
}

/// 按所选模型能力解析/校验 `speak` 参数；不支持的字段直接拒绝，不静默忽略。
pub(crate) fn parse_speak_arguments_for_model(
    raw: &str,
    model: &str,
) -> Result<(String, TtsSpeakOptions)> {
    let model = if model.trim().is_empty() {
        DEFAULT_TTS_MODEL
    } else {
        model
    };
    let capabilities = tts_model_capabilities(model);
    let arguments: SpeakArguments = serde_json::from_str(raw)?;
    let text = arguments.text.trim().to_string();
    if text.is_empty() {
        anyhow::bail!("speak.text 不能为空");
    }
    if text.chars().any(char::is_control) {
        anyhow::bail!("speak.text 不能包含控制字符");
    }
    let normalize_text = |value: Option<String>, field: &str| -> Result<Option<String>> {
        let Some(value) = value else {
            return Ok(None);
        };
        let value = value.trim().to_string();
        if value.is_empty() {
            anyhow::bail!("speak.{field} 不能为空");
        }
        if value.chars().count() > 60 {
            anyhow::bail!("speak.{field} 不能超过 60 个字符");
        }
        if value.chars().any(char::is_control) {
            anyhow::bail!("speak.{field} 不能包含控制字符");
        }
        Ok(Some(value))
    };
    let emotion = normalize_text(arguments.emotion, "emotion")?;
    if emotion.is_some() && !capabilities.supports_emotion {
        anyhow::bail!("当前模型不支持 speak.emotion");
    }
    let style = normalize_text(arguments.style, "style")?;
    if style.is_some() && !capabilities.supports_style {
        anyhow::bail!("当前模型不支持 speak.style");
    }
    if arguments.speed.is_some() && !capabilities.supports_speed {
        anyhow::bail!("当前模型不支持 speak.speed");
    }
    if arguments.volume_db.is_some() && !capabilities.supports_volume_db {
        anyhow::bail!("当前模型不支持 speak.volume_db");
    }
    if let Some(speed) = arguments.speed
        && (!speed.is_finite() || !(0.5..=2.0).contains(&speed))
    {
        anyhow::bail!("speak.speed 必须在 0.5..=2.0");
    }
    if let Some(volume) = arguments.volume_db
        && (!volume.is_finite() || !(-20.0..=20.0).contains(&volume))
    {
        anyhow::bail!("speak.volume_db 必须在 -20..=20");
    }
    Ok((
        text,
        TtsSpeakOptions {
            emotion,
            style,
            speed: arguments.speed,
            volume_db: arguments.volume_db,
        },
    ))
}

/// 解析并校验 `execute_game_command` 的参数。
///
/// 模型输出是不可信输入：命令必须是单行、以 `@`/`#` 开头、长度 2..=40。
/// 校验放在这里而不是执行侧，保证工具循环里的每一次调用都过同一道闸门。
pub(crate) fn parse_execution(raw: &str) -> Result<AiChatExecution> {
    let mut execution: AiChatExecution = serde_json::from_str(raw)?;
    if execution
        .command
        .chars()
        .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
    {
        anyhow::bail!("AI 命令必须是无控制字符的单行文本");
    }
    execution.command = execution.command.trim().to_string();
    if !(execution.command.starts_with('@') || execution.command.starts_with('#'))
        || execution.command.chars().count() <= 1
        || execution.command.chars().count() > 40
    {
        anyhow::bail!("AI 命令前缀或长度不合法");
    }
    Ok(execution)
}

/// `execute_game_command` 的工具 schema。
///
/// **schema 必须与玩家身份无关**：工具定义属于请求的稳定前缀，把
/// 「这位玩家可用的命令列表」写进 description 会让每个玩家（以及每次权限
/// 变化）都产生不同的前缀，provider 的前缀缓存全部失效。可用命令仍在
/// `build_prompt` 的尾部（`{{commands}}` 与系统补充段）逐轮给出，
/// 权限复核也始终在执行前做。
const DEFAULT_COMMAND_TOOL_DESCRIPTION: &str = "以当前提问者身份执行一条游戏命令。可用命令以用户消息末尾「这位玩家当前可用的命令」为准，只能使用其中列出的命令；不要编造列表外的命令。回执 status=handled 即表示命令已执行完成、业务结果已确认；此时必须直接告知玩家已完成，禁止说「能不能成功看平台反馈」或让玩家自行查证。";

pub(crate) fn command_tools(config: &AiChatConfig) -> Vec<serde_json::Value> {
    let description = configured_tool_prompt(
        &config.tool_descriptions.command,
        DEFAULT_COMMAND_TOOL_DESCRIPTION,
    );
    vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": COMMAND_TOOL,
            "description": description,
            "strict": true,
            "parameters": {
                "type": "object",
                "properties": {"command": {"type": "string", "description": "完整单行游戏命令，例如 @点歌晴天 或 #提示", "minLength": 2, "maxLength": 40}},
                "required": ["command"],
                "additionalProperties": false
            }
        }
    })]
}
