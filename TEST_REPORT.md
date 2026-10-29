# 测试记录

日期：2026-10-29。以下记录区分真实服务、模拟上游、QEMU 和 Docker 环境；模拟结果不能替代真实服务验证。

本记录中的 `../.build-tools/` 路径指原测试机上的本地日志，不随源码仓库分发。GitHub Actions 的结果应以对应工作流运行记录为准。

## 2026-10-29 后续复测

- Windows GNU：33 项 Rust 测试、`cargo fmt --check`、Clippy 严格检查再次通过。
- 固定 ToolForge 版本的解析器对照：18 个定向样例与 100 个生成样例通过，包括零宽字符干扰的工具标记。
- 两轮官方 SDK 模拟测试分别通过 16 个客户端场景与两个流式聚合器；CLI 配置重载和三类模拟上游的文本、工具、图片及原生搜索透传通过。
- [GitHub Actions 运行 40901803583](https://github.com/ph-nb/CNB_NPC2API/actions/runs/40901803583) 针对提交 `8bdf2c674b49294e42f3c815cf7f2986e110e369` 全部通过：原生 Ubuntu 和 Windows 分别完成格式、Clippy、33 项 Rust 测试、ToolForge 对照、SDK、配置重载和三类模拟上游测试。
- 同一运行在原生 Ubuntu 上实际构建标准多阶段 Dockerfile 和 musl scratch 镜像，并检查两个镜像的非 root 用户、健康接口、鉴权、模型列表和经模拟上游的聊天请求。
- 首轮 Windows 和 Ubuntu CI 的 Clippy 在较新 Rust 上发现测试代码使用了弃用的原子 API；测试代码改用兼容的原子比较和交换后，以上两项作业通过。
- `cargo-audit` 0.22.2 更新 RustSec advisory-db 至 `ee1a49a2769a24a596afc1c20cb33748d2769586`，并更新 crates.io 索引后扫描 202 个锁定依赖；`--deny warnings` 通过，未报告漏洞或警告。

## 源码和产物

以下哈希对应早期本机测试产物，不能作为后续源码提交的构建产物哈希。

- `cnb2api`：`1bdc08116cdb9a92ee84869a15d49a5e2705bd00`，其 `docker/toolforge` 子模块已初始化到 `dc994e321e32455bb6520373e0eaaebf5a6acf9b`。
- `YuJunZhiXue/toolforge`：`8d5b3b06b7a82628cc5d916f72f0a81af33581f1`。两个原仓库工作树干净。
- Windows x86_64 GNU 发布程序：`target/release/cnb-forge.exe`，SHA-256 `8840c14fe0a291d95e8a900fbafa59e8ba25aa0cfa4422c23a3286798ce55180`。
- Linux x86_64 musl 发布程序：`target/x86_64-unknown-linux-musl/release/cnb-forge`，SHA-256 `601008e1a446a3ea02e511dec11d30191fc4ffc303fffc114d9fba9e3193e01c`。它是 ELF x86_64 可执行文件，没有动态解释器段。

## 已通过

- Windows：`build-windows.ps1 -Test`，33 项 Rust 测试通过；`cargo fmt --check` 和 `cargo clippy --locked --all-targets -- -D warnings` 通过。实际发布程序的 `--reload` 测试验证了 key 和模型更新、无效配置回退及进程持续运行。
- 隔离 QEMU 11.1 TCG / Alpine Linux 6.12.110：Linux musl 测试程序 33 项通过，使用 4 vCPU、1536 MiB、仅回环网络。日志为 `../.build-tools/linux-vm/cnb-forge-linux-final-current.log`。
- 隔离 QEMU Linux 中运行 Docker 29.9.2，使用 `vfs` 存储驱动；直接导入静态程序的容器，以及由 `Dockerfile.musl` 实际构建的非 root 镜像均通过健康检查和模型列表检查。虚拟机无外网，日志为 `../.build-tools/linux-vm/cnb-docker-final-current.log`。此项未构建仓库的多阶段 `Dockerfile`。
- 协议矩阵：56 组函数调用场景覆盖四类客户端、CNB 和三类外部模拟上游、原生/提示词模式与流式/非流式；另有 32 组普通文本场景。
- Windows 发布程序通过官方 Python SDK 模拟上游测试：OpenAI 3.40.0、Anthropic 2.3.0、google-genai 2.33.0，16 个客户端场景和两个 SDK 流式聚合器通过，共 18 次模拟 CNB 请求。
- 同一 Windows 发布程序通过真实 CNB 的官方 Python SDK 测试：OpenAI Chat、OpenAI Responses、Anthropic Messages、Gemini generateContent 各覆盖文本和工具调用的流式/非流式，共 16 个场景；Responses 和 Anthropic 的 SDK 流式聚合器也通过。另有真实 Responses 连续对话和工具结果回传测试通过。
- `tests/parser_reference.py` 对照原 ToolForge 解析器及 Rust 实现验证 17 个定向样例和 100 个生成样例；这些样例不能证明所有宽松格式逐项等价。
- 本地模拟的 OpenAI、Anthropic 和 Gemini 上游均通过官方 SDK、图片输入和原生搜索工具透传检查。图像转换覆盖四种客户端协议到三类模拟上游，无法转换的类型和 CNB 图片输入被拒绝。
- 模拟测试还覆盖身份隔离、Responses 查询/删除/流中断、原生工具历史、配置热重载、CSRF 重试、超时、错误脱敏及增量输出。
- `cargo-audit` 0.22.2 使用本地 RustSec advisory-db `40f5be6fb12ed93fc591bf4ab23b7d793b3f2239` 离线扫描 202 个锁定依赖，未报告已知漏洞或维护状态警告；数据库未在本轮更新。

## 真实 CNB 结果

匿名聊天曾返回 `401 [NOT_LOGIN]`。首次使用用户会话字段时因 Cookie 名称写错而返回 401；按浏览器实际名称 `CNBSESSION` 和 `csrfkey` 发送后，身份接口返回 200。本轮使用会话完成了上面列出的真实测试。会话值通过临时进程输入，没有写入项目源码、测试文件或本报告。

## 失败和限制

- 首轮 Gemini 模拟测试发现上游路径多一个斜杠并返回 404，修正后协议矩阵通过。
- Linux 交叉编译初次遇到 Zig 与 `ring` 的目标格式不匹配，随后遇到重复 CRT 启动符号；调整目标 C 编译参数和 Rust 链接选项后构建及 QEMU 测试通过。Linux 发布链接器仍提示忽略一个已弃用的优化选项，构建和启动检查成功。
- 首次依赖审计发现 `unsafe-libyaml` 已停止维护；替换 YAML 解析器后重新审计，结果见上。
- 复查发现 CNB 上游的请求超时固定为 180 秒，现已改为遵循 `server.timeout_seconds` 并加入回归测试。该设置适用于每次请求，CNB 失败重试可能使整体耗时更长。
- 真实 SDK 矩阵首次执行到 Anthropic 时，测试脚本向当前 Anthropic SDK 传入不支持的参数；删除该参数后，使用模拟上游验证测试脚本，再分别完成真实 Anthropic 和 Gemini 场景。
- Linux 热重载测试首次因 QEMU 测试镜像缺少 `/tmp` 失败；补齐目录后全套通过。Docker 预检先后遇到用户态代理配置、initramfs 的 `pivot_root` 限制和构建上下文排除 musl 程序；调整隔离环境与构建规则后通过。对应失败日志保留在 `../.build-tools/linux-vm/`。
- 本机没有原生 Linux、Docker 或已启用的 WSL/WHPX；本机的 Docker 结果来自 QEMU 客机。后续原生 Linux 和两种 Dockerfile 的验证来自上述 GitHub Actions。真实 OpenAI、Anthropic 和 Gemini 上游仍未测试，外部上游密钥未在本机环境中发现；可使用 `tests/live_upstreams.py` 本地隐藏输入测试。
- Responses 状态只保存在本进程内，24 小时过期且受数量和字节上限约束，重启后丢失；`conversation`、后台响应和多候选结果仍不支持。原生非函数工具仅同协议透传，跨协议及 CNB 不支持。文件 ID、音频、视频和部分多模态格式不支持。
- CLI `--reload` 监测配置，不能像原 Python 开发模式一样重编译源码；监听地址变更需重启。提示词模式仍缓冲流式工具文本，宽松解析及提示规则未证明与原 ToolForge 全部等价。
