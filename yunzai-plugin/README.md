# 千星 Yunzai 插件

这是 Miliastra Wonderland Music 的配套 Yunzai 插件。插件只通过 Miliastra Wonderland Music 的本地 HTTP API 工作，不直接启动或控制 EXE 进程。

## 安装

把 `yunzai-plugin` 目录复制到 Yunzai 的 `plugins/` 目录下，然后重启 Yunzai。

要求 Yunzai 使用 Node.js 18 或更新版本。

## 配置

编辑 `config/config.js`：

```js
export default {
  requestTimeoutMs: 5000,
  queuePreviewLimit: 5,
  backends: [
    {
      key: "A",
      name: "1号千星",
      baseUrl: "http://127.0.0.1:18888"
    },
    {
      key: "B",
      name: "2号千星",
      baseUrl: "http://127.0.0.1:18889"
    }
  ]
}
```

`key` 会用于命令里的后端选择，例如 `#千星A状态`。不要把后端 key 配成 `状态`、`发送`、`启动`、`截图` 或 `列表`。

## 命令

不指定后端时：

```text
#千星状态
#千星发送 晚上好
#千星启动
#千星截图
#千星列表
```

`#千星状态` 会同时请求所有后端并合并状态和队列。

`#千星发送 <内容>` 会同时让所有后端发送游戏内聊天。

`#千星启动` 和 `#千星截图` 会先显示所有后端状态，并要求回复数字选择一个后端，避免同时启动或截图多个后端。

指定后端时：

```text
#千星A状态
#千星A发送 晚上好
#千星A启动
#千星A截图
```

这些命令只请求 A 后端。

后端离线或接口异常时，插件会回复 `千星机器人未在线或接口不可用`。
