# 开发与测试

## 环境和源码

使用 Rust stable、系统 C 编译器以及 Python 3.11 或更高版本；CI 使用 Python 3.13。Windows 推荐 Rust MSVC 与 Visual Studio C++ 构建工具；Linux 可使用 GCC 或 Clang。测试依赖固定于 `tests/sdk-requirements.txt`。

`src/main.rs` 负责启动和监听；`config.rs` 处理配置及路由；`api.rs` 提供接口；`pool.rs` 管理 CNB CSRF 凭证；`upstream.rs` 发送上游请求；`model.rs`、`wire.rs`、`multimodal.rs`、`streaming.rs` 和 `fc.rs` 负责协议和工具调用；`response_store.rs` 管理进程内 Responses 历史；`reload.rs` 监测配置。

## 本地检查

在项目目录运行：

```sh
rustup component add rustfmt clippy
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
python -m pip install -r tests/sdk-requirements.txt
python tests/sdk_smoke.py
python tests/sdk_smoke.py --mock-live-inputs
```

以下脚本默认使用 `target/release/cnb-npc2api`，可以通过 `CNB_NPC2API_BINARY` 指向刚才构建的 debug 程序，或者先执行 `cargo build --locked --release`：

```sh
python tests/cli_reload_smoke.py
python tests/live_upstreams.py --provider openai --model mock --mock --vision --native-tools
python tests/live_upstreams.py --provider anthropic --model mock --mock --vision --native-tools
python tests/live_upstreams.py --provider gemini --model mock --mock --vision --native-tools
python -m unittest discover -s tests -p release_version_test.py
```

为兼容已有本地脚本，`CNB_FORGE_BINARY` 仍可作为 `CNB_NPC2API_BINARY` 未设置时的后备变量。

这些命令使用本地模拟服务，不需要真实 Cookie 或 API key。Rust 集成测试覆盖四类客户端和四类模拟上游的协议组合，以及认证、流、状态和错误处理。SDK 测试调用真实官方客户端库，但上游是模拟服务。

## ToolForge 解析器对照

Rust 测试会读取仓库中的固定语料。验证这些语料仍与原实现一致，需要在本项目同级目录检出原仓库的固定提交：

```sh
git clone https://github.com/YuJunZhiXue/toolforge.git ../toolforge
git -C ../toolforge checkout 8d5b3b06b7a82628cc5d916f72f0a81af33581f1
python tests/parser_reference.py
```

如果同级目录已有检出，先确认它没有需要保留的改动再切换提交。`python tests/parser_reference.py --generate` 会改写生成语料；更新语料后应运行 Rust 测试，并审查差异。当前语料只覆盖选定示例，不构成所有解析行为逐项等价的证明。

开发时如已安装 `cargo-watch`，可使用 `cargo watch -x 'run -- --config config.example.yaml'` 在源码变化后重新编译并重启程序。该命令会中断已有连接，不能代替服务的配置热重载。

## 真实上游测试

以下操作会访问第三方服务，可能产生请求费用或受配额限制。仅在本机提供凭据，不要将凭据传给 CI 或提交到仓库。

```sh
python tests/probe_cnb.py --identity-only --session-prompt
python tests/live_cnb.py --session-prompt --state
python tests/sdk_smoke.py --live --session-prompt
python tests/live_upstreams.py --provider openai --model MODEL_NAME --key-prompt --vision --native-tools
```

`live_cnb.py` 还支持 `--state-only`；SDK 脚本可使用 `--surface openai`、`anthropic` 或 `gemini`。`live_upstreams.py` 的 provider 可设为 `openai`、`anthropic` 或 `gemini`，需要具有相应能力的真实模型和密钥；`--vision`、`--native-tools` 各增加一次真实请求。测试前构建 release 程序或设置 `CNB_NPC2API_BINARY`。这些脚本提供不回显的输入方式，但测试数据仍会发送给指定上游。

## CI

`.github/workflows/ci.yml` 在 `main` push、pull request 和手动触发时运行。Windows 和 Ubuntu 原生 runner 分别执行格式、Clippy、Rust 测试、官方 SDK 模拟测试、CLI 重载测试和 ToolForge 语料对照；Ubuntu 还构建标准 Docker 镜像和 musl scratch 镜像，检查两者非 root 启动、健康、鉴权及模拟聊天。容器测试使用 Docker 的 `host-gateway` 功能连接宿主机上的临时模拟上游。CI 固定 ToolForge 参考提交和 GitHub Actions 的完整提交哈希，不配置真实上游凭据，也不发布镜像。CI 结果仅证明所执行场景，不能替代真实外部上游和生产负载测试。

## 自动 Release

只有 `main` 的 push 同时通过所有 CI 作业后，工作流才编译 Windows x86_64 MSVC 和 Linux x86_64 musl 发布程序，打包文档与示例配置，再创建 Git tag 和 GitHub Release，附带两个压缩包及 `SHA256SUMS.txt`。Pull request 和手动运行只测试，不发布；失败的测试或构建不会发布。发布前会检查该提交仍是最新的 `main`，过时的运行会跳过发布。

版本号取 `Cargo.toml` 的三段基础版本，加上当前 CI 的运行编号作为 patch 增量。例如基础版本为 `0.1.0`、运行编号为 `7` 时，标签为 `v0.1.7`。其他事件和失败的运行也会消耗编号，因此版本号可以有间隔；重新运行同一工作流使用同一个版本，已发布的同提交版本不会被覆盖。同名标签若指向不同提交会导致发布失败。Release 程序的 `--version` 和健康接口会显示该版本；本地普通构建显示 `Cargo.toml` 的基础版本。

历史验证记录及尚未验证的部分见 [测试记录](../TEST_REPORT.md)。
