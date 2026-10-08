//! SwiftFetch v3 多模块并行启动架构
//!
//! 核心抽象：DownloadModule trait + EngineContext 全局状态容器
//! 所有模块通过 tokio::spawn + JoinSet 并行启动

use async_trait::async_trait;
use flume::{Sender, Receiver};
use parking_lot::{Mutex as PMutex, RwLock as PRwLock};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs::File;
use tokio::sync::{Notify, Semaphore, Mutex as TMutex};
use tokio::task::JoinSet;

use crate::speed_engine::{
    BaseChunk, HybridChunkManager, SmoothScheduler, SpeedSmoother, OscillationGuard,
    ProbeResult, SubChunk, DownloadConfig, ProgressInfo, DownloadResult,
    MIN_SUBCHUNK_SIZE, format_bytes, format_speed, format_progress_bar,
};

// ============================================================
// 全局常量
// ============================================================

pub const MIN_BASE_SIZE_FOR_BT_ALIGN: u64 = 1024 * 1024 * 1024;
pub const HYBRID_ALIGNED_BASE: u64 = 32 * 1024 * 1024;
pub const PREFETCH_WARM_BYTES: usize = 16 * 1024;
/// ★ 请求块大小 32KB → 64KB (BitComet 式优化 2026-09-10):
///   BitComet 使用更大的请求块来减少协议开销;
///   64KB 时头占比 0.025%, 高速 peer 下有效吞吐提升;
///   仍远低于协议 128KB 上限, 兼容所有客户端
pub const BT_REQUEST_BLOCK: u64 = 64 * 1024;
/// ★ 极限优化 (2026-09-12): peer limit 500 → 1000
///   BitComet bittorrent.max_connections_per_task=9999,
///   更多 peer = 更多聚合带宽, 即使单 peer 慢, 1000 个 30KB/s peer = 30MB/s
pub const DEFAULT_PEER_LIMIT: u32 = 1000;
/// ★ FIVEG peer limit 与 DEFAULT 对齐 (均为 1000)
pub const FIVEG_PEER_LIMIT: u32 = 1000;
/// ★ 极限优化: 全局连接 1000 → 2000
pub const DEFAULT_GLOBAL_MAX_CONNS: u32 = 2000;
/// ★ FIVEG 全局连接与 DEFAULT 对齐 (均为 2000)
pub const FIVEG_GLOBAL_MAX_CONNS: u32 = 2000;
pub const FIVEG_HTTP_MAX_CONNS: u32 = 18;
pub const DEFAULT_BT_PORT_START: u16 = 6881;
pub const DEFAULT_BT_PORT_END: u16 = 6889;
pub const DEFAULT_RATIO: f64 = 1.0;
pub const DEFAULT_SEED_MINUTES: u32 = 0;

/// ★ BT piece SHA-1 校验失败的最大重试次数.
///   超过后放弃该 piece (swarm 持续提供坏数据), 且禁止兜底强制完成,
///   下载以失败告终而不是把损坏数据当成功.
pub const MAX_PIECE_VERIFY_FAILS: u32 = 8;

// ============================================================
// BT 分片锁 (极限优化 2026-09-13)
// ============================================================
/// ★ bt_blocks_done / bt_blocks_inflight 分片数: 300+ peers 并发访问全局 HashSet
///   导致严重锁竞争, 分片为 64 个独立锁后竞争降低 64 倍
pub const BT_BLOCK_SHARDS: usize = 64;

/// ★ 计算 (piece_idx, block_idx) 属于哪个分片
#[inline]
pub fn bt_block_shard(piece_idx: u32, block_idx: u32) -> usize {
    // 混合哈希: piece_idx * 大素数 ^ block_idx, 取模分片数
    let h = piece_idx.wrapping_mul(2654435761) ^ block_idx.wrapping_mul(40503);
    (h % BT_BLOCK_SHARDS as u32) as usize
}

/// ★ 创建分片 HashSet (BT_BLOCK_SHARDS 个空 HashSet)
pub fn new_sharded_block_set() -> Vec<PMutex<std::collections::HashSet<(u32, u32)>>> {
    (0..BT_BLOCK_SHARDS)
        .map(|_| PMutex::new(std::collections::HashSet::new()))
        .collect()
}

// ============================================================
// BT 动态块大小选择
// ============================================================

