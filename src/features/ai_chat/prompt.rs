//! 请求拼装：system 提示词、用户消息尾部与历史注入、最终回复渲染。
//!
//! 分工固定：**静态规则**进 system（消息数组第 0 条，逐字节稳定以保住 provider
//! 前缀缓存）；**每轮变化的内容**（事实、摘要、大厅原文、刚用过的动作、可用命令、
//! 时间、当前问题）由代码无条件追加在用户消息尾部。

use serde::Deserialize;

use crate::features::identity::AiNamePair;
use crate::text::MAX_CHAT_WIDTH;

use super::config::{
    ACTION_PROTOCOL, AiChatConfig, HallSpeakerRole, IDENTITY_PROTOCOL, MEMORY_PROTOCOL,
    REPLY_PROTOCOL, TIME_PROTOCOL, TOOL_PROTOCOL, default_answer_prompt,
    default_command_tool_prompt, default_look_tool_prompt, default_speak_tool_prompt,
    default_system_prompt,
};
use super::memory::{ChatContext, HallContext, StoredTurn, asker_prefix, estimate_tokens};

/// 提示词组装所需的全部输入。
pub(crate) struct PromptInput<'a> {
    pub(crate) config: &'a AiChatConfig,
    pub(crate) question: &'a str,
    pub(crate) context: &'a ChatContext,
    pub(crate) allowed_commands: &'a [String],
    /// 提问人。大厅里同时在场的玩家很多，模型必须知道在跟谁说话。
    pub(crate) asker: &'a str,
    /// 大厅共享记忆（所有人最近说的 + 滚动摘要）。
    pub(crate) hall: &'a HallContext,
    /// 本轮调用的墙上时间（Unix 秒），由调用方取一次传入。
    ///
    /// 收 u64 而不是时钟对象：`build_prompt` 保持纯函数，测试直接给常量，
    /// 不需要 sleep 或任何时间设施。一轮只取一个值——工具循环内重算会让
    /// 同一次提问前后时间不一致。
    pub(crate) now_unix: u64,
}

