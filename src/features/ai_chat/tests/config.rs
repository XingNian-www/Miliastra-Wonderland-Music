//! 关键词触发、配置校验与协议默认值。

use super::support::*;
use super::*;

#[test]
fn prefix_trigger_matches_only_at_the_start() {
    let config = config();

    assert_eq!(
        config.match_trigger("橘子 今天天气怎么样").as_deref(),
        Some("今天天气怎么样")
    );
    assert_eq!(config.match_trigger("橘子").as_deref(), Some(""));
    // 关键词出现在中间时，默认模式不触发。
    assert!(config.match_trigger("我昨天吃了橘子").is_none());
}

#[test]
fn contains_trigger_matches_anywhere_and_strips_the_keyword() {
    let mut config = config();
    config.trigger_mode = TriggerMode::Contains;

    assert_eq!(
        config.match_trigger("我昨天吃了橘子你呢").as_deref(),
        Some("我昨天吃了你呢")
    );
    assert!(config.match_trigger("今天天气不错").is_none());
}

#[test]
fn keyword_is_configurable_and_blank_disables_triggering() {
    let mut config = config();
    config.keyword = "小助手".to_string();
    assert_eq!(config.match_trigger("小助手在吗").as_deref(), Some("在吗"));
    assert!(config.match_trigger("橘子在吗").is_none());

    config.keyword = "   ".to_string();
    assert!(config.match_trigger("橘子在吗").is_none());
}

/// 回归：真实 OCR 文本来的一行正文带着发送者分隔符时也必须命中。
///
/// split_sender_line 只切在分隔符**之前**，所以 `[2#BOT|勿加]:橘子你是什么模型？`
/// 拆出来的 body 是 `:橘子你是什么模型？`。没有这一步归一化，Prefix 模式下
/// strip_prefix(橘子) 永远失败，表现就是玩家发「橘子…」AI 完全没反应。
#[test]
fn separator_prefixed_body_still_triggers() {
    let config = config();
    assert_eq!(
        config.match_trigger(":橘子你是什么模型？").as_deref(),
        Some("你是什么模型？")
    );
    assert_eq!(
        config.match_trigger("：橘子你是什么模型？").as_deref(),
        Some("你是什么模型？"),
        "全角冒号同样要处理"
    );
    // 幂等：已经去过分隔符的正文结果不变。
    assert_eq!(
        config.match_trigger("橘子你是什么模型？").as_deref(),
        Some("你是什么模型？")
    );
}

/// AiChatConfig::default() 必须是关闭的：配置段缺省时不静默替用户开启 AI。
///
/// 这也是「网页配置中心没开就完全没反应」的第一道门槛，必须显式可测。
#[test]
fn default_config_is_disabled_and_reports_that_reason() {
    let config = AiChatConfig::default();
    assert!(!config.enabled, "默认必须关闭，不能替用户开启外部调用");
    assert_eq!("橘子", config.keyword, "默认关键词保持在代码里");
    assert_eq!(
        keyword_trigger(&config, true, "blue", Some(("甲".to_string(), "橘子在吗"))),
        KeywordTrigger::Skip(TriggerSkip::Disabled)
    );
    assert!(TriggerSkip::Disabled.reason().contains("enabled"));
}

