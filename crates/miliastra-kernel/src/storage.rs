//! 功能层持久化所需的最小文件状态端口。
//!
//! 这里只声明「把一份状态安全落盘」与「读回已有状态」两个能力，
//! 不规定存储介质。实现放在最外层的适配器里（例如原子替换的文件实现），
//! 垂直模块只依赖这些 trait，因此单测可以用内存或临时目录替身驱动。

use std::path::Path;

use anyhow::Result;

/// 以原子替换方式提交文件内容。
///
/// 语义要求：调用方要么看到完整的新内容，要么看到完整的旧内容，
/// 不允许出现「写了一半」的中间态。`description` 用于把失败定位到
/// 具体业务文件（例如「播放队列」），实现应原样带进错误信息。
pub trait AtomicFileStore: Send + Sync {
    fn write_atomic(&self, path: &Path, bytes: &[u8], description: &str) -> Result<()>;
}

/// 功能层持久化所需的最小文件状态端口。
///
/// 在 [`AtomicFileStore`] 之上补齐读取能力，构成业务模块读写状态文件的
/// 唯一入口。同一份状态只允许一个写者，因此实现不需要额外的并发控制。
pub trait StateStore: AtomicFileStore + Send + Sync + std::fmt::Debug {
    fn exists(&self, path: &Path) -> bool;
    fn read(&self, path: &Path) -> Result<Vec<u8>>;
    fn read_to_string(&self, path: &Path) -> Result<String>;
}
