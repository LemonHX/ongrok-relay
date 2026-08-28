# ongrok 开发计划

> 本计划覆盖当前 MVP 到可发布版本的开发、测试、E2E 和前端工作。产品行为见 `design.md`，具体库和协议约束见 `techspec.md`。

## 0. 当前版本边界

### 必须实现

- 一个 Rust workspace。
- 两个 binary crate：`ongrok-relay-server`、`ongrok-relay-client`。
- 一个 library crate：`libongrok`。
- server/client 之间的长连接 multiplexing。
- QUIC 首选传输。
- UDP 不可用时的 TCP + TLS + Yamux fallback。
- admin/user 两类长期 token。
- client 节点自动身份：`node_id` + 密钥对；不要求 `--name`，不使用 MAC 地址。
- HTTP/HTTPS 域名转发。
- 原始 TCP 端口租约转发。
- services list：API、前端、client CLI 使用同一数据模型。
- 每分钟协议心跳、RTT、在线状态和基础机器负载。
- redb 控制面持久化。
- server 从用户指定的 PEM full chain 和私钥启动，不负责证书签发或续期。
- 可视化控制台：服务、节点、连接、延迟和机器负载。

### 明确不进入当前 MVP

- client-to-client P2P 打洞。
- TUN/VPN/network mode。
- 本地 DNS、DoH、系统 DNS hook。
- 多 server 同时连接。
- 复杂 WAF、P50/P95、外部探针集群。
- 社交用户注册和复杂组织权限。
- 浏览器长期保存 admin/user 长 token 的“记住我”。

## 1. Workspace 目标结构

```text
ongrok/
├── Cargo.toml                 # virtual workspace
├── Cargo.lock
├── crates/
│   ├── libongrok/
│   │   ├── Cargo.toml
│   │   └── src/
│   ├── ongrok-relay-server/
│   │   ├── Cargo.toml
│   │   └── src/main.rs
│   └── ongrok-relay-client/
│       ├── Cargo.toml
│       └── src/main.rs
├── frontend/
├── tests/
│   ├── fixtures/
│   ├── e2e/
│   └── scripts/
├── design.md
├── techspec.md
└── PLAN.md
```

workspace 根目录使用 `[workspace.dependencies]` 统一版本。所有新依赖用 `cargo add` 加入，提交 `Cargo.lock`；禁止手工复制版本号造成 workspace 漂移。

## 2. 开发顺序总览

1. Workspace 和编译基线。
2. `libongrok` 的公共类型、错误、ID、配置和 wire frame。
3. QUIC transport adapter。
4. TCP/TLS/Yamux fallback adapter。
5. server 内存路由和端口监听。
6. client 本地连接和 stream 转发。
7. token、节点注册、服务注册和 redb repository。
8. heartbeat、负载快照和状态事件。
9. Hyper HTTP/HTTPS ingress、控制 API 和外部证书加载。
10. 前端控制台。
11. musl 构建、部署和全链路 E2E。

每一步都要求：代码、单元测试、失败路径测试、日志、文档和最小可运行示例一起完成。

## 3. `libongrok` 计划

### 3.1 公共领域模型

定义并测试：

- `NodeId`、`ServiceId`、`TunnelId`、`PortLeaseId`：内部使用 UUID v7。
- `TokenKind`：`Admin` / `User`。
- `Protocol`：`Http` / `Https` / `Tcp`。
- `TransportKind`：`Quic` / `TcpTlsYamux`。
- `ServiceStatus`、`ConnectionStatus`、断开原因。
- `NodeMetadata`、`ServiceMetadata`。
- `PublicEndpoint`：HTTP 域名或 TCP host/port。
- `HeartbeatSnapshot`：RTT、CPU、内存、load、磁盘和网络累计字节。

为所有 API 响应定义稳定的 serde 结构；未知字段可忽略，枚举新增值不能导致旧 client 崩溃。

### 3.2 Wire frame

实现长度前缀 + frame type + request/stream id + payload 的 framing。控制消息使用 `serde` + `postcard`；数据 payload 使用 `bytes` 原样转发。

至少实现：