/// ★ 动态分块: 根据 torrent 的 piece_size 选择请求块大小 (BT_REQUEST_BLOCK).
///   原理: 大块减少请求帧开销 (每块 17 字节头), 但过大的块会让慢 peer 排队超时.
///   选择策略 (BitComet 式优化 2026-09-12):
///   - piece >= 1MB  → 128KB (协议上限, 适合大 piece)
///   - piece >= 256KB → 64KB (平衡, 128KB 会导致慢 peer 30s 超时)
///   - piece < 256KB  → 32KB (小 piece 精细调度)
///   同时保证 block <= piece_size/4 (至少 4 块/piece)
pub fn choose_bt_request_block(piece_size: u64) -> u64 {
    // ★ 修复 (2026-09-29): BT 协议标准请求块为 16KB (2^14).
    //   主流客户端 (libtorrent/qBittorrent/Transmission) 的 max_request_size 默认 16KB,
    //   对 >16KB 的 Request 一律忽略 → 表现为"peer 已 unchoke、请求已发出、但 0 字节返回".
    //   旧逻辑按 piece_size 放大到 32KB/64KB, 导致几乎所有 peer 都不回数据 (实测 150+ 连接零吞吐).
    //   固定 16KB; 极小 piece 时按 piece_size/4 收缩, 保证每 piece 至少 4 块.
    let mut block: u64 = 16 * 1024;
    let max_block = piece_size / 4;
    if max_block > 0 && block > max_block {
        block = max_block;
    }
    block.clamp(4 * 1024, 16 * 1024)
}

// ============================================================
// 网络模式
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkMode {
    Auto,
    FiveG,
    Wired1G,
    Wired25G,
}

impl Default for NetworkMode {
    fn default() -> Self { NetworkMode::Auto }
}

// ============================================================
// 下载模式
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DownloadMode {
    SparseRareFirst,
    SequentialStream,
}

impl Default for DownloadMode {
    fn default() -> Self { DownloadMode::SparseRareFirst }
}

// ============================================================
// 协议模式
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProtocolMode {
    Hybrid,
    HttpOnly,
    BtOnly,
}

impl Default for ProtocolMode {
    fn default() -> Self { ProtocolMode::Hybrid }
}

// ============================================================
// 子分片源协议提示 (Http / Bitorrent / Any)
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceHint {
    Http,
    Bitorrent,
    Any,
}

impl Default for SourceHint {
    fn default() -> Self { SourceHint::Any }
}

// ============================================================
// 引擎事件 (flume 通道)
// ============================================================

#[derive(Debug, Clone)]
pub enum EngineEvent {
    Stop,
    FatalError(String),
    HttpBoost(i32),
    BtBoost(i32),
    BandwidthRatio { http: f64, bt: f64 },
    SysInfo(String),
    HotResource { protocol: ProtocolMode, weight: f64 },
    ColdResource { protocol: ProtocolMode, weight: f64 },
    NatOverload,
}

// ============================================================
// HTTP EMA / BT EMA 共享
// ============================================================

#[derive(Debug)]
pub struct BandwidthEMA {
    pub http_ema: AtomicU64,
    pub bt_ema: AtomicU64,
    last_http_bytes: AtomicU64,
    last_bt_bytes: AtomicU64,
    last_tick: PRwLock<Instant>,
}

impl BandwidthEMA {
    pub fn new() -> Self {
        Self {
            http_ema: AtomicU64::new(0),
            bt_ema: AtomicU64::new(0),
            last_http_bytes: AtomicU64::new(0),
            last_bt_bytes: AtomicU64::new(0),
            last_tick: PRwLock::new(Instant::now()),
        }
    }

    pub fn tick(&self, http_bytes: u64, bt_bytes: u64, alpha: f64) -> (u64, u64) {
        let now = Instant::now();
        let mut lt = self.last_tick.write();
        let dt = now.duration_since(*lt).as_secs_f64().max(0.001);
        *lt = now;
        drop(lt);

        let last_http = self.last_http_bytes.swap(http_bytes, Ordering::Relaxed);
        let last_bt = self.last_bt_bytes.swap(bt_bytes, Ordering::Relaxed);
        let http_delta = http_bytes.saturating_sub(last_http);
        let bt_delta = bt_bytes.saturating_sub(last_bt);
        let http_inst = (http_delta as f64 / dt) as u64;
        let bt_inst = (bt_delta as f64 / dt) as u64;

        let prev_http = self.http_ema.load(Ordering::Relaxed) as f64;
        let prev_bt = self.bt_ema.load(Ordering::Relaxed) as f64;
        let new_http = (alpha * prev_http + (1.0 - alpha) * http_inst as f64) as u64;
        let new_bt = (alpha * prev_bt + (1.0 - alpha) * bt_inst as f64) as u64;
        self.http_ema.store(new_http, Ordering::Relaxed);
        self.bt_ema.store(new_bt, Ordering::Relaxed);
        (new_http, new_bt)
    }

