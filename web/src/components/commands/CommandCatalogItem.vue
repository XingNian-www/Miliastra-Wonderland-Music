<template>
  <!-- 单条命令：标题/别名/徽标、摘要、权限控件、用法与示例展开。 -->
  <div
    class="command-item"
    :class="{
      'hidden-command': item.hidden && showHidden,
      disabled: item.enabled === false,
      selected: selected,
    }"
  >
    <div class="command-head" @click="$emit('select', item)">
      <div class="command-title">
        <button
          type="button"
          class="command-name command-select"
          :class="{ active: selected }"
          @click="$emit('select', item)"
        >
          {{ item.name || '（未命名命令）' }}
        </button>
        <span v-for="alias in item.aliases || []" :key="alias" class="command-alias">{{ alias }}</span>
      </div>
      <div class="command-badges">
        <span class="command-badge prefix">{{ item.prefix || '—' }}</span>
        <span v-for="source in sourceLabels(item)" :key="source" class="command-badge source">语法来源：{{ source }}</span>
        <span v-if="item.openEnded" class="command-badge open">会话中可自由输入</span>
        <span v-if="item.hidden" class="command-badge hidden-mark">隐藏</span>
        <span v-if="item.enabled === false" class="command-badge disabled-mark">已禁用</span>
        <span class="command-badge source">最低：{{ roleLabel(item.minimumRole) }}</span>
      </div>
    </div>
    <p class="command-summary">{{ item.summary || '（暂无摘要）' }}</p>

    <div v-if="item.editable" class="row wrap" style="margin-top: 6px">
      <label class="switch-line">
        <input
          type="checkbox"
          :checked="policy.enabled"
          :disabled="saving"
          @change="$emit('policy-enabled', item, $event)"
        />
        <span>启用</span>
      </label>
      <label>
        最低权限
        <select
          :value="policy.minimumRole"
          :disabled="saving"
          @change="$emit('policy-role', item, $event)"
        >
          <option
            v-for="role in roleOptions"
            :key="role.value"
            :value="role.value"
            :disabled="role.disabled"
          >
            {{ role.label }}
          </option>
        </select>
      </label>
      <span v-if="policyDirty" class="config-dirty">未保存</span>
      <span v-if="hasRoleFloor(item)" class="config-field-hint">
        权限下限 {{ roleLabel(item.minimumFloor) }}，不能降低；仍可单独禁用
      </span>
      <span v-if="item.lockedReason" class="config-field-hint">{{ item.lockedReason }}</span>
      <span v-if="!item.editable" class="config-field-hint">该条目描述会话输入，请修改对应玩法命令的权限</span>
    </div>

    <div class="command-toggle">
      <button class="btn small ghost" type="button" @click="$emit('toggle-detail', item)">
        {{ expanded ? '收起' : '查看' }}用法与示例
      </button>
    </div>
    <div v-if="expanded" class="command-detail">
      <div v-if="usageLines.length" class="command-detail-row">
        <span class="command-detail-label">用法</span>
        <div class="command-detail-lines">
          <code v-for="line in usageLines" :key="'u' + line">{{ line }}</code>
        </div>
      </div>
      <div v-if="exampleLines.length" class="command-detail-row">
        <span class="command-detail-label">示例</span>
        <div class="command-detail-lines">
          <code v-for="line in exampleLines" :key="'e' + line">{{ line }}</code>
        </div>
      </div>
      <div v-if="!usageLines.length && !exampleLines.length" class="command-detail-row command-detail-empty">
        后端未提供用法与示例
      </div>
      <div v-if="item.openEnded" class="command-detail-row command-detail-open">
        会话中可自由输入：玩法进行中接受任意输入，上列关键词只是入口或保留词。
      </div>
    </div>
  </div>
</template>

<script setup>
import { computed } from 'vue'
import { commandLines, hasRoleFloor, roleLabel, sourceLabels } from '../../util/commandCatalog.js'

const props = defineProps({
  item: { type: Object, required: true },
  selected: { type: Boolean, default: false },
  expanded: { type: Boolean, default: false },
  showHidden: { type: Boolean, default: false },
  saving: { type: Boolean, default: false },
  policy: { type: Object, required: true },
  policyDirty: { type: Boolean, default: false },
  roleOptions: { type: Array, default: () => [] },
})
defineEmits(['select', 'toggle-detail', 'policy-enabled', 'policy-role'])

const usageLines = computed(() => commandLines(props.item.usage))
const exampleLines = computed(() => commandLines(props.item.examples))
</script>
