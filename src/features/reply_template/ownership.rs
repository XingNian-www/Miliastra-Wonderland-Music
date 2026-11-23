//! 回复模板 key 到命令的归属表。
//!
//! 设计约束：
//! - 每条模板 key 必须且只能归属一个命令，否则命令面板会出现同一条文案被两个命令同时编辑
//!   的歧义，或用户改了 A 命令却影响 B 命令。
//! - 归属只决定这条文案在面板的哪个命令下编辑，不改变发送路径；一个命令的多个分支回复仍各自独立。
//! - 共享文案（例如牌类用 玩法 变量同时服务斗地主与跑得快）挂在主命令下，
//!   面板会提示该文案同时影响另一个玩法。
//! - key 一旦发布不要改名：改名等同于删除用户已保存的覆盖。

/// 一条命令下的可编辑回复集合（面板视图）。
///
/// 与登记表里的 [`ReplyGroupSpec`] 字段相同，但**不含 owner**：面板只关心
/// "这条命令有哪些文案可编辑"，不关心它归哪个枚举变体。
pub(crate) use crate::features::command::CommandReplyGroup;

/// 全部归属分组（按面板展示顺序）。由 `features::command` 的登记表派生。
pub(crate) use crate::features::command::REPLY_GROUPS;

/// 与已有分组共用回复的命令别名。
///
/// 这些命令走的是同一条执行分支、发同一批文案（例如 @播放 与 @继续 都由
/// PlaybackCommand::Resume 处理），所以不另开分组。映射只影响面板：
/// 选中别名时展示它所属分组的回复，避免出现"这个命令没有可编辑回复"。
pub(crate) const COMMAND_ID_ALIASES: &[(&'static str, &'static str)] = &[
    // 播放：同一分支。
    ("@播放", "@继续"),
    // 点歌：各平台共用 song.* 文案。
    ("@网易点歌", "@QQ点歌"),
    ("@酷狗点歌", "@QQ点歌"),
    ("@B站点歌", "@QQ点歌"),
    ("@AI点歌", "@QQ点歌"),
    // 歌单：别名与主命令同一分支。
    ("@歌单随机", "@歌单"),
    ("@收藏", "@歌单"),
    ("@取消收藏", "@歌单"),
    ("@换一批", "@歌单"),
    ("@关闭", "@歌单"),
    ("@大厅模式", "@歌单"),
    ("@播放歌单", "@歌单"),
    ("@停止播放歌单", "@歌单"),
    ("@歌单清空", "@歌单"),
    ("@确认清空", "@歌单"),
    ("@放弃清空", "@歌单"),
    ("@导入QQ歌单", "@歌单"),
    ("@确认导入", "@歌单"),
    ("@取消导入", "@歌单"),
    // 歌词：同属 @歌词 分支的变体。
    ("@切换", "@歌词"),
    ("@持续歌词", "@歌词"),
    // 管理：@屏蔽 与 @拉黑 共用 moderation.*。
    ("@屏蔽", "@拉黑"),
    // 娱乐：重抽与同音接龙复用同一批文案。
    ("#塔罗牌重抽", "#塔罗牌"),
    ("#同音接龙", "#接龙"),
    ("#娱乐帮助", "@娱乐帮助"),
];

/// 把命令 ID 归一化为真正持有回复的分组 ID。
pub(crate) fn canonical_command_id(command_id: &str) -> &str {
    COMMAND_ID_ALIASES
        .iter()
        .find(|(alias, _)| *alias == command_id)
        .map(|(_, owner)| *owner)
        .unwrap_or(command_id)
}

/// 一条 key 的归属命令。
pub(crate) fn group_for_key(key: &str) -> Option<CommandReplyGroup> {
    REPLY_GROUPS
        .iter()
        .find(|group| group.keys.contains(&key))
        .map(|spec| CommandReplyGroup {
            command_id: spec.command_id,
            name: spec.name,
            category: spec.category,
            keys: spec.keys,
        })
}

/// 一条命令拥有的全部 key。
pub(crate) fn group_for_command(command_id: &str) -> Option<CommandReplyGroup> {
    REPLY_GROUPS
        .iter()
        .find(|group| group.command_id == command_id)
        .map(|spec| CommandReplyGroup {
            command_id: spec.command_id,
            name: spec.name,
            category: spec.category,
            keys: spec.keys,
        })
}
