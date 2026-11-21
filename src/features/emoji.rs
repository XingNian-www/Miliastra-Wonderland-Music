//! 二级聊天表情标签：`features` 层的兼容入口。
//!
//! 表情标签本质是**纯文本语法**（`{表情包:<页签序号>:<文字>}`），解析与数据模型
//! 已下沉到 [`crate::text::emoji`]：`text` 是叶子层，`ui`/`features`/`composition`
//! 都能直接复用，`ui` 再也不必反向依赖 `features`。
//!
//! 本模块**不再拥有任何实现**，只保留既有的 `crate::features::emoji::*` 路径
//! （AI 提示词目录、投递前过滤、HTTP 只读路由、配置段、回复模板都按这组路径引用），
//! 因此它是一次纯重导出：新增能力请直接加到 `crate::text::emoji`，不要在这里实现。
//!
//! 语法说明（用户 2026-11-09 定稿）见 [`crate::text::emoji`] 的模块文档。

pub use crate::text::emoji::*;
