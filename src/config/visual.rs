//! Versioned visual profile projection. SQLite sections remain the sole authority;
//! import merges only mapped leaves, preserving unrelated settings and Live workflows.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VisualProfile {
    pub schema_version: u32,
    pub canvas: Canvas,
    pub templates: BTreeMap<String, String>,
    pub thresholds: BTreeMap<String, Value>,
    pub regions: BTreeMap<String, Value>,
    pub points: BTreeMap<String, Value>,
    pub chat_geometry: BTreeMap<String, Value>,
    pub capture: BTreeMap<String, Value>,
    pub matching: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
}

fn fields() -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut add = |group: &str, key: String, path: String| out.push((group.to_owned(), key, path));
    for key in [
        "blue_marker",
        "yellow_marker",
        "pink_marker",
        "friend",
        "secondary_back",
        "secondary_hall",
        "main_menu",
        "hall_bar",
        "world_wish",
        "invite_view_star",
        "invite_goto_hall",
        "invite_enter_hall",
        "friend_panel",
        "friend_search_panel",
        "friend_more_settings",
        "friend_block_chat",
        "friend_blacklist",
        "friend_confirm",
        "paimon_menu_panel",
    ] {
        add("templates", key.into(), format!("templates.{key}"));
    }
    for key in ["wonderland_map_star", "wonderland_confirm", "paimon_menu"] {
        add(
            "templates",
            format!("startup.{key}"),
            format!("startup.templates.{key}"),
        );
    }
    for (key, path) in [
        ("builtin", "templates.marker_threshold"),
        ("startup", "startup.template_threshold"),
        ("startup_confirm", "startup.wonderland_confirm_threshold"),
    ] {
        add("thresholds", key.into(), path.into());
    }
    for key in [
        "chat",
        "friend",
        "secondary_back",
        "secondary_hall",
        "main_menu",
        "hall_bar",
        "world_wish",
        "paimon_menu",
        "hall_name",
        "hall_member_count",
        "hall_time",
        "hall_member_list",
    ] {
        add("regions", key.into(), format!("screen.{key}_rect"));
    }
    for (section, keys) in [
        (
            "moderation",
            &[
                "friend_panel",
                "search_panel",
                "more_settings",
                "block_chat",
                "blacklist",
                "confirm",
            ][..],
        ),
        (
            "invite",
            &[
                "friend_list",
                "friend_chat",
                "view_star",
                "goto_hall",
                "enter_hall",
            ][..],
        ),
        (
            "startup",
            &[
                "enter_game_text",
                "wonderland_hall_ocr",
                "wonderland_confirm",
                "main_ui",
                "wonderland_map_star",
            ][..],
        ),
    ] {
        for key in keys {
            add(
                "regions",
                format!("{section}.{key}"),
                format!("{section}.{key}_region"),
            );
        }
    }
    for (key, path) in [
        ("window.focus", "window.focus_point"),
        ("output.focus", "output.focus_point"),
        ("output.chat_click", "output.chat_click_2"),
        ("moderation.search_input", "moderation.search_input_point"),
        ("moderation.search_button", "moderation.search_button_point"),
    ] {
        add("points", key.into(), path.into());
    }
    for key in [
        "marker_search_width",
        "marker_dedupe_min_x",
        "marker_dedupe_min_y",
        "marker_dedupe_x",
        "marker_dedupe_y",
        "text_left_gap",
        "block_top_padding",
        "block_bottom_padding",
        "max_block_height",
        "same_line_y_tolerance",
        "next_marker_min_gap",
        "right_padding",
        "change_mean_threshold",
        "change_pixel_threshold",
    ] {
        add("chat_geometry", key.into(), format!("ocr.{key}"));
    }
    add(
        "capture",
        "warn_on_size_mismatch".into(),
        "screen.warn_on_size_mismatch".into(),
    );
    add(
        "matching",
        "panel_downsample_factors".into(),
        "templates.panel_downsample_factors".into(),
    );
    out
}

