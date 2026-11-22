// 发布布局：EXE 同目录只保留 config.yaml 与主程序，动态库统一放入 deps/dll/。
// MNN.dll 与 ffmpeg DLL（avcodec/avformat/avutil/swresample）都是 PE 导入表依赖，
// 必须全部标记为延迟加载（/DELAYLOAD + delayimp.lib），否则加载器在 main() 之前
// 解析导入表时只能从 EXE 目录/System32/PATH 找 DLL，找不到直接启动失败（0xC0000135）；
// 本机开发环境可能靠 PATH 里的 msys2 同版本 DLL 侥幸启动，发布到干净机器必挂。
// 延迟加载后，main() 里的 SetDllDirectoryW 先把 deps/dll/ 加入搜索路径，
// OCR/播放器首次调用对应 API 时才解析；MNN.dll 的依赖（MSVCP140/VCRUNTIME140）、
// avutil-60.dll 的依赖（libwinpthread-1.dll）以及后续依赖都能从 deps/dll/ 找到。
fn main() {
    // 前端产物：web/dist 存在时生成静态资源表；不存在时只发 cargo:warning 并回退空表
    // （见 generate_panel_assets 的说明——这里**不是**编译错误）。
    generate_panel_assets();
    // The delay-load directives below are MSVC/Windows-only. Skip them on other targets so the
    // crate (and its test harness) can be built for Linux test runs; Windows output is unchanged.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rustc-link-lib=delayimp");
    for library in [
        "MNN.dll",
        "avcodec-62.dll",
        "avformat-62.dll",
        "avutil-60.dll",
        "swresample-6.dll",
    ] {
        println!("cargo:rustc-link-arg=/DELAYLOAD:{library}");
    }
}

