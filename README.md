# Miliastra Wonderland Music

Miliastra Wonderland Music 是一个面向 Windows 的原神/千星奇域点歌与聊天自动化工具。它读取游戏聊天中的命令，通过内置播放器播放音乐，并提供歌词、队列、好友操作和多人娱乐玩法。

## 功能概览

- 游戏内大厅和好友私聊点歌，支持 QQ 音乐、网易云音乐和酷狗音乐；好友私聊还支持 B 站来源。
- 播放、暂停、切歌、音量、队列、歌词和大厅信息查询。
- 一级/二级聊天监听、好友邀请、麦克风切换、UID 拉黑和聊天屏蔽投票。
- 成语接龙、斗地主、跑得快、海龟汤和谁是卧底等娱乐玩法。
- 本地 Web 面板、远程控制和高级诊断工具。

运行与配置方法见 [用户使用指南](docs/user-guide.md)。

发布包包含主程序、登录辅助器和运行所需的媒体、OCR 资源；Windows WebView2 Evergreen Runtime 是登录功能的系统前置条件。
凭据保存在 `deps/data/credentials/`，不通过 HTTP 暴露。

## 简易配置流程

1. 解压发布包，在 `config.yaml` 中确认 `database_path`、`http.host` 和 `http.port`。
2. 启动程序，打开 `http://127.0.0.1:18888/`，在页面「访问令牌」中填入 `config.yaml` 的 `http.access_token`。首次启动时若该字段为空，程序会自动生成并写回。
3. 在「配置中心」确认游戏进程名、画面尺寸、OCR 和播放配置，按实际环境修改并保存。
4. 在「账号登录」登录需要使用的音乐平台，确认显示「已登录」。
5. 在「总览」确认 OCR 和播放器状态，进入游戏后发送 `@点歌 歌名 歌手`。

功能配置默认保存在 `deps/data/playback.sqlite3`；Web 面板中的配置中心是日常修改入口。远程访问时在 `config.yaml` 中调整 `http.host`，重启程序后使用对应地址和同一访问令牌。

## 用户文档

- [用户使用指南](docs/user-guide.md)：安装、配置、常用命令、好友操作、歌词、队列和故障排查。
- [成语接龙](docs/idiom-chain.md)：玩法、命令和词库配置。
- [海龟汤](docs/turtle-soup.md)：开局、提问、长答案和题库配置。
- [斗地主与跑得快](docs/card-games.md)：组局、抢地主、出牌、手牌查询和投递重试。
- [谁是卧底](docs/undercover.md)：报名、发言、投票和词库配置。
- [Web 面板使用](docs/web-tools.md)：状态面板、远程控制和高级工具。
- [平台登录](docs/login-qr-feasibility.md)：登录步骤、凭据和环境要求。

## 许可证

本项目使用 MIT 许可证，详见 [LICENSE](LICENSE)。第三方组件的许可信息见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
