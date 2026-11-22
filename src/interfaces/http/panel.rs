//! 前端构建产物（web/dist）的编译期嵌入。
//!
//! 构建脚本 build.rs 把 dist 打平成三张表：`PANEL_INDEX`（入口 HTML）、
//! `PANEL_ASSETS`（文本资源，`include_str!`）与 `PANEL_BINARY_ASSETS`
//! （png / woff2 等字节资源，`include_bytes!`）。这里只提供查询，不读磁盘——
//! 发布后的 exe 不依赖任何前端文件。

include!(concat!(env!("OUT_DIR"), "/panel_assets.rs"));

/// SPA 入口 HTML。
pub(crate) fn index_html() -> Option<&'static str> {
    PANEL_INDEX
}

/// 按请求路径找**文本**静态资源：返回 (内容, MIME)。
///
/// 只覆盖 `PANEL_ASSETS` 里的条目（js/css/svg/json…）；图片、字体等二进制资源
/// 不在这一侧，必须走 [`binary_asset`]。
pub(crate) fn asset(path: &str) -> Option<(&'static str, &'static str)> {
    PANEL_ASSETS
        .iter()
        .find(|(asset_path, _, _)| *asset_path == path)
        .map(|(_, body, mime)| (*body, *mime))
}

/// 按请求路径找**二进制**静态资源：返回 (字节, MIME)。
///
/// build.rs 把 png / woff2 / ico 等按扩展名分流到 `PANEL_BINARY_ASSETS`
/// （`include_bytes!`），它们不在 `PANEL_ASSETS` 里。响应层必须按字节回包，
/// 否则这些文件会被嵌进二进制却永远发不出去——只能靠 Vite 恰好把图片落进
/// `web/dist/assets` 时从磁盘读到。
pub(crate) fn binary_asset(path: &str) -> Option<(&'static [u8], &'static str)> {
    PANEL_BINARY_ASSETS
        .iter()
        .find(|(asset_path, _, _)| *asset_path == path)
        .map(|(_, body, mime)| (*body, *mime))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不存在的路径必须返回 None，且不能误命中文本表里的同名前缀/相似名。
    #[test]
    fn binary_asset_misses_unknown_paths() {
        assert!(binary_asset("/assets/definitely-not-embedded.png").is_none());
        assert!(binary_asset("/assets/definitely-not-embedded-secret").is_none());
        assert!(binary_asset("/").is_none());
        // 文本表里存在的条目（若有）不应从二进制表命中：两张表按扩展名互斥。
        for (text_path, _, _) in PANEL_ASSETS {
            assert!(
                binary_asset(text_path).is_none(),
                "文本资源不该被二进制表命中: {text_path}"
            );
        }
    }

    /// 文本资源仍走原查询路径，签名与返回值保持不变（现有调用方零改动）。
    ///
    /// `web/dist` 缺失时 `PANEL_ASSETS` 为空，这里没有可断言的条目——
    /// 用 `PANEL_ASSETS.is_empty()` 条件跳过，而不是造一条假资源。
    #[test]
    fn text_asset_still_serves_known_entries() {
        if PANEL_ASSETS.is_empty() {
            eprintln!("跳过：web/dist 未构建，PANEL_ASSETS 为空");
            return;
        }
        for (path, expected_body, expected_mime) in PANEL_ASSETS {
            let (body, mime) = asset(path).expect("PANEL_ASSETS 里的路径必须能被 asset() 查到");
            assert_eq!(body, *expected_body);
            assert_eq!(mime, *expected_mime);
        }
        assert!(asset("/assets/definitely-not-embedded.js").is_none());
    }

    /// 二进制表非空时，每一条都必须能被 `binary_asset` 查到且 MIME 非空；
    /// 为空时跳过（本地未构建含图片/字体的 `web/dist`）。
    #[test]
    fn binary_asset_serves_every_embedded_entry() {
        if PANEL_BINARY_ASSETS.is_empty() {
            eprintln!("跳过：web/dist/assets 没有二进制文件，PANEL_BINARY_ASSETS 为空");
            return;
        }
        for (path, expected_body, expected_mime) in PANEL_BINARY_ASSETS {
            let (body, mime) = binary_asset(path).expect("二进制表里的路径必须能被查到");
            assert_eq!(body, *expected_body);
            assert_eq!(mime, *expected_mime);
            assert!(!mime.is_empty(), "二进制资源缺 MIME: {path}");
        }
    }
}
