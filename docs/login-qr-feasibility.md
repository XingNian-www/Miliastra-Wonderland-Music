# 辅助登录·二维码提取功能实现规格

> 调研依据：真实页面结构、WebView2 本地运行结果和现有凭据采集路径。
> 当前目标：QQ 音乐、网易云音乐、酷狗音乐统一使用官方 WebView2 登录页提取二维码；登录状态和最终凭据也由同一个 WebView2 profile 采集。哔哩哔哩暂时保留已有原生二维码链路。

## 1. 总体架构

`crates/miliastra-login-helper/src/webview2.rs` 负责三家 WebView2 登录：

- WebView2 加载官方登录页，脚本每 250 ms 探测一次 DOM/Canvas。
- 脚本只返回二维码图片 data URL 或官方图片 URL，不读取 `document.cookie`，也不把页面脚本当作登录协议。
- data URL 在协议层校验 PNG/JPEG 文件头和 256 KiB 上限；图片 URL 只允许对应平台域名，由短时限 HTTP 客户端下载后再校验。
- 二维码帧通过现有 `LoginHelperMessage::QrCode` 传给主进程，Cookie、OAuth 回调和登录完成判定继续走 WebView2 CookieManager。
- WebView2 的绝对截止时间同时覆盖页面加载、二维码提取和 Cookie 轮询；取消或窗口关闭会释放 profile 和所有异步回调。

`crates/miliastra-login-helper/src/native_qr.rs` 目前只由哔哩哔哩分支使用。QQ/网易云/酷狗不再启动 MQTT、二维码状态轮询或酷狗测试版二维码接口。

## 2. 三个平台的页面入口和提取方式

| 平台 | WebView2 入口 | 页面二维码形态 | 提取方式 |
| --- | --- | --- | --- |
| QQ 音乐 | `xui.ptlogin2.qq.com/cgi-bin/xlogin` 官方 QQ OAuth QR 页 | `img.qrImg`，URL 指向 `xui.ptlogin2.qq.com/ssl/ptqrshow` | 读取图片 URL，带官方 Referer 下载并转 data URL；扫码后的 `graph.qq.com/oauth2.0/login_jump` 回调由导航事件捕获 |
| 网易云音乐 | `https://music.163.com/` | 登录弹窗内约 228×228 的 `canvas` | 页面脚本点击 `data-action="login"`，调用 `canvas.toDataURL('image/png')` |
| 酷狗音乐 | `https://login-user.kugou.com/login/?appid=1014...` | `alt="登录二维码"` 的 PNG data URL | 读取可见 `img` 的 data URL；扫码成功后页面写入 `KuGoo` Cookie |

QQ 直接打开官方 QR 页面是为了避开 QQ 音乐主页中的跨域 iframe；这仍是网页登录流程，OAuth 回调和 Cookie 都在同一个 WebView2 会话内完成。QQ 回调若只带 `code` 而没有外层 `login_type`，仅在 `graph.qq.com/oauth2.0/login_jump` 路径下推断为 QQ 登录类型 1。

## 3. 二维码脚本约束

脚本采用通用候选评分而不是固定页面层级：

- 只接受可见、近似正方形且边长至少 80 px 的 `canvas`/`img`。
- `qr`、`qrcode`、`qrlogin`、`二维码` 等标记优先；普通 Logo 不会被选中。
- NetEase Canvas 转 PNG；外链图片由 Rust 侧下载，限制 `http/https`、平台域名后缀和 256 KiB 响应大小。
- 页面脚本不访问 Cookie、localStorage 或凭据字段；B 站已有的 refresh token 探测仍是独立的登录完成步骤。

## 4. 凭据采集和完成判定

- QQ：导航回调触发已有 OAuth 授权码交换；CookieManager 快照与交换结果合并，保留 `uin`、`qqmusic_key`、`openid/access_token/refresh_token` 等别名。
- 网易云：CookieManager 以 `MUSIC_U` 为硬条件。旧客户端二维码接口扫码后返回 8821 的问题不再影响此路径，因为不再调用该接口。
- 酷狗：CookieManager 以 `KuGoo` 中同时存在 `t=` 和 `KugooID=` 为硬条件；兼容网页 `encodeURIComponent` 写入的 `%3D/%26` 形式，设备辅助字段按 allowlist 保留。
- 主进程仍通过 `/player/login/status` 返回活动会话的最新二维码，二维码刷新时替换旧图；最终凭据只在满足平台硬条件后发送终态帧。

## 5. 验证结果

已在 Windows 本机使用临时 WebView2 profile 实测：

- `qqmusic`：成功提取官方 `qrImg` URL 并发送协议版本 4 的 `qrCode` 帧。
- `netease`：成功点击登录弹窗并把 Canvas 转成 PNG，发送 `qrCode` 帧。
- `kugou`：成功提取官方 `data:image/png`，发送 `qrCode` 帧。
- 三个测试均在未扫码时按绝对超时退出，没有启动对应原生二维码轮询线程。

尚未完成真实账号扫码后的凭据闭环验证；需要在二维码有效期内扫码确认各平台当前 Cookie/OAuth 字段是否完整。失败不会写入空凭据，重复提取失败三次会返回稳定错误。
