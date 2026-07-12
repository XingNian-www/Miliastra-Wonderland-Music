# 海龟汤 AI 接入 DeepSeek 官方 API 的推荐配置

> 调研日期：2026-07-12  
> 资料范围：仅使用 DeepSeek 官方 API 文档。本文所称“OpenAI 兼容”均指 DeepSeek 官方提供的 OpenAI 格式 Chat Completions 接口。

## 结论摘要

1. 官方 OpenAI 格式 `base_url` 是 `https://api.deepseek.com`，Chat Completions 的完整地址是 `https://api.deepseek.com/chat/completions`。本项目的 `turtle_soup.ai.endpoint` 需要填写完整地址。[来源 1][来源 3]
2. `deepseek-chat` 与 `deepseek-reasoner` 目前仍可调用，但官方已明确注明二者将在 **2026-07-24 15:59 UTC** 弃用。当前二者分别映射到 `deepseek-v4-flash` 的非思考模式和思考模式，不适合作为新的长期配置。[来源 1][来源 2]
3. 海龟汤裁决默认推荐 `deepseek-v4-flash` 并显式关闭思考模式。本项目已发送 `thinking.type=disabled`，不需要依赖即将弃用的 `deepseek-chat` 别名。[来源 1][来源 4]
4. JSON Output 的正确参数是 `"response_format":{"type":"json_object"}`；提示词中还必须明确出现 `json`，并给出期望的 JSON 示例。需要设置合理的 `max_tokens`，并处理偶发空响应。[来源 5]
5. DeepSeek 官方 Chat Completions 请求字段是 `max_tokens`，官方接口参考中没有声明 `max_completion_tokens`。本项目不应依赖后者的 OpenAI 参数兼容性。[来源 3]
6. 非思考模式下，海龟汤这种低随机性的结构化裁决建议使用 `temperature: 0.0`，并删除 `top_p`；官方建议只调整 `temperature` 与 `top_p` 中的一项。思考模式下这两个参数都不生效。[来源 3][来源 4][来源 6]
7. `system` 角色受官方接口支持。`stream: false` 也受支持，并且是 API 默认行为；对于只返回一个很短 JSON 的裁决请求，保持非流式最简单。[来源 1][来源 3][来源 7]

## 推荐请求配置

### 长期推荐

本项目当前按以下结构发送 DeepSeek V4 请求：

```json
{
  "model": "deepseek-v4-flash",
  "messages": [
    {
      "role": "system",
      "content": "你是海龟汤谜题裁判。只返回合法 JSON。"
    },
    {
      "role": "user",
      "content": "输出 JSON 示例：{\"decision\":\"yes\"}。其余裁决上下文略。"
    }
  ],
  "thinking": {
    "type": "disabled"
  },
  "response_format": {
    "type": "json_object"
  },
  "temperature": 0.0,
  "max_tokens": 256,
  "stream": false
}
```

对应配置文件中的 Provider 部分应为：

```yaml
turtle_soup:
  ai:
    endpoint: "https://api.deepseek.com/chat/completions"
    api_key: "填写 DeepSeek API Key"
    model: "deepseek-v4-flash"
    thinking_enabled: false
    max_tokens: 256
```

`thinking_enabled` 和 `max_tokens` 是项目配置项，程序会把它们转换成 DeepSeek 请求体中的 `thinking.type` 和 `max_tokens`。`response_format`、`temperature` 和 `stream` 仍由代码按协议固定；开启思考时不会发送无效的 `temperature`。

### 不推荐的旧别名配置

```yaml
turtle_soup:
  ai:
    endpoint: "https://api.deepseek.com/chat/completions"
    api_key: "填写 DeepSeek API Key"
    model: "deepseek-chat"
```

这只是旧客户端的临时兼容方案，不是本项目当前配置。官方把 `deepseek-chat` 映射为 `deepseek-v4-flash` 的非思考模式，但该别名将在 2026-07-24 15:59 UTC 弃用。[来源 1][来源 2]

## 逐项核对

### 1. Base URL 与完整 Endpoint

DeepSeek 官方快速开始给出的 OpenAI 格式 `base_url` 是：

```text
https://api.deepseek.com
```

同一页的官方 `curl` 示例直接请求：

```text
POST https://api.deepseek.com/chat/completions
```

API Reference 同时把路径定义为 `POST /chat/completions`。因此本项目需要完整 URL 时，应配置 `https://api.deepseek.com/chat/completions`。[来源 1][来源 3]

