//! 外部聊天数据源：把"游戏外部的聊天推送"抽象成可替换的接口。
//!
//! 本模块只定义数据形状与扩展点，不含网络实现。具体实现（WebSocket /
//! SSE / 长轮询 / 其他游戏）由上层 crate 提供，新增一种来源只需实现
//! [`ChatSource`]，命令链路与时间线一行不改。

use std::fmt::{Display, Formatter};

/// 当前接入的游戏标识。
///
/// 参与歌单归属键与账号绑定，用来隔离不同游戏的 UID 空间。
pub const GAME_GENSHIN: &str = "genshin";

/// 聊天频道。
///
/// 用字符串而不是枚举，是因为不同游戏的频道划分不同；已知取值作为常量给出，
/// 未识别的取值原样保留并参与去重键，不会丢消息。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChatChannel(String);

impl ChatChannel {
    /// 当前所在大厅的聊天。
    pub const CURRENT_HALL: &'static str = "current_hall";
    /// 私聊。
    pub const PRIVATE: &'static str = "private";

    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn current_hall() -> Self {
        Self(Self::CURRENT_HALL.to_string())
    }

    pub fn private() -> Self {
        Self(Self::PRIVATE.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_current_hall(&self) -> bool {
        self.0 == Self::CURRENT_HALL
    }

    pub fn is_private(&self) -> bool {
        self.0 == Self::PRIVATE
    }
}

impl Display for ChatChannel {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// 游戏账号标识。
///
/// 用字符串承载而不是 `u32`：原神是 uint32，但换一个游戏就可能是雪球或 GUID。
/// 提供 u32 互转方便原神场景，同时不把结构写死。
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GameUid(String);

impl GameUid {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn from_u32(value: u32) -> Self {
        Self(value.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 原神场景下转回数字；非数字 UID 返回 None。
    pub fn as_u32(&self) -> Option<u32> {
        self.0.parse().ok()
    }
}

impl Display for GameUid {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl From<u32> for GameUid {
    fn from(value: u32) -> Self {
        Self::from_u32(value)
    }
}

/// 外部消息的发送者。
///
/// 昵称可能缺失（原神的昵称来自独立资料包，到达时序不确定），
/// 因此两者都是可选的；但**至少要有一个**才能构造消息。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExternalSender {
    pub nickname: Option<String>,
    pub uid: Option<GameUid>,
}

impl ExternalSender {
    pub fn new(nickname: Option<String>, uid: Option<GameUid>) -> Self {
        Self { nickname, uid }
    }

    /// 去重与身份匹配用的稳定键。
    ///
    /// 优先用 UID：昵称可以改、可以重名，UID 才是账号身份。
    /// 没有 UID 时才退化为昵称。
    pub fn key(&self) -> String {
        match (&self.uid, &self.nickname) {
            (Some(uid), _) => format!("uid:{uid}"),
            (None, Some(name)) => format!("name:{name}"),
            (None, None) => "unknown".to_string(),
        }
    }
}

/// 一条消息对应的游戏账号。
///
/// 定义在 kernel 而不是 features：观察层要把账号从消息带到命令信封，
/// 若定义在 features 会造成 observation → features 的分层倒退。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommandAccount {
    pub game: String,
    pub uid: String,
}

impl CommandAccount {
    pub fn new(game: impl Into<String>, uid: impl Into<String>) -> Self {
        Self {
            game: game.into(),
            uid: uid.into(),
        }
    }
}

/// 一条来自外部的聊天消息。
///
/// 与 OCR 读到的气泡不同，它**没有屏幕坐标**：不参与气泡对齐、不进命令锁的
/// 位置匹配。字段含义见 `chat-api/docs/API.md`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalChatMessage {
    /// 来源进程内的自增序号（不是游戏消息 ID）。用于去重与排序。
    pub sequence: u64,
    /// Unix 秒。仅作展示，**不参与排序或去重**（同秒多条会重复）。
    pub sent_at: Option<u64>,
    pub channel: ChatChannel,
    /// 大厅 ID；私聊为 None。
    pub hall_id: Option<u64>,
    pub sender: ExternalSender,
    /// 接收者 UID；大厅广播为 None。
    pub to_uid: Option<GameUid>,
    pub text: String,
}

impl ExternalChatMessage {
    /// 去重键：同一来源、同一频道、同一发送者的同一序号。
    ///
    /// 只用 `sequence` 不够——不同来源/频道的序号各自从 1 开始，
    /// 会把别人的消息误判成重复。发送者参与进来才能隔离。
    pub fn dedup_key(&self, source: &str) -> String {
        format!(
            "{source}:{}:{}:{}",
            self.channel,
            self.sender.key(),
            self.sequence
        )
    }
}

/// 拉取一批消息的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatSourceEvent {
    /// 有消息到达。
    Messages(Vec<ExternalChatMessage>),
    /// 大厅切换：上层应重置与该大厅绑定的临时状态。
    HallChanged {
        hall_id: u64,
        hall_type: Option<u32>,
    },
    /// 连接建立（含重连）。携带服务端已知的最大序号，便于补拉。
    Connected { last_sequence: u64 },
}

/// 外部聊天数据源。
///
/// 实现方负责：连接管理、重连、协议解析；**不负责**去重与命令语义。
/// 扩展点：新增一种来源（SSE、长轮询、其他游戏）只需实现本 trait。
pub trait ChatSource: Send {
    /// 来源名称，用于日志与去重键前缀。
    fn name(&self) -> &str;

    /// 阻塞等待下一个事件。返回 `None` 表示当前没有事件（读超时或心跳），
    /// 调用方应继续调用；错误表示需要重连。
    ///
    /// 用阻塞而不是 async，是为了与仓库既有的"每类线程各自阻塞"模型一致；
    /// 实现内部可以自由使用 tokio 或其他异步运行时。
    fn next_event(&mut self) -> anyhow::Result<Option<ChatSourceEvent>>;

    /// 连接此刻是否仍然可用（未关闭）。
    ///
    /// `next_event` 返回 `None` 时调用方需要区分两种情形：连接还活着
    /// （读超时 / Ping / Pong），此时应刷新租约；连接已关，此时应等租约
    /// 超时后回落 OCR 并重连。实现方默认返回 `true`，避免新增来源时
    /// 被迫重写——但凡是会把连接置空的实现都必须覆盖它。
    fn is_connected(&self) -> bool {
        true
    }
}
