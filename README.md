# CNB_NPC2API

CNB_NPC2API 将 `cnb2api` 的 CNB 上游和 CSRF 凭证池，与 ToolForge 的多协议工具调用中间件整合进一个 Rust 服务。CNB 调用在进程内完成，不需要运行第二个代理服务。工具调用只返回给客户端，服务不会执行工具。

本文提供快速开始。完整说明见 [配置](docs/CONFIGURATION.md)、[API](docs/API.md)、[部署](docs/DEPLOYMENT.md)、[开发与测试](docs/DEVELOPMENT.md) 和 [安全](SECURITY.md)。

## 当前 CNB 上游状态

2026-10-28 的实测中，CNB 首页仍签发 CSRF 字段，但匿名聊天请求返回 `401 [NOT_LOGIN]`。2026-10-29 使用有效登录会话完成了真实 CNB 身份、聊天、流式输出和工具调用验证。通过 `CNB_SESSION_COOKIE` 提供当前用户完整的 Cookie 请求头值；Cookie 名称区分大小写，本次浏览器中的名称为 `CNBSESSION` 和 `csrfkey`。每个用户的 Cookie 和 CSRF 值可能不同，程序不会固定这些值：每个上游使用独立 Cookie jar 从当前会话及首页响应获取配对凭据。不要将 Cookie 写入仓库、日志或公开配置。

## 构建

安装 Rust 工具链和目标平台的 C 编译器后：

```sh
cargo build --locked --release
```

Windows 可安装 Rust 的 MSVC 工具链与 Visual Studio C++ 构建工具，使用上述命令；程序位于 `target/release/cnb-npc2api.exe`。Linux 需要系统 C 编译器，程序位于 `target/release/cnb-npc2api`。使用 Rust stable；目前验证过 Rust 1.98。可选的 `build-windows.ps1` 只适用于父目录已提供特定 LLVM-MinGW 工具链的本机工作区，不是独立克隆的必需条件。

Linux 上可安装 `musl-tools` 并执行 `rustup target add x86_64-unknown-linux-musl`，再运行 `cargo build --locked --release --target x86_64-unknown-linux-musl` 来构建静态程序。

## 配置和启动

示例配置为 `config.example.yaml`。它默认只监听 `127.0.0.1:8080`，并要求设置客户端 API key；`CNB_SESSION_COOKIE` 可以为空。Windows PowerShell 示例：

```powershell
$env:CNB_NPC2API_API_KEY = [Convert]::ToBase64String([Security.Cryptography.RandomNumberGenerator]::GetBytes(32))
$session = Read-Host "CNB session Cookie (optional)" -AsSecureString
$env:CNB_SESSION_COOKIE = [System.Net.NetworkCredential]::new("", $session).Password
Remove-Variable session
.\target\release\cnb-npc2api.exe --config config.example.yaml
```

以前的 `config.yaml` 仍可使用原有环境变量；新示例使用 `CNB_NPC2API_API_KEY`。多个 CNB 会话的配置和隔离边界见 [配置参考](docs/CONFIGURATION.md)。

需要配置其他上游时，可复制示例配置至 `config.yaml`，按 ToolForge 的格式添加 `openai_compat`、`anthropic` 或 `gemini` 项，并用 `${ENV}` 或 `${ENV:-default}` 引用环境变量。OpenAI 兼容上游的 `base_url` 通常以 `/v1` 结尾；Anthropic 和 Gemini 使用服务根地址。`routing.aliases` 可映射客户端模型名到指定上游及模型。

`server.timeout_seconds` 限制每次上游 HTTP 请求的总时长，包括读取响应流的时间。CNB 网络错误或 CSRF 失败时最多尝试三次，因此整个请求可能超过单次超时时间；获取 CNB 首页凭证使用单独的 15 秒超时。

客户端可使用当前进程的 `CNB_NPC2API_API_KEY` 访问服务。支持 Bearer、`x-api-key` 和 `x-goog-api-key`；开启鉴权时，空 key 列表会阻止启动。`/healthz` 无需鉴权；`/v1/models`、`/pool` 和 `/metrics` 需要鉴权。`/pool` 只返回有效性、借用数和剩余时间，不返回 Cookie 或 token。

```powershell
curl.exe http://127.0.0.1:8080/v1/models -H "Authorization: Bearer $env:CNB_NPC2API_API_KEY"
```

Docker 镜像会监听容器内 `0.0.0.0:8080`，必须提供 `CNB_NPC2API_API_KEY` 和所需的上游会话；示例：

```sh
docker build -t cnb-npc2api .
docker run --rm -p 127.0.0.1:8080:8080 -e CNB_NPC2API_API_KEY -e CNB_SESSION_COOKIE cnb-npc2api
```

也可先构建 `x86_64-unknown-linux-musl` 发布程序，再用 `docker build -f Dockerfile.musl -t cnb-npc2api:musl .` 构建不含 shell、以非 root 用户运行的精简镜像。此 Dockerfile 使用现有的 `target/x86_64-unknown-linux-musl/release/cnb-npc2api`，不在镜像构建阶段编译源码。

