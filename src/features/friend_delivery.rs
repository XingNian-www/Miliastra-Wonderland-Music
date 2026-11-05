#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FriendMessage {
    /// 收件人：可以是软件显示名、内部身份键，或只有 UID 时的命令主语。
    ///
    /// 搜好友列表要用的名字**不在这里给**：装配层统一用
    /// identity.friend_search_names 从收件人反查（备注串 → 真实昵称），
    /// 避免调用方各拼一份候选导致口径分叉。
    recipient: String,
    message: String,
}

impl FriendMessage {
    pub(crate) fn new(recipient: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            recipient: recipient.into(),
            message: message.into(),
        }
    }

    pub(crate) fn recipient(&self) -> &str {
        &self.recipient
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FriendBatchFailureKind {
    ConfirmedUnsent,
    ResultUnknown,
}

impl FriendBatchFailureKind {
    /// 日志用的中文说法：失败性质决定能否安全重试，必须写清。
    pub(crate) fn log_label(self) -> &'static str {
        match self {
            Self::ConfirmedUnsent => "确认未送达，可安全重试",
            Self::ResultUnknown => "送达结果未知，重试可能重复发送",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FriendBatchFailure {
    kind: FriendBatchFailureKind,
    reason: String,
}

impl FriendBatchFailure {
    pub(crate) fn new(kind: FriendBatchFailureKind, reason: impl Into<String>) -> Self {
        Self {
            kind,
            reason: reason.into(),
        }
    }

    pub(crate) fn kind(&self) -> FriendBatchFailureKind {
        self.kind
    }

    pub(crate) fn reason(&self) -> &str {
        &self.reason
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FriendBatchOutcome {
    Complete,
    Failed {
        retryable: Vec<FriendMessage>,
        failure: FriendBatchFailure,
    },
}
