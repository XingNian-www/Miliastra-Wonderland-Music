// W6a 回归：面板「添加回复」的 key 必须能被后端查到。
//
// 修复前的形状是静默失效：`addRow()` 生成 `custom:<时间戳>`，后端只认
// `custom.<命名空间>.<名字>`，于是保存成功、界面显示已保存、实际永远查不到。
// 断言写死到形态与字符集，而不是"能生成一个字符串"。

import test from 'node:test'
import assert from 'node:assert/strict'
import { customNamespaceFor, customReplyKey } from './replyKey.js'

test('命名空间从命令 id 归一化：去掉 @、中文、大小写，字符集只留 [a-z0-9_]', () => {
  assert.equal(customNamespaceFor('@点歌'), 'custom')
  assert.equal(customNamespaceFor('song.queue'), 'song_queue')
  assert.equal(customNamespaceFor('Playback'), 'playback')
  assert.equal(customNamespaceFor('a__b'), 'a_b')
  assert.equal(customNamespaceFor('__x__'), 'x')
  assert.equal(customNamespaceFor(''), 'custom')
  assert.equal(customNamespaceFor(null), 'custom')
  assert.ok(customNamespaceFor('x'.repeat(80)).length <= 32)
})

test('新增回复的 key 是三段式 custom.<命名空间>.<名字>，能被后端解析', () => {
  const key = customReplyKey('@点歌', 1_700_000_000_000)
  const parts = key.split('.')
  assert.equal(parts.length, 3, `key 必须是三段式：${key}`)
  assert.equal(parts[0], 'custom')
  assert.match(parts[1], /^[a-z0-9_]+$/, `命名空间只允许 [a-z0-9_]：${parts[1]}`)
  assert.match(parts[2], /^[a-z0-9_]+$/, `名字只允许 [a-z0-9_]：${parts[2]}`)
  assert.ok(!key.includes(':'), '绝不能再用冒号形态（后端只认点号命名空间）')
})

test('同命令两次新增得到不同 key（不会互相覆盖）', () => {
  const first = customReplyKey('@点歌', 1_700_000_000_000)
  const second = customReplyKey('@点歌', 1_700_000_001_000)
  assert.notEqual(first, second)
  assert.equal(first.split('.')[1], second.split('.')[1], '同命令应落在同一命名空间')
})
