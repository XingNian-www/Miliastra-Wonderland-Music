# 辅助登录·二维码提取功能 实现规格

> 调研依据：真实接口实测 + 现有代码路径核查，非静态猜测。
> 目标：让 QQ 音乐 / 网易云 / 哔哩哔哩 也走「官方 API 直连拿二维码」登录，与酷狗统一。三个平台的原生二维码链路已经落地；当接口不可用时保留 WebView2 兼容回退。

---

## 1. 现有架构

详见 `crates/miliastra-login-helper/src/webview2.rs`，分两类：

### 1.1 酷狗（已实现，作为参考模板）
- 官方 API 直连：`kugou_qr_key()` 调 `https://login-user.kugou.com/v2/qrcode`，**直接返回 base64 二维码图**（`data.qrcode_img`）+ `data.qrcode`(key)。
- WebView2 仅用来显示图片（`kugou_qr_page()` 内嵌 base64 图）+ 后台线程轮询 `https://login-user.kugou.com/v2/get_userinfo_qrcode`，`status=4` 拿到 token/userid。
- 无需页面导航、无需抓 cookie。二维码图直接通过现有协议上抛主程序。

### 1.2 QQ / 网易 / B 站（原生二维码已实现）
- `native_qr.rs` 负责官方接口、二维码图片编码、轮询线程、取消和凭据映射。
- 原生链路首先把 `QrCode` 帧送到主进程；登录成功后再送终态凭据。接口初始化失败时回退到原有 WebView2 页面抓 Cookie。
- QQ 原生链路使用移动端 `CreateQRCode` + MQTT v5，成功后调用 `music.login.LoginServer/Login(tmeLoginType=6)` 换取完整凭据；旧 WebView OAuth 逻辑仍保留作回退。

---

## 2. 关键判断：三平台接口实测结论

| 平台 | 二维码获取接口 | 返回"图"还是"内容URL" | 轮询接口 | 实测 |
|------|--------------|---------------------|---------|------|
| **哔哩哔哩** | `GET https://passport.bilibili.com/x/passport-login/web/qrcode/generate` | 内容 URL（登录页 URL）+ `qrcode_key`，本地生成 PNG | `GET /x/passport-login/web/qrcode/poll?qrcode_key=` | ✅ 已接入 |
| **网易云** | `POST https://music.163.com/api/login/qrcode/unikey?type=1` | 只返回 `unikey`，本地生成 PNG | `POST /api/login/qrcode/client/login?key=&type=1` | ✅ 已接入；普通 API 实测返回 801，暂不依赖 eAPI |
| **QQ音乐** | `POST https://u.y.qq.com/cgi-bin/musicu.fcg` 的 `CreateQRCode` | 官方直接返回 PNG data URL | WSS MQTT `mu.y.qq.com/ws/handshake` + `management.qrcode_login/<id>` | ✅ 已接入 |

**分类结论：**
- QQ 音乐返回官方 PNG；B 站和网易云返回内容 URL，由 Rust `qrcode` 生成 PNG。
- 三个平台都复用现有 `QrCode` 非终态帧；当前实现将登录助手协议升级到版本 4，Web API 仍复用 `/player/login/status`，没有新增路由。

---

## 3. 原阻断点的处理结果

已在 `miliastra-login-helper` 引入纯 Rust `qrcode` + `image`，统一输出经过协议层 PNG 签名/大小校验的 data URL；网易云不再依赖文档中风险较高的 eAPI 路径，优先使用实测可用的普通 `/api` 轮询。

---

## 4. 现有可复用代码

### 4.1 协议层
`crates/miliastra-login-protocol/src/lib.rs`：
- `LoginHelperMessage::QrCode { version, provider, image_data_url }` —— 非终态消息，主程序持续接收直到 `success`/`error`。
- `validate_qr_data_url`：仅接受 `data:image/png;base64,` / `data:image/jpeg;base64,`，限 `MAX_QR_IMAGE_BYTES=256*1024`，校验文件头签名（PNG `\x89PNG\r\n\x1a\n`、JPEG `\xff\xd8\xff`）。
- **A 类平台直接产出 PNG 即可通过校验，无需改协议。**

### 4.2 helper 主流程（参考酷狗分支）
`crates/miliastra-login-helper/src/main.rs`：
- `Provider` 枚举已有 `QqMusic / Netease / Bilibili / Kugou`，`id()` / `payload()` 已就绪。
- 酷狗把二维码上抛到 `qr_code(args.provider, image_data_url)`，主程序进 `run_helper` 收到 `QrCode` 后 `store_qr_code`。

### 4.3 主程序 QR 存储边界
`src/adapters/login_helper.rs` 的 `store_qr_code()` 校验返回平台必须与活动会话一致，现在允许四个已支持平台：
```rust
if actual != session.provider {
    return Err(...invalid_helper_provider...);
}
```
二维码帧仍受协议层图片签名、大小和活动会话校验保护。

### 4.4 平台常量与命名字段对照
- `ProviderId::QqMusic/Netease/Bilibili/Kugou`，`as_str()` 分别为 `"qqmusic"/"netease"/"bilibili"/"kugou"`。
- `webview2.rs` 的 `allowed_cookie_names` / `has_required_cookies` 已给出各平台登录后要抓的核心 cookie（QQ: uin/qqmusic_key 等；B 站: SESSDATA/bili_jct/DedeUserID + ac_time_value 刷新 token；网易: MUSIC_U；酷狗: KuGoo）。

