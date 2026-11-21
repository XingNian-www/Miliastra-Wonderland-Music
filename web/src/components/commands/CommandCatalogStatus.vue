<template>
  <!-- 命令目录状态条：全局开关、默认平台、目录版本与刷新入口。 -->
  <div class="command-status">
    <span class="chip" :data-state="catalogState">
      <i class="dot"></i><span>全局命令</span><em>{{ commandsEnabledLabel }}</em>
    </span>
    <span class="chip"><span>默认平台</span><em>{{ platformLabel(defaultSourceValue) || '—' }}</em></span>
    <span class="config-field-hint">{{ catalogMeta }}</span>
    <span v-if="policyDirtyCount || replyDirtyCount" class="config-dirty">
      未保存：权限 {{ policyDirtyCount }} · 回复 {{ replyDirtyCount }}
    </span>
    <button class="btn small" :disabled="catalogLoading || saving" @click="$emit('refresh')">刷新目录</button>
  </div>
</template>

<script setup>
defineProps({
  catalogState: { type: String, default: '' },
  commandsEnabledLabel: { type: String, default: '' },
  defaultSourceValue: { type: String, default: '' },
  catalogMeta: { type: String, default: '' },
  platformLabel: { type: Function, required: true },
  policyDirtyCount: { type: Number, default: 0 },
  replyDirtyCount: { type: Number, default: 0 },
  catalogLoading: { type: Boolean, default: false },
  saving: { type: Boolean, default: false },
})
defineEmits(['refresh'])
</script>
