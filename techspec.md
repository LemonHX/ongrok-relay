# ongrok 技术选型与协议规格

> 本文承载具体技术选型和实现约束；产品行为与架构目标见 [design.md](./design.md)。

## 1. 推荐技术栈

- 核心语言：Rust
- 异步运行时：Tokio
- 首选传输：QUIC，使用 Quinn
- TCP fallback：TLS over TCP，使用 Yamux 做 multiplexing
- TLS：rustls/tokio-rustls
- 控制面数据库：redb（单 server 部署）
- 序列化：postcard + serde（控制帧）；数据流直接传 `bytes`
- ID：uuid，启用 v7
- 内存分配器：mimalloc（musl 目标的两个 bin 使用全局 allocator）
- HTTP 数据面：hyper + hyper-util + http-body-util
- 控制 API：直接使用 hyper/hyper-util，自行实现版本化路由、认证和 JSON handler
- 前端：React + Vite + TypeScript + i18next；控制台的 layout/theme 参考同级 `matrix` 项目

## 1.1 Rust 依赖分层

### `libongrok`

- `tokio`：异步 I/O 基础（通过 workspace features 统一配置）
- `bytes`：高效的字节缓冲和数据流转发
- `serde`：控制面消息和配置结构的 derive
- `postcard`：紧凑、低开销的控制帧序列化/反序列化
- `uuid`：启用 `v7`（以及需要时的 `serde`）作为资源 ID
- `rand`：生成 admin/user token 与节点密钥材料
- `blake3`：保存和比较 token hash
- `thiserror`：库边界的可匹配错误类型
- `tracing`：库内部结构化日志/span；不在 library 中初始化 subscriber
- `quinn`：QUIC transport 与原生 multiplexed streams
- `yamux`：仅用于 TCP/TLS fallback 的 stream multiplexing
- `tokio-util`：必要时使用 `compat` 适配 Yamux 的 futures I/O trait
- `rustls`、`tokio-rustls`：TLS 类型和 Tokio 异步 TLS
- `webpki-roots`：client/server 验证公网 CA 时使用内置根证书，不读取操作系统根证书

### `ongrok-relay-server`

- `hyper`：公网 HTTP/HTTPS ingress 和流式反向代理数据面
- `hyper-util`：Tokio I/O、server/client connection utilities
- `http-body-util`：HTTP body 组合、限制和流式适配
- `hyper`、`hyper-util`：控制 API、认证接口、健康检查及连接生命周期
- `tower-http`：trace、request limit 等 HTTP middleware
- `redb`：单机控制面数据库
- `clap`：server CLI
- `figment` 或 `config`：配置加载（环境变量、文件、CLI 三者择一组合，避免重复引入）
- `tracing-subscriber`：日志输出、过滤和 JSON/compact formatter
- `mimalloc`：musl 发布目标的全局 allocator
- `anyhow`：bin 启动层错误聚合；业务错误仍由 `libongrok` 的 `thiserror` 提供

### `ongrok-relay-client`

- `clap`：client CLI
- `tracing-subscriber`：CLI 日志初始化
- `mimalloc`：musl 发布目标的全局 allocator
- `anyhow`：bin 启动层错误聚合
- `directories`：跨平台配置/状态目录（若不想增加依赖，可自行按平台实现）
- `ed25519-dalek`：首次运行生成 Ed25519 节点密钥对；私钥仅保存在 client state file，server 仅接收和持久化公钥

### 依赖边界

- `libongrok` 不依赖 `axum`、`hyper`、`redb`、`clap`、`tracing-subscriber` 或全局 allocator。
- server 不引入 Axum；控制 API 与公网 ingress 都直接挂在 Hyper connection service 上。
- 不引入 OpenSSL、native-tls、系统 TLS、`mplex` 或第二个 Yamux 实现。
- `quinn` 原生 QUIC streams 已经提供 multiplexing；Yamux 只存在于 TCP fallback。
- 序列化先统一使用 `postcard`；不在第一版同时维护 `bincode`/`rkyv` 两套 wire format。
- 所有新增依赖必须通过 `cargo add` 获取当前 crates.io 最新兼容版本，并随后锁定 `Cargo.lock`。

## 2. 控制面存储

