// 自定义回复 key 的命名空间归一化（W6a 的回归钉子）。
//
// 背景：后端 `features/reply_template/custom.rs::override_slot` 只认
// `custom.<命名空间>.<名字>` 前缀，命名空间匹配 `[a-z0-9_]`。而面板「添加回复」
// 原先直接生成 `custom:<时间戳>`，既有冒号又有时间戳 —— 保存成功但**永远查不到**，
// 即"点了保存、UI 显示已保存、实际不生效"。这类静默失效只能靠测试钉住。

/** 后端接受的命名空间字符集。 */
const ALLOWED = /[^a-z0-9_]/

/**
 * 由命令 id 派生合法命名空间。
 *
 * 命令 id 可能含 `@`、中文（如 `@点歌`）或大小写，逐字符归一化成 `_`，
 * 折叠连续下划线、去首尾下划线、截断到后端上限 32。
 */
export function customNamespaceFor(id) {
  const normalized = String(id ?? '')
    .toLowerCase()
    .replace(/[^a-z0-9_]/g, '_')
    .replace(/_+/g, '_')
    .replace(/^_+|_+$/g, '')
    .slice(0, 32)
  return normalized || 'custom'
}

/**
 * 生成一行新的自定义回复 key。
 *
 * @param {string} commandId 所属命令 id（用于派生命名空间）
 * @param {number} [stamp] 时间戳，缺省取当前时间；测试传入固定值以保证确定
 */
export function customReplyKey(commandId, stamp = Date.now()) {
  const namespace = customNamespaceFor(commandId)
  const name = stamp.toString(36).replace(ALLOWED, '_')
  return `custom.${namespace}.${name}`
}
