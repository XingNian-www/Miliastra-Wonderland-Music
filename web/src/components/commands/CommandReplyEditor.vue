<template>
  <!-- 回复编辑器：按行编辑文案、排序、启停、插入变量，全部只写草稿。 -->
  <section class="command-reply-editor" aria-label="命令回复编辑">
    <template v-if="entry">
      <div class="row wrap" style="justify-content: space-between; align-items: baseline">
        <h3>
          {{ entry.name || entry.id }} 回复
          <span v-if="!selectedVisible" class="config-dirty" title="当前命令被搜索或来源筛选隐藏">不在当前筛选</span>
        </h3>
        <button class="btn small ghost" type="button" :disabled="saving || loading" @click="$emit('refresh')">刷新回复目录</button>
      </div>
      <p class="config-field-hint">
        {{ entry.editable === false ? '此命令的回复不可编辑。' : '多行文案按顺序逐条发送；留空使用内置默认；停用/静音控制是否回复。' }}
      </p>
      <template v-if="entry.editable !== false">
        <div class="command-reply-list">
          <article
            v-for="(row, index) in rows"
            :key="`${row.key || 'row'}:${index}`"
            class="command-reply-row"
            :class="{ inactive: row.active === false }"
          >
            <div class="row wrap command-reply-head">
              <strong>{{ row.label || row.key || '自定义回复' }}</strong>
              <code class="command-reply-key">{{ row.key || '自定义' }}</code>
              <span v-if="row.active === false" class="field-state">已停用</span>
            </div>
            <div v-if="row.defaultText != null" class="config-field-hint">内置默认：{{ row.defaultText }}</div>
            <textarea
              :ref="(el) => $emit('register-input', index, el)"
              rows="3"
              :maxlength="maxChars"
              :value="row.text"
              :placeholder="row.defaultText != null ? '留空则使用内置默认' : '输入回复文案；换行即分条发送'"
              :disabled="saving"
              :aria-label="`回复文案 ${row.key || index + 1}`"
              @focus="$emit('focus-row', index)"
              @input="$emit('input-row', index, $event)"
            ></textarea>
            <div class="command-reply-tools">
              <button class="btn small ghost" type="button" :disabled="saving || index === 0" @click="$emit('move-row', index, -1)">上移</button>
              <button class="btn small ghost" type="button" :disabled="saving || index === rows.length - 1" @click="$emit('move-row', index, 1)">下移</button>
              <button v-if="row.active === false" class="btn small" type="button" :disabled="saving" @click="$emit('set-row-active', index, true)">启用</button>
              <button v-else class="btn small ghost" type="button" :disabled="saving" @click="$emit('set-row-active', index, false)">停用</button>
              <button v-if="row.defaultText == null" class="btn small ghost" type="button" :disabled="saving" @click="$emit('remove-row', index)">删除</button>
            </div>
            <div v-if="vars.length" class="command-reply-vars" aria-label="插入变量">
              <button
                v-for="variable in vars"
                :key="variable.name"
                type="button"
                class="btn small ghost"
                :title="`${variable.label || variable.name}｜插入 {${variable.name}}（如 ${variable.example || variable.name}）`"
                :disabled="saving"
                @click="$emit('insert-variable', variable.name)"
              >
                {{ '{' + variable.name + '}' }}
              </button>
            </div>
          </article>
          <div v-if="!rows.length" class="command-empty">
            当前无回复；把文案留空表示使用内置默认，点“静音此命令”才会停止回复。
          </div>
        </div>
        <div class="row wrap" style="margin-top: 8px">
          <button class="btn small" type="button" :disabled="saving" @click="$emit('add-row')">添加回复</button>
          <button class="btn small" type="button" :disabled="saving" @click="$emit('restore-defaults')">恢复内置回复</button>
          <button class="btn small warn" type="button" :disabled="saving" @click="$emit('mute')">静音此命令</button>
        </div>
        <p class="config-field-hint">{{ status }}</p>
      </template>
    </template>
    <div v-else class="command-empty">
      <template v-if="loading">正在读取回复目录…</template>
      <template v-else>选择一个命令编辑回复</template>
    </div>
  </section>
</template>

<script setup>
defineProps({
  entry: { type: Object, default: null },
  selectedVisible: { type: Boolean, default: true },
  rows: { type: Array, default: () => [] },
  vars: { type: Array, default: () => [] },
  maxChars: { type: Number, default: 300 },
  status: { type: String, default: '' },
  loading: { type: Boolean, default: false },
  saving: { type: Boolean, default: false },
})
defineEmits([
  'refresh',
  'register-input',
  'focus-row',
  'input-row',
  'move-row',
  'set-row-active',
  'remove-row',
  'insert-variable',
  'add-row',
  'restore-defaults',
  'mute',
])
</script>
