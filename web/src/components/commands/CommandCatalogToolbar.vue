<template>
  <!-- 搜索 + 来源 Tab + 隐藏开关 + 计数：命令列表的工具条。 -->
  <div class="command-toolbar">
    <input v-model="search" type="text" placeholder="搜索命令名称 / 别名 / 摘要" aria-label="搜索命令" />
    <div class="tabs" role="group" aria-label="按来源过滤">
      <button
        v-for="tab in tabs"
        :key="tab.value"
        class="tab"
        :class="{ active: sourceFilter === tab.value }"
        type="button"
        @click="$emit('update:sourceFilter', tab.value)"
      >
        {{ tab.label }}
      </button>
    </div>
    <label class="switch-line">
      <input
        type="checkbox"
        :checked="showHidden"
        @change="$emit('update:showHidden', $event.target.checked)"
      />
      显示隐藏命令
    </label>
    <span class="command-toolbar-meta">
      显示 {{ visibleCount }} / {{ totalCount }} 条<span v-if="fetchedAt"> · {{ fetchedAt }}</span>
    </span>
  </div>
</template>

<script setup>
import { SOURCE_TABS } from '../../util/commandCatalog.js'

defineProps({
  sourceFilter: { type: String, default: 'all' },
  showHidden: { type: Boolean, default: false },
  visibleCount: { type: Number, default: 0 },
  totalCount: { type: Number, default: 0 },
  fetchedAt: { type: String, default: '' },
})
defineEmits(['update:sourceFilter', 'update:showHidden'])

const search = defineModel({ type: String, default: '' })
const tabs = SOURCE_TABS
</script>
