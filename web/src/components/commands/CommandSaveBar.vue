<template>
  <!-- 底部保存栏：权限与回复各有独立草稿与独立保存按钮。 -->
  <div class="command-savebar row wrap">
    <button class="btn primary" type="button" :disabled="saving || !policyDirtyCount" @click="$emit('save-policy')">
      {{ savingPolicy ? '保存中…' : `保存权限与平台（${policyDirtyCount}）` }}
    </button>
    <button class="btn" type="button" :disabled="saving || !policyDirtyCount" @click="$emit('discard-policy')">放弃权限草稿</button>
    <button class="btn primary" type="button" :disabled="saving || !replyDirtyCount" @click="$emit('save-replies')">
      {{ savingReplies ? '保存中…' : `保存回复（${replyDirtyCount}）` }}
    </button>
    <button class="btn" type="button" :disabled="saving || !replyDirtyCount" @click="$emit('discard-replies')">放弃回复草稿</button>
    <button
      class="btn"
      type="button"
      :disabled="saving || replyLoading || !repliesLoaded || !restorableReplyCount"
      title="按 GET /commands/replies 返回的 defaultReplies 逐个命令套回默认文案；只暂存草稿，点「保存回复」才写入"
      @click="$emit('restore-all-defaults')"
    >
      全部恢复内置回复（{{ restorableReplyCount }}）
    </button>
    <span class="config-field-hint">{{ notice }}</span>
  </div>
</template>

<script setup>
defineProps({
  saving: { type: Boolean, default: false },
  savingPolicy: { type: Boolean, default: false },
  savingReplies: { type: Boolean, default: false },
  policyDirtyCount: { type: Number, default: 0 },
  replyDirtyCount: { type: Number, default: 0 },
  restorableReplyCount: { type: Number, default: 0 },
  replyLoading: { type: Boolean, default: false },
  repliesLoaded: { type: Boolean, default: false },
  notice: { type: String, default: '' },
})
defineEmits([
  'save-policy',
  'discard-policy',
  'save-replies',
  'discard-replies',
  'restore-all-defaults',
])
</script>