当前官方快速开始没有把 `/v1` 写入 OpenAI 格式的规范 `base_url`。因此本文不把 `https://api.deepseek.com/v1/chat/completions` 作为推荐地址，也不假设它属于长期兼容契约。[来源 1]

### 2. `deepseek-chat` 与 `deepseek-reasoner`

截至调研日，官方说明如下：[来源 1][来源 2]

| 模型名 | 当前含义 | 海龟汤适用性 | 结论 |
| --- | --- | --- | --- |
| `deepseek-chat` | `deepseek-v4-flash` 的非思考模式兼容别名 | 短 JSON 分类、并发裁决和低延迟场景更匹配 | 仅可临时使用，别名即将弃用 |
| `deepseek-reasoner` | `deepseek-v4-flash` 的思考模式兼容别名 | 可能用于特别复杂的谜题复核，但会产生额外 `reasoning_content`，且采样参数不生效 | 不建议作为默认裁决模型，且别名即将弃用 |
| `deepseek-v4-flash` | 当前正式模型，支持非思考和思考模式 | 可显式关闭思考，适合高频短裁决 | 默认推荐 |
| `deepseek-v4-pro` | 当前正式模型，支持非思考和思考模式 | 可在实测证明复杂谜题准确率明显更重要时采用 | 质量优先的备选 |

官方说明思考模式会先产生 `reasoning_content`，再产生最终 `content`；海龟汤现有解析器只读取最终 `content`，协议上可以工作，但思考内容会增加一次裁决的生成工作量。对只允许六种结果的裁决，默认开启思考模式通常没有必要。[来源 3][来源 4]

V4 模型的思考模式默认是开启的，所以从 `deepseek-chat` 直接把模型名改为 `deepseek-v4-flash`，却不发送 `"thinking":{"type":"disabled"}`，行为会从非思考变为思考。这是迁移时必须处理的差异。[来源 4]

### 3. JSON Output

官方要求同时满足以下条件：[来源 5]

1. 请求体设置 `"response_format":{"type":"json_object"}`。
2. `system` 或 `user` 提示词中明确出现单词 `json`。
3. 提示词给出期望的 JSON 格式示例。
4. 合理设置 `max_tokens`，避免 JSON 在中途被截断。
5. 调用方处理偶发的空 `content`；官方说明该问题仍可能发生，可通过调整提示词缓解。

API Reference 还警告：如果启用 `json_object` 却没有在提示词中要求 JSON，模型可能持续输出空白直到达到 Token 上限；若 `finish_reason` 是 `length`，内容也可能是不完整 JSON。[来源 3]

本项目现状：

- 已设置 `response_format.type=json_object`。
- 固定系统提示词包含 `JSON`，用户提示词给出了 `{"decision":"..."}` 的明确格式。
- 空内容、非法 JSON 和非法枚举会进入现有重试流程，方向正确。
- 输出上限已使用官方 `max_tokens` 字段，数值由 `turtle_soup.ai.max_tokens` 配置，默认 `256`。
- `256` 个输出 Token 对单字段裁决 JSON 足够，可以保留该数值；官方没有针对海龟汤给出更具体的固定值。

### 4. `temperature`

官方参数建议表给出的值为：[来源 6]

| 场景 | 官方建议值 |
| --- | ---: |
| 编程 / 数学 | `0.0` |
| 数据清洗 / 数据分析 | `1.0` |
| 通用对话 | `1.3` |
| 翻译 | `1.3` |
| 创意写作 / 诗歌 | `1.5` |

官方没有单独列出“海龟汤裁决”。本项目需要稳定地把同一事实映射到固定枚举，性质更接近确定性分析而非创作，因此建议采用最接近的官方低随机性设置 `temperature: 0.0`。当前 `0.1` 是合法值，但不是官方表中的推荐档位。[来源 3][来源 6]

API Reference 建议调整 `temperature` 或 `top_p` 中的一项，不要同时调整。本项目在非思考模式发送 `temperature: 0.0`，不发送 `top_p`；DeepSeek V4 思考模式下两者都不发送。[来源 3][来源 4]

思考模式不支持 `temperature` 和 `top_p`。服务端为兼容现有软件会接受它们，但参数不会生效。因此若选择思考模式，应省略两者，避免配置给人错误预期。[来源 4]

### 5. `max_tokens` 与 `max_completion_tokens`

