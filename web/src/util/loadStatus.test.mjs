// 容错加载状态的展示口径（W5 默认 Lenient 之后的面板契约）。
//
// 这些字段来自 `GET /config` 的 `loadMode`/`quarantinedCount`/`quarantined`，
// 是**只读诊断信息**：后端读不到时降级为 null，前端必须容忍缺失而不是整页报错。

import test from 'node:test'
import assert from 'node:assert/strict'
import { describeLoadStatus } from './loadStatus.js'

test('容错模式且有隔离条目：显示条数与可读原因', () => {
  const view = describeLoadStatus({
    loadMode: 'lenient',
    quarantinedCount: 2,
    quarantined: [
      { elementRef: 'identity.mappings[1]', reason: '账号重复' },
      { elementRef: 'command_access.overrides.@不存在的命令', reason: '未知命令ID' },
    ],
  })
  assert.equal(view.modeLabel, '容错')
  assert.equal(view.count, 2)
  assert.equal(view.hasQuarantine, true)
  assert.match(view.title, /identity\.mappings\[1\]/)
  assert.match(view.title, /@不存在的命令/)
})

test('容错模式但无隔离条目：不显示隔离徽标', () => {
  const view = describeLoadStatus({ loadMode: 'lenient', quarantinedCount: 0, quarantined: [] })
  assert.equal(view.modeLabel, '容错')
  assert.equal(view.hasQuarantine, false)
  assert.equal(view.count, 0)
})

test('严格模式：模式标签是「严格」，且不显示隔离', () => {
  const view = describeLoadStatus({ loadMode: 'strict', quarantinedCount: 0, quarantined: [] })
  assert.equal(view.modeLabel, '严格')
  assert.equal(view.hasQuarantine, false)
})

test('字段缺失（旧后端 / 读取失败降级为 null）时不报错、不显示', () => {
  for (const payload of [null, undefined, {}, { loadMode: null, quarantinedCount: null }]) {
    const view = describeLoadStatus(payload)
    assert.equal(view.modeLabel, '')
    assert.equal(view.hasQuarantine, false)
    assert.equal(view.count, 0)
    assert.equal(view.title, '')
  }
})

test('count 缺失时用条目数兜底，两者都缺则为 0', () => {
  assert.equal(describeLoadStatus({ quarantined: [{ elementRef: 'a' }] }).count, 1)
  assert.equal(describeLoadStatus({ quarantinedCount: 5 }).count, 5)
})
