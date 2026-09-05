# 用户文档

## 使用入口

- [用户使用指南](user-guide.md)：首次运行、常用命令、监听模式、好友操作、歌词、队列、配置和故障排查。
- [成语接龙](idiom-chain.md)：成语接龙的命令、规则和词库配置。
- [斗地主与跑得快](card-games.md)：组局、出牌、手牌查询和投递重试。
- [海龟汤](turtle-soup.md)：海龟汤的命令、提问方式和题库配置。
- [谁是卧底](undercover.md)：报名、发言、投票和词库配置。
- [Web 面板使用](web-tools.md)：本地网页、远程控制和高级工具。
- [平台登录](login-qr-feasibility.md)：音乐平台登录、二维码和凭据状态。

## 配置说明

- 功能配置默认保存在数据库 `deps/data/playback.sqlite3` 中，通过 Web 面板「配置中心」查看与修改（见 [Web 面板使用](web-tools.md)）。
- `config.yaml` 用于数据库路径、Web 监听和日志等启动配置。
- `turtle_soup.example.yaml`：海龟汤题库格式示例（内容数据仍是文件）。
- `undercover.example.yaml`：谁是卧底词库格式示例（内容数据仍是文件）。

首次运行按 [README 的简易配置流程](../README.md#简易配置流程) 检查游戏环境并登录音乐平台；娱乐玩法和 AI 功能按需配置。