```text
Hello
Auth
AuthAccepted / AuthRejected
RegisterNode
RegisterService
UnregisterService
ServiceList
Heartbeat
HeartbeatAck
OpenStream
OpenStreamAck
StreamData
CloseStream
HttpRequestHead
HttpRequestBody
HttpRequestEnd
HttpResponseHead
HttpResponseBody
HttpResponseEnd
Error
Goodbye
```

测试：半包、粘包、超长 frame、未知 frame、错误 request id、重复注册、版本不兼容、恶意长度字段，以及 HTTP body 空帧/结束帧/提前关闭。

### 3.3 Transport trait

抽象出统一的 `TransportConnection` / `MuxStream` 接口，使 QUIC 和 TCP fallback 复用同一套上层逻辑：

- 建立连接。
- 完成认证。
- 打开控制 stream。
- 打开/关闭数据 stream。
- 读写 payload。
- 获取 transport kind。
- 关闭原因和错误分类。

QUIC 使用 Quinn 原生双向 stream；Yamux 只在 TCP/TLS fallback 使用，不能在 QUIC 上重复套 Yamux。

### 3.4 安全基础

- 使用 rustls，不引入 OpenSSL/native-tls。
- token 由 CSPRNG 生成，数据库保存 BLAKE3 hash。
- 节点密钥对首次启动生成并持久化。
- token 比较使用恒时比较或 hash API 的安全比较方式。
- 所有长度、并发数和缓冲区都有限制。

## 4. `ongrok-relay-server` 计划

### 4.1 启动与配置

使用 `clap`：

```text
ongrok-relay-server init
ongrok-relay-server run --tls-cert /path/to/fullchain.pem --tls-key /path/to/private-key.pem
ongrok-relay-server token create --kind user
ongrok-relay-server token revoke <id>
ongrok-relay-server doctor
```

当前已实现 `init`（创建空 redb 数据库并生成 admin/user token）和 `doctor`（证书链/私钥匹配与数据库可读性）。配置文件优先级、端口/权限检查和 QUIC UDP 可达性提示仍待实现；目标顺序为 CLI > environment > config file > defaults。

### 4.2 Listener

至少包含：

- QUIC/UDP listener。
- TCP/TLS fallback listener。
- 公网 HTTP/HTTPS ingress listener。
- 控制 API/health listener。

所有 listener 都使用 rustls 相关配置；不得因为启用 fallback 而引入另一套 TLS 实现。

HTTP 栈分层：

- 公网 HTTP/HTTPS 数据面直接使用 Hyper，负责连接生命周期、流式 body、反向代理、header 修改、限流/WAF 扩展点和 tunnel backpressure。
- 控制 API 也直接使用 Hyper，避免为一个后端引入两套 HTTP 路由抽象；路由、认证、JSON 编解码、错误响应和 body 限制由本项目自己的 service 层完成。
- ingress 和控制 API 使用独立 router/service 与独立监听地址，避免数据面请求进入管理中间件链。
- Hyper 后端统一采用 `hyper::server::conn`/`hyper-util` connection service 模式；公网 ingress 与控制 API 分别拥有独立的 `Service`、超时、并发限制和 tracing span。
- 控制 API 使用明确的 method/path 匹配和版本化 `/v1` 路径；统一返回 JSON 错误结构，不把内部错误或 token 内容泄露给客户端。
- API 认证从 `Authorization: Bearer` 解析 token，admin/user 能力在 service 层校验；不得依赖前端隐藏按钮实现权限控制。
- API 请求 body、header、URI、并发连接和响应大小均设置上限；JSON 解析失败、未知路由、超时和取消都必须返回稳定的 HTTP 状态码。
- 使用 Hyper 的流式 body 与 backpressure；控制 API 仅在需要时聚合小 JSON body，数据面绝不无界缓存请求或响应。
- 统一处理优雅停机：停止接受新连接，等待 in-flight request/tunnel 到超时，再关闭 QUIC、TCP 和 HTTP listeners。
- 不在数据面缓存完整 request/response body；默认以 bounded streaming 方式经过 tunnel。
- Hyper 在 server 终止公网 HTTP/TLS，解析 method、URI、headers 和 body；request/response head 与流式 body 使用专用 tunnel frame 传输。
- client 在目标侧建立本地 HTTP connection 并重建请求，返回响应 head/body；hop-by-hop headers 必须清理。
- WebSocket/HTTP Upgrade 使用独立升级路径；升级成功后切换为双向字节流。

