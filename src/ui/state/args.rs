//! 模板参数（`UiTemplateArgs` / `ResolvedUiTemplateArgs`）与配置解析。

use crate::config::OcrConfig;
use crate::config::TemplateConfig;
use crate::observation::chat::ResolvedTemplateArgs;
use crate::observation::chat::TemplateArgs;
use std::path::PathBuf;

#[derive(Clone, Debug, Default)]
pub(crate) struct UiTemplateArgs {
    pub(super) secondary_back_template: Option<PathBuf>,
    pub(super) main_menu_template: Option<PathBuf>,
    pub(super) chat_templates: TemplateArgs,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedUiTemplateArgs {
    pub(crate) panel_downsample_factors: std::collections::BTreeMap<String, u32>,
    pub(crate) secondary_back_template: PathBuf,
    /// 主菜单「返回提瓦特」按钮模板。它比 secondary_back 优先判定：
    /// 奇域主菜单上也有那个圆形返回按钮，若不先挡住，菜单会被误判成二级聊天。
    pub(crate) main_menu_template: PathBuf,
    /// 二级聊天输入行「发送」圆钮模板；与圆形返回按钮一起判二级。
    pub(crate) secondary_chat_submit_template: PathBuf,
    /// F2 千星大厅页（多人面板）两个底部按钮锚点：左下「大厅列表」、右下「邀请玩家」。
    pub(crate) qianxing_f2_hall_list_template: PathBuf,
    pub(crate) qianxing_f2_invite_player_template: PathBuf,
    pub(crate) qianxing_exit_menu_template: PathBuf,
    /// 千星地图右下角锚点（「千星奇域·大厅」文字 + 庙宇图标；红色未读角标已 alpha 透明化）。
    pub(crate) qianxing_map_label_template: PathBuf,
    /// 个人资料页右下角「心愿单」字形模板（66×23，bbox + 2px 透明 padding）。
    pub(crate) qianxing_profile_page_wishlist_template: PathBuf,
    /// 个人资料页右下角「衣橱」字形模板（45×23，bbox + 2px 透明 padding）。
    pub(crate) qianxing_profile_page_wardrobe_template: PathBuf,
    /// 资料卡四分类锚点（全部纯文字模板）：
    /// 提瓦特「冒险等阶 / 世界等级 / 添加好友」，千星「奇域等级 / 添加好友 / 查看资料」。
    pub(crate) teyvat_profile_adventure_rank_template: PathBuf,
    pub(crate) teyvat_profile_world_level_template: PathBuf,
    pub(crate) teyvat_profile_add_friend_template: PathBuf,
    pub(crate) qianxing_profile_domain_level_template: PathBuf,
    pub(crate) qianxing_profile_add_friend_template: PathBuf,
    pub(crate) qianxing_profile_view_profile_template: PathBuf,
    pub(crate) world_wish_template: PathBuf,
    pub(crate) chat_templates: ResolvedTemplateArgs,
    /// 面板判据模板（好友面板 / 更多设置菜单 / 添加好友页 / 确认弹窗 /
    /// 千星档案卡 · 大厅卡 · 头像弹窗）。取自 `moderation` / `invite` 配置，
    /// 已在真实帧上逐个验证命中（见 dist/ui-flow/README.md）。
    ///
    /// 注意这里**没有** `friend_more_settings`（⋯）：它是共用 chrome，不是专属锚点，
    /// 更多设置菜单改由 `friend_block_chat_template`（菜单内才有的 `屏蔽聊天`）判定。
    pub(crate) friend_panel_template: PathBuf,
    pub(crate) friend_search_panel_template: PathBuf,
    pub(crate) friend_block_chat_template: PathBuf,
    /// 确认按钮模板（拉黑/屏蔽弹窗，65×34 纯文字「确认」）。
    pub(crate) confirm_button_template: PathBuf,
    /// 确认弹窗文案关键词锚点：进入大厅 / 离开大厅 / 屏蔽玩家。
    pub(crate) confirm_message_random_hall_template: PathBuf,
    pub(crate) confirm_message_return_teyvat_template: PathBuf,
    pub(crate) confirm_message_block_template: PathBuf,
    pub(crate) invite_view_star_template: PathBuf,
    pub(crate) invite_goto_hall_template: PathBuf,
    pub(crate) invite_enter_hall_template: PathBuf,
}

impl UiTemplateArgs {
    pub(crate) fn resolve(
        &self,
        templates: &TemplateConfig,
        ocr: &OcrConfig,
    ) -> ResolvedUiTemplateArgs {
        ResolvedUiTemplateArgs {
            panel_downsample_factors: templates.panel_downsample_factors.clone(),
            secondary_back_template: self
                .secondary_back_template
                .clone()
                .unwrap_or_else(|| templates.secondary_back.clone()),
            main_menu_template: self
                .main_menu_template
                .clone()
                .unwrap_or_else(|| templates.main_menu.clone()),
            secondary_chat_submit_template: templates.secondary_chat_submit.clone(),
            qianxing_f2_hall_list_template: templates.qianxing_f2_hall_list.clone(),
            qianxing_f2_invite_player_template: templates.qianxing_f2_invite_player.clone(),
            qianxing_exit_menu_template: templates.qianxing_exit_menu.clone(),
            qianxing_map_label_template: templates.qianxing_map_label.clone(),
            qianxing_profile_page_wishlist_template: templates
                .qianxing_profile_page_wishlist
                .clone(),
            qianxing_profile_page_wardrobe_template: templates
                .qianxing_profile_page_wardrobe
                .clone(),
            teyvat_profile_adventure_rank_template: templates.teyvat_profile_adventure_rank.clone(),
            teyvat_profile_world_level_template: templates.teyvat_profile_world_level.clone(),
            teyvat_profile_add_friend_template: templates.teyvat_profile_add_friend.clone(),
            qianxing_profile_domain_level_template: templates.qianxing_profile_domain_level.clone(),
            qianxing_profile_add_friend_template: templates.qianxing_profile_add_friend.clone(),
            qianxing_profile_view_profile_template: templates.qianxing_profile_view_profile.clone(),
            world_wish_template: templates.world_wish.clone(),
            chat_templates: self.chat_templates.resolve(templates, ocr),
            friend_panel_template: templates.friend_panel.clone(),
            friend_search_panel_template: templates.friend_search_panel.clone(),
            friend_block_chat_template: templates.friend_block_chat.clone(),
            confirm_button_template: templates.confirm_button.clone(),
            confirm_message_random_hall_template: templates.confirm_message_random_hall.clone(),
            confirm_message_return_teyvat_template: templates.confirm_message_return_teyvat.clone(),
            confirm_message_block_template: templates.confirm_message_block.clone(),
            invite_view_star_template: templates.invite_view_star.clone(),
            invite_goto_hall_template: templates.invite_goto_hall.clone(),
            invite_enter_hall_template: templates.invite_enter_hall.clone(),
        }
    }
}
