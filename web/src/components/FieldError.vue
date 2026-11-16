<template>
  <div v-if="message" class="config-field-error" role="alert">{{ message }}</div>
</template>

<script setup>
defineProps({
  message: { type: String, default: '' },
})
</script>
