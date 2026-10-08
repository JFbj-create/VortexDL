# SwiftFetch v3 — 高性能 CLI 下载内核

> Rust CLI 无 UI 下载引擎。HTTP 侧提供两套引擎：CLI 用「静态基底块 + 慢块动态拆分子分片」的混合分块，
> VortexDL 用「无静态预切分、按实测速度完全动态派生」的单层分块；配合 Tokio 多线程异步任务调度
> （默认 2 核 4 线程 → 64 条并发流）与滑动窗口 + EMA 速度平滑，平衡稳定性与带宽利用率。
> BT 侧为自研 wire 协议实现，piece 收齐后做 SHA-1 校验。

---

## 一、支持的下载协议

> 💡 所有协议均通过 `Cargo feature flag` 按需要编译，默认只开 `http` + `bittorrent`。**一键全开**：`cargo build --release --features all-protocols`。

| 序号 | 协议族 | Scheme 示例 URL | Feature | 断点续传 | 并发分段 | 内置校验 | 传输加密 | 底层 Rust Crate | 状态 |
|---|---|---|---|---|---|---|---|---|---|
| 1 | **HTTP/1.1** | `http://server/file.zip`<br>`https://server/file.zip` | `http` (默认) | ✅ Range | ✅ 多连接 | ✅ ETag / Content-MD5 | ⚠️ HTTP 明文<br>✅ HTTPS TLS | [reqwest](https://crates.io/crates/reqwest) 0.12 + hyper 1.x | ✅ 生产就绪 |
| 2 | **HTTP/2** | `https://http2.akamai.com/` | `http2` (默认) | ✅ Range | ⚠️ 下载路径不使用 | ✅ ETag | ✅ ALPN 协商 TLS | reqwest `http2` feature | ⚠️ 已编译，但**下载路径强制 HTTP/1.1**（见 §二.8） |
| 3 | **HTTP/3 (QUIC)** | `https://quic.tech:8443/` | `http3` (实验性) | ✅ Range | ⚠️ 下载路径不使用 | ✅ ETag | ✅ QUIC 内置 TLS 1.3 | reqwest `http3` → [quinn](https://crates.io/crates/quinn) + [h3-quinn](https://crates.io/crates/h3-quinn) | ⚠️ 实验性，且**下载路径不使用** |
| 4 | **FTP** | `ftp://user:pass@server/pub/file.iso` | `ftp` | ✅ REST 命令 | ✅ 多控制连接 | ➖ 无 | ❌ 明文 TCP | [suppaftp](https://crates.io/crates/suppaftp) 10 | ⚠️ **明文 FTP 未实现**，仅支持 `ftps://` |
| 5 | **FTPS (FTP over TLS)** | `ftps://user:pass@server/pub/file.iso` | `ftps` (=`ftp`) | ✅ REST | ✅ 多控制连接 | ➖ 无 | ✅ AUTH TLS (Explicit) / Implicit 990 | suppaftp `tokio-rustls-aws-lc-rs` | ✅ 可用 |
| 6 | **SFTP (SSH File Transfer)** | `sftp://user@server:2222/home/user/data.bin` | `sftp` | ✅ pread offset | ✅ 多 SSH Channel | ➖ 无 | ✅ SSH-2 加密通道 | 双后端：<br>• [openssh-sftp-client](https://crates.io/crates/openssh-sftp-client) 0.15 (纯Rust异步)<br>• [ssh2](https://crates.io/crates/ssh2) (libssh2 FFI, vendored-openssl) | 🏗️ 骨架已写 |
| 7 | **WebDAV / WebDAVS** | `dav://user:pass@nas/backup.tar.zst`<br>`davs://nextcloud.user/remote.php/dav/files/u/` | `webdav` | ✅ HTTP Range | ✅ 多连接 (复用HTTP) | ✅ ETag (服务器实现相关) | ❌ DAV 明文<br>✅ DAVS TLS | [reqwest_dav](https://crates.io/crates/reqwest_dav) PROPFIND + reqwest GET | ✅ 可用 |
| 8 | **rsync (增量同步)** | `rsync+ssh://user@server:/data/huge.tar.zst`<br>`rsync://mirror.centos.org/centos/` | `rsync` | ➖ 算法级 delta (不能任意 Range) | ➖ 不切分 (整文件对比) | ✅ xxHash3 强校验 | ✅ SSH 通道 (rsync+ssh://)<br>⚠️ rsync:// 明文 | [libsync3](https://github.com/Bechma/libsync3) 纯 Rust xxhash3 rsync 算法<br>+ SSH 管道执行远端 `rsync --sender` | 🏗️ 骨架已写；`rsync://` daemon 模式未实现 |
| 9 | **IPFS / IPNS** | `ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi`<br>`ipns://en.wikipedia-on-ipfs.org` | `ipfs` | ➖ CID 不可变 (支持全量+Gateway Range) | ✅ Bitswap 多 Peer | ✅ CID 内联 Multihash | ✅ Kubo RPC (localhost)<br>✅ HTTPS Gateway | 双通道：<br>• [Kubo](https://github.com/ipfs/kubo) HTTP RPC `http://127.0.0.1:5001/api/v0` (需本地运行 ipfs daemon)<br>• Gateway `https://ipfs.io/ipfs/<CID>` (复用 HTTP 内核) | 🏗️ 骨架已写 |
| 10 | **BitTorrent** | `.torrent` 文件路径 | `bittorrent` (默认) | ➖ Piece-level (resume 时 recheck) | ✅ 多 Peer 并发<br>⚠️ uTP 仅骨架未接线<br>✅ **WebSeed** (BEP-19) | ✅ **Each Piece SHA-1** (收齐后校验，失败撤销重下) | ➖ 明文 Peer Wire<br>✅ **BEP-33 Scrape** Swarm 统计 | 自研 `bt_engine.rs` Wire Protocol + DHT/PEX Peer 发现 + **BEP-19 WebSeed / BEP-33 HTTP Tracker Scrape** | ✅ 可用 |
| 11 | **Magnet 磁力链** | `magnet:?xt=urn:btih:...` | `bittorrent` | — | — | — | — | — | ❌ **不可用**：未实现 BEP-9 ut_metadata，缺少文件名/大小/piece 哈希，会明确报错 |

### 🔐 协议能力位标志 (Capability Bitflags)

每个协议实现声明一组能力位，调度器 `SmoothScheduler` 会依据这些位自动选择最优分片策略：

```
WHOLE            — 支持全量下载 (所有 provider 均具备)
RANGE            — 支持字节级 Range → 可静态+动态分片 (HTTP / FTP REST / SFTP pread)
PARA             — 支持并发多连接并行
RESUME           — 支持断点快照 (HTTP ETag / FTP SIZE+REST / SFTP mtime+size)
LS               — 支持目录列表 (FTP LIST / WebDAV PROPFIND / SFTP readdir / IPFS ls)
HASH             — 协议内置强校验和 (BT Piece SHA1 / IPFS CID Multihash / rsync xxhash3)
P2P              — 多源 P2P 网络 (BT DHT / IPFS Bitswap)
H2-MUX           — HTTP/2 单连接多路复用
H3-QUIC          — HTTP/3 over QUIC (0-RTT 握手 + 连接迁移)
TLS              — 传输层加密 (HTTPS / FTPS / SFTP / DAVS / rsync+ssh)
```

### ⚡ 快速：列出所有可用 Provider
```bash
swiftfetch --list-providers

# 输出示例 (开了 http, http2, bittorrent 默认 feature):
# NAME           CAPABILITY FLAGS                           SUPPORTED SCHEMES
# ------------------------------------------------------------------------------------------
# http1          WHOLE|RANGE|PARA|RESUME|LS                 http, https
# http2          WHOLE|RANGE|PARA|RESUME|H2-MUX|TLS         http, https
# bittorrent     WHOLE|PARA|HASH|P2P                       torrent, magnet
```

### 📦 编译对应协议
```bash
# 默认仅 HTTP1/2 + BT
cargo build --release

# 开 HTTP/3 (实验性 QUIC)
cargo build --release --features http3

# 开 FTP + FTPS
cargo build --release --features ftp

# 开 WebDAV (自动含 HTTP2)
cargo build --release --features webdav

# 开 SFTP (含双后端)
cargo build --release --features sftp

# 开 rsync 增量 (xxhash3)
cargo build --release --features rsync

# 开 IPFS (Kubo RPC + Gateway fallback)
cargo build --release --features ipfs

# 🔓 全协议一次全开 (推荐给重度用户)
cargo build --release --features all-protocols
```

---

## 二、核心运作模式 & 技术特性

> ⚠️ **本项目有两套 HTTP 引擎，务必区分**（早期文档只描述了其中一套，容易误读）：
>
> | 引擎 | 文件 | 分片方式 | 使用者 |
> |---|---|---|---|
> | **混合引擎** (旧) | `speed_engine.rs` | 静态 Base Chunk + 慢块动态 SubChunk | SwiftFetch CLI (`-u`) |
> | **完全动态引擎** (v2) | `dynamic_engine.rs` | **无静态预切分**：只投放少量种子块，其余全部按实测速度动态派生 | **VortexDL 的 HTTP 下载** |
>
> 下面 §二.1 描述的是**混合引擎**；完全动态引擎见 §二.1b。

### 🏗️ 1. 分层混合静态‑动态分片架构 (Hybrid Chunking) — 仅 CLI

| 层级 | 策略 | 说明 |
|---|---|---|
| **外层 · 静态基底块 (Base Chunk)** | **固定大小静态分块** (按文件大小自适应 4MB/8MB/16MB/32MB；≥1GB 强制 32MB 对齐) | 稳定的断点快照边界，减少 HTTP 请求数量，避免 Range 爆炸 |
| **内层 · 动态子分片 (Sub Chunk)** | **仅对速度滞后的慢基底块内部自适应动态拆分** | 快块保持静态不扰动，慢块按 `慢度指数` 动态切 N 个子分片派给空闲 Worker 抢跑 |
| **最小子分片阈值** | 硬性下限 256 KB (`MIN_SUBCHUNK_SIZE`) | 防止动态切分过度 → HTTP 请求数爆炸 |

```
文件 (10GB)
├─ Base Chunk 0  [0   .. 4MB]  → ✅ 正常速度，静态完成
├─ Base Chunk 1  [4MB .. 8MB]  → 🐢 慢块！动态拆分：
│   ├─ SubChunk 1a  [4.0M .. 5.0M]  Worker A 抢跑
│   ├─ SubChunk 1b  [5.0M .. 6.0M]  Worker B 抢跑
│   └─ SubChunk 1c  [6.0M .. 8.0M]  Worker C 抢跑
└─ Base Chunk 2  [8MB .. 12MB] → ✅ 正常
```

---

### 🏗️ 1b. 完全动态分块架构 (Fully Dynamic Chunking) — VortexDL HTTP 下载

`dynamic_engine.rs` 是单层扁平 `Chunk` 模型，**没有任何静态预切分**：

| 环节 | 做法 |
|---|---|
| **初始分块** | 只投放"种子块"：按 probe 实测速度档位取 4 / 8 / 16 / 32 个 (`initial_chunk_seed`)，且不超过并发流数与 `文件大小 / 1MB` |
| **块增长** | 完全由运行时驱动 —— `split_half` (IDM 式对半切) 与 `steal_from_slowest` (空闲 worker 从最慢块尾部接管) |
| **切分阈值** | `dynamic_max_chunk` 随 EMA 速度自适应 (高速 8MB / 中速 4MB / 低速 2MB / 极低速 1MB)，每 1 秒重估一次 |
| **块数上限** | `并发流数 × 4` 作为安全阀，防止请求风暴与调度开销失控 |
| **写盘** | Windows `FileExt::seek_write` 定位写，**不改变文件指针** → 多 worker 真正并发写不同偏移，无互斥锁 |
| **卡死自愈** | 进度看门狗 `reclaim_stuck`：chunk 超过 20s 无任何新字节 → 回收重排，回收超 10 次标记失败 |
| **限流退避** | 收到 HTTP 429 → 全局退避时间戳 (2s→30s 指数) + 并发许可减半 (`throttle_on_429`) |

> 分块布局与 probe 速度解耦，因此断点续传按**字节区间**而非 chunk id 保存（见 §二.7）。

---

### 🧵 2. Tokio 多线程异步任务调度

- **Tokio `runtime = multi_thread`**，`worker_threads` 默认 **4 (对应 2 核)**，
  可用 `SF_WORKER_THREADS`（CLI）或 `VORTEX_WORKER_THREADS`（VortexDL）覆盖，夹在 [2, 64]
- 并发下载流数由线程数推导：`streams_for_threads(threads) = clamp(threads × 16, 8, 64)`
  → 4 线程 = **64 条并发流**（网络 I/O 密集，每线程承载多条独立 TCP 连接）
- CLI 混合引擎：`max_conns` 另按网络模式 clamp (5G:18 / 2.5G:32 / 1G:16/24)
- **工作窃取 (Work Stealing)**：空闲 Worker 主动从慢基底块的子分片队列抢任务，避免静态分片长尾阻塞
- 子分片失败自动重试 = 3 次 (per base chunk)
- 完成判定安全锁：**所有 Worker 退出仍缺块**时才报"下载不完整"，防止提前误判

---

### ⚡ 3. 平滑网速 & 进度调度控制器

| 算法 | 参数 | 作用 |
|---|---|---|
| **滑动窗口速度** | 窗口 **5 秒** (`SPEED_WINDOW_MS`)，窗口内字节差 ÷ 时长 | 覆盖 bursty 下载 (2s 突发 + 3s 空闲) 的完整周期，避免只采到空闲期 |
| **速度 EMA** | `α = 0.30` (`SPEED_EMA_ALPHA`) | 在 5s 窗口速度上再做一次轻量平滑，抑制毛刺 |
| **进度采样** | 每 **100 ms** 一次 (`SPEED_SAMPLE_MS`) | 进度条流畅刷新，浮点精度，杜绝"一跳一跳" |
| **智能防震荡** | 10s 窗口**变异系数** `CV > 0.60` → 冻结调参 5 秒 (回落到 `CV < 0.35` 才解冻) | 避免调度器朝令夕改 |
| **进度单调递增保证** | 单调钳制 + 上限封顶 | downloaded 绝对不超过 total，不回退 |

> 注：`speed_engine.rs` 与 `dynamic_engine.rs` 各自实现了速度平滑，参数略有差异：
> 前者用 5s 窗口 + EMA(0.30) + 100ms 采样 + 变异系数防震荡；
> 后者用 100ms 增量 EMA(`SPEED_EMA_ALPHA = 0.20`) + 2s 冷却的许可数微调。
> 早期文档中"TEMA α=0.96 @200ms / 窗口 3.3s / 采样 250ms"与代码不符，已按实现更正。

**平滑调度控制器的三项决策输入：**
1. `预估最大可用带宽` (前置探测 + 运行时测速)
2. `EMA 平滑实时网速`
3. `空闲 Worker 数量`

→ **输出**：`初始并发数` / `分片粒度` / `任务并发上限` 三项自适应调整

---

### 🔌 4. 插件化模块化解耦架构

主下载核心 (`PluginHost`) 作为调度中枢，HTTP / BT / 带宽探测 / 镜像解析 拆为 **独立插件** 并行协同工作：

| 插件类型 | 说明 | 容错 |
|---|---|---|
| **`AsyncThread`** 线程级异步模块 | 同进程高性能，直接 Arc 共享状态 | panic 可能影响主进程 |
| **`IsolatedProcess`** 进程级隔离模块 | 子进程 + IPC 协议通信，故障完全隔离 | 模块崩溃不影响下载核心 |

Plugin trait 定义在 [plugin.rs](file:///D:/tework/vdgame/SwiftFetch/src/plugin.rs)：`id/name/kind/version/start/stop/health_check/send_message`。

---

### 📡 5. IPC 异步消息协议

**协议格式**：JSON Lines (NDJSON)，每个消息一帧

| 帧类型 | 字段 |
|---|---|
| `Request` | `req_id` (唯一追踪) / `method` / `payload` / `deadline_ms` |
| `Reply` | `req_id` / `status` (Ok/Err/Timeout) / `payload` |
| `Event` | `topic` / `payload` (广播事件：进度/速度/状态) |
| `Handshake / Ping / Pong / ShutdownV1` | 控制帧 |

**附加策略：**
- `RequestId` 全程追踪，超时自动 Err
- **消息节流合并**：100ms 窗口内同 Topic 事件自动合并 delta，避免高频通信压垮调度器
- 主调度器**唯一**负责：全局连接池发放 + 断点快照写入 (杜绝多模块读写冲突)

---

### 🧠 6. 动态网络模式自适应

通过 `--5g / --wired-2g5 / --wired-1g / --auto` 切换，不同模式自动 clamp 连接上限：

| 模式 | HTTP 默认并发 | HTTP 并发上限 | BT Peer 上限 | 适用场景 |
|---|---|---|---|---|
| **5G 移动 (`--5g`)** | 18 | 18 | 1000 | 高延迟、高抖动、带宽波动大的 5G/Wi-Fi 6 移动网络 |
| **2.5G 有线 (`--wired-2g5`)** | 32 | 32 | 1000 | 2.5Gbps / 5Gbps 有线局域网 |
| **1G 有线 (`--wired-1g`)** | 16 | 24 | 1000 | 千兆有线 / 普通家庭宽带 |
| **Auto (`--auto`)** | 32 | 256 | 1000 | 前置探测 RTT / Loss 推断后动态选路 |

> 上表为 **CLI 混合引擎** 的取值 (`DownloadConfig::calc_http_connections`)。
> VortexDL 走的 `dynamic_engine` 不使用网络模式，其并发流数由运行时线程数推导（见 §二.2）。
> BT peer 上限 `DEFAULT_PEER_LIMIT` / `FIVEG_PEER_LIMIT` 均为 1000。
> `-c <N>` 用户指定后会被对应模式 **clamp** (例如 5G 传 `-c 24` → 自动压到上限 18)，防止配置过大导致 CDN 断连。

---

### 🛟 7. 断点续传 & 幂等保证

**CLI 混合引擎 (`speed_engine.rs`)** — 文件 `*.swiftfetch-resume`
- 记录 `completed_base_chunk_ids` + `completed_bytes_per_base_chunk`
- **SubChunk CAS 幂等完成锁**：镜像双通道 / 重试 / 动态子分片重叠场景下，同一段字节只被计数一次
- `downloaded_bytes` 强制封顶 (`.min(file_size)`)，杜绝重复计数

**VortexDL 完全动态引擎 (`dynamic_engine.rs`)** — 同一文件名，但**按字节区间保存**
- 保存 `completed_ranges: [[start,end]]` 与 `in_progress: [[start,end,downloaded]]`
- 恢复时按**覆盖关系**映射到新布局：只有被已完成区间**完全覆盖**的新 chunk 才标记完成；
  部分重叠的字节一律重新下载（保守策略，绝不让未校验字节被当作已完成）
- 之所以不用 chunk id：完全动态分块后 id→区间 的映射依赖运行时速度，跨进程不保证一致

**BT 引擎 (`bt_engine.rs`)** — 目录下 `.swiftfetch_bt_resume.json`
- 保存 `info_hash` + `piece_size` + 已完成 piece 位图；加载时校验 info_hash 防串种
- **恢复前对每个 piece 重新做 SHA-1 校验**（等价 qBittorrent 的 recheck），
  不通过则不恢复、交由 peer 重下
- 写盘采用 tmp + 原子 rename，避免写一半崩溃损坏状态文件

> 早期文档宣称"断点续传恢复 < 200ms"，实际耗时取决于 resume 文件大小与
> 磁盘回读量（BT recheck 需读取已下载的 piece），并无 200ms 保证。

---

## 三、CLI 用法

```bash
# ========== HTTP 下载 ==========
# 5G 模式下载 ChromeSetup，18 并发，JSON 进度输出
SwiftFetch.exe --5g \
  -u "https://dl.google.com/.../ChromeSetup.exe" \
  -o ChromeSetup.exe -c 24 --json

# 2.5G 模式 + 多镜像节点容错 (两个无效镜像不影响主节点下载)
SwiftFetch.exe --wired-2g5 -c 40 \
  -u https://example.com/largefile.zip \
  --mirror https://mirror-1.example.com/largefile.zip \
  --mirror https://mirror-2.example.com/largefile.zip

# 禁用断点续传 (全新下载)
SwiftFetch.exe -u https://example.com/a.zip --no-resume

# ========== BT 下载 ==========
# 通过 .torrent 文件下载，顺序播放模式，分享率 0 下完就停
SwiftFetch.exe --bt-only \
  --torrent "./game.torrent" \
  -o ./downloads/game/ \
  --sequential --ratio 0 --seed-minutes 0

# 磁力链接 (❌ 当前不可用: 未实现 BEP-9 ut_metadata, 会明确报错退出)
# 请改用 .torrent 文件
SwiftFetch.exe --magnet "magnet:?xt=urn:btih:..." \
  --5g --peer-limit 24 -o ./downloads

# ========== HTTP + BT 混合 ==========
# 默认开启协同：同一个文件同时从 HTTP 节点 + BT Swarm 获取，自动选最快源
SwiftFetch.exe \
  -u "https://cdn.example.com/ubuntu.iso" \
  --torrent "./ubuntu-24.04.torrent" \
  --5g

# ========== 插件相关 ==========
# 列出所有已注册插件
SwiftFetch.exe --list-plugins

# 启动时禁用某插件 + 传递自定义参数
SwiftFetch.exe -u <URL> \
  --disable-plugin probe_prefetch \
  --plugin-arg bt_engine.port=6882 \
  --plugin-arg http_plugin.timeout_ms=30000
```

### CLI 参数速查表

| 参数 | 说明 |
|---|---|
| `-u, --url <URL>` | HTTP(S) 链接 |
| `--torrent <PATH>` | `.torrent` 文件 (与 -u 互斥) |
| `--magnet <URI>` | magnet 磁力链接 |
| `-o, --output <PATH>` | 输出路径/目录 |
| `-c, --connections <N>` | HTTP 并发数 (被网络模式 clamp) |
| `--base-chunk <BYTES>` | 手动覆盖静态基底块大小 (如 4194304=4MB) |
| `--no-resume` | 禁用断点续传 |
| `--proxy <URL>` | 代理 (http/socks5) |
| `--json` | JSON Lines 格式输出进度 (脚本友好) |
| `-q, --quiet` | 静默模式 |
| `--mirror <URL>` | HTTP 镜像节点，可重复指定 |
| `--sequential` | BT 顺序播放模式 (在线视频/安装包) |
| `--peer-limit <N>` | BT 活跃 Peer 上限 |
| `--ratio <0~N>` | BT 分享率目标，达标自动停种 (默认 1.0) |
| `--seed-minutes <N>` | BT 最小做种分钟 (默认 0，下完即停) |
| `--5g / --wired-2g5 / --wired-1g / --auto` | 网络模式 |
| `--http-only / --bt-only` | 禁止另一协议 |
| `--no-cross-protocol` | 关闭 HTTP+BT 混合协同 |
| `--list-plugins / --disable-plugin <NAME> / --plugin-arg <NAME=VAL>` | 插件管理 |

---

## 四、编译说明

### 环境要求
- Rust 1.75+ (`rustup update`)
- Windows 目标：`x86_64-pc-windows-gnu` (已测试) 或 `x86_64-pc-windows-msvc`
- Linux / macOS：同样支持 (需对应 target)

### 命令

```bash
# Debug 构建 (~10s)
cargo build

# Release 构建 (LTO fat + codegen-units=1 + strip，最高性能二进制)
cargo build --release --target x86_64-pc-windows-gnu

# 产物位置
#   target/x86_64-pc-windows-gnu/release/swiftfetch.exe

# 运行示例 (HTTP)
cargo run --release -- -u "https://www.python.org/ftp/python/3.11.9/python-3.11.9-amd64.exe" -o py.exe --json

# 构建示例插件
cd plugins/hello_plugin && cargo build --release
```

---

## 五、项目目录结构

```
SwiftFetch/
├── Cargo.toml                      # Workspace (主项目 + hello_plugin)
├── Cargo.lock
├── build.rs                        # 构建脚本
├── examples/
│   └── basic_download.rs           # 库调用示例
├── plugins/
│   └── hello_plugin/               # 进程级隔离插件示例 (DLL/EXE)
│       ├── Cargo.toml
│       └── src/main.rs
├── release/SwiftFetch_CLI/         # 预编译发布产物
│   ├── SwiftFetch.exe              # Release 二进制 (5.6 MB)
│   ├── SwiftFetch_Debug.exe        # Debug 二进制 (含符号)
│   └── plugins/hello_plugin.exe
└── src/                            # 核心源码 (12 个模块, 约 13.8k 行)
    ├── main.rs                     # CLI 入口 + on_progress + 参数解析
    ├── lib.rs                      # 库导出
    ├── speed_engine.rs             # 混合分块引擎 (CLI 的 HTTP 下载): BaseChunk+SubChunk/调度/断点
    ├── dynamic_engine.rs           # 完全动态分块引擎 (VortexDL 的 HTTP 下载): 单层 Chunk/IDM 对半切/真暂停
    ├── bt_engine.rs                # BitTorrent 引擎 (Wire Protocol / Piece Picker / DHT / PEX / WebSeed)
    ├── dht.rs                      # BEP-5 Mainline Kademlia DHT
    ├── protocols.rs                # ProtocolProvider trait + capability bitflags
    ├── protocols_impls.rs          # HTTP族/FTP(S)/SFTP/WebDAV/rsync/IPFS/eD2k provider 实现
    ├── plugin.rs                   # Plugin trait + PluginRegistry (AsyncThread / IsolatedProcess)
    ├── host.rs                     # PluginHostRuntime + ResumeWriterActor + 内置插件薄包装
    ├── ipc.rs                      # IPC 协议帧：Request/Reply/Event + Handshake/Ping/Pong + 节流合并
    └── modules.rs                  # EngineContext / EngineBuilder / 常量 / 网络模式 clamp
```

| 模块 | 职责 | 代码量占比 |
|---|---|---|
| [bt_engine.rs](file:///D:/tework/vdgame/SwiftFetch/src/bt_engine.rs) | **最大**：BT Swarm / Peer Wire / Piece Picker / SHA-1 校验 / WebSeed | ≈ 28% |
| [speed_engine.rs](file:///D:/tework/vdgame/SwiftFetch/src/speed_engine.rs) | 混合分块 HTTP 下载、平滑调度、断点快照 | ≈ 16% |
| [protocols_impls.rs](file:///D:/tework/vdgame/SwiftFetch/src/protocols_impls.rs) | 多协议 provider 实现 | ≈ 14% |
| [dynamic_engine.rs](file:///D:/tework/vdgame/SwiftFetch/src/dynamic_engine.rs) | **VortexDL 用的**完全动态分块引擎、真暂停、429 退避 | ≈ 13% |
| [main.rs](file:///D:/tework/vdgame/SwiftFetch/src/main.rs) | clap CLI、on_progress 三路径、终态退出兜底 | ≈ 8% |
| [plugin.rs](file:///D:/tework/vdgame/SwiftFetch/src/plugin.rs) | SwiftPlugin trait / PluginId / PluginKind / PluginMsg | ≈ 5% |
| [dht.rs](file:///D:/tework/vdgame/SwiftFetch/src/dht.rs) | BEP-5 DHT (Kademlia) | ≈ 5% |
| [modules.rs](file:///D:/tework/vdgame/SwiftFetch/src/modules.rs) | EngineContext / EngineBuilder / 常量 / clamp | ≈ 4% |
| [host.rs](file:///D:/tework/vdgame/SwiftFetch/src/host.rs) | PluginHost 运行时、ResumeWriterActor | ≈ 4% |
| [protocols.rs](file:///D:/tework/vdgame/SwiftFetch/src/protocols.rs) | 协议抽象层 | ≈ 2% |
| [ipc.rs](file:///D:/tework/vdgame/SwiftFetch/src/ipc.rs) | IPC 帧编解码、RequestId 追踪、节流合并 | ≈ 2% |
| [lib.rs](file:///D:/tework/vdgame/SwiftFetch/src/lib.rs) | 库导出 | ≈ 1% |

---

## 六、性能特性一览

| 维度 | 说明 |
|---|---|
| **最大理论吞吐** | 单文件 2.5Gbps 有线环境 (active=32) → 250~300 MB/s 实测 |
| **CPU 占用** | 下载空闲 <1%，满载 <15% (4 核 8 线程) |
| **内存占用** | 10GB 文件 < 120MB (SubChunk 缓冲流式直写磁盘，不驻内存) |
| **断点续传恢复** | HTTP: 读 resume JSON 后按区间跳过（毫秒~秒级，取决于文件大小）；BT: 需对已下载 piece 做 SHA-1 recheck，耗时与已下载量成正比 |
| **进度条流畅度** | 100ms 采样 / 浮点精度 / 滑动窗口 + EMA 平滑，无肉眼跳变 |
| **计数器精度** | SubChunk CAS 幂等锁 + `.min(file_size)` 封顶，downloaded == total 精确到字节 |
| **自动退出** | completed/failed 终态后 1 秒内 `process::exit` 兜底，永无卡进程 |

---

## 七、已知限制

1. 插件热加载：目前需启动时通过 `--plugin-arg` 指定，运行时动态 Loader 在开发中
2. 跨协议 HTTP+BT 混合协同的 piece 对齐校验 (HTTP Byte Range ↔ BT Piece Boundary) 仅在 `file_size % piece_length == 0` 时最优
3. **磁力链接不可用**：未实现 BEP-9 ut_metadata 扩展，`magnet:` 只能拿到 info_hash，
   无法获知文件名/大小/piece 哈希。现在会**明确报错退出**（此前会静默产出一个 0 字节文件并"成功"）
4. **HTTP/2 与 HTTP/3 下载路径不使用**：两条 HTTP 引擎都硬编码 `.http1_only()`。
   这是刻意的性能选择 —— HTTP/2 会把所有请求多路复用到单条 TCP 连接，
   服务器对单连接限速时开再多 worker 也无法提速；改用 HTTP/1.1 + 多条独立连接才能突破单连接限速
5. **uTP 仅骨架**：`UtpSocket`（包头 / SYN-ACK 握手 / DATA 收发）已实现但**未接入 peer 会话**，
   BT 实际全部走 TCP
6. **明文 FTP / rsync daemon 模式未实现**：`ftp://` 直接报错要求改用 `ftps://`；
   `rsync://` 报错要求改用 `rsync+ssh://`；eD2k 仅支持自带 HTTP 源的链接
7. **SFTP / rsync / IPFS 为骨架实现**，未经完整验证
8. 运行期存在大量 `eprintln!` 诊断输出（`[BT_DEBUG]` / `[BT_STATS]` / `[BT_SESS]` 等），
   在数百 peer 场景下会明显增加 I/O 开销
9. **swarm 持续提供坏数据时下载不会"完成"**：piece SHA-1 校验失败会撤销块标记并重试，
   同时禁止兜底强制完成。这是刻意取舍 —— 宁可卡住也不产出损坏文件。
   若某 piece 连续失败超过 `MAX_PIECE_VERIFY_FAILS`(8) 次会打出 error 级日志

---

## 八、文档修正记录 (2026-09-30)

本轮修正了 README 与代码长期不符之处，并补齐缺失能力：

| 项 | 原文档宣称 | 实际情况 | 处理 |
|---|---|---|---|
| BT piece 校验 | ✅ Each Piece SHA-1 | **下载路径从不校验**，坏数据静默写盘 | **补上校验**：收齐后比对 `meta.pieces[idx]`，失败撤销块标记重下；WebSeed 路径对内存数据校验；resume 恢复前 recheck |
| 磁力链 | 列为支持协议 | 无 BEP-9，只能产出 0 字节文件 | **改为明确报错**，并从支持列表移除 |
| 兜底完成 | 三重完成判定 | 存在坏 piece 时仍会"强制完成" | **加门禁**：有校验失败的 piece 时禁止走兜底完成路径 |
| HTTP/2、HTTP/3 | ✅ 生产就绪 / 🧪 实验性 | 下载路径硬编码 `.http1_only()` | 文档标注"已编译但下载路径不使用" |
| uTP | ✅ UDP 低延迟传输 | `UtpSocket` 从未被调用 | 文档标注"仅骨架未接线" |
| 速度算法 | TEMA α=0.96 @200ms / 窗口 3.3s / 采样 250ms | 5s 窗口 + EMA(0.30) + 100ms 采样 | 按实现更正 |
| 断点续传 | < 200ms 恢复 | 无此保证；BT 需 recheck | 按实现更正 |
| 完全动态分块 | 未提及 | VortexDL 实际使用 `dynamic_engine` | 新增 §二.1b 说明 |
| 过期单元测试 | — | `HIGH_STREAK`/`LOW_STREAK` 常量改了但测试没同步 | 按常量修正测试 |

---

## License

MIT OR Apache-2.0 at your option.