/// Schema paths owned by the visual profile. The schema marks these fields so the
/// default form can hide them and edit them only through the profile panel.
pub fn managed_paths() -> BTreeSet<String> {
    let mut paths: BTreeSet<String> = [
        "window.content_width",
        "window.content_height",
        "screen.expected_width",
        "screen.expected_height",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    for (_, _, path) in fields() {
        paths.insert(path);
    }
    paths
}

fn get<'a>(root: &'a Value, path: &str) -> Result<&'a Value> {
    path.split('.').try_fold(root, |node, key| {
        node.get(key)
            .with_context(|| format!("缺少配置字段 {path}"))
    })
}
fn set(root: &mut Value, path: &str, value: Value) -> Result<()> {
    let mut parts = path.split('.').peekable();
    let mut node = root;
    while let Some(key) = parts.next() {
        if parts.peek().is_none() {
            node.as_object_mut()
                .context("配置节点不是对象")?
                .insert(key.into(), value);
            return Ok(());
        }
        node = node
            .get_mut(key)
            .with_context(|| format!("缺少配置字段 {path}"))?;
    }
    bail!("无效配置路径")
}

impl VisualProfile {
    /// Accept raw stored values, never resolved runtime paths.
    pub fn export(sections: &Value) -> Result<Self> {
        if get(sections, "window.content_width")? != get(sections, "screen.expected_width")?
            || get(sections, "window.content_height")? != get(sections, "screen.expected_height")?
        {
            bail!("窗口参考尺寸与识别画布不一致，请先修正现有配置");
        }
        let mut groups = Map::new();
        groups.insert("schema_version".into(), json!(1));
        groups.insert("canvas".into(), json!({"width": get(sections,"screen.expected_width")?, "height": get(sections,"screen.expected_height")?}));
        for (group, key, path) in fields() {
            let obj = groups.entry(group).or_insert_with(|| json!({}));
            obj.as_object_mut()
                .unwrap()
                .insert(key, get(sections, &path)?.clone());
        }
        serde_json::from_value(Value::Object(groups)).context("构建视觉档案失败")
    }

    /// Return whole affected sections, preserving every non-visual leaf.
    pub fn candidate(&self, current: &Value) -> Result<Map<String, Value>> {
        if self.schema_version != 1 {
            bail!("不支持的视觉档案版本 {}", self.schema_version);
        }
        if self.canvas.width == 0 || self.canvas.height == 0 {
            bail!("画布尺寸必须大于 0");
        }
        let value = serde_json::to_value(self)?;
        let expected = serde_json::to_value(Self::export(current)?)?;
        for group in [
            "templates",
            "thresholds",
            "regions",
            "points",
            "chat_geometry",
            "capture",
            "matching",
        ] {
            let actual = value[group].as_object().context("档案分组不是对象")?;
            let keys = expected[group].as_object().unwrap();
            if actual.len() != keys.len() || !actual.keys().all(|k| keys.contains_key(k)) {
                bail!("{group} 字段集合不完整或含未知字段，请使用当前版本导出档案");
            }
        }
        for (key, path) in &self.templates {
            if path.trim().is_empty() {
                bail!("模板路径不能为空: {key}");
            }
        }
        for (key, t) in &self.thresholds {
            if !t.as_f64().is_some_and(|n| (0.0..=1.0).contains(&n)) {
                bail!("阈值须在 0 到 1 之间: {key}");
            }
        }
        for (key, r) in &self.regions {
            let x = r["x"].as_i64();
            let y = r["y"].as_i64();
            let w = r["width"].as_u64();
            let h = r["height"].as_u64();
            let valid = match (x, y, w, h) {
                (Some(x), Some(y), Some(w), Some(h)) => {
                    x >= 0
                        && y >= 0
                        && w > 0
                        && h > 0
                        && (x as u64)
                            .checked_add(w)
                            .is_some_and(|v| v <= self.canvas.width as u64)
                        && (y as u64)
                            .checked_add(h)
                            .is_some_and(|v| v <= self.canvas.height as u64)
                }
                _ => false,
            };
            if !valid {
                bail!("区域格式错误或越界: {key}");
            }
        }
        for (key, p) in &self.points {
            if !p["x"]
                .as_u64()
                .is_some_and(|x| x < self.canvas.width as u64)
                || !p["y"]
                    .as_u64()
                    .is_some_and(|y| y < self.canvas.height as u64)
            {
                bail!("点击点格式错误或越界: {key}");
            }
        }
        let mut merged = current.clone();
        let mut affected = std::collections::BTreeSet::new();
        for (group, key, path) in fields() {
            set(&mut merged, &path, value[&group][&key].clone())?;
            affected.insert(path.split('.').next().unwrap().to_owned());
        }
        for (path, n) in [
            ("window.content_width", self.canvas.width),
            ("window.content_height", self.canvas.height),
            ("screen.expected_width", self.canvas.width),
            ("screen.expected_height", self.canvas.height),
        ] {
            set(&mut merged, path, json!(n))?;
        }
        affected.insert("window".into());
        affected.insert("screen".into());
        Ok(affected
            .into_iter()
            .map(|key| {
                let value = merged[&key].clone();
                (key, value)
            })
            .collect())
    }
}

