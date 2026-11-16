<template>
  <h1 class="page-title">高级控制</h1>
  <p class="page-subtitle">OCR、模板匹配与手动输入诊断工具</p>
  <div class="panel" style="margin:0 24px">
    <div class="panel-body">
      <p class="config-field-hint">诊断工具保留在独立页面（/tools），功能与旧版一致；嵌入下方使用。</p>
      <a class="btn small" href="/tools" target="_blank" rel="noreferrer">在新窗口打开</a>
    </div>
  </div>
  <iframe ref="frame" class="legacy-frame" src="/tools" style="height:calc(100vh - 220px)" @load="onLoad"></iframe>
</template>