    pub fn global_ema(&self) -> u64 {
        self.http_ema.load(Ordering::Relaxed).saturating_add(
            self.bt_ema.load(Ordering::Relaxed)
        )
    }
}

// ============================================================
// Peer 评分
// ============================================================

#[derive(Debug, Clone)]
pub struct PeerScore {
    pub addr: String,
    pub ema_speed: f64,
    pub pieces_sent: u32,
    pub errors: u32,
    pub rtt_ms: u64,
    pub timeouts: u32,
    pub banned_until: Option<Instant>,
    /// ★ tit-for-tat 增强 (2026-09-10): 我们向此 peer 上传的字节数
    pub uploaded_to_peer: u64,
    /// ★ tit-for-tat 增强: 此 peer 向我们上传的字节数 (由 Piece 消息累计)
    pub downloaded_from_peer: u64,
    /// ★ tit-for-tat 增强: 上次服务此 peer Request 的时间 (用于限速不互惠的 peer)
    pub last_serve_time: Option<Instant>,
}

impl PeerScore {
    pub fn new(addr: String) -> Self {
        Self {
            addr,
            ema_speed: 0.0,
            pieces_sent: 0,
            errors: 0,
            rtt_ms: 0,
            timeouts: 0,
            banned_until: None,
            uploaded_to_peer: 0,
            downloaded_from_peer: 0,
            last_serve_time: None,
        }
    }

    pub fn is_banned(&self) -> bool {
        matches!(self.banned_until, Some(t) if t > Instant::now())
    }

    pub fn update_speed(&mut self, bytes: u64, dt_secs: f64) {
        if dt_secs <= 0.0 { return; }
        let inst = bytes as f64 / dt_secs;
        let alpha = 0.8;
        self.ema_speed = alpha * self.ema_speed + (1.0 - alpha) * inst;
    }

    /// ★ tit-for-tat: 判断是否应该优先服务此 peer 的 Request
    /// - peer 向我们上传过 (downloaded_from_peer > 0) → 优先服务 (互惠)
    /// - peer 从未向我们上传 → 限速服务 (每 500ms 最多 1 个 Request, 给乐观 unchoke 留探测窗口)
    ///   这释放了我们的上传带宽给互惠 peer, 提升 tit-for-tat 效率
    ///   ★ 2026-09-12: 间隔从 2s → 500ms: 2s 太严格导致几乎不上传 → peer 批量 choke → 速度归零
    pub fn should_serve_request(&self) -> bool {
        if self.downloaded_from_peer > 0 {
            return true; // 互惠 peer: 立即服务
        }
        // 非互惠 peer: 限速 (500ms 内只服务 1 次, 给乐观 unchoke 留探测窗口)
        match self.last_serve_time {
            Some(t) => t.elapsed() >= std::time::Duration::from_millis(500),
            None => true, // 首次请求: 服务一次 (乐观 unchoke 探测)
        }
    }
}

// ============================================================
// EngineContext - 全局状态容器
// ============================================================

