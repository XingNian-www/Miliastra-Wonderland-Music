//! 工具 schema、参数校验与工具提示词注入。

use super::support::*;
use super::*;

#[test]
fn native_tool_schema_and_arguments_keep_hash_commands_separate_from_display() {
    let tools = command_tools(&AiChatConfig::default());
    assert_eq!(tools[0]["function"]["name"], COMMAND_TOOL);
    assert_eq!(
        tools[0]["function"]["parameters"]["additionalProperties"],
        false
    );
    // 工具 schema 属于稳定前缀：不得内嵌任何具体玩家的命令列表。
    let description = tools[0]["function"]["description"]
        .as_str()
        .expect("description");
    assert!(!description.contains("@点歌"));
    assert!(!description.contains("#提示"));
    assert_eq!(
        serde_json::to_string(&command_tools(&AiChatConfig::default())).unwrap(),
        serde_json::to_string(&tools).unwrap(),
        "同一配置下工具 schema 必须逐字节稳定"
    );
    assert_eq!(
        parse_execution(r##"{"command":"#提示"}"##).unwrap().command,
        "#提示"
    );
    assert!(parse_execution("我来点歌\n@点歌晴天").is_err());
    assert!(parse_execution(r#"{"reply":"@点歌晴天","commands":[]}"#).is_err());
    let valid = serde_json::json!({"command":format!("@{}", "字".repeat(39))}).to_string();
    assert!(parse_execution(&valid).is_ok());
}

#[test]
fn tool_arguments_reject_invalid_fields_and_multiline_commands() {
    for raw in [
        r#"{}"#,
        r#"{"command":null}"#,
        r#"{"command":"@队列","actor":"管理员"}"#,
        r#"{"command":"@队列","command":"@暂停"}"#,
        r#"{"command":"@队列\n@暂停"}"#,
        r#"{"command":"@队列\t"}"#,
        r#"{"command":"@队列\u2028@暂停"}"#,
        r#"{"command":"@队列\u2029@暂停"}"#,
        r#"{"command":"点歌晴天"}"#,
        r#"{"command":"@"}"#,
    ] {
        assert!(parse_execution(raw).is_err(), "accepted: {raw}");
    }
    let raw = serde_json::json!({"command":format!("@{}", "字".repeat(40))}).to_string();
    assert!(parse_execution(&raw).is_err());
}

#[test]
fn tool_prompts_are_injected_only_for_exposed_tools_and_can_be_overridden() {
    let mut config = config();
    let none = response_system_prompt_with_tools(&config, false, false, false);
    assert!(!none.contains("look："));
    assert!(!none.contains("speak："));
    assert!(!none.contains("execute_game_command："));

    let look = response_system_prompt_with_tools(&config, true, false, false);
    assert!(look.contains("look："));
    assert!(look.contains("文字转述"));
    assert!(look.contains("本轮的用户问题会一并传给视觉子模型"));
    assert!(look.contains("direct_to_main"));
    assert!(!look.contains("没有直接看到原图"));
    assert!(!look.contains("不会直接看到原图"));
    assert!(!look.contains("speak："));
    assert!(!look.contains("execute_game_command："));

    let speak = response_system_prompt_with_tools(&config, false, true, false);
    assert!(speak.contains("speak："));
    assert!(speak.contains("queued"));
    assert!(!speak.contains("look："));

    let command = response_system_prompt_with_tools(&config, false, false, true);
    assert!(command.contains("execute_game_command："));
    assert!(command.contains("replies"));
    assert!(command.contains("相同命令不重复"));

    config.tool_prompts.look = "CUSTOM_LOOK".to_string();
    config.tool_prompts.speak = String::new(); // 空值回退内置默认
    let custom = response_system_prompt_with_tools(&config, true, true, false);
    assert!(custom.contains("CUSTOM_LOOK"));
    assert!(!custom.contains("look：查看当前画面"));
    assert!(custom.contains("speak："));
    assert!(custom.contains("queued"));

    // 单个 speak 字段的默认文案同时包含两套，由用户按实际模型删掉另一套。
    let default_speak = default_speak_tool_prompt();
    assert!(default_speak.contains("根据实际 tts.model 只保留对应一套，删掉另一套即可"));
    assert!(default_speak.contains("【S2.1 / S2.1-free / S2-pro】"));
    assert!(default_speak.contains("每句最多建议 3 个"));
    assert!(default_speak.contains("【Drama-3】"));
    assert!(default_speak.contains("[small laugh]"));
    assert!(default_speak.contains("<whisper>…</whisper>"));
    assert!(default_speak.contains("多说话人用 <|speaker:N|>"));
    assert!(default_speak.contains("queued 只表示排队"));
    let rendered = response_system_prompt_with_tools(&config, false, true, false);
    assert!(rendered.contains("【S2.1 / S2.1-free / S2-pro】"));
    assert!(rendered.contains("【Drama-3】"));

    // 用户删掉 Drama 一套后，只有自己保留的 S2.1 文案。
    config.tool_prompts.speak = "CUSTOM_S2_ONLY".to_string();
    let custom = response_system_prompt_with_tools(&config, false, true, false);
    assert!(custom.contains("CUSTOM_S2_ONLY"));
    assert!(!custom.contains("【Drama-3】"));
    assert!(!custom.contains("【S2.1 / S2.1-free / S2-pro】"));
}

#[test]
fn missing_tool_prompts_fall_back_to_built_in_defaults() {
    let mut value = serde_json::to_value(AiChatConfig::default()).unwrap();
    value.as_object_mut().unwrap().remove("tool_prompts");
    let parsed: AiChatConfig = serde_json::from_value(value).unwrap();
    assert_eq!(parsed.tool_prompts.look, default_look_tool_prompt());
    assert_eq!(parsed.tool_prompts.speak, default_speak_tool_prompt());
    assert_eq!(parsed.tool_prompts.command, default_command_tool_prompt());
    let saved = serde_json::to_value(&parsed).unwrap();
    assert!(saved["tool_prompts"]["look"].is_string());
}

#[test]
fn tool_descriptions_are_configurable() {
    let mut config = AiChatConfig::default();
    config.tool_descriptions.look = "自定义 look 描述".into();
    config.tool_descriptions.command = "自定义命令描述".into();
    config.tool_descriptions.speak = "自定义 speak 前缀".into();
    let look = look_tool(&config);
    assert_eq!(look["function"]["description"], "自定义 look 描述");
    let command = command_tools(&config);
    assert_eq!(command[0]["function"]["description"], "自定义命令描述");
    let speak = speak_tool_for_model("s1", 120, &config);
    let description = speak["function"]["description"].as_str().unwrap();
    assert!(
        description.starts_with("自定义 speak 前缀"),
        "{description}"
    );
}

#[test]
fn speak_tool_schema_varies_by_model_capabilities() {
    let s1 = speak_tool_for_model("s1", 120, &AiChatConfig::default());
    let s1_props = s1["function"]["parameters"]["properties"]
        .as_object()
        .unwrap();
    assert!(s1_props.contains_key("emotion"));
    assert!(!s1_props.contains_key("style"));
    assert!(s1_props.contains_key("speed"));
    assert!(s1_props.contains_key("volume_db"));

    let s2 = speak_tool_for_model("s2.1-pro", 120, &AiChatConfig::default());
    let s2_props = s2["function"]["parameters"]["properties"]
        .as_object()
        .unwrap();
    assert!(s2_props.contains_key("emotion"));
    assert!(s2_props.contains_key("style"));
    assert_eq!(s2_props["speed"]["maximum"].as_f64().unwrap(), 2.0);
    let s2_text = s2_props["text"]["description"].as_str().unwrap();
    assert!(s2_text.contains("whispers sweetly"), "S2 text 描述缺少示例");
    assert!(
        s2["function"]["description"]
            .as_str()
            .unwrap()
            .contains("laughing nervously"),
        "S2 工具描述缺少示例"
    );

    let drama = speak_tool_for_model("drama-3", 120, &AiChatConfig::default());
    let drama_props = drama["function"]["parameters"]["properties"]
        .as_object()
        .unwrap();
    let drama_text = drama_props["text"]["description"].as_str().unwrap();
    for needle in ["<whisper>", "导演指示"] {
        assert!(drama_text.contains(needle), "Drama text 描述缺少 {needle}");
    }

    let unknown = speak_tool_for_model("unknown-model", 120, &AiChatConfig::default());
    let unknown_props = unknown["function"]["parameters"]["properties"]
        .as_object()
        .unwrap();
    assert_eq!(unknown_props.len(), 1, "未知模型只暴露 text");
    assert!(unknown_props.contains_key("text"));
}

#[test]
fn speak_parser_rejects_unsupported_or_out_of_range_options() {
    assert!(parse_speak_arguments_for_model(r#"{"text":"你好"}"#, "s1").is_ok());
    assert!(parse_speak_arguments_for_model(r#"{"text":"你好","style":"温柔"}"#, "s1").is_err());
    assert!(parse_speak_arguments_for_model(r#"{"text":"你好","speed":2.5}"#, "s2.1-pro").is_err());
    assert!(
        parse_speak_arguments_for_model(r#"{"text":"你好","volume_db":21}"#, "s2.1-pro").is_err()
    );
    assert!(
        parse_speak_arguments_for_model(r#"{"text":"你好","emotion":"laugh"}"#, "unknown-model")
            .is_err()
    );
    let (text, options) = parse_speak_arguments_for_model(
        r#"{"text":"你好","emotion":"laugh","style":"温柔","speed":1.2,"volume_db":-2}"#,
        "s2.1-pro",
    )
    .unwrap();
    assert_eq!(text, "你好");
    assert_eq!(options.emotion.as_deref(), Some("laugh"));
    assert_eq!(options.style.as_deref(), Some("温柔"));
    assert_eq!(options.speed, Some(1.2));
    assert_eq!(options.volume_db, Some(-2.0));
}

#[test]
fn look_tool_schema_and_parser_are_optional_instruction() {
    let tool = look_tool(&AiChatConfig::default());
    assert_eq!(tool["function"]["name"], LOOK_TOOL);
    assert_eq!(tool["function"]["strict"], false);
    assert_eq!(
        tool["function"]["parameters"]["properties"]["instruction"]["maxLength"],
        200
    );
    let description = tool["function"]["description"].as_str().unwrap();
    assert!(description.contains("本轮问题会一并传给视觉子模型"));
    assert!(description.contains("direct_to_main"));
    assert!(!description.contains("没有直接看到原图"));
    assert!(!description.contains("不会直接看到原图"));
    assert_eq!(parse_look_arguments("{}").unwrap(), None);
    assert_eq!(
        parse_look_arguments(r#"{"instruction":"看聊天区"}"#).unwrap(),
        Some("看聊天区".to_string())
    );
    assert!(parse_look_arguments(r#"{"instruction":"  "}"#).is_err());
    assert!(parse_look_arguments(r#"{"extra":1}"#).is_err());
    assert!(parse_look_arguments(r#"{"instruction":"a\nb"}"#).is_err());
    let mut seen = std::collections::HashSet::new();
    assert!(look_quota_allows(0, &seen, "a"));
    seen.insert("a".to_string());
    assert!(!look_quota_allows(1, &seen, "a"));
    assert!(look_quota_allows(1, &seen, "b"));
    assert!(!look_quota_allows(2, &seen, "c"));
}

#[test]
fn speak_tool_schema_and_argument_parser_are_strict() {
    let tool = speak_tool_for_model("", 120, &AiChatConfig::default());
    assert_eq!(tool["function"]["name"], SPEAK_TOOL);
    assert_eq!(
        tool["function"]["parameters"]["properties"]["text"]["maxLength"],
        120
    );
    assert_eq!(
        tool["function"]["strict"], false,
        "可选字段不能配 strict:true"
    );
    let required = tool["function"]["parameters"]["required"]
        .as_array()
        .unwrap();
    assert_eq!(required, &vec![serde_json::json!("text")]);
    let description = tool["function"]["description"].as_str().unwrap_or_default();
    for needle in ["让 BOT 用语音说出文本", "Markdown/URL", "示例"] {
        assert!(description.contains(needle), "工具描述缺少约束：{needle}");
    }
    assert!(
        !description.contains("仅在"),
        "工具描述不应规定何时调用；交给用户 system_prompt"
    );
    assert_eq!(parse_speak_arguments(r#"{"text":"你好"}"#).unwrap(), "你好");
    assert!(parse_speak_arguments(r#"{"text":"  "}"#).is_err());
    assert!(parse_speak_arguments(r#"{"text":"a\nb"}"#).is_err());
    assert!(parse_speak_arguments("not json").is_err());
}