DeepSeek 官方 Chat Completions API Reference 声明的是 `max_tokens`：它限制本次 Chat Completion 最多生成多少 Token，并受模型上下文长度约束。[来源 3]

当前官方请求字段列表没有声明 `max_completion_tokens`。DeepSeek 所说的 OpenAI 兼容，不能推导为自动支持 OpenAI 的每一个新旧参数名。因此本项目遵循以下规则：

- DeepSeek 请求统一发送 `max_tokens`。
- 不同时发送两个字段。
- 不依赖 `max_completion_tokens` 被忽略、转换或容忍的未文档化行为。
- 若 `finish_reason=length`，把响应视为失败并重试或记录错误，不解析可能被截断的 JSON。[来源 3]

### 6. `system` 角色

官方请求结构明确列出 System message，`role` 的合法值为 `system`，`content` 为必填字符串；官方快速开始示例也使用了 system message。因此本项目把固定裁决规则放在 `system` 消息、把题目和玩家提问放在 `user` 消息是受支持且合适的结构。[来源 1][来源 3]

### 7. 是否关闭流式

官方同时支持流式和非流式：`stream: true` 时以 SSE 分片返回，非流式则一次返回完整 Chat Completion。官方 FAQ 说明 API 默认是 `stream=false`。[来源 1][来源 3][来源 7]

海龟汤裁决只需要读取一个很短的完整 JSON，当前实现也按一次性 JSON 响应解析。因此建议保留 `stream: false`：

- 不需要拼接 SSE 分片。
- 不需要分别处理 `reasoning_content` 和 `content` 的流式增量。
- 只有在完整响应到达后才解析 JSON，失败重试边界清晰。

这是针对本项目实现的工程选择，不是 DeepSeek 对 JSON Output 的强制要求。官方 JSON Output 示例本身也使用一次性 Chat Completions 调用。[来源 5]

## 与当前实现的差异清单

| 项目 | 当前实现 | 推荐状态 | 优先级 |
| --- | --- | --- | --- |
| Endpoint | 由配置提供完整地址 | `https://api.deepseek.com/chat/completions` | 必须正确配置 |
| 模型 | 由配置提供 | `deepseek-v4-flash`，显式关闭思考 | 高 |
| 思考模式 | 由 `thinking_enabled` 控制 V4 模型的 `enabled/disabled` | 按场景配置，默认关闭 | 已符合 |
| JSON Output | 已发送 `json_object` | 保持 | 已符合 |
| JSON 提示 | 已含 `JSON` 和目标格式 | 保持 | 已符合 |
| 输出上限 | 由 `max_tokens` 配置，默认 `256` | 思考模式按需提高 | 已符合 |
| 随机性 | 非思考模式 `temperature: 0.0`；思考模式省略；不发送 `top_p` | 保持 | 已符合 |
| 流式 | `false` | 保持 `false` | 已符合 |
| system role | 已使用 | 保持 | 已符合 |
| 空内容重试 | 已有 | 保持，并记录最终失败 | 已符合 |

## 官方来源

1. [DeepSeek API Docs：Your First API Call](https://api-docs.deepseek.com/)：OpenAI 格式 `base_url`、完整 `curl` endpoint、模型别名弃用说明、system message 示例及非流式示例。
2. [DeepSeek API Docs：Models & Pricing](https://api-docs.deepseek.com/quick_start/pricing)：当前正式模型、思考模式、JSON Output 支持情况，以及 `deepseek-chat` / `deepseek-reasoner` 的映射和弃用时间。
3. [DeepSeek API Reference：Create Chat Completion](https://api-docs.deepseek.com/api/create-chat-completion)：`POST /chat/completions`、消息角色、`thinking`、`max_tokens`、`response_format`、`stream`、`temperature`、`top_p` 和响应字段定义。
4. [DeepSeek API Guides：Thinking Mode](https://api-docs.deepseek.com/guides/thinking_mode)：思考模式开关、默认值、`reasoning_content`，以及思考模式下不生效的采样参数。
5. [DeepSeek API Guides：JSON Output](https://api-docs.deepseek.com/guides/json_mode)：JSON Output 启用条件、提示词要求、`max_tokens` 注意事项和偶发空内容说明。
6. [DeepSeek API Docs：The Temperature Parameter](https://api-docs.deepseek.com/quick_start/parameter_settings)：官方各类任务的 temperature 建议值。
7. [DeepSeek API Docs：FAQ](https://api-docs.deepseek.com/faq)：API 默认非流式行为及流式交互差异。