/// 当前消息模板。历史由独立 user/assistant 消息承载，保留旧占位符兼容。
pub(crate) fn build_prompt(input: &PromptInput<'_>) -> String {
    // 事实按写入时间从旧到新排列。必须显式告诉模型这个顺序与优先级，
    // 否则改名、改称呼之后它会在新旧两条之间随机挑一条，表现为「没使用最新信息」。
    let facts = if input.context.facts.is_empty() {
        "（暂无）".to_string()
    } else {
        format!(
            "{}（按时间从旧到新，冲突时以后面一条为准）",
            input.context.facts.join("；")
        )
    };
    let history = if input.config.inject_actor_history {
        "（历史已作为独立对话消息提供）"
    } else {
        "（历史未注入：已配置关闭 inject_actor_history，仅以大厅共享记忆为准）"
    };
    // 除了原文，再单独列出「最近用过哪些开场动作」。
    //
    // 只给原文时模型看不到自己上次用了什么，只看到一大段几乎相同的文本，
    // 反而更容易继续套同一个模板（线上 63 条里 41 条同一个开头）。
    let recent_actions = {
        let openings = recent_actions(&input.context.turns, input.hall, 6);
        if openings.is_empty() {
            "（暂无）".to_string()
        } else {
            format!(
                "{}（这些动作最近刚用过，本次必须换一个）",
                openings.join("、")
            )
        }
    };
    let commands = if input.allowed_commands.is_empty() {
        // 空列表不等于「没有这个功能」：@邀请 这类命令存在但当前玩家无权限。
        // 原来渲染成「（当前不可用）」，模型读成了「我这没这功能」。
        "（命令功能是存在的，只是这位玩家当前没有权限使用；不要说「没有这个功能」）".to_string()
    } else {
        input.allowed_commands.join("、")
    };
    let asker = if input.asker.trim().is_empty() {
        "（未知）".to_string()
    } else {
        input.asker.trim().to_string()
    };
    // 大厅共享记忆分两段：摘要（中期）在前，原文（近期）在后。
    //
    // 全文只有这一条大厅通道：玩家行用 `[昵称]：`（与问题行同一套身份标记，
    // 裸名字容易被当成正文里的普通词），BOT 自己的历史行用 `你（角色名）：`
    // 明示是模型此前的输出，避免它把自己的猜测当成玩家说过的话。
    let mut hall_parts = Vec::new();
    if let Some(summary) = input.hall.summary.as_deref() {
        hall_parts.push(format!("【较早的大厅对话摘要】{summary}"));
    }
    if !input.hall.lines.is_empty() {
        let recent = input
            .hall
            .lines
            .iter()
            .map(|(speaker, text, role)| match role {
                HallSpeakerRole::Player => format!("[{speaker}]：{text}"),
                HallSpeakerRole::Bot => format!("你（{speaker}）：{text}"),
            })
            .collect::<Vec<_>>();
        if !recent.is_empty() {
            hall_parts.push(format!(
                "【最近的大厅对话】
{}",
                recent.join(
                    "
"
                )
            ));
        }
    }
    let hall = if hall_parts.is_empty() {
        "（暂无）".to_string()
    } else {
        hall_parts.join(
            "
",
        )
    };
    // 个人记忆的中期摘要同样要注入，否则压缩后信息就看不到了。
    let summary = input
        .context
        .summary
        .clone()
        .unwrap_or_else(|| "（暂无）".to_string());
    // 提问必须带 [昵称] 前缀。
    //
    // 只提供 {{asker}} 占位符是不够的：模板存在配置库里，用户在面板改过或
    // 沿用了旧默认值，就永远不会把这个占位符补上，模型也就看不到是谁在说话。
    // 把标识做进问题行本身，任何模板下都成立。
    // 昵称缺失时 asker_prefix 给 [未知]，不会出现空方括号。
    let question_line = format!("{}:{}", asker_prefix(input.asker), input.question.trim());
    // 单遍替换：模板里的占位符各自只展开一次。
    //
    // 链式 `replace` 会把**已插入的内容**再扫一遍：玩家在提问里写
    // `{{commands}}` 就会被当成模板展开，把命令清单、自己的事实记忆、
    // 甚至大厅共享记忆回显进本轮提示词——提示词注入 / 跨玩家内容泄露。
    //
    // `question`、`recent_actions`、`commands` 统一展开为**空串**：
    // 这三项由代码在尾部追加且只追加一次。旧模板里的占位符因此不会再重复，
    // 也不会把玩家输入里的同名占位符二次展开（单遍替换保证）。
    let empty = "";
    let vars = [
        ("keyword", input.config.keyword.as_str()),
        ("question", empty),
        ("facts", facts.as_str()),
        ("history", history),
        ("recent_actions", empty),
        ("commands", empty),
        ("asker", asker.as_str()),
        ("hall", hall.as_str()),
        ("summary", summary.as_str()),
    ];
    let rendered = render_template(&input.config.answer_prompt, &vars, default_answer_prompt());
    // 以下由代码无条件追加，**不依赖模板占位符**。
    //
    // 线上 answer_prompt 存在配置库里，用户沿用的是旧模板，新加的
    // recent_actions 等占位符永远不会被展开——这是动作去重一直没生效的
    // 直接原因。把必须生效的规则放在这里，任何模板下都成立。
    // 时间同样必须由代码无条件追加：线上模板是旧的手写版本，
    // 新占位符永远不会被展开（{{recent_actions}} 就是这么失效的）。
    //
    // 问题行必须留在**最后一行**：模型对末尾指令最敏感，而问题前半段
    // 可能包含会被误读成新指令的记忆/大厅原文。代码不依赖模板把
    // `{{question}}` 放在哪里，统一在尾部重建这一行。
    let clock = crate::features::clock_text::describe(input.now_unix);
    format!(
        "{rendered}\n\
         （以下由系统补充，任何模板下都生效）\n\
         当前时间：{clock}（UTC+8）。\n\
         本轮说话人：{asker}。只回答这个人，只使用这个人的事实。\n\
         最近用过的开场动作（含大厅其他人的回复）：{recent_actions}\n\
         这位玩家当前可用的命令：{commands}\n\
         {question_line}"
    )
}

