use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use serde::Serialize;

use super::geometry::Rect;

const MAX_TOOL_RECORDS: usize = 30;
const MAX_QUEUED_WEB_TOOLS: usize = 10;
const MAX_TOOL_RESULT_CHARS: usize = 48 * 1024;

#[derive(Clone)]
pub(super) struct WebToolShared {
    inner: Arc<Mutex<WebToolState>>,
}

struct WebToolState {
    next_id: u64,
    queued: VecDeque<WebToolTask>,
    records: VecDeque<WebToolRecord>,
}

#[derive(Clone, Debug)]
pub(super) struct WebToolTask {
    pub(super) id: u64,
    pub(super) request: WebToolRequest,
}

#[derive(Clone, Debug)]
pub(super) enum WebToolRequest {
    Ocr {
        rect: Option<Rect>,
    },
    ScanChat,
    UiState,
    HallName,
    MatchTemplate {
        template: WebToolTemplate,
        rect: Option<Rect>,
        threshold: Option<f32>,
        click: bool,
    },
    Click {
        x: i32,
        y: i32,
    },
    Key {
        key: String,
    },
    ChatChangeSamples {
        samples: u32,
        interval_ms: u64,
    },
    PanelResponseBenchmark {
        rounds: u32,
    },
    OcrBackendProbe,
    AiSearchPreview {
        keyword: String,
        prefer_accompaniment: bool,
    },
}

#[derive(Clone, Debug)]
pub(super) enum WebToolTemplate {
    BlueMarker,
    YellowMarker,
    PinkMarker,
    Enter,
    SecondaryHall,
    InviteViewStar,
    InviteGotoHall,
    InviteEnterHall,
    FriendPanel,
    FriendSearchPanel,
    FriendMoreSettings,
    FriendBlockChat,
    FriendBlacklist,
    FriendConfirm,
    WonderlandEnterButton,
    PaimonMenu,
    WonderlandClose,
    Custom(String),
}

impl WebToolTemplate {
    pub(super) fn parse(value: &str, custom_templates: &HashMap<String, PathBuf>) -> Result<Self> {
        match value {
            "blue-marker" => Ok(Self::BlueMarker),
            "yellow-marker" => Ok(Self::YellowMarker),
            "pink-marker" => Ok(Self::PinkMarker),
            "enter" => Ok(Self::Enter),
            "secondary-hall" => Ok(Self::SecondaryHall),
            "invite-view-star" => Ok(Self::InviteViewStar),
            "invite-goto-hall" => Ok(Self::InviteGotoHall),
            "invite-enter-hall" => Ok(Self::InviteEnterHall),
            "friend-panel" => Ok(Self::FriendPanel),
            "friend-search-panel" => Ok(Self::FriendSearchPanel),
            "friend-more-settings" => Ok(Self::FriendMoreSettings),
            "friend-block-chat" => Ok(Self::FriendBlockChat),
            "friend-blacklist" => Ok(Self::FriendBlacklist),
            "friend-confirm" => Ok(Self::FriendConfirm),
            "wonderland-enter-button" => Ok(Self::WonderlandEnterButton),
            "paimon-menu" => Ok(Self::PaimonMenu),
            "wonderland-close" => Ok(Self::WonderlandClose),
            custom if custom_templates.contains_key(custom) => Ok(Self::Custom(custom.to_string())),
            _ => Err(anyhow!("template不是已配置的命名模板")),
        }
    }

    pub(super) fn label(&self) -> String {
        match self {
            Self::BlueMarker => "蓝色聊天标志".to_string(),
            Self::YellowMarker => "黄色聊天标志".to_string(),
            Self::PinkMarker => "粉色聊天标志".to_string(),
            Self::Enter => "回车界面".to_string(),
            Self::SecondaryHall => "二级当前大厅".to_string(),
            Self::InviteViewStar => "邀请查看千星".to_string(),
            Self::InviteGotoHall => "邀请前往大厅".to_string(),
            Self::InviteEnterHall => "邀请进入大厅".to_string(),
            Self::FriendPanel => "好友面板".to_string(),
            Self::FriendSearchPanel => "好友搜索面板".to_string(),
            Self::FriendMoreSettings => "好友更多设置".to_string(),
            Self::FriendBlockChat => "屏蔽聊天".to_string(),
            Self::FriendBlacklist => "拉黑".to_string(),
            Self::FriendConfirm => "好友操作确认".to_string(),
            Self::WonderlandEnterButton => "千星前往大厅".to_string(),
            Self::PaimonMenu => "派蒙主界面".to_string(),
            Self::WonderlandClose => "千星主页关闭按钮".to_string(),
            Self::Custom(name) => format!("自定义模板: {name}"),
        }
    }
}

impl WebToolRequest {
    pub(super) fn label(&self) -> String {
        match self {
            Self::Ocr { .. } => "OCR 识别".to_string(),
            Self::ScanChat => "聊天区扫描".to_string(),
            Self::UiState => "UI 状态检测".to_string(),
            Self::HallName => "大厅名称识别".to_string(),
            Self::MatchTemplate {
                template, click, ..
            } => format!(
                "{}{}",
                if *click {
                    "点击模板: "
                } else {
                    "模板匹配: "
                },
                template.label()
            ),
            Self::Click { x, y } => format!("点击坐标: {x},{y}"),
            Self::Key { key } => format!("按键: {key}"),
            Self::ChatChangeSamples {
                samples,
                interval_ms,
            } => {
                format!("聊天变化采样: {samples}次/{interval_ms}ms")
            }
            Self::PanelResponseBenchmark { rounds } => {
                format!("面板响应基准: {rounds}轮")
            }
            Self::OcrBackendProbe => "OCR 后端探测".to_string(),
            Self::AiSearchPreview { keyword, .. } => format!("AI 候选诊断: {keyword}"),
        }
    }

