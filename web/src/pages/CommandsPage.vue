<template>
  <div id="page-commands" class="commands-mac-page">
    <h1 class="page-title">命令面板</h1>
    <p class="page-subtitle">聊天命令目录、回复覆盖与权限策略；权限与回复分别保存</p>

    <CommandCatalogStatus
      :catalog-state="catalogState"
      :commands-enabled-label="commandsEnabledLabel"
      :default-source-value="defaultSourceValue"
      :catalog-meta="catalogMeta"
      :platform-label="platformLabel"
      :policy-dirty-count="policyDirtyCount"
      :reply-dirty-count="replyDirtyCount"
      :catalog-loading="catalogLoading"
      :saving="saving"
      @refresh="refreshCatalog"
    />

    <CommandDefaultSource
      :default-source-value="defaultSourceValue"
      :song-platforms="catalog?.songPlatforms || []"
      :unsupported-default-source="unsupportedDefaultSource"
      :hint="defaultSourceHint"
      :disabled="saving || !catalog"
      :platform-label="platformLabel"
      @change="onDefaultSourceChange"
    />
    <p class="config-field-hint">
      默认平台只影响 @点歌 走哪个平台的搜索，不改变各平台命令的权限；B站 请直接发 @B站点歌。
    </p>

    <CommandCatalogToolbar
      v-model="searchText"
      :source-filter="sourceFilter"
      :show-hidden="showHidden"
      :visible-count="visibleCount"
      :total-count="totalCount"
      :fetched-at="fetchedAt"
      @update:source-filter="sourceFilter = $event"
      @update:show-hidden="showHidden = $event"
    />

    <div v-if="catalogError || policyError" class="config-message err" role="status">
      {{ catalogError || policyError }}
    </div>
    <div v-if="replyError" class="config-message err" role="status">{{ replyError }}</div>

    <CommandWorkspace
      v-model:active-category="activeCategory"
      :loading="catalogLoading"
      :catalog="catalog"
      :categories="categories"
      :visible-groups="visibleGroups"
      :visible-count="visibleCount"
      :collapsed="collapsed"
      :expanded="expanded"
      :show-hidden="showHidden"
      :saving="saving"
      :item-key="itemKey"
      :is-selected="isSelected"
      :get-policy="policyValue"
      :is-policy-dirty="isPolicyDirty"
      :role-options="roleOptions"
      :selected-entry="selectedEntry"
      :selected-visible="selectedVisible"
      :reply-rows="replyRows"
      :reply-vars="replyVars"
      :max-reply-chars="maxReplyChars"
      :reply-status="replyStatus"
      :reply-loading="replyLoading"
      @toggle-group="toggleGroup"
      @select-command="selectCommand"
      @toggle-detail="toggleDetail"
      @policy-enabled="onPolicyEnabled"
      @policy-role="onPolicyRole"
      @refresh-replies="refreshReplies"
      @register-reply-input="setReplyInput"
      @focus-reply-row="lastFocusedRow = $event"
      @input-reply-row="onReplyInput"
      @move-row="moveRow"
      @set-row-active="setRowActive"
      @remove-row="removeRow"
      @insert-variable="insertVariable"
      @add-row="addRow"
      @restore-defaults="restoreDefaults"
      @mute="muteCommand"
    />

    <CommandSaveBar
      :saving="saving"
      :saving-policy="savingPolicy"
      :saving-replies="savingReplies"
      :policy-dirty-count="policyDirtyCount"
      :reply-dirty-count="replyDirtyCount"
      :restorable-reply-count="restorableReplyCount"
      :reply-loading="replyLoading"
      :replies-loaded="!!replies"
      :notice="policyNotice"
      @save-policy="savePolicy"
      @discard-policy="discardPolicy"
      @save-replies="saveReplies"
      @discard-replies="discardReplies"
      @restore-all-defaults="restoreAllReplyDefaults"
    />
    <p class="config-field-hint">
      最低权限按普通用户 → 好友 → 管理员 → 主人逐级包含；来源与权限分开；权限与回复各有独立保存按钮。
    </p>
  </div>
</template>

<script setup>
import CommandCatalogStatus from '../components/commands/CommandCatalogStatus.vue'
import CommandDefaultSource from '../components/commands/CommandDefaultSource.vue'
import CommandCatalogToolbar from '../components/commands/CommandCatalogToolbar.vue'
import CommandWorkspace from '../components/commands/CommandWorkspace.vue'
import CommandSaveBar from '../components/commands/CommandSaveBar.vue'
import { useCommandsConfig } from '../composables/useCommandsConfig.js'

const {
  catalog,
  catalogLoading,
  catalogError,
  commandsEnabledLabel,
  catalogState,
  catalogMeta,
  categories,
  visibleGroups,
  visibleCount,
  totalCount,
  selectedEntry,
  selectedVisible,
  fetchedAt,
  searchText,
  sourceFilter,
  showHidden,
  activeCategory,
  collapsed,
  expanded,
  isSelected,
  selectCommand,
  toggleGroup,
  toggleDetail,
  itemKey,
  refreshCatalog,
  savingPolicy,
  policyDirtyCount,
  policyError,
  defaultSourceValue,
  unsupportedDefaultSource,
  defaultSourceHint,
  platformLabel,
  policyValue,
  isPolicyDirty,
  roleOptions,
  onPolicyEnabled,
  onPolicyRole,
  onDefaultSourceChange,
  savePolicy,
  discardPolicy,
  replies,
  replyLoading,
  replyError,
  replyRows,
  replyVars,
  replyStatus,
  maxReplyChars,
  replyDirtyCount,
  restorableReplyCount,
  savingReplies,
  lastFocusedRow,
  setReplyInput,
  onReplyInput,
  setRowActive,
  moveRow,
  removeRow,
  addRow,
  restoreDefaults,
  restoreAllReplyDefaults,
  muteCommand,
  insertVariable,
  saveReplies,
  discardReplies,
  refreshReplies,
  saving,
  policyNotice,
} = useCommandsConfig()
</script>
