#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FriendMessage {
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
