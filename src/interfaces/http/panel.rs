//! 前端构建产物（web/dist）的编译期嵌入。
//!
//! 构建脚本 build.rs 把 dist 打平成 `PANEL_ASSETS` / `PANEL_INDEX`；
//! 这里只提供查询，不读磁盘——发布后的 exe 不依赖任何前端文件。

include!(concat!(env!("OUT_DIR"), "/panel_assets.rs"));

/// SPA 入口 HTML。
pub(crate) fn index_html() -> Option<&'static str> {
    PANEL_INDEX
}

/// 按请求路径找静态资源：返回 (内容, MIME)。
pub(crate) fn asset(path: &str) -> Option<(&'static str, &'static str)> {
    PANEL_ASSETS
        .iter()
        .find(|(asset_path, _, _)| *asset_path == path)
        .map(|(_, body, mime)| (*body, *mime))
}
