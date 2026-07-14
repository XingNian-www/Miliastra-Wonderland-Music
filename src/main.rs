#[cfg(not(target_os = "windows"))]
fn main() {
    compile_error!("miliastra-wonderland-music only supports Windows.");
}

#[cfg(target_os = "windows")]
fn main() -> anyhow::Result<()> {
    miliastra_wonderland_music::app::run()
}
