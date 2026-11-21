//! 表情面板相关单测（原 `friend_delivery.rs` 内联的 `emoji_flow_tests`）。
//!
//! 断言、夹具与注释全部保持原样，只从父模块改为独立文件。

use image::{DynamicImage, Rgb, RgbImage, RgbaImage};

use super::emoji_panel::{
    EMOJI_GEAR_TEMPLATE, EMOJI_HEART_TEMPLATE, EMOJI_TRAY_DESIGN, emoji_grid_visible,
    emoji_tab_center, find_emoji_anchor, ordered_emoji_labels, select_emoji_candidate,
};
use crate::geometry::{Point, Rect};
use crate::text::emoji::EMOJI_TAB_TOTAL;

fn dark_frame() -> RgbaImage {
    RgbaImage::from_pixel(1920, 1080, image::Rgba([20, 20, 30, 255]))
}

fn overlay_template(bytes: &[u8], x: i64, y: i64) -> DynamicImage {
    let mut base = dark_frame();
    let template = image::load_from_memory(bytes)
        .expect("模板可解码")
        .to_rgba8();
    image::imageops::overlay(&mut base, &template, x, y);
    DynamicImage::ImageRgba8(base)
}

#[test]
fn finds_heart_anchor_by_template() {
    let image = overlay_template(EMOJI_HEART_TEMPLATE, 400, 900);
    let heart = find_emoji_anchor(&image, EMOJI_TRAY_DESIGN, 1.0).expect("应找到红心锚点");
    // 模板 36×28，中心在 (400+18, 900+14)
    assert!((heart.x - 418).abs() <= 2, "heart.x={}", heart.x);
    assert!((heart.y - 914).abs() <= 2, "heart.y={}", heart.y);
}

#[test]
fn falls_back_to_gear_template() {
    let image = overlay_template(EMOJI_GEAR_TEMPLATE, 300, 900);
    let heart = find_emoji_anchor(&image, EMOJI_TRAY_DESIGN, 1.0).expect("应退回齿轮锚点");
    // 齿轮 40×40 中心 (320,920)，加偏移 (94,-2)
    assert!((heart.x - 414).abs() <= 3, "heart.x={}", heart.x);
    assert!((heart.y - 918).abs() <= 3, "heart.y={}", heart.y);
}

#[test]
fn missing_anchor_returns_none() {
    let image = DynamicImage::ImageRgba8(dark_frame());
    assert!(find_emoji_anchor(&image, EMOJI_TRAY_DESIGN, 1.0).is_none());
}

#[test]
fn tab_centers_follow_fixed_pitch() {
    let heart = Point::new(424, 927);
    let scale = 1.0;
    let first = emoji_tab_center(heart, 2, scale).unwrap();
    assert_eq!(first.x, 534);
    let second = emoji_tab_center(heart, 3, scale).unwrap();
    assert_eq!(second.x, 645); // 110.5 取整
    let last = emoji_tab_center(heart, EMOJI_TAB_TOTAL, scale).unwrap();
    // 页签上限 6：最后一个角色页签中心 ≈ 976。
    assert!((last.x - 976).abs() <= 1, "last.x={}", last.x);
    assert_eq!(last.y, 927);
}

#[test]
fn tab_center_rejects_out_of_range() {
    let heart = Point::new(424, 927);
    assert!(emoji_tab_center(heart, 0, 1.0).is_none());
    assert!(emoji_tab_center(heart, EMOJI_TAB_TOTAL + 1, 1.0).is_none());
}

fn line(text: &str, x: i32, y: i32) -> crate::runtime::ocr::OcrLine {
    crate::runtime::ocr::OcrLine {
        text: text.to_string(),
        confidence: 0.99,
        bbox: Rect::new(x, y, 40, 20),
    }
}

#[test]
fn selects_exact_candidate_center() {
    let lines = vec![line("交给我吧", 100, 50), line("生气", 200, 50)];
    let candidates = vec!["交给我吧".to_string(), "吃惊".to_string()];
    let pick = select_emoji_candidate(&lines, &candidates, Point::new(300, 200), 1.0)
        .unwrap()
        .expect("应命中");
    assert_eq!((pick.x, pick.y), (300 + 120, 200 + 60));
}

#[test]
fn rejects_ambiguous_candidate() {
    let lines = vec![line("交给我吧", 100, 50), line("交给我吧", 200, 50)];
    let candidates = vec!["交给我吧".to_string()];
    assert_eq!(
        select_emoji_candidate(&lines, &candidates, Point::new(0, 0), 1.0).unwrap_err(),
        "emoji_ocr_ambiguous"
    );
}

#[test]
fn grid_visibility_uses_white_card_ratio() {
    let bright = DynamicImage::ImageRgb8(RgbImage::from_pixel(40, 40, Rgb([240, 240, 240])));
    let dark = DynamicImage::ImageRgb8(RgbImage::from_pixel(40, 40, Rgb([30, 40, 50])));
    let grid = Rect::new(5, 5, 20, 20);
    assert!(emoji_grid_visible(&bright, grid));
    assert!(!emoji_grid_visible(&dark, grid));
}

#[test]
fn ordered_labels_follow_grid_reading_order() {
    let lines = vec![
        line("第二行右", 300, 120),
        line("第二行左", 100, 118),
        line("第一行右", 300, 40),
        line("第一行左", 100, 42),
        line("第一行左", 105, 44), // 同行重复文字
        line("   ", 0, 0),
    ];
    assert_eq!(
        ordered_emoji_labels(&lines),
        vec!["第一行左", "第一行右", "第二行左", "第二行右"]
    );
}

fn returns_none_when_no_candidate_visible() {
    let lines = vec![line("别的表情", 100, 50)];
    let candidates = vec!["交给我吧".to_string()];
    assert!(
        select_emoji_candidate(&lines, &candidates, Point::new(0, 0), 1.0)
            .unwrap()
            .is_none()
    );
}
