# socks-server

基于 [fast-socks5](https://github.com/dizda/fast-socks5) 1.0.0 的 Rust SOCKS5 服务端。

- TCP CONNECT：支持 IPv4、IPv6、域名解析和双向传输。
- UDP ASSOCIATE：支持 IPv4、IPv6、域名目标，按 TCP 会话校验 UDP 客户端来源。
- 用户名密码认证与客户端 IP 限制独立配置，可以任意组合。
- IP 规则支持单个 IP 和 CIDR，拒绝规则优先，支持 IPv4 映射 IPv6 地址。
- JSON / 文本连接日志，包含连接标识、来源、目标、结果、时长和流量。
- 握手超时、目标连接超时、并发上限和优雅退出。

UDP 默认关闭，需要显式开启。当前不支持 BIND，收到该命令会返回 SOCKS5 `CommandNotSupported`。

## 启动

```sh
cp config.example.toml config.toml
# 按需编辑 config.toml
cargo run --release -- --config config.toml
```

默认只监听 `127.0.0.1:1080`，认证和 IP 限制均关闭。对外服务时修改 `listen`，并按需开启认证、IP 限制。配置中的 `listen` 必须是 IP:端口，IPv6 使用 `[::]:1080` 格式。

仅校验配置，不启动监听：

```sh
cargo run -- --config config.toml --check
```

## 构建与下载

GitHub Actions 的 **Build packages** 工作流会在向 `main` / `master` 推送、创建 `v*` 标签、提交 PR 或手动触发时运行。在 Actions 对应运行页面的 **Artifacts** 区域下载目标平台的包。

| 平台 | 架构 | Rust target | 包格式 |
| --- | --- | --- | --- |
| Linux musl | amd64 | `x86_64-unknown-linux-musl` | `.tar.gz` |
| Linux musl | arm64 | `aarch64-unknown-linux-musl` | `.tar.gz` |
| macOS | amd64 | `x86_64-apple-darwin` | `.tar.gz` |
| macOS | arm64 | `aarch64-apple-darwin` | `.tar.gz` |
| Windows | amd64 | `x86_64-pc-windows-msvc` | `.zip` |
| Windows | arm64 | `aarch64-pc-windows-msvc` | `.zip` |

六个目标均在对应架构的原生 runner 上测试和构建，使用 stable Rust 和 `Cargo.lock`。Linux 安装 musl 工具链，并检查成品不包含 ELF 动态解释器和动态库依赖；Windows 使用 MSVC。

每个包包含 `socks-server`（Windows 为 `socks-server.exe`）、`config.example.toml` 和本 README，旁边附有 `.sha256` 校验文件。文件名包含 `Cargo.toml` 中的版本号和平台，例如 `socks-server-v0.1.0-linux-arm64-musl.tar.gz`。Actions 上传的外层压缩包解压后，再解压其中的平台包即可使用；构建产物保留 30 天。

本机构建与打包示例（Apple Silicon）：

```sh
cargo build --locked --release --target aarch64-apple-darwin
python3 .github/scripts/package.py --target aarch64-apple-darwin
```

打包脚本需要 Python 3.11+，产物写入 `dist/`。

## 命令行配置

所有配置项都支持命令行，优先级为 **命令行 > 配置文件 > 默认值**。未显式指定 `--config` 时，会读取当前目录的 `config.toml`；若该文件不存在则使用默认值，可直接通过命令行启动。显式指定的文件不存在、不可读或内容有误时仍会报错。

只使用命令行（密码由进程环境变量 `SOCKS_PASSWORD` 提供）：

```sh
cargo run --release -- \
  --listen 0.0.0.0:1080 \
  --auth-enabled --username proxy --password-env SOCKS_PASSWORD \
  --ip-filter-enabled --allow 203.0.113.8,198.51.100.0/24 \
  --log-format json --log-level info
```

覆盖配置文件中的部分值：

```sh
cargo run --release -- --config config.toml \
  --listen 127.0.0.1:2080 --auth-enabled=false \
  --ip-filter-enabled=true --allow 127.0.0.1,::1 --clear-deny
```

| 配置项 | 命令行参数 |
| --- | --- |
| `listen` | `-l, --listen IP:PORT` |
| `handshake_timeout_secs` | `--handshake-timeout-secs SECONDS` |
| `connect_timeout_secs` | `--connect-timeout-secs SECONDS` |
| `shutdown_timeout_secs` | `--shutdown-timeout-secs SECONDS` |
| `max_connections` | `--max-connections COUNT` |
| `udp.enabled` | `--udp-enabled[=true\|false]` |
| `udp.public_ip` | `--udp-public-ip IP` |
| `udp.idle_timeout_secs` | `--udp-idle-timeout-secs SECONDS`（默认 60） |
| `auth.enabled` | `--auth-enabled[=true\|false]` |
| `auth.username` | `-u, --username USERNAME` |
| `auth.password` | `-p, --password PASSWORD` |
| `auth.password_env` | `--password-env ENV_NAME` |
| `ip_filter.enabled` | `--ip-filter-enabled[=true\|false]` |
| `ip_filter.allow` | `--allow IP_OR_CIDR...` / `--clear-allow` |
| `ip_filter.deny` | `--deny IP_OR_CIDR...` / `--clear-deny` |
| `logging.format` | `--log-format json\|text`（默认 `text`） |
| `logging.level` | `--log-level error\|warn\|info\|debug\|trace\|off` |

布尔开关不带值时表示 `true`，可用 `=false` 显式关闭；省略参数则保留配置值。提供用户名或 IP 列表不会自动启用对应功能。

`--allow` / `--deny` 支持逗号分隔、多个值和重复传参，命令行收集的列表会**替换**配置文件中的对应列表。`--clear-allow` / `--clear-deny` 将对应列表清空，不能与同名列表参数同时使用。

命令行 `--password` 会替换配置中的 `password` 或 `password_env`；`--password-env` 同样会替换配置中的密码来源。二者不能同时传入。合并完成后才读取密码环境变量并校验最终配置。`--check` 校验的也是合并后的结果。全部参数可通过 `--help` 查看。

## 密码与 IP 限制

| auth.enabled | ip_filter.enabled | 放行条件 |
| --- | --- | --- |
| false | false | 无需认证，不检查来源 IP |
| true | false | 用户名密码正确 |
| false | true | 来源 IP 通过规则 |
| true | true | 来源 IP 通过规则，且用户名密码正确 |

两者同时启用时，先检查来源 IP，再执行 SOCKS5 用户名密码认证。白名单中的 IP 不会跳过密码验证。

```toml
listen = "0.0.0.0:1080"

[auth]
enabled = true
username = "proxy"
password_env = "SOCKS_PASSWORD"

[ip_filter]
enabled = true
allow = ["203.0.113.8", "198.51.100.0/24", "2001:db8::/32"]
deny = ["198.51.100.23"]
```

上述地址为文档示例，需替换成实际客户端的来源 IP。密码通过启动进程的环境变量 `SOCKS_PASSWORD` 提供，也可以用 `password = "..."` 配置；`password` 和 `password_env` 只能选一个。用户名和密码各需 1–255 个 UTF-8 字节。

规则语义：

1. `ip_filter.enabled = false`：忽略所有 IP 规则。
2. 启用时，匹配 `deny` 立即拒绝。
3. 未匹配 `deny`，但匹配 `allow`，放行。
4. 其他情况拒绝；启用且 `allow = []` 时拒绝全部来源。

仅使用黑名单时，可以将 `allow` 设为 `["0.0.0.0/0", "::/0"]`。规则检查 TCP 对端地址；客户端经过 NAT 时对应公网出口 IP，经过前置代理时对应前置代理 IP。不读取客户端声明的来源地址。

SOCKS5 用户名密码认证本身不加密传输，公网使用时可配合 VPN / SSH 等加密通道。修改配置后需重启服务。

## 验证代理

未启用认证：

```sh
curl --proxy socks5h://127.0.0.1:1080 https://example.com
```

启用认证（省略密码，让 curl 交互式询问）：

```sh
curl --proxy socks5h://127.0.0.1:1080 --proxy-user proxy https://example.com
```

`socks5h` 让服务端解析目标域名。

## UDP 转发

```sh
cargo run --release -- -l 0.0.0.0:1080 \
  --auth-enabled -u proxy --password-env SOCKS_PASSWORD --udp-enabled
```

也可以通过配置文件启用，命令行仍优先于配置文件：

```toml
[udp]
enabled = true
idle_timeout_secs = 60
# public_ip = "203.0.113.10"
```

客户端必须支持 SOCKS5 `UDP ASSOCIATE`：先通过 TCP 完成认证和关联请求，按回复的 IP/端口发送带 SOCKS5 UDP 头的数据报，并保持 TCP 控制连接。普通 curl HTTP 请求只验证 TCP，不能验证 UDP 转发。

每个关联使用一个独立的随机 UDP 中继端口。默认绑定并通告当前 TCP 连接对应的服务端 IP，不会返回 `0.0.0.0` 或 `::`。直连部署需允许这些 UDP 端口通过防火墙；NAT / 容器部署可用 `--udp-public-ip` 指定客户端可达的外部 IP，并保证随机中继端口的内外映射一致。该选项只修改通告 IP，不会自动设置端口映射，也不会将 UDP 固定到 TCP 监听端口。通告 IP 必须与该连接的服务端地址使用相同地址族。

密码认证和 IP 限制沿用 TCP 会话的独立配置。UDP 只接受与已通过检查的 TCP 对端 IP 相同的来源。如果关联请求指定了非零 UDP 源端口，就严格检查该端口；端口为 0 时，锁定首个合法、未分片数据包的来源端口。之后其他端口无法接管该关联。经过 NAT 时，非零源端口应填写 NAT 后的端口；未知时填写 0。

一个关联可以访问多个目标；回包只允许来自该关联已经成功发送过数据的目标 IP/端口。每个关联最多记录 1024 个目标，记录在最后一次向该目标发送后经过 `idle_timeout_secs` 失效。域名由服务端异步解析，使用解析结果中首个可成功发送的地址；UDP 发送成功不保证目标收到数据。

TCP 控制连接关闭时立即释放 UDP 资源；持续无成功转发超过 `idle_timeout_secs` 时也会关闭关联。畸形包、未知来源和不支持的 SOCKS 分片（`FRAG != 0`）会被丢弃，且不刷新空闲计时。这里只处理完整 UDP 数据报，不实现 SOCKS 分片重组。服务停止时，UDP 会话使用同样的优雅退出期限。

## 连接日志

默认以文本输出到 stderr，可直接交给 systemd / 容器收集，或重定向保存；设置 `--log-format json` 或配置 `format = "json"` 则使用 JSON Lines：

```sh
cargo run --release -- --config config.toml 2>>connections.log
```

```toml
[logging]
format = "text" # 默认 text，可选 json
level = "info"
```

关键事件：

| 事件 | 内容 |
| --- | --- |
| `connection_accepted` | 客户端连接已接受 |
| `connection_rejected` | IP 被拒绝或达到并发上限，含 `reason` |
| `authentication_succeeded` / `authentication_rejected` | 密码认证结果 |
| `proxy_requested` | 请求命令和目标地址 |
| `proxy_connected` | 已连接目标，含实际 `upstream` 地址 |
| `udp_associated` | UDP 中继的绑定地址、通告地址及请求源端口 |
| `udp_client_bound` | 从首个合法数据包确定的客户端 UDP 地址 |
| `udp_destination` | 本次关联首次使用的目标，含目标域名/IP 与实际 `upstream` |
| `connection_closed` | `outcome`、`elapsed_ms`、`bytes_up`、`bytes_down`；失败时含 `stage` 和 `error` |

每个连接事件的 JSON `span` 带有 `connection_id`、`client_ip`、`client_port`，在认证和读取请求后补充 `username`、`protocol`（`tcp` / `udp`）和 TCP 的 `destination`。UDP 的每个目标记录在 `udp_destination` 事件中；UDP ASSOCIATE 请求中的地址是客户端来源提示，单独记录为 `client_hint`。连接 ID 在单次服务进程内唯一。日志不记录密码或代理内容；上游库日志始终关闭，因为其 debug 日志包含密码字节。通过配置文件或 `--log-level` 设置日志级别，`RUST_LOG` 不生效。

默认 `info` 记录完整连接生命周期；`warn` / `error` 会过滤较低级别事件。流量只统计代理阶段：`bytes_up` 为从客户端读取的字节数，`bytes_down` 为写回客户端的字节数，不含 SOCKS5 握手。传输异常时，上行可能包含尚未成功写入目标的缓冲数据。未进入代理阶段的流量为 0。日志文件轮转由运行环境负责。

UDP 的字节计数仅包含成功发送的应用载荷，不含 SOCKS5 UDP 头；`packets_up` / `packets_down` 记录成功发送的数据报数量（空数据报也计数，TCP 这两项为 0）。每个数据报的收发及丢弃原因以 debug 事件记录，默认 info 不逐包输出。

## 资源与退出

顶层配置项：`handshake_timeout_secs`（认证及命令请求总时限，默认 10 秒）、`connect_timeout_secs`（DNS 解析与目标连接总时限，默认 10 秒）、`max_connections`（默认 1024）。超时不限制已建立隧道的持续时间。

- **Ctrl-C / SIGINT**：停止接受新连接，立即关闭现有 TCP / UDP 会话，记录日志并退出。
- **SIGTERM**：停止接受新连接，等待现有连接结束；超过 `shutdown_timeout_secs`（默认 30 秒）后关闭剩余连接。设为 0 则立即关闭。
- SIGTERM 等待期间再次收到 SIGTERM 或 Ctrl-C，立即关闭剩余连接并退出。

退出日志会标明退出模式、剩余连接数和等待期限。

## 开发检查

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

测试使用本机 loopback，不依赖公网目标，覆盖认证/IP 开关四种组合、IPv4/IPv6/CIDR、拒绝优先级、认证失败、握手超时、并发限制、域名解析、半关闭、失败回复、退出，以及真实进程的连接日志与密码屏蔽。UDP 还覆盖多目标、空数据报、大报文、来源限制、端口锁定、未知回包、无效头/分片、控制连接生命周期、空闲超时及对外 IP。
