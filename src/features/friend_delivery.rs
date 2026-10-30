#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FriendMessage {
    recipient: String,
    /// 搜好友列表时可尝试的其他名字。
    ///
    /// 用于只有 UID 的场景：`recipient` 可能是纯 UID（外部聊天源的信封主语），
    /// 而好友列表上显示的是昵称/备注。这里补上真正可匹配的名字。
    search_names: Vec<String>,
    message: String,
}

impl FriendMessage {
    pub(crate) fn new(recipient: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            recipient: recipient.into(),
            search_names: Vec::new(),
            message: message.into(),
        }
    }

    /// 追加可搜索的候选名（UID 反查得到）。
    pub(crate) fn with_search_names<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.search_names = names.into_iter().map(Into::into).collect();
        self
    }

    pub(crate) fn search_names(&self) -> &[String] {
        &self.search_names
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
