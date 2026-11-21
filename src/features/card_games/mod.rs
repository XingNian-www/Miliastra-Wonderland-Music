//! 牌类玩法（斗地主 / 跑得快）。
//!
//! 目录化后的职责划分：
//! - `commands`：聊天命令词表、锁键与「谁抢注这条命令」；
//! - `config`：`landlord` 配置段与校验；
//! - `deadline`：牌局截止类型（大厅超时 / 回合预警 / 回合超时）；
//! - `cards` / `rng`：牌面基础类型与确定性随机数；
//! - `dealing`：发牌、手牌与出牌展示格式；
//! - `rules`：牌型识别与压制规则（纯函数）；
//! - `landlord`：对局状态机（叫地主 → 抢地主 → 出牌 → 结算）；
//! - `outcome` / `reply_helpers`：对外结果类型与文案渲染入口；
//! - `service`：进入牌局的闸门、挂起效果链与投递端口。
//!
//! 设计约束：
//! - 玩家可见文案只用展示名；内部身份键（UUID）不得渲染进任何回复。
//! - 状态机与投递解耦：命令只推进状态并产出 `LandlordOutcome`，
//!   真正发送由 `service` 的效果链驱动，失败可重试、可取消。

mod cards;
mod commands;
mod config;
mod deadline;
mod dealing;
mod landlord;
mod outcome;
mod reply_helpers;
mod rng;
mod rules;
mod service;

#[cfg(test)]
mod tests;

pub use commands::LandlordCommand;
pub use config::LandlordConfig;
#[allow(unused_imports)]
pub use deadline::{CardGameDeadlineKind, CardGameDeadlineModule, CardGameDeadlineToken};
// 状态机与结果类型在 Linux 测试构建下没有本模块之外的引用方（真正的消费方
// `composition` 仅 Windows 编译），因此重导出本身会被判为未使用。导出路径必须
// 保持稳定，这里只静音「没人经过这条路径取用」的告警。
#[allow(unused_imports)]
pub use landlord::LandlordGame;
#[allow(unused_imports)]
pub use outcome::{
    CardGameFriendBatchOutcome, CardGameReplyTemplate, LandlordOutcome, LandlordPrivateDelivery,
};
pub use service::{
    CardGameApplication, CardGameCancel, CardGameCommandStart, CardGameEffectClaim,
    CardGameEffectKey, CardGameEffectLane, CardGameEffectResult, CardGameEffectTask,
    CardGameResume, CardGameRuntimePort, CardGameTimedOutcome,
};
#[cfg(test)]
pub(crate) use service::{
    CardGameCompletion, CardGameEffect, CardGameEffectRequest, CardGameLateResult,
    CardGameStartReservation,
};
pub use service::{CardGameDeliveryPort, CardGameService};
