# 受控网络部署 / Controlled-network deployment

## 中文说明

Unionid 的推荐远程部署是让数据库服务继续只监听 `127.0.0.1:7878`，由 Envoy 在受控网络边界终止 mTLS。仓库中的 [`deploy/envoy`](../deploy/envoy/) 是固定到 Envoy `v1.39.1` 的可执行参考配置：它要求客户端证书、只接受 TLS 1.3、限制 64 个活动连接、限制单连接缓冲，并记录不含 query、参数或返回值的连接审计事件。

```text
受控客户端 -- mTLS :8443 --> Envoy -- 明文 loopback :7878 --> Unionid -- redb 文件
                                |
                                +-- 客户端证书身份与连接元数据审计
```

这条路径提供粗粒度的数据库身份边界：被受信 CA 签发证书的客户端拥有其所连接服务的全部协议能力。Unionid 不引入用户、角色或查询语言权限语义。需要区分只读和写入调用方时，应运行独立的 `--read-only` 与可写实例，为它们分配不同的监听地址、网关和客户端 CA。

### 从本机到受控网络

先准备生产证书。`server.crt` 必须包含客户端连接使用的 DNS/IP SAN；`client.crt` 必须由网关信任的客户端 CA 签发。私钥由独立运行身份持有，建议目录 `0700`、私钥 `0400`，不要把 CA 私钥或客户端私钥复制到网关主机。下面的脚本只生成有效期两天的本机测试证书，不得用于生产：

```bash
certificate_dir="$(mktemp -d /tmp/unionid-certs.XXXXXX)"
deploy/envoy/generate-dev-certs.sh "$certificate_dir"
export UNIONID_CERT_DIR="$certificate_dir/gateway"
export UNIONID_ENVOY_UID="$(id -u)"
export UNIONID_ENVOY_GID="$(id -g)"
```

输出按 `authority/`、`gateway/`、`client/` 分离；Compose 只挂载 `0700` 的 `gateway/`，其中没有 CA 私钥或客户端私钥。Envoy 以该目录所有者的 UID/GID 运行，`server.key` 保持 `0400`。

在宿主机仅启动 loopback 服务：

```bash
unionid server \
  --db /var/lib/unionid/app.redb \
  --addr 127.0.0.1:7878
```

参考 Compose 使用 Linux host network，使容器中的 Envoy 能连接宿主机 loopback，同时不向容器挂载数据库。签入的 `envoy.yaml` 默认只监听 `127.0.0.1:8443`。先在本机启动并验证：

```bash
export UNIONID_ENVOY_CONFIG="$PWD/deploy/envoy/envoy.yaml"
docker compose --project-directory deploy/envoy \
  -f deploy/envoy/compose.yaml up -d
```

验证无误后，用渲染器显式选择受控私网地址；渲染器拒绝 `0.0.0.0` 和 `::`。宿主防火墙和上游网络策略仍需只允许预期来源：

```bash
install -d -m 0700 /etc/unionid
python3 deploy/envoy/render.py \
  --listen-address 10.20.0.15 \
  --listen-port 8443 \
  --output /etc/unionid/envoy.yaml
export UNIONID_ENVOY_CONFIG=/etc/unionid/envoy.yaml
export UNIONID_ENVOY_UID="$(id -u envoy)"
export UNIONID_ENVOY_GID="$(id -g envoy)"
docker compose --project-directory deploy/envoy \
  -f deploy/envoy/compose.yaml up -d
```

在非 Linux 主机上，应以原生 Envoy 服务运行渲染后的同一配置；Docker bridge/port publishing 无法连接只监听宿主机 loopback 的 Unionid，不要通过把 Unionid 改成 `0.0.0.0:7878` 绕过这一边界。

客户端使用 CA、证书和私钥建立 TLS 连接，然后在隧道内发送普通 version 1 JSON Line。查询语言和协议没有变化：

