//! 识图配置、JPEG 编码与「这轮要不要带图」判据。

use super::*;

#[test]
fn vision_defaults_match_p0_contract() {
    let config = AiChatConfig::default();
    assert!(config.vision.enabled);
    assert_eq!(config.vision.mode, VisionMode::Auto);
    assert_eq!(
        config.vision.capture_scope,
        VisionCaptureScope::PrimaryScreen
    );
    assert!(config.vision.allow_full_screen);
    assert_eq!(config.vision.max_edge, 1_600);
    assert_eq!(config.vision.jpeg_quality, 100);
    assert_eq!(config.vision.max_bytes, 4_000_000);
    assert_eq!(config.vision.timeout_ms, 800);
    assert!(config.vision.model.is_empty());
    let mut disabled = AiChatConfig::default();
    disabled.vision.enabled = false;
    assert!(!disabled.vision.enabled);
    assert_eq!(disabled.vision.max_edge, 1_600);
}

#[test]
fn missing_legacy_vision_section_defaults_disabled() {
    let mut value = serde_json::to_value(AiChatConfig::default()).unwrap();
    value.as_object_mut().unwrap().remove("vision");
    let parsed: AiChatConfig = serde_json::from_value(value).unwrap();
    assert!(!parsed.vision.enabled, "旧配置缺 vision 应默认关闭");
    assert_eq!(parsed.vision.mode, VisionMode::Auto);
    assert!(
        !parsed.vision.direct_to_main,
        "旧配置 direct_to_main 应默认 false"
    );
    // 保存 round-trip：后端读到的旧配置必须写回 false，页面/运行时口径一致。
    let saved = serde_json::to_value(&parsed).unwrap();
    assert_eq!(saved["vision"]["enabled"], false);
    let reloaded: AiChatConfig = serde_json::from_value(saved).unwrap();
    assert!(!reloaded.vision.enabled);
    assert!(AiChatConfig::default().vision.enabled, "新默认仍应开启");
}

#[test]
fn vision_encoding_downscales_to_max_edge_and_stays_under_byte_cap() {
    let image = DynamicImage::new_rgb8(3_000, 2_000);
    let encoded = encode_vision_image(&image, 1_280, 80, 4_000_000).expect("encode");
    assert_eq!(encoded.width(), 1_280);
    assert_eq!(encoded.height(), 853);
    assert!(encoded.byte_len() <= 4_000_000);
    assert!(encoded.data_url().starts_with("data:image/jpeg;base64,"));
    let debug = format!("{encoded:?}");
    assert!(debug.contains("1280x853") || debug.contains("width: 1280"));
    assert!(!debug.contains("base64"));
}

#[test]
fn vision_encoding_accepts_quality_100_and_still_honors_byte_cap() {
    let image = DynamicImage::new_rgb8(64, 64);
    let encoded = encode_vision_image(&image, 1_600, 100, 4_000_000).expect("quality 100");
    assert_eq!(encoded.width(), 64);
    assert_eq!(encoded.height(), 64);
    // 超限时仍会降质量/放弃，quality=100 不绕过 max_bytes。
    assert!(encode_vision_image(&image, 1_600, 100, 1).is_none());
    // 超范围输入按 40..=100 clamp，不 panic。
    assert!(encode_vision_image(&image, 1_600, 255, 4_000_000).is_some());
}

#[test]
fn vision_validate_accepts_quality_100_and_rejects_out_of_range() {
    let mut config = AiChatConfig::default();
    config.enabled = true;
    config.vision.enabled = true;
    config.vision.jpeg_quality = 100;
    config.validate().expect("100 合法");
    config.vision.jpeg_quality = 101;
    assert!(config.validate().is_err());
    config.vision.jpeg_quality = 39;
    assert!(config.validate().is_err());
    config.vision.jpeg_quality = 40;
    config.validate().expect("40 合法");
}

#[test]
fn vision_encoding_rejects_zero_size_and_impossible_byte_cap() {
    let tiny = DynamicImage::new_rgb8(1, 1);
    assert!(encode_vision_image(&tiny, 1_280, 80, 1).is_none());
    assert!(encode_vision_image(&tiny, 0, 80, 4_000_000).is_none());
    assert!(encode_vision_image(&tiny, 1_280, 80, 0).is_none());
}

#[test]
fn vision_mode_controls_auto_image_and_look_tool() {
    let mut config = AiChatConfig::default();
    assert_eq!(config.vision.mode, VisionMode::Auto);
    assert!(auto_image_enabled(&config, true));
    assert!(!look_tool_available(&config));

    config.vision.mode = VisionMode::Tool;
    assert!(!auto_image_enabled(&config, true));
    assert!(look_tool_available(&config));

    config.vision.mode = VisionMode::Both;
    assert!(auto_image_enabled(&config, true));
    assert!(look_tool_available(&config));

    config.vision.allow_full_screen = false;
    assert!(!look_tool_available(&config), "整屏权限关闭时不应暴露 look");
    config.vision.allow_full_screen = true;
    config.vision.enabled = false;
    assert!(!auto_image_enabled(&config, true));
    assert!(!look_tool_available(&config));

    // direct_to_main 只影响 look 时的直接附图，不再自动截图。
    let mut direct = AiChatConfig::default();
    direct.vision.direct_to_main = true;
    direct.vision.mode = VisionMode::Tool;
    assert!(
        !auto_image_enabled(&direct, true),
        "tool+direct 普通聊天不截图"
    );
    assert!(look_tool_available(&direct), "look 仍暴露");
    assert!(
        !auto_image_enabled(&direct, false),
        "主模型不支持图片时不附图"
    );
    direct.vision.mode = VisionMode::Auto;
    assert!(
        auto_image_enabled(&direct, true),
        "auto 模式仍按原逻辑自动附图"
    );
    direct.vision.enabled = false;
    assert!(!auto_image_enabled(&direct, true));
    assert!(!look_tool_available(&direct));
}
