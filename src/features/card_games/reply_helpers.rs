//! 牌局文案渲染的两个底层入口。

use crate::features::reply_template::ReplyVars;

use super::outcome::CardGameReplyTemplate;

/// 按内置文案渲染成品文本（不含用户覆盖）。
///
/// 仅用于把"内层片段"直接拼进更大字符串的场景与测试断言；任何要真正发送
/// 的回复都必须走 keyed_reply/Composed 保留模板 key，否则用户覆盖读不到。
pub(super) fn reply(
    spec: &crate::features::reply_template::ReplyTemplate,
    vars: ReplyVars,
) -> String {
    crate::features::reply_template::render(spec, None, &vars).text
}

pub(super) fn keyed_reply(
    spec: &'static crate::features::reply_template::ReplyTemplate,
    vars: ReplyVars,
) -> CardGameReplyTemplate {
    CardGameReplyTemplate::Template { spec, vars }
}
