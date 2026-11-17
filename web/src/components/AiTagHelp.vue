<template>
  <section class="panel" style="margin-top:12px">
    <div class="panel-head">
      <h2>AI 标签与变量示例</h2>
      <small>标签原样写进回复才会生效；写错会被当成普通文字</small>
    </div>
    <div class="panel-body ai-tag-help">
      <div class="ai-tag-row">
        <code>{AI回复}</code>
        <div>
          <b>让模型补一句话</b>
          <p>回复模板里写 <code>{AI回复}</code>，生成时会替换成模型的一句应答；可用 <code>ai_reply.system_prompt</code> / <code>user_prompt</code> 约束风格。</p>
        </div>
      </div>

      <div class="ai-tag-row">
        <code>{表情包:2:傲娇}</code>
        <div>
          <b>发表情包</b>
          <p>格式 <code>{表情包:页签:文字}</code>：页签 <code>1</code> 是收藏，<code>2..6</code> 是角色页；第 7 个页签会把收藏页挤走，不支持。</p>
          <p>多个候选写 <code>{表情包:2:傲娇|得意|害羞}</code>，随机选一个；文字必须和游戏内表情名一致（符号会被忽略）。</p>
          <p>表情会独立成一条消息，所以标签放在整条回复末尾，不要插在句子中间。</p>
        </div>
      </div>

      <div class="ai-tag-row">
        <code>{歌名|歌曲名}</code>
        <div>
          <b>随机候选</b>
          <p>竖线分隔的多个值随机取一个；不支持嵌套标签。</p>
        </div>
      </div>

      <div class="ai-tag-row">
        <code>示例回复文字。{表情包:2:傲娇}</code>
        <div>
          <b>完整示例</b>
          <p>文字照常发送，表情作为单独一条紧随其后；标签缺失或写错不会中断文字。</p>
        </div>
      </div>

      <p class="config-field-hint">
        提示：AI 只能使用「全部表情目录」里已加入白名单的表情；未列出的标签会被剥离。
        标签不参与换行宽度计算，也不会被切成两段。
      </p>

      <h3 class="ai-tag-subtitle">回复模板变量（写在 ai_chat.answer_prompt 里）</h3>
      <table class="ai-tag-table">
        <thead>
          <tr><th>变量</th><th>实际内容</th><th>示例</th></tr>
        </thead>
        <tbody>
          <tr>
            <td><code>{{ braces('keyword') }}</code></td>
            <td>全局点歌关键词 / 角色名，来自 <code>ai_chat.keyword</code></td>
            <td><code>示例角色</code></td>
          </tr>
          <tr>
            <td><code>{{ braces('asker') }}</code></td>
            <td>本轮说话人的游戏昵称</td>
            <td><code>示例玩家</code></td>
          </tr>
          <tr>
            <td><code>{{ braces('facts') }}</code></td>
            <td>当前玩家的事实记忆（面板「AI 记忆」里维护）</td>
            <td><code>- 名字：示例玩家<br>- 喜欢：示例物品</code></td>
          </tr>
          <tr>
            <td><code>{{ braces('summary') }}</code></td>
            <td>与该玩家更早对话的压缩摘要</td>
            <td><code>之前聊过某个话题</code></td>
          </tr>
          <tr>
            <td><code>{{ braces('hall') }}</code></td>
            <td>大厅共享记忆：最近其他玩家/机器人发言，逐行 <code>[昵称]：内容</code></td>
            <td><code>[其他玩家]：今天有什么推荐</code></td>
          </tr>
          <tr>
            <td><code>{{ braces('bot_history') }}</code></td>
            <td>机器人自己最近在大厅说过的话（只用于避免重复，不代表对当前玩家说）</td>
            <td><code>你（示例角色）：示例回复</code></td>
          </tr>
          <tr>
            <td><code>{{ braces('history') }}</code></td>
            <td>较早的对话历史文本（正常由独立消息提供，模板里一般留空）</td>
            <td><code>示例玩家：示例消息</code></td>
          </tr>
          <tr class="ai-tag-deprecated">
            <td><code>{{ braces('question') }}</code> <code>{{ braces('recent_actions') }}</code> <code>{{ braces('commands') }}</code></td>
            <td>已废弃：会被替换成空串；问题行、动作、命令清单由系统在末尾统一追加一次</td>
            <td>不要依赖这些占位符</td>
          </tr>
        </tbody>
      </table>
      <p class="config-field-hint">
        未定义的占位符会原样保留（方便发现拼写错误）；模板为空时使用内置默认模板。
        <code>system_prompt</code> 不展开这些变量，变量只在 <code>answer_prompt</code> 生效。
      </p>
    </div>
  </section>
</template>

<script setup>
function braces(name) {
  return '{{' + name + '}}'
}
</script>

<style scoped>
.ai-tag-subtitle { margin: 6px 0 2px; font-size: 13px; }
.ai-tag-table { width: 100%; border-collapse: collapse; font-size: 11.5px; }
.ai-tag-table th, .ai-tag-table td { text-align: left; vertical-align: top; padding: 6px 8px; border-bottom: 1px solid var(--sep-2); }
.ai-tag-table th { color: var(--text-3); font-weight: 600; font-size: 11px; }
.ai-tag-table td code { color: var(--accent); }
.ai-tag-table td:nth-child(2) { color: var(--text-2); }
.ai-tag-deprecated td { color: var(--text-3); }
.ai-tag-deprecated td code { color: var(--text-3); text-decoration: line-through; }
.ai-tag-help { display: flex; flex-direction: column; gap: 10px; }
.ai-tag-row { display: grid; grid-template-columns: 210px minmax(0, 1fr); gap: 12px; align-items: start; padding-bottom: 10px; border-bottom: 1px dashed var(--sep-2); }
.ai-tag-row:last-of-type { border-bottom: 0; }
.ai-tag-row > code { display: inline-flex; align-items: center; padding: 4px 8px; border-radius: 8px; background: var(--bg-2); color: var(--accent); font-size: 12px; }
.ai-tag-row b { font-size: 12.5px; }
.ai-tag-row p { margin: 4px 0 0; color: var(--text-2); font-size: 11.5px; line-height: 1.55; }
.ai-tag-row code { font-family: var(--font-mono); }
@media (max-width: 720px) {
  .ai-tag-row { grid-template-columns: 1fr; gap: 6px; }
}
</style>