### 4.3 TLS 证书

server 只消费用户提供的两个文件：

```text
--tls-cert /path/to/fullchain.pem
--tls-key  /path/to/private-key.pem
```

要求：

- `--tls-cert` 是 PEM 格式的完整证书链。
- `--tls-key` 是与叶子证书匹配的 PEM 私钥。
- 启动时校验证书解析、有效期、域名提示信息和私钥匹配；错误时 fail fast。
- Quinn、TCP/TLS/Yamux 和 HTTPS ingress 复用同一份证书材料。
- server 不实现 ACME、Certbot、DNS-01、自动续期或证书安装。
- 第一版不实现热加载；证书更新后由部署者重启 server。

### 4.4 Control plane

redb repository 实现：

- token hash、kind、状态、创建/撤销/轮换时间。
- node、public key、hostname、OS、架构、client version。
- service、metadata、本地目标、协议。
- HTTP hostname 绑定。
- TCP port lease。
- 低频状态事件和心跳快照。

数据面实时连接、stream 路由、并发连接计数放内存；连接/租约变化再持久化。

### 4.5 Relay routing

实现以下路由链：

```text
public TCP port -> service_id -> node session -> data stream -> local target
HTTP Host/SNI   -> service_id -> node session -> data stream -> local target
```

必须处理：服务离线、client 重连、端口冲突、重复 service name、半关闭、客户端主动关闭、公网访问者超时、节点被撤销。

### 4.6 Admin/API（Hyper 原生）

至少提供：

```text
GET  /healthz
GET  /readyz
POST /v1/auth/validate
GET  /v1/services
GET  /v1/services/{id}
POST /v1/services
PATCH /v1/services/{id}
DELETE /v1/services/{id}
GET  /v1/nodes
GET  /v1/nodes/{id}
GET  /v1/nodes/{id}/metrics
GET  /v1/events
POST /v1/admin/tokens/rotate
POST /v1/admin/tokens/revoke
```

实现拆分：

- `api::router`：method/path 匹配、版本前缀和路由分发。
- `api::auth`：Bearer token 提取、hash 校验、admin/user capability。
- `api::json`：请求反序列化、响应序列化和统一错误 envelope。
- `api::handlers`：调用 repository 与内存状态，不直接操作 socket。
- `api::server`：Hyper listener、连接级配置、超时、并发限制、trace 和 graceful shutdown。

不引入 Axum；若未来需要复用 Tower middleware，只将其作为 Hyper `Service` 的装饰层，不改变 API 的核心路由实现。

## 5. `ongrok-relay-client` 计划

### 5.1 CLI

使用 `clap`，不要求 `--name`：

```text
ongrok-relay-client run --server https://relay.example
ongrok-relay-client service publish --name ssh --tcp 127.0.0.1:22
ongrok-relay-client service publish --name web --http 127.0.0.1:3000
ongrok-relay-client services list
ongrok-relay-client status
ongrok-relay-client doctor
```

token 可以通过显式参数、环境变量或权限受限的配置文件提供；命令行参数不得被普通日志打印，诊断日志也不得包含 token 明文。

### 5.2 本地持久化

首次运行生成 node id 和 Ed25519 密钥对，写入跨平台 state directory；Unix state file 使用 `0600`。旧版仅有 `node_id` 的状态文件会原地迁移并保留既有 node id。配置文件不记录 token 明文以外的诊断日志。

### 5.3 连接状态机

```text
Disconnected
  -> QuicConnecting
  -> Authenticated
  -> Registered
  -> Running
  -> QuicFailed / Disconnected
  -> TcpTlsYamuxConnecting
  -> Running
```

包含指数退避、随机抖动、连接超时、认证失败不重试、token revoke 立即停止、网络恢复自动重连。

### 5.4 本地转发

- 发布服务前校验本地地址格式和协议。
- 每个 server stream 对应一个本地 TCP connection。
- 双向 copy 使用受限 buffer 和 backpressure。
- 本地连接关闭要通知 server；server 关闭也要释放本地 socket。
- `services list` 显示 server 返回的完整公开地址、在线状态、RTT、transport 和 metadata。

## 6. Token 登录与前端认证

当前产品只有两类长期 token，不在 MVP 强行增加第三类短 token。

