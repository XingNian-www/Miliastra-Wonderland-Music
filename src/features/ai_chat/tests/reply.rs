//! 回复解析与清洗（动作/台词分离、残片丢弃）。

use super::*;

#[test]
fn answer_is_sanitized_of_markdown_and_control_characters() {
    // 星号是动作定界符，必须保留；反引号才是噪声。
    assert_eq!(sanitize_answer("**加粗** 与 `代码`"), "**加粗** 与 代码");
    assert_eq!(sanitize_answer("  正常回答\u{7}  "), "正常回答");
}

/// 动作定界符（星号）不能被清洗掉。
///
/// 剥掉后动作与台词粘成一句（`猫耳一抖哼，`），既难读，
/// 也让模型失去「这是独立片段」的线索。
#[test]
fn action_markers_survive_sanitizing() {
    assert_eq!(
        sanitize_answer("*猫耳一抖* 哼，才不是呢"),
        "*猫耳一抖* 哼，才不是呢"
    );
}

/// 回归：模型多返回一个字段时，不能整体解析失败。
///
/// 去掉 `deny_unknown_fields` 之前，模型多给一个 `emotion` 就会让两个分支
/// 全部失败，然后落到「整段当台词」，把 `{"action":…}` 原样发进游戏聊天。
#[test]
fn extra_fields_do_not_break_reply_parsing() {
    let reply = parse_reply(r#"{"action":"猫耳一抖","text":"哼","emotion":"shy"}"#);
    assert_eq!(reply.action, "猫耳一抖");
    assert_eq!(reply.text, "哼");
}

/// 回归：被截断的 JSON 残片必须丢弃，不能发到大厅。
#[test]
fn truncated_json_fragments_are_dropped_not_sent() {
    let reply = parse_reply(r#"{"action":"猫耳一抖","text":"哼，才不"#);
    assert!(
        reply.action.is_empty() && reply.text.is_empty(),
        "残片必须被丢弃"
    );
    let rendered = render_reply(&reply);
    assert!(rendered.is_empty());
}

/// 模型完全不按格式走时，整段当台词保住内容。
#[test]
fn plain_text_reply_is_kept_as_the_text_field() {
    let reply = parse_reply("哼，才不是特意等你的喵！");
    assert!(reply.action.is_empty());
    assert_eq!(reply.text, "哼，才不是特意等你的喵！");
}

/// 动作统一成 *…*，动作与台词用空格衔接（不是换行）。
#[test]
fn rendered_reply_keeps_the_action_marker_and_never_uses_newlines() {
    let reply = AiChatReply {
        action: "猫耳一抖".to_string(),
        text: "哼，才不是呢".to_string(),
    };
    let rendered = render_reply(&reply);
    assert_eq!(rendered, "*猫耳一抖* 哼，才不是呢");
    assert!(!rendered.contains('\n'), "换行会被下游折成空格，不要依赖它");
}

/// 动作里的换行要被压平，否则会被下游折成断句。
#[test]
fn action_newlines_are_flattened() {
    assert_eq!(sanitize_action("猫耳\n一抖"), "*猫耳 一抖*");
}
