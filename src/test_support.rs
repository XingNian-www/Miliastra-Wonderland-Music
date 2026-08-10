use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use miliastra_contracts::{AtomicFileStore, StateStore};

/// 测试使用的最小文件状态存储，不属于稳定 crate API。
#[derive(Debug)]
struct TestStateStore;

impl AtomicFileStore for TestStateStore {
    fn write_atomic(&self, path: &Path, bytes: &[u8], description: &str) -> Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                anyhow::anyhow!("创建{description}目录失败: {}: {error}", parent.display())
            })?;
        }
        std::fs::write(path, bytes)
            .map_err(|error| anyhow::anyhow!("写入{description}失败: {}: {error}", path.display()))
    }
}

impl StateStore for TestStateStore {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        Ok(std::fs::read(path)?)
    }

    fn read_to_string(&self, path: &Path) -> Result<String> {
        Ok(std::fs::read_to_string(path)?)
    }
}

pub(crate) fn test_state_store() -> Arc<dyn StateStore> {
    Arc::new(TestStateStore)
}

/// 可注入写盘故障的状态存储：故障开启时 write_atomic 一律失败，
/// 用于验证持久化事务在写入失败时整体回滚（不产生部分写入）。
#[derive(Debug)]
pub(crate) struct FailingWriteStore {
    pub(crate) inner: Arc<dyn StateStore>,
    pub(crate) fail_writes: Arc<AtomicBool>,
}

impl FailingWriteStore {
    pub(crate) fn new(inner: Arc<dyn StateStore>) -> Self {
        Self {
            inner,
            fail_writes: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl AtomicFileStore for FailingWriteStore {
    fn write_atomic(&self, path: &Path, bytes: &[u8], description: &str) -> Result<()> {
        if self.fail_writes.load(Ordering::SeqCst) {
            bail!("注入的写盘故障: {}", description);
        }
        self.inner.write_atomic(path, bytes, description)
    }
}

impl StateStore for FailingWriteStore {
    fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path)
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        self.inner.read(path)
    }

    fn read_to_string(&self, path: &Path) -> Result<String> {
        self.inner.read_to_string(path)
    }
}
