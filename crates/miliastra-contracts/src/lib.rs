use std::path::Path;

use anyhow::Result;

/// 以原子替换方式提交文件内容。
pub trait AtomicFileStore: Send + Sync {
    fn write_atomic(&self, path: &Path, bytes: &[u8], description: &str) -> Result<()>;
}

/// 功能层持久化所需的最小文件状态端口。
pub trait StateStore: AtomicFileStore + Send + Sync + std::fmt::Debug {
    fn exists(&self, path: &Path) -> bool;
    fn read(&self, path: &Path) -> Result<Vec<u8>>;
    fn read_to_string(&self, path: &Path) -> Result<String>;
}
