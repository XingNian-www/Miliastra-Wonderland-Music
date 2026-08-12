# 用户文档

这里的文档只介绍安装、配置和使用方法，不记录项目内部实现、架构决策或开发过程。

## 使用入口

- [用户使用指南](user-guide.md)：首次运行、常用命令、监听模式、好友操作、歌词、队列、配置和故障排查。
- [成语接龙](idiom-chain.md)：成语接龙的命令、规则和词库配置。
- [海龟汤](turtle-soup.md)：海龟汤的命令、提问方式和题库配置。
- [谁是卧底](undercover.md)：报名、发言、投票和词库配置。
- [Web 面板使用](web-tools.md)：本地网页、远程控制和高级工具。

## 配置说明

- 功能配置统一保存在数据库 `deps/data/playback.sqlite3` 中，通过 Web 面板「配置中心」页面查看与修改（见 [Web 面板使用](web-tools.md)）。
- `config.yaml` 只保留启动引导三段：`database_path`、`http`、`logging`。
- `turtle_soup.example.yaml`：海龟汤题库格式示例（内容数据仍是文件）。
- `undercover.example.yaml`：谁是卧底词库格式示例（内容数据仍是文件）。

如果只想运行点歌功能，使用默认配置即可；娱乐玩法和 AI 功能可以按需开启。
