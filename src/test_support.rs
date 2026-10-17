use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
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