pub struct EngineContext {
    pub config: DownloadConfig,
    pub protocol: ProtocolMode,
    pub network_mode: NetworkMode,
    pub download_mode: DownloadMode,
    pub probe: RwLockContainer<Option<ProbeResult>>,
    pub output_path: PathBuf,
    pub file_size: AtomicU64,
    pub base_chunk_size: AtomicU64,
    pub chunk_mgr: Arc<HybridChunkManager>,
    pub downloaded: Arc<AtomicU64>,
    pub http_downloaded: AtomicU64,
    pub bt_downloaded: AtomicU64,
    pub bt_total_received: AtomicU64, // ★ 调试: 所有 Piece 消息字节 (含重复块)
    pub bt_dup_blocks: AtomicU64,     // ★ 调试: 重复块计数
    pub file: Arc<TMutex<Option<File>>>,
    pub active_http_conns: AtomicU32,
    pub active_bt_conns: AtomicU32,
    pub http_conn_limit: AtomicU32,
    pub bt_peer_limit: AtomicU32,
    pub global_max_conns: AtomicU32,
    pub sem_http: Arc<Semaphore>,
    pub sem_bt: Arc<Semaphore>,
    pub bandwidth_ema: Arc<BandwidthEMA>,
    pub event_tx: Sender<EngineEvent>,
    pub event_rx: Receiver<EngineEvent>,
    pub stop_notify: Arc<Notify>,
    pub stop_event_tx: Sender<()>,
    pub stop_event_rx: Receiver<()>,
    pub scheduler: PMutex<SmoothScheduler>,
    pub speed_smoother: PMutex<SpeedSmoother>,
    /// ★ 极限优化 (2026-09-13): 原子化聚合速度, 避免 pick_next_piece 每次都锁 speed_smoother
    ///   300+ peers 每秒数千次 pick → 锁竞争严重; ema_speed 每 5s 才变, 用 AtomicU64 无锁读取
    pub bt_ema_speed: AtomicU64,
    pub oscillation_guard: PMutex<OscillationGuard>,
    pub base_chunk_done: PMutex<Vec<u32>>,
    pub bt_piece_map_completed: PMutex<Vec<u32>>,
    /// BT 块级进度: (piece_idx, block_idx) 已收到的块
    /// (修复: 旧逻辑只请求每 piece 首块且无完成标记 → 所有 session 重复拉同一块, 永远卡住)
    /// ★ 极限优化 (2026-09-13): 分片为 64 个独立 HashSet, 降低 300+ peers 的锁竞争
    pub bt_blocks_done: Vec<PMutex<std::collections::HashSet<(u32, u32)>>>,
    /// BT 块级在途: 已发 REQUEST 未收 PIECE 的块 (会话退出/超时后释放供其他 session 重试)
    pub bt_blocks_inflight: Vec<PMutex<std::collections::HashSet<(u32, u32)>>>,
    /// ★ 极限优化 (2026-09-13): piece 级已完成块计数器, 避免每次 Piece 消息都锁全局 done 集遍历检查
    ///   索引 = piece_idx, 值 = 该 piece 已完成块数; 达到该 piece 总块数即标记完成
    pub bt_piece_block_counts: PMutex<Vec<AtomicU32>>,
    pub bt_piece_size: AtomicU64,
    pub bt_total_pieces: AtomicU32,
    /// ★ 动态请求块大小 (BT_REQUEST_BLOCK 的运行时版本):
    ///   根据 torrent piece_size 和聚合速度自适应选择 [16KB, 128KB].
    ///   大 piece + 高速 peer → 大块 (减少请求帧开销); 小 piece/低速 → 小块 (精细调度).
    ///   整个下载期间固定 (避免 block_idx 计算不一致), 由 bt_module 启动时设置.
    pub bt_request_block: AtomicU64,
    pub peer_scores: PMutex<HashMap<String, PeerScore>>,
    pub bt_seeders: AtomicU32,
    pub bt_peers: AtomicU32,
    pub http_weight: std::sync::atomic::AtomicU64,
    pub bt_weight: std::sync::atomic::AtomicU64,
    pub http_ratio_target: std::sync::atomic::AtomicU64,
    pub bt_ratio_target: std::sync::atomic::AtomicU64,
    pub last_reset_count: AtomicU32,
    pub last_reset_window: PRwLock<VecDeque<(Instant, bool)>>,
    pub conn_delay_ms: AtomicU64,
    pub completed_time_series: PMutex<Vec<(u32, Instant)>>,
    pub prefetch_warmed: PMutex<HashMap<u32, bytes::Bytes>>,
    pub slow_subchunks: PMutex<HashMap<u64, (Instant, u64, Option<tokio::task::JoinHandle<()>>)>>,
    pub mirrors: Vec<String>,
    pub peer_port: AtomicU32,
    pub ratio_target: std::sync::atomic::AtomicU64,
    pub seed_minutes: AtomicU32,
    pub task_id: String,
    pub start_instant: Instant,
    pub no_cross_protocol: bool,
    // ===== BT 引擎新字段 (3 个, 用于修复 byrut BT 连接不上问题) =====
    /// 自己的 DHT node id (20 字节), 用于 DHT bootstrap + announce_peer
    /// None 表示本任务不走 DHT (HTTP 下载或 BT 已有 tracker peers)
    pub bt_dht_node_id: PRwLock<Option<[u8; 20]>>,
    /// pick_bt_port 返回的真实可用监听端口 (取代 peer_port 哈希值, 用于 tracker announce)
    pub bt_listen_port: AtomicU32,
    /// 入站 peer 的 TcpListener, 由 pick_bt_port 返回, move 到 ctx 让 incoming_peer_acceptor 使用
    /// None 表示本任务不接收入站 peer (NAT 穿透模式下仍可主动连出)
    pub bt_incoming_listener: PMutex<Option<tokio::net::TcpListener>>,
    /// BT 文件句柄缓存: 路径 → 已打开的文件句柄 (避免每 16KB 块 open/seek/close)
    /// 文件已在模块启动时预分配大小, 这里只做定位写入
    /// ★ 极限优化 (2026-09-13): 改用 std::fs::File + seek_write 定位写入
    ///   旧版 Arc<TMutex<tokio::fs::File>> 用 tokio Mutex 串行化 seek+write,
    ///   300+ peers 写同一文件时全部排队 → 磁盘吞吐瓶颈
    ///   seek_write 不改变文件指针, 多线程可并发写不同偏移 → 消除锁竞争
    pub bt_file_handles: PMutex<HashMap<String, Arc<std::fs::File>>>,
    /// BT Have 广播: 每个 session 注册一个 receiver, piece 完成时广播给所有 peer
    /// (让 peer 知道我们有什么 → 会向我们发 Request → 我们回传数据维持 tit-for-tat 不被 choke)
    pub bt_have_txs: PMutex<Vec<flume::Sender<u32>>>,
    /// BT 上传字节计数 (tit-for-tat 上传服务)
    pub bt_uploaded: AtomicU64,
    /// ★ 诊断 (2026-10): 收到的 peer Request 帧总数 (无论是否服务成功).
    ///   用于确认 tit-for-tat 是否成立: 若长期为 0 → peer 从不向我们请求
    ///   (我们 bitfield 为空/未送达, 或 peer 不需要我们的数据).
    pub bt_upload_requests: AtomicU64,
    /// ★ 诊断 (2026-10): 我们发出的 Unchoke 帧总数 (证明"允许 peer 向我们请求"已送达).
    pub bt_unchoke_sent: AtomicU64,
    /// ★ 诊断 (2026-10, t74): 连接阶段统计 —— 用于定位 "conns 峰值 254 但累计仅 32 次握手成功"
    ///   的并发槽位浪费问题. 三者相加 = 总连接尝试次数; tcp_ok/utp_ok 为成功握手数.
    pub bt_conn_tcp_ok: AtomicU64,
    pub bt_conn_utp_ok: AtomicU64,
    pub bt_conn_fail: AtomicU64,
    /// BT 当前 unchoke 我们的 peer 数 (诊断: 速度衰减时判断是否被批量 choke)
    pub bt_unchoked_now: AtomicI32,
    /// BT PEX peer 发现通道: session 解析 ut_pex 消息得到的新 peers 发回主循环
    /// (start() 时填充, 主循环持有 receiver 消费后 spawn supervisor)
    pub bt_pex_tx: PMutex<Option<flume::Sender<Vec<std::net::SocketAddr>>>>,
    /// ★ qBittorrent/BitComet 式优化: end-game 模式标志 (AtomicBool)
    /// 下载完成度 ≥ 95% 时激活, 允许多个 session 同时请求相同块,
    /// 避免最后几个块只由一个慢 peer 持有导致下载卡尾
    pub bt_endgame_mode: std::sync::atomic::AtomicBool,
    /// ★ qBittorrent 式优化: piece 稀有度计数 (每个 piece 被多少 peer 持有)
    /// pick_next_piece 优先选稀有度最低的 piece (rarest-first 策略)
    /// ★ 极限优化: RwLock<Vec<AtomicU32>>, 读共享锁 + 元素原子更新, 仅 resize 需写锁
    pub bt_piece_availability: PMutex<Vec<u32>>,
    /// ★ 活跃 peer 池 (2026-10-02): 由 BT 主循环维护 (与本地 live_peers 共享同一个 Arc),
    ///   供每个会话在 PEX 推送时读取 —— 只收不发 PEX 会被 BEP-11 对端视为只取不予,
    ///   容易被降权/choke, 这是对外网种子速度上不去的一个结构性原因。
    ///   初始为 None, 由 BtDownloaderModule::start 填入。
    pub bt_live_peers: PMutex<Option<Arc<PMutex<std::collections::HashSet<std::net::SocketAddr>>>>>,
    /// ★ SHA-1 校验失败计数 (piece_idx → 失败次数)
    ///   piece 收齐后必须与 meta.pieces[idx] 的 SHA-1 一致才标记完成;
    ///   不一致 → 撤销该 piece 的块完成标记并重新下载, 计数 +1.
    ///   超过 MAX_PIECE_VERIFY_FAILS 次后放弃该 piece (说明 swarm 持续提供坏数据).
    ///   非空时禁止走"兜底强制完成"路径, 避免把损坏数据当成功.
    pub bt_piece_verify_fails: PMutex<HashMap<u32, u32>>,
}

