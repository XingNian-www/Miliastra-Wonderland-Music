//! 管理与邀请类回复文案。key 一旦发布不要改名。

use super::ReplyTemplate;

/// 管理（拉黑/屏蔽）与邀请回复。
pub(crate) const MODERATION: &[ReplyTemplate] = &[
    ReplyTemplate {
        key: "moderation.request.pending",
        default: "@UID{UID}的{动作}请求正在处理中",
        summary: "同一 UID 已有处理中的请求",
        group: "管理",
    },
    ReplyTemplate {
        key: "moderation.vote.opened",
        default: "管理员发起了对@UID{UID}的{动作}请求,请好友{超时秒数}s内使用@同意/不同意进行判决",
        summary: "发起管理投票",
        group: "管理",
    },
    ReplyTemplate {
        key: "invite.accepted",
        default: "已同意加入大厅,请等待BOT进入大厅并发送就绪信息后再开启麦克风",
        summary: "好友同意加入大厅",
        group: "管理",
    },
    ReplyTemplate {
        key: "workflow.confirm.timeout",
        default: "自定义流程确认超时,已取消",
        summary: "自定义流程确认超时",
        group: "管理",
    },
    ReplyTemplate {
        key: "invite.sent",
        default: "{玩家}邀请BOT前往大厅,30s内@邀请确认@邀请拒绝,默认通过",
        summary: "邀请 BOT 进入大厅",
        group: "管理",
    },
];
