<template>
  <!-- 历史版本与回滚：details 折叠面板，展开时才拉取 /config/revisions。 -->
  <details
    id="configRevisionsPanel"
    ref="panelEl"
    class="config-group-card"
    style="margin-top: 16px"
    @toggle="$emit('toggle', $event)"
  >
    <summary class="config-group-head" style="cursor: pointer">历史版本与回滚</summary>
    <div style="padding: 12px">
      <div v-if="error" class="config-message err">{{ error }}</div>
      <div v-else-if="loading" class="config-field-hint">正在读取历史版本…</div>
      <div v-else-if="!revisions.length" class="config-field-hint">暂无历史版本</div>
      <div
        v-else
        class="history"
        style="display: grid; gap: 8px; max-height: 260px; overflow: auto"
      >
        <div v-for="item in revisions" :key="item.revision" class="history-item">
          <div>
            <span class="history-time">#{{ item.revision }}</span>
            <b>{{ formatTime(item.createdAtMs) }}</b>
            <span
              v-if="Number(item.revision) === currentRevision"
              class="field-state ok"
              style="margin-left: 8px"
            >
              当前
            </span>
          </div>
          <div class="history-result">
            <button
              type="button"
              class="btn small"
              :disabled="disabled || Number(item.revision) === currentRevision"
              @click="$emit('rollback', item)"
            >
              回滚到此版本
            </button>
          </div>
        </div>
      </div>
    </div>
  </details>
</template>

<script setup>
import { nextTick, ref } from 'vue'
import { formatTime } from '../../util/configSchema.js'

defineProps({
  revisions: { type: Array, default: () => [] },
  loading: { type: Boolean, default: false },
  error: { type: String, default: '' },
  disabled: { type: Boolean, default: false },
  currentRevision: { default: null },
})
defineEmits(['toggle', 'rollback'])

// 父级的「历史版本」按钮要能展开并滚到这里；保存成功后还要判断它是否展开着。
// 只暴露两个语义化方法，不把 DOM 节点漏出去。
const panelEl = ref(null)

function open() {
  const element = panelEl.value
  if (!element) return
  element.open = true
  nextTick(() => {
    if (typeof element.scrollIntoView === 'function') element.scrollIntoView({ block: 'end' })
  })
}

function isOpen() {
  return Boolean(panelEl.value && panelEl.value.open)
}

defineExpose({ open, isOpen })
</script>
