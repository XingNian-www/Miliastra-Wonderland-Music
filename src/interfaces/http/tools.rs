use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Result, anyhow};

use crate::ui::geometry::Rect;

#[derive(Clone, Debug)]
pub(crate) enum WebToolRequest {
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
pub(crate) enum WebToolTemplate {
    BlueMarker,
    YellowMarker,
    PinkMarker,
    Friend,
    SecondaryBack,
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
    pub(crate) fn parse(value: &str, custom_templates: &HashMap<String, PathBuf>) -> Result<Self> {
        match value {
            "blue-marker" => Ok(Self::BlueMarker),
            "yellow-marker" => Ok(Self::YellowMarker),
            "pink-marker" => Ok(Self::PinkMarker),
            "friend" => Ok(Self::Friend),
            "secondary-back" => Ok(Self::SecondaryBack),
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

    pub(crate) fn label(&self) -> String {
        match self {
            Self::BlueMarker => "蓝色聊天标志".to_string(),
            Self::YellowMarker => "黄色聊天标志".to_string(),
            Self::PinkMarker => "粉色聊天标志".to_string(),
            Self::Friend => "好友按钮".to_string(),
            Self::SecondaryBack => "二级聊天返回按钮".to_string(),
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
    pub(crate) fn label(&self) -> String {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friend_is_the_only_web_name_for_the_primary_anchor() {
        let templates = HashMap::new();

        assert!(matches!(
            WebToolTemplate::parse("friend", &templates).expect("friend template"),
            WebToolTemplate::Friend
        ));
        assert!(WebToolTemplate::parse("group", &templates).is_err());
        assert!(WebToolTemplate::parse("enter", &templates).is_err());
    }
}
