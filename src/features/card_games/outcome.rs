//! 牌局对外结果与回复模板载体。
//!
//! 从 `card_games.rs` 拆出：`LandlordOutcome` 是状态机与投递层之间的唯一契约，
//! `CardGameReplyTemplate` 负责保留模板 key 以便用户覆盖。
//!
//! 可见性口径：`CardGameReplyTemplate` 与 `LandlordPrivateDelivery` 只供 crate 内的
//! `composition` 投递层构造/消费（对外只是成品文案），因此都是 `pub(crate)`；
//! 它们承载的 `ReplyTemplate` / `ReplyVars` 同样是 crate 内部类型。历史上这两个类型
//! 声明为 `pub` 并再导出，导致 rustc 提示「类型比字段更私有」。这里就地收口到
//! `pub(crate)`，而不是放宽 reply_template 的对外 API。

use crate::features::reply_template::ReplyVars;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandlordOutcome {
    pub action: &'static str,
    pub public_reply: Option<String>,
    pub public_reply_template: Option<CardGameReplyTemplate>,
    pub private_reply: Option<String>,
    pub private_reply_template: Option<CardGameReplyTemplate>,
    pub private_deliveries: Vec<LandlordPrivateDelivery>,
    pub ended: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CardGameReplyTemplate {
    Template {
        spec: &'static crate::features::reply_template::ReplyTemplate,
        vars: ReplyVars,
    },
    /// 外层模板的某个变量本身来自另一条模板（例如斗地主的"动作"词）。
    ///
    /// 单独保留内层 key，是为了让内层文案也能被用户覆盖：如果先把内层渲染成
    /// 字符串再当变量塞进外层，组装层只拿到成品文本，内层覆盖就永远读不到。
    Composed {
        spec: &'static crate::features::reply_template::ReplyTemplate,
        vars: ReplyVars,
        nested: Vec<(&'static str, CardGameReplyTemplate)>,
    },
    Raw(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LandlordPrivateDelivery {
    pub player: String,
    pub message: String,
    pub template: Option<CardGameReplyTemplate>,
}

/// 好友批量投递的结果：只有 crate 内的投递层消费（见 [`LandlordPrivateDelivery`]）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CardGameFriendBatchOutcome {
    Complete,
    Failed {
        retryable: Vec<LandlordPrivateDelivery>,
        failure: crate::features::friend_delivery::FriendBatchFailure,
    },
}

impl LandlordOutcome {
    pub(super) fn public(action: &'static str, reply: impl Into<CardGameReplyTemplate>) -> Self {
        let reply = reply.into();
        Self {
            action,
            public_reply: Some(reply.render_default()),
            public_reply_template: reply.is_keyed().then_some(reply),
            private_reply: None,
            private_reply_template: None,
            private_deliveries: Vec::new(),
            ended: false,
        }
    }

    pub(super) fn private(action: &'static str, reply: impl Into<CardGameReplyTemplate>) -> Self {
        let reply = reply.into();
        Self {
            action,
            public_reply: None,
            public_reply_template: None,
            private_reply: Some(reply.render_default()),
            private_reply_template: reply.is_keyed().then_some(reply),
            private_deliveries: Vec::new(),
            ended: false,
        }
    }

    pub(super) fn ended(action: &'static str, reply: impl Into<CardGameReplyTemplate>) -> Self {
        let reply = reply.into();
        Self {
            action,
            public_reply: Some(reply.render_default()),
            public_reply_template: reply.is_keyed().then_some(reply),
            private_reply: None,
            private_reply_template: None,
            private_deliveries: Vec::new(),
            ended: true,
        }
    }

    pub(super) fn with_private_deliveries(
        mut self,
        deliveries: Vec<LandlordPrivateDelivery>,
    ) -> Self {
        self.private_deliveries = deliveries;
        self
    }
}

impl From<String> for CardGameReplyTemplate {
    fn from(text: String) -> Self {
        Self::Raw(text)
    }
}

impl From<&str> for CardGameReplyTemplate {
    fn from(text: &str) -> Self {
        Self::Raw(text.to_string())
    }
}

impl CardGameReplyTemplate {
    pub fn render_default(&self) -> String {
        match self {
            Self::Template { spec, vars } => {
                crate::features::reply_template::render(spec, None, vars).text
            }
            Self::Composed { spec, vars, nested } => {
                let nested = nested
                    .iter()
                    .map(|(name, reply)| (*name, reply.render_default()))
                    .collect();
                let vars = vars.clone().with_nested(nested);
                crate::features::reply_template::render(spec, None, &vars).text
            }
            Self::Raw(text) => text.clone(),
        }
    }

    /// 这条回复是否带模板 key；只有带 key 的才能在发送时查用户覆盖。
    pub(crate) fn is_keyed(&self) -> bool {
        matches!(self, Self::Template { .. } | Self::Composed { .. })
    }
}
