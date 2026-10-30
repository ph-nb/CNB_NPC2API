# 配置参考

默认读取当前目录的 `config.yaml`，不存在时读取 `config.example.yaml`。`CNB_NPC2API_CONFIG` 可指定路径，原来的 `TOOLFORGE_CONFIG` 仍可作为后备；`--config` 或 `-c` 优先于这两个环境变量。YAML 中的字符串支持 `${NAME}` 与 `${NAME:-default}`；变量未设置时前者展开为空字符串，后者使用默认值。展开后的值仍是字符串，不能用它替代数值或布尔字段。示例配置可以复制为被 Git 忽略的 `config.yaml`。

## 服务和客户端鉴权

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `server.host` | `127.0.0.1` | 监听地址；`--host` 可覆盖。 |
| `server.port` | `8080` | 监听端口；`--port` 可覆盖。 |
| `server.timeout_seconds` | `180` | 单次上游请求的总超时，必须大于零；包括响应流读取。 |
| `client_authentication.enabled` | `true` | 是否校验客户端密钥。 |
| `client_authentication.allowed_keys` | `[]` | 启用鉴权时必须配置至少一个非空密钥，空密钥会导致启动失败。 |

客户端密钥是访问本服务的凭证，与 CNB Cookie 或其他上游 API key 不同。CNB 首页获取凭证使用独立的 15 秒超时；CNB 网络或 CSRF 失败时可能重试，因此总耗时可超过单次请求超时。对外监听时需要额外配置 TLS 和网络访问控制，详见 [部署](DEPLOYMENT.md)。

## 上游

`upstreams` 必须包含至少一项，名称必须唯一。示例：

```yaml
upstreams:
  - name: cnb
    type: cnb
    base_url: https://cnb.cool
    session_cookie: "${CNB_SESSION_COOKIE}"
    models: [deepseek-v4-flash, deepseek-v4-pro]
    native_fc: false
    is_default: true
    pool_min: 2
    pool_max: 8
    ttl_minutes: 30
  - name: external
    type: openai_compat
    base_url: https://api.example.com/v1
    api_key: "${EXTERNAL_API_KEY}"
    models: [example-model]
    native_fc: true
    headers: {}
routing:
  aliases:
    chat-fast:
      upstream: cnb
      model: deepseek-v4-flash
```

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `name` | `unnamed` | 上游名称。 |
| `type` | `openai_compat` | `cnb`、`openai_compat`、`openai`、`grok`、`anthropic` 或 `gemini`。 |
| `base_url` | 空 | HTTP(S) 地址，不得包含用户名、密码、查询参数或片段。OpenAI 兼容服务通常以 `/v1` 结尾，Anthropic 和 Gemini 通常使用服务根地址。 |
| `api_key` | 空 | 非 CNB 上游使用的 API key。 |
| `session_cookie` | 空 | CNB Cookie 请求头值；外部地址要求 HTTPS。 |
| `models` | `["*"]` | 匹配模型名；`*` 为通配。 |
| `native_fc` | `true` | 是否向该上游发送原生函数工具；CNB 必须设为 `false`。 |
| `is_default` | `false` | 未匹配模型时的默认上游。 |
| `headers` | `{}` | 发送给该上游的附加 HTTP 请求头。 |
| `pool_min` / `pool_max` | `2` / `8` | CNB 凭证池的最小和最大数量，须满足 `0 < pool_min <= pool_max`。 |
| `ttl_minutes` | `30` | CNB 凭证在池中的有效时间，必须大于零。 |

路由按以下顺序选择：明确的 `routing.aliases`，精确匹配的模型，第一个通配上游，第一个 `is_default` 上游，最后是第一个上游。别名的 `model` 为空时使用客户端模型名。`/v1/models` 展示明确配置的模型和别名，不展示 `*`。

CNB 启动时会使用该上游的 Cookie 访问首页取得配对的 CSRF token 和 Cookie；首页设置了新的 `csrfkey` 时使用新值，否则使用当前会话中已有的值。不同用户的值可以不同，程序不提供固定的用户 CSRF 值。首页不可达或无法取得凭证时启动失败。获得凭证不代表聊天已登录：真实聊天通常需要有效的 CNB 登录 Cookie。Cookie 值请通过环境变量或服务管理器提供，不要写入仓库。

不同 CNB 账户可分别配置为独立上游，并通过模型别名选择：

```yaml
upstreams:
  - name: account-a
    type: cnb
    base_url: https://cnb.cool
    session_cookie: "${CNB_ACCOUNT_A_COOKIE}"
    models: [deepseek-v4-flash]
    native_fc: false
  - name: account-b
    type: cnb
    base_url: https://cnb.cool
    session_cookie: "${CNB_ACCOUNT_B_COOKIE}"
    models: [deepseek-v4-flash]
    native_fc: false
routing:
  aliases:
    account-a-model: {upstream: account-a, model: deepseek-v4-flash}
    account-b-model: {upstream: account-b, model: deepseek-v4-flash}
```

每个变量均由对应账户在本地提供完整 Cookie 请求头值，凭证池相互独立。客户端密钥只用于鉴权，不自动绑定上游：有权限访问网关的客户端可以选择已配置的任一别名。需要按用户隔离账户时，应分别运行实例或在受信任的访问层限制路由。

## 功能开关

| `features` 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `fc_mode` | `auto` | `auto`、`prefer_native` 或 `force_prompt`；前两者在可用时选择原生工具，后者强制提示词模式。 |
| `enable_streaming` | `true` | 关闭时拒绝流式请求。 |
| `enable_fc_error_retry` | `true` | 非流式提示词模式发现损坏工具调用时重试。 |
| `fc_error_retry_max` | `2` | 上述重试次数，实际最多 10 次。 |
| `strip_think_tags` | `true` | 从处理结果中去除 think 标记。 |
| `inject_protocol` | `XYML` | 提示词协议名，仅允许字母、数字和下划线。 |
| `convert_developer_to_system` | `true` | 将 developer 消息转换为 system 消息。 |
| `key_passthrough` | `false` | 将客户端密钥用作非 CNB 上游密钥；开启后客户端密钥会发送给所选上游。 |
| `model_passthrough` | `false` | 向上游发送原始客户端模型名，而非别名解析后的模型名。 |
| `enable_cli_profiles` | `true` | 向提示词加入部分 CLI 客户端工具规则。 |
| `obfuscate_tool_names` | `false` | 提示词模式下改写工具名并在返回时映射回来。 |
| `enable_metrics` | `true` | 关闭时 `/metrics` 返回 `{"enabled":false}`。 |

请求头 `x-toolforge-fc-mode: force_prompt` 可对单个请求强制提示词模式。无工具或工具选择为 `none` 时使用普通转发模式。响应头 `x-toolforge-fc-mode` 标识实际模式。

## 重载

使用 `--reload` 启动后，程序每秒检查配置文件内容。有效的新配置会用于新请求，旧请求继续使用原配置；解析或上游初始化失败时保留旧配置。配置中的监听地址和端口变更需要重启，命令行传入的 `--host` 和 `--port` 会继续覆盖配置。环境变量只在解析配置时读取：单独修改环境变量不会触发重载。编译后的程序不会因源码改动自动重新构建。
