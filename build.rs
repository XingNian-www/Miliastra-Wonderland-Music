// 发布布局：EXE 同目录只保留 config.yaml 与主程序，动态库统一放入 deps/dll/。
// MNN.dll 与 ffmpeg DLL（avcodec/avformat/avutil/swresample）都是 PE 导入表依赖，
// 必须全部标记为延迟加载（/DELAYLOAD + delayimp.lib），否则加载器在 main() 之前
// 解析导入表时只能从 EXE 目录/System32/PATH 找 DLL，找不到直接启动失败（0xC0000135）；
// 本机开发环境可能靠 PATH 里的 msys2 同版本 DLL 侥幸启动，发布到干净机器必挂。
// 延迟加载后，main() 里的 SetDllDirectoryW 先把 deps/dll/ 加入搜索路径，
// OCR/播放器首次调用对应 API 时才解析；MNN.dll 的依赖（MSVCP140/VCRUNTIME140）、
// avutil-60.dll 的依赖（libwinpthread-1.dll）以及后续依赖都能从 deps/dll/ 找到。
fn main() {
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
