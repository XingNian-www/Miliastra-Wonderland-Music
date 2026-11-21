// 命令面板：目录与回复的静态词汇表与标签映射。
//
// 权限、来源、类别的文案都在这里，组件只负责渲染；调整文案不会碰到模板。

/** 权限档位，从低到高；顺序即「逐级包含」的顺序。 */
export const ROLES = ['user', 'friend', 'admin', 'owner']

export const ROLE_LABELS = { user: '普通用户', friend: '好友', admin: '管理员', owner: '主人' }

export const SOURCE_LABELS = { hall: '大厅成员', friend: '好友私聊' }

export const SOURCE_TABS = [
  { value: 'all', label: '全部来源' },
  { value: 'hall', label: '仅大厅' },
  { value: 'friend', label: '仅好友' },
]

export function roleLabel(value) {
  return ROLE_LABELS[value] || value || '普通用户'
}

/** 一条命令可能来自多个入口（大厅 / 好友私聊）；没有声明时按两者都支持处理。 */
export function sourceLabels(item) {
  const authorities = Array.isArray(item.authorities)
    ? item.authorities.map((value) => String(value).toLowerCase()).filter(Boolean)
    : []
  if (!authorities.length) return ['大厅成员 / 好友私聊']
  return authorities.map((value) => SOURCE_LABELS[value] || value)
}

/** 命令的稳定键：优先用后端 id，回退到「前缀 + 名称」。 */
export function itemKey(item) {
  return String(item?.id || `${item?.prefix || ''}${item?.name || ''}`)
}

/** 权限下限：只有高于 user 的下限才需要提示「不能降低」。 */
export function hasRoleFloor(item) {
  return Boolean(item.minimumFloor) && item.minimumFloor !== 'user'
}

/** 把 usage / examples 这类「字符串或字符串数组」统一成非空行列表。 */
export function commandLines(values) {
  const list = Array.isArray(values) ? values : values ? [values] : []
  return list.map((value) => String(value == null ? '' : value).trim()).filter(Boolean)
}
