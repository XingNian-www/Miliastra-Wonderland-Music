<template>
  <!-- 左侧段导航：按 domain 分组 + 搜索。窄屏下退化成一条可横向滑动的段列表。 -->
  <aside class="config-sidebar" aria-label="配置段导航" title="窄屏可左右滑动查看更多配置段">
    <input
      v-model="search"
      type="text"
      placeholder="搜索段 / 字段（名称、路径、说明）"
      style="width: 180px; flex: 0 0 auto; margin-bottom: 6px"
      aria-label="搜索配置"
    />
    <div
      v-for="domain in domains"
      :key="domain.key"
      class="config-domain"
      :class="{ open: domain.open }"
    >
      <button
        type="button"
        class="config-domain-head"
        :aria-expanded="domain.open ? 'true' : 'false'"
        @click="$emit('toggle-domain', domain.key)"
      >
        <span v-html="highlight(domain.label)"></span>
        <span style="display: inline-flex; align-items: center; gap: 4px">
          <span class="config-domain-count">{{ domain.sections.length }}</span>
          <span v-if="domain.dirty" class="dirty-dot" title="该域有未保存修改"></span>
        </span>
      </button>
      <div class="config-domain-body">
        <button
          v-for="section in domain.sections"
          :key="section.name"
          type="button"
          class="config-section-item"
          :class="{ active: section.name === activeSectionName }"
          :aria-current="section.name === activeSectionName ? 'true' : undefined"
          @click="$emit('select-section', section.name)"
        >
          <span v-html="highlight(section.label)"></span>
          <span style="display: inline-flex; align-items: center; gap: 6px; flex: none">
            <span class="config-domain-count" :title="`${sectionFieldCount(section)} 个字段`">
              {{ sectionFieldCount(section) }}
            </span>
            <span class="section-name">{{ section.name }}</span>
            <span v-if="dirty[section.name]" class="dirty-dot" title="有未保存修改"></span>
          </span>
        </button>
      </div>
    </div>
    <div v-if="!domains.length" class="config-field-hint" style="padding: 8px">
      没有匹配的配置段
    </div>
  </aside>
</template>

<script setup>
import { computed } from 'vue'
import { state } from '../../store.js'
import { sectionFieldCount } from '../../util/configSchema.js'

const props = defineProps({
  domains: { type: Array, default: () => [] },
  activeSectionName: { type: String, default: '' },
  highlight: { type: Function, required: true },
})
defineEmits(['select-section', 'toggle-domain'])

const search = defineModel({ type: String, default: '' })

// 脏标记直接读全局 state：侧边栏是唯一需要「跨段汇总」的地方，不必再传一份副本。
const dirty = computed(() => state.dirty)
</script>
