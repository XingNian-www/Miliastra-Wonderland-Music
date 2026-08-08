use std::env;
use std::path::PathBuf;

const WEBVIEW2_SDK_VERSION: &str = "1.0.4129.50";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("x86_64") {
        panic!("miliastra-login-helper only supports Windows x64");
    }

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let library = manifest_dir
        .join("../..")
        .join("vendor/webview2")
        .join(WEBVIEW2_SDK_VERSION)
        .join("x64/WebView2LoaderStatic.lib");
    if !library.is_file() {
        panic!(
            "missing pinned WebView2 static loader {} at {}",
            WEBVIEW2_SDK_VERSION,
            library.display()
        );
    }
    println!("cargo:rerun-if-changed={}", library.display());
    println!(
        "cargo:rustc-link-search=native={}",
        library.parent().unwrap().display()
    );
    println!("cargo:rustc-link-lib=static=WebView2LoaderStatic");
}
