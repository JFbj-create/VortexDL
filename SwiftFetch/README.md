# SwiftFetch

**纯 CLI、无 UI 的 Rust 下载内核。** 一个进程里同时提供 HTTP 多源聚合下载和自研 BitTorrent
实现，并把这套内核以「插件」的形式嵌进桌面应用（[VortexDL](https://github.com/JFbj-create/VortexDL)）。

> ⚠️ **这是重写后的 v3，和仓库里 8 月那版（v2）不是同一个东西。**
> 见下面「和旧版的区别」。旧版的核心是「静态基底块 + 内层动态拆分子分片」的
> HybridChunkManager + SmoothScheduler + OscillationGuard 三件套；
> **v3 把这三件套整个删了**，换成单层扁平分块的 `dynamic_engine.rs` +
> `smart_sched.rs`，并加了 DHT、UPnP/NAT-PMP、PEX 主动推送、插件化 Host/IPC 架构。

---

## 和旧版（v2）的区别

| | v2（仓库里的老版） | v3（当前） |
|---|---|---|
| 分块模型 | 两层：BaseChunk + SubChunk，慢块拆子分片 | **单层扁平 Chunk**，IDM 风格对半切分 |
| 调度器 | HybridChunkManager + SmoothScheduler + OscillationGuard | 三件套**删除**，换成 `dynamic_engine.rs` + `smart_sched.rs` |
| 并发控制 | 固定窗口 + 振荡抑制 | **Semaphore 许可（4–32）+ 30s 冷却 + ±1 防抖**，加 `AIMD`（加速 +2 / 撞 429 减半） |
| 带宽探测 | 单次 8KB 采样 | **分级探测（并发 4 → 8 → 16）**，避免被 CDN 限速误导 |
| 暂停 | 靠取消任务再重建 | **真暂停**：`watch::channel` 状态广播 + `Notify` 恢复信号 |
| 架构 | 单体：模块直接互相调用 | **插件化**：`PluginHost` + `Plugin` trait + JSON Lines IPC 消息总线 |
| BT 对等发现 | 只有 tracker | tracker + **BEP-05 DHT** + **BEP-11 PEX（主动推送）** |
| NAT 穿透 | 无 | **UPnP IGD + NAT-PMP** 自动端口映射 |
| 断点续传 | 有 | 有（`.swiftfetch-resume` 边车文件，逐块落盘） |

---

## 一、架构

```
                         ┌──────────────────────────────────┐
   CLI (main.rs) ───────► │           PluginHost             │
   lib API ─────────────► │  host.rs: 调度 + 消息总线 + 落盘   │
                         └───────────────┬──────────────────┘
                                         │ JSON Lines IPC (ipc.rs)
                 ┌───────────────────────┼───────────────────────┐
                 ▼                       ▼                       ▼
        ┌────────────────┐     ┌──────────────────┐    ┌──────────────────┐
        │  HTTP 插件      │     │   BT 插件         │    │  调度 / 探测插件   │
        │ dynamic_engine │     │  bt_engine.rs    │    │ smart_sched.rs   │
        │ speed_engine   │     │  dht.rs          │    │ modules.rs       │
        │ protocols_impls│     │  upnp.rs         │    │ speed_engine.rs  │
        └────────────────┘     └──────────────────┘    └──────────────────┘
              AsyncThreadPlugin（同进程 tokio 任务，高性能）
              IsolatedProcessPlugin（独立子进程，故障隔离）
```

- **`plugin.rs`** —— 双模式插件：`AsyncThreadPlugin`（同进程 tokio 任务）与
  `IsolatedProcessPlugin`（独立子进程，崩了不影响主进程）。
- **`ipc.rs`** —— 消息协议用 **JSON Lines**（UTF-8 + `\n` 分隔），分 `REQ`（同步请求/响应，
  oneshot）与 `EVT`（广播事件，pub/sub）两类，带消息节流与命名管道传输。
- **`host.rs`** —— `PluginHost` 负责路由与生命周期；`ResumeWriterActor` 单独一个 actor
  负责把进度落盘（避免和下载线程抢锁）。
- **`protocols.rs` / `protocols_impls.rs`** —— 统一 `ProtocolProvider` trait +
  **能力位标志**（capability bitflags）：每个协议声明自己支持不支持断点、分片、并行、
  目录列表，调度器据此自动选分片策略，而不是硬编码 if-else。

## 二、HTTP 下载

- **多源镜像聚合**：同一个文件给多个镜像 URL，30ms 竞态预连接，谁快用谁；慢分片会被
  **提前重调度**到其它镜像（镜像分叉并发下载）。
- **动态分块**：不预切静态块。按实测速度决定块大小 —— 慢就切细（拿更多并发机会），
  快就合并成大块（减少请求开销）。
- **AIMD 并发**：加速时加性增（每次 +2），撞到 **429 立刻乘性减（减半）**并进入冷却。
  这是"激进上探但一被拒绝立刻退"的标准做法。
- **分级带宽探测**：开局用并发 4 → 8 → 16 实测可达带宽作为初始阈值。
  不用单次 8KB 采样 —— 实测那个值经常被 CDN 限速误导。
- **真暂停**：`watch::channel` 广播状态 + `Notify` 唤醒，暂停/恢复不需要销毁重建任务。
- **断点续传**：`.swiftfetch-resume` 边车文件逐块记录，关掉软件再打开能接着下。
- **分片预取**：16KB socket 预热，减少每个分片的首字节等待。

## 三、BitTorrent（自研实现，不用第三方 BT 库）

- **Bencode** 极简解析（dict / list / int / bytes）。
- **`.torrent` 文件 + magnet 链接**解析。
- **HTTP Tracker** announce + **BEP-33 Scrape**（拿 swarm 做种/下载统计）。
- **Peer 握手 + Wire Protocol**：Bitfield / Have / Unchoke / Interested / Request / Piece /
  Cancel；piece 内部按 16KB request 块拉取。
- **BEP-10 扩展协议** + **BEP-11 `ut_pex`**：不只解析对方发来的 PEX，**还会主动推送**自己
  知道的 peer（只收不发等于白声明扩展）。
- **BEP-19 WebSeed**：HTTP 种子源，冷门种子也能靠 HTTP 镜像提速。
- **BEP-05 DHT**（`dht.rs`）：Mainline Kademlia，实现 KRPC 的
  `ping` / `find_node` / `get_peers` / `announce_peer`，带公共 bootstrap 节点；
  迭代 `get_peers` 最多 6 轮、每轮并发 K=8，拿到 peer 就返回。tracker 挂了也能找人。
- **Piece SHA-1 校验**：收齐后校验，失败撤销重下（不做"下完才发现坏"这种事）。
- **UPnP IGD + NAT-PMP**（`upnp.rs`）：向路由器申请端口映射。
  加这个的原因很实在：实测 tracker 报 95–112 个 seeder、DHT 找到最多 130 个 peer，
  但 **1825 次连接只有 32 次握手成功（1.8%）** —— 大量做种者在 NAT 后面，
  我们连得出去、它们回不来，吞吐被压在 ~0.5–0.7 MB/s。
  UPnP（SSDP M-SEARCH → 设备描述 XML → SOAP `AddPortMapping`）和
  NAT-PMP（RFC 6886，向网关 5351 发 Map TCP）两条路都实现了，不引第三方依赖。

## 四、支持的协议

| 协议 | 说明 | 状态 |
|---|---|---|
| **HTTP/1.1** | 主力。多源聚合、动态分块、断点续传、429 退让 | ✅ 生产在用 |
| **HTTPS** | rustls TLS | ✅ 生产在用 |
| **BitTorrent** | `.torrent` / magnet，含 DHT / PEX / WebSeed | ✅ 生产在用 |
| **HTTP/2** | ALPN 协商；下载路径仍走 HTTP/1.1 分片 | ⚠️ 已编译 |
| **HTTP/3 (QUIC)** | quinn + h3-quinn，实验性 | ⚠️ 实验性 |
| **FTP / FTPS** | suppaftp；`ftps://` 走 AUTH TLS | ✅ 可用（明文 `ftp://` 未实现） |
| **SFTP** | openssh-sftp-client / ssh2 双后端 | 🏗️ 骨架 |
| **WebDAV / WebDAVS** | PROPFIND 元数据 + HTTP Range 复用 HTTP 内核 | ✅ 可用 |
| **rsync** | librsync 算法 + SSH 管道调远端 `rsync --sender` | 🏗️ 骨架（`rsync://` daemon 未实现） |
| **IPFS / IPNS** | Kubo 本地 RPC 或 HTTPS Gateway | 🏗️ 骨架 |
| **eD2k** | `ed2k://` URL 解析 + 分块 MD4 校验，从公开镜像走 HTTP | 🏗️ 骨架 |

协议全部通过 **Cargo feature flag** 按需编译，默认只开 `http` + `bittorrent`：

```bash
cargo build --release                          # 默认：http + bittorrent
cargo build --release --features all-protocols # 全开
```

## 五、实测数字

不是跑分，是这台机器上真下真文件测出来的：

| 场景 | 结果 |
|---|---|
| HTTP 大文件（CDN 直链） | **峰值 110 MB/s** |
| HTTP 限速 | 设 80% 时实测**精确 80%**，不超不欠 |
| BT 热门种子 | **14.8 MB/s**（客户端无瓶颈，瓶颈是种子冷） |
| BT 冷门种子 | 靠 DHT + PEX + WebSeed 才有人；NAT 穿透前握手成功率仅 1.8% |

## 六、构建与使用

```bash
git clone https://github.com/JFbj-create/SwiftFetch.git
cd SwiftFetch
cargo build --release
# 产物：target/release/swiftfetch(.exe)
```

```bash
# 普通 HTTP 下载（自动分块 + 断点续传）
swiftfetch -u https://example.com/big.iso -o big.iso

# 多源镜像聚合
swiftfetch -u https://mirror-a/x.iso --mirror https://mirror-b/x.iso --mirror https://mirror-c/x.iso

# BT
swiftfetch --torrent ./some.torrent -o ./out/
swiftfetch --magnet "magnet:?xt=urn:btih:..." -o ./out/
```

### 作为库使用

```toml
[dependencies]
swiftfetch = { git = "https://github.com/JFbj-create/SwiftFetch", features = ["http", "bittorrent"] }
```

## 七、目录

```
src/
  lib.rs               库入口（对外 API）
  main.rs              CLI
  host.rs              PluginHost + ResumeWriterActor（调度与生命周期）
  plugin.rs            插件 trait：同进程 / 隔离子进程
  ipc.rs               JSON Lines 消息协议（REQ / EVT）
  dynamic_engine.rs    单层扁平分块引擎（v3 新增，取代 v2 三件套）
  smart_sched.rs       智能调度：带宽探测 / AIMD / 动态块大小 / 429 冷却（v3 新增）
  speed_engine.rs      HTTP 下载内核：多源聚合 / 慢分片重调度 / 预取
  bt_engine.rs         自研 BT：wire protocol / tracker / PEX / WebSeed
  dht.rs               BEP-05 DHT（v3 新增）
  upnp.rs              UPnP IGD + NAT-PMP 端口映射（v3 新增）
  protocols.rs         统一协议 trait + 能力位
  protocols_impls.rs   HTTP / FTP(S) / SFTP / WebDAV / rsync / IPFS / eD2k
  modules.rs           模块并行启动（DownloadModule + EngineContext）
```

## 八、许可

见仓库根目录 `LICENSE`。第三方依赖各自遵循其原许可。

---

### 附：v2 的文档

上一版（8 月的 HybridChunkManager 那套）的完整协议矩阵与说明保留在
[`docs/README-v2.md`](docs/README-v2.md)，只作历史参考 —— 那份文档里的架构描述
已经不适用于当前代码。
