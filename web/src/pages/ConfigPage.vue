<template>
  <div id="page-config" class="form-mac-page">
    <div v-if="loading" class="loading-card">正在载入配置…</div>
    <div v-else-if="loadError" class="config-message err">{{ loadError }}</div>
    <template v-else>
      <div class="config-status">
        <span class="chip"><span>版本</span><em>{{ state.revision == null ? '—' : state.revision }}</em></span>
        <span class="chip"><span>配置段</span><em>{{ sections.length }}</em></span>
        <span class="chip">
          <span>未保存</span>
          <em :class="{ 'config-dirty': dirtyCount > 0 }">{{ dirtyCount }} 段</em>
        </span>
        <!-- 容错加载状态：默认模式下坏条目会被隔离而不是让程序起不来，
             用户必须能在这里看到「我库里有几条坏数据、分别是什么」，
             否则「隔离」等于静默吞掉问题。 -->
        <span v-if="loadStatus.hasQuarantine" class="chip" :title="loadStatus.title">
          <span>已隔离</span>
          <em class="config-dirty">{{ loadStatus.count }} 条</em>
        </span>
        <span
          v-else-if="loadStatus.modeLabel"
          class="chip"
          title="配置加载模式：容错=坏条目被隔离后继续启动，严格=校验失败即拒绝启动"
        >
          <span>加载模式</span>
          <em>{{ loadStatus.modeLabel }}</em>
        </span>
        <span class="config-field-hint" style="margin-left: auto">
          隐藏 identity / ai / ai_chat / tts / command_access（各有独立页）{{ savedAt ? ' · 本次保存 ' + savedAt : '' }}
        </span>
      </div>

      <!-- 隔离详情：条数徽标只回答「有几条」，用户还需要知道「是哪一条、为什么、怎么修」。
           数据全是只读的，来源是 GET /config 的 quarantined[]（W5 的 config_quarantine 表）；
           面板不提供删除，修好数据后下次启动不再隔离。 -->
      <details v-if="loadStatus.hasQuarantine" class="config-message warn">
        <summary>
          有 {{ loadStatus.count }} 条配置条目被隔离（程序已按其余配置正常启动）
        </summary>
        <table class="config-quarantine">
          <thead>
            <tr><th>条目</th><th>原因</th></tr>
          </thead>
          <tbody>
            <tr v-for="(entry, index) in loadStatus.entries" :key="index">
              <td><code>{{ entry.ref }}</code></td>
              <td>{{ entry.reason }}</td>
            </tr>
          </tbody>
        </table>
        <p class="config-field-hint">
          修好对应数据后重启，这些条目就不会再被隔离。被隔离的条目不会生效，也不会影响其余配置。
        </p>
      </details>

      <div
        v-if="showVisualHint"
        class="config-message"
        style="margin-bottom:12px;display:flex;align-items:center;gap:10px;flex-wrap:wrap"
      >
        <span>画布、模板区域、阈值等完整视觉参数在「视觉档案」页集中编辑；这里只改档案名称等基础字段。</span>
        <button class="btn small" type="button" @click="openVisual">打开视觉档案</button>
      </div>

      <div class="config-layout">
        <ConfigSectionNav
          v-model="searchText"
          :domains="domains"
          :active-section-name="activeSectionName"
          :highlight="highlight"
          @select-section="selectSection"
          @toggle-domain="toggleDomain"
        />

        <div class="config-main">
          <div class="config-toolbar">
            <button type="button" class="btn" :disabled="busy" @click="reload">重新载入</button>
            <button type="button" class="btn" :disabled="busy" @click="openHistory">历史版本</button>
            <button
              type="button"
              class="btn"
              :disabled="busy || !activeSection"
              title="按 schema 默认值重建当前段（仅填入表单，仍需保存）"
              @click="restoreSectionDefaults"
            >
              恢复本段默认
            </button>
            <span v-if="searchQuery" class="config-search-count">
              搜索命中 {{ matchedFieldCount }} 个字段
            </span>
          </div>

          <div v-if="activeSection" class="config-section-head">
            <h2>{{ activeSection.label }}</h2>
            <code>{{ activeSection.name }}</code>
            <span class="config-section-stats">
              {{ visibleFields.length }} 项
              <span
                v-for="item in effectSummary"
                :key="item.key"
                class="config-effect"
                :class="item.key"
              >
                {{ item.label }} {{ item.count }}
              </span>
            </span>
          </div>

          <ConfigFieldGroups
            v-if="activeSection"
            :groups="groups"
            :visible-field-count="visibleFields.length"
            :get-value="getFieldValue"
            :get-error="errorFor"
            :is-disabled="isDisabled"
            :highlight="highlight"
            @submit="save"
            @update-field="setFieldValue"
            @invalid-field="setInvalid"
            @reset-field="resetField"
          >
            <!-- 表情段特判：全量表情目录（与 schema 字段同样只写草稿） -->
            <EmojiCatalog
              v-if="isEmojiSection"
              :allowed="emojiAllowed"
              :can-add="!!emojiAllowedField"
              :disabled="busy"
              @update:allowed="onEmojiAllowed"
            />
          </ConfigFieldGroups>

          <div v-if="activeSection" class="config-savebar">
            <span class="config-savebar-hint" :class="{ 'config-dirty': dirtyCount > 0 }">
              {{ dirtyCount ? `有未保存的修改（${dirtyCount} 段）` : '未修改' }}
            </span>
            <button type="button" class="btn" :disabled="busy || !dirtyCount" @click="discardChanges">
              放弃修改
            </button>
            <button
              type="button"
              class="btn primary"
              :disabled="busy || !dirtyCount || hasLocalErrors"
              @click="save"
            >
              {{ saving ? '保存中…' : '保存修改' }}
            </button>
          </div>

          <ConfigRevisionPanel
            ref="historyPanelRef"
            :revisions="revisions"
            :loading="revisionsLoading"
            :error="revisionsError"
            :disabled="busy"
            :current-revision="currentRevision"
            @toggle="onHistoryToggle"
            @rollback="rollback"
          />
        </div>
      </div>
    </template>
  </div>
