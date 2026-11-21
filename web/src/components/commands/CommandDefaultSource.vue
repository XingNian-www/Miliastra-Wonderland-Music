<template>
  <!-- @点歌 默认平台：只影响搜索平台选择，不改变各平台命令权限。 -->
  <div class="row command-default-source">
    <label for="commandDefaultSongSource">@点歌默认平台</label>
    <select
      id="commandDefaultSongSource"
      :value="defaultSourceValue"
      :disabled="disabled || !(songPlatforms || []).length"
      @change="$emit('change', $event)"
    >
      <option v-for="item in songPlatforms || []" :key="item.value" :value="item.value">
        {{ item.label }}
      </option>
      <option v-if="unsupportedDefaultSource" :value="unsupportedDefaultSource" disabled>
        {{ platformLabel(unsupportedDefaultSource) }}（不能作为默认平台）
      </option>
    </select>
    <span class="config-field-hint">{{ hint }}</span>
  </div>
</template>

<script setup>
defineProps({
  defaultSourceValue: { type: String, default: '' },
  songPlatforms: { type: Array, default: () => [] },
  unsupportedDefaultSource: { type: String, default: '' },
  hint: { type: String, default: '' },
  disabled: { type: Boolean, default: false },
  platformLabel: { type: Function, required: true },
})
defineEmits(['change'])
</script>
