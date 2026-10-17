use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

use miliastra_contracts::{AtomicFileStore, StateStore};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Default)]
pub(crate) struct FsStateStore;

impl AtomicFileStore for FsStateStore {
    fn write_atomic(&self, path: &Path, bytes: &[u8], description: &str) -> Result<()> {
        write_atomic(path, bytes, description)
    }
}

impl StateStore for FsStateStore {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        fs::read(path).with_context(|| format!("读取状态文件失败: {}", path.display()))
    }

    fn read_to_string(&self, path: &Path) -> Result<String> {
        fs::read_to_string(path).with_context(|| format!("读取状态文件失败: {}", path.display()))
    }
}

/// 写入同目录临时文件并以替换方式提交，避免进程中断留下半份业务文件。
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], description: &str) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建{description}目录失败: {}", parent.display()))?;
    }

    let temporary = temporary_path(path);
    let result = (|| -> Result<()> {
        let mut file = fs::File::create(&temporary)
            .with_context(|| format!("创建{description}临时文件失败: {}", temporary.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("写入{description}临时文件失败: {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("同步{description}临时文件失败: {}", temporary.display()))?;
        drop(file);
        replace_file(&temporary, path, description)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn temporary_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("state"));
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let mut name = file_name.to_os_string();
    name.push(format!(".{}.{}.tmp", std::process::id(), id));
    path.with_file_name(name)
}

#[cfg(windows)]
fn replace_file(temporary: &Path, target: &Path, description: &str) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    let temporary = temporary
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            PCWSTR(temporary.as_ptr()),
            PCWSTR(target.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .with_context(|| format!("替换{description}失败"))
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, target: &Path, description: &str) -> Result<()> {
    fs::rename(temporary, target).with_context(|| {
        format!(
            "替换{description}失败: {} -> {}",
            temporary.display(),
            target.display()
        )
    })
}