### 6.1 Web 登录方案

前端提供 token 输入页：

- 用户粘贴 admin 或 user token。
- 调用 `/v1/auth/validate`。
- server 返回 token kind、可见能力和当前 server 信息。
- token 只保存在浏览器内存中，刷新页面后重新输入。
- 不写入 URL、日志、localStorage、分析事件或错误上报。
- API 请求使用 `Authorization: Bearer`。

后续若需要“保持登录”，再增加短期 HttpOnly session cookie；它属于后续安全增强，不改变 client 使用长期 user token 的模型。

### 6.2 登录测试

- admin token 能进入管理页面，user token 不能进入 admin 操作。
- user token 能查看完整 services list。
- 错误、撤销、过期格式 token 都得到统一错误，不泄露 token 是否部分匹配。
- token 不出现在浏览器 URL、console、网络错误正文和 server 普通日志。
- 轮换后旧 token 立即失效，新 token 可用。

## 7. Frontend 计划

frontend 先参考 `../matrix` 的 layout/theme，但不复制业务组件。目标是工作台，而不是营销 landing page。

### 7.1 页面

- 登录页：token 输入、server 地址、连接状态和错误反馈。
- Overview：在线节点数、服务数、当前连接、relay 流量和告警。
- Services：完整服务表格、协议、公开地址、状态、RTT、transport、metadata。
- Nodes：hostname、node id、公网 IP、连接源端口、OS、client version、当前负载。
- Service detail：连接历史、实时连接、字节数、状态时间线、metadata。
- Node detail：CPU/内存/load/网络图表、心跳时间线、连接事件。
- Admin settings：token 轮换/撤销、端口范围、域名和限额；证书页只读展示 server 当前加载的证书信息。

### 7.2 可视化

- 在线/离线时间线，不显示 uptime 百分比。
- RTT 折线图，默认展示最近 1 小时，可切换 24 小时/7 天。
- CPU、内存、load、网络累计字节图表。
- 当前值用数字和状态标识，历史值用图表。
- 图表必须处理断线空洞、`null` 采样、时间区间和时区。
- 不用过度装饰性的渐变、浮夸大卡片或不可读的动画；优先扫描、比较和操作。

### 7.3 前端测试

- 组件测试：表格、状态 badge、图表空状态、错误状态、token 表单。
- API mock 测试：admin/user 能力差异、分页/筛选、断线和重试。
- Playwright：登录、查看服务、查看节点、切换时间范围、撤销 token。
- 视觉回归：桌面宽屏、笔记本、移动窄屏；检查文字溢出、图表空白和按钮遮挡。

## 8. 心跳与机器负载

### 8.1 协议行为

- 周期固定为 60 秒。
- heartbeat 带序列号、发送时间、负载快照和 transport 信息。
- server 返回 heartbeat ack，client 用发送/接收时间计算 RTT。
- 连续超时后将节点标记离线，并释放或过期处理 tunnel。

### 8.2 负载字段

第一版字段：

- CPU 使用率。
- 内存使用率/已用量。
- load average（平台支持时）。
- 磁盘使用率，可选。
- 网络累计收发字节，可选。

字段单位固定，采集失败为 `null`，不能阻塞心跳。server 保存低频快照，前端绘制；client 只显示当前数字。

## 9. Metadata

节点和服务都支持受限 key-value metadata：

- key：小写 ASCII、长度受限。
- value：长度受限，总 map 字节数和条目数受限。
- 只用于展示、筛选和运维标注。
- 不参与认证、路由或权限判断。
- 前端展示必须转义，拒绝 HTML/script 注入。
- 标准 hostname、OS、架构、版本使用固定字段，不重复塞入 metadata。

## 10. 测试策略

### 10.1 静态和基础检查

