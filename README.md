# socks-server

基于 [fast-socks5](https://github.com/dizda/fast-socks5) 1.0.0 的 Rust SOCKS5 服务端。

- TCP CONNECT：支持 IPv4、IPv6、域名解析和双向传输。
- 用户名密码认证与客户端 IP 限制独立配置，可以任意组合。
- IP 规则支持单个 IP 和 CIDR，拒绝规则优先，支持 IPv4 映射 IPv6 地址。
- JSON / 文本连接日志，包含连接标识、来源、目标、结果、时长和流量。
- 握手超时、目标连接超时、并发上限和优雅退出。

当前不支持 UDP ASSOCIATE 和 BIND，收到这两种命令会返回 SOCKS5 `CommandNotSupported`。

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
| `connection_closed` | `outcome`、`elapsed_ms`、`bytes_up`、`bytes_down`；失败时含 `stage` 和 `error` |

每个连接事件的 JSON `span` 带有 `connection_id`、`client_ip`、`client_port`，在认证和读取请求后补充 `username`、`destination`。连接 ID 在单次服务进程内唯一。日志不记录密码或代理内容；上游库日志始终关闭，因为其 debug 日志包含密码字节。通过配置文件或 `--log-level` 设置日志级别，`RUST_LOG` 不生效。

默认 `info` 记录完整连接生命周期；`warn` / `error` 会过滤较低级别事件。流量只统计代理阶段：`bytes_up` 为从客户端读取的字节数，`bytes_down` 为写回客户端的字节数，不含 SOCKS5 握手。传输异常时，上行可能包含尚未成功写入目标的缓冲数据。未进入代理阶段的流量为 0。日志文件轮转由运行环境负责。

## 资源与退出

顶层配置项：`handshake_timeout_secs`（认证及命令请求总时限，默认 10 秒）、`connect_timeout_secs`（DNS 解析与目标连接总时限，默认 10 秒）、`max_connections`（默认 1024）。超时不限制已建立隧道的持续时间。

收到 Ctrl-C / SIGTERM 后停止接受新连接，等待现有连接结束；超过 `shutdown_timeout_secs`（默认 30 秒）后关闭剩余连接并记录日志。设为 0 则立即关闭。

## 开发检查

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

测试使用本机 loopback，不依赖公网目标，覆盖认证/IP 开关四种组合、IPv4/IPv6/CIDR、拒绝优先级、认证失败、握手超时、并发限制、域名解析、半关闭、失败回复、退出，以及真实进程的连接日志与密码屏蔽。