```bash
printf '%s\n' '{"version":1,"request_id":"gateway-read","query":"from tasks | take 10","params":{}}' |
  openssl s_client -quiet \
    -connect 127.0.0.1:8443 \
    -servername localhost \
    -verify_hostname localhost \
    -CAfile "$certificate_dir/client/ca.crt" \
    -cert "$certificate_dir/client/client.crt" \
    -key "$certificate_dir/client/client.key"
```

生产证书的 SAN 应使用真实网关名称，并把 `-servername` 改成该名称。参考配置只信任 CA；如果同一 CA 还签发其他用途的客户端证书，应在 `validation_context` 增加 `match_typed_subject_alt_names`，或为 Unionid 使用独立 CA。

从源码仓库可执行完整验收。它使用隔离的临时 redb 和两天证书，验证有证书读取、无证书拒绝、Envoy 审计身份、`SIGTERM` 优雅关闭、重开完整检查，以及 `--read-only` 后写入返回 `E_READ_ONLY`：

```bash
cargo build --locked --bin unionid
deploy/envoy/verify.sh target/debug/unionid
```

该验收需要 Linux、Docker Compose v2、OpenSSL 和 Python 3；会占用本机 `7878` 与 `8443`。发布包中把二进制参数改成 `bin/unionid`。普通 PR 的 Ubuntu CI 只检查配置内部约束，容器级验收仅在发版 workflow 的 Linux job 运行，避免增加日常和 macOS 检查时间。

### 权限与审计边界

- `create`、`insert`、`upsert`、`update`、`delete`、migration，以及 receipt prune 都是写权限。receipt status 会暴露重试窗口和提交序列，也必须限制给运维身份。任何可写客户端证书都应视为数据库写入凭据。
- Envoy 的 JSON access log 记录客户端地址、证书 subject、证书 SHA-256 指纹、TLS 版本、字节数和关闭 flags。它刻意不记录协议 payload、query、参数、row、幂等 key 或 stream cancel capability。日志存储仍需访问控制与保留期限。
- raw TCP 是 L4 隧道，Unionid 看不到 TLS 身份，Envoy 也不能可靠判断一行 payload 是读取、写入还是 receipt 操作。需要逐请求 principal 与操作分类时，应使用嵌入式 HTTP adapter：HTTP 网关先删除外部传入的身份 header，再从已验证证书或 OIDC 结果写入规范身份；应用中间件在调用 `unionid::asynchronous::http::router` 前完成 route/request 鉴权并记录 value-free 操作类型。不要把未验证的 header 当作身份。
- 数据库、backup 和 migration 文件只对 Unionid 的专用操作系统用户开放。Envoy 容器只挂载 TLS 材料，不能挂载 redb 路径。`--read-only` 是执行边界，不代替文件权限。

HTTP mTLS 网关可在 `HttpConnectionManager` 使用下面的身份转发模式。`SANITIZE_SET` 会替换客户端提供的 XFCC，再从当前已验证证书构造 subject/URI SAN；应用只允许 loopback 或专用内部 socket 上的 Envoy 访问该 HTTP listener，并解析该规范 header：

```yaml
forward_client_cert_details: SANITIZE_SET
set_current_client_cert_details:
  subject: true
  uri: true
```

每条 HTTP route 仍由应用映射为 read、write 或 receipt-operations authority，并在进入 Unionid router 前拒绝越权请求。不要转发整个证书/证书链，也不要记录 XFCC、Authorization 或 query body。

### Deadline、关闭与失败模式

raw TCP 网关无法传递逐请求 deadline；Unionid 的 25 秒执行 deadline 是权威边界。Envoy 的 35 秒连接 idle timeout 比它长，允许服务先返回结构化 `E_TIMEOUT`。嵌入式 HTTP adapter 应把 `Config::request_timeout` 设置得短于外层 HTTP 网关 timeout，给错误响应和审计留出时间。

维护时先让负载均衡器停止新流量并 drain Envoy，再向 Unionid 发送 `SIGTERM`。Unionid 停止接纳连接，等待已经进入 Engine 的请求完成或到达 deadline，释放 redb 锁后再停止网关。客户端断开不撤销已提交写入；可安全重试的写入仍需原样复用 `idempotency_key`。