当前选择 redb 作为单台 `ongrok-server` 的嵌入式控制面数据库。它不需要独立数据库进程，适合与 server binary 一起部署和备份。

redb 保存需要跨重启保留的控制面数据：

- admin/user token 的 hash、状态和创建/撤销时间
- 节点公钥、节点元数据和最后已知状态
- 节点和服务 metadata
- 服务定义与本地目标描述
- HTTP 域名绑定和 TCP 端口租约
- 配置、配额和低频连接/心跳状态事件

不把实时数据面状态作为 redb 的主读取路径：在线 QUIC/TCP 连接、stream 到 client 的路由、当前连接计数和短生命周期状态保留在内存；断线、注册、租约变化和每分钟心跳再异步/批量持久化。

redb 是嵌入式单机存储，不承担多台 server 的共享一致性或跨地域复制。未来需要 active-active、多 gateway 共享租约或高可用控制面时，再迁移到 PostgreSQL 等外部数据库；数据访问层必须通过内部 repository/store trait 隔离，避免业务逻辑直接依赖 redb API。

## 3. TLS 证书与 musl

- 全部 TLS 使用 rustls，不链接 OpenSSL，也不使用系统 TLS 实现。
- server 启动时由用户提供 PEM full chain 和对应 PEM 私钥路径。
- server 不负责 ACME、签发、续期、DNS challenge 或证书安装。
- QUIC listener、TCP/TLS/Yamux listener 和 HTTPS ingress 复用同一组证书材料，但分别构造合适的 rustls/Quinn 配置。
- 启动时读取并校验证书链、私钥格式和匹配关系；失败时立即退出，不以无 TLS 模式继续运行。
- 第一版不实现证书热加载。证书更新后由部署者重启 server。
- client 不负责公网证书申请，只验证 server 证书。
- Linux 发布目标使用 `*-unknown-linux-musl`，目标是不依赖 glibc。musl 本身就是 libc，因此“无 libc”并不成立；mimalloc 作为应用 allocator，减少对系统默认 allocator 的依赖，但仍通过 musl 完成系统调用。
- allocator 仅在两个 bin 中设置为 `#[global_allocator]`；`libongrok` 不自行设置全局 allocator。

## 4. 传输层双栈

QUIC 是默认首选，但不能假设 UDP 永远可用。公司内网、公共 Wi-Fi、NAT、防火墙以及部分国家/地区运营商可能限制或丢弃 UDP，因此 TCP fallback 是正式能力。

### QUIC

```text
Rust + Tokio + Quinn
```

一条 QUIC connection 包含：

```text
control stream：认证、服务注册、services list、heartbeat
data stream：每个公网 TCP 连接一个双向 stream
```

QUIC stream 独立传输，避免 TCP-over-TCP 的整体队头阻塞。控制流和数据流分离，并限制最大并发 stream、缓冲区和 idle timeout。

### TCP/TLS/Yamux fallback

```text
TCP :443
  -> TLS (rustls/tokio-rustls)
  -> `yamux` crate multiplexing
  -> ongrok control/data streams
```

client 启动时优先尝试 QUIC；失败或超时后切换 TCP/TLS。server 在同一域名下同时提供 UDP/QUIC 与 TCP/TLS 入口，优先使用 `443`。

fallback 与 QUIC 共享相同的认证、服务注册、端口转发、services list 和 heartbeat 上层协议。fallback 接受 TCP 丢包造成的整体队头阻塞；为降低影响，可以允许一个 client 建立多条 TCP fallback 连接，并限制连接、stream、写入缓冲和空闲超时。

HTTP/3 不作为 tunnel 协议；直接使用 QUIC 原生双向 stream。HTTP/2/WebSocket over TLS 暂不进入第一版 fallback，除非实测网络连 TCP/TLS 但阻断非 HTTP 流量。

QUIC 不再额外叠加 Yamux：Quinn/QUIC 原生提供 multiplexed bidirectional streams。Yamux 只用于 TCP/TLS fallback；不要同时引入多个 Yamux 实现或 `mplex`。

`yamux` 面向 futures I/O；若 Tokio 类型不直接满足 trait，通过 `tokio-util` 的 compat 适配层连接，不引入一个不明确维护状态的 `tokio-yamux` 替代品。