/// 单遍展开 `{{name}}` 占位符。
///
/// 未定义的占位符原样保留（便于用户发现拼错），模板为空时回退到内置默认值——
/// 与 `response_system_prompt` 对 system_prompt 的处理保持一致。
fn render_template(template: &str, vars: &[(&str, &str)], fallback: String) -> String {
    let template = if template.trim().is_empty() {
        fallback
    } else {
        template.to_string()
    };
    let mut output = String::with_capacity(template.len());
    let mut rest = template.as_str();
    while let Some(start) = rest.find("{{") {
        let (before, tail) = rest.split_at(start);
        output.push_str(before);
        let Some(end) = tail[2..].find("}}").map(|index| index + 2) else {
            output.push_str(tail);
            return output;
        };
        let name = &tail[2..end];
        match vars.iter().find(|(key, _)| *key == name) {
            Some((_, value)) => output.push_str(value),
            // 未知占位符原样保留，且**不**把它当作模板继续扫描。
            None => {
                output.push_str(&tail[..end + 2]);
            }
        }
        rest = &tail[end + 2..];
    }
    output.push_str(rest);
    output
}

/// 按配置决定是否注入个人历史问答，并按注入预算裁剪。
///
/// 默认注入（保信息）；turns 仍完整保留在库里供面板/重建事实/`@忘记我`
/// 使用。注入时只取最近的一段：按 `estimate_tokens(question)+estimate_tokens(answer)`
/// 从新到旧累计，超过 `actor_history_token_budget` 就停，至少保留最近 1 轮。
pub(crate) fn injected_conversation_history(
    config: &AiChatConfig,
    context: &ChatContext,
    actor_display: &str,
) -> Vec<serde_json::Value> {
    if !config.inject_actor_history {
        return Vec::new();
    }
    let turns = injected_turns(&context.turns, config.actor_history_token_budget);
    conversation_history_for_turns(turns, actor_display)
}

/// 注入历史的边界块大小：边界按绝对 turn 位置每 8 轮跳一次。
///
/// messages 前缀缓存要求「第一条历史」尽量稳定。若每轮只滑一条，边界每轮
/// 变化，整段消息前缀都会失效；这里把 raw_start 向上取整到 8 的倍数，追加
/// 新 turn 时边界多数轮保持不变，只在跨块时跳一次。
const INJECT_BOUNDARY_CHUNK: usize = 8;

/// 从 turns 尾部（最近）向前挑出注入预算内的连续一段，至少保留 1 轮。
///
/// 隐私/费用边界都在这里：只按 token 选连续的一段尾部，不抽样、不拼接
/// 不连续的轮次，保证注入的 messages 之后与库里的真实顺序一致。
pub(crate) fn injected_turns(turns: &[StoredTurn], budget: usize) -> &[StoredTurn] {
    if turns.is_empty() {
        return turns;
    }
    let cost = |turn: &StoredTurn| estimate_tokens(&turn.question) + estimate_tokens(&turn.answer);
    let total: usize = turns.iter().map(cost).sum();
    if total <= budget {
        return turns;
    }
    // 先按预算算 raw_start（最近的连续一段，第一轮无条件保留）。
    let mut used = 0usize;
    let mut raw_start = turns.len();
    for turn in turns.iter().rev() {
        let cost = cost(turn);
        if raw_start < turns.len() && used.saturating_add(cost) > budget {
            break;
        }
        used = used.saturating_add(cost);
        raw_start -= 1;
    }
    // 绝对位置量化：向上取整到 8 轮块；clamp 到 len-1 保证至少保留最新 1 轮。
    // 只可能比 raw_start 更旧（丢更多），因此成本一定 ≤ budget。
    let rounded =
        ((raw_start + INJECT_BOUNDARY_CHUNK - 1) / INJECT_BOUNDARY_CHUNK) * INJECT_BOUNDARY_CHUNK;
    let start = rounded.min(turns.len() - 1);
    &turns[start..]
}

/// 按时间顺序恢复角色化的个人会话，绝不把全局发言冒充该玩家的历史。
pub(crate) fn conversation_history(
    context: &ChatContext,
    actor_display: &str,
) -> Vec<serde_json::Value> {
    // 历史统一用**展示昵称**，不能用记忆键（已登记玩家是 UUID）：
    // 当前问题写作 `[昵称]:…`，历史却写作 `[UUID]:…`，模型会当成两个人；
    // 把内部键发给第三方 API 也没有必要。
    conversation_history_for_turns(&context.turns, actor_display)
}

