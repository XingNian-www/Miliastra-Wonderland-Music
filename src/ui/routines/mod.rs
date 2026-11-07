mod custom_action;
mod friend_delivery;
mod hall;
mod invite;
mod moderation;
mod secondary_unread;
mod startup;
mod state_observation;

pub(crate) use custom_action::{CustomActionPlan, CustomActionUi};
// `CapturedPrimaryOutcome` 由 task-62 的 AI 聊天接入后才在库内被引用；
// 先在这里导出，避免接入方跨私有模块路径取类型。
#[allow(unused_imports)]
pub(crate) use friend_delivery::{
    CapturedPrimaryOutcome, EstablishResidency, FriendDelivery, FriendDeliveryMessageStatus,
    FriendDeliveryRoutineConfig, FriendDeliveryRoutineConfigSource, FriendDeliveryUi,
    HallBatchStatus, HallBatchUi, ResidencyUi, SendFriendDeliveries, SendHallBatch,
    UiResidencyOutcome, UiResidencyTarget, highlighted_conversation_row,
};
pub(crate) use hall::{
    DetectPublicHall, DetectPublicHallEffect, HallRoutineConfig, HallUi, ReadHallInfo,
    ReadHallInfoEffect, ToggleMicrophone, ToggleMicrophoneEffect,
};
pub(crate) use invite::{
    ExecuteInvite, InviteEffect, InviteNotificationOutcome, InviteRoutineConfig,
    InviteRoutineConfigSource, InviteUi,
};
pub(crate) use moderation::{
    ExecuteModeration, ModerationEffect, ModerationRoutineConfig, ModerationRoutineConfigSource,
    ModerationUi, ModerationUiAction,
};
pub(crate) use secondary_unread::{
    ProcessSecondaryUnread, SecondaryUnreadEffect, SecondaryUnreadRoutineConfig, SecondaryUnreadUi,
};
pub(crate) use startup::{
    EnterGame, EnterGameEffect, EnterWonderland, EnterWonderlandEffect, StartupRoutineConfig,
    StartupUi, StartupUiConfig, StartupUiTemplates,
};
