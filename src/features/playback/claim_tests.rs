//! 播放命令认领的回归钉子（W4 尾项）。
//!
//! `claims_chat` 原先靠一张手写的 `PLAYBACK_COMMAND_PREFIXES`（21 条）判定，
//! 而 `command_catalog::PLAYBACK` 的 name+aliases 全集**逐字等价**于它（两个方向
//! 都无差集，已用脚本验证）。既然命令表是唯一事实来源，前缀表就是纯冗余——
//! 删掉它、改为从命令表派生。这两个测试锁住派生结果与删表前的行为一致。

use super::*;

fn envelope(message_type: &str, command: &str) -> CommandEnvelope {
    // `@` 前缀不能省：`CommandEnvelope::new` 按前缀分派，没有 `@`/`&`/`#`
    // 的文本会被当成 `#` 处理并因缺 `#` 直接返回 None。
    CommandEnvelope::new(
        format!("用户：@{command}"),
        "用户",
        message_type,
        format!("@{command}"),
        crate::features::command::CommandObservation::default(),
    )
    .expect("test envelope")
}

/// 「删除」只能由好友会话触发，大厅身份即使文本匹配也不得认领。
///
/// 这条门控原先靠"前缀表里把它 filter 掉"表达，是隐式的；改为从命令表派生后
/// 必须显式钉住，否则好友专用命令会泄漏给大厅。
#[test]
fn hidden_delete_command_is_friend_only() {
    let friend = envelope("pink", "删除");
    assert!(PlaybackCommand::claims_chat(&friend));
    let hall = envelope("blue", "删除");
    assert!(
        !PlaybackCommand::claims_chat(&hall),
        "「删除」是好友专用隐藏命令，大厅身份不得认领"
    );
}

/// 命令表里的每个名字都必须被认领，否则面板有、游戏里不认。
///
/// `claims_chat` 现在直接从 `command_catalog::PLAYBACK` 派生，这条测试锁住
/// "派生的确是同一份表"，而不是某个被手抄的子集。
#[test]
fn every_playback_name_is_claimed_and_raw_parses() {
    for spec in crate::features::command_catalog::PLAYBACK {
        for name in spec.all_names() {
            // 「删除」是好友专用，用大厅身份检查会得到（正确的）false。
            let authority = if name == PlaybackCommand::FRIEND_ONLY_PREFIX {
                "pink"
            } else {
                "blue"
            };
            let envelope = envelope(authority, name);
            assert!(
                PlaybackCommand::claims_chat(&envelope),
                "命令表登记的「{name}」未被认领"
            );
            // 「歌词」等必须带参数的命令裸写解析不出来是正常的，只要求
            // "认领了就能拿到 Some 或明确 None"，不能 panic。
            let _ = PlaybackCommand::parse_hall(name);
        }
    }
}
