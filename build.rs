use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=vendor/mnn/3.6.0/windows-x64/bin/MNN.dll");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let dll_path = manifest_dir.join("vendor/mnn/3.6.0/windows-x64/bin/MNN.dll");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let profile_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("OUT_DIR should be under target/<profile>/build/<pkg>/out");

    fs::copy(&dll_path, profile_dir.join("MNN.dll"))
        .expect("failed to copy vendored MNN.dll next to the built executable");
}