/// Read-only caller catalog, not a JSON-programmable classifier or complete call graph.
pub fn match_catalog() -> Value {
    json!({
        "algorithms": {"chat_markers":"rgb_sad", "other_templates":"gray_sad"},
        "panelDownsampleFactors": {"friend_panel":1,"friend_search_panel":1,"friend_block_chat":4,"friend_confirm":4,"invite_enter_hall":4,"invite_goto_hall":4,"invite_view_star":4},
        "retainedClassificationAssets":["friend","hall_bar"],
        "derivedRegions":{"friend_delivery.secondary_hall_search":{"operation":"bounding_union","sources":["secondary_hall","invite.friend_list"]}},
        "notes":["更多设置模板仍供管理点击和恢复使用", "面板用途降采样可覆盖；流程点击继续使用原精度", "自定义流程保留独立 Live 配置与步骤覆盖", "更换同路径资源需重载进程清除模板缓存", "实际窗口尺寸不等于基准画布尺寸"]
    })
}

pub fn differences(before: &VisualProfile, after: &VisualProfile) -> Result<Vec<Value>> {
    fn walk(a: &Value, b: &Value, path: &str, out: &mut Vec<Value>) {
        if a == b {
            return;
        }
        if let (Some(a), Some(b)) = (a.as_object(), b.as_object()) {
            let keys: std::collections::BTreeSet<_> = a.keys().chain(b.keys()).collect();
            for k in keys {
                walk(
                    a.get(k).unwrap_or(&Value::Null),
                    b.get(k).unwrap_or(&Value::Null),
                    &format!("{path}/{k}"),
                    out,
                );
            }
        } else {
            out.push(json!({"path":path,"before":a,"after":b}));
        }
    }
    let mut out = Vec::new();
    walk(
        &serde_json::to_value(before)?,
        &serde_json::to_value(after)?,
        "",
        &mut out,
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    #[test]
    fn visual_profile_roundtrip_preserves_nondefault_and_unrelated_fields() {
        let mut config = AppConfig::default();
        config.templates.marker_threshold = 0.81;
        config.templates.friend_confirm = "custom/确认.png".into();
        config.startup.template_threshold = 0.72;
        config.custom_workflows.default_threshold = 0.63;
        let current = serde_json::to_value(config).unwrap();
        let mut profile = VisualProfile::export(&current).unwrap();
        let sections = profile.candidate(&current).unwrap();
        for (key, value) in &sections {
            assert_eq!(value, &current[key]);
        }
        profile.canvas.width = 2560;
        let sections = profile.candidate(&current).unwrap();
        assert_eq!(sections["window"]["content_width"], 2560);
        assert_eq!(sections["screen"]["expected_width"], 2560);
        assert!(!sections.contains_key("custom_workflows"));
        assert_eq!(
            sections["startup"]["enabled"],
            current["startup"]["enabled"]
        );
        assert_eq!(sections["templates"]["friend_confirm"], "custom/确认.png");
    }
    #[test]
    fn visual_profile_rejects_unknown_missing_invalid_and_outside_values() {
        let current = serde_json::to_value(AppConfig::default()).unwrap();
        let p = VisualProfile::export(&current).unwrap();
        let mut bad = p.clone();
        bad.templates.remove("friend");
        assert!(bad.candidate(&current).is_err());
        let mut bad = p.clone();
        bad.thresholds.insert("builtin".into(), json!(2));
        assert!(bad.candidate(&current).is_err());
        let mut bad = p.clone();
        bad.points
            .insert("output.focus".into(), json!({"x":1920,"y":0}));
        assert!(bad.candidate(&current).is_err());
        let mut bad = p.clone();
        bad.schema_version = 2;
        assert!(bad.candidate(&current).is_err());
        let mut v = serde_json::to_value(p).unwrap();
        v["unknown"] = json!(true);
        assert!(serde_json::from_value::<VisualProfile>(v).is_err());
    }
}