| 失败 | 可观察结果 | 操作 |
| --- | --- | --- |
| 缺少、不受信或过期的客户端证书 | TLS handshake 失败，Unionid 不收到请求 | 修复证书链；不要降级到明文 |
| Unionid 未运行或 loopback 不可达 | Envoy 关闭连接并记录 upstream failure flag | 检查服务日志、redb 锁和 `doctor/check` |
| 超过 64 个网关连接 | Envoy connection-limit filter 关闭新增连接 | 客户端退避；检查连接泄漏和服务指标 |
| 请求执行超过 25 秒 | Unionid 返回 `E_TIMEOUT`，候选写入不提交 | 缩小工作集或增加 indexed filter |
| 网关 idle timeout | 连接在 35 秒无活动后关闭 | 重连；幂等写使用相同 key 和完整请求 |
| 证书被盗或错误签发 | 该证书获得对应实例的完整权限 | 吊销/轮换证书与 CA，检查网关和 Unionid 审计 |

参考字段来自 Envoy 官方的 [mTLS 指南](https://www.envoyproxy.io/docs/envoy/v1.39.1/start/quick-start/securing.html#use-mutual-tls-mtls-to-enforce-client-certificate-authentication)、[connection-limit filter](https://www.envoyproxy.io/docs/envoy/v1.39.1/configuration/listeners/network_filters/connection_limit_filter)、[XFCC 处理](https://www.envoyproxy.io/docs/envoy/v1.39.1/configuration/http/http_conn_man/headers#x-forwarded-client-cert)与 [access-log formatter](https://www.envoyproxy.io/docs/envoy/v1.39.1/configuration/advanced/substitution_formatter)。部署时应跟进 Envoy 安全修复，并在升级镜像后重新运行验收。

## English Description

The recommended remote topology keeps Unionid on `127.0.0.1:7878` and terminates mTLS at Envoy on a controlled network boundary. The executable reference under [`deploy/envoy`](../deploy/envoy/) pins Envoy `v1.39.1`, requires a client certificate and TLS 1.3, caps active connections at 64, bounds per-connection buffering, and emits connection audit events without query, parameter, or result values.

This is a coarse database authority boundary: every certificate signed by the trusted client CA receives every protocol capability exposed by that Unionid instance. Unionid does not add users, roles, or query-language authorization. Run separate writable and `--read-only` instances with distinct listeners, gateways, and client CAs when callers need different authority.

Generate short-lived development certificates and start the default loopback-only gateway with:

```bash
certificate_dir="$(mktemp -d /tmp/unionid-certs.XXXXXX)"
deploy/envoy/generate-dev-certs.sh "$certificate_dir"
export UNIONID_CERT_DIR="$certificate_dir/gateway"
export UNIONID_ENVOY_UID="$(id -u)"
export UNIONID_ENVOY_GID="$(id -g)"
export UNIONID_ENVOY_CONFIG="$PWD/deploy/envoy/envoy.yaml"
docker compose --project-directory deploy/envoy \
  -f deploy/envoy/compose.yaml up -d
```

The generator separates `authority/`, `gateway/`, and `client/`. Compose mounts only the `0700` gateway directory, which contains neither the CA private key nor the client private key, and runs Envoy with that directory owner's explicit UID/GID. `server.key` remains `0400`.

Start Unionid separately with `unionid server --db /var/lib/unionid/app.redb --addr 127.0.0.1:7878`. The Compose reference uses Linux host networking so Envoy can reach host loopback without mounting the database, and the checked-in config listens only on `127.0.0.1:8443`. Render an explicit controlled address with `python3 deploy/envoy/render.py --listen-address 10.20.0.15 --listen-port 8443 --output /etc/unionid/envoy.yaml`, export that absolute path as `UNIONID_ENVOY_CONFIG`, and restrict sources with host and upstream network policy. The renderer rejects unspecified addresses. On non-Linux hosts, run native Envoy with the same rendered config; do not expose Unionid on `0.0.0.0:7878` to make Docker bridge networking work.

Production keys should be owned by dedicated identities with `0700` certificate directories and `0400` private keys; never place the CA or client private key on the gateway host.

Use a production server certificate whose SAN matches the gateway name. The checked-in config trusts the complete client CA; add `match_typed_subject_alt_names` or use a dedicated Unionid CA if that CA issues certificates for other purposes. A client sends the unchanged JSON Lines protocol through the authenticated TLS tunnel, as in the `openssl s_client` command in the Chinese walkthrough.

Run the real journey with:

```bash
cargo build --locked --bin unionid
deploy/envoy/verify.sh target/debug/unionid
```

It verifies an authenticated read, rejection without a client certificate, audited certificate identity, graceful `SIGTERM`, a full database check after reopen, and `E_READ_ONLY` for a write through a restarted read-only service. It requires Linux, Docker Compose v2, OpenSSL, and Python 3 and occupies local ports `7878` and `8443`; use `bin/unionid` as the argument inside a release package. Daily Ubuntu CI checks only the reference invariants; the container journey runs in the Linux release job and never adds a routine macOS job.

Treat schema/data mutations, migrations, and receipt pruning as write authority. Receipt status exposes retry-window and commit-sequence metadata and also belongs behind an operations identity. The Envoy log records the peer address, certificate subject and SHA-256 fingerprint, TLS version, byte counts, and response flags. It deliberately excludes payloads, queries, parameters, rows, idempotency keys, and stream capabilities.

Raw TCP is an L4 tunnel: Unionid cannot see the TLS principal, and Envoy cannot reliably classify a payload line as a read, write, or receipt operation. For per-request principals, use the embedded HTTP adapter. The HTTP-aware gateway must remove client-supplied identity headers and inject only an identity derived from verified mTLS or OIDC state. Application middleware must authorize and emit a value-free operation audit event before calling `unionid::asynchronous::http::router`. Keep redb, backup, and migration paths readable only by the dedicated Unionid OS identity; the Envoy container mounts certificates only.

For HTTP mTLS, configure `HttpConnectionManager` with `forward_client_cert_details: SANITIZE_SET` and `set_current_client_cert_details: {subject: true, uri: true}`. Envoy then replaces any client-supplied XFCC value with identity derived from the verified certificate. Restrict the embedded HTTP listener to loopback or a dedicated internal socket, parse only that canonical header, map every route to read, write, or receipt-operations authority, and reject it before entering the Unionid router. Do not forward whole certificates/chains or log XFCC, Authorization, or query bodies.

The L4 gateway cannot propagate a per-request deadline. Unionid's 25 seconds remains authoritative, while Envoy's 35-second idle timeout leaves time for a structured `E_TIMEOUT`. An embedded HTTP service should set `Config::request_timeout` below its outer HTTP-gateway timeout. During maintenance, stop new load-balancer traffic, drain Envoy, send `SIGTERM` to Unionid, wait for accepted work to finish or reach its deadline and for the redb lock to be released, then stop the gateway. A disconnected client does not undo a committed write; safe retry still requires the identical request and `idempotency_key`.

Expected failures are explicit: invalid or absent certificates fail the TLS handshake; unavailable loopback upstreams produce an Envoy upstream failure; excess connections are closed by the 64-connection filter; Unionid returns `E_TIMEOUT` at 25 seconds; idle tunnels close after 35 seconds. A stolen trusted certificate has the full authority of its instance and requires certificate/CA rotation plus an audit review.

The reference follows Envoy's official [mTLS guide](https://www.envoyproxy.io/docs/envoy/v1.39.1/start/quick-start/securing.html#use-mutual-tls-mtls-to-enforce-client-certificate-authentication), [connection-limit filter](https://www.envoyproxy.io/docs/envoy/v1.39.1/configuration/listeners/network_filters/connection_limit_filter), [XFCC handling](https://www.envoyproxy.io/docs/envoy/v1.39.1/configuration/http/http_conn_man/headers#x-forwarded-client-cert), and [access-log formatter](https://www.envoyproxy.io/docs/envoy/v1.39.1/configuration/advanced/substitution_formatter). Track Envoy security releases and rerun the journey after image upgrades.
