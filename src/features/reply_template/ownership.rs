//! 回复模板 key 到命令的归属表。
//!
//! 设计约束：
//! - 每条模板 key 必须且只能归属一个命令，否则命令面板会出现同一条文案被两个命令同时编辑
//!   的歧义，或用户改了 A 命令却影响 B 命令。
//! - 归属只决定这条文案在面板的哪个命令下编辑，不改变发送路径；一个命令的多个分支回复仍各自独立。
//! - 共享文案（例如牌类用 玩法 变量同时服务斗地主与跑得快）挂在主命令下，
//!   面板会提示该文案同时影响另一个玩法。
//! - key 一旦发布不要改名：改名等同于删除用户已保存的覆盖。

/// 一条命令下的可编辑回复集合。
#[derive(Clone, Copy, Debug)]
pub(crate) struct CommandReplyGroup {
    /// 稳定命令 ID，与命令面板的 前缀+命令名 一致。
    pub(crate) command_id: &'static str,
    /// 面板显示名。
    pub(crate) name: &'static str,
    /// 面板分组，取 CommandGroup::label() 的取值。
    pub(crate) category: &'static str,
    /// 该命令拥有的模板 key，顺序即面板展示顺序。
    pub(crate) keys: &'static [&'static str],
}

/// 全部归属分组。新增模板时必须同时登记到这里，完整性测试会拦住遗漏。
pub(crate) const REPLY_GROUPS: &[CommandReplyGroup] = &[
    CommandReplyGroup {
        command_id: "@QQ点歌",
        name: "点歌",
        category: "点歌",
        keys: &[
            "playback.dedup.recently_played",
            "playback.dedup.skipped",
            "playback.no_source",
            "playback.search.failed",
            "playback.item_failure",
            "playback.failure.reason",
            "playback.source_switched",
            "playback.queue.skipped",
            "song.platform.denied",
            "song.queue.duplicate",
            "song.playing.already",
            "song.ai.disabled",
            "song.ai.disabled_with_hint",
            "song.ai.matching",
            "song.ai.rewritten",
            "song.ai.second_search_failed",
            "song.no_source",
            "song.ai.recognize_failed",
            "song.search_failed",
            "song.queue.added",
            "song.queue.full",
            "song.queue.full_unknown_state",
            "song.dedup.recently_played",
            "song.review.rejected",
        ],
    },
    CommandReplyGroup {
        command_id: "@歌单",
        name: "歌单",
        category: "点歌",
        keys: &[
            "song.playlist.friend_only",
            // 窗口推荐（@歌单 打开后的一批候选）与选曲回执。
            "playlist.window.header",
            "playlist.window.item",
            "playlist.window.footer",
            "playlist.select.queued",
            "playlist.select.queue_full",
            "playlist.select.out_of_range",
            // @换一批 与 @关闭 都作用在同一个窗口上。
            "playlist.reroll.exhausted",
            "playlist.window.closed",
            // 收藏 / 取消收藏 / 随机推荐。
            "playlist.favorite.no_track",
            "playlist.favorite.added",
            "playlist.favorite.exists",
            "playlist.unfavorite.no_track",
            "playlist.unfavorite.removed",
            "playlist.unfavorite.missing",
            "playlist.random.picked",
            "playlist.empty.hint",
            "playlist.empty.plain",
            // 歌单模式开关。
            "playlist.play.empty",
            "playlist.play.started",
            "playlist.play.occupied",
            "playlist.mode.exited",
            "playlist.mode.already_hall",
            // 导入：发起 / 确认 / 取消与异步完成回调。
            "playlist.import.pending",
            "playlist.import.busy",
            "playlist.import.loading",
            "playlist.import.still_loading",
            "playlist.import.done",
            "playlist.import.timeout",
            "playlist.import.failed",
            "playlist.import.too_large",
            "playlist.import.cancelled",
            // 清空：二次确认。
            "playlist.clear.already_empty",
            "playlist.clear.confirm",
            "playlist.clear.done",
            "playlist.clear.done_with_exit",
            "playlist.clear.aborted",
            // 入口通用错误。
            "playlist.actor.missing",
        ],
    },
    CommandReplyGroup {
        command_id: "@歌单帮助",
        name: "歌单帮助",
        category: "点歌",
        keys: &[
            "help.playlist",
        ],
    },
    CommandReplyGroup {
        command_id: "@暂停",
        name: "暂停",
        category: "播放",
        keys: &[
            "playback.pause.ok",
        ],
    },
    CommandReplyGroup {
        command_id: "@继续",
        name: "继续 / 播放",
        category: "播放",
        keys: &[
            "playback.resume.ok",
        ],
    },
    CommandReplyGroup {
        command_id: "@下一首",
        name: "下一首",
        category: "播放",
        keys: &[
            "playback.next.pool_fallback",
            "playback.next.empty",
        ],
    },
    CommandReplyGroup {
        command_id: "@上一首",
        name: "上一首",
        category: "播放",
        keys: &[
            "playback.previous.missing",
        ],
    },
    CommandReplyGroup {
        command_id: "@删除",
        name: "删除当前曲目",
        category: "播放",
        keys: &[
            "playback.delete.track_unavailable",
            "playback.delete.removed.pool_fallback",
            "playback.delete.not_in_pool.pool_fallback",
            "playback.delete.removed.queue_empty",
            "playback.delete.not_in_pool.queue_empty",
        ],
    },
    CommandReplyGroup {
        command_id: "@音量",
        name: "音量",
        category: "播放",
        keys: &[
            "playback.volume.set",
        ],
    },
    CommandReplyGroup {
        command_id: "@队列",
        name: "队列",
        category: "播放",
        keys: &[
            "playback.queue.empty",
            "playback.queue.header",
            "playback.queue.entry",
        ],
    },
    CommandReplyGroup {
        command_id: "@队列删除",
        name: "队列删除",
        category: "播放",
        keys: &[
            "playback.queue.delete.invalid",
            "playback.queue.delete.missing",
            "playback.queue.delete.done",
        ],
    },
    CommandReplyGroup {
        command_id: "@队列清空",
        name: "队列清空",
        category: "播放",
        keys: &[
            "playback.queue.cleared",
        ],
    },
    CommandReplyGroup {
        command_id: "@歌词",
        name: "歌词",
        category: "播放",
        keys: &[
            "playback.lyrics.toggle.translation",
            "playback.lyrics.toggle.original",
            "playback.lyrics.timed_started",
            "playback.lyrics.already_running",
            // @歌词 本身的输出（有歌词 / 无歌词两种形态）。
            "playback.lyrics.line",
            "playback.lyrics.empty",
        ],
    },
    CommandReplyGroup {
        command_id: "@状态",
        name: "播放状态",
        category: "播放",
        // @状态 是运行时拼装的动态行，拆成主模板 + 可选片段：
        // 有歌 / 无歌两张排版，外加点歌人与歌单前缀两个可留空的片段。
        keys: &[
            "playback.status.with_song",
            "playback.status.without_song",
            "playback.status.requester",
            "playback.status.playlist_prefix",
        ],
    },
    CommandReplyGroup {
        command_id: "@后台歌词",
        name: "后台歌词",
        category: "播放",
        keys: &[
            "playback.lyrics.background.started",
        ],
    },
    CommandReplyGroup {
        command_id: "@单曲歌词",
        name: "单曲歌词",
        category: "播放",
        keys: &[
            "playback.lyrics.single_song.started",
        ],
    },
    CommandReplyGroup {
        command_id: "@停止歌词",
        name: "停止歌词",
        category: "播放",
        keys: &[
            "playback.lyrics.background.stopped",
            "playback.lyrics.background.not_running",
        ],
    },
    CommandReplyGroup {
        command_id: "@大厅检测",
        name: "大厅检测",
        category: "大厅",
        keys: &[
            "hall.detect.failed",
            "hall.public.current",
            "hall.current",
        ],
    },
    CommandReplyGroup {
        command_id: "@大厅时间",
        name: "大厅时间",
        category: "大厅",
        keys: &[
            "hall.time.remaining",
            "hall.time.unknown",
            "hall.public.no_limit",
            "hall.expiring.warning",
            "hall.expiring.warning_minutes",
        ],
    },
    CommandReplyGroup {
        command_id: "@麦克风",
        name: "麦克风",
        category: "大厅",
        keys: &[
            "hall.microphone.toggled",
        ],
    },
    CommandReplyGroup {
        command_id: "#塔罗牌",
        name: "塔罗牌",
        category: "娱乐",
        keys: &[
            "tarot.question.too_long",
            "tarot.actor.missing",
            "tarot.cooldown",
            "tarot.busy",
            "tarot.spread.title",
            "tarot.ai.disabled",
            "tarot.ai.unavailable",
        ],
    },
    CommandReplyGroup {
        command_id: "#塔罗牌帮助",
        name: "塔罗牌帮助",
        category: "娱乐",
        keys: &[
            "tarot.help",
        ],
    },
    CommandReplyGroup {
        command_id: "#接龙",
        name: "成语接龙",
        category: "娱乐",
        keys: &[
            "idiom_chain.occupied.turtle_soup",
            "idiom_chain.occupied.other",
            "idiom_chain.disabled",
            "idiom_chain.expired",
            "idiom_chain.already_active",
            "idiom_chain.no_start_candidate",
            "idiom_chain.dead_end_start",
            "idiom_chain.expected.homophone",
            "idiom_chain.expected.exact",
            "idiom_chain.started",
            "idiom_chain.no_session.start",
            "idiom_chain.wrong_first_char",
            "idiom_chain.duplicate",
            "idiom_chain.completed",
            "idiom_chain.accepted",
            "idiom_chain.no_session",
            "idiom_chain.status",
            "idiom_chain.no_explanation_target",
            "idiom_chain.missing_explanation",
            "idiom_chain.explanation.title",
            "idiom_chain.no_hint",
            "idiom_chain.hint",
            "idiom_chain.stop_denied",
            "idiom_chain.stopped",
            "idiom_chain.invalid_idiom",
            "idiom_chain.not_in_lexicon",
        ],
    },
    CommandReplyGroup {
        command_id: "#斗地主",
        name: "斗地主",
        category: "娱乐",
        keys: &[
            "card_games.disabled",
            "card_games.already_active",
            "card_games.lobby.created",
            "card_games.lobby.none",
            "card_games.lobby.already_joined",
            "card_games.lobby.joined",
            "card_games.bidding.started",
            "card_games.retry.none",
            "card_games.bidding.not_bidding",
            "card_games.game.not_in_landlord",
            "card_games.bidding.turn",
            "card_games.bidding.decided",
            "card_games.bidding.redealt",
            "card_games.hand.initial",
            "card_games.status.player_count",
            "card_games.hand.landlord_initial",
            "card_games.landlord.selected",
            "card_games.lobby.timeout",
            "card_games.bidding.warning",
            "card_games.bidding.timeout",
            "card_games.turn.warning",
            "card_games.pass.timeout_no_beating",
            "card_games.pass.timeout",
            "card_games.pass.timeout_with_suffix",
            "card_games.game.no_active",
            "card_games.game.not_in",
            "card_games.play.wrong_turn",
            "card_games.play.empty",
            "card_games.play.invalid_pattern",
            "card_games.play.cannot_beat",
            "card_games.pass.leader_cannot_pass",
            "card_games.hand.no_dealt_game",
            "card_games.hand.current",
            "card_games.status.idle",
            "card_games.status.lobby",
            "card_games.status.bidding",
            "card_games.status.playing_landlord",
            "card_games.exit.not_in_lobby",
            "card_games.lobby.canceled",
            "card_games.lobby.left",
            "card_games.exit.landlord_ended",
            "card_games.exit.game_ended",
            "card_games.won.landlord",
            "card_games.action.play",
            "card_games.action.timeout",
            "card_games.play.landlord",
            "card_games.pass.manual_next",
            "card_games.pass.manual_lead",
            "card_games.pass.timeout_next",
            "card_games.pass.timeout_lead",
            "card_games.pass.forced_next",
            "card_games.pass.forced_lead",
            "card_games.pass.new_trick",
            "card_games.pass.next",
            "card_games.parse.unknown_card",
            "card_games.parse.missing_card",
        ],
    },
    CommandReplyGroup {
        command_id: "#跑得快",
        name: "跑得快",
        category: "娱乐",
        keys: &[
            "card_games.run_fast.started",
            "card_games.play.spade_three_required",
            "card_games.play.must_play_largest_single",
            "card_games.pass.must_play",
            "card_games.status.playing_run_fast",
            "card_games.won.run_fast",
            "card_games.play.run_fast",
        ],
    },
    CommandReplyGroup {
        command_id: "#海龟汤",
        name: "海龟汤",
        category: "娱乐",
        keys: &[
            "turtle_soup.judgment.suffix",
            "turtle_soup.judgment.reply",
            "turtle_soup.result.yes",
            "turtle_soup.result.no",
            "turtle_soup.result.irrelevant",
            "turtle_soup.result.partial",
            "turtle_soup.result.complete",
            "turtle_soup.result.refused",
            "turtle_soup.result.review_failed",
            "turtle_soup.batch.notice",
            "turtle_soup.batch.saved",
            "turtle_soup.batch.missing",
            "turtle_soup.batch.limit",
            "turtle_soup.batch.empty_part",
            "turtle_soup.batch.no_draft",
            "turtle_soup.batch.format",
            "turtle_soup.batch.cleared",
            "turtle_soup.batch.done",
            "turtle_soup.batch.submit",
            "turtle_soup.batch.format_hint",
            "turtle_soup.question.pending",
            "turtle_soup.question.too_many",
            "turtle_soup.question.worker_unavailable",
            "turtle_soup.disabled",
            "turtle_soup.no_session",
            "turtle_soup.status.no_session",
            "turtle_soup.status",
            "turtle_soup.end.hall_denied",
            "turtle_soup.end.no_session",
            "turtle_soup.end.failed",
            "turtle_soup.start.failed",
            "turtle_soup.announcing",
            "turtle_soup.settling",
            "turtle_soup.surface.repeat_failed",
            "turtle_soup.label.surface",
            "turtle_soup.label.settlement",
            "turtle_soup.label.bottom",
            "turtle_soup.reason.winner",
            "turtle_soup.reason.friend",
            "turtle_soup.reason.web",
            "turtle_soup.reason.idle_timeout",
            "turtle_soup.reason.max_duration",
            "turtle_soup.settlement.summary",
        ],
    },
    CommandReplyGroup {
        command_id: "#卧底",
        name: "谁是卧底",
        category: "娱乐",
        keys: &[
            "undercover.vote.instruction",
            "undercover.start.secret",
            "undercover.start.header",
            "undercover.start.prompt",
            "undercover.delivery.canceled",
            "undercover.runoff.no_voters",
            "undercover.runoff.described",
            "undercover.describe.completed",
            "undercover.status.idle",
            "undercover.status.lobby",
            "undercover.status.playing",
            "undercover.phase.describing",
            "undercover.phase.voting",
            "undercover.reveal.answer",
            "undercover.exit.creator",
            "undercover.exit.lobby",
            "undercover.exit.playing",
            "undercover.round.restart",
            "undercover.lobby.canceled",
            "undercover.lobby.timeout",
            "undercover.delivery.timeout",
            "undercover.vote.missing",
            "undercover.describe.timeout",
            "undercover.runoff.describe_timeout",
            "undercover.runoff.describe_cutoff",
            "undercover.describe.cutoff",
            "undercover.runoff.tied",
            "undercover.runoff.enter",
            "undercover.vote.none",
            "undercover.vote.eliminated",
            "undercover.round.start",
            "undercover.settlement.header",
            "undercover.settlement.player",
            "undercover.settlement.rounds",
            "undercover.start.position_mapping",
            "undercover.winner.civilian",
            "undercover.winner.undercover",
            "undercover.winner.none",
            "undercover.role.civilian",
            "undercover.role.undercover",
            "undercover.delivery.waiting",
            "undercover.verify.create_success",
            "undercover.verify.join_success",
            "undercover.verify.failed",
            "undercover.error.no_lobby",
            "undercover.error.already_started",
            "undercover.error.already_joined",
            "undercover.error.disabled",
            "undercover.error.already_active",
            "undercover.error.occupied",
            "undercover.reveal.friend_only",
            "undercover.retry.none",
            "undercover.retry.expired",
            "undercover.retry.unsafe",
            "undercover.delivery.result_unknown",
            "undercover.delivery.no_safe_remainder",
            "undercover.delivery.incomplete",
        ],
    },
    CommandReplyGroup {
        command_id: "@帮助",
        name: "帮助",
        category: "管理",
        keys: &[
            "help.song_request",
        ],
    },
    CommandReplyGroup {
        command_id: "@娱乐帮助",
        name: "娱乐帮助",
        category: "管理",
        keys: &[
            "help.entertainment",
        ],
    },
    CommandReplyGroup {
        command_id: "@启用",
        name: "启用命令识别",
        category: "管理",
        keys: &["admin.commands.enabled"],
    },
    CommandReplyGroup {
        command_id: "@禁用",
        name: "禁用命令识别",
        category: "管理",
        keys: &["admin.commands.disabled"],
    },
    CommandReplyGroup {
        command_id: "@忘记我",
        name: "AI 聊天",
        category: "管理",
        keys: &[
            "ai_chat.forget.done",
            "ai_chat.question.too_long",
            "ai_chat.unavailable",
        ],
    },
    CommandReplyGroup {
        command_id: "@拉黑",
        name: "拉黑 / 屏蔽",
        category: "管理",
        keys: &[
            "moderation.request.pending",
            "moderation.vote.opened",
        ],
    },
    CommandReplyGroup {
        command_id: "@邀请",
        name: "邀请",
        category: "管理",
        keys: &[
            "invite.accepted",
            "invite.sent",
        ],
    },
    CommandReplyGroup {
        command_id: "custom_workflow",
        name: "自定义流程",
        category: "管理",
        keys: &[
            "workflow.confirm.timeout",
        ],
    },
];

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
pub(crate) fn group_for_key(key: &str) -> Option<&'static CommandReplyGroup> {
    REPLY_GROUPS
        .iter()
        .find(|group| group.keys.contains(&key))
}

/// 一条命令拥有的全部 key。
pub(crate) fn group_for_command(command_id: &str) -> Option<&'static CommandReplyGroup> {
    REPLY_GROUPS
        .iter()
        .find(|group| group.command_id == command_id)
}