## 5. Ongrok tunnel 协议

- QUIC 使用 ALPN，例如 `ongrok/1`。TCP fallback 的 TLS ALPN 使用同一版本命名。
- `Auth` 完成 token 与 `node_id` 认证；随后 `RegisterNode` 上传 client 版本、节点元数据和 Ed25519 公钥。
- token 不放入 URL、DNS 或明文 Host。
- 每个入站公网连接创建一个逻辑双向 data stream。
- 控制 stream 独立于 data stream，负责注册、注销、心跳和错误通知。
- frame 应包含长度、类型、请求/stream id 和 payload，避免依赖一次 read 对应一次消息。
- 控制帧使用 `postcard` 编解码；消息结构通过 `serde` derive。postcard 适合紧凑 wire format；若基准测试显示控制面编解码成为瓶颈，再单独评估 `rkyv`，不在第一版同时维护两套协议编码。
- `Heartbeat` 消息可携带带版本的节点负载快照；未知字段可忽略。
- 当前 HTTP/HTTPS ingress 由 Hyper 终止后，在已建立的 data stream 上使用 HTTP/1 streaming 转发；不序列化 Hyper 内部类型。专用的 request head/body/end frame 仍是后续协议演进项。
- HTTP body 使用 Hyper streaming 与 tunnel backpressure，不在 server/client 整体聚合。
- HTTP/2 与 WebSocket/Upgrade 路径仍未进入当前实现。

建议消息类型：`Hello`、`Auth`、`RegisterService`、`UnregisterService`、`ServiceList`、`Heartbeat`、`OpenStream`、`StreamData`、`CloseStream`、`Error`。

## 6. 当前范围与后续实验

当前版本只实现 client 到 server 的 relay 路径。以下内容暂不进入实现：

- client-to-client P2P 打洞
- TUN/VPN/network mode
- 本地 DNS、DoH、系统 DNS hook
- 多 server 同时连接

这些功能未来可以作为独立模块评估，不应影响当前 tunnel 协议和服务目录模型。

## 7. 服务目录数据模型

建议最小字段：

```json
{
  "service_id": "svc_...",
  "service_name": "ssh",
  "node_id": "node_...",
  "protocol": "tcp",
  "local_address": "127.0.0.1:22",
  "public_host": "gateway.aaa.com",
  "public_port": 22001,
  "status": "online",
  "transport": "quic",
  "last_heartbeat_at": "...",
  "rtt_ms": 42
}
```

`transport` 取值至少包括 `quic` 与 `tcp_tls_yamux`，用于前端和 client 展示实际链路。

## 8. 心跳与状态采集

- 协议心跳周期：60 秒。
- 每次心跳带有序列号和发送时间；响应时间用于计算 RTT。
- 心跳可携带轻量快照：CPU 使用率、系统 load average（平台支持时）、内存使用率/已用量、磁盘使用率（可选）、网络收发累计字节（可选）。
- server 保存快照历史供前端绘图；client 只显示当前值和最近一次采样。
- 记录在线/离线、最近心跳、连接建立时间、断开原因、公网 IP 与连接源端口。
- 不做 uptime 百分比聚合，不做 P50/P95。
- 状态时间线按连接/心跳事件保存，前端直接可视化原始事件。

负载采集约束：采样在 client 本地完成；字段和单位固定；采集失败字段使用 `null` 且不能阻塞心跳；快照大小有限制。高频原始指标不通过每分钟心跳上传。

## 9. Metadata

节点和服务使用受限的 key-value metadata map。metadata 只用于展示、筛选和运维标注，不参与认证或路由决策。key 使用小写 ASCII 并限制长度，value、条目数和总字节数均设上限；前端必须转义展示内容。标准 hostname、OS、架构和 client 版本使用固定字段，不重复塞入 metadata。

## 10. 安全与资源限制

- token 高熵随机生成，数据库仅存 hash。
- admin/user 权限在 API 和 tunnel 控制面分别校验。
- server 对 token、node、service、端口、并发 stream、连接数、缓冲和带宽设置限制。
- 端口租约分配必须原子化；断线释放或按租约 TTL 回收。
- 不信任 client 自报的公网 IP；以 server socket 对端地址为准。
