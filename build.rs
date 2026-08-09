// 发布布局：EXE 同目录只保留 config.yaml 与主程序，动态库统一放入 deps/dll/。
// MNN.dll 是唯一的 PE 导入表硬依赖（加载器在 main() 之前解析），必须标记为
// 延迟加载（/DELAYLOAD + delayimp.lib），否则启动时找不到 DLL 直接失败。
// 延迟加载后，main() 里的 SetDllDirectoryW 先把 deps/dll/ 加入搜索路径，
// OCR 首次调用 MNN 时才解析；MNN.dll 的依赖（libwinpthread-1.dll）以及
// ffmpeg DLL（运行时动态加载）随后都能从 deps/dll/ 找到。
fn main() {
    println!("cargo:rustc-link-lib=delayimp");
    println!("cargo:rustc-link-arg=/DELAYLOAD:MNN.dll");
}