/// 配置段整体缺省时，serde 的 `#[serde(default)]` 会补上 AiChatConfig::default()。
///
/// 复现 AppConfig 的取法（`#[serde(default)] pub ai_chat`）：配置文件里根本没有
/// `ai_chat:` 段时，拿到的就是默认值——`enabled: false`。发布模板
/// src/config/test.full.yaml 正是这种情况，因此「没在配置中心勾启用就完全静默」
/// 是代码事实，不是猜测。
#[test]
fn missing_section_falls_back_to_disabled_defaults() {
    #[derive(Deserialize)]
    struct Holder {
        #[serde(default)]
        ai_chat: AiChatConfig,
    }

    let holder: Holder = serde_json::from_str("{}").expect("缺段时用默认值");
    assert!(
        !holder.ai_chat.enabled,
        "段缺失必须落到 enabled=false，不能默认开启"
    );
    assert_eq!(holder.ai_chat.keyword, "橘子");

    // 只写一部分字段不行：enabled / keyword 没有 serde 默认值，
    // 配置中心保存时必须提交整段（它确实是整段保存的）。
    assert!(
        serde_json::from_str::<Holder>(r#"{"ai_chat":{"keyword":"小助手"}}"#).is_err(),
        "缺 enabled 的部分段必须被拒绝，而不是猜一个开关值"
    );
}

/// 每条静默退路都要能被区分出来：以前它们都是同一个 `return Ok(())`。
#[test]
fn keyword_trigger_names_every_silent_skip_reason() {
    let config = config();
    let ok_split = || Some(("甲".to_string(), "橘子在吗"));

    assert_eq!(
        keyword_trigger(&config, false, "blue", ok_split()),
        KeywordTrigger::Skip(TriggerSkip::AiNotConfigured)
    );
    assert_eq!(
        keyword_trigger(&config, true, "pink", ok_split()),
        KeywordTrigger::Skip(TriggerSkip::NotHallMessage)
    );
    assert_eq!(
        keyword_trigger(&config, true, "blue", None),
        KeywordTrigger::Skip(TriggerSkip::SenderUnparsed)
    );
    assert_eq!(
        keyword_trigger(&config, true, "blue", Some(("  ".to_string(), "橘子在吗"))),
        KeywordTrigger::Skip(TriggerSkip::EmptySender)
    );
    assert_eq!(
        keyword_trigger(
            &config,
            true,
            "blue",
            Some(("甲".to_string(), "今天天气不错"))
        ),
        KeywordTrigger::Skip(TriggerSkip::KeywordMiss)
    );
    assert_eq!(
        keyword_trigger(&config, true, "blue", Some(("甲".to_string(), "橘子"))),
        KeywordTrigger::Skip(TriggerSkip::EmptyQuestion)
    );
    let long = format!("橘子{}", "字".repeat(MAX_QUESTION_CHARS + 1));
    assert_eq!(
        keyword_trigger(&config, true, "blue", Some(("甲".to_string(), &long))),
        KeywordTrigger::Skip(TriggerSkip::QuestionTooLong)
    );
}

/// 命中路径：昵称不带关键词；**正文原样传给模型，不剥关键词**。
///
/// 剥掉会造成语义断层：「橘子是笨蛋吗」丢主语、「你叫橘子吗」变成
/// 「你叫吗」。触发只需要关键词存在，模型需要完整句子。
#[test]
fn keyword_trigger_returns_sender_and_the_verbatim_body() {
    let config = config();
    assert_eq!(
        keyword_trigger(
            &config,
            true,
            "blue",
            Some(("2#BOT|勿加".to_string(), ":橘子你是什么模型？")),
        ),
        KeywordTrigger::Ask {
            sender: "2#BOT|勿加".to_string(),
            // 原话原样：关键词在开头也保留。
            question: "橘子你是什么模型？".to_string(),
        }
    );
    // 昵称里出现关键词不算触发：只看正文。
    assert_eq!(
        keyword_trigger(
            &config,
            true,
            "blue",
            Some(("橘子粉丝".to_string(), "今天天气不错")),
        ),
        KeywordTrigger::Skip(TriggerSkip::KeywordMiss)
    );
}

#[test]
fn disabled_configuration_rejects_an_empty_keyword() {
    let mut config = config();
    config.keyword = String::new();
    assert!(config.validate().is_err());

    config.enabled = false;
    config.keyword = String::new();
    assert!(config.validate().is_ok(), "关闭时不做额外校验");
}

/// 权限档位必须真的挡住不该触发的人。
#[test]
fn access_levels_gate_the_trigger_by_role() {
    use crate::features::identity::IdentityRole;
    // 默认不限制：连身份映射里查不到的人也能触发，保持既有行为。
    assert!(AiChatAccess::Everyone.allows(None));
    assert!(AiChatAccess::Everyone.allows(Some(IdentityRole::Friend)));

    // 收紧后，查不到身份的人一律拒绝——否则改个昵称就绕过限制。
    for access in [
        AiChatAccess::Friend,
        AiChatAccess::Admin,
        AiChatAccess::Owner,
    ] {
        assert!(!access.allows(None), "{access:?} 不该放过未登记的人");
    }

    assert!(AiChatAccess::Friend.allows(Some(IdentityRole::Friend)));
    assert!(AiChatAccess::Friend.allows(Some(IdentityRole::Admin)));
    assert!(!AiChatAccess::Admin.allows(Some(IdentityRole::Friend)));
    assert!(AiChatAccess::Admin.allows(Some(IdentityRole::Admin)));
    assert!(AiChatAccess::Admin.allows(Some(IdentityRole::Owner)));
    assert!(!AiChatAccess::Owner.allows(Some(IdentityRole::Admin)));
    assert!(AiChatAccess::Owner.allows(Some(IdentityRole::Owner)));
}

/// 新字段必须能被旧配置反序列化（缺省即所有人，不改变行为）。
#[test]
fn min_access_defaults_to_everyone_for_legacy_configs() {
    let config: AiChatConfig = serde_json::from_value(serde_json::json!({
        "enabled": true,
        "keyword": "橘子"
    }))
    .expect("旧配置必须仍可解析");
    assert_eq!(config.min_access, AiChatAccess::Everyone);
}

#[test]
fn forget_command_is_claimed_only_as_an_at_hall_command() {
    use crate::features::command::{CommandAuthority, CommandObservation};

    let envelope = |text: &str, message_type: &str| {
        CommandEnvelope::new(
            text,
            "甲",
            message_type,
            text,
            CommandObservation::default(),
        )
        .expect("envelope")
    };

    assert!(AiChatCommand::claims_chat(&envelope("@忘记我", "blue")));
    // 好友来源不抢注，保持既有私聊语义。
    assert!(!AiChatCommand::claims_chat(&envelope("@忘记我", "pink")));
    assert!(!AiChatCommand::claims_chat(&envelope("@忘记我啊", "blue")));
    assert!(!AiChatCommand::claims_chat(&envelope("#忘记我", "blue")));
    assert!(matches!(
        AiChatCommand::parse_chat(&envelope("@忘记我", "blue")).map(|m| m.command),
        Some(AiChatCommand::Forget)
    ));
    let _ = CommandAuthority::HallMember;
}

#[test]
fn custom_protocols_replace_builtins() {
    let mut config = config();
    config.protocols.memory = "自定义记忆规范".into();
    config.protocols.identity = "自定义身份规范".into();
    config.protocols.time = "自定义时间规范".into();
    config.protocols.action = "自定义动作规范".into();
    config.protocols.reply = "必须输出 {\"action\":\"…\",\"text\":\"…\"}".into();
    config.protocols.tool = "自定义工具协议".into();
    let prompt = response_system_prompt_with_tools(&config, false, false, false);
    for needle in [
        "自定义记忆规范",
        "自定义身份规范",
        "自定义时间规范",
        "自定义动作规范",
        "自定义工具协议",
    ] {
        assert!(prompt.contains(needle), "缺少自定义协议 {needle}: {prompt}");
    }
    for builtin in [
        "记忆与回复规范",
        "身份与归属规范",
        "时间规范：",
        "动作规范：",
        "工具调用协议：",
    ] {
        assert!(
            !prompt.contains(builtin),
            "内置协议未替换 {builtin}: {prompt}"
        );
    }
}

#[test]
fn empty_protocols_keep_builtin_exactly() {
    let config = config();
    let prompt = response_system_prompt_with_tools(&config, false, false, false);
    assert!(prompt.contains(MEMORY_PROTOCOL));
    assert!(prompt.contains(IDENTITY_PROTOCOL));
    assert!(prompt.contains(TIME_PROTOCOL));
    assert!(prompt.contains(REPLY_PROTOCOL));
    assert!(prompt.contains(ACTION_PROTOCOL));
    assert!(prompt.contains(TOOL_PROTOCOL));
}

#[test]
fn reply_protocol_requires_action_and_text_keys() {
    let mut config = AiChatConfig::default();
    config.protocols.reply = "直接回复纯文本即可".into();
    let message = config.validate().unwrap_err().to_string();
    assert!(
        message.contains("ai_chat.protocols.reply"),
        "message={message}"
    );
    config.protocols.reply = "请输出 {\"action\":\"动作\",\"text\":\"台词\"}".into();
    assert!(config.validate().is_ok());
}

#[test]
fn tool_protocol_rejects_negative_phrasing() {
    let mut config = AiChatConfig::default();
    config.protocols.tool = "不要调用工具，只能文字回复".into();
    let message = config.validate().unwrap_err().to_string();
    assert!(
        message.contains("ai_chat.protocols.tool"),
        "message={message}"
    );
}

#[test]
fn prompt_placeholder_restrictions() {
    let mut config = AiChatConfig::default();
    config.protocols.memory = "含 {{previous}} 占位符".into();
    assert!(config.validate().is_err(), "协议块不支持占位符");
    config.protocols.memory = String::new();
    config.summary_prompt = "旧摘要：{{previous}}\n新增：{{records}}".into();
    assert!(config.validate().is_ok(), "summary_prompt 允许占位符");
}

#[test]
fn custom_summary_prompt_substitutes_placeholders() {
    let mut config = AiChatConfig::default();
    config.summary_prompt = "旧：{{previous}}\n新：{{records}}".into();
    let prompt = summary_prompt(&config, "", "记录A");
    assert_eq!(prompt, "旧：（暂无）\n新：记录A");
    let prompt = summary_prompt(&config, "旧摘要", "记录B");
    assert_eq!(prompt, "旧：旧摘要\n新：记录B");
}

#[test]
fn append_mode_expands_both_placeholders() {
    let config = AiReplyConfig::default();
    let prompt = config.render_user_prompt("@QQ点歌", "小明点播的《晴天》已排上");
    assert!(prompt.contains("@QQ点歌"));
    assert!(prompt.contains("小明点播的《晴天》已排上"));
    assert!(!prompt.contains("{{"), "占位符必须全部展开: {prompt}");
}

#[test]
fn disabled_context_leaves_the_placeholder_empty() {
    let config = AiReplyConfig {
        include_command: false,
        ..AiReplyConfig::default()
    };
    let prompt = config.render_user_prompt("@QQ点歌", "回复正文");
    assert!(!prompt.contains("@QQ点歌"));
    assert!(prompt.contains("回复正文"));
}

#[test]
fn rewrite_mode_keeps_template_even_when_disabled() {
    // 改写模式没有原始信息就只能编，而这条回复会直接发到大厅。
    let config = AiReplyConfig {
        mode: AiReplyMode::Rewrite,
        include_template: false,
        ..AiReplyConfig::default()
    };
    let prompt = config.render_user_prompt("@状态", "小明点播的《晴天》");
    assert!(prompt.contains("小明点播的《晴天》"));
}

#[test]
fn defaults_are_append_with_both_contexts_on() {
    let config = AiReplyConfig::default();
    assert_eq!(config.mode, AiReplyMode::Append);
    assert!(config.include_template);
    assert!(config.include_command);
    assert!(config.timeout_seconds >= 1);
    assert!(config.max_tokens > 0);
}