pub struct RwLockContainer<T> {
    inner: PRwLock<T>,
}

impl<T> RwLockContainer<T> {
    pub fn new(v: T) -> Self { Self { inner: PRwLock::new(v) } }
    pub fn read<R>(&self, f: impl FnOnce(&T) -> R) -> R { f(&self.inner.read()) }
    pub fn write<R>(&self, f: impl FnOnce(&mut T) -> R) -> R { f(&mut self.inner.write()) }
}

// ============================================================
// DownloadModule trait
// ============================================================

#[async_trait]
pub trait DownloadModule: Send + Sync {
    fn name(&self) -> &'static str;
    async fn start(self: Arc<Self>, ctx: Arc<EngineContext>) -> anyhow::Result<()>;
}

// ============================================================
// EngineBuilder
// ============================================================

pub struct EngineBuilder {
    modules: Vec<Arc<dyn DownloadModule>>,
}

impl EngineBuilder {
    pub fn new() -> Self {
        Self { modules: Vec::new() }
    }

    pub fn register<M: DownloadModule + 'static>(mut self, module: M) -> Self {
        self.modules.push(Arc::new(module));
        self
    }

    pub fn register_arc(mut self, module: Arc<dyn DownloadModule>) -> Self {
        self.modules.push(module);
        self
    }

    pub fn modules(&self) -> &[Arc<dyn DownloadModule>] {
        &self.modules
    }

    pub async fn run_all(self, ctx: Arc<EngineContext>) -> anyhow::Result<()> {
        let mut set: JoinSet<anyhow::Result<()>> = JoinSet::new();
        for module in self.modules {
            let ctx_c = ctx.clone();
            let name = module.name().to_string();
            set.spawn(async move {
                tracing::info!("模块启动: {}", name);
                let res = module.start(ctx_c).await;
                if let Err(ref e) = res {
                    tracing::error!("模块 {} 致命错误: {:#}", name, e);
                } else {
                    tracing::info!("模块完成: {}", name);
                }
                res
            });
        }

        let mut fatal: Option<String> = None;
        let mut all_done = false;
        while !all_done {
            tokio::select! {
                Some(res) = set.join_next() => {
                    match res {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            if fatal.is_none() {
                                fatal = Some(e.to_string());
                                let _ = ctx.event_tx.send(EngineEvent::FatalError(e.to_string()));
                                ctx.stop_notify.notify_waiters();
                                let _ = ctx.stop_event_tx.send(());
                            }
                        }
                        Err(join_err) => {
                            if fatal.is_none() {
                                fatal = Some(format!("模块任务panic: {}", join_err));
                                let _ = ctx.event_tx.send(EngineEvent::FatalError(fatal.clone().unwrap()));
                                ctx.stop_notify.notify_waiters();
                                let _ = ctx.stop_event_tx.send(());
                            }
                        }
                    }
                }
                _ = ctx.stop_event_rx.recv_async() => {
                    all_done = true;
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                    // ★ Bug 修复: 原 else 分支是 busy-wait, 阻止其他任务运行
                    // 改为定时检查: 如果所有模块都完成了, 退出循环; 否则继续等待
                    if set.is_empty() {
                        all_done = true;
                    }
                }
            }
        }

        set.shutdown().await;
        if let Some(msg) = fatal {
            anyhow::bail!(msg);
        }
        Ok(())
    }
}

impl Default for EngineBuilder {
    fn default() -> Self { Self::new() }
}

// ============================================================
// SubChunk 扩展 (source_hint)
// ============================================================

#[derive(Debug, Clone)]
pub struct HybridSubChunk {
    pub inner: SubChunk,
    pub source_hint: SourceHint,
}

// ============================================================
// 辅助: 打包 f64 到 AtomicU64
// ============================================================

pub fn f64_to_atomic_store(atom: &std::sync::atomic::AtomicU64, v: f64) {
    atom.store(v.to_bits(), Ordering::Relaxed);
}

pub fn f64_from_atomic_load(atom: &std::sync::atomic::AtomicU64) -> f64 {
    f64::from_bits(atom.load(Ordering::Relaxed))
}
