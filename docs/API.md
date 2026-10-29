# API 参考

服务接受 JSON 请求，单个请求体上限为 64 MiB。除健康检查外，下列接口启用鉴权时均需要客户端密钥；支持 `Authorization: Bearer KEY`、`x-api-key: KEY` 或 `x-goog-api-key: KEY`。当前不接受通过 URL 查询参数传递密钥。

| 方法和路径 | 用途 |
| --- | --- |
| `GET /healthz`、`GET /health` | 进程健康信息，不检查上游聊天是否可用，不要求鉴权。 |
| `GET /v1/models` | 已配置模型和别名。 |
| `GET /pool` | CNB 池的状态，不返回 Cookie 或 CSRF 值。 |
| `GET /metrics` | 请求、失败和重试计数，JSON 格式。 |
| `POST /v1/chat/completions`、`/chat/completions` | OpenAI Chat Completions。 |
| `POST /v1/responses`、`/responses` | OpenAI Responses。 |
| `GET`、`DELETE /v1/responses/{id}` 或 `/responses/{id}` | 查询或删除本进程保存的 Responses。 |
| `POST /v1/messages`、`/messages`、`/anthropic/v1/messages` | Anthropic Messages。 |
| `POST /v1/messages/count_tokens`、`/anthropic/v1/messages/count_tokens` | 近似 token 数，按字符数估算，并非模型 tokenizer。 |
| `POST /v1beta/models/{model}:generateContent` | Gemini 非流式请求，也支持 `/v1/models/` 前缀。 |
| `POST /v1beta/models/{model}:streamGenerateContent` | Gemini 流式请求，也支持 `/v1/models/` 前缀。 |

OpenAI SDK 的 base URL 可设置为 `http://127.0.0.1:8080/v1`；Anthropic SDK 使用 `http://127.0.0.1:8080`；Gemini SDK 使用同一服务根地址及 `v1beta` API 版本。SDK 示例和自动化验证见 [开发与测试](DEVELOPMENT.md)。

例如在 POSIX shell 中，设置 `CNB_FORGE_API_KEY` 后：

```sh
curl --fail-with-body http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer $CNB_FORGE_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"你好"}]}'
```

该示例会调用真实 CNB，具体模型与登录状态以当前服务为准。模型请求可以使用对应协议的 `stream` 或 Gemini 流式方法；服务以 SSE 返回流式事件。普通文本和原生工具流可增量返回；提示词工具流为避免提前泄漏未完成的协议标记，会缓冲模型文本后解析。

## 工具调用和多模态

函数工具可在客户端支持的四种协议和已支持的上游之间转换。CNB 使用注入的 XYML 提示词与模型输出解析；服务只返回工具调用，客户端负责执行工具。提示词解析支持多种常见格式和部分修复，并未证明与原 ToolForge 的全部宽松解析行为等价。

非函数原生工具仅在客户端和上游协议匹配、上游启用 `native_fc` 且未强制提示词模式时透传；CNB 和跨协议路径会拒绝。具体工具是否可用取决于上游和模型。OpenAI 原生 Responses 工具路径在本地重放历史，并向上游发送 `store: false`。

四种客户端格式可以在 OpenAI 兼容、Anthropic、Gemini 上游之间转换 PNG、JPEG、GIF 和 WebP 的 base64 图片或可转换 URL。网关不会下载图片 URL；`gs://` 仅能发往 Gemini，Gemini URL 需要已知媒体类型。CNB 图片、工具结果中的图片、文件 ID、音频和视频不支持；不同协议的图片细节参数也无法保证等价。当前只支持单个候选结果。

## Responses 状态

默认 `store: true`；可显式设置 `store: false`。保存的响应支持 `previous_response_id`、查询和删除，并按客户端密钥隔离。关闭客户端鉴权时，未携带密钥的请求共享匿名身份，不能以此隔离不同用户。状态仅保存在当前进程：最多 256 条、总序列化大小 64 MiB、单条 16 MiB，24 小时过期，进程重启后丢失。实际内存占用可能更高；容量不足时淘汰最早记录。流式响应成功完成后才会保存。前一轮 `instructions` 不自动继承，`conversation` 对象和后台响应不支持；多进程或多副本之间不共享状态。

## 错误

客户端密钥不正确返回 401；无效参数或不支持的转换通常返回 400；找不到已保存的响应返回 404。上游 HTTP 错误通常保留状态码并隐藏上游响应体，连接失败返回 502。CNB 返回未登录时需要更新本地会话，不应把 Cookie 放进 issue 或日志。流式响应开始后发生的失败不能一概映射为新的 HTTP 状态码，客户端应检查事件并处理连接中断。
