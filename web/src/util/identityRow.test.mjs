// 身份映射 表格行 ↔ 后端 JSON 往返（B20-P0-1 的回归钉子）。
//
// 这个 bug 的形状是「字段语义反了但 UI 看起来完全正常」：表格列叫「游戏备注 / 显示昵称」，
// 后端 JSON 键叫 nickname / note，这两个英文名的字面直觉和真实语义**正好交叉**：
//   nickname（后端 remark，serde 内部名 remark）= 游戏内好友备注 = 屏幕显示名 = OCR 匹配键 / 授权依据
//   note（后端 display_name）                    = 软件内显示昵称 = 仅展示，不授权
// 所以测试的价值不在「往返能过」，而在**方向**：把 nickname 读成显示昵称、把 note 读成
// 备注，往返依然“守恒”，但数据已经错了。下面的断言全部写死到具体键名与具体值。

import test from 'node:test'
import assert from 'node:assert/strict'
import { accountsText, blankRow, parseAccounts, toMapping, toRow } from './identityRow.js'

test('toRow：nickname 落到「游戏备注」列，note 落到「显示昵称」列', () => {
  const row = toRow({
    id: 'm-1',
    role: 'friend',
    nickname: '屏幕上的名字',
    note: '软件里叫这个',
    real_nickname: '真实名',
    accounts: [{ game: 'genshin', uid: '100000001' }],
  })

  assert.equal(row.remark, '屏幕上的名字')
  assert.equal(row.displayName, '软件里叫这个')
  assert.equal(row.real_nickname, '真实名')
  assert.equal(row.accountsText, 'genshin:100000001')
  assert.equal(row.id, 'm-1')
  assert.equal(row.role, 'friend')
})

test('toMapping：写回后端键 nickname / note，方向与 toRow 相反', () => {
  const row = toRow({ id: 'm-1', nickname: '屏幕上的名字', note: '软件里叫这个' })
  const mapping = toMapping(row)

  assert.equal(mapping.nickname, '屏幕上的名字', '游戏备注必须写回 nickname')
  assert.equal(mapping.note, '软件里叫这个', '显示昵称必须写回 note')
  // 反向自证：绝不能写反
  assert.notEqual(mapping.nickname, '软件里叫这个')
  assert.notEqual(mapping.note, '屏幕上的名字')
  // 内部名不能被透传出内部列名
  assert.equal('remark' in mapping, false, '内部列名 remark 不得进入 payload（后端 deny_unknown_fields）')
  assert.equal('displayName' in mapping, false, '内部列名 displayName 不得进入 payload')
})

test('往返守恒：JSON → 行 → JSON 只改已知字段，未知字段原样带回', () => {
  const original = {
    id: 'm-1',
    role: 'admin',
    nickname: '屏幕上的名字',
    note: '软件里叫这个',
    real_nickname: '真实名',
    accounts: [
      { game: 'genshin', uid: '100000001' },
      { game: 'starrail', uid: '200000002' },
    ],
    // 将来才有的 / 系统写入的字段：过一遍表格不能丢
    remark_sourced: true,
    future_field: { nested: [1, 2, 3] },
  }

  const mapping = toMapping(toRow(original))

  assert.deepEqual(mapping, original)
})

test('空值语义：缺省 nickname/note 读成空串，保存时不会变成 undefined', () => {
  const row = toRow({ id: 'm-2' })
  assert.equal(row.remark, '')
  assert.equal(row.displayName, '')
  assert.equal(row.role, 'friend')

  const mapping = toMapping(row)
  assert.equal(mapping.nickname, '')
  assert.equal(mapping.note, '')
  assert.equal(mapping.id, 'm-2')
})

test('新增行：blankRow 的字段名与 toRow 一致，保存后键名不丢', () => {
  const row = blankRow()
  row.remark = '新备注'
  row.displayName = '新显示名'
  const mapping = toMapping(row)

  assert.equal(mapping.nickname, '新备注')
  assert.equal(mapping.note, '新显示名')
  assert.equal(mapping.role, 'friend')
  assert.equal(mapping.real_nickname, '')
  assert.deepEqual(mapping.accounts, [])
  // 未指定 id 时必须补一个，否则后端无法区分两条映射
  assert.equal(typeof mapping.id, 'string')
  assert.ok(mapping.id.length > 0)
})

test('accounts 文本 ↔ 列表：兼容简写、空项与不同平台', () => {
  assert.equal(accountsText([{ game: 'genshin', uid: '1' }, { game: 'starrail', uid: '2' }]), 'genshin:1, starrail:2')
  assert.equal(accountsText(null), '')
  assert.deepEqual(parseAccounts('genshin:1, starrail:2'), [
    { game: 'genshin', uid: '1' },
    { game: 'starrail', uid: '2' },
  ])
  // 只写 UID 视为原神
  assert.deepEqual(parseAccounts(' 100000001 '), [{ game: 'genshin', uid: '100000001' }])
  // 空项 / 纯空白被丢弃
  assert.deepEqual(parseAccounts('genshin:1,,   ,'), [{ game: 'genshin', uid: '1' }])
  assert.deepEqual(parseAccounts(''), [])
})
