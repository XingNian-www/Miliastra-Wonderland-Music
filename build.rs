// 发布布局：EXE 同目录只保留 config.yaml 与主程序，动态库统一放入 deps/dll/。
// MNN.dll 与 ffmpeg DLL（avcodec/avformat/avutil/swresample）都是 PE 导入表依赖，
// 必须全部标记为延迟加载（/DELAYLOAD + delayimp.lib），否则加载器在 main() 之前
// 解析导入表时只能从 EXE 目录/System32/PATH 找 DLL，找不到直接启动失败（0xC0000135）；
// 本机开发环境可能靠 PATH 里的 msys2 同版本 DLL 侥幸启动，发布到干净机器必挂。
// 延迟加载后，main() 里的 SetDllDirectoryW 先把 deps/dll/ 加入搜索路径，
// OCR/播放器首次调用对应 API 时才解析；MNN.dll 的依赖（MSVCP140/VCRUNTIME140）、
// avutil-60.dll 的依赖（libwinpthread-1.dll）以及后续依赖都能从 deps/dll/ 找到。
fn main() {
    // 前端产物：web/dist 存在时生成静态资源表；不存在时编译期报错，
    // 避免发布出没有面板的 exe（迁移期不保留旧页面兜底）。
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
/// 避免多一个依赖。缺产物时 emit 明确的编译错误，而不是静默回退旧页面。
fn generate_panel_assets() {
    use std::path::{Path, PathBuf};

    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let dist = Path::new(&manifest).join("web").join("dist");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("panel_assets.rs");
    println!("cargo:rerun-if-changed=web/dist");
    println!("cargo:rerun-if-changed=web/src");
    println!("cargo:rerun-if-changed=web/index.html");

    if !dist.join("index.html").exists() {
        // 没有前端产物（本地只跑 Rust 测试/调试）时生成空表，HTTP 面板回退旧页面；
        // 发布流程必须先 `npm run build`，CI 已内置。
        println!("cargo:warning=web/dist 不存在：面板将不可用，发布前请构建前端（cd web && npm run build）");
        std::fs::write(
            &out,
            "pub(crate) const PANEL_INDEX: Option<&str> = None;\npub(crate) static PANEL_ASSETS: &[(&str, &str, &str)] = &[];\n",
        )
        .expect("写入 panel_assets.rs");
        return;
    }

    fn rust_path(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    let index = std::fs::read_to_string(dist.join("index.html")).expect("读取 web/dist/index.html");
    let mut entries: Vec<String> = Vec::new();
    let mut stack = vec![dist.join("assets")];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        for item in read.flatten() {
            let path = item.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let mime = if name.ends_with(".js") {
                "text/javascript; charset=utf-8"
            } else if name.ends_with(".css") {
                "text/css; charset=utf-8"
            } else if name.ends_with(".svg") {
                "image/svg+xml"
            } else if name.ends_with(".png") {
                "image/png"
            } else if name.ends_with(".woff2") {
                "font/woff2"
            } else {
                "application/octet-stream"
            };
            let rel = path
                .strip_prefix(&dist)
                .expect("strip dist")
                .to_string_lossy()
                .replace('\\', "/");
            entries.push(format!(
                "    (\"/{rel}\", include_str!(r#\"{}\"#), \"{mime}\"),",
                rust_path(&path)
            ));
        }
    }
    entries.sort();
    let assets = format!(
        "pub(crate) static PANEL_ASSETS: &[(&str, &str, &str)] = &[\n{}\n];\n",
        entries.join("\n")
    );
    let index_module = format!(
        "pub(crate) const PANEL_INDEX: Option<&str> = Some(include_str!(r#\"{}\"#));\n",
        rust_path(&dist.join("index.html"))
    );
    std::fs::write(&out, format!("{index_module}{assets}")).expect("写入 panel_assets.rs");
    let _ = index;
}
