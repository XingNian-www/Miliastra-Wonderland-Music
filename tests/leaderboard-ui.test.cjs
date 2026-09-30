// 运行：node --test tests/leaderboard-ui.test.cjs；不连接游戏或真实 HTTP 服务。
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const html = fs.readFileSync(path.join(__dirname, '../src/interfaces/http/page.html'), 'utf8');
const start = html.indexOf('let leaderboardTask=');
const end = html.indexOf('let cacheTracksLoading=', start);
assert.ok(start >= 0 && end > start, 'leaderboard script must exist');
const lines = html.split(String.fromCharCode(10));
const helpers = ['const esc=', 'const safeNum='].map(prefix => {
  const line = lines.find(line => line.startsWith(prefix));
  assert.ok(line, 'missing page helper: ' + prefix);
  return line;
}).join(String.fromCharCode(10));

function harness() {
  const elements = {};
  for (const id of ['leaderboard', 'leaderboardMeta', 'leaderboardRange']) {
    let text = '', markup = '';
    elements[id] = { value: '7' };
    Object.defineProperties(elements[id], {
      textContent: { get: () => text, set: value => { text = String(value); markup = ''; } },
      innerHTML: { get: () => markup, set: value => { markup = String(value); text = ''; } },
    });
  }
  const requests = [], timers = new Map();
  let nextTimer = 1;
  const context = vm.createContext({
    $: id => elements[id],
    AbortController,
    setTimeout: callback => { const id = nextTimer++; timers.set(id, callback); return id; },
    clearTimeout: id => timers.delete(id),
    request: (url, method, signal) => new Promise((resolve, reject) => {
      requests.push({ url, resolve, reject, signal });
      if (signal) signal.addEventListener('abort', () => reject(new Error('aborted')), { once: true });
    }),
  });
  vm.runInContext(helpers + String.fromCharCode(10) + html.slice(start, end), context);
  return { elements, requests, timers, load: () => context.loadLeaderboard(), render: data => context.renderLeaderboard(data) };
}
const flush = () => new Promise(resolve => setImmediate(resolve));
const page = (days, requester = 'Alice') => ({ days, limit: 20, entries: [{ requester, count: 3, lastRequestedAtMs: 1700000000000 }] });

test('rapid range switches discard stale response and fetch only latest range', async () => {
  const h = harness();
  const first = h.load();
  h.elements.leaderboardRange.value = '1';
  assert.equal(h.load(), first);
  h.elements.leaderboardRange.value = '30';
  h.load();
  assert.equal(h.requests.length, 1);
  h.requests[0].resolve(page(7, 'stale-user'));
  await flush();
  assert.equal(h.requests.length, 2);
  assert.equal(h.requests[1].url, '/playback/leaderboard?days=30');
  assert.doesNotMatch(h.elements.leaderboard.innerHTML, /stale-user/);
  h.requests[1].resolve(page(30, 'latest-user'));
  await first;
  assert.match(h.elements.leaderboard.innerHTML, /latest-user/);
  assert.match(h.elements.leaderboardMeta.textContent, /最近 30 天/);
});

test('concurrent periodic refreshes coalesce into one pending request', async () => {
  const h = harness();
  const task = h.load();
  for (let i = 0; i < 20; i++) assert.equal(h.load(), task);
  assert.equal(h.requests.length, 1);
  h.requests[0].resolve(page(7));
  await flush();
  assert.equal(h.requests.length, 2);
  h.requests[1].resolve(page(7));
  await task;
  assert.equal(h.requests.length, 2);
});

test('stale request failure cannot overwrite a newer selected range', async () => {
  const h = harness();
  const task = h.load();
  h.elements.leaderboardRange.value = '0';
  h.load();
  h.requests[0].reject(new Error('obsolete failure'));
  await flush();
  assert.equal(h.requests[1].url, '/playback/leaderboard');
  assert.doesNotMatch(h.elements.leaderboard.textContent, /obsolete failure/);
  h.requests[1].resolve(page(null));
  await task;
  assert.match(h.elements.leaderboardMeta.textContent, /全部/);
});

test('current failure is visible and the next request can recover', async () => {
  const h = harness();
  const first = h.load();
  h.requests[0].reject(new Error('offline'));
  await first;
  assert.match(h.elements.leaderboardMeta.textContent, /读取失败/);
  assert.match(h.elements.leaderboard.textContent, /offline/);
  const retry = h.load();
  h.requests[1].resolve(page(7));
  await retry;
  assert.match(h.elements.leaderboard.innerHTML, /Alice/);
});

test('rendering escapes names and distinguishes returned top entries from total people', () => {
  const h = harness();
  h.render(page(1, '<img src=x onerror=alert(1)>'));
  assert.match(h.elements.leaderboard.innerHTML, /&lt;img/);
  assert.doesNotMatch(h.elements.leaderboard.innerHTML, /<img/);
  assert.match(h.elements.leaderboard.innerHTML, /3 次/);
  assert.match(h.elements.leaderboard.innerHTML, /最近播放确认时刻/);
  assert.match(h.elements.leaderboardMeta.textContent, /最近 24 小时/);
  assert.match(h.elements.leaderboardMeta.textContent, /前 20 名 · 已显示 1 人/);
});

test('stalled request times out and a queued range still loads', async () => {
  const h = harness();
  const task = h.load();
  h.elements.leaderboardRange.value = '30';
  h.load();
  assert.equal(h.timers.size, 1);
  h.timers.values().next().value();
  await flush();
  assert.equal(h.requests[0].signal.aborted, true);
  assert.equal(h.requests.length, 2);
  h.requests[1].resolve(page(30));
  await task;
  assert.equal(h.timers.size, 0);
  assert.match(h.elements.leaderboardMeta.textContent, /最近 30 天/);
});

test('current timeout is reported and releases the loading lock', async () => {
  const h = harness();
  const task = h.load();
  h.timers.values().next().value();
  await task;
  assert.match(h.elements.leaderboard.textContent, /请求超时/);
  assert.equal(h.timers.size, 0);
  const retry = h.load();
  h.requests[1].resolve(page(7));
  await retry;
  assert.match(h.elements.leaderboard.innerHTML, /Alice/);
});

test('empty range renders a clear empty state', () => {
  const h = harness();
  h.render({ days: null, limit: 20, entries: [] });
  assert.equal(h.elements.leaderboard.innerHTML, '该时间范围内暂无成功点歌记录');
  assert.match(h.elements.leaderboardMeta.textContent, /已显示 0 人/);
});
