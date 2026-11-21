//! 播放器配置：凭据目录、登录助手与音频缓存。
//!
//! 队列、去重与匹配在各自的功能模块里；这里只保留播放器进程本身的配置。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::paths::resolve_path_with_default;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaybackConfig {
    pub credential_directory: PathBuf,
    pub login_helper_executable: PathBuf,
    pub login_timeout_ms: u64,
    /// 音频数据缓存配置；缺失或 enabled=false 时不启用。
    pub audio_cache: Option<AudioCacheFileConfig>,
}

/// 音频数据缓存（本地代理 + 磁盘缓存）的文件配置。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioCacheFileConfig {
    pub enabled: bool,
    /// 音频缓存文件目录。
    pub directory: PathBuf,
    /// 磁盘占用上限，单位 MiB。
    pub max_bytes_mb: u64,
    /// 同时进行的源站下载任务上限。
    pub max_concurrent_downloads: usize,
    /// 源站连接/响应超时，单位毫秒。
    pub request_timeout_ms: u64,
    /// 请求尚未下载完成的位置时，等待下载推进的最长时间，单位毫秒。
    pub seek_wait_timeout_ms: u64,
}

impl Default for PlaybackConfig {
    fn default() -> Self {
        Self {
            credential_directory: PathBuf::from("deps/data/credentials"),
            login_helper_executable: PathBuf::from("deps/bin/miliastra-login-helper.exe"),
            login_timeout_ms: 180000,
            audio_cache: None,
        }
    }
}

impl PlaybackConfig {
    pub(crate) fn normalize_audio_cache_paths(&mut self, executable_root: &Path) {
        let Some(audio_cache) = &mut self.audio_cache else {
            return;
        };
        resolve_path_with_default(
            executable_root,
            &mut audio_cache.directory,
            "deps/cache/audio",
        );
    }

    pub(super) fn validate(&self) -> Result<()> {
        if self.credential_directory.as_os_str().is_empty() {
            bail!("playback.credential_directory 不能为空");
        }
        if self.login_helper_executable.as_os_str().is_empty() {
            bail!("playback.login_helper_executable 不能为空");
        }
        if self.login_timeout_ms == 0 {
            bail!("playback.login_timeout_ms 必须大于 0");
        }
        if let Some(audio_cache) = &self.audio_cache {
            if !audio_cache.enabled {
                return Ok(());
            }
            if audio_cache.max_bytes_mb == 0 {
                bail!("playback.audio_cache.max_bytes_mb 必须大于 0");
            }
            if audio_cache.max_concurrent_downloads == 0 {
                bail!("playback.audio_cache.max_concurrent_downloads 必须大于 0");
            }
            if audio_cache.request_timeout_ms == 0 {
                bail!("playback.audio_cache.request_timeout_ms 必须大于 0");
            }
            if audio_cache.seek_wait_timeout_ms == 0 {
                bail!("playback.audio_cache.seek_wait_timeout_ms 必须大于 0");
            }
        }
        Ok(())
    }

    /// 转换成播放 crate 的运行时缓存配置；未启用时返回 None。
    /// `metadata_directory` 固定为统一数据库所在目录（state.playback_state_path
    /// 的父目录）：项目只有一个数据库，缓存元数据与配置共用 playback.sqlite3。
    pub(crate) fn audio_cache_runtime_config(
        &self,
        metadata_directory: &Path,
    ) -> Option<miliastra_playback::AudioCacheConfig> {
        let file = self.audio_cache.as_ref()?;
        if !file.enabled {
            return None;
        }
        Some(miliastra_playback::AudioCacheConfig {
            enabled: true,
            directory: file.directory.clone(),
            metadata_directory: metadata_directory.to_path_buf(),
            max_bytes: file.max_bytes_mb.saturating_mul(1024 * 1024),
            max_concurrent_downloads: file.max_concurrent_downloads,
            request_timeout: Duration::from_millis(file.request_timeout_ms),
            seek_wait_timeout: Duration::from_millis(file.seek_wait_timeout_ms),
            max_registry_entries: miliastra_playback::DEFAULT_MAX_REGISTRY_ENTRIES,
        })
    }
}
