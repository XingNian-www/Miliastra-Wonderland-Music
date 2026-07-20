use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkflowPoint {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkflowRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorkflowPixelStability {
    pub timeout_ms: u64,
    pub mean_threshold: f32,
    pub changed_ratio_threshold: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkflowResidency {
    Primary,
    SecondaryCurrentHall,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkflowMouseButton {
    Left,
    Middle,
    Right,
}

/// A feature-owned workflow can request only these mechanical UI operations.
/// The UI runtime owns their execution semantics and never sees feature configuration.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkflowOperation {
    Wait {
        duration_ms: u64,
    },
    PressKey {
        key: String,
    },
    HoldKey {
        key: String,
        duration_seconds: u64,
    },
    ActivateGame {
        after_activate_ms: u64,
    },
    FocusGame {
        after_activate_ms: u64,
    },
    EnsureResidency {
        target: WorkflowResidency,
    },
    /// Restore the active chat-listener residency; resolved by the application layer.
    ReturnListenerResidency,
    ClickPoint {
        point: WorkflowPoint,
    },
    ClickMouseButton {
        button: WorkflowMouseButton,
    },
    WaitTemplate {
        template: PathBuf,
        region: WorkflowRect,
        threshold: f32,
        timeout_ms: u64,
        poll_ms: u64,
    },
    ClickTemplate {
        template: PathBuf,
        region: WorkflowRect,
        threshold: f32,
        timeout_ms: u64,
        poll_ms: u64,
        offset: WorkflowPoint,
    },
    WaitTemplateAbsent {
        template: PathBuf,
        region: WorkflowRect,
        threshold: f32,
        timeout_ms: u64,
        poll_ms: u64,
        stability: Option<WorkflowPixelStability>,
    },
    WaitPixelsStable {
        region: WorkflowRect,
        poll_ms: u64,
        stability: WorkflowPixelStability,
    },
    WaitText {
        expected: String,
        region: WorkflowRect,
        timeout_ms: u64,
        poll_ms: u64,
    },
    ClickText {
        expected: String,
        region: WorkflowRect,
        timeout_ms: u64,
        poll_ms: u64,
        offset: WorkflowPoint,
    },
    PasteText {
        text: String,
        clipboard_hold_ms: u64,
    },
}