    pub(super) fn requires_screen_exclusive(&self) -> bool {
        matches!(
            self,
            Self::MatchTemplate { click: true, .. }
                | Self::Click { .. }
                | Self::Key { .. }
                | Self::PanelResponseBenchmark { .. }
        )
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WebToolSnapshot {
    pub(super) id: u64,
    pub(super) label: String,
    pub(super) status: String,
    pub(super) result: Option<String>,
}

#[derive(Clone, Debug)]
struct WebToolRecord {
    id: u64,
    label: String,
    status: &'static str,
    result: Option<String>,
}

impl WebToolShared {
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(WebToolState {
                next_id: 1,
                queued: VecDeque::new(),
                records: VecDeque::new(),
            })),
        }
    }

    pub(super) fn enqueue(&self, request: WebToolRequest) -> Result<WebToolSnapshot> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| anyhow!("Web 工具队列锁已损坏"))?;
        if state.queued.len() >= MAX_QUEUED_WEB_TOOLS {
            return Err(anyhow!("Web 工具任务过多，请等待现有任务完成"));
        }
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1).max(1);
        let label = request.label();
        state.queued.push_back(WebToolTask { id, request });
        state.records.push_front(WebToolRecord {
            id,
            label: label.clone(),
            status: "queued",
            result: None,
        });
        Self::trim_records(&mut state.records);
        Ok(WebToolSnapshot {
            id,
            label,
            status: "queued".to_string(),
            result: None,
        })
    }

    pub(super) fn take_next(&self) -> Result<Option<WebToolTask>> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| anyhow!("Web 工具队列锁已损坏"))?;
        let Some(task) = state.queued.pop_front() else {
            return Ok(None);
        };
        if let Some(record) = state.records.iter_mut().find(|record| record.id == task.id) {
            record.status = "running";
        }
        Ok(Some(task))
    }

    pub(super) fn finish(&self, id: u64, result: Result<String>) {
        let Ok(mut state) = self.inner.lock() else {
            log::error!("Web 工具队列锁已损坏，无法写入任务结果: id={id}");
            return;
        };
        let Some(record) = state.records.iter_mut().find(|record| record.id == id) else {
            log::warn!("Web 工具任务记录不存在: id={id}");
            return;
        };
        match result {
            Ok(value) => {
                record.status = "completed";
                record.result = Some(limit_result(value));
            }
            Err(error) => {
                record.status = "failed";
                record.result = Some(limit_result(format!("错误: {error:#}")));
            }
        }
    }

    pub(super) fn snapshot(&self, id: u64) -> Result<Option<WebToolSnapshot>> {
        let state = self
            .inner
            .lock()
            .map_err(|_| anyhow!("Web 工具队列锁已损坏"))?;
        Ok(state
            .records
            .iter()
            .find(|record| record.id == id)
            .map(Self::record_snapshot))
    }

    pub(super) fn recent(&self) -> Result<Vec<WebToolSnapshot>> {
        let state = self
            .inner
            .lock()
            .map_err(|_| anyhow!("Web 工具队列锁已损坏"))?;
        Ok(state
            .records
            .iter()
            .take(8)
            .map(Self::record_snapshot)
            .collect())
    }

    fn record_snapshot(record: &WebToolRecord) -> WebToolSnapshot {
        WebToolSnapshot {
            id: record.id,
            label: record.label.clone(),
            status: record.status.to_string(),
            result: record.result.clone(),
        }
    }

    fn trim_records(records: &mut VecDeque<WebToolRecord>) {
        while records.len() > MAX_TOOL_RECORDS {
            let can_drop = records
                .back()
                .is_some_and(|record| record.status != "queued" && record.status != "running");
            if !can_drop {
                break;
            }
            records.pop_back();
        }
    }
}

fn limit_result(value: String) -> String {
    let char_count = value.chars().count();
    if char_count <= MAX_TOOL_RESULT_CHARS {
        return value;
    }
    format!(
        "{}\n\n[结果过长，已截断：原始字符数={char_count}]",
        value
            .chars()
            .take(MAX_TOOL_RESULT_CHARS)
            .collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_input_tools_require_screen_exclusivity() {
        assert!(!WebToolRequest::UiState.requires_screen_exclusive());
        assert!(
            !WebToolRequest::MatchTemplate {
                template: WebToolTemplate::Enter,
                rect: None,
                threshold: None,
                click: false,
            }
            .requires_screen_exclusive()
        );
        assert!(
            WebToolRequest::MatchTemplate {
                template: WebToolTemplate::Enter,
                rect: None,
                threshold: None,
                click: true,
            }
            .requires_screen_exclusive()
        );
        assert!(WebToolRequest::Click { x: 1, y: 1 }.requires_screen_exclusive());
    }

    #[test]
    fn queue_has_a_fixed_upper_bound() {
        let tools = WebToolShared::new();
        for _ in 0..MAX_QUEUED_WEB_TOOLS {
            tools.enqueue(WebToolRequest::UiState).expect("queued task");
        }

        assert!(tools.enqueue(WebToolRequest::UiState).is_err());
    }

    #[test]
    fn task_result_is_bounded() {
        let value = "x".repeat(MAX_TOOL_RESULT_CHARS + 1);
        let result = limit_result(value);

        assert!(result.contains("结果过长，已截断"));
        assert!(result.chars().count() <= MAX_TOOL_RESULT_CHARS + 80);
    }
}