---

## 5. 各平台接入细则（已落地）

### 5.1 QQ 音乐：CreateQRCode + MQTT v5

- 生成：向 `https://u.y.qq.com/cgi-bin/musicu.fcg` 发送 `music.login.LoginServer/CreateQRCode`，使用 Android 公共参数（`ct=11`、`cv=14090008`、`tmeAppID=qqmusic`）。响应中的 `req_0.data.qrcode` 是官方 PNG data URL，`qrcodeID` 是本次会话标识。
- 监听：连接 `wss://mu.y.qq.com/ws/handshake`，在 CONNECT/订阅属性中携带 `tmeAppID=qqmusic`、`business=management`、`hashTag/userID=qrcodeID`，订阅 `management.qrcode_login/<qrcodeID>`。事件类型优先从 MQTT v5 user property `type` 读取，再回退到 JSON 的 `type` 字段。
- `scanned` 继续等待，`timeout`/`canceled` 结束，`cookies` 事件提取 `qqmusic_uin` 与 `qqmusic_key`，调用 `music.login.LoginServer/Login`（`tmeLoginType=6`）换取音乐凭据。响应同时检查顶层和 `req_0` 内层业务码，避免把顶层 `code=0` 误判为成功。
- 原有 QQ WebView OAuth 链路仍保留：原生接口初始化失败时导航到原登录页，保证接口被限流或变更时仍可登录。

### 5.2 哔哩哔哩：公开 Web QR 接口

- 生成：`GET https://passport.bilibili.com/x/passport-login/web/qrcode/generate`，读取 `data.url` 与 `data.qrcode_key`；`url` 是二维码内容，不是图片，使用 Rust `qrcode` 渲染 PNG。
- 轮询：`GET https://passport.bilibili.com/x/passport-login/web/qrcode/poll?qrcode_key=...`。`data.code` 为 `86101`（未扫码）或 `86090`（已扫码）时继续；`86038` 触发换码，最多使用 3 张二维码（初始二维码加 2 次自动刷新）；`0` 读取响应 `Set-Cookie`、成功 URL 查询参数和 `refresh_token`。
- 成功至少要求 `SESSDATA` 非空；`bili_jct` 若由服务端下发则一并保留，但不是完成登录的硬性条件。`refresh_token` 映射到现有 B 站凭据的独立 `refresh_token` 字段（helper 内部暂存为 `ac_time_value`，发送终态前移出 cookie map）。
- 轮询线程在每次换码后都会发送新的 `QrCode` 帧，主进程和 Web 页面会替换旧图片。

### 5.3 网易云：普通 API unikey + client/login

- 生成：`POST https://music.163.com/api/login/qrcode/unikey?type=1`，响应 `code=200` 与 `unikey`。二维码内容为 `https://music.163.com/login?codekey=<unikey>`，本地渲染 PNG；不依赖已失效的 `qrimg` 接口。
- 轮询：`POST https://music.163.com/api/login/qrcode/client/login?key=<unikey>&type=1`。`801` 未扫码、`802` 已扫码、`803` 成功、`800` 过期。成功时合并 `Set-Cookie` 及响应中的 `cookie`/`cookies` 字符串，至少要求 `MUSIC_U`。
- 这里没有引入 eAPI 加密：当前普通 `/api` 链路已实测生成 key、返回 `801`，实现面更小且避免把风控失败误报成登录成功；如果服务端未来关闭普通轮询，代码会收敛为可重试错误并保留 WebView2 回退。

---

## 6. 传输与生命周期

- helper 在取得第一张二维码后立即写出 `QrCode` 帧，主进程 `run_helper` 持续读取非终态帧并按活动会话保存；二维码刷新时发送新的 `QrCode`，Web 页面会替换旧图。
- `store_qr_code` 校验 provider 必须与当前会话一致，协议层同时校验 data URL 的 MIME、Base64、PNG/JPEG 文件头和 256 KiB 解码上限；日志和 Debug 实现不会输出图片内容或凭据。
- 每个平台轮询线程都有绝对截止时间、取消标志、网络连续失败收敛和有限换码次数。登录取消或窗口销毁时设置取消标志；原生二维码初始化失败时回到原 WebView2 登录页，轮询阶段的终态错误则明确返回给 Web UI。
- Web UI `/player/login/status` 直接返回当前活动会话的 `qrCode.imageDataUrl`，页面按平台显示标题和扫码提示，不再只对酷狗显示二维码。

## 7. 验证结果与剩余风险

- 已验证：三平台真实公网二维码生成均通过协议图片校验；QQ MQTT WSS 连接在 8 秒边界内正常建立并可有界退出；helper `cargo check`、`cargo clippy --all-targets -D warnings`、单元测试全部通过。
- 未执行真实账号扫码闭环（需要在设备上确认登录），因此 QQ `cookies` 事件到 `Login` 的最终凭据字段仍以服务端实际返回为准；解析器已覆盖数字/字符串业务码、嵌套 `req_0` 响应和常见 cookie 载体。
- 平台接口属于第三方公开登录协议，可能出现限流、区域网络或字段变更。请求失败不会把空凭据写入存储；达到连续失败/过期上限后返回稳定错误码，用户可重新发起登录。
- 原生二维码仍在现有 Windows WebView2 helper 窗口中运行，以便保留旧登录回退和消息循环；WebView2 Runtime 缺失时不会伪造成功凭据，而是返回运行时不可用错误。
