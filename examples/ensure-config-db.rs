//! 生成并校验 SQLite 配置数据库（发布包预置用）。
//!
//! 用法: cargo run --example ensure-config-db -- <config.yaml 路径>
//!
//! 复用主程序启动链路（BootstrapConfig::load → ConfigStore::open → load_full → validate）：
//! 库不存在时按内置默认值完整初始化（revision=1，27 个配置段全部写入），
//! 已存在时校验可完整加载。这样发布目录删除全部功能 YAML 后，
//! 程序仍能从 playback.sqlite3 加载完整配置并正常启动。

fn main() -> anyhow::Result<()> {
    let config_path = std::env::args()
        .nth(1)
        .expect("用法: ensure-config-db <config.yaml 路径>");
    let config_path = std::path::Path::new(&config_path);
    let _restart_ms = miliastra_wonderland_music::watchdog_restart_ms(config_path)
        .map_err(|error| anyhow::anyhow!("配置库初始化或完整加载失败: {error:#}"))?;
    println!(
        "OK: 配置库已初始化并通过完整校验: {}",
        config_path.display()
    );
    Ok(())
}
