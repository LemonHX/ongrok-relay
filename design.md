# ongrok 设计文档

> 本文记录产品行为、用户体验和架构边界。具体实现库、协议栈和部署参数见 [techspec.md](./techspec.md)。

## 1. 产品定位

ongrok 是面向可信群组的自托管反向隧道平台。拥有公网 IP 和域名的机器运行 server，其他机器运行 client，通过主动出站连接把本地服务暴露到公网。

核心组成：无状态 Web 前端、控制 API、ongrok server、ongrok client、服务目录与状态监控。前端本身无状态；用户、token、节点、服务、端口租约和状态事件由 API 与持久化存储管理。

持有有效 `user token` 的成员属于同一个受信任群组，可以查看全部服务目录并访问群组内公开服务。

## 2. Token 与信任模型

只保留两类长期 token：

### Admin token

管理整个部署，包含全局配置、域名、端口范围、限制、安全策略、user token，以及全部节点、服务和状态。

### User token

用于 client 注册和连接 server、发布服务、更新/删除自己发起的服务、查看全部服务，以及获取服务发现信息。

没有独立用户注册身份；token 代表群组信任边界。两类 token 都必须可撤销和轮换，服务端只保存 token hash。

## 3. 节点与服务身份

client 不需要 `--name`，也不使用 MAC 地址作为身份。首次启动自动生成并持久化稳定的 `node_id` 和密钥对；系统 hostname 只作为可变展示信息。

服务发布时需要 `service_name`，用于目录和路由，例如 `ssh`、`web`、`database`。服务归属记录到 `node_id`，不要求知道用户真实身份。

## 4. 公网地址

HTTP/HTTPS 服务使用单层子域名，例如：

```text
https://mymachine.aaa.com
```

通配符 DNS 和证书覆盖 `*.aaa.com`；server 根据请求中的域名将流量转发到对应服务。避免使用 `22.ccc.aaa.com` 这种多级端口域名。

原始 TCP/SSH 服务使用公网端口租约，例如：

```text
gateway.aaa.com:22001 -> client 本地 127.0.0.1:22
gateway.aaa.com:22002 -> client 本地 127.0.0.1:5432
```

因此标准 SSH 的第一版连接形式是 `ssh user@gateway.aaa.com -p 22001`。

## 5. 端口租约

server 从可配置范围自动分配 TCP 公网端口，端口到 service/tunnel 的绑定必须原子化。client 断线时端口释放或短暂保留以支持重连；支持临时端口和固定端口。端口、tunnel、带宽和连接数均可受全局限制。容量不足时通过增加公网 IP 扩展。

HTTP/HTTPS 服务共享标准 Web 入口；原始 TCP 服务消耗端口租约。

## 6. 多 server 与服务发现

一个 ongrok client 未来可以同时连接多个 ongrok server。当前版本先聚焦单 server 连接；多 server 连接保留为后续扩展。每个 server 有自己的服务目录；client、前端和 API 通过服务目录查询服务。

服务发现只使用 `services list` 与服务详情 API，返回服务所属 server、协议、公网主机、端口和当前状态。第一版不提供本地 DNS、DoH、短域名或系统 DNS hook。

## 7. Multiplexing 行为

每个 client 与每个 server 维持一条或多条长连接；每个公网连接在长连接上创建独立逻辑 stream。控制、服务注册、服务列表、心跳和数据转发共享统一的上层 tunnel 协议。

QUIC 为首选传输；UDP 不可用时必须自动使用 TCP/TLS fallback。两种传输对上层服务模型透明。当前所有业务流量都经 server relay，不做 client-to-client 打洞。

## 8. 心跳、延迟与状态

client 与 server 使用协议内心跳，每分钟一次。心跳确认 client/tunnel 是否在线并测量控制连接 RTT，同时可携带当前机器的轻量负载快照。网站和 client 展示在线/离线、最近心跳、当前延迟、连接持续时间和状态变化时间线。

网页端可根据心跳历史绘制 CPU、内存、负载和网络流量等时间序列图表；client 只显示当前数值或简短摘要。暂不计算或展示 `99.82%` 这类 uptime 百分比，也暂不实现 P50/P95、多级公网探针和复杂 uptime 汇总。

服务和节点详情可展示公网出口 IP、连接源端口、hostname、操作系统、架构、client 版本、最近心跳、当前 RTT、当前负载和断开原因。NAT/VPN 场景下 server 看到的是公网出口地址。

节点和服务支持 metadata，用于环境、位置、用途、版本、项目等展示和筛选。metadata 不作为认证身份；client 上报的 metadata 默认视为不可信展示数据，并限制键名、值长度、总大小和更新频率。

## 9. 共同服务目录

前端、API、client 共用同一套 services 模型。`user token` 的 services list 返回整个可信群组的服务；admin token 可额外管理全部配置。

服务记录至少包含：`service_id`、`service_name`、`node_id`、协议、本地目标、公网地址、状态、最近心跳和 RTT。

## 10. 当前边界

- 不建立复杂用户注册/社交身份系统。
- 不使用 MAC 地址作为节点身份。
- 不要求 `--name` 作为 client 启动参数。
- 不把原始 SSH 强行伪装成共享 `:22` 按域名路由。
- 不提供本地 DNS、DoH、短域名或系统 DNS hook。
- 不把 UDP 可达性作为 client 正常工作的前提。
- 不在当前版本实现 P2P 打洞、TUN/VPN 或 client-to-client 直连。
- 不在当前版本实现多 server 同时连接。
