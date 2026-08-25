# 辅助登录·二维码提取功能实现规格

> 调研依据：真实页面结构、WebView2 本地运行结果和现有凭据采集路径。
> 当前目标：QQ 音乐、网易云音乐、酷狗音乐和哔哩哔哩统一使用官方 WebView2 登录页提取二维码；登录状态和最终凭据也由同一个 WebView2 profile 采集。

## 1. 总体架构

`crates/miliastra-login-helper/src/webview2.rs` 负责四个平台的 WebView2 登录：

- WebView2 在首次导航前安装 document-created 脚本；脚本覆盖顶层文档和 child frame，优先通过 WebMessageReceived 推送二维码，ExecuteScript 轮询作为兼容兜底。
- 脚本只返回二维码图片 data URL 或官方图片 URL，不读取 `document.cookie`，也不把页面脚本当作登录协议。
- data URL 在协议层校验 PNG/JPEG 文件头和 256 KiB 上限；图片 URL 只允许对应平台域名，由短时限 HTTP 客户端下载后再校验。
- 二维码帧通过现有 `LoginHelperMessage::QrCode` 传给主进程，Cookie、OAuth 回调和登录完成判定继续走 WebView2 CookieManager。
- WebView2 的绝对截止时间同时覆盖页面加载、二维码提取和 Cookie 轮询；取消或窗口关闭会释放 profile 和所有异步回调。

## 2. 四个平台的页面入口和提取方式

| 平台 | WebView2 入口 | 页面二维码形态 | 提取方式 |
| --- | --- | --- | --- |
| QQ 音乐 | `graph.qq.com/oauth2.0/authorize` 官方 QQ OAuth 页 | 跨域 ptlogin iframe 内的 `img.qrImg`，URL 指向 `xui.ptlogin2.qq.com/ssl/ptqrshow` | child frame 通过 WebView2 注入桥转发图片 URL；扫码后顶层文档跳到 `y.qq.com/portal/wx_redirect.html`，由导航事件捕获 |
| 网易云音乐 | `https://music.163.com/` | 登录弹窗内约 228×228 的 `canvas` | 页面脚本点击 `data-action="login"`，调用 `canvas.toDataURL('image/png')` |
| 酷狗音乐 | `https://login-user.kugou.com/login/?appid=1014...` | `alt="登录二维码"` 的 PNG data URL | 读取可见 `img` 的 data URL；扫码成功后页面写入 `KuGoo` Cookie |
| 哔哩哔哩 | `https://www.bilibili.com/` | 登录弹窗内 `img[alt="登录二维码"]` | document-created 脚本点击登录入口并读取 data URL；CookieManager 采集登录 cookie |

QQ 打开官方 OAuth 外层页面，使 ptlogin iframe 拥有正常的父页面回调；这仍是网页登录流程，OAuth 回调和 Cookie 都在同一个 WebView2 会话内完成。QQ 回调若只带 `code` 而没有外层 `login_type`，仅在 `graph.qq.com/oauth2.0/login_jump` 路径下推断为 QQ 登录类型 1。

## 3. 二维码脚本约束

脚本采用通用候选评分而不是固定页面层级：

- 只接受可见、近似正方形且边长至少 80 px 的 `canvas`/`img`。
- `qr`、`qrcode`、`qrlogin`、`二维码` 等标记优先；普通 Logo 不会被选中。
- QQ 的二维码位于跨域 ptlogin iframe；child frame 通过 `window.top.postMessage` 转发给顶层 WebView2 桥，不绕过浏览器会话。
- NetEase Canvas 转 PNG；外链图片由 Rust 侧下载，限制 `http/https`、平台域名后缀和 256 KiB 响应大小。
- 页面脚本不访问 Cookie、localStorage 或凭据字段；B 站已有的 refresh token 探测仍是独立的登录完成步骤。

## 4. 凭据采集和完成判定

- QQ：导航回调触发已有 OAuth 授权码交换；CookieManager 快照与交换结果合并，保留 `uin`、`qqmusic_key`、`openid/access_token/refresh_token` 等别名。
- 网易云：CookieManager 以 `MUSIC_U` 为硬条件。旧客户端二维码接口扫码后返回 8821 的问题不再影响此路径，因为不再调用该接口。
- 酷狗：CookieManager 以 `KuGoo` 中同时存在 `t=` 和 `KugooID=` 为硬条件；兼容网页 `encodeURIComponent` 写入的 `%3D/%26` 形式，设备辅助字段按 allowlist 保留。
- 主进程仍通过 `/player/login/status` 返回活动会话的最新二维码，二维码刷新时替换旧图；最终凭据只在满足平台硬条件后发送终态帧。

## 5. 验证结果

已在 Windows 本机使用临时 WebView2 profile 完成四个平台的真实扫码闭环：

- `qqmusic`：成功提取官方 `qrImg` URL；扫码后收到 OAuth 回调并返回 QQ 登录字段和会话 Cookie，窗口自动关闭。
- `netease`：扫码后返回 `MUSIC_U`、`__csrf`，收到 `success` 终态，窗口自动关闭。
- `kugou`：扫码后返回包含 `t=` 和 `KugooID=` 的 `KuGoo` Cookie，并解析出 token、用户 ID，窗口自动关闭。
- `bilibili`：扫码后返回 `SESSDATA`，窗口自动关闭；本次 `ac_time_value` 未在等待窗口内出现，因此回退到基础 Cookie，自动刷新能力仍需单独验证。
- 登录助手不再包含原生二维码生成、MQTT 或平台二维码轮询实现。

失败不会写入空凭据，重复提取失败三次会返回稳定错误。