</template>

<script setup>
import { computed } from 'vue'
import { describeLoadStatus } from '../util/loadStatus.js'
import { state } from '../store.js'
import ConfigSectionNav from '../components/config/ConfigSectionNav.vue'
import ConfigFieldGroups from '../components/config/ConfigFieldGroups.vue'
import ConfigRevisionPanel from '../components/config/ConfigRevisionPanel.vue'
import EmojiCatalog from '../components/EmojiCatalog.vue'
import { useConfigForm } from '../composables/useConfigForm.js'

const {
  loading,
  loadError,
  historyPanelRef,
  searchText,
  searchQuery,
  activeSectionName,
  sections,
  activeSection,
  showVisualHint,
  visibleFields,
  groups,
  domains,
  matchedFieldCount,
  effectSummary,
  selectSection,
  toggleDomain,
  highlight,
  dirtyCount,
  hasLocalErrors,
  getFieldValue,
  setFieldValue,
  setInvalid,
  errorFor,
  isDisabled,
  resetField,
  restoreSectionDefaults,
  saving,
  savedAt,
  busy,
  save,
  reload,
  discardChanges,
  revisions,
  revisionsLoading,
  revisionsError,
  currentRevision,
  onHistoryToggle,
  openHistory,
  rollback,
  isEmojiSection,
  emojiAllowed,
  emojiAllowedField,
  onEmojiAllowed,
  openVisual,
} = useConfigForm()

// 容错加载状态取自 `GET /config` 的附加字段（后端 `load_status_json`）。
// 口径与降级逻辑抽在 `util/loadStatus.js`，这里只做接线。
const loadStatus = computed(() => describeLoadStatus(state?.config))
</script>