/// 把 web/dist 打平成 Rust 静态表，供 HTTP 路由直接返回。
///
/// 生成文件写入 OUT_DIR，主 crate 用 include! 引入；不使用第三方嵌入库，
/// 避免多一个依赖。
///
/// 缺产物时 emit `cargo:warning` 并回退空表（`PANEL_INDEX = None`），**构建照样成功**；
/// 运行期 `GET /` 会返回 503「面板未构建」。CI 靠 docker/前端步骤把 web/dist 先构建好
/// 来规避这条（见 .github/workflows/quality-gate.yml 的「校验前端产物」）。
///
/// 文本与二进制分流（2026-11-22）：Vite 会把 `web/src/assets` 里的 png / woff2 原样
/// 复制进 `web/dist/assets`，而 `include_str!` 只接受合法 UTF-8——过去对所有文件一律
/// `include_str!`，只要面板里出现一张图片或一个字体，构建就以 "invalid utf-8 sequence"
/// 失败。现在按扩展名决定嵌入方式：
///   - 文本（js/css/svg/json/html…）→ `include_str!`，进 `PANEL_ASSETS: &[(&str, &str, &str)]`；
///   - 二进制（png/woff2/ico/…）    → `include_bytes!`，进 `PANEL_BINARY_ASSETS: &[(&str, &[u8], &str)]`。
///
/// `PANEL_ASSETS` / `PANEL_INDEX` 的名称与类型保持不变（现有面板查询代码零改动即可编译）。
/// 二进制表目前**还没有读取方**——`src/interfaces/http/panel.rs` 的 `asset()` 返回 `&str`，
/// 服不出字节流；接上它需要改那个文件（不在本次改动范围）。因此这里对二进制条目发一条
/// 明确的 `cargo:warning`，而不是把它悄悄丢掉或让编译失败。
fn generate_panel_assets() {
    use std::path::{Path, PathBuf};

    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let dist = Path::new(&manifest).join("web").join("dist");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("panel_assets.rs");
    println!("cargo:rerun-if-changed=web/dist");
    println!("cargo:rerun-if-changed=web/src");
    println!("cargo:rerun-if-changed=web/index.html");

    if !dist.join("index.html").exists() {
        // 没有前端产物（本地只跑 Rust 测试/调试）时生成空表，`GET /` 返回 503；
        // 发布流程必须先 `npm run build`，CI 已内置。
        println!(
            "cargo:warning=web/dist 不存在：面板将不可用，发布前请构建前端（cd web && npm run build）"
        );
        // 三张表的定义必须与“产物存在”分支**完全一致**：否则 include! 之后
        // 引用 PANEL_BINARY_ASSETS 的代码会在无产物时编译失败，形成“有 dist 才能编译”的怪圈。
        std::fs::write(
            &out,
            concat!(
                "pub(crate) const PANEL_INDEX: Option<&str> = None;\n",
                "pub(crate) static PANEL_ASSETS: &[(&str, &str, &str)] = &[];\n",
                "#[allow(dead_code)]\n",
                "pub(crate) static PANEL_BINARY_ASSETS: &[(&str, &[u8], &str)] = &[];\n",
            ),
        )
        .expect("写入 panel_assets.rs");
        return;
    }

    fn rust_path(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    /// 二进制扩展名：这些文件必须走 include_bytes!。
    /// 用白名单而不是“先试 include_str 再回退”——build.rs 无法在生成期判断 UTF-8
    /// 合法性，而 include_str! 的失败发生在编译期、错误信息离这里很远。
    fn is_binary(name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        const BINARY_EXTENSIONS: &[&str] = &[
            ".png", ".jpg", ".jpeg", ".gif", ".webp", ".avif", ".ico", ".bmp", ".woff", ".woff2",
            ".ttf", ".otf", ".eot", ".mp3", ".wav", ".ogg", ".mp4", ".webm", ".wasm", ".pdf",
            ".zip", ".gz", ".br",
        ];
        BINARY_EXTENSIONS
            .iter()
            .any(|extension| lower.ends_with(extension))
    }

    fn mime_of(name: &str) -> &'static str {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".js") {
            "text/javascript; charset=utf-8"
        } else if lower.ends_with(".css") {
            "text/css; charset=utf-8"
        } else if lower.ends_with(".svg") {
            "image/svg+xml"
        } else if lower.ends_with(".png") {
            "image/png"
        } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
            "image/jpeg"
        } else if lower.ends_with(".gif") {
            "image/gif"
        } else if lower.ends_with(".webp") {
            "image/webp"
        } else if lower.ends_with(".ico") {
            "image/x-icon"
        } else if lower.ends_with(".woff2") {
            "font/woff2"
        } else if lower.ends_with(".woff") {
            "font/woff"
        } else if lower.ends_with(".ttf") {
            "font/ttf"
        } else if lower.ends_with(".json") || lower.ends_with(".map") {
            "application/json; charset=utf-8"
        } else {
            "application/octet-stream"
        }
    }

    // index.html 必须是 UTF-8：PANEL_INDEX 是 &str，也是 include_str! 的目标。
    // 这里显式读一次并给出可读的错误，避免编译期才报 "invalid utf-8 sequence"。
    let index = dist.join("index.html");
    if let Err(error) = std::fs::read_to_string(&index) {
        panic!(
            "web/dist/index.html 不是合法的 UTF-8 文本：{}（{error}）",
            index.display()
        );
    }

    let mut text_entries: Vec<String> = Vec::new();
    let mut binary_entries: Vec<String> = Vec::new();
    let mut stack = vec![dist.join("assets")];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for item in read.flatten() {
            let path = item.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let mime = mime_of(&name);
            let rel = path
                .strip_prefix(&dist)
                .expect("strip dist")
                .to_string_lossy()
                .replace('\\', "/");
            let source = rust_path(&path);
            if is_binary(&name) {
                binary_entries.push(format!(
                    "    (\"/{rel}\", include_bytes!(r#\"{source}\"#), \"{mime}\"),"
                ));
            } else {
                text_entries.push(format!(
                    "    (\"/{rel}\", include_str!(r#\"{source}\"#), \"{mime}\"),"
                ));
            }
        }
    }
    text_entries.sort();
    binary_entries.sort();

    let mut generated = format!(
        "pub(crate) const PANEL_INDEX: Option<&str> = Some(include_str!(r#\"{}\"#));\n",
        rust_path(&index)
    );
    generated.push_str(&format!(
        "pub(crate) static PANEL_ASSETS: &[(&str, &str, &str)] = &[\n{}\n];\n",
        text_entries.join("\n")
    ));
    // 这张表**总是**生成，即使为空：名字/类型固定下来，将来面板侧接上读取方时
    // 两种分支（有产物 / 无产物）都能编译。
    // allow(dead_code)：读取方（panel.rs 的 asset()）目前只认 &str，尚未接上这张表。
    // 接上之后应删掉这个 allow，否则会掩盖“表建了却没人用”的回归。
    generated.push_str(&format!(
        "#[allow(dead_code)]\npub(crate) static PANEL_BINARY_ASSETS: &[(&str, &[u8], &str)] = &[\n{}\n];\n",
        binary_entries.join("\n")
    ));
    if !binary_entries.is_empty() {
        println!(
            "cargo:warning=web/dist/assets 里有 {} 个二进制资源已用 include_bytes! 嵌入，\
但 src/interfaces/http/panel.rs 的 asset() 只返回 &str，暂时无法对外提供；\
要让它们可访问，需要给 panel.rs 增加一个返回 &[u8] 的查询并让 protocol.rs 按字节回包",
            binary_entries.len()
        );
    }
    std::fs::write(&out, generated).expect("写入 panel_assets.rs");
}
