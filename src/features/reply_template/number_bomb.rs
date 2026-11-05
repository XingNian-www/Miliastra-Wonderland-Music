//! 数字炸弹的默认回复文案。
//!
//! 所有文案都走 ReplyTemplate，玩家在命令面板里按命令统一覆盖/静音；
//! 这里只提供内置默认值，不在业务状态机里拼字符串。

use super::ReplyTemplate;

pub(crate) const NUMBER_BOMB: &[ReplyTemplate] = &[
    ReplyTemplate {
        key: "number_bomb.disabled",
        default: "数字炸弹未启用",
        summary: "玩法未启用",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.occupied",
        default: "「{其他玩法}」正在进行，等它结束后再开数字炸弹",
        summary: "其它娱乐玩法占用中",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.already_active",
        default: "数字炸弹已经在进行中；发送 #状态 查看当前范围",
        summary: "房间已存在",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.no_session",
        default: "当前没有进行中的数字炸弹，发送 #数字炸弹 开局",
        summary: "没有房间时的通用提示",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.only_creator",
        default: "只有发起人可以开始、重开或结束数字炸弹",
        summary: "非发起人的管理操作",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.lobby.created",
        default: "{玩家} 创建了数字炸弹房间，发送 #加入 参与；当前 {人数}/{上限} 人，至少 {最少人数} 人后由发起人发送 #开始",
        summary: "创建房间",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.lobby.already_joined",
        default: "{玩家} 已经在数字炸弹房间里了",
        summary: "重复加入",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.lobby.joined",
        default: "{玩家} 加入数字炸弹，当前 {人数}/{上限} 人",
        summary: "加入房间",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.lobby.full",
        default: "数字炸弹房间已满（{上限} 人）",
        summary: "房间已满",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.lobby.already_started",
        default: "数字炸弹已经开始，等本局结束后再 #加入",
        summary: "开局后加入",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.start.too_few",
        default: "数字炸弹至少需要 {最少人数} 人才能开始，当前 {人数} 人",
        summary: "人数不足",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.start.too_many",
        default: "数字炸弹最多 {上限} 人，当前 {人数} 人",
        summary: "人数超限",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.start.already_playing",
        default: "数字炸弹本局已经开始了，先发送 #结束 再重开",
        summary: "重复开局",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.start.started",
        default: "数字炸弹开始！范围 [{下限}, {上限}]，{人数} 人轮流猜；{玩家} 先猜，直接发送数字。",
        summary: "开局公布范围",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.guess.not_playing",
        default: "数字炸弹还没开始，发送 #开始 开局",
        summary: "未开局时猜数字",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.guess.not_in_game",
        default: "你不在这一局数字炸弹里，先发送 #加入",
        summary: "未加入时猜数字",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.guess.not_your_turn",
        default: "还没轮到你，当前轮到 {玩家}",
        summary: "非当前回合",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.guess.out_of_range",
        default: "超出范围：请猜 {下限} 到 {上限} 之间的数字",
        summary: "越界猜测",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.guess.duplicate",
        default: "{数字} 已经猜过了，换一个数字",
        summary: "重复猜测",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.guess.accepted",
        default: "{玩家} 猜 {数字}，范围收窄为 [{下限}, {上限}]；轮到 {下一位}",
        summary: "安全猜测后的收窄提示",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.guess.exploded",
        default: "砰！{玩家} 猜中了炸弹 {炸弹数字}，本局结束；发起人可发送 #重来 再来一局",
        summary: "猜中炸弹",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.timeout.auto",
        default: "{玩家} 超时，自动猜 {数字}",
        summary: "回合超时自动猜测",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.status.no_session",
        default: "当前没有进行中的数字炸弹，发送 #数字炸弹 开局",
        summary: "查询状态时没有房间",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.status.lobby",
        default: "数字炸弹房间：{人数}/{上限} 人（{玩家列表}）；发送 #加入 参与，#开始 开始",
        summary: "等待中的房间状态",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.status.playing",
        default: "数字炸弹：范围 [{下限}, {上限}]，轮到 {玩家}；当前 {人数} 人在局中",
        summary: "进行中的范围与回合",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.status.settled",
        default: "上一局炸弹是 {炸弹数字}，{玩家} 猜中；当前房间 {人数}/{上限} 人，发送 #重来 再开一局",
        summary: "爆炸后的结算状态",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.end.no_session",
        default: "当前没有进行中的数字炸弹",
        summary: "结束时不存在的房间",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.end.done",
        default: "数字炸弹已结束",
        summary: "主动结束房间",
        group: "娱乐",
    },
    ReplyTemplate {
        key: "number_bomb.guess.not_number",
        default: "请发送纯数字来猜，当前范围 [{下限}, {上限}]",
        summary: "当前玩家发了非数字",
        group: "娱乐",
    },
];

