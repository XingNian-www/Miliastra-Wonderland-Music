<template>
  <!-- 命令工作区：左侧分类导航 + 中间命令分组 + 右侧回复编辑器。 -->
  <div v-if="loading && !catalog" class="loading-card">正在读取命令目录…</div>
  <div v-else class="command-workspace">
    <nav class="command-reply-nav" aria-label="回复分类">
      <button
        v-for="item in categories"
        :key="item.value"
        type="button"
        class="btn small"
        :class="{ active: activeCategory === item.value }"
        @click="$emit('update:activeCategory', item.value)"
      >
        {{ item.label }} · {{ item.count }}
      </button>
    </nav>

    <div class="command-groups">
      <div
        v-for="group in visibleGroups"
        :key="group.name"
        class="command-group"
        :class="{ collapsed: collapsed.has(group.name) }"
      >
        <button class="command-group-head" type="button" :aria-expanded="!collapsed.has(group.name)" @click="$emit('toggle-group', group.name)">
          <span class="command-group-name">{{ group.name }}</span>
          <span class="command-group-count">{{ group.commands.length }} 条</span>
        </button>
        <div class="command-group-body">
          <CommandCatalogItem
            v-for="item in group.commands"
            :key="itemKey(item)"
            :item="item"
            :selected="isSelected(item)"
            :expanded="expanded.has(itemKey(item))"
            :show-hidden="showHidden"
            :saving="saving"
            :policy="getPolicy(item)"
            :policy-dirty="isPolicyDirty(item)"
            :role-options="roleOptions(item)"
            @select="$emit('select-command', $event)"
            @toggle-detail="$emit('toggle-detail', $event)"
            @policy-enabled="(target, event) => $emit('policy-enabled', target, event)"
            @policy-role="(target, event) => $emit('policy-role', target, event)"
          />
        </div>
      </div>
      <div v-if="!visibleCount" class="command-empty">
        没有匹配的命令；可清空搜索、放宽来源过滤，或勾选「显示隐藏命令」。
      </div>
    </div>

    <CommandReplyEditor
      :entry="selectedEntry"
      :selected-visible="selectedVisible"
      :rows="replyRows"
      :vars="replyVars"
      :max-chars="maxReplyChars"
      :status="replyStatus"
      :loading="replyLoading"
      :saving="saving"
      @refresh="$emit('refresh-replies')"
      @register-input="(index, el) => $emit('register-reply-input', index, el)"
      @focus-row="$emit('focus-reply-row', $event)"
      @input-row="(index, event) => $emit('input-reply-row', index, event)"
      @move-row="(index, delta) => $emit('move-row', index, delta)"
      @set-row-active="(index, active) => $emit('set-row-active', index, active)"
      @remove-row="$emit('remove-row', $event)"
      @insert-variable="$emit('insert-variable', $event)"
      @add-row="$emit('add-row')"
      @restore-defaults="$emit('restore-defaults')"
      @mute="$emit('mute')"
    />
  </div>
</template>

<script setup>
import CommandCatalogItem from './CommandCatalogItem.vue'
import CommandReplyEditor from './CommandReplyEditor.vue'

defineProps({
  loading: { type: Boolean, default: false },
  catalog: { type: Object, default: null },
  categories: { type: Array, default: () => [] },
  activeCategory: { type: String, default: 'all' },
  visibleGroups: { type: Array, default: () => [] },
  visibleCount: { type: Number, default: 0 },
  collapsed: { type: Object, required: true },
  expanded: { type: Object, required: true },
  showHidden: { type: Boolean, default: false },
  saving: { type: Boolean, default: false },
  itemKey: { type: Function, required: true },
  isSelected: { type: Function, required: true },
  getPolicy: { type: Function, required: true },
  isPolicyDirty: { type: Function, required: true },
  roleOptions: { type: Function, required: true },
  selectedEntry: { type: Object, default: null },
  selectedVisible: { type: Boolean, default: true },
  replyRows: { type: Array, default: () => [] },
  replyVars: { type: Array, default: () => [] },
  maxReplyChars: { type: Number, default: 300 },
  replyStatus: { type: String, default: '' },
  replyLoading: { type: Boolean, default: false },
})
defineEmits([
  'update:activeCategory',
  'toggle-group',
  'select-command',
  'toggle-detail',
  'policy-enabled',
  'policy-role',
  'refresh-replies',
  'register-reply-input',
  'focus-reply-row',
  'input-reply-row',
  'move-row',
  'set-row-active',
  'remove-row',
  'insert-variable',
  'add-row',
  'restore-defaults',
  'mute',
])
</script>
