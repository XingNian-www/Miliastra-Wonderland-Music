// 身份映射：表格行 ↔ 后端 JSON 的**唯一**换算层。
//
// 为什么要单独成文件（不要塞回 IdentityPage.vue 的 `<script setup>`）：
// 这里承载了 B20-P0-1 这个「字段语义反了但 UI 看起来完全正常」的 bug ——
// 表格内部按**列语义**命名（备注 / 显示昵称），后端 JSON 键名却叫 nickname / note，
// 两个名字的字面意思和实际语义正好交叉。只有把它做成纯函数才能用单测钉死方向：
//
//   remark（游戏备注 = 屏幕显示名 / OCR 匹配键 / 授权依据）  ↔  JSON nickname
//   displayName（软件内显示昵称，仅展示，不授权）            ↔  JSON note
//
// 后端契约见 technical-docs/identity-name-display-and-permission.md 与
// src/features/identity.rs（`#[serde(rename = "nickname")] pub remark`、
// `#[serde(rename = "note")] pub display_name`，且 `deny_unknown_fields`）。
// 键名不可改、形状不可改，只能在这一层换算。

/** 账号列表 → 表格里的 `game:uid, game:uid` 文本。 */
export function accountsText(accounts) {
  return (accounts || [])
    .filter((item) => item && (item.game || item.uid))
    .map((item) => `${item.game || 'genshin'}:${item.uid || ''}`)
    .join(', ')
}

/**
 * 表格文本 → 账号列表。
 * 兼容只写 UID 的简写（`123456` → `{game:'genshin', uid:'123456'}`），
 * UID 里带冒号也保留（`genshin:a:b` 的 uid 是 `a:b`，取第一个冒号前的当 game）。
 */
export function parseAccounts(text) {
  return String(text || '')
    .split(',')
    .map((part) => part.trim())
    .filter(Boolean)
    .map((part) => {
      const [game, ...rest] = part.split(':')
      const uid = rest.join(':').trim()
      return rest.length ? { game: game.trim() || 'genshin', uid } : { game: 'genshin', uid: game.trim() }
    })
    .filter((item) => item.uid)
}

/** 行 key 的自增序号：只用于模板 `:key`，与业务语义无关。 */
let rowSeq = 0

/**
 * 后端 JSON → 表格行。
 *
 * `raw` 保留整条原始对象：`toMapping` 以它为底再覆盖已知键，这样
 * `remark_sourced`、未知的将来字段都不会因为过一遍表格而丢失。
 */
export function toRow(mapping, index) {
  const source = mapping && typeof mapping === 'object' ? mapping : {}
  rowSeq += 1
  return {
    // key 只给模板用：用行号参与，保证同 id 的两行（JSON 里重复粘贴）也有唯一 key。
    key: `${source.id || 'row'}-${index == null ? rowSeq : index}-${rowSeq}`,
    id: source.id || '',
    role: source.role || 'friend',
    // 表格内部字段名按「列语义」命名，JSON 键名只在本文件换算：
    // remark（游戏备注）= JSON nickname；displayName（显示昵称）= JSON note。
    remark: source.nickname || '',
    displayName: source.note || '',
    real_nickname: source.real_nickname || '',
    accounts: Array.isArray(source.accounts) ? source.accounts.map((item) => ({ ...item })) : [],
    accountsText: accountsText(source.accounts),
    raw: { ...source },
    dirty: false,
  }
}

/** 新增行：字段名必须与 toRow 一致，否则新增的行保存时会丢字段。 */
export function blankRow() {
  rowSeq += 1
  return {
    key: `new-${Date.now()}-${rowSeq}`,
    id: '',
    role: 'friend',
    remark: '',
    displayName: '',
    real_nickname: '',
    accounts: [],
    accountsText: '',
    raw: {},
    dirty: true,
  }
}

/** 表格行 → 后端 JSON（`toRow` 的严格逆运算，未知字段原样带回）。 */
export function toMapping(row) {
  const mapping = { ...((row && row.raw) || {}) }
  mapping.role = row.role
  // 后端 JSON 键名不可改：游戏备注写 nickname，显示昵称写 note。
  mapping.nickname = row.remark
  mapping.note = row.displayName
  mapping.real_nickname = row.real_nickname
  mapping.accounts = parseAccounts(row.accountsText)
  if (!mapping.id) {
    const uuid = globalThis.crypto && typeof globalThis.crypto.randomUUID === 'function'
      ? globalThis.crypto.randomUUID()
      : `mapping-${Date.now()}-${rowSeq}`
    mapping.id = uuid
  }
  return mapping
}