pub(crate) const DISABLED: ReplyTemplate = NUMBER_BOMB[0];
pub(crate) const OCCUPIED: ReplyTemplate = NUMBER_BOMB[1];
pub(crate) const ALREADY_ACTIVE: ReplyTemplate = NUMBER_BOMB[2];
pub(crate) const NO_SESSION: ReplyTemplate = NUMBER_BOMB[3];
pub(crate) const ONLY_CREATOR: ReplyTemplate = NUMBER_BOMB[4];
pub(crate) const LOBBY_CREATED: ReplyTemplate = NUMBER_BOMB[5];
pub(crate) const LOBBY_ALREADY_JOINED: ReplyTemplate = NUMBER_BOMB[6];
pub(crate) const LOBBY_JOINED: ReplyTemplate = NUMBER_BOMB[7];
pub(crate) const LOBBY_FULL: ReplyTemplate = NUMBER_BOMB[8];
pub(crate) const LOBBY_ALREADY_STARTED: ReplyTemplate = NUMBER_BOMB[9];
pub(crate) const START_TOO_FEW: ReplyTemplate = NUMBER_BOMB[10];
pub(crate) const START_TOO_MANY: ReplyTemplate = NUMBER_BOMB[11];
pub(crate) const START_ALREADY_PLAYING: ReplyTemplate = NUMBER_BOMB[12];
pub(crate) const STARTED: ReplyTemplate = NUMBER_BOMB[13];
pub(crate) const GUESS_NOT_PLAYING: ReplyTemplate = NUMBER_BOMB[14];
pub(crate) const GUESS_NOT_IN_GAME: ReplyTemplate = NUMBER_BOMB[15];
pub(crate) const GUESS_NOT_YOUR_TURN: ReplyTemplate = NUMBER_BOMB[16];
pub(crate) const GUESS_OUT_OF_RANGE: ReplyTemplate = NUMBER_BOMB[17];
pub(crate) const GUESS_DUPLICATE: ReplyTemplate = NUMBER_BOMB[18];
pub(crate) const GUESS_ACCEPTED: ReplyTemplate = NUMBER_BOMB[19];
pub(crate) const EXPLODED: ReplyTemplate = NUMBER_BOMB[20];
pub(crate) const TIMEOUT_AUTO: ReplyTemplate = NUMBER_BOMB[21];
pub(crate) const STATUS_NO_SESSION: ReplyTemplate = NUMBER_BOMB[22];
pub(crate) const STATUS_LOBBY: ReplyTemplate = NUMBER_BOMB[23];
pub(crate) const STATUS_PLAYING: ReplyTemplate = NUMBER_BOMB[24];
pub(crate) const STATUS_SETTLED: ReplyTemplate = NUMBER_BOMB[25];
pub(crate) const END_NO_SESSION: ReplyTemplate = NUMBER_BOMB[26];
pub(crate) const END_DONE: ReplyTemplate = NUMBER_BOMB[27];
pub(crate) const GUESS_NOT_NUMBER: ReplyTemplate = NUMBER_BOMB[28];