/// 把一段连续 turns 编成 OpenAI messages（user/assistant 成对）。
fn conversation_history_for_turns(
    turns: &[StoredTurn],
    actor_display: &str,
) -> Vec<serde_json::Value> {
    // 历史统一用**展示昵称**，不能用记忆键（已登记玩家是 UUID）：
    // 当前问题写作 `[昵称]:…`，历史却写作 `[UUID]:…`，模型会当成两个人；
    // 把内部键发给第三方 API 也没有必要。
    let display = if actor_display.trim().is_empty() {
        "（未知）"
    } else {
        actor_display.trim()
    };
    turns
        .iter()
        .flat_map(|turn| {
            [
                // 统一 `[昵称]:内容`：模型靠这个格式认人。
                serde_json::json!({
                    "role":"user",
                    "content": format!("{}:{}", asker_prefix(display), turn.question)
                }),
                serde_json::json!({"role":"assistant", "content":turn.answer}),
            ]
        })
        .collect()
}

/// 提示词块：装配预览与真实请求共用的最小单元。
#[derive(Clone, Debug)]
pub(crate) struct PromptBlock {
    pub(crate) name: &'static str,
    pub(crate) kind: PromptBlockKind,
    /// 来源：配置路径（动态）或 `code(常量名)`（内置默认）。
    pub(crate) source: std::borrow::Cow<'static, str>,
    pub(crate) text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PromptBlockKind {
    System,
    Tail,
}

fn protocol_block(
    name: &'static str,
    config_path: &'static str,
    code_name: &'static str,
    custom: &str,
    default: &'static str,
) -> PromptBlock {
    let custom = custom.trim();
    let (text, source) = if custom.is_empty() {
        (
            default.to_string(),
            std::borrow::Cow::Owned(format!("code({code_name})")),
        )
    } else {
        (custom.to_string(), std::borrow::Cow::Borrowed(config_path))
    };
    PromptBlock {
        name,
        kind: PromptBlockKind::System,
        source,
        text,
    }
}

/// system prompt 的块列表；拼接规则 = 各块 `text` 用单换行连接。
pub(crate) fn response_system_prompt_parts(
    config: &AiChatConfig,
    look_enabled: bool,
    speak_enabled: bool,
    command_enabled: bool,
) -> Vec<PromptBlock> {
    let persona = if config.system_prompt.trim().is_empty() {
        default_system_prompt()
    } else {
        config.system_prompt.clone()
    };
    let mut parts = vec![PromptBlock {
        name: "人设",
        kind: PromptBlockKind::System,
        source: if config.system_prompt.trim().is_empty() {
            std::borrow::Cow::Borrowed("code(default_system_prompt)")
        } else {
            std::borrow::Cow::Borrowed("ai_chat.system_prompt")
        },
        text: persona,
    }];
    parts.push(protocol_block(
        "记忆与回复规范",
        "ai_chat.protocols.memory",
        "MEMORY_PROTOCOL",
        &config.protocols.memory,
        MEMORY_PROTOCOL,
    ));
    parts.push(protocol_block(
        "身份与归属规范",
        "ai_chat.protocols.identity",
        "IDENTITY_PROTOCOL",
        &config.protocols.identity,
        IDENTITY_PROTOCOL,
    ));
    parts.push(protocol_block(
        "时间规范",
        "ai_chat.protocols.time",
        "TIME_PROTOCOL",
        &config.protocols.time,
        TIME_PROTOCOL,
    ));
    parts.push(protocol_block(
        "回复格式",
        "ai_chat.protocols.reply",
        "REPLY_PROTOCOL",
        &config.protocols.reply,
        REPLY_PROTOCOL,
    ));
    parts.push(protocol_block(
        "动作规范",
        "ai_chat.protocols.action",
        "ACTION_PROTOCOL",
        &config.protocols.action,
        ACTION_PROTOCOL,
    ));
    parts.push(protocol_block(
        "工具调用协议",
        "ai_chat.protocols.tool",
        "TOOL_PROTOCOL",
        &config.protocols.tool,
        TOOL_PROTOCOL,
    ));
    for (enabled, name, path, code_name, custom, default) in [
        (
            look_enabled,
            "look 工具提示词",
            "ai_chat.tool_prompts.look",
            "default_look_tool_prompt",
            &config.tool_prompts.look,
            "default_look_tool_prompt",
        ),
        (
            speak_enabled,
            "speak 工具提示词",
            "ai_chat.tool_prompts.speak",
            "default_speak_tool_prompt",
            &config.tool_prompts.speak,
            "default_speak_tool_prompt",
        ),
        (
            command_enabled,
            "命令工具提示词",
            "ai_chat.tool_prompts.command",
            "default_command_tool_prompt",
            &config.tool_prompts.command,
            "default_command_tool_prompt",
        ),
    ] {
        if !enabled {
            continue;
        }
        let default_text = match code_name {
            "default_look_tool_prompt" => default_look_tool_prompt(),
            "default_speak_tool_prompt" => default_speak_tool_prompt(),
            _ => default_command_tool_prompt(),
        };
        let custom = custom.trim();
        let (text, source) = if custom.is_empty() {
            (
                default_text,
                std::borrow::Cow::Owned(format!("code({default})")),
            )
        } else {
            (custom.to_string(), std::borrow::Cow::Borrowed(path))
        };
        parts.push(PromptBlock {
            name,
            kind: PromptBlockKind::System,
            source,
            text,
        });
    }
    parts
}

/// 只注入当前实际暴露工具的提示词；未暴露的工具不出现。
pub(crate) fn response_system_prompt_with_tools(
    config: &AiChatConfig,
    look_enabled: bool,
    speak_enabled: bool,
    command_enabled: bool,
) -> String {
    response_system_prompt_parts(config, look_enabled, speak_enabled, command_enabled)
        .into_iter()
        .map(|block| block.text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// 只给 look 视觉子请求使用的备注↔昵称对照；空表返回 None。
pub(crate) fn format_ai_name_pairs(pairs: &[AiNamePair]) -> Option<String> {
    if pairs.is_empty() {
        return None;
    }
    let mut body = String::new();
    for pair in pairs {
        let display = pair.effective_display();
        let line = format!("- 「{}」是「{}」\n", pair.left(), display);
        if body.chars().count().saturating_add(line.chars().count())
            > crate::features::identity::AI_NAME_PAIR_TOTAL_CHARS
        {
            break;
        }
        body.push_str(&line);
    }
    if body.is_empty() {
        return None;
    }
    Some(format!(
        "在图片中,昵称与身份对照关系如下\n{body}「」左侧的备注/真实昵称是内部识别信息，不要在回复中说出或复述；只可使用右侧显示昵称。"
    ))
}

/// 兼容无工具场景（例如纯文本调用）；工具提示词按需使用上面的函数。
pub(crate) fn response_system_prompt(config: &AiChatConfig) -> String {
    response_system_prompt_with_tools(config, false, false, false)
}

/// 清洗模型输出，去掉不适合发到游戏聊天的字符。
pub(crate) fn sanitize_answer(text: &str) -> String {
    // 星号是动作描写的定界符（`*猫耳一抖* 哼……`），**不能剥**。
    // 剥掉之后动作和台词粘成一句（`猫耳一抖哼，`），读起来像人设崩了，
    // 也让模型失去「这是一个独立片段」的线索，更容易每次照抄同一个开场。
    // 只清掉反引号与标题井号，这两个在聊天框里才是纯噪声。
    text.chars()
        .filter(|ch| !ch.is_control() || *ch == '\n')
        .collect::<String>()
        .replace(['`', '#'], "")
        .trim()
        .to_string()
}

/// 模型返回的结构化回复：动作与台词分开。
///
/// 以前靠模型自觉写成 `*猫耳一抖* 哼……`，代码再用分隔符反推哪段是动作——
/// 分隔符一换（星号后有没有空格、用逗号还是冒号）就切错，线上表现为
/// 「猫耳一抖」这种半句话被当成动作单独列出来，反而强化了重复。
/// 结构化输出把「哪段是动作」变成显式字段，不再靠猜。
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct AiChatReply {
    /// 动作/神态描写。可以为空——大多数回复不需要动作。
    #[serde(default)]
    pub(crate) action: String,
    /// 台词正文。
    #[serde(default)]
    pub(crate) text: String,
}

/// 一次问答的产物：给玩家看的渲染文本 + 供历史使用的原始 JSON。
///
/// 分开是因为三个消费者需求不同：发送要可读文本，`conversation_history`
/// 要原始 JSON（few-shot 一致，模型才会继续按格式输出），事实抽取要纯文本。
#[derive(Clone, Debug)]
pub(crate) struct AiChatAnswer {
    /// 原始 JSON（模型按格式返回时）；降级为纯文本时等于 rendered。
    pub(crate) raw: String,
    /// 实际发到聊天框的文本。
    pub(crate) rendered: String,
}

/// 解析模型回复。
///
/// 兼容三种形态，任一成立就用：
/// 1. 纯 JSON `{"action":"…","text":"…"}`（期望形态）；
/// 2. 被代码块或前后语包裹的 JSON（取第一个 `{` 到最后一个 `}`）；
/// 3. 纯文本（模型没按格式走）——整段当台词，动作留空。
///
/// **不接受 `deny_unknown_fields`**：模型多给一个 `emotion`/`expression` 字段就
/// 整体解析失败，然后落到分支 3 把 `{"action":…}` 原样发进游戏聊天——
/// 那比现在的自由文本更糟。多余字段一律忽略。
///
/// 分支 3 之前还要拦一次「看起来是 JSON 却解析不出来」：
/// max_tokens 截断会得到不闭合的 JSON，那种残片同样不能发给玩家。
pub(crate) fn parse_reply(raw: &str) -> AiChatReply {
    let trimmed = raw.trim();
    let accept = |reply: AiChatReply| -> Option<AiChatReply> {
        let action = reply.action.trim().to_string();
        let text = reply.text.trim().to_string();
        (!(action.is_empty() && text.is_empty())).then_some(AiChatReply { action, text })
    };
    if let Ok(reply) = serde_json::from_str::<AiChatReply>(trimmed) {
        if let Some(reply) = accept(reply) {
            return reply;
        }
    }
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}'))
        && end > start
        && let Ok(reply) = serde_json::from_str::<AiChatReply>(&trimmed[start..=end])
    {
        if let Some(reply) = accept(reply) {
            return reply;
        }
    }
    // 走到这里说明没拿到可用结构。如果原文明显是想输出 JSON（带花括号或键名），
    // 说明它被截断或写坏了——**丢弃**，不要原样发给玩家。
    // 调用方拿到空 text 会走 AI_CHAT_UNAVAILABLE 兜底文案。
    if looks_like_json(trimmed) {
        return AiChatReply::default();
    }
    AiChatReply {
        action: String::new(),
        text: trimmed.to_string(),
    }
}

/// 这段文本是不是「想输出 JSON 但没成功」。
///
/// 判据从宽：带花括号，或含 `"action"`/`"text"` 键名。宁可误判丢弃一条回复，
/// 也不要把 `{"action":"猫耳一抖","text":"哼` 这种残片发到大厅。
fn looks_like_json(text: &str) -> bool {
    text.starts_with('{')
        || text.ends_with('}')
        || text.contains("\"action\"")
        || text.contains("\"text\"")
}

/// 把结构化回复拼成实际要发的内容。
///
/// 动作用空格与台词衔接，**不是换行**。
///
/// 曾以为换行能让动作成为独立气泡，实际不行：下游
/// `split_numbered_chat_message` 的 `normalize_segment_source` 会把 `\n`
/// 折成空格（那条折叠规则是给「一行一条消息」的配置文案写的），
/// 所以换行在这里是空操作，唯一收益只是去掉动作与台词之间的粘连。
///
/// 真要两个气泡，得让本函数返回 `Vec<String>` 直接喂 `reply_batch`，
/// 并同时下调 160 字的回答预算——那是独立改动，不在本次范围。
pub(crate) fn render_reply(reply: &AiChatReply) -> String {
    let action = sanitize_action(&reply.action);
    let text = sanitize_answer(&reply.text);
    match (action.is_empty(), text.is_empty()) {
        (true, true) => String::new(),
        (true, false) => text,
        (false, true) => action,
        (false, false) => format!("{action} {text}"),
    }
}

/// 动作描写清洗：压平换行、去 Markdown 噪声，统一成 `*…*`。
///
/// 动作必须是单行片段：里面留换行会被下游折成空格，反而把一条动作拆成
/// 两段读起来断掉的文字。反引号与井号跟台词用同一套清理口径。
pub(crate) fn sanitize_action(text: &str) -> String {
    let flattened: String = text
        .chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .filter(|ch| !ch.is_control())
        .collect::<String>()
        .replace(['`', '#'], "");
    let trimmed = flattened.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // 模型可能自己带星号，也可能不带；统一成 *…* 形式。
    let inner = trimmed.trim_matches('*').trim();
    if inner.is_empty() {
        return String::new();
    }
    format!("*{inner}*")
}

/// 从最近动作里提取「开场动作」，用于抑制重复。
///
/// 两个来源合并（按最近去重，最多 `limit` 条，返回旧→新）：
/// 1. 当前玩家个人 turns 的 raw answer JSON（`action` 字段）；
/// 2. hall 里 BOT 对其他人的回复——受众过滤后，当前玩家自己的 BOT 行不在
///    hall，只看它会永远拿到「（暂无）」，所以必须先从个人 turns 取。
///
/// 提取规则：取冒号/空格/标点之前的一小段（动作描写）；不做复杂解析。
pub(crate) fn recent_actions(
    turns: &[StoredTurn],
    hall: &HallContext,
    limit: usize,
) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    fn push_opening(seen: &mut Vec<String>, text: &str, limit: usize) {
        if seen.len() >= limit {
            return;
        }
        let Some(opening) = opening_of(text) else {
            return;
        };
        if !seen.iter().any(|item| item == &opening) {
            seen.push(opening);
        }
    }
    // 个人 turns：answer 是原始 JSON，优先取 action；旧数据/纯文本回退原文。
    for turn in turns.iter().rev() {
        if seen.len() >= limit {
            break;
        }
        match parse_answer_action(&turn.answer) {
            // 成功解析出协议 JSON：只认 action。action 为空是完全合法的
            // （「不是每次回复都需要动作」），此时**跳过**，绝不能回退原文——
            // 回退会把 `{"action": "", "text": ...}` 整个 JSON 塞进提示词，
            // `opening_of` 再按标点切出 `{"action` 当成"最近用过的动作"。
            AnswerAction::Parsed(action) => {
                if let Some(action) = action {
                    push_opening(&mut seen, &action, limit);
                }
            }
            // 非 JSON（旧数据/纯文本回复）：原文就是动作描写，回退。
            AnswerAction::NotJson => push_opening(&mut seen, &turn.answer, limit),
        }
    }
    // 其他人的 BOT 回复：hall 已排除当前玩家自己的行。
    for (_, text, role) in hall.lines.iter().rev() {
        if seen.len() >= limit {
            break;
        }
        if matches!(role, HallSpeakerRole::Bot) {
            push_opening(&mut seen, text, limit);
        }
    }
    seen.reverse();
    seen
}

/// 一条历史回答的 action 解析结果。
enum AnswerAction {
    /// 协议 JSON，`action` 已 trim；空串/缺字段按 None 处理（表示这一轮没有动作）。
    Parsed(Option<String>),
    /// 不是 JSON 对象：旧数据或纯文本回复。
    NotJson,
}

fn parse_answer_action(answer: &str) -> AnswerAction {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(answer) else {
        return AnswerAction::NotJson;
    };
    // 能解析成 JSON 但不是对象（例如纯文本恰好是个数字/字符串字面量）：
    // 仍按旧数据处理，避免把 "123" 当成动作。
    if !value.is_object() {
        return AnswerAction::NotJson;
    }
    AnswerAction::Parsed(
        value
            .get("action")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|action| !action.is_empty())
            .map(str::to_owned),
    )
}

fn opening_of(text: &str) -> Option<String> {
    let opening = text
        .split(['：', ':', ' ', '，', '。', '\n'])
        .next()
        .map(str::trim)
        .unwrap_or_default();
    if opening.is_empty() || opening.chars().count() > 24 {
        return None;
    }
    Some(opening.to_string())
}

/// 回答长度上限（显示宽度），用于提示词约束与发送前检查。
pub(crate) const MAX_ANSWER_WIDTH: usize = MAX_CHAT_WIDTH * 2;
