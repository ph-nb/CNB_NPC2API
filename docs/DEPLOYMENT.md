# 部署

## 本机运行

安装 Rust stable 和 C 编译器后，在项目目录运行：

```sh
cargo build --locked --release
```

Windows 使用 MSVC 时，需要 Visual Studio C++ 构建工具。Linux 上通常需要 GCC 或 Clang；程序使用 Rustls，不要求系统 OpenSSL 开发库。生成的程序分别是 `target/release/cnb-forge.exe` 和 `target/release/cnb-forge`。

PowerShell 7 示例，使用不回显的交互输入提供 CNB Cookie：

```powershell
$env:CNB_FORGE_API_KEY = [Convert]::ToBase64String([Security.Cryptography.RandomNumberGenerator]::GetBytes(32))
$session = Read-Host 'CNB Cookie header' -AsSecureString
$env:CNB_SESSION_COOKIE = [System.Net.NetworkCredential]::new('', $session).Password
Remove-Variable session
.\target\release\cnb-forge.exe --config config.example.yaml
```

POSIX shell 示例：

```sh
export CNB_FORGE_API_KEY="$(openssl rand -base64 32)"
printf 'CNB Cookie header: '
stty -echo
read -r CNB_SESSION_COOKIE
stty echo
printf '\n'
export CNB_SESSION_COOKIE
./target/release/cnb-forge --config config.example.yaml
```

上面的 Cookie 是完整 HTTP Cookie 请求头的值，字段名称和大小写必须与浏览器实际值一致。应从本机安全环境提供会话，不要粘贴到聊天、源码或 shell 历史。退出服务后可清理进程环境变量。匿名启动可能取得 CSRF 凭证，但真实聊天可能返回未登录。`RUST_LOG=info` 可启用启动和重载日志。

## 容器

标准镜像在构建阶段编译 Rust 程序，运行阶段使用非 root 用户：

```sh
docker build -t cnb-forge .
docker run --rm -p 127.0.0.1:8080:8080 \
  -e CNB_FORGE_API_KEY -e CNB_SESSION_COOKIE cnb-forge
```

容器从环境继承上述变量，容器内监听 `0.0.0.0:8080`，示例将宿主机端口限制在回环地址。使用自己的配置时可通过只读挂载传入，并将 `--config /app/config.yaml` 作为镜像参数传给程序；配置必须允许容器内的非 root 用户读取。环境变量和容器配置均可能被本机管理员查看，应限制对主机和 Docker 守护进程的访问。

可选的 scratch 镜像使用预先构建的静态 x86_64 程序：

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --locked --release --target x86_64-unknown-linux-musl
docker build -f Dockerfile.musl -t cnb-forge:musl .
```

在 Debian/Ubuntu 构建该目标通常需要安装 `musl-tools`。scratch 镜像没有 shell；需要通过宿主机 HTTP 请求检查健康状态，不能依赖容器内 `curl`。两个镜像均可使用 `python tests/docker_smoke.py IMAGE` 运行无真实上游的冒烟测试。

## 网络与维护

服务自身不提供 TLS、限流、持久会话存储或多副本状态同步。不要直接把未经保护的服务绑定到公网；如需远程访问，在受控反向代理或私有网络后部署，配置 TLS、访问控制和适当的请求限制。健康接口仅表明进程正在响应，不能证明 CNB 登录、上游配额或模型可用。监控应分别检查健康、鉴权和经过授权的实际模型请求。

更新时先保留可回滚的旧程序和配置；停止新流量并等待已有请求结束后再替换。进程重启会丢失已保存的 Responses 历史。配置热重载的行为和限制见 [配置参考](CONFIGURATION.md)。