启动命令支持 `--reload`，每秒检查配置文件；配置有效且上游初始化成功后，新请求会使用新配置，已有请求继续使用旧配置。无效配置不会替换当前配置，监听地址或端口变更需要重启。该选项重载配置，不会自动编译 Rust 源码。

## 接口和边界

- 客户端：OpenAI Chat Completions、OpenAI Responses、Anthropic Messages、Gemini generateContent，支持非流式和 SSE。
- 上游：CNB、OpenAI 兼容、Anthropic 和 Gemini；按模型或别名路由。CNB 不发送原生 `tools`，通过 XYML 提示词和解析生成调用。其他上游可配置原生工具调用。
- 提示词工具调用支持 XYML、QNML、JSON/JSON5、常见 XML、宽松标记、部分 JSON 修复、必填参数检查和结构化参数转换。非流式检测到损坏的调用时可重试；流式提示词路径为避免泄漏未完成的标记，会缓冲模型文本直至解析完成。普通文本及原生工具路径按上游增量流出。对照样例覆盖了原 ToolForge 的部分解析行为，不代表逐项等价。
- Responses 支持 `previous_response_id`、`store`、查询和删除。状态按客户端 key 隔离，只存于进程内：最多 256 条，按序列化长度计算总计不超过 64 MiB、单条不超过 16 MiB，实际内存开销会更高；记录在 24 小时后过期，容量不足时淘汰最早记录，进程重启后丢失。流式响应仅在成功完成后保存；前一轮 `instructions` 不会自动继承。`conversation` 对象和后台响应不支持。
- 非函数工具只在客户端和上游协议匹配、且上游启用 `native_fc` 时原样交给上游处理；CNB、跨协议和强制提示词路径会拒绝。OpenAI Responses 的原生工具路径在本地保存历史，并向上游发送 `store: false`，以重放历史方式实现连续对话。具体工具可用性取决于上游和模型。
- 支持 OpenAI Chat/Responses、Anthropic 和 Gemini 的 PNG、JPEG、GIF、WebP 图片输入在三类兼容上游之间转换，包括 base64 和可转换的 URL；图片 URL 不由网关下载。CNB、工具结果内的图片、文件 ID、音频和视频仍会明确拒绝；`gs://` 图片仅能发往 Gemini，Gemini URL 需要已知媒体类型。图片细节参数无法在不同协议间完整等价。
- 当前仅支持单个候选结果。CLI 配置支持常见客户端提示规则和工具参数别名；原 ToolForge 的其他解析恢复细节、源码热重载和各类协议扩展不能视为已逐项等价移植。

## 测试

```sh
cargo test --locked
cargo build --locked
python -m pip install -r tests/sdk-requirements.txt
python tests/sdk_smoke.py
python tests/probe_cnb.py
python tests/live_cnb.py
```

解析器对照测试需要将原 ToolForge 检出到本项目的同级目录 `../toolforge`，并固定到 [开发文档](docs/DEVELOPMENT.md) 中列出的版本，然后运行 `python tests/parser_reference.py`。自动化测试会自行检出该版本。

Rust 测试和 `sdk_smoke.py` 默认使用本地模拟上游；`probe_cnb.py` 和 `live_cnb.py` 会访问真实 CNB，后者读取当前环境中的 `CNB_SESSION_COOKIE`，并发送两条简短测试请求。可用 `CNB_NPC2API_BINARY` 指定测试程序。Rust 测试覆盖四种客户端协议、四种上游、原生/提示词工具调用、流式/非流式组合，以及鉴权、凭证池、会话配对、取消、重试和故障处理。

也可运行 `python tests/probe_cnb.py --identity-only --session-prompt`，在不回显输入的提示中提供完整 Cookie 请求头的值，先检查登录状态；`python tests/live_cnb.py --session-prompt --state` 会进一步检查真实聊天、工具调用和 Responses 连续对话，`--state-only` 只执行后者。安装 SDK 测试依赖后，`python tests/sdk_smoke.py --live --session-prompt` 会通过真实 CNB 执行 16 个协议场景和两个 SDK 流式聚合场景。`--surface openai`、`--surface anthropic` 或 `--surface gemini` 可只运行对应场景。这些方式不将输入写入配置文件。

真实外部上游测试需要对应服务的 API key 和支持测试能力的模型，例如 `python tests/live_upstreams.py --provider openai --model MODEL_NAME --key-prompt --vision --native-tools`。`--provider` 也可设为 `anthropic` 或 `gemini`；`--vision` 和 `--native-tools` 各增加一次实际请求，原生搜索可能产生上游费用。省略这两个选项可只测试文本及函数工具。`--mock` 使用本地模拟服务，不需要密钥；不要在聊天中发送密钥。

原项目来源与许可见 [NOTICE.md](NOTICE.md)。
本次测试结果和未验证项目见 [TEST_REPORT.md](TEST_REPORT.md)。

`main` 分支的推送在 CI 全部通过后自动创建带版本号的 GitHub Release，提供 Windows x86_64、Linux x86_64 musl 程序及 SHA-256 校验和。版本规则和重试行为见 [开发与测试](docs/DEVELOPMENT.md)。