每次提交至少运行：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo deny check
cargo audit
```

如果项目尚未引入 `cargo-deny`/`cargo-audit`，先在 CI 中安装并固定版本。

### 10.2 Unit tests

覆盖：

- ID 和 metadata 校验。
- token 生成、hash、验证、撤销和轮换。
- postcard frame 编解码、半包/粘包和边界长度。
- port lease 分配、释放、TTL 和冲突。
- 服务名/域名合法性。
- heartbeat RTT 和状态迁移。
- 负载快照单位、null 字段和版本兼容。
- redb repository CRUD 和事务回滚。
- retry/backoff 和错误分类。

### 10.3 Integration tests

使用临时端口、临时 redb 文件和本地证书 fixture：

- server/client QUIC 建连、认证、注册。
- server/client TCP/TLS/Yamux 建连、认证、注册。
- 同一服务多个并发公网连接。
- 双向大 payload、背压和半关闭。
- Hyper ingress 的流式上传/下载、chunked body、空 body、HTTP/2 并发 stream 和 WebSocket upgrade。
- Hyper 原生控制 API 的路由、Bearer 认证、JSON 错误 envelope、body/header 限制、超时、取消和 graceful shutdown。
- hop-by-hop header 清理、Host/X-Forwarded-* 策略和超大 headers 拒绝。
- client 重连后服务恢复。已加入 `tests/e2e/reconnect-service.sh`：server 重启后 client 不重启，节点必须重新变为 Online，持久化 TCP lease 必须重新监听并完成 echo。
- server 重启后 token、服务和端口租约行为正确。
- token revoke 后连接被关闭且不能重连。
- 证书链/私钥加载、错误 PEM、私钥不匹配、过期证书提示和启动失败。

### 10.4 E2E 测试环境

本地 E2E 启动：


```text
relay server
  ├─ control API
  ├─ QUIC listener
  ├─ TCP/TLS fallback listener
  └─ HTTP/TCP ingress

client A -> local HTTP fixture / TCP echo fixture
client B -> local HTTP fixture / TCP echo fixture
visitor -> public endpoint
browser -> frontend
```

E2E 场景：

1. 初始化 server，生成 admin token 和 user token。
2. client 用 user token 注册 node。
3. client 发布 HTTP 服务和 TCP 服务。
4. visitor 访问 HTTP 域名并验证响应体、Host、连接关闭。
5. visitor 连接分配的 TCP 端口并验证双向 echo。
6. client 每分钟 heartbeat，网页显示 RTT 和当前负载。
7. 强制阻断 UDP，确认 client 自动切换 TCP/TLS/Yamux。
8. 强制 server/client 重启，确认重连和状态时间线。
9. 撤销 user token，确认服务下线和连接拒绝。
10. admin 前端轮换 token，旧 token 立即失败。
11. Playwright 登录并完成服务、节点、图表流程。

### 10.5 网络故障注入

至少模拟：

- UDP 全阻断。
- TCP 延迟、丢包和短暂断开。
- server 到 client 单向断流。
- client 本地目标拒绝连接。
- 公网 visitor 半连接和异常关闭。
- redb 磁盘只读/空间不足。
- heartbeat 延迟超过阈值。

## 11. musl 与发布验证

目标：

```text
x86_64-unknown-linux-musl
aarch64-unknown-linux-musl
```

检查：

- 两个 binary 使用 mimalloc 全局 allocator。
- `libongrok` 不设置全局 allocator。
- 不链接 OpenSSL/native-tls。
- 在干净 Alpine 容器中启动 server/client。
- QUIC、TCP/TLS fallback、Hyper ingress、Hyper API 和 redb 均可运行。
- `ldd`/容器检查确认不依赖 glibc。
- 交叉编译产物可执行、配置目录和证书目录正确。

## 12. CI/CD

CI 分层：

1. format、clippy、unit test。
2. Linux native integration test。
3. musl build/test。
4. E2E relay matrix：QUIC、TCP fallback、重连、token revoke。
5. frontend lint/test/Playwright。
6. artifact checksum、SBOM 和 release bundle。

release bundle 至少包含：

- server binary
- client binary
- 默认配置示例
- systemd service 示例
- macOS launchd 示例
- Windows service 文档
- 升级/回滚说明
- token 备份说明和外部证书路径配置说明

## 13. Definition of Done

一个阶段只有同时满足以下条件才算完成：

- 功能代码合并到正确 crate。
- API/wire model 有文档和兼容性测试。
- 正常路径和主要失败路径都有测试。
- server/client 日志包含可定位的 node/service/connection id，但不包含 token。
- frontend 有加载、空数据、错误、断线和权限状态。
- E2E 覆盖 QUIC 与 TCP fallback。
- musl 目标构建通过。
- `design.md` 与 `techspec.md` 已同步实际行为。
