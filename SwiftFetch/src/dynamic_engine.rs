//! dynamic_engine.rs - 新下载引擎 v2
//!
//! 设计目标:
//! 1. 单一扁平 Chunk (废除 HybridChunkManager 的 BaseChunk + SubChunk 两层)
//! 2. 异步 worker (tokio::spawn) + IDM 风格对半切分
//! 3. 真暂停 (watch::channel 状态广播 + Notify 恢复信号)
//! 4. 动态连接数 (Semaphore 许可, 4-32 范围, 30s 冷却 + ±1 防抖)
//!
//! 取代旧 HybridChunkManager + SmoothScheduler + OscillationGuard 三件套.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use parking_lot::Mutex as PMutex;
use parking_lot::RwLock as PRwLock;
use tokio::sync::{watch, Notify, Semaphore};

#[cfg(windows)]
use std::os::windows::fs::FileExt;

use crate::speed_engine::{DownloadConfig, ProgressInfo, build_reqwest_client};

// ============================================================
// 文件日志 (替代 engine_log!, 因为 windows subsystem 程序的 stderr 在子线程中不可靠)
// ============================================================

static LOG_FILE: std::sync::OnceLock<std::sync::Mutex<std::fs::File>> = std::sync::OnceLock::new();

fn log_file() -> &'static std::sync::Mutex<std::fs::File> {
    LOG_FILE.get_or_init(|| {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let logs_dir = exe_dir.join("logs");
        let _ = std::fs::create_dir_all(&logs_dir);
        let log_path = logs_dir.join("download_engine.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .expect("无法创建日志文件");
        std::sync::Mutex::new(file)
    })
}

/// 写入日志到 logs/download_engine.log (线程安全)
pub fn log_engine(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = log_file().lock() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() % 100_000)
            .unwrap_or(0);
        let _ = writeln!(f, "[{:05}s] {}", now, msg);
    }
}

/// 兼容 engine_log! 的宏: 同时写入文件日志
macro_rules! engine_log {
    ($($arg:tt)*) => {
        let msg = format!($($arg)*);
        $crate::dynamic_engine::log_engine(&msg);
    };
}

pub(crate) use engine_log;

// ============================================================
// 常量
// ============================================================

/// IDM 风格对半切分的最小剩余阈值: 8MB (与 project_memory MAX_CHUNK_SIZE 一致)
pub const MAX_CHUNK_SIZE: u64 = 8 * 1024 * 1024;
/// 最小 chunk 大小: 切分后小于此值不再切
pub const MIN_CHUNK_SIZE: u64 = 1 * 1024 * 1024; // 1MB
/// ★ 尾部最小切分粒度 (2026-10-02): 收尾阶段把最后一段摊给多个 worker 用。
///   比 MIN_CHUNK_SIZE 小得多, 因为尾部切分的目的不是"减少请求开销"(
///   那时请求已很少), 而是**让多个慢连接并行分摊最后这几百 KB**。
pub const MIN_TAIL_CHUNK: u64 = 256 * 1024; // 256KB
/// ★ 尾部允许切分的最小剩余量 (2026-10-08)。
///
/// 尾部（剩余 ≤ 64MB）**不再一刀切禁止切分**，但也不能见块就切 ——
/// 把最后几百 KB 碎成十几个请求会白白撞 429。这条线以上的慢块才值得摊给多个 worker。
pub const TAIL_SPLIT_MIN: u64 = 4 * 1024 * 1024; // 4MB
/// 慢 chunk 判定: 速度低于 avg * 0.6 且剩余 > MAX_CHUNK_SIZE
/// ★ IDM 式优化 (2026-09-08): 0.5 → 0.6, 更积极检测慢源切分
pub const SLOW_RATIO: f64 = 0.6;
/// worker 默认数
/// ★ 修复 (2026-09-15): 256 → 64, 低配电脑友好, 避免 CDN 因连接过多拒绝新连接导致 chunk 卡死
///   (原 256 连接对 CDN 服务器过于激进, 部分服务器会直接拒绝新连接)
///   注: BitComet 式极限优化 256 已证明会触发卡死循环, 回退到 64
/// ★ 放宽 (2026-10-02): 与 MAX_CONNS 同步 —— worker 数决定"最多几条流同时在跑",
///   单连接限速场景下必须给足。原 64 会把 worker_count 卡在 64 条。
pub const DEFAULT_WORKER_COUNT: u32 = 256;
/// 连接数动态范围
/// ★ 修复 (2026-09-15): MAX_CONNS 256 → 64, 与 DEFAULT_WORKER_COUNT 对齐
pub const MIN_CONNS: u32 = 16;
/// ★ 上限放宽 (2026-10-02): 实测该 CDN 单连接仅 65~119 KB/s (curl 直测),
///   8 条连接合计才 ~420 KB/s —— 想跑满带宽只能靠**更多连接**。
///   原 64 会把档位上限 (96/192/256) 压回 64, 于是 64×163KB ≈ 10MB/s 封顶。
///   现在放到 256, 与 BandwidthTier::Ultra 对齐。
pub const MAX_CONNS: u32 = 256;
/// ★ 429 限流时的并发下限 (刻意低于 MIN_CONNS)
///
/// `MIN_CONNS` 只是**正常 AIMD 调度**时不允许再往下减的下限;
/// 它绝不能被当成 429 自适应上限的地板 —— 服务器已经在明确限流时,
/// 并发必须能压到更低, 否则 429 闸门形同虚设。
/// 之前 `tick()` / `progress_loop` 用 `.max(MIN_CONNS)` 兜底, 会把
/// `throttle_on_429` 降到的 8 又抬回 16, 使限流保护失效。
pub const MIN_THROTTLED_CONNS: u32 = 8;
/// 采纳服务器 `Retry-After` 的上限 (毫秒)。
///
/// 实测 cdn1.trashbytes.to (Cloudflare) 限流时返回 `Retry-After: 60`。
/// 这个头是**服务器明确要求的最短静默期**: 期间再发任何请求都会续上惩罚窗口,
/// 所以必须完整等待。旧实现完全忽略它、只用自家 16s 指数退避 —— 重试永远
/// 落在惩罚窗口内, 于是限流永不解除, 尾部速度永久掉到 B/s (实测)。
/// 上限 5 分钟用于兜住异常的超大头值, 避免一个坏响应把下载冻结几小时。
pub const MAX_RETRY_AFTER_MS: u64 = 300_000;
/// 单次下载允许切出的最大块数 (= 最大请求数)。
///
/// ★ 2026-10-02: 由实测的**请求数**限流反推。该 CDN 约 140 请求/60 秒,
///   而每个块至少要发 1 个请求 (切分/接管还会再发)。旧实现
///   `max_chunks = streams × 4` 会算出 768 块 —— 请求数远超预算,
///   必然触发 429 风暴。压到 128 后, 配合 `RequestBucket` 的速率控制,
///   整个下载的请求数始终落在服务器预算内。
pub const REQUEST_BUDGET: u32 = 128;
/// 调度器冷却 2s (was 5s)
/// ★ 极限优化 (2026-09-11): 5s → 2s, 更快响应速度变化, 及时加连接跑满带宽
pub const SCHEDULER_COOLDOWN_MS: u64 = 2_000;
/// EMA α: 0.20 (was 0.15), 更敏感
/// ★ 极限优化 (2026-09-11): 0.15 → 0.20, 更快响应速度变化
pub const SPEED_EMA_ALPHA: f64 = 0.20;
/// 高速判定: ema > baseline * 0.80
pub const HIGH_RATIO: f64 = 0.80;
/// 低速判定: ema < baseline * 0.50
pub const LOW_RATIO: f64 = 0.50;
/// 高速连续 2 次就加连接 (was 3 次)
/// ★ 极限优化 (2026-09-11): 3 → 2, 更快加连接
pub const HIGH_STREAK: u32 = 2;
/// 低速连续 4 次才减连接 (was 5 次)
/// ★ 极限优化 (2026-09-11): 5 → 4, 减连接更保守 (避免误减)
pub const LOW_STREAK: u32 = 4;
/// HTTP 读超时 (project_memory: 15s)
pub const CHUNK_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// 读超时重试次数 (project_memory: 3 次)
pub const READ_TIMEOUT_RETRIES: u32 = 3;
/// ★ 卡死 chunk 判定 (2026-09-28): ASSIGNED 且超过此毫秒数无任何新字节 → 回收重排
///   修复日志 "回收卡死 chunk 23 (ASSIGNED 188386ms)" 的问题:
///   原逻辑仅在 pending 为空时才检查, 256 个 chunk 时卡死 chunk 要等 188s 才被发现.
///   现改为每次 acquire 都做进度看门狗检查 (基于 last_progress_at).
pub const STUCK_PROGRESS_TIMEOUT_MS: u64 = 20_000;
/// 单 chunk 最大回收次数 (超过则标记失败, 避免死循环)
pub const MAX_RECYCLE_COUNT: u32 = 10;
/// 流式读取的缓冲区大小: 2MB (was 1MB)
/// ★ 极限优化 (2026-09-11): 1MB → 2MB, 减少系统调用次数, 提高吞吐
///   BitComet HttpDownloadConnectionCacheSize=2097152 (2MB), 对齐
pub const STREAM_BUF_SIZE: usize = 2 * 1024 * 1024;
/// 速度采样周期
pub const SPEED_SAMPLE_INTERVAL: Duration = Duration::from_millis(100);

// ============================================================
// EngineState - 8 canonical states (与前端契约一致)
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineState {
    Starting,
    Running,
    Paused,
    Completed,
    Extracting,
    Failed,
    Canceled,
}

impl EngineState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Extracting => "extracting",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }
}

impl Default for EngineState {
    fn default() -> Self { Self::Starting }
}

// ============================================================
// Chunk - 单层扁平分片 (取代 BaseChunk + SubChunk)
// ============================================================

/// chunk state 原子值
/// 0 = Pending (待分配)
/// 1 = Assigned (worker 持有)
/// 2 = Completed (已完成)
/// 3 = Failed
const CHUNK_PENDING: u8 = 0;
const CHUNK_ASSIGNED: u8 = 1;
const CHUNK_COMPLETED: u8 = 2;
const CHUNK_FAILED: u8 = 3;

pub struct Chunk {
    pub id: u64,
    pub start: u64,
    /// inclusive 结束偏移. ★ 2026-10-01 改为原子量: 慢块接管 (steal_from_slowest)
    /// 需要把原 chunk 的尾部截断给新 chunk, 若不截断则原 worker 会继续把整段下完,
    /// 造成"同一段字节被下载两遍"——既浪费带宽 (速度随时间衰减), 又让
    /// total_downloaded() 重复计数 (进度虚高, 出现"4G 下完 2.8G")。
    pub end: AtomicU64,
    pub downloaded: AtomicU64,
    pub state: AtomicU8,
    pub worker_id: AtomicU32,
    pub started_at: AtomicU64, // ms 时间戳, 0=未启动
    pub first_byte_at: AtomicU64, // 收到首字节时间
    /// 被回收的次数 (超过阈值时标记为 failed 而非重新入队, 避免死循环)
    /// ★ 修复 (2026-09-15): 增加此字段, 防止同一 chunk 被反复回收-分配-卡死
    pub recycle_count: AtomicU32,
    /// ★ 进度看门狗 (2026-09-28): 最后一次收到新字节的时间戳 (ms)
    ///   acquire 时重置为当前时间; download_chunk_range 每写入一批字节刷新;
    ///   超过 STUCK_PROGRESS_TIMEOUT_MS 无更新 → 判定卡死并回收, 无需等待 pending 为空
    pub last_progress_at: AtomicU64,
}

impl Chunk {
    /// 当前 (可能已被截断的) 结束偏移
    #[inline]
    pub fn end(&self) -> u64 {
        self.end.load(Ordering::Relaxed)
    }
    /// 截断结束偏移 (仅用于慢块接管, 只能往小改)
    #[inline]
    pub fn set_end(&self, v: u64) {
        self.end.store(v, Ordering::Relaxed);
    }
    pub fn size(&self) -> u64 {
        self.end() - self.start + 1
    }
    pub fn remaining(&self) -> u64 {
        self.size().saturating_sub(self.downloaded.load(Ordering::Relaxed))
    }
    pub fn is_completed(&self) -> bool {
        self.state.load(Ordering::Relaxed) == CHUNK_COMPLETED
    }
    pub fn speed_bps(&self) -> Option<u64> {
        let dl = self.downloaded.load(Ordering::Relaxed);
        if dl == 0 { return None; }
        let start_ms = self.first_byte_at.load(Ordering::Relaxed);
        if start_ms == 0 { return None; }
        let now = now_ms();
        let elapsed = now.saturating_sub(start_ms).max(1);
        Some((dl * 1000) / elapsed)
    }
}

fn now_ms() -> u64 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 解析 429 响应里的 `Retry-After` (RFC 9110: 秒数或 HTTP-date)。
/// 实测 Cloudflare 返回秒数形式; date 形式解析失败返回 None, 由调用方回退到指数退避。
fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    raw.trim().parse::<u64>().ok()
}

/// 从 `download_chunk_range` 附加的 429 错误串里取回 Retry-After 秒数。
/// 格式: `HTTP 429 Too Many Requests|retry_after=60`
fn parse_retry_after(err_msg: &str) -> Option<u64> {
    let rest = err_msg.split("retry_after=").nth(1)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u64>().ok()
}

// ============================================================
// ChunkPool - 取代 HybridChunkManager
// ============================================================

pub struct ChunkPool {
    chunks: PRwLock<Vec<Arc<Chunk>>>,
    pending: PMutex<VecDeque<u64>>, // 待 acquire 的 chunk id 队列 (按 size 降序排列)
    next_id: AtomicU64,
    pub file_size: u64,
    /// ★ 块数上限 (完全动态分块的安全阀): 动态切分/慢块接管不会让块数无限膨胀,
    ///   避免"请求风暴"与调度开销失控. 由 streams_for_threads() 推导.
    ///
    /// ★ 2026-10-02 改为原子量: 用户要求"检测到速度变慢就加大分块块数",
    ///   所以上限必须在运行时可变 —— 速度掉下来时放宽上限让块切得更细,
    ///   速度跑满时收紧上限减少调度开销。
    max_chunks: AtomicUsize,
    /// ★ 卡死回收阈值 (ms), 可由引擎在 429 限流期间调高, 避免把"被服务器限流而变慢"
    ///   的正常 chunk 误判为卡死并反复回收重连 (那是 429 死循环的放大器).
    stuck_timeout_ms: AtomicU64,
}

impl ChunkPool {
    pub fn new(file_size: u64, initial_chunks: u32, max_chunks: u32) -> Self {
        let count = initial_chunks.max(1) as u64;
        let base = (file_size / count + 4095) & !4095u64; // 4KB 对齐
        let mut chunks = Vec::with_capacity(initial_chunks as usize);
        let mut pending = VecDeque::with_capacity(initial_chunks as usize);
        let mut offset = 0u64;
        let mut id = 0u64;
        while offset < file_size {
            let end = std::cmp::min(offset + base - 1, file_size - 1);
            chunks.push(Arc::new(Chunk {
                id,
                start: offset,
                end: AtomicU64::new(end),
                downloaded: AtomicU64::new(0),
                state: AtomicU8::new(CHUNK_PENDING),
                worker_id: AtomicU32::new(0),
                started_at: AtomicU64::new(0),
                first_byte_at: AtomicU64::new(0),
                recycle_count: AtomicU32::new(0),
                last_progress_at: AtomicU64::new(0),
            }));
            pending.push_back(id);
            id += 1;
            offset += base;
        }
        Self {
            chunks: PRwLock::new(chunks),
            pending: PMutex::new(pending),
            next_id: AtomicU64::new(id),
            file_size,
            max_chunks: AtomicUsize::new((max_chunks as usize).max(initial_chunks as usize).max(1)),
            stuck_timeout_ms: AtomicU64::new(STUCK_PROGRESS_TIMEOUT_MS),
        }
    }

    /// ★ 设置卡死回收阈值 (429 限流期间调高, 抑制"回收→重连→429"死循环)
    pub fn set_stuck_timeout_ms(&self, ms: u64) {
        self.stuck_timeout_ms.store(ms.max(5_000), Ordering::Relaxed);
    }

    /// ★ 尾部的卡死回收阈值 (2026-10-02, 2026-10-03 改为按字节判定)。
    ///
    /// 原来用"剩余**块数** ≤ 4"判定尾部 —— 但实测尾部卡住时还剩 5~7 块、
    /// 约 46MB (日志 `chunks=126/131` 卡在 99.0%), 按块数算**不算尾部**,
    /// 于是沿用基准阈值; 而 429 限流期间基准还会被乘 3 到 60 秒。
    /// 结果: 卡死的块要等 42~188 秒才回收 (实测 `ASSIGNED 188384ms`),
    /// 这就是"99% 卡很久"的直接原因之一。
    ///
    /// 改为按**剩余字节**判定: 剩余不足 64MB 就是尾部, 阈值直接压到 5 秒。
    /// 尾部本来就没多少数据了, 早回收早让别的 worker 接手, 代价很小。
    pub fn effective_stuck_timeout_ms(&self) -> u64 {
        let base = self.stuck_timeout_ms.load(Ordering::Relaxed).max(5_000);
        if self.is_tail() || self.remaining_chunks() <= 4 {
            base.min(5_000)
        } else {
            base
        }
    }

    /// 还剩多少字节没下完 (按区间去重, 与 total_downloaded 口径一致)
    pub fn remaining_bytes(&self) -> u64 {
        self.file_size.saturating_sub(self.total_downloaded())
    }

    /// 是否处于尾部 (剩余数据很少)。
    ///
    /// 判定按**剩余字节**而不是块数 —— 实测尾部卡住时还剩 5~7 块、约 46MB
    /// (日志 `chunks=126/131` 卡在 99.0%), 按块数算根本不算尾部。
    pub fn is_tail(&self) -> bool {
        const TAIL_BYTES: u64 = 64 * 1024 * 1024;
        self.remaining_bytes() <= TAIL_BYTES
    }

    /// ★ 块数是否还可以继续增长 (供 split_half / steal_from_slowest 的安全阀)
    fn can_grow(&self) -> bool {
        self.chunks.read().len() < self.max_chunks.load(Ordering::Relaxed)
    }

    /// 当前块数上限 (诊断/日志用)
    pub fn max_chunks(&self) -> usize {
        self.max_chunks.load(Ordering::Relaxed)
    }

    /// ★ 运行时调整块数上限 (2026-10-02)。
    ///
    /// 用户要求"检测到速度变慢就加大分块块数": 块数上限越高, 允许切出的块越多,
    /// 每个 worker 越有机会抢到快的段。速度恢复后由调用方调回, 避免长期高开销。
    ///
    /// 下限保护: 不得小于当前已有块数 (否则已有块无法调度), 也不得小于 8。
    pub fn set_max_chunks(&self, target: usize) -> usize {
        let cur_len = self.chunks.read().len();
        let wanted = target.max(cur_len).max(8);
        let old = self.max_chunks.swap(wanted, Ordering::Relaxed);
        if old != wanted {
            engine_log!("[smart] 块数上限 {} -> {} (当前 {} 块)", old, wanted, cur_len);
        }
        wanted
    }

    /// worker 取剩余最大的 pending chunk (按 size 降序遍历 pending)
    pub fn acquire(&self, worker_id: u32) -> Option<Arc<Chunk>> {
        // ★ 进度看门狗 (2026-09-28): 每次 acquire 都先回收无进度的卡死 chunk,
        //   不再等到 pending 为空才检查 (旧逻辑导致 chunk 可卡死 188s).
        self.reclaim_stuck();

        let chunks_r = self.chunks.read();
        let mut pending = self.pending.lock();
        // 找剩余最大的
        let mut best_idx = None;
        let mut best_remaining = 0u64;
        for (idx, &cid) in pending.iter().enumerate() {
            if let Some(c) = chunks_r.get(cid as usize) {
                if c.state.load(Ordering::Relaxed) != CHUNK_PENDING { continue; }
                let r = c.remaining();
                if r > best_remaining {
                    best_remaining = r;
                    best_idx = Some(idx);
                }
            }
        }
        if let Some(idx) = best_idx {
            let cid = pending.remove(idx).unwrap();
            if let Some(c) = chunks_r.get(cid as usize) {
                let now = now_ms();
                c.state.store(CHUNK_ASSIGNED, Ordering::Relaxed);
                c.worker_id.store(worker_id, Ordering::Relaxed);
                // ★ 重置 started_at / last_progress_at: 回收后重新分配的 chunk 重新计时
                c.started_at.store(now, Ordering::Relaxed);
                c.first_byte_at.store(0, Ordering::Relaxed);
                c.last_progress_at.store(now, Ordering::Relaxed);
                return Some(c.clone());
            }
        }
        None
    }

    /// ★ 进度看门狗回收 (2026-09-28): 回收 ASSIGNED 但长时间无任何新字节的 chunk.
    ///   判定: now - max(last_progress_at, started_at) > STUCK_PROGRESS_TIMEOUT_MS.
    ///   修复: 旧逻辑仅在 pending 为空时检查, 且只以 started_at 计时 (卡死 188s 才回收).
    ///   回收后 push 回 pending 供其他 worker 接手 (重复写同偏移无害).
    fn reclaim_stuck(&self) {
        // 只管回收; 入队已在 reclaim_stuck_now 内部完成
        let _ = self.reclaim_stuck_now();
    }

    /// ★ 公开给 progress_loop 调用的卡死回收 (2026-10-03)。
    ///
    /// 为什么必须由 progress_loop 兜底: `reclaim_stuck` 原本只在 `acquire()` 里调用,
    /// 而尾部阶段 429 把并发压到 2~4 条 → **其余 60 个 worker 全阻塞在信号量上**,
    /// 根本走不到 acquire → 看门狗永远不执行 → 卡死的块要等 42~188 秒才被回收
    /// (实测日志 `回收卡死 chunk 61 (ASSIGNED 188384ms)`, 而阈值是 5~20 秒)。
    /// 这就是"99% 卡很久"的直接原因。
    ///
    /// progress_loop 每 100ms 必跑一次, 是唯一可靠的看门狗宿主。
    /// 返回本次回收的块数。
    pub fn reclaim_stuck_now(&self) -> usize {
        let now = now_ms();
        let timeout = self.effective_stuck_timeout_ms();
        let chunks_r = self.chunks.read();
        let mut stuck: Vec<Arc<Chunk>> = Vec::new();
        for c in chunks_r.iter() {
            if c.state.load(Ordering::Relaxed) != CHUNK_ASSIGNED { continue; }
            let last = c.last_progress_at.load(Ordering::Relaxed);
            let started = c.started_at.load(Ordering::Relaxed);
            let base = if last > started { last } else { started };
            if base == 0 { continue; }
            if now.saturating_sub(base) > timeout {
                stuck.push(c.clone());
            }
        }
        drop(chunks_r);
        let mut reclaimed = 0usize;
        for c in &stuck {
            let prev = c.state.compare_exchange(
                CHUNK_ASSIGNED, CHUNK_PENDING,
                Ordering::Relaxed, Ordering::Relaxed,
            );
            if prev.is_ok() {
                let count = c.recycle_count.fetch_add(1, Ordering::Relaxed) + 1;
                if count > MAX_RECYCLE_COUNT {
                    c.state.store(CHUNK_FAILED, Ordering::Relaxed);
                    engine_log!("[acquire] chunk {} 卡死回收超限 ({}次), 标记失败", c.id, count);
                } else {
                    let mut p = self.pending.lock();
                    p.push_back(c.id);
                    reclaimed += 1;
                    // 无进度时长按"最后一个进度时间"算 (与回收判定一致 —— 判定用的是
                    // last_progress_at 与 started_at 的较大者, 与 base 同口径)
                    let last = c.last_progress_at.load(Ordering::Relaxed);
                    let started = c.started_at.load(Ordering::Relaxed);
                    let base = if last > started { last } else { started };
                    engine_log!(
                        "[acquire] 回收卡死 chunk {} (无进度 {}ms, 回收第{}次), 重新入队",
                        c.id,
                        now.saturating_sub(base),
                        count
                    );
                }
            }
        }
        reclaimed
    }

    /// 完成一个 chunk
    pub fn release_complete(&self, chunk: &Arc<Chunk>) {
        chunk.state.store(CHUNK_COMPLETED, Ordering::Relaxed);
    }

    /// 释放回 pending (被 pause 中断, downloaded 保留)
    pub fn release_pending(&self, chunk: &Arc<Chunk>) {
        // ★ 修复 (2026-10-03): 不能"先 swap 再判断"。
        //   原写法 `let prev = state.swap(CHUNK_PENDING); if prev == COMPLETED { return; }`
        //   有个致命副作用: swap 已经**无条件**把状态写成了 PENDING, 早退只是"不入队"。
        //   于是当块已经是 COMPLETED/FAILED 时, 结果是"状态 PENDING + 不在队列里"的孤儿:
        //     · acquire 只在队列里找 → 永远拿不到它
        //     · all_completed() 永远 false → worker 空转, 下载既不完成也不失败
        //     · 收尾完整性校验会把已下完的块当成未完成 → 误报"完整性校验失败"
        //   触发路径真实存在: 同一 Arc<Chunk> 被两个 worker 同时持有时 (回收/自愈造成),
        //   A 已 release_complete, B 随后走 429 分支又调 release_pending。
        //
        //   现在改为 CAS: 只允许从 ASSIGNED/PENDING 转到 PENDING, 绝不覆盖
        //   COMPLETED/FAILED。这样既保留了"保证在队列里"的修复, 又不会破坏终态。
        let cur = chunk.state.load(Ordering::Relaxed);
        if cur == CHUNK_COMPLETED || cur == CHUNK_FAILED {
            return; // 终态不可逆 —— 连状态都不动
        }
        if chunk
            .state
            .compare_exchange(cur, CHUNK_PENDING, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            // 竞争中被别人改了状态 (可能刚被标记完成), 让那一方赢, 本次不动作
            return;
        }
        let mut pending = self.pending.lock();
        // 幂等: 已经在队列里就不重复推 (长队列时避免堆积)
        if !pending.iter().any(|&id| id == chunk.id) {
            pending.push_back(chunk.id);
        }
    }

    /// 标记失败
    pub fn mark_failed(&self, chunk: &Arc<Chunk>) {
        chunk.state.store(CHUNK_FAILED, Ordering::Relaxed);
    }

    /// ★ 自愈看门狗 (2026-10-02): 把"卡在 PENDING 但不在 pending 队列里"的块重新入队。
    ///
    /// 背景 (用户实测: "下载一直连接中"、"99% 不动"): 日志出现
    ///   tick=500..9500  downloaded 恒定 2.2%  speed=0  active=0  chunks=0/64
    /// 即 **9000 个 tick (15 分钟) 零进展** —— `active_conns=0` 且没有任何块完成,
    /// 说明所有块都停在 PENDING 状态, 却不在 pending 队列中 (acquire 永远取不到)。
    ///
    /// 这种"队列与状态不一致"可能来自: 切分/接管路径的竞态、worker 在闸门或
    /// acquire 中途返回时状态未回滚、以及 release_pending 只在 ASSIGNED 时才入队的
    /// 保守判断。无论根因是哪一个, 结果都是引擎**永久空转**。
    ///
    /// 这里做一个兜底: 只要"有未完成的块, 但 pending 队列为空、且没有任何活跃连接",
    /// 就把所有非完成块重新置为 PENDING 并重新入队。幂等、可重复调用。
    /// 返回重新入队的块数 (0 = 无需自愈)。
    pub fn heal_orphan_pending(&self, active_conns: u32) -> usize {
        // ★ 修正 (2026-10-02): 原来 "active_conns > 0 就不自愈" —— 但实测失败场景是
        //   active=1 (还有 1 个 worker 在跑或卡住), 其余块却成了孤儿, pending 为空。
        //   于是自愈从不触发, 下载在 99.9% 处直接判失败:
        //     [DL_FIN_FAIL] downloaded=533149604/533483588 (差 333,984 字节)
        //   现在放宽为: 只要"有未完成块 + pending 队列为空", 就允许自愈
        //   (幂等操作, 重复入队由 pending 去重与状态检查保护)。
        let _ = active_conns;
        let chunks = self.chunks.read();
        let has_unfinished = chunks.iter().any(|c| !c.is_completed());
        if !has_unfinished { return 0; }
        let mut pending = self.pending.lock();
        // pending 队列非空 → 有块可领, 不是孤儿状态
        if !pending.is_empty() { return 0; }
        // 收集所有未完成块的 id (跳过 FAILED, 那些是真正放弃的)
        let mut ids: Vec<u64> = Vec::new();
        for c in chunks.iter() {
            if c.is_completed() { continue; }
            if c.state.load(Ordering::Relaxed) == CHUNK_FAILED { continue; }
            c.state.store(CHUNK_PENDING, Ordering::Relaxed);
            ids.push(c.id);
        }
        let n = ids.len();
        for id in ids {
            pending.push_back(id);
        }
        n
    }

    /// IDM 对半切分: 把 chunk 的 [start+downloaded, end] 对半切
    /// 前半留在原 chunk (调整 end), 后半作为新 chunk 加入 pending
    /// ★ min_remaining: 动态阈值 (来自 engine.dynamic_max_chunk), 高速→大阈值少切, 低速→小阈值多切
    pub fn split_half(&self, chunk: &Arc<Chunk>, min_remaining: u64) -> Option<Arc<Chunk>> {
        // ★ 尾部切分策略 (2026-10-08 改回来)。
        //
        //   2026-10-03 曾把尾部切分整个禁掉，理由是"尾部新请求会触发 429，
        //   一次静默 60 秒比并行省下的几秒更亏"。但实测代价更糟：
        //   最后十几 MB 只剩**一两条连接**在拉，速度掉到 1.2MB/s，
        //   用户报"结尾速度骤降、下载不完"。
        //
        //   现在改成"有条件的切"：尾部仍然切，但只切**还够大的块**
        //   （剩余 ≥ TAIL_SPLIT_MIN = 4MB），避免把最后几百 KB 碎成一堆请求。
        //   429 由两道闸兜住：① RequestBucket 限请求速率（每个请求都过令牌桶）
        //   ② 这个剩余量下限。真正危险的"把 256KB 切成两个 128KB"不会再发生。
        if self.is_tail() && chunk.remaining() < TAIL_SPLIT_MIN {
            return None;
        }
        // ★ 完全动态分块安全阀: 达到块数上限后不再切分
        if !self.can_grow() { return None; }
        // ★ 关键: 只有 PENDING 状态的 chunk 才能切分
        //   防止竞态: chunk 已被 acquire (ASSIGNED) 但 dl 仍为 0 时切分导致重叠下载
        if chunk.state.load(Ordering::Relaxed) != CHUNK_PENDING {
            return None;
        }
        let dl = chunk.downloaded.load(Ordering::Relaxed);
        if dl > 0 {
            // 已开始下载的不切, 返回 None
            return None;
        }
        let remain_start = chunk.start + dl;
        let remain_end = chunk.end();
        if remain_start >= remain_end { return None; }
        let remain_len = remain_end - remain_start + 1;
        if remain_len < min_remaining { return None; }

        let half = remain_len / 2;
        if half < MIN_CHUNK_SIZE { return None; }

        let mid = remain_start + half - 1;
        let old_end = chunk.end();
        let new_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let new_chunk = Arc::new(Chunk {
            id: new_id,
            start: mid + 1,
            end: AtomicU64::new(old_end),
            downloaded: AtomicU64::new(0),
            state: AtomicU8::new(CHUNK_PENDING),
            worker_id: AtomicU32::new(0),
            started_at: AtomicU64::new(0),
            first_byte_at: AtomicU64::new(0),
            recycle_count: AtomicU32::new(0),
            last_progress_at: AtomicU64::new(0),
        });
        // 原 chunk: [start, mid], 新 chunk: [mid+1, end]
        // 通过替换 chunks 列表中的 Arc 实现原 chunk end 截断
        {
            let mut chunks = self.chunks.write();
            // 再次确认 state 仍是 PENDING (防止在获取 write lock 期间被 acquire)
            if chunk.state.load(Ordering::Relaxed) != CHUNK_PENDING {
                return None;
            }
            if let Some(slot) = chunks.iter_mut().find(|c| c.id == chunk.id) {
                let replaced = Arc::new(Chunk {
                    id: chunk.id,
                    start: chunk.start,
                    end: AtomicU64::new(mid),
                    downloaded: AtomicU64::new(0),
                    state: AtomicU8::new(CHUNK_PENDING),
                    worker_id: AtomicU32::new(0),
                    started_at: AtomicU64::new(0),
                    first_byte_at: AtomicU64::new(0),
                    recycle_count: AtomicU32::new(0),
                    last_progress_at: AtomicU64::new(0),
                });
                *slot = replaced;
            }
            chunks.push(new_chunk.clone());
            let mut pending = self.pending.lock();
            pending.push_back(chunk.id);
            pending.push_back(new_id);
        }
        Some(new_chunk)
    }

    /// 找最慢的 in-progress chunk (用于 worker 完成后触发 split)
    pub fn slowest_active(&self, min_remaining: u64) -> Option<Arc<Chunk>> {
        let chunks = self.chunks.read();
        let mut slowest: Option<(Arc<Chunk>, u64)> = None;
        for c in chunks.iter() {
            if c.state.load(Ordering::Relaxed) != CHUNK_ASSIGNED { continue; }
            let r = c.remaining();
            if r < min_remaining { continue; }
            if let Some(sp) = c.speed_bps() {
                match &slowest {
                    None => slowest = Some((c.clone(), sp)),
                    Some((_, cur)) if sp < *cur => slowest = Some((c.clone(), sp)),
                    _ => {}
                }
            }
        }
        slowest.map(|(c, _)| c)
    }

    /// ★ IDM 式慢 chunk 接管: 从最慢的 ASSIGNED chunk 的剩余部分切出后半段,
    ///   创建新 PENDING chunk 供空闲 worker 下载, 并把原 chunk 的尾部截断到 mid.
    ///   ★ 2026-10-01 修复: 原实现不截断原 chunk → 原 worker 会把 [mid+1, end]
    ///   也下载一遍, 造成"同一段字节被下载两遍". 实测日志: 5GB 文件下载计数器
    ///   高达 9.8GB (≈2x), 既浪费带宽 (有效速度腰斩且随时间衰减), 又让
    ///   total_downloaded() 重复计数 (进度虚高). 现改为截断: 原 worker 在
    ///   download_chunk_range 每轮重读 chunk.end() 后于 mid 处停止.
    /// ★ min_remaining: 动态阈值 (来自 engine.dynamic_max_chunk)
    pub fn steal_from_slowest(&self, min_remaining: u64) -> Option<Arc<Chunk>> {
        // ★ 完全动态分块安全阀: 达到块数上限后不再派生新块。
        //   ★ 例外 (2026-10-02): 尾部 (剩余未完成块很少) 允许突破上限 ——
        //   尾部切分是为了"把最后一个慢块摊给多个 worker", 产生的块数极少
        //   (最多再切几刀), 不会造成请求风暴, 却是尾部速度的关键。
        //   ★ 2026-10-08 改回来: 尾部**不再直接放弃偷块**。禁掉的代价是尾部只剩
        //     一两条连接、速度掉到 1.2MB/s（用户报"结尾骤降、下载不完"）。
        //     现在尾部照样偷，只是候选门槛提到 TAIL_SPLIT_MIN(4MB) —— 只摊分
        //     "还够大的慢块"，不把最后几百 KB 碎成一堆请求；请求速率由令牌桶兜。
        if self.is_tail() && self.remaining_bytes() < TAIL_SPLIT_MIN {
            return None;
        }
        let tail = self.remaining_chunks() <= 4;
        if !tail && !self.can_grow() { return None; }
        // ★ 尾部放宽必须**先于候选筛选**生效 (2026-10-02)。
        //
        //   原实现: 候选筛选 (下面的 `remain_len < min_remaining`) 用的是调用侧传来的
        //   dynamic_max_chunk (常为 2MB), 而"尾部放宽到 MIN_TAIL_CHUNK(256KB)"是在
        //   筛选**之后**才计算的 —— 于是"剩余不足 2MB 的最后一块"在筛选阶段就被剔除,
        //   slowest 恒为 None, 尾部切分永远不触发。
        //
        //   实测后果 (295MB 任务): 99.8% 时 active=1, 最后一个块用**一条连接**
        //   硬拉 37 秒 (speed 0), 其余 63 个 worker 无块可抢只能空转;
        //   直到 heal 自愈把它重新入队才收口。用户看到的就是"结尾掉到 B/s"。
        //
        //   现在: 尾部一律用 MIN_TAIL_CHUNK 作为候选门槛, 让最后这一小段能被
        //   切给多个 worker 并行分摊。
        let cand_min = if tail { MIN_TAIL_CHUNK } else { min_remaining };
        let chunks_r = self.chunks.read();
        let mut slowest: Option<(Arc<Chunk>, u64)> = None;
        for c in chunks_r.iter() {
            if c.state.load(Ordering::Relaxed) != CHUNK_ASSIGNED { continue; }
            let dl = c.downloaded.load(Ordering::Relaxed);
            let remain_start = c.start + dl;
            let remain_end = c.end();
            if remain_start >= remain_end { continue; }
            let remain_len = remain_end - remain_start + 1;
            if remain_len < cand_min { continue; }
            let sp = match c.speed_bps() {
                Some(s) => s,
                // ★ 尾部: 没有速度采样的块 (刚接手 / 已完全停滞) 恰恰最需要被分摊,
                //   不能因为"取不到速度"就跳过 —— 这正是尾部卡死时的形态。
                None => {
                    if tail {
                        0
                    } else {
                        continue;
                    }
                }
            };
            match &slowest {
                None => slowest = Some((c.clone(), sp)),
                Some((_, cur)) if sp < *cur => slowest = Some((c.clone(), sp)),
                _ => {}
            }
        }
        drop(chunks_r);
        let (chunk, _) = slowest?;
        let dl = chunk.downloaded.load(Ordering::Relaxed);
        let remain_start = chunk.start + dl;
        let remain_end = chunk.end();
        let remain_len = remain_end - remain_start + 1;
        // ★ 尾部判定必须**前置** (2026-10-02 修复)。
        //
        //   原来这里先做 "remain_len < min_remaining * 2 → 拒绝" —— 而 min_remaining
        //   是调用侧传来的 dynamic_max_chunk (常为 2MB), 于是剩余不足 4MB 就永远切不动,
        //   后面那段 tail_mode 放宽 (256KB)**根本走不到**。实测后果:
        //     tick=500  94.9% speed=5.38MB/s active=34  chunks=160/192
        //     [heal] 零进展重新入队 1 个孤儿块
        //     tick=1000 99.6% speed=161KB/s  active=2   chunks=191/192
        //   —— 最后 1 个块卡在慢连接上, 其余 worker 无块可抢只能空转, 速度掉到 KB 级。
        //
        //   现在: 先判断是否处于尾部 (剩余量不大), 尾部一律放宽到
        //   MIN_TAIL_CHUNK (256KB) 这一硬下限, 忽略调用侧的大阈值。
        let tail_mode = remain_len < MIN_CHUNK_SIZE * 4;
        let effective_min = if tail_mode {
            // 尾部: 只要够切一刀就切, 让多个 worker 分摊最后这段
            MIN_TAIL_CHUNK
        } else {
            min_remaining
        };
        if remain_len < effective_min * 2 { return None; }
        let half = remain_len / 2;
        // ★ 尾部加速修复 (2026-10-02): 原来硬性要求 half >= MIN_CHUNK_SIZE (1MB),
        //   于是最后那个块只要剩余不足 2MB 就**永远切不动** —— 尾部只能靠单个 worker
        //   硬啃慢连接, 用户看到的就是"99% 之后只有几十 KB 且不动"。
        //   日志实证: tick=500 时 downloaded=99.8%, active=1, chunks=63/64。
        //   现在改为: 正常阶段仍用 1MB 下限 (避免碎片化), 但**剩余量已经不大时**
        //   放宽到 256KB, 让多个 worker 能同时抢最后这一小段, 尾部立刻收口。
        // floor 与 tail_mode 已在上面算好 (effective_min)
        let floor = if tail_mode { MIN_TAIL_CHUNK } else { MIN_CHUNK_SIZE };
        if half < floor { return None; }
        let mid = remain_start + half - 1;
        let new_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let new_chunk = Arc::new(Chunk {
            id: new_id,
            start: mid + 1,
            end: AtomicU64::new(remain_end),
            downloaded: AtomicU64::new(0),
            state: AtomicU8::new(CHUNK_PENDING),
            worker_id: AtomicU32::new(0),
            started_at: AtomicU64::new(0),
            first_byte_at: AtomicU64::new(0),
            recycle_count: AtomicU32::new(0),
            last_progress_at: AtomicU64::new(0),
        });
        {
            let mut chunks = self.chunks.write();
            // 再次确认原 chunk 仍 ASSIGNED (防止竞态)
            if chunk.state.load(Ordering::Relaxed) != CHUNK_ASSIGNED {
                return None;
            }
            // ★ 关键修复 (2026-10-01): 把原 chunk 的尾部截断到 mid.
            //   若不截断, 原 worker 会继续把 [mid+1, remain_end] 也下完 → 重复下载
            //   (带宽浪费, 有效速度随时间衰减) + total_downloaded() 重复计数.
            //   download_chunk_range 每轮重读 chunk.end(), 截断后原 worker 于 mid 处停止.
            chunk.set_end(mid);
            chunks.push(new_chunk.clone());
            let mut pending = self.pending.lock();
            pending.push_back(new_id);
        }
        Some(new_chunk)
    }

    /// 已下载字节数 (按区间去重)。
    ///
    /// ★ 修复 (2026-10-02): 原来是把各 chunk 的 downloaded 直接相加, 但
    ///   `steal_from_slowest` 会**故意让新 chunk 与原 chunk 的尾部重叠**
    ///   (慢块被接管时, 新 worker 从剩余区间的后半段开始)。重叠部分被写两次、
    ///   也被计数两次 → 进度虚高、百分比乱跳。日志实证: 一个 5.03GB 的文件
    ///   结束时报告 downloaded=9,809,200,634 (≈1.95 倍)。
    ///
    /// 这里改为取各 chunk 的 [start, start+downloaded) 区间并集长度,
    /// 重叠只算一次。不改变下载/写盘行为, 只修正统计口径。
    pub fn total_downloaded(&self) -> u64 {
        let chunks = self.chunks.read();
        // 收集各 chunk 已完成的字节区间
        let mut ranges: Vec<(u64, u64)> = Vec::with_capacity(chunks.len());
        for c in chunks.iter() {
            let dl = c.downloaded.load(Ordering::Relaxed);
            if dl == 0 { continue; }
            let start = c.start;
            // 已下载字节连续覆盖 [start, start+dl-1] (写入是顺序推进的)
            let end = (start + dl).min(c.end() + 1);
            if end > start {
                ranges.push((start, end));
            }
        }
        if ranges.len() <= 1 {
            return ranges.first().map(|(s, e)| e - s).unwrap_or(0);
        }
        // 按起点排序后合并相交区间, 累加并集长度
        ranges.sort_unstable();
        let mut total = 0u64;
        let (mut cur_s, mut cur_e) = ranges[0];
        for &(s, e) in &ranges[1..] {
            if s <= cur_e {
                // 相交或相接 → 合并
                if e > cur_e { cur_e = e; }
            } else {
                total += cur_e - cur_s;
                cur_s = s;
                cur_e = e;
            }
        }
        total += cur_e - cur_s;
        total.min(self.file_size)
    }

    pub fn all_completed(&self) -> bool {
        let chunks = self.chunks.read();
        if chunks.is_empty() { return true; }
        chunks.iter().all(|c| c.is_completed())
    }

    pub fn completed_count(&self) -> usize {
        self.chunks.read().iter().filter(|c| c.is_completed()).count()
    }

    /// 尚未完成的块数 (尾部判定用)
    pub fn remaining_chunks(&self) -> usize {
        self.chunks.read().iter().filter(|c| !c.is_completed()).count()
    }

    /// pending 队列是否为空 (智能调度用: 无块可领 → 该把块切细)
    pub fn pending_is_empty(&self) -> bool {
        self.pending.lock().is_empty()
    }

    pub fn chunks_count(&self) -> usize {
        self.chunks.read().len()
    }

    // ============================================================
    // ★ 断点续传 (v2): 保存/加载 chunk 进度到 .swiftfetch-resume 文件
    // ============================================================
    /// 断点续传文件路径 (与旧 SwiftFetch 引擎格式兼容)
    pub fn resume_path(output: &std::path::Path) -> std::path::PathBuf {
        let mut s = output.as_os_str().to_os_string();
        s.push(".swiftfetch-resume");
        std::path::PathBuf::from(s)
    }

    /// 保存当前进度到断点续传文件 (JSON 格式)
    ///
    /// ★ 2026-09-30 改造: 改为保存**字节区间**而非 chunk id.
    ///   完全动态分块后, chunk 的 id → 字节区间 映射依赖运行时的初始种子数与
    ///   动态切分历史, 跨进程不保证一致; 按 id 恢复会把"已完成"标记到错误区间,
    ///   造成文件内容错乱. 按区间保存后, 恢复时按"覆盖关系"重新映射到新布局.
    pub fn save_resume(&self, output: &std::path::Path) {
        let chunks = self.chunks.read();
        let completed_ranges: Vec<[u64; 2]> = chunks.iter()
            .filter(|c| c.is_completed())
            .map(|c| [c.start, c.end()])
            .collect();
        let in_progress: Vec<[u64; 3]> = chunks.iter()
            .filter(|c| !c.is_completed() && c.downloaded.load(Ordering::Relaxed) > 0)
            .map(|c| [c.start, c.end(), c.downloaded.load(Ordering::Relaxed)])
            .collect();
        let rf = serde_json::json!({
            "file_size": self.file_size,
            "completed_ranges": completed_ranges,
            "in_progress": in_progress,
        });
        let path = Self::resume_path(output);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, serde_json::to_vec(&rf).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// 从断点续传文件加载进度, 返回恢复的字节数.
    /// 只有"被已完成区间完全覆盖"的新 chunk 才会被标记完成 (保守策略: 部分重叠
    /// 的字节一律重新下载, 绝不让未校验的字节被当作已完成).
    pub fn load_resume(&self, output: &std::path::Path) -> u64 {
        let path = Self::resume_path(output);
        let data = match std::fs::read_to_string(&path) {
            Ok(d) => d,
            Err(_) => return 0,
        };
        let v: serde_json::Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => return 0,
        };
        let file_size = v.get("file_size").and_then(|n| n.as_u64()).unwrap_or(0);
        if file_size != self.file_size {
            return 0; // 文件大小不匹配, 忽略
        }

        // 已完成区间 (按起点排序, 便于覆盖判定)
        let mut completed: Vec<(u64, u64)> = v.get("completed_ranges")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| {
                        let a = e.as_array()?;
                        Some((a.first()?.as_u64()?, a.get(1)?.as_u64()?))
                    })
                    .collect()
            })
            .unwrap_or_default();
        completed.sort_unstable();

        let mut resumed: u64 = 0;
        let chunks = self.chunks.read();
        for c in chunks.iter() {
            if c.is_completed() { continue; }
            let (cs, ce) = (c.start, c.end());
            // 1) 新 chunk 被某个已完成区间完全覆盖 → 直接标记完成
            if completed.iter().any(|&(s, e)| s <= cs && e >= ce) {
                c.downloaded.store(ce - cs + 1, Ordering::Relaxed);
                c.state.store(CHUNK_COMPLETED, Ordering::Relaxed);
                resumed += ce - cs + 1;
                continue;
            }
            // 2) 布局完全一致 (区间精确相等) 时才恢复部分进度
            if let Some(in_progress) = v.get("in_progress").and_then(|c| c.as_array()) {
                for entry in in_progress.iter() {
                    let Some(a) = entry.as_array() else { continue };
                    let s = a.first().and_then(|x| x.as_u64());
                    let en = a.get(1).and_then(|x| x.as_u64());
                    let dl = a.get(2).and_then(|x| x.as_u64());
                    if let (Some(s), Some(en), Some(dl)) = (s, en, dl) {
                        let size = ce - cs + 1;
                        if s == cs && en == ce && dl <= size
                            && dl > c.downloaded.load(Ordering::Relaxed)
                        {
                            c.downloaded.store(dl, Ordering::Relaxed);
                            resumed += dl;
                        }
                    }
                }
            }
        }
        resumed
    }

    /// 删除断点续传文件 (下载完成后调用)
    pub fn remove_resume(output: &std::path::Path) {
        let path = Self::resume_path(output);
        let _ = std::fs::remove_file(&path);
    }
}

// ============================================================
// DynamicScheduler - 轻量调度器, 仅调 Semaphore 许可数
// ============================================================

pub struct DynamicScheduler {
    pub ema_speed: u64,
    pub baseline_speed: u64, // probe 拿到的估算速度
    pub high_streak: u32,
    pub low_streak: u32,
    pub last_adjust_at: Instant,
    pub current_permits: u32,
    /// ★ 待回收的许可差额 (2026-10-03)。
    ///
    /// `throttle_on_429` 想把许可压到 ceiling 时, 若那些许可正被 worker 持有,
    /// `try_acquire_many` 会失败 —— 此时**不能**假装已经压下去了 (否则
    /// `sync_permits_to` 会按低估的账面值补许可, 造成只增不减的泄漏)。
    /// 差额记在这里, 由 `sync_permits_to` 在后续 tick 真正回收。
    pub permit_debt: u32,
}

impl DynamicScheduler {
    pub fn new(initial_permits: u32, baseline_speed: u64) -> Self {
        Self {
            ema_speed: 0,
            baseline_speed: baseline_speed.max(1024 * 1024),
            high_streak: 0,
            low_streak: 0,
            last_adjust_at: Instant::now() - Duration::from_secs(60),
            current_permits: initial_permits,
            permit_debt: 0,
        }
    }

    /// 输入瞬时速度, 返回 Some(新许可数) 表示需要调整
    /// ★ ceiling: 429 自适应并发上限, 高速加连接时不得越过 (防止撞 429 墙)
    pub fn tick(&mut self, instant_speed: u64, ceiling: u32) -> Option<u32> {
        // EMA 平滑
        if self.ema_speed == 0 {
            self.ema_speed = instant_speed;
        } else {
            let alpha = SPEED_EMA_ALPHA;
            self.ema_speed = ((1.0 - alpha) * self.ema_speed as f64 + alpha * instant_speed as f64) as u64;
        }

        // ★ 上限已被 429 下调时, 同步把 current_permits 压回上限以内。
        //   地板用 MIN_THROTTLED_CONNS(8) 而非 MIN_CONNS(16): 429 上限允许低于
        //   正常调度下限, 否则限流保护会被反向抬高而失效。
        let hard_cap = ceiling.min(MAX_CONNS).max(MIN_THROTTLED_CONNS);
        if self.current_permits > hard_cap {
            self.current_permits = hard_cap;
        }

        // 冷却期内不调
        if self.last_adjust_at.elapsed().as_millis() < SCHEDULER_COOLDOWN_MS as u128 {
            return None;
        }

        let ratio = if self.baseline_speed > 0 {
            self.ema_speed as f64 / self.baseline_speed as f64
        } else { 0.0 };

        if ratio > HIGH_RATIO {
            self.high_streak += 1;
            self.low_streak = 0;
            if self.high_streak >= HIGH_STREAK && self.current_permits < hard_cap {
                self.current_permits += 1;
                self.high_streak = 0;
                self.last_adjust_at = Instant::now();
                return Some(self.current_permits);
            }
        } else if ratio < LOW_RATIO {
            self.low_streak += 1;
            self.high_streak = 0;
            if self.low_streak >= LOW_STREAK && self.current_permits > MIN_CONNS {
                self.current_permits -= 1;
                self.low_streak = 0;
                self.last_adjust_at = Instant::now();
                return Some(self.current_permits);
            }
        } else {
            self.high_streak = 0;
            self.low_streak = 0;
        }
        None
    }
}

// ============================================================
// DownloadEngine - 取代 HybridChunkManager + SmoothScheduler + OscillationGuard
// ============================================================

pub struct DownloadEngine {
    pub cfg: DownloadConfig,
    pub pool: Arc<ChunkPool>,
    /// 输出文件: Arc<std::fs::File>, 配合 Windows FileExt::seek_write 实现无锁并发写入
    /// (seek_write 不修改文件指针, 多个 worker 可同时写不同偏移量, 无需互斥锁)
    pub file: Arc<std::fs::File>,
    pub downloaded: Arc<AtomicU64>,
    pub active_conns: AtomicU32,
    pub state_tx: watch::Sender<EngineState>,
    pub state_rx: watch::Receiver<EngineState>,
    pub cancel_flag: Arc<AtomicBool>,
    pub resume_notify: Arc<Notify>,
    pub client: reqwest::Client,
    pub mirrors: Vec<String>,
    pub task_id: String,
    pub start_instant: Instant,
    pub worker_count: u32,
    pub semaphore: Arc<Semaphore>,
    pub scheduler: PMutex<DynamicScheduler>,
    pub extra_headers: Vec<(String, String)>,
    /// 服务器是否支持 Range 请求 (不支持时退化为单连接整文件下载)
    pub supports_range: bool,
    /// ★ 打开输出文件**之前**它有多大 (2026-10-03)。
    ///   0 = 文件不存在或为空 → 说明续传记录是陈旧的 (用户可能已删掉坏文件),
    ///   此时绝不能信任续传记录, 否则会生成一个全零的"完成"文件。
    pub output_preexisting_bytes: u64,
    /// ★ 动态分块阈值: split_half / steal_from_slowest 的最小剩余阈值.
    ///   由 DynamicScheduler 根据实时速度调整: 高速 → 大阈值(少切分), 低速 → 小阈值(多切分)
    pub dynamic_max_chunk: AtomicU64,
    /// ★ 429 限流全局退避截止时间戳 (ms). 任何 worker 收到 429 时设置,
    ///   所有 worker 在此时刻前必须等待, 避免集体轰炸服务器导致持续 429.
    pub rate_limit_until: AtomicU64,
    /// ★ 连续 429 计数 (用于指数退避: 2s→4s→8s→16s→30s).
    ///   任一 chunk 下载成功时重置为 0.
    pub consecutive_429: AtomicU32,
    /// ★ 429 自适应并发上限 (核心修复): 一旦服务器开始 429, 记录一个"安全并发上限",
    ///   worker 实际并发 (active_conns) 被硬性限制在此值以内, 不再靠 semaphore 的
    ///   available_permits (下载中几乎为 0, 导致旧 throttle 形同虚设).
    ///   初始 = MAX_CONNS, 遇到 429 分级下调, 长时间无 429 再缓慢回升 (AIMD).
    pub throttled_ceiling: AtomicU32,
    /// ★ 累计 429 次数 (分级下调上限的依据, 不随成功重置)
    pub total_429: AtomicU32,
    /// ★ 最近一次 429 的时间戳 (ms), 用于判断是否已长时间无 429 可尝试回升上限
    pub last_429_at: AtomicU64,
    /// ★ 智能调度核心 (2026-10-02): 带宽档位判定 / AIMD 并发 / 块大小决策 / 429 冷却。
    ///   与上面的 DynamicScheduler 并存: DynamicScheduler 负责许可数的微调,
    ///   SmartScheduler 负责"档位 + 天花板 + 退让幅度"的宏观决策, 并在 429 时接管。
    pub smart: PMutex<crate::smart_sched::SmartScheduler>,
    /// ★ 请求令牌桶 (2026-10-02): 把"发起 HTTP 请求"这一动作本身限速。
    ///   实测该 CDN 按**请求数**限流 (约 140 次 / 60 秒), 见 `RequestBucket` 文档。
    pub req_bucket: RequestBucket,
    /// ★ 授权分级用的占空比限速器 (免费版 = 80%)。
    ///   `None` = 不限速 (付费/开发者)。见 `SpeedCap` 文档 —— 为什么不缩放连接数。
    pub throttle: Arc<SpeedCap>,
}

/// 自适应字节速率限制器 —— 真正能给出"原速的 N%"的限速。
///
/// ## 为什么不是"缩放并发连接数"
///
/// 免费版要求"下载速度只有原速的 80%"。最初按 0.8 缩放并发连接数 (64 → 51 条),
/// 实测在真实 CDN 上**完全无效**: 该 CDN 对单 IP 总带宽封顶 ~12~15MB/s,
/// 51 条与 64 条撞同一个天花板 —— 两轮实测免费档(12.74MB/s)反而比付费档(8.90MB/s)快。
///
/// ## 为什么不是"时间占空比"
///
/// 第二版改成"每秒只给 800ms 收数据"。实测**同样无效**: 94.46 vs 96.07 MB/s, 只差 1.7%。
/// 根因是 **TCP 接收缓冲区**: 停读的那 200ms 里服务器继续往内核缓冲区灌数据,
/// 恢复后瞬间排空 —— 暂停等于没发生。缓冲区越大、周期越短, 这个"泄漏"越严重。
///
/// ## 现在的做法: 先测基准, 再按字节限速
///
/// 先跳过 `SKIP_MS` 的建连期, 再用 `MEASURE_MS` 窗口测出这条源的**真实原速**,
/// 之后按 `原速 × ratio`
/// 做字节令牌桶。字节速率限制**不会被缓冲区吃掉** —— 因为我们限制的是
/// "内核总共交出多少字节": 缓冲区填满后 TCP 窗口关闭, 服务器被自然反压。
///
/// 这样无论源站多快 (清华镜像 ~96MB/s, 限流 CDN ~12MB/s), 免费档拿到的都是它的 80%。
pub struct SpeedCap {
    /// 允许下载的比例 ×10000 (10000 = 不限速)。
    /// 用原子量是为了**运行中可改**: 用户暂停后换密钥再继续时, 必须立刻按新档位限速
    /// (实测过: 暂停→切免费→继续, 因为引擎是启动时建好的, 切换完全没生效)。
    ratio_x10000: AtomicU64,
    /// 计时起点 (毫秒时间戳)。用时间戳而不是 Instant, 便于切换档位时重置测量窗口。
    start_ms: AtomicU64,
    /// 测量窗口内累计读到的字节数
    cur_bucket_bytes: AtomicU64,
    /// 测得的基准速率 (B/s); 0 = 还没测出来
    baseline_bps: AtomicU64,
    /// 按当前比例算出的速率上限 (B/s); 0 = 还没算出来
    cap_bps: AtomicU64,
    /// 字节令牌桶: (当前可用字节, 上次补充时刻)
    bucket: PMutex<(f64, Instant)>,
}

impl SpeedCap {
    /// 跳过时长: 开局这一段在建立 64 条 TLS 连接 + TCP 慢启动, 速率远低于稳态
    /// (实测 2 秒窗口只有 60MB/s, 而稳态是 106MB/s)。拿它当基准会把免费档压过头
    /// (实测只有付费档的 47%, 而不是 80%)。
    pub const SKIP_MS: u64 = 1_500;
    /// 基准测量窗口长度 (跳过建连期之后才开始计)。
    ///
    /// ★ 2s → 6s (2026-10-02)。这个 CDN 自身波动极大, 同一文件多轮实测
    /// 付费档跑出 19.64 / 12.61 / 11.20 MB/s (相差近 2 倍), 免费档测得的基准
    /// 也有 13.42 / 13.97 / 23.88。窗口太短时基准严重失真:
    ///   · 用 2 秒**平均** → 偏低 → 免费档被压过头 (实测只有付费档的 59%);
    ///   · 改用窗口内**最高一秒** → 偏高 → 免费档反而比付费档快 (实测 17.89 vs 11.20)。
    /// 6 秒平均值才既不被单秒尖峰带偏, 也不被单秒掉速带偏。
    pub const MEASURE_MS: u64 = 6_000;
    /// 速率下限: 防止基准测歪时把速度压到不可用
    pub const MIN_CAP_BPS: f64 = 128.0 * 1024.0;

    pub fn new(ratio: f64) -> Self {
        Self {
            ratio_x10000: AtomicU64::new((ratio.clamp(0.05, 1.0) * 10000.0) as u64),
            start_ms: AtomicU64::new(now_ms()),
            cur_bucket_bytes: AtomicU64::new(0),
            baseline_bps: AtomicU64::new(0),
            cap_bps: AtomicU64::new(0),
            bucket: PMutex::new((0.0, Instant::now())),
        }
    }

    /// 运行中改限速比例 (1.0 = 不限速)。
    ///
    /// 已经测出基准时直接按新比例重算上限, **不必重新等 6 秒测量窗口** ——
    /// 用户切档后应该立刻按新速度跑, 而不是先全速跑 7.5 秒。
    pub fn set_ratio(&self, ratio: f64) {
        let v = (ratio.clamp(0.05, 1.0) * 10000.0) as u64;
        if self.ratio_x10000.swap(v, Ordering::Relaxed) == v {
            return;
        }
        let base = self.baseline_bps.load(Ordering::Relaxed);
        if base > 0 {
            let c = ((base as f64) * (v as f64 / 10000.0)).max(Self::MIN_CAP_BPS);
            self.cap_bps.store(c as u64, Ordering::Relaxed);
        } else {
            // 还没测过基准 → 重置测量窗口, 重新测一遍
            self.start_ms.store(now_ms(), Ordering::Relaxed);
            self.cur_bucket_bytes.store(0, Ordering::Relaxed);
        }
    }

    /// 当前比例 (1.0 = 不限速)
    pub fn ratio(&self) -> f64 {
        self.ratio_x10000.load(Ordering::Relaxed) as f64 / 10000.0
    }

    /// 测出的速率上限 (B/s); 还没进入限速阶段时为 0。
    pub fn cap_bps(&self) -> u64 {
        self.cap_bps.load(Ordering::Relaxed)
    }

    /// 基准窗口内测得的速率 (B/s), 用于诊断/对比。
    pub fn baseline_bps(&self) -> u64 {
        self.baseline_bps.load(Ordering::Relaxed)
    }

    /// 刚读到 `n` 字节后调用。返回需要等待的毫秒数 (0 = 不用等)。
    pub fn consume(&self, n: u64) -> u64 {
        let ratio = self.ratio();
        // 不限速 → 一次原子读就返回, 不碰锁 (付费/开发者走这条路径)
        if ratio >= 1.0 {
            return 0;
        }
        let elapsed = now_ms().saturating_sub(self.start_ms.load(Ordering::Relaxed));

        // ---- 阶段 1: 建连/慢启动期 —— 不计数也不限制 ----
        if elapsed < Self::SKIP_MS {
            return 0;
        }
        // ---- 阶段 2: 基准测量窗口 —— 只累计, 不限制 ----
        if elapsed < Self::SKIP_MS + Self::MEASURE_MS {
            self.cur_bucket_bytes.fetch_add(n, Ordering::Relaxed);
            return 0;
        }

        // ---- 阶段 3: 用测得的平均速率算出上限 ----
        let mut cap = self.cap_bps.load(Ordering::Relaxed);
        if cap == 0 {
            let bytes = self.cur_bucket_bytes.load(Ordering::Relaxed) as f64;
            let base_bps = bytes * 1000.0 / Self::MEASURE_MS as f64;
            self.baseline_bps.store(base_bps as u64, Ordering::Relaxed);
            let c = (base_bps * ratio).max(Self::MIN_CAP_BPS);
            let ci = c as u64;
            // 只有一个 worker 会赢下这次 CAS, 避免多个 worker 重复重置桶
            if self
                .cap_bps
                .compare_exchange(0, ci, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                let mut b = self.bucket.lock();
                *b = (c, Instant::now()); // 桶容量 = 1 秒的量
            }
            cap = self.cap_bps.load(Ordering::Relaxed);
        }
        let cap_f = cap as f64;

        // ---- 字节令牌桶 (全局共享, 所以是所有 worker 加起来的上限) ----
        let mut b = self.bucket.lock();
        let (tokens, last) = &mut *b;
        let now = Instant::now();
        let dt = now.duration_since(*last).as_secs_f64();
        *last = now;
        *tokens = (*tokens + dt * cap_f).min(cap_f);
        *tokens -= n as f64;
        if *tokens >= 0.0 {
            0
        } else {
            ((-*tokens) / cap_f * 1000.0) as u64
        }
    }
}

/// 全局请求令牌桶 —— 把请求速率压在服务器的限流阈值之下。
///
/// 实测 cdn1.trashbytes.to (Cloudflare) 的限流是按**请求数**计的:
///   · 24 个请求          → 0 个 429
///   · 150 个请求 / 60 秒 → 9 个 429 (≈141 通过)
///   · 192 个请求一次性发 → 55 个 429
/// 即大约 **140 请求 / 60 秒**。
///
/// 旧实现一开局就把 192 个 chunk 全部发出去, 必然越限 → 429 → 而 429 的重试
/// 本身又是请求, 把惩罚窗口不断续期 → 限流永不解除 → 尾部速度归零 (实测)。
///
/// 容量 100 允许开局一次正常突发; 补充 2.2 个/秒 (=132/分钟) 略低于实测阈值,
/// 留出余量给重试与并发抖动。
pub struct RequestBucket {
    inner: PMutex<(f64, Instant)>,
    /// 因桶空而被迫等待的次数 (诊断用: 证明限速确实在起作用)
    blocked: AtomicU32,
}

impl RequestBucket {
    /// 桶容量 (允许的开局突发请求数)
    ///
    /// ★ 定值依据 (2026-10-02): 取 **worker 数** —— 开局每条连接各发一个请求,
    ///   这是应用天然的突发上限, 不需要也不应该更激进。
    ///
    ///   注意别把 curl 实测的"~130 请求/60 秒"直接搬到这里当请求速率上限:
    ///   curl 每个请求都**新建 TLS 连接**, 而 reqwest 复用 keep-alive 连接 ——
    ///   服务器真正限制的是**新连接数**, 不是 HTTP 请求数。实测佐证:
    ///   应用以 64 条复用连接发出 90 个请求 (≈300/分钟) 时 429 次数为 **0**。
    ///   早先把容量/补充按"130/60s"收紧到 40 + 1.5/s, 结果桶自己成了瓶颈
    ///   (一次 295MB 下载被限速 769 次, 耗时 18.7s → 39.2s), 而 429 并未减少。
    pub const CAPACITY: f64 = 64.0;
    /// 每秒补充的令牌数。
    ///   取值宽松 (180/分钟), 目的是给"块很小导致请求过快"兜底, 而不是限制正常下载 ——
    ///   正常下载的请求速率由 worker 数 (64) 与单请求耗时天然约束。
    pub const REFILL_PER_SEC: f64 = 3.0;

    pub fn new() -> Self {
        Self {
            inner: PMutex::new((Self::CAPACITY, Instant::now())),
            blocked: AtomicU32::new(0),
        }
    }

    /// 累计被限速的次数
    pub fn blocked_count(&self) -> u32 {
        self.blocked.load(Ordering::Relaxed)
    }

    /// 归还一个令牌。
    ///
    /// ★ 2026-10-02: worker 在**取到块之前**就要领令牌 (否则等令牌的 worker 会把
    ///   块全标成 ASSIGNED 却不下载, 触发"pending 为空 → 切分"的误判)。但这样
    ///   一来, 领了令牌却因拿不到块而空转的 worker 会把令牌白白吃掉 ——
    ///   实测一个只需 88 个请求的任务, 令牌桶被空转 worker 抽干, 从 23s 起
    ///   把真实请求也节流到 2.2/s, 平均速度被拖到 6.3MB/s。
    ///   凡是没真正发出请求的分支都必须调用本方法把令牌还回去。
    pub fn give_back(&self) {
        let mut g = self.inner.lock();
        g.0 = (g.0 + 1.0).min(Self::CAPACITY);
    }

    /// 尝试取一个令牌。取到返回 None; 取不到返回需要等待的毫秒数。
    pub fn try_take(&self) -> Option<u64> {
        let mut g = self.inner.lock();
        let (tokens, last) = &mut *g;
        let now = Instant::now();
        let elapsed = now.duration_since(*last).as_secs_f64();
        *tokens = (*tokens + elapsed * Self::REFILL_PER_SEC).min(Self::CAPACITY);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            None
        } else {
            let n = self.blocked.fetch_add(1, Ordering::Relaxed) + 1;
            // 只记前几次: 证明限速生效即可, 避免日志被刷屏
            if n <= 5 {
                engine_log!(
                    "[bucket] 请求限速生效: 第 {} 次等待 (容量 {:.0}, 补充 {:.1}/s)",
                    n,
                    Self::CAPACITY,
                    Self::REFILL_PER_SEC
                );
            }
            let need_secs = (1.0 - *tokens) / Self::REFILL_PER_SEC;
            Some((need_secs * 1000.0).ceil().max(1.0) as u64)
        }
    }
}

#[derive(Debug)]
pub enum DownloadError {
    Timeout,
    Canceled,
    Network(String),
    Io(String),
    Other(String),
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "timeout"),
            Self::Canceled => write!(f, "canceled"),
            Self::Network(s) => write!(f, "network: {}", s),
            Self::Io(s) => write!(f, "io: {}", s),
            Self::Other(s) => write!(f, "{}", s),
        }
    }
}

impl std::error::Error for DownloadError {}

impl DownloadEngine {
    /// 创建新引擎: probe 文件大小 + Range 支持, 初始化 chunk pool
    pub async fn new(
        cfg: DownloadConfig,
        state_tx: watch::Sender<EngineState>,
        state_rx: watch::Receiver<EngineState>,
        cancel_flag: Arc<AtomicBool>,
        resume_notify: Arc<Notify>,
    ) -> Result<Self, DownloadError> {
        // ★ 用专用下载 client (HTTP/1.1 + 多独立连接), 替代通用 client (可能 HTTP/2 多路复用)
        let client = build_download_client(&cfg)?;
        // probe: GET Range: bytes=0-0 (project_memory 约束, 不用 HEAD)
        let probe = probe_file_size(&client, &cfg.url, &cfg.headers).await?;
        let file_size = probe.file_size;
        let supports_range = probe.supports_range;

        // ★ 修复 Bug: 不支持 Range 时退化为单连接整文件下载
        //   原代码: 无论是否支持 Range, 都用 16 worker + 多 chunk
        //   → 服务器返回 200 (整个文件), 每个 worker 都从头下载整个文件, 互相覆盖
        //   → 文件损坏 + 速度虚假 + 实际只下了部分字节
        // ★ 完全动态分块 + 多线程异步调度 (2026-09-30):
        //   并发流数与分块规模全部由运行时线程数推导 (2 核 → 4 线程 → 64 条流),
        //   初始只投放少量"种子块", 其余块在下载过程中按实测速度动态派生.
        //
        // ★ 慢启动修复 (2026-10-02): 起始并发从"直接打满 64"改为**保守起步**。
        //   日志实证: 开局 2 秒内 64 条连接把速度冲到 53.8 MB/s, 立刻触发 CDN 429,
        //   之后被反复限流, 速度从 53MB/s 崩到 161KB/s 再也起不来。
        //   现在起点取 max_streams/8 (4 线程 → 8 条), 由 DynamicScheduler 按实测速度
        //   逐步加连接 (每 2 秒 +1, 直到 ceiling)。起步慢一点, 但不会撞限流墙。
        let threads = runtime_worker_threads();
        let max_streams = streams_for_threads(threads);
        let (initial_chunks, worker_count, permits, max_chunks) = if supports_range {
            let ic = initial_chunk_seed(file_size, probe.estimated_speed_bps, max_streams);
            let wc = DEFAULT_WORKER_COUNT.min(max_streams).max(1);
            // 块数上限 = min(并发流数 × 4, 请求预算): 保证每条流都有多个候选块可抢,
            // 同时封顶 —— 块数就是请求数, 超过服务器的限流预算必然触发 429。
            //
            // ★★ 2026-10-08 修正: 上限**必须明显大于种子数**。
            //   种子数对大文件会被 REQUEST_BUDGET(128) 压满（5GB → 128 块 × 40MB），
            //   而上限原来也等于同一个 128 → `can_grow()` 永远 false →
            //   **整个下载过程中再也切不出新块**。于是块一完成并发就少一条：
            //   实测 tick=1000 时 active=23、speed=1.2MB/s，尾部几十 MB 全靠
            //   二十几条连接硬啃，用户看到的就是"结尾速度骤降、下载不完"。
            //   真正防 429 的是 RequestBucket（每个请求都要过令牌桶），
            //   块数上限只用来防无限碎片化，所以给足余量：
            //   按"每 4MB 一块"推算，上限 512（远超 128，且请求速率仍受令牌桶约束）。
            const MAX_CHUNKS_HARD: u32 = 512;
            let size_based = ((file_size / (4 * 1024 * 1024)) as u32).clamp(64, MAX_CHUNKS_HARD);
            let mc = max_streams
                .saturating_mul(4)
                .max(ic)
                .max(size_based)
                .min(MAX_CHUNKS_HARD)
                .max(ic);
            // 起始许可: 保守 (1/8), 但不低于 MIN_THROTTLED_CONNS
            let start = (max_streams / 8).max(MIN_THROTTLED_CONNS).min(wc);
            (ic, wc, start, mc)
        } else {
            engine_log!("[dynamic_engine] 服务器不支持 Range, 退化为单连接整文件下载: {}", cfg.url);
            (1u32, 1u32, 1u32, 1u32)
        };
        engine_log!(
            "[dynamic_engine] 动态分块初始化: threads={} streams={} start_conns={} seed_chunks={} max_chunks={} file_size={} probe_speed={} B/s",
            threads, max_streams, permits, initial_chunks, max_chunks, file_size, probe.estimated_speed_bps
        );

        let pool = Arc::new(ChunkPool::new(file_size, initial_chunks, max_chunks));
        let semaphore = Arc::new(Semaphore::new(permits as usize));
        let scheduler = DynamicScheduler::new(permits, probe.estimated_speed_bps);

        // 创建输出文件 (预分配) — 直接持有 Arc<std::fs::File>, 配合 seek_write 无锁并发写
        let output = cfg.output.clone().unwrap_or_default();
        if let Some(parent) = output.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // ★ 必须在 set_len 之前记下"打开前文件有多大" (2026-10-03)。
        //   set_len 会把文件撑成 file_size 的稀疏文件, 之后再问长度就永远是
        //   file_size, 分不出"本来就有数据"和"刚被创建成空文件"。
        //   用途见 download_file 里的续传安全检查。
        let output_preexisting_bytes = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .open(&output)
            .map_err(|e| DownloadError::Io(e.to_string()))?;
        // 预分配大小 (sparse)
        let _ = file.set_len(file_size);

        Ok(Self {
            // ★ 修复 Bug (2026-09-08): 原 extra_headers 为空 → download_chunk_range 只发 Range 头,
            //   不发 User-Agent → PlayZip CDN 等服务器识别为非浏览器请求并限速到 20MB/s.
            //   修复: 把 cfg.headers (含 User-Agent/Accept/Referer 等浏览器头) 复制到 extra_headers,
            //   使每个 chunk 下载请求都带浏览器指纹, 突破 CDN 单连接限速.
            extra_headers: cfg.headers.clone(),
            cfg,
            pool,
            file: Arc::new(file),
            downloaded: Arc::new(AtomicU64::new(0)),
            active_conns: AtomicU32::new(0),
            state_tx,
            state_rx,
            cancel_flag,
            resume_notify,
            client,
            mirrors: Vec::new(),
            task_id: String::new(),
            start_instant: Instant::now(),
            worker_count,
            semaphore,
            scheduler: PMutex::new(scheduler),
            supports_range,
            output_preexisting_bytes,
            // ★ 初始动态分块阈值: 根据 probe 速度选择
            dynamic_max_chunk: AtomicU64::new(dynamic_max_chunk_for_speed(probe.estimated_speed_bps)),
            rate_limit_until: AtomicU64::new(0),
            consecutive_429: AtomicU32::new(0),
            throttled_ceiling: AtomicU32::new(MAX_CONNS),
            total_429: AtomicU32::new(0),
            last_429_at: AtomicU64::new(0),
            smart: PMutex::new({
                // ★ 不再用 probe 定初始档位 (2026-10-02)。
                //
                //   实测证明 probe 的 8KB 采样对 CDN 极不准: 报 1MB/s 而实际峰值 88MB/s
                //   (相差 88 倍!)。据此定档位会把整条链路一开始就压在最低档,
                //   起步只 4 条连接 + 慢爬升, 短任务里前几秒完全是浪费。
                //
                //   改为统一从 Medium 起步 (起步 64 条连接): 宁可一开始就快,
                //   由 SmartScheduler 在运行中按**实测峰值**升档 (升档不降档),
                //   撞到 429 则由乘性减半 + 冷却兜住。这样不依赖任何不可靠的先验。
                let tier = crate::smart_sched::BandwidthTier::Medium;
                // peak 基准用一个中性值而非 probe 值: 若用 probe 的 1MB/s 当 peak,
                // 第一次采样 (可能几十 MB/s) 会被当成"暴涨", 利用率判定失去意义。
                // 取 8MB/s 作为中性起点, 运行中由真实采样迅速取代。
                let baseline = 8 * 1024 * 1024u64;
                crate::smart_sched::SmartScheduler::new(tier, baseline)
            }),
            req_bucket: RequestBucket::new(),
            throttle: Arc::new(SpeedCap::new(1.0)),
        })
    }

    /// 设置授权限速比例 (1.0 = 不限速; 0.8 = 免费版只给 80% 的时间在收数据)
    pub fn with_speed_ratio(self, ratio: f64) -> Self {
        self.throttle.set_ratio(ratio);
        self
    }

    /// 设置任务 ID (用于日志)
    pub fn with_task_id(mut self, task_id: String) -> Self {
        self.task_id = task_id;
        self
    }

    /// 设置额外 headers
    pub fn with_extra_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }

    /// 打开文件用于写入 (已在 new 中完成, 此处为 no-op 保持 API 兼容)
    pub async fn open_file(&self) -> Result<(), DownloadError> {
        Ok(())
    }
}

// ============================================================
// 初始 chunk 数计算
// ============================================================

/// ★ 下载运行时线程数 (2 核 → 4 线程).
///   优先读 SF_WORKER_THREADS, 其次 VORTEX_WORKER_THREADS, 默认 4, 夹在 [2, 64].
///   与 SwiftFetch CLI / VortexDL 构造 tokio runtime 时用的是同一个环境变量,
///   因此这里是"分块粒度与并发流数"的唯一权威来源.
pub fn runtime_worker_threads() -> usize {
    std::env::var("SF_WORKER_THREADS")
        .or_else(|_| std::env::var("VORTEX_WORKER_THREADS"))
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(4)
        .clamp(2, 64)
}

/// ★ 由运行时线程数推导并发下载流数 (多线程异步任务调度的规模).
///   下载是网络 I/O 密集型, 每个 tokio 工作线程可承载多条独立 TCP 流:
///   4 线程 → 64 条流 (与历史默认一致); 线程更多按 16 倍线性增长, 上限 MAX_CONNS.
pub fn streams_for_threads(threads: usize) -> u32 {
    // ★ 倍率定在 16 (4 线程 → 64 条流)。
    //
    //   2026-10-02 实测(同一 1GB 文件, 同一 CDN):
    //     64 条 → 11.74 MB/s      128 条 → 10.56 MB/s (反而更慢)
    //     两个进程各 64 条 (合计 128) → 14.0 MB/s
    //     curl 48 条 → 15.06 MB/s
    //   即**聚合吞吐在 ~12~15 MB/s 处封顶**, 再加连接几乎没有收益。
    //   对照实验: 同一条宽带从清华镜像 16 条连接可跑 **106.99 MB/s (897Mbps)** ——
    //   所以瓶颈是这个 CDN 对单 IP 的总带宽限制, 不是客户端、更不是用户的宽带。
    //   既然 64 条已经贴着天花板, 就没有理由开更多连接去换 429 风险。
    ((threads as u32).saturating_mul(16)).clamp(8, MAX_CONNS)
}

/// ★ 动态分块阈值 (split_half / steal_from_slowest 的最小剩余大小)
///   高速 → 大阈值: 减少切分次数, 降低调度开销
///   低速 → 小阈值: 频繁切分, 让慢 chunk 被接管, 提高并发利用率
///
/// ★ 2026-09-30 调整: 各档位下调一档 (16/8/4/2 MB → 8/4/2/1 MB).
///   完全动态分块后, 块的增长完全依赖切分触发, 阈值过大时"慢块"要等到剩余
///   32MB 以下才可切 → 开局/中段慢源无法及时被接管. 下调后切分更及时,
///   由 MIN_CHUNK_SIZE(1MB) 兜底防止碎片化.
fn dynamic_max_chunk_for_speed(speed_bps: u64) -> u64 {
    const MB: u64 = 1024 * 1024;
    if speed_bps >= 5 * MB {
        8 * MB    // 高速: 8MB 才切
    } else if speed_bps >= 1 * MB {
        4 * MB    // 中速: 4MB
    } else if speed_bps >= 256 * 1024 {
        2 * MB    // 低速: 2MB
    } else {
        MIN_CHUNK_SIZE   // 极低速: 1MB, 最大化切分频率
    }
}

/// ★ 完全动态分块 (2026-09-30 改造) — 初始"种子分块"数量.
///
/// 旧实现按 `文件大小 / 目标块大小` 一次性把整个文件预切成 2~256 个等长块,
/// 属于"静态预切分"与"动态切分"并存. 问题:
///   · 大文件 + 低速时开局瞬间铺开上百个 chunk → 请求风暴;
///   · 高速时块又被切得过碎 → 调度开销上升;
///   · 分块布局随 probe 速度变化 → 断点续传按 chunk id 恢复会错位.
///
/// 新实现只投放少量种子块 (按实测速度档位决定), 之后块数完全由运行时的
/// `split_half` / `steal_from_slowest` 按实时速度动态派生, 并受
/// `MIN_CHUNK_SIZE` 与 `max_chunks` 约束. 布局也因此与速度解耦.
fn initial_chunk_seed(file_size: u64, speed_bps: u64, max_streams: u32) -> u32 {
    const MB: u64 = 1024 * 1024;
    if file_size <= MIN_CHUNK_SIZE { return 1; }
    // 速度档位 → 种子数 (刻意保守, 让动态切分接管增长)
    let tier: u32 = if speed_bps >= 5 * MB {
        32
    } else if speed_bps >= 1 * MB {
        16
    } else if speed_bps >= 256 * 1024 {
        8
    } else {
        4
    };
    // 不能超过文件本身能切出的块数, 否则会产生 < MIN_CHUNK_SIZE 的碎片
    let max_by_size = (file_size / MIN_CHUNK_SIZE).max(1) as u32;
    // ★ 请求预算约束 (2026-10-02): 该 CDN 按**请求数**限流 (实测 ~140/60s),
    //   而"种子块数"直接决定开局一次性要发多少个请求。旧实现取
    //   max(档位, 并发流数) → 192 条流就切 192 块 → 192 个请求齐发 → 必然越限
    //   → 429 风暴 → 重试把惩罚窗口不断续期 → 尾部速度归零 (实测)。
    //   现在按"每块约 7MB"定块数, 并把总数压在 REQUEST_BUDGET 以内:
    //   块少而大 → 请求少 → 速率可控, 且每条连接能持续下载大段数据。
    //
    //   ★ 4MB → 7MB (2026-10-02): 求解"请求速率不得超过服务器额度 R、吞吐 = 连接数 × 单连接速度 v"
    //   得最优块数 N = S·R / min(C·v, sqrt(S·R·v))。代入实测值
    //   (R≈2.17/s, v≈348KB/s, C=64):
    //     295MB → N≈43 块 (≈7MB/块), 理论吞吐 ≈15MB/s
    //     1GB   → N≈100 块
    //   4MB 会让 295MB 切出 73 块, 请求数远超最优 → 白白逼近限流额度。
    let by_chunk_target = (file_size / (7 * 1024 * 1024)).max(1) as u32;
    tier
        .max(max_streams)
        .max(by_chunk_target)
        .min(REQUEST_BUDGET)
        .min(max_by_size)
        .max(1)
}

// ============================================================
// Probe 文件大小 (GET Range: bytes=0-0)
// ============================================================

pub struct ProbeFileSize {
    pub file_size: u64,
    pub supports_range: bool,
    pub estimated_speed_bps: u64,
}

async fn probe_file_size(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
) -> Result<ProbeFileSize, DownloadError> {
    // ★ 429 时重试 (2026-10-03): 旧代码**不看状态码**, 直接在响应里找
    //   Content-Range / Content-Length。撞上限流时服务器返回的是错误页,
    //   两个头都没有 → file_size 落到 0 → 引擎把文件当成 0 字节 →
    //   瞬间"下载完成"却是个空文件 (实测日志: `文件大小=0 种子块=0` +
    //   `结果=成功 下载=0 字节`)。
    let mut attempt = 0u32;
    let (resp, elapsed) = loop {
        let mut req = client.get(url).header("Range", "bytes=0-0");
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let start = Instant::now();
        let resp = req
            .send()
            .await
            .map_err(|e| DownloadError::Network(format!("{:#}", e)))?;
        let elapsed = start.elapsed().as_secs_f64().max(0.001);
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt < 3 {
            attempt += 1;
            // 尊重服务器的 Retry-After, 并留 2 秒余量越过窗口边界
            let wait_ms = retry_after_secs(resp.headers())
                .unwrap_or(5)
                .saturating_mul(1000)
                .min(120_000)
                + 2_000;
            engine_log!(
                "[probe] 探测撞 429 限流, 等待 {}ms 后重试 (第 {} 次)",
                wait_ms, attempt
            );
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;
            continue;
        }
        break (resp, elapsed);
    };
    // Content-Range: bytes 0-0/12345
    let mut file_size = 0u64;
    let mut supports_range = false;
    if let Some(cr) = resp.headers().get(reqwest::header::CONTENT_RANGE) {
        if let Ok(s) = cr.to_str() {
            // bytes 0-0/12345
            if let Some(total) = s.split('/').nth(1) {
                if let Ok(n) = total.trim().parse::<u64>() {
                    file_size = n;
                    supports_range = true;
                }
            }
        }
    }
    if file_size == 0 {
        if let Some(cl) = resp.headers().get(reqwest::header::CONTENT_LENGTH) {
            if let Ok(s) = cl.to_str() {
                if let Ok(n) = s.trim().parse::<u64>() {
                    file_size = n;
                }
            }
        }
    }
    // ★ 诊断 (2026-10-08): "服务器不支持 Range" 会直接退化成**单连接整文件下载**
    //   （实测 2.4GB 只能跑 ~7MB/s，而多连接能到 40MB/s —— 用户报"比以前慢太多"）。
    //   这一行把判定依据全打出来：状态码 200 = 服务器无视了 Range 头（真不支持），
    //   206 + Content-Range = 支持。以后遇到"单连接"直接看这条日志就能定性。
    engine_log!(
        "[probe] status={} content-range={:?} content-length={:?} accept-ranges={:?} → supports_range={} size={}",
        resp.status().as_u16(),
        resp.headers().get(reqwest::header::CONTENT_RANGE).and_then(|v| v.to_str().ok()),
        resp.headers().get(reqwest::header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()),
        resp.headers().get(reqwest::header::ACCEPT_RANGES).and_then(|v| v.to_str().ok()),
        supports_range,
        file_size
    );
    // ★★ 二次探测 (2026-10-08)：只测 `bytes=0-0` 会误判。
    //
    //   实测 game.galgamex.com（S3 预签名 URL 反代）对 `Range: bytes=0-0` 直接
    //   返回 **200 + 完整 Content-Length**，但响应头里明明写着 `Accept-Ranges: bytes`
    //   —— 于是引擎判定"服务器不支持 Range"，**整个文件退化成单连接下载**
    //   （2.4GB 只有 7MB/s，而多连接能到 40MB/s，用户报"比以前慢太多"）。
    //
    //   很多这类反代只是把"0-0 这种退化区间"当普通请求处理。所以这里再试一次
    //   **中段区间**：能拿到 206/Content-Range 就说明 Range 是支持的，按多连接下。
    //   代价只是多一个几乎不发正文的请求（读完响应头就丢掉，不会把整个文件拉下来）。
    if !supports_range && file_size > 0 {
        let mid = file_size / 2;
        let mut req2 = client
            .get(url)
            .header("Range", format!("bytes={}-{}", mid, mid + 1023));
        for (k, v) in headers {
            req2 = req2.header(k, v);
        }
        if let Ok(r2) = req2.send().await {
            let st2 = r2.status().as_u16();
            let cr2 = r2
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let cl2 = r2
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            if st2 == 206 || cr2.is_some() {
                supports_range = true;
                // 用 Content-Range 的总长校准一次文件大小（中段探测才有的信息）
                if let Some(cr) = &cr2 {
                    if let Some(total) = cr.split('/').nth(1) {
                        if let Ok(n) = total.trim().parse::<u64>() {
                            if n > 0 {
                                file_size = n;
                            }
                        }
                    }
                }
                engine_log!(
                    "[probe] 中段区间探测成功 (status={} content-range={:?}) → 改判为支持 Range，走多连接",
                    st2, cr2
                );
            } else {
                engine_log!(
                    "[probe] 中段区间仍返回 status={} content-length={:?} → 确认不支持 Range，单连接下载",
                    st2, cl2
                );
            }
        }
    }
    // ★ 2026-10-01 修复: 旧实现仅测 8KB 且未剔除连接建立开销 → 实测恒为 5~19 KB/s,
    //   被 floor 到 1MB/s → initial_chunk_seed / dynamic_max_chunk 的 tier 永远选错.
    //   现改为三步: (1) 先发 1 字节预热请求建立并复用 TCP/TLS 连接;
    //               (2) 对 1MB 样本计时; (3) 减去初始 probe 的 RTT(elapsed) 得纯传输时间.
    let resp_status = resp.status();
    let sample_bytes: u64 = if file_size > 0 { std::cmp::min(1024 * 1024, file_size) } else { 0 };
    let estimated = if sample_bytes > 0 {
        // (1) 预热请求: 建立连接, 避免把握手时间算进传输耗时
        let mut warm = client.get(url).header("Range", "bytes=0-0");
        for (k, v) in headers {
            warm = warm.header(k, v);
        }
        let _ = warm.send().await;
        // (2) 正式计时: 1MB 样本
        let mut sample_req = client.get(url).header("Range", format!("bytes=0-{}", sample_bytes - 1));
        for (k, v) in headers {
            sample_req = sample_req.header(k, v);
        }
        let t0 = Instant::now();
        match sample_req.send().await {
            Ok(r) if r.status().is_success() => {
                let mut total = 0u64;
                let mut stream = r.bytes_stream();
                use futures::StreamExt;
                while let Some(chunk) = stream.next().await {
                    if let Ok(b) = chunk {
                        total += b.len() as u64;
                        if total >= sample_bytes { break; }
                    } else { break; }
                }
                // (3) 纯传输时间 = 总耗时 - 连接/首字节延迟
                let secs = (t0.elapsed().as_secs_f64() - elapsed).max(0.005);
                let est = (total as f64 / secs) as u64;
                // 夹到合理区间, 防止异常值把 tier 带偏
                est.clamp(256 * 1024, 200 * 1024 * 1024)
            }
            _ => (1.0 / elapsed) as u64 * 8,
        }
    } else {
        (1.0 / elapsed) as u64 * 8
    };
    // ★ 拿不到大小就必须报错, 不能默认 0 (2026-10-03)。
    //   否则引擎会把文件当成 0 字节 → 瞬间"下载完成" → 前端弹出解压空文件,
    //   用户看到的是"下载好了"但文件是坏的。宁可明确失败让用户重试。
    if file_size == 0 && !resp_status.is_success() {
        return Err(DownloadError::Network(format!(
            "探测文件大小失败: HTTP {} (服务器可能正在限流, 请稍后重试)",
            resp_status
        )));
    }
    if file_size == 0 {
        return Err(DownloadError::Network(
            "服务器未返回文件大小 (缺少 Content-Range/Content-Length)".into(),
        ));
    }
    // ★ 诊断日志: 记录 probe 结果, 便于排查 "4g 下完 2.8g" 类问题
    engine_log!(
        "[dynamic_engine] probe: url={} status={} file_size={} ({} bytes) supports_range={} elapsed={:.3}s estimated_speed={} B/s",
        url, resp_status, file_size, file_size, supports_range, elapsed, estimated
    );
    Ok(ProbeFileSize {
        file_size,
        supports_range,
        estimated_speed_bps: estimated.max(1024 * 1024),
    })
}

/// ★ 构建专用下载 client: 强制 HTTP/1.1 + 多独立 TCP 连接
///   原因: reqwest 默认支持 HTTP/2, 所有请求多路复用到一个 TCP 连接,
///   服务器对单连接限速时, 即使开 16 个 worker 也只有 1 个连接的速度.
///   IDM 风格: 用 HTTP/1.1 + 多个独立 TCP 连接突破单连接限速.
pub fn build_download_client(cfg: &DownloadConfig) -> Result<reqwest::Client, DownloadError> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(cfg.timeout_connect)
        .read_timeout(cfg.timeout_read)
        .timeout(cfg.timeout_request)
        .tcp_nodelay(true)
        // ★ 关键: 强制 HTTP/1.1, 每个请求用独立 TCP 连接
        .http1_only()
        // ★ 内存优化 (2026-09-28): 512 → 128。
        //   实际 worker 数 = 64, 每个 worker 同一时刻只发 1 个 chunk 请求,
        //   即最大并发连接 64; 旧值 512 保留了 8 倍冗余的空闲连接,
        //   每条空闲连接持有 hyper 读缓冲 + 内核 socket 缓冲 (数十~上百 KB),
        //   512 条 → 数十 MB 无谓占用。128 (= 2× worker) 已足够覆盖重试与慢源接管。
        //   同时把空闲回收 120s → 30s, 让下载结束后尽快释放连接内存。
        // ★ 与 MAX_CONNS 同步放宽 (2026-10-02): 原 128 会在并发 >128 时
        //   让 reqwest 排队建连, 实际并发被池上限卡住。
        .pool_max_idle_per_host(MAX_CONNS as usize)
        .pool_idle_timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10));

    // 代理设置
    if let Some(ref proxy_str) = cfg.proxy {
        if let Ok(p) = reqwest::Proxy::all(proxy_str) {
            builder = builder.proxy(p);
        }
    } else {
        // 系统代理
        if let Ok(http) = std::env::var("HTTP_PROXY").or_else(|_| std::env::var("http_proxy")) {
            if let Ok(p) = reqwest::Proxy::http(&http) {
                builder = builder.proxy(p);
            }
        }
        if let Ok(https) = std::env::var("HTTPS_PROXY").or_else(|_| std::env::var("https_proxy")) {
            if let Ok(p) = reqwest::Proxy::https(&https) {
                builder = builder.proxy(p);
            }
        }
    }

    builder.build().map_err(|e| DownloadError::Other(format!("build_download_client: {:#}", e)))
}

// ============================================================
// Worker 主循环
// ============================================================

/// 发起 HTTP 请求前先领取令牌, 把请求速率压在服务器限流阈值之下。
/// 见 `RequestBucket`: 实测该 CDN 约 140 请求/60 秒, 超限即 429。
/// 主请求与重试都要过这一关 —— 重试同样是请求, 正是旧实现里把限流窗口
/// 不断"续期"的元凶。
async fn acquire_request_slot(engine: &DownloadEngine) {
    while !engine.cancel_flag.load(Ordering::Relaxed) {
        match engine.req_bucket.try_take() {
            None => return,
            Some(wait_ms) => {
                tokio::time::sleep(Duration::from_millis(wait_ms.clamp(20, 1_000))).await;
            }
        }
    }
}

pub async fn worker_main(
    engine: Arc<DownloadEngine>,
    worker_id: u32,
) -> Result<(), DownloadError> {
    loop {
        // 0. ★ 429 限流全局退避: 任一 worker 收到 429 时设置 rate_limit_until,
        //    所有 worker 在此时间戳前必须等待, 避免集体轰炸服务器导致持续 429 死循环。
        //
        //    ⚠️ 关键约束 (2026-10-03): 这里**只等"发请求"**, 绝不能把"取块 / 回收卡死块"
        //    也一起挡住 —— 否则会形成尾部死锁: 静默期间所有 worker 都睡在这一行,
        //    没有任何人执行 acquire/回收, 于是块永远无人接管。
        //
        //    ★ 实测教训(两轮才修对):
        //      第一轮我只让静默期间**回收 ASSIGNED 的块**, 结果仍然卡死 1100 秒 ——
        //      因为 429 处理本身已经把块 `release_pending` 回**队列**了, 队列里有块、
        //      却没有任何 worker 去取 (全在睡觉)。`heal_orphan_pending` 也救不了:
        //      它要求"pending 队列为空", 而这时队列恰恰非空。
        //      所以静默期间必须检查的是"**队列里有没有块可取**", 而不只是"有没有卡死块"。
        //
        //    现在的策略: 每 500ms 醒来一次, 只做一件事 ——
        //    判断"待发请求数是否已低于静默期允许的速率"。若队列有块且服务器
        //    限流已明显缓解(或队列里全是尾部小块), 就提前结束静默去下载;
        //    否则继续等。这样既不会在惩罚窗口内猛发请求, 也不会让块白等。
        let backoff_until = engine.rate_limit_until.load(Ordering::Relaxed);
        let mut now = now_ms();
        if now < backoff_until {
            while now < backoff_until {
                let remain = (backoff_until - now).min(500);
                tokio::time::sleep(Duration::from_millis(remain)).await;
                if engine.cancel_flag.load(Ordering::Relaxed) {
                    return Err(DownloadError::Canceled);
                }
                // 静默期间维持看门狗: 回收卡死块
                engine.pool.reclaim_stuck_now();
                // ★ 队列里有块可领 → 立刻结束静默去下载。
                //   块回到队列说明它上一个持有者已经放弃 (多半就是被 429 打回),
                //   再让它在队列里躺满整个静默期就是白等 —— 这正是"99% 卡很久"的成因。
                //   尾部数据量很小, 用低并发把它拉完比干等划算得多。
                if !engine.pool.pending_is_empty() && engine.pool.is_tail() {
                    break;
                }
                now = now_ms();
            }
            // ★ 错峰 (2026-10-03): 全局静默是同一时刻到期的, 64 个 worker 会在
            //   同一瞬间一起醒来、一起发请求 —— 典型惊群。若服务器惩罚窗口刚好
            //   没完全结束, 整波会被全部拒绝, 于是再集体静默 59 秒, 永远出不来
            //   (日志实证: 54 次 429 挤在 66ms 内爆发, 界面显示几十 B/s 不动)。
            //   给每个 worker 一个与 id 挂钩的固定偏移, 把这一波摊到 ~800ms 内。
            let stagger = (worker_id as u64).wrapping_mul(37) % 800;
            if stagger > 0 {
                tokio::time::sleep(Duration::from_millis(stagger)).await;
            }
        }
        // 1. 检查 cancel
        if engine.cancel_flag.load(Ordering::Relaxed) {
            return Err(DownloadError::Canceled);
        }
        // 2. 检查 pause: 若状态为 Paused 等待 resume_notify
        if *engine.state_rx.borrow() == EngineState::Paused {
            // 释放当前持有的 chunk (worker 上下文内若有, 调用方负责 release_pending)
            tokio::select! {
                _ = engine.resume_notify.notified() => {
                    if engine.cancel_flag.load(Ordering::Relaxed) {
                        return Err(DownloadError::Canceled);
                    }
                    continue;
                }
                _ = engine.cancel_changed() => {
                    return Err(DownloadError::Canceled);
                }
            }
        }
        // 3. 获取 semaphore 许可 (动态连接数控制)
        let _permit = match engine.semaphore.acquire().await {
            Ok(p) => p,
            Err(_) => return Err(DownloadError::Other("semaphore closed".into())),
        };
        // 3a. ★ 请求限速 (2026-10-02): 领到令牌才继续。
        //   放在**闸门/取块之前**是有意的 —— 若先占住 chunk 再等令牌, 那些
        //   排队等令牌的 worker 会把块全部标成 ASSIGNED 却不下载, 触发
        //   "pending 为空 → 切分" 与 "零进展自愈" 的误判。先领令牌,
        //   则 active_conns 与"真正在发请求的 worker 数"一致。
        acquire_request_slot(&engine).await;
        if engine.cancel_flag.load(Ordering::Relaxed) {
            return Err(DownloadError::Canceled);
        }
        // 3b. ★ 429 自适应并发闸门 (核心修复): 实际并发 active_conns 硬性限制在
        //     throttled_ceiling 以内. 旧实现仅调 semaphore, 但下载中许可几乎全被 worker
        //     持有, 无法真正缩减 → 429 限流形同虚设. 这里用 CAS 闸门确保真生效.
        {
            // ★ 修复 (2026-10-02): 去掉这里的 `.max(MIN_THROTTLED_CONNS)`。
            //   那行会把 429 闸门压到的 2~4 硬抬回 8 —— 闸门说要限 4 条,
            //   实际仍放行 8 条, 限流保护在**闸门这一层**被彻底抵消 (实测日志
            //   `ceiling 4 → 4` 与 `active` 长期不符即源于此)。
            //   `throttled_ceiling` 本身已由 SmartScheduler 保证 ≥4, 无需再兜底。
            let ceiling = engine.throttled_ceiling.load(Ordering::Relaxed).max(1);
            // 限流窗口内调高卡死阈值: 被服务器限速的 chunk 不算"卡死", 避免误回收→重连→再 429
            let throttled = now_ms() < engine.rate_limit_until.load(Ordering::Relaxed);
            engine.pool.set_stuck_timeout_ms(if throttled {
                STUCK_PROGRESS_TIMEOUT_MS.saturating_mul(3)
            } else {
                STUCK_PROGRESS_TIMEOUT_MS
            });
            let mut spins = 0u32;
            loop {
                if engine.cancel_flag.load(Ordering::Relaxed) {
                    return Err(DownloadError::Canceled);
                }
                let cur = engine.active_conns.load(Ordering::Relaxed);
                if cur < ceiling
                    && engine
                        .active_conns
                        .compare_exchange(cur, cur + 1, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    break;
                }
                spins += 1;
                if spins % 20 == 0 {
                    // 长时间被闸门挡住, 检查是否已全部完成 (避免空转)
                    if engine.pool.all_completed() {
                        return Ok(());
                    }
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        }
        // 4. acquire chunk
        let chunk = match engine.pool.acquire(worker_id) {
            Some(c) => c,
            None => {
                // ★ IDM 式慢 chunk 接管: pending 为空时, 从最慢的 ASSIGNED chunk 偷取后半段
                //   使用动态阈值 (engine.dynamic_max_chunk) 自适应切分频率
                let dyn_threshold = engine.dynamic_max_chunk.load(Ordering::Relaxed).max(MIN_CHUNK_SIZE);
                if engine.pool.steal_from_slowest(dyn_threshold).is_some() {
                    engine.active_conns.fetch_sub(1, Ordering::Relaxed);
                    drop(_permit);
                    // 本轮没有真正发请求 → 归还令牌, 否则空转 worker 会抽干令牌桶
                    engine.req_bucket.give_back();
                    continue; // 重新循环 acquire 偷来的 chunk
                }
                engine.active_conns.fetch_sub(1, Ordering::Relaxed);
                drop(_permit);
                engine.req_bucket.give_back();
                if engine.pool.all_completed() {
                    return Ok(());
                }
                // ★ 无块可领时主动回收卡死块 (2026-10-03)。
                //
                //   这是尾部死锁的解药。实测: 剩余 3.1% (32MB) 却整整 1100 秒零字节,
                //   `active=0 chunks=124/128` —— 块卡在 429 静默里, 而**没有任何 worker
                //   在跑 acquire**, 于是 reclaim_stuck (挂在 acquire 上) 永远不执行,
                //   那几个块就成了永久孤儿。
                //
                //   progress_loop 虽然也调 reclaim_stuck_now, 但它有 `stall_ticks >= 10`
                //   的前置; 这里让**每个空闲 worker 也参与回收**, 使看门狗在"全员阻塞"
                //   时依然活着。回收是幂等的 (CAS + 状态检查), 多 worker 并发调用安全。
                if engine.pool.reclaim_stuck_now() > 0 {
                    continue; // 刚回收出块, 立刻再试一次 acquire
                }
                // ★ 极限优化 (2026-09-11): 50ms → 10ms, 更快重新获取 chunk
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
        };
        // 5. 下载 chunk
        let start = chunk.start + chunk.downloaded.load(Ordering::Relaxed);
        let end = chunk.end();
        if start > end {
            engine.pool.release_complete(&chunk);
            engine.active_conns.fetch_sub(1, Ordering::Relaxed);
            engine.req_bucket.give_back(); // 没发请求 → 归还令牌
            continue;
        }
        if chunk.first_byte_at.load(Ordering::Relaxed) == 0 {
            chunk.first_byte_at.store(now_ms(), Ordering::Relaxed);
        }
        let result = download_chunk_range(
            &engine.client, &engine.cfg.url, start, end,
            &engine.file, &chunk, &engine.cancel_flag, &engine.state_rx,
            &engine.extra_headers,
            &engine.throttle,
        ).await;
        engine.active_conns.fetch_sub(1, Ordering::Relaxed);
        drop(_permit);

        match result {
            Ok(_bytes_written) => {
                // ★ 修复 Bug: 不再 + bytes_written (download_chunk_range 内部已更新 chunk.downloaded)
                //   原代码: new_dl = chunk.downloaded + bytes_written → 双重计数, chunk 在半下时被误判完成
                //   现仅读 chunk.downloaded (内部循环每收到一字节就累加过)
                let new_dl = chunk.downloaded.load(Ordering::Relaxed);
                if new_dl >= chunk.size() {
                    engine.pool.release_complete(&chunk);
                    // ★ 下载成功: 重置连续 429 计数 (服务器已恢复正常)
                    engine.consecutive_429.store(0, Ordering::Relaxed);
                    // 6. 慢块接管: 交给 steal_from_slowest 处理。
                    //
                    // ★ 移除死路径 (2026-10-03): 这里原本调
                    //     `slowest_active(阈值)` + `split_half(&slow, 阈值)`
                    //   但两者状态机不匹配, 该路径**永远不生效**:
                    //     · slowest_active 只返回 CHUNK_ASSIGNED 的块;
                    //     · split_half 要求状态必须是 CHUNK_PENDING, 且要求 dl == 0。
                    //   传入的慢块既 ASSIGNED 又通常 dl > 0 → split_half 必返回 None。
                    //   而"慢块接管"这一功能已由 steal_from_slowest 完整覆盖
                    //   (worker 抢不到块时调用, 它正确接受 ASSIGNED 块并把后半段切给新 chunk,
                    //    同时把原块 end 截断到 mid, 不会重复下载)。
                    //   保留一段永不执行的代码只会让状态机更难推理, 故移除。
                    debug_assert!(
                        engine.pool.slowest_active(u64::MAX).map_or(true, |c| {
                            c.state.load(Ordering::Relaxed) == CHUNK_ASSIGNED
                        }),
                        "slowest_active 的契约是只返回 ASSIGNED 块"
                    );
                } else {
                    // 部分完成 (pause 中断), 释放回 pending
                    engine.pool.release_pending(&chunk);
                }
            }
            // ★ HTTP 429 Too Many Requests: 服务器限流, 必须全局退避
            //   原代码把 429 当普通 Network 错误重试 3 次 (200/400/600ms) 后释放回 pending,
            //   其他 worker 立即接手又 429 → 死循环 → 下载卡在 99%/100% 永远不完成.
            //   修复: 设置全局退避时间戳 (指数增长 2s→4s→8s→16s→30s), 让所有 worker 等待.
            Err(DownloadError::Network(ref msg)) if msg.contains("429") => {
                // ★ 2026-10-02: 退避时长交给 note_429 —— 它优先采纳服务器的
                //   Retry-After (实测该 CDN 给 60s), 没有该头时才回退到自家指数退避。
                //   旧实现只用自己的 16s, 永远落在惩罚窗口内 → 限流永不解除。
                let backoff_ms = engine.note_429(msg);
                engine_log!(
                    "[429] 全局静默 {}ms (连续第 {} 次 429), chunk {} 释放回 pending",
                    backoff_ms,
                    engine.consecutive_429.load(Ordering::Relaxed),
                    chunk.id
                );
                engine.pool.release_pending(&chunk);
                // ★ 缩减并发连接数 (forget 许可), 避免静默期结束后集体重连又 429
                engine.throttle_on_429().await;
                // 按**全局**截止时间等待 (可能已被其他 worker 推得更远)
                engine.sleep_until_quiet(backoff_ms, worker_id).await;
            }
            // ★ Timeout 和 Network 错误都重试 (error decoding response body / connection reset 等
            //   临时性网络错误不应直接导致下载失败, 应重试或释放回 pending 让其他 worker 接手)
            Err(DownloadError::Timeout) | Err(DownloadError::Network(_)) => {
                let err_msg = match &result {
                    Err(DownloadError::Network(m)) => m.clone(),
                    _ => String::new(),
                };
                let mut retried = 0u32;
                const MAX_RETRIES: u32 = 3;
                while retried < MAX_RETRIES {
                    if engine.cancel_flag.load(Ordering::Relaxed) {
                        engine.pool.release_pending(&chunk);
                        return Err(DownloadError::Canceled);
                    }
                    if *engine.state_rx.borrow() == EngineState::Paused {
                        engine.pool.release_pending(&chunk);
                        break;
                    }
                    let s = chunk.start + chunk.downloaded.load(Ordering::Relaxed);
                    if s > chunk.end() {
                        engine.pool.release_complete(&chunk);
                        break;
                    }
                    // 重试前短暂退避, 避免立即重连被服务器拒绝
                    tokio::time::sleep(Duration::from_millis(200 * (retried + 1) as u64)).await;
                    // ★ 重试也要过请求限速 (重试同样是请求)
                    acquire_request_slot(&engine).await;
                    let r = download_chunk_range(
                        &engine.client, &engine.cfg.url, s, chunk.end(),
                        &engine.file, &chunk, &engine.cancel_flag, &engine.state_rx,
                        &engine.extra_headers,
                        &engine.throttle,
                    ).await;
                    match r {
                        Ok(_b) => {
                            let nd = chunk.downloaded.load(Ordering::Relaxed);
                            if nd >= chunk.size() {
                                // ★ 日志增强 (2026-09-15): chunk 完成时记录 (简洁, 避免过多噪音)
                                engine_log!(
                                    "[chunk_ok] chunk={} start={} end={} size={} downloaded={}",
                                    chunk.id, chunk.start, chunk.end(), chunk.size(), nd
                                );
                                engine.pool.release_complete(&chunk);
                                engine.consecutive_429.store(0, Ordering::Relaxed);
                                break;
                            }
                        }
                        // ★ 重试时遇到 429: 触发全局静默 + 缩减并发, 释放 chunk 回 pending
                        Err(DownloadError::Network(ref m)) if m.contains("429") => {
                            // 同上面的 note_429 (优先采纳 Retry-After, 窗口单调延长)
                            let backoff_ms = engine.note_429(m);
                            engine.pool.release_pending(&chunk);
                            engine.throttle_on_429().await;
                            engine_log!(
                                "[429] 重试中限流, 全局静默 {}ms (连续第 {} 次)",
                                backoff_ms,
                                engine.consecutive_429.load(Ordering::Relaxed)
                            );
                            engine.sleep_until_quiet(backoff_ms, worker_id).await;
                            break;
                        }
                        Err(DownloadError::Timeout) | Err(DownloadError::Network(_)) => {
                            retried += 1;
                            continue;
                        }
                        Err(DownloadError::Canceled) => {
                            engine.pool.release_pending(&chunk);
                            return Err(DownloadError::Canceled);
                        }
                        Err(e) => {
                            // ★ 日志增强 (2026-09-15): chunk 直接失败时记录详细参数, 方便排查不可恢复错误
                            engine_log!(
                                "[chunk_fail] chunk={} stage=irrecoverable start={} end={} downloaded={} size={} retries={} recycle={} error={}",
                                chunk.id, chunk.start, chunk.end(),
                                chunk.downloaded.load(Ordering::Relaxed),
                                chunk.size(), MAX_RETRIES,
                                chunk.recycle_count.load(Ordering::Relaxed),
                                e
                            );
                            engine.pool.mark_failed(&chunk);
                            return Err(e);
                        }
                    }
                }
                // 重试完仍失败, 释放回 pending 让其他 worker 接手 (不直接失败)
                if chunk.state.load(Ordering::Relaxed) == CHUNK_ASSIGNED {
                    // ★ 修复 (2026-09-15): 释放回 pending 前先退避 2s, 避免立即被其他 worker 接手再次失败
                    //   原逻辑: 重试 3 次失败后直接 release_pending → chunk 立即被另一 worker 接手 → 服务器还没恢复就又被请求 → 再次失败
                    //   新逻辑: 退避 2s 给服务器恢复时间, 降低再次失败概率
                    // ★ 日志增强 (2026-09-15): 详细记录 chunk 失败参数, 方便排查"连接中"卡死
                    engine_log!(
                        "[chunk_fail] chunk={} stage=retry_exhausted start={} end={} downloaded={} size={} retries={} recycle={} error={}",
                        chunk.id, chunk.start, chunk.end(),
                        chunk.downloaded.load(Ordering::Relaxed),
                        chunk.size(), MAX_RETRIES,
                        chunk.recycle_count.load(Ordering::Relaxed),
                        err_msg
                    );
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    engine.pool.release_pending(&chunk);
                }
            }
            Err(DownloadError::Canceled) => {
                engine.pool.release_pending(&chunk);
                return Err(DownloadError::Canceled);
            }
            Err(e) => {
                engine.pool.mark_failed(&chunk);
                return Err(e);
            }
        }
    }
}

impl DownloadEngine {
    /// 监听 cancel_flag 变化 (轮询版, 简单可靠)
    fn cancel_changed(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + Sync + '_>> {
        let flag = self.cancel_flag.clone();
        Box::pin(async move {
            loop {
                if flag.load(Ordering::Relaxed) { return; }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    }

    /// ★ 429 限流时自适应降低并发上限 (AIMD 的乘性下降).
    ///   核心修复: 旧实现用 `semaphore.available_permits()` 计算可缩减量, 但下载进行中
    ///   几乎所有许可都被 worker 持有, available≈0 → 函数直接 return, 限流从未真正生效,
    ///   于是 64 个 worker 反复撞 429 → "回收卡死/429 风暴" 死循环, 速度崩到 <20MB/s.
    ///   现改为设置 `throttled_ceiling` (worker 实际并发硬上限), 并同步压缩 scheduler 目标值,
    ///   保证退避结束后不会再集体重连撞墙.
    /// ★ 把许可数对齐到目标并发 (2026-10-02)。
    ///
    /// 之前 SmartScheduler 只写 throttled_ceiling (闸门上限), 而真正决定"几个 worker
    /// 能同时下载"的是 semaphore 的许可数 —— 那个由旧的 DynamicScheduler 每 2 秒
    /// 才 ±1 地微调, 严重滞后。结果: 智能调度判定"带宽没跑满, 该加到 16 条",
    /// 但实际只有 8 条 worker 能拿到许可, 决策等于没生效。
    ///
    /// 这里把许可数直接对齐到 target:
    ///   · 需要更多 → add_permits(差额), 立刻生效
    ///   · 需要更少 → 尽力 acquire 差额并 forget (拿不到说明都被占用, 下次再试)
    /// 非阻塞, 不会卡住 progress_loop。
    pub fn sync_permits_to(&self, target: u32) {
        let target = target.max(1) as usize;

        // ★ 先偿还 429 退让欠下的许可 (2026-10-03)。
        //   这些是"想让并发降到 ceiling, 但当时许可被 worker 持有、没拿掉"的差额。
        //   必须先扣掉它们, 否则下面按账面 add_permits 会把没拿掉的那部分再补一遍,
        //   许可数就会只增不减 (semaphore 彻底失效)。
        {
            let mut s = self.scheduler.lock();
            if s.permit_debt > 0 {
                let debt = s.permit_debt as usize;
                if let Ok(permits) = self.semaphore.try_acquire_many(debt as u32) {
                    permits.forget();
                    s.permit_debt = 0;
                    engine_log!("[429] 补回收欠账许可: 拿走 {} 个", debt);
                }
                // 拿不到就留着, 下个 tick 继续还 (worker 释放后会成功的)
            }
        }

        let available = self.semaphore.available_permits();
        // 当前总许可用 scheduler.current_permits 作为权威记录, 而不是 available_permits。
        // 后者是"空闲"许可数, 下载中常常为 0 —— 旧代码就是拿它当总数比较,
        // 导致 target > 0 > current 恒成立, 许可无限膨胀。
        let current = {
            let s = self.scheduler.lock();
            s.current_permits as usize
        };
        if target == current {
            return;
        }
        if target > current {
            let delta = target - current;
            // ★ 有欠账时不要补许可 (2026-10-03): 欠账意味着"实际许可比账面多",
            //   此时 add_permits 会让实际值进一步超标。先把账还清再谈加。
            let debt = {
                let s = self.scheduler.lock();
                s.permit_debt
            };
            if debt > 0 {
                return;
            }
            self.semaphore.add_permits(delta);
            {
                let mut s = self.scheduler.lock();
                s.current_permits = target as u32;
            }
            engine_log!(
                "[smart] 许可对齐: {} -> {} (+{}, 空闲={})",
                current, target, delta, available
            );
        } else {
            let delta = current - target;
            match self.semaphore.try_acquire_many(delta as u32) {
                Ok(permits) => {
                    permits.forget();
                    {
                        let mut s = self.scheduler.lock();
                        s.current_permits = target as u32;
                    }
                    engine_log!("[smart] 许可对齐: {} -> {} (-{})", current, target, delta);
                }
                Err(_) => {
                    // 差额许可正被占用, 无法立即回收; 下次 tick 再试
                }
            }
        }
    }

    /// 记录一次 429, 设置全局静默窗口。返回本次应等待的毫秒数。
    ///
    /// ★ 2026-10-02 关键修复: 优先采纳服务器的 `Retry-After`。
    ///   实测该 CDN (Cloudflare) 限流时返回 `Retry-After: 60` —— 这是服务器明确
    ///   要求的最短静默期, 期间发任何请求都会把惩罚窗口续上。旧实现忽略它、
    ///   固定 16s 后重试, 于是每次重试都在给限流"续期", 永远出不来 (尾部 B/s)。
    ///
    ///   窗口用 `fetch_max` **单调延长**而不是 `store`:
    ///   多个 worker 同时撞 429 时, 后来者的短窗口会把先前的长窗口覆盖掉,
    ///   全局静默期被不断缩短 —— 这正是"退避设了却仍在猛发请求"的根因。
    fn note_429(&self, err_msg: &str) -> u64 {
        let last = self.last_429_at.swap(now_ms(), Ordering::Relaxed);
        // 距上次 429 超过 30s → 视为限流已缓解, 重新计数 (仅影响指数退避路径)
        if last != 0 && now_ms().saturating_sub(last) > 30_000 {
            self.consecutive_429.store(0, Ordering::Relaxed);
        }
        let count = self.consecutive_429.fetch_add(1, Ordering::Relaxed) + 1;
        let backoff_ms = match parse_retry_after(err_msg) {
            Some(secs) => secs.saturating_mul(1000).clamp(1_000, MAX_RETRY_AFTER_MS),
            // 服务器没给 Retry-After → 回退到自家指数退避 (1s/2s/4s/8s/16s)
            None => (1000u64 * (1u64 << count.min(4))).min(16_000),
        };
        // ★ 安全余量 (2026-10-03)。实测该 CDN 返回 `Retry-After: 59`, 而 worker
        //   恰好睡满 59 秒就醒来发请求 —— 卡在惩罚窗口边缘, 醒来即再次 429。
        //   日志实证: 54 次 429 挤在 66 毫秒内爆发, 之后再静默 59 秒, 循环不止,
        //   界面上就是"速度几十 B/s 一动不动"。多等 2 秒越过窗口边界。
        const RETRY_AFTER_MARGIN_MS: u64 = 2_000;
        self.rate_limit_until
            .fetch_max(now_ms() + backoff_ms + RETRY_AFTER_MARGIN_MS, Ordering::Relaxed);
        backoff_ms
    }

    /// 睡到全局静默窗口结束 —— 而不是只睡本次 backoff。
    /// 别的 worker 可能已经把窗口推得更远, 必须按最新截止时间等, 否则醒来即撞墙。
    ///
    /// ★ 醒来后还要**错峰** (2026-10-03): 全局静默是同一时刻到期的, 64 个 worker
    ///   会在同一瞬间一起醒来、一起发请求 —— 典型的惊群。若服务器惩罚窗口刚好
    ///   没完全结束, 这一整波会被全部拒绝, 于是又集体静默 59 秒, 永远出不来。
    ///   给每个 worker 一个与其 id 挂钩的固定偏移, 把这一波摊开。
    async fn sleep_until_quiet(&self, backoff_ms: u64, worker_id: u32) {
        let until = self.rate_limit_until.load(Ordering::Relaxed);
        let wait = until.saturating_sub(now_ms()).max(backoff_ms);
        tokio::time::sleep(Duration::from_millis(wait)).await;
        // 错峰: 64 个 worker 摊到 ~800ms 内 (约 12.5ms 一个), 不再是同一瞬间齐发
        let stagger = (worker_id as u64).wrapping_mul(37) % 800;
        if stagger > 0 {
            tokio::time::sleep(Duration::from_millis(stagger)).await;
        }
    }

    pub async fn throttle_on_429(&self) {
        let total = self.total_429.fetch_add(1, Ordering::Relaxed) + 1;

        // ★ 智能调度接管 (2026-10-02): 由 SmartScheduler 决定退让幅度。
        //   它做乘性减半 + 进入 20s 冷却 + 标记"服务器怕并发"(长期压低天花板),
        //   比原来硬编码的"第1次-25% / 之后减半"更有依据, 且能避免"退让后又冲高"的抖振。
        //   ceiling 仍然写回 throttled_ceiling, 因为 worker 的并发闸门读的是它。
        let new_ceiling = {
            let mut sm = self.smart.lock();
            sm.on_429();
            // ★ 用 sm.ceiling 而不是 sm.conns.max(MIN_THROTTLED_CONNS) (2026-10-02)。
            //   后者会把"乘性减半"的结果又抬回 8, 而调度 tick 里写的是 sm.ceiling
            //   (= conns.max(4), 可能只有 4) —— 两个写入方下限不同, 同一个
            //   throttled_ceiling 在 4 和 8 之间来回跳, 日志里就表现为
            //   "ceiling 4 → 4" 与 "许可对齐 4 → 2" 长期自相矛盾。
            //   统一取 sm.ceiling, 与 tick 完全一致。
            sm.ceiling
        };
        let cur_ceiling = self.throttled_ceiling.load(Ordering::Relaxed);
        let ceiling = new_ceiling.min(cur_ceiling).max(2);
        self.throttled_ceiling.store(ceiling, Ordering::Relaxed);

        // ★ 真正回收许可 (2026-10-02 关键修复)。
        //
        //   原来这里只改了 scheduler.current_permits (一个记账字段), 而**没有动 semaphore**。
        //   于是 worker 闸门读到 ceiling=4, 实际却仍有 8 条连接在跑 —— 服务器继续 429。
        //   实测日志把这个漏洞暴露得很清楚:
        //     [429] 智能退让: ceiling 4 → 4 (累计 429 第 228 次)
        //     tick=500 96.1% speed=0 active=2 chunks=184/192
        //   228 次 429 却退不下来, 就是因为许可从未真正减少。
        //
        //   现在: 把 semaphore 许可数压到 ceiling —— 多余的许可用 acquire+forget 真正移除。
        //   拿不到的说明正被占用, 那些 worker 完成当前块后自然退出, 下轮再补齐差额。
        {
            let mut s = self.scheduler.lock();
            let cur = s.current_permits;
            if cur > ceiling {
                let delta = cur - ceiling;
                match self.semaphore.try_acquire_many(delta) {
                    Ok(permits) => {
                        permits.forget();   // 永久移除, 真正降并发
                        s.current_permits = ceiling;
                        s.permit_debt = 0;  // 已压到位, 清掉之前的欠账
                        engine_log!(
                            "[429] 真正回收许可: {} -> {} (拿走 {} 个)",
                            cur, ceiling, delta
                        );
                    }
                    Err(_) => {
                        // ★ 许可正被占用, 本次拿不到 (2026-10-03 修正记账口径)。
                        //
                        //   原实现直接把 current_permits 改成 ceiling —— 但许可**并没有
                        //   真的被拿走**。之后天花板回升时 sync_permits_to 会按这个被低估的
                        //   账面值 add_permits, 把"之前没拿掉的"又补一遍, 于是许可数只增不减:
                        //     实际8/账面8 → 429 降到 4(失败, 账面=4, 实际仍 8)
                        //     → 回升到 8 时 add_permits(4) → 实际 12/账面 8 → 每轮净增。
                        //   semaphore 会彻底失去限流作用。
                        //
                        //   现在: 账面仍记为 ceiling (反映"目标是这么多"), 同时把差额记入
                        //   permit_debt, 由 sync_permits_to 在后续 tick 真正回收 ——
                        //   记账与实际才对得上。
                        s.permit_debt = s.permit_debt.saturating_add(delta);
                        s.current_permits = ceiling;
                    }
                }
            } else {
                s.current_permits = ceiling;
            }
        }
        engine_log!(
            "[429] 智能退让: ceiling {} → {} (累计 429 第 {} 次, 交 SmartScheduler)",
            cur_ceiling, ceiling, total
        );
    }
}

// ============================================================
// 下载单个 chunk 的字节范围 (HTTP Range)
// ============================================================

pub async fn download_chunk_range(
    client: &reqwest::Client,
    url: &str,
    start: u64,
    end: u64,
    file: &Arc<std::fs::File>,
    chunk: &Arc<Chunk>,
    cancel_flag: &Arc<AtomicBool>,
    state_rx: &watch::Receiver<EngineState>,
    extra_headers: &[(String, String)],
    throttle: &SpeedCap,
) -> Result<u64, DownloadError> {
    let range = format!("bytes={}-{}", start, end);
    let mut req = client.get(url).header("Range", &range);
    for (k, v) in extra_headers {
        req = req.header(k, v);
    }
    let resp = req.send().await.map_err(|e| {
        let msg = format!("{:#}", e);
        if msg.contains("timeout") || msg.contains("timed out") {
            DownloadError::Timeout
        } else {
            DownloadError::Network(msg)
        }
    })?;
    // ★ 修复 Bug: 检测服务器是否真的返回 206 Partial Content
    let resp_status = resp.status();
    if start > 0 && resp_status == reqwest::StatusCode::OK {
        return Err(DownloadError::Network(format!(
            "服务器不支持 Range 请求 (发 Range 但返回 200), 无法多连接下载: {}",
            url
        )));
    }
    if !resp_status.is_success() {
        // ★ 429 必须带上服务器的 Retry-After (2026-10-02)。
        //   实测 cdn1.trashbytes.to 限流时返回 `Retry-After: 60`, 而旧代码把它丢掉、
        //   只用自己的 16s 退避 → 重试仍落在惩罚窗口内 → 再 429 → 窗口被续期,
        //   限流永不解除, 尾部速度永久掉到 B/s。这里把秒数附在错误串上,
        //   由 worker 侧 note_429 解析并作为全局静默期。
        let mut msg = format!("HTTP {}", resp_status);
        if resp_status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // ★ 诊断 (2026-10-02): 把 429 的**全部响应头**打进日志。
            //   我们一直只能看到"429 + Retry-After: 60", 却不知道 CDN 究竟按什么
            //   判定的 (并发数? 新连接数? 请求速率? 还是别的防护规则)。
            //   记录 Range 与全部头, 才能把猜测换成证据。
            let mut hdrs = String::new();
            for (k, v) in resp.headers().iter() {
                hdrs.push_str(&format!(" {}={}", k.as_str(), v.to_str().unwrap_or("?")));
            }
            engine_log!("[429_HDR] range=bytes={}-{} headers:{}", start, end, hdrs);
            if let Some(secs) = retry_after_secs(resp.headers()) {
                msg.push_str(&format!("|retry_after={}", secs));
            }
        }
        return Err(DownloadError::Network(msg));
    }
    let mut stream = resp.bytes_stream();
    use futures::StreamExt;
    let mut bytes_written = 0u64;
    let mut last_pause_check = Instant::now();
    loop {
        // 检查 cancel
        if cancel_flag.load(Ordering::Relaxed) {
            return Err(DownloadError::Canceled);
        }
        // ★ 慢块接管 (2026-10-01): 每轮重读 chunk 的 (可能已被截断的) 结束偏移.
        //   steal_from_slowest 会把原 chunk 的 end 截断到 mid; 此处立即感知,
        //   到达截断点即停止, 避免把已交给新 chunk 的尾部再下载一遍 (2x 浪费).
        //   只允许"往小截断" (min(end)), 绝不放大, 防止越界写入.
        let cur_end = chunk.end().min(end);
        let allowed = cur_end.saturating_sub(start) + 1;
        if bytes_written >= allowed {
            break;
        }
        // 检查 pause (每 16ms 检查一次, 不阻塞流式读取)
        if last_pause_check.elapsed() >= Duration::from_millis(16) {
            last_pause_check = Instant::now();
            if *state_rx.borrow() == EngineState::Paused {
                // 暂停时保留已下载字节, 退出 (OS write cache 会异步落盘, 无需 sync_data)
                return Ok(bytes_written);
            }
        }
        match tokio::time::timeout(CHUNK_READ_TIMEOUT, stream.next()).await {
            Ok(Some(Ok(bytes))) => {
                let len = bytes.len();
                if len == 0 { break; }
                // ★ 授权限速 (免费版 80%): 按**字节**计费, 而不是"停读一段时间"。
                //   停读会被 TCP 接收缓冲区吃掉 (实测只差 1.7%), 见 SpeedCap 文档。
                // 不限速时 consume 只做一次原子读就返回 0, 几乎零开销
                let wait = throttle.consume(len as u64);
                if wait > 0 {
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                }
                // ★ 尾部截断: 只写入 allowed 以内的字节, 超出部分丢弃
                let cap = (allowed - bytes_written) as usize;
                let take = len.min(cap);
                // offset = start + bytes_written (不读 chunk.downloaded, 避免双重计数)
                let mut offset = start + bytes_written;
                // ★ 性能优化: 直接用 Windows FileExt::seek_write 写入, 不 spawn_blocking
                //   seek_write 不修改文件指针, 32 worker 可同时写不同偏移量
                //   seek_write 内部是 WriteFile + OVERLAPPED offset, OS write cache 缓冲, 微秒级返回
                //   之前用 spawn_blocking + to_vec() 每个 256KB 都要: 线程池调度 + 内存拷贝 + 上下文切换,
                //   32 worker 并发下产生大量任务切换开销, 是速度慢的主因.
                let mut remaining = &bytes[..take];
                while !remaining.is_empty() {
                    match file.seek_write(remaining, offset) {
                        Ok(0) => {
                            return Err(DownloadError::Io("seek_write 0 bytes".into()));
                        }
                        Ok(written) => {
                            bytes_written += written as u64;
                            offset += written as u64;
                            remaining = &remaining[written..];
                        }
                        Err(e) => {
                            return Err(DownloadError::Io(e.to_string()));
                        }
                    }
                }
                // 实时更新 chunk.downloaded (用于进度采样) — 唯一累加点
                //
                // ★ 改成原子累加 (2026-10-03): 原写法 `load()` + `store(cur + take)`
                //   在**同一块被两个 worker 并发写**时会互相覆盖 (经典丢失更新):
                //   回收看门狗 (尾部阈值 5 秒) 可能在旧持有者还阻塞在 15 秒读超时时
                //   就把块重新入队, 新 worker 立刻接手 → 两个写者各自 load-store。
                //   后果是 downloaded 少计 → 进度上不去, 收尾完整性校验
                //   (actual_downloaded < total) 误判失败, 而文件其实已经写全。
                //   fetch_add 无锁且不会丢更新。
                chunk.downloaded.fetch_add(take as u64, Ordering::Relaxed);
                // ★ 进度看门狗 (2026-09-28): 刷新最后进度时间, 供 acquire 判定卡死
                chunk.last_progress_at.store(now_ms(), Ordering::Relaxed);
            }
            Ok(Some(Err(e))) => {
                let msg = format!("{:#}", e);
                if msg.contains("timeout") || msg.contains("timed out") {
                    return Err(DownloadError::Timeout);
                }
                return Err(DownloadError::Network(msg));
            }
            Ok(None) => break,
            Err(_) => return Err(DownloadError::Timeout),
        }
    }
    // 注: 不在每个 chunk 结束时 sync_data, OS write cache 会异步落盘.
    // 仅在整个下载完成时 (download_file 末尾) 统一 sync_all.
    Ok(bytes_written)
}

// ============================================================
// 进度采样循环 (供 downloader.rs spawn)
// ============================================================

pub async fn progress_loop(
    engine: Arc<DownloadEngine>,
    downloaded: Arc<AtomicU64>,
    total: u64,
    callback: Arc<dyn Fn(ProgressInfo) + Send + Sync>,
    cancel_flag: Arc<AtomicBool>,
) {
    let mut last_sample = Instant::now();
    let mut last_downloaded = 0u64;
    let mut speed_smoother = SpeedSmoother::new();
    // ★ 修复 (2026-10-03): 平滑器的起点必须等于引擎**当前**已下载量。
    //   SpeedSmoother::new() 把 last_downloaded 初始化为 0, 而断点续传时
    //   downloaded 会在启动瞬间从 0 跳到"已恢复字节"(可达数 GB) —— 第一帧
    //   算出 5GB / 0.1s ≈ 50GB/s, 界面显示"几千 MB 的速度"。
    speed_smoother.last_downloaded = engine.pool.total_downloaded();
    let mut tick_count: u64 = 0;
    // ★ 自愈看门狗 (2026-10-02): 连续零进展的 tick 计数 + 上次自愈时的已下载字节
    let mut stall_ticks: u64 = 0;
    let mut heal_last_bytes: u64 = 0;
    let mut heal_count: u32 = 0;

    loop {
        if cancel_flag.load(Ordering::Relaxed) {
            break;
        }
        // 间隔 100ms
        tokio::time::sleep(SPEED_SAMPLE_INTERVAL).await;
        if cancel_flag.load(Ordering::Relaxed) { break; }

        let current = engine.pool.total_downloaded();
        downloaded.store(current, Ordering::Relaxed);
        let total_now = total.max(current);
        let prev_ema = speed_smoother.ema;
        let prev_last = speed_smoother.last_downloaded;
        let speed = speed_smoother.tick(current, total_now);

        // ★ 自愈看门狗 (2026-10-02): 解决"一直连接中 / 零进展"死锁。
        //   用户实测日志: tick=500..9500 downloaded 恒定, speed=0, active=0, chunks=0/64
        //   —— 15 分钟完全空转。根因是块停在 PENDING 却不在 pending 队列里 (acquire 取不到)。
        //   这里: 连续 30 tick (≈3 秒) 零字节增长 → 调用 heal_orphan_pending 重新入队。
        //   只在 active_conns==0 时生效 (heal 内部判断), 不会干扰正常下载。
        tick_count += 1;
        if current > heal_last_bytes {
            heal_last_bytes = current;
            stall_ticks = 0;
        } else {
            stall_ticks += 1;
            // ★ 修正 (2026-10-02): 零进展 10 tick (≈1 秒) 即尝试自愈, 不再等 30 tick。
            //   实测尾部失败只差 326KB, 却因等待过久而直接判失败。
            //   自愈是幂等的 (pending 非空时直接返回 0), 频繁调用没有副作用。
            if stall_ticks >= 10 && current < total {
                let active = engine.active_conns.load(Ordering::Relaxed);
                let healed = engine.pool.heal_orphan_pending(active);
                if healed > 0 {
                    heal_count += 1;
                    engine_log!(
                        "[heal] 零进展 {} tick (active={}, downloaded={}/{}), 重新入队 {} 个孤儿块 (第 {} 次自愈)",
                        stall_ticks, active, current, total, healed, heal_count
                    );
                }
                // ★ 同时回收卡死块 (2026-10-03)。必须在这里兜底:
                //   reclaim_stuck 原本只在 acquire() 里跑, 而尾部并发被 429 压到
                //   2~4 条 → 其余 60 个 worker 全阻塞在信号量上, 走不到 acquire
                //   → 看门狗不执行 → 卡死的块要等 42~188 秒才回收 (实测 188384ms),
                //   这就是"99% 卡很久"的直接原因。progress_loop 每 100ms 必跑。
                let reclaimed = engine.pool.reclaim_stuck_now();
                if reclaimed > 0 {
                    engine_log!(
                        "[heal] 零进展 {} tick 期间回收 {} 个卡死块 (active={}, downloaded={}/{})",
                        stall_ticks, reclaimed, active, current, total
                    );
                }
                stall_ticks = 0;
            }
        }
        if tick_count <= 10 || tick_count % 500 == 0 {
            let chunks_total = engine.pool.chunks_count();
            let chunks_done = engine.pool.completed_count();
            engine_log!(
                "[progress_loop] tick={} downloaded={}/{} ({:.1}%) speed={} B/s active={} chunks={}/{}",
                tick_count, current, total,
                if total > 0 { current as f64 / total as f64 * 100.0 } else { 0.0 },
                speed,
                engine.active_conns.load(Ordering::Relaxed),
                chunks_done, chunks_total
            );
        }

        let prog = if total > 0 {
            (current as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
        } else { 0.0 };
        let active = engine.active_conns.load(Ordering::Relaxed);
        // ★ ETA (2026-10-02): 原实现有两个问题 ——
        //   1) speed 归零时直接给 None, 前端 ETA 会突然变成 "--:--" 或空白
        //      (收尾阶段/限流退避时速度常为 0, 用户看到"预估时间不对");
        //   2) 用瞬时速度算, 数值剧烈跳动。
        //   现在: 用本次会话的平均速度做下限兜底 (瞬时为 0 或异常偏小时仍能给出
        //   一个稳定的估计), 且剩余量很小时不显示 (避免 "0 秒" 之类的噪声)。
        let eta = {
            let remaining = total.saturating_sub(current);
            if remaining == 0 {
                Some(0u64)
            } else {
                // 平均速度 (本次会话) 作为稳定基准
                let avg = {
                    let secs = engine.start_instant.elapsed().as_secs_f64();
                    if secs > 0.5 { (current as f64 / secs) as u64 } else { 0 }
                };
                // 优先用瞬时速度; 瞬时为 0 或明显低于平均时用平均 (限流退避期间更准)
                let basis = if speed > 0 { speed } else { avg };
                let basis = if basis == 0 { avg } else { basis };
                if basis > 0 {
                    Some(remaining / basis.max(1))
                } else {
                    None
                }
            }
        };

        // ★ 关键: 状态从 engine.state_rx 读, 不硬编码 "running"
        let state = {
            let st = *engine.state_rx.borrow();
            if prog >= 100.0 - f64::EPSILON {
                EngineState::Completed.as_str()
            } else {
                st.as_str()
            }
        };

        callback(ProgressInfo {
            task: engine.task_id.clone(),
            progress: prog,
            downloaded: current,
            total,
            speed_bps: speed,
            eta_sec: eta,
            active_conns: active,
            slow_bases: 0,
            state: state.to_string(),
        });


        // ★ 并发决策权统一 (2026-10-02): throttled_ceiling 的**唯一写入者**是
        //   下面的 SmartScheduler。此前这里另有一段独立的 "ceiling 回升" 逻辑
        //   与之抢写同一个原子量, 造成两个后果:
        //     · 刚被 429 压低 → 这里又抬回 MAX_CONNS → 再撞 429 (死循环)
        //     · 两处周期不同 (10s vs 1.5s) → 并发数来回抖, 速度不稳
        //   SmartScheduler 内部已含 AIMD 式"冷却后缓慢恢复 + 多次 429 后长期压低",
        //   尾部加速也由它的档位机制覆盖, 无需再开特例。
        //
        // ★ 智能调度: 让 SmartScheduler 做宏观决策 ——
        //   档位判定 / 是否有块可领 / 429 冷却 / 加性增或切细分块。
        //   它的 ceiling 会写进 throttled_ceiling, 影响下面的许可调整与 worker 闸门。
        {
            let pending_empty = engine.pool.pending_is_empty();
            let active = engine.active_conns.load(Ordering::Relaxed);
            let old_conns;
            let decision = {
                let mut sm = engine.smart.lock();
                // 冷却结束后缓慢恢复天花板 (避免一次退让后永远低位)
                sm.relax_ceiling();
                old_conns = sm.conns;
                let d = sm.tick(speed, active, pending_empty);
                // 把智能调度的并发目标同步到闸门, 让 worker 真正受它约束
                engine.throttled_ceiling.store(sm.ceiling, Ordering::Relaxed);
                // ★ 关键补全 (2026-10-02): 把 sm.conns 真正落到 semaphore 上。
                //   此前只写 ceiling (闸门上限), 而许可数仍由 DynamicScheduler 慢慢爬,
                //   于是 SmartScheduler 算出的目标并发**根本没有生效** ——
                //   它说要加到 16 条, 实际还是 8 条 worker 能拿到许可。
                //   这里直接把许可数对齐到 sm.conns: 需要更多就 add_permits,
                //   需要更少就 acquire+forget (拿不到就下次再试, 不阻塞)。
                //   ★ 并发目标还不得超过实际 worker 数 (2026-10-02): 实测 1GB 任务里
                //   SmartScheduler 一路 AIMD 爬到 conns=232, 而 worker_count 只有 64 ——
                //   多出来的许可根本没有 worker 去用, 纯属空转, 还会让日志里的
                //   "目标并发"严重失真。这里夹到 worker 数, 让目标值反映真实能力。
                engine.sync_permits_to(sm.conns.min(engine.worker_count));
                d
            };
            // 决策: 切细分块 → 下调动态分块阈值 (阈值越小, 切得越勤)
            if let Some(cs) = decision.chunk_size {
                let cur = engine.dynamic_max_chunk.load(Ordering::Relaxed);
                let next = cs.min(cur).max(MIN_CHUNK_SIZE);
                if next != cur {
                    engine.dynamic_max_chunk.store(next, Ordering::Relaxed);
                    engine_log!(
                        "[smart] {}: 分块阈值 {} → {} (speed={}KB/s active={} pending_empty={})",
                        decision.reason, cur, next, speed / 1024, active, pending_empty
                    );
                    // ★ 同步放宽块数上限 (2026-10-02, 用户要求"变慢就加大分块块数"):
                    //   阈值下调意味着要切出更多更小的块, 若上限不跟着放宽,
                    //   can_grow() 会挡住切分 —— 阈值改了却切不动, 等于没改。
                    //   按"每个 worker 至少 8 个候选块"计算新上限, 保证有空闲块可抢。
                    // ★ 但同样受请求预算封顶 (2026-10-02): 块数即请求数, 无上限地
                    //   切分等于无上限地发请求, 必然撞上服务器的请求数限流。
                    let want = (active.max(4) as usize)
                        .saturating_mul(8)
                        .max(64)
                        .min(REQUEST_BUDGET as usize);
                    engine.pool.set_max_chunks(want);
                }
            }
            if let Some(nc) = decision.conns {
                // ★ 日志修正 (2026-10-02): 原来把 `active` (实际活跃连接数) 当旧值打印,
                //   而真正的旧值是 SmartScheduler 自己的 conns。两者不等时日志会自相矛盾
                //   (实测打出 "加并发 +2: 目标并发 8 → 6" —— "加"却变小)。
                //   现在在决策前后各读一次 sm.conns, 打印真实变化。
                engine_log!(
                    "[smart] {}: 目标并发 {} → {} ({})",
                    decision.reason, old_conns, nc,
                    { let sm = engine.smart.lock(); sm.describe() }
                );
            }
        }

        // ★ 许可决策权已统一给 SmartScheduler (2026-10-02)。
        //
        //   原来这里紧跟着 SmartScheduler 又跑一次 DynamicScheduler.tick() 并改
        //   同一份许可数 —— 两者依据不同 (旧: 绝对速度 vs 旧上限; 新: 利用率+档位+429),
        //   于是每轮互相覆盖。实测日志把矛盾直接打了出来:
        //     [smart] 带宽未跑满, 加并发 +2: 目标并发 8 → 6 (tier=Medium conns=6 ...)
        //   "加并发"却把 8 改成了 6 —— 因为 SmartScheduler 说 6, 旧调度器立刻又调回 8,
        //   下一轮 SmartScheduler 看到 8 又"加"到 ... 如此反复, 并发永远上不去。
        //
        //   现在: SmartScheduler 是许可数的**唯一决策者** (经 sync_permits_to 落地),
        //   旧 DynamicScheduler 不再调整许可, 仅保留其速度统计职责。
        // ★ 许可决策权已统一给 SmartScheduler (2026-10-02)。
        //
        //   这里原本紧跟 SmartScheduler 再跑一次 DynamicScheduler.tick() 并改同一份
        //   许可数。两者依据不同 (旧: 绝对速度 + 旧上限; 新: 利用率 + 档位 + 429 冷却),
        //   于是每轮互相覆盖。实测日志把矛盾直接打了出来:
        //     [smart] 带宽未跑满, 加并发 +2: 目标并发 8 → 6 (tier=Medium conns=6 ...)
        //   —— "加并发"却把 8 改成了 6: SmartScheduler 说 6, 旧调度器立刻调回 8,
        //   下一轮 SmartScheduler 看到 8 又"加"到 6 …… 反复拉锯, 并发永远上不去,
        //   这正是"一直连接中/速度上不去"的直接原因之一。
        //
        //   现在 SmartScheduler 是许可数的唯一决策者 (经 sync_permits_to 落地)。

        // ★ 阈值决策权已移交 SmartScheduler (2026-10-02)。
        //
        //   这里原来用 dynamic_max_chunk_for_speed(ema) 按**绝对速度**调阈值,
        //   而 SmartScheduler 按**相对利用率**调同一个 dynamic_max_chunk ——
        //   两者互相覆盖, 实测日志里 8MB↔4MB 来回横跳 23/24 次:
        //     [smart] 速度偏低, 切细分块: 8388608 → 4194304 (speed=8001KB/s)
        //     [progress_loop] 动态分块阈值调整: 4194304 → 8388608 (ema_speed=8193332)
        //   结果是阈值永远在抖, 块大小跟着抖, 下载速度始终上不去。
        //
        //   现在 dynamic_max_chunk 的**唯一写入者**是 SmartScheduler (它同时考虑
        //   带宽档位、pending 是否为空、429 冷却), 这里不再插手。

        // ★ 断点续传: 每 50 tick (5秒) 保存进度到 .swiftfetch-resume 文件
        //   仅在下载有进展时保存, 避免空写; 保存失败不影响下载流程
        if tick_count % 50 == 0 && engine.cfg.resume_enabled {
            if let Some(output) = engine.cfg.output.as_ref() {
                engine.pool.save_resume(output);
            }
        }

        if prog >= 100.0 - f64::EPSILON { break; }
        let _ = last_sample;
        let _ = last_downloaded;
    }
}

// ============================================================
// SpeedSmoother (轻量, 100ms 采样)
// ============================================================

pub struct SpeedSmoother {
    last_tick: Instant,
    pub last_downloaded: u64,
    pub ema: u64,
}

impl SpeedSmoother {
    pub fn new() -> Self {
        Self {
            last_tick: Instant::now(),
            last_downloaded: 0,
            ema: 0,
        }
    }
    pub fn tick(&mut self, current: u64, _total: u64) -> u64 {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_tick).as_secs_f64().max(0.001);
        let delta = current.saturating_sub(self.last_downloaded);
        let instant_speed = (delta as f64 / elapsed) as u64;
        // ★ 修复 Bug: delta=0 时直接归零 EMA, 避免衰减残留造成 "虚假高速"
        //   原代码: ema = 0.7*ema + 0.3*0, 慢衰减, dl 不变时仍报 587→411→287 MB/s
        //   现策略: 无新字节立即归零, 有新字节才用 EMA 平滑
        if delta == 0 {
            self.ema = 0;
        } else if self.ema == 0 {
            self.ema = instant_speed;
        } else {
            self.ema = ((1.0 - 0.3) * self.ema as f64 + 0.3 * instant_speed as f64) as u64;
        }
        self.last_tick = now;
        self.last_downloaded = current;
        self.ema
    }
}

// ============================================================
// 公开 API: download_file - 供 downloader.rs 调用
// ============================================================

/// 启动新引擎: spawn N 个 worker + 1 个 progress loop, 等待全部完成
pub async fn download_file(
    engine: Arc<DownloadEngine>,
    progress_callback: Arc<dyn Fn(ProgressInfo) + Send + Sync>,
) -> Result<(), DownloadError> {
    // ★ 诊断: 引擎启动时的初始状态
    engine_log!(
        "[download_file] ENTRY task_id={} file_size={} initial_total_downloaded={} chunks={} supports_range={} worker_count={}",
        engine.task_id, engine.pool.file_size, engine.pool.total_downloaded(),
        engine.pool.chunks_count(), engine.supports_range, engine.worker_count
    );
    // 打开文件
    engine.open_file().await?;
    // ★ 断点续传: 加载之前保存的进度 (在 workers 启动前, 避免重复下载)
    if engine.cfg.resume_enabled {
        if let Some(output) = engine.cfg.output.as_ref() {
            // ★ 安全检查 (2026-10-03): 只有"输出文件本来就存在且非空"时才信任续传记录。
            //   否则会出现这条链路: 用户删掉坏掉的压缩包 (但 .swiftfetch-resume 还在)
            //   → 下次下载 create(true) 建出稀疏空文件 → 续传把整份文件标记为已完成
            //   → 完整性校验也通过 (区间覆盖全文件) → 得到一个全零的"完成"文件。
            //   实测症状: 5GB 的 rar 全是零, 7z 报 "Cannot open the file as archive";
            //   关掉续传重新下载同一个文件则完全正常 (已实测验证)。
            if engine.output_preexisting_bytes == 0 {
                ChunkPool::remove_resume(output);
                engine_log!(
                    "[download_file] 输出文件不存在或为空, 丢弃陈旧的续传记录 (避免生成全零文件): {}",
                    output.display()
                );
            } else {
                let resumed = engine.pool.load_resume(output);
                if resumed > 0 {
                    engine.downloaded.store(resumed, Ordering::Relaxed);
                    engine_log!(
                        "[download_file] 断点续传: 恢复 {} 字节 (file_size={} chunks_completed={}/{})",
                        resumed, engine.pool.file_size,
                        engine.pool.completed_count(), engine.pool.chunks_count()
                    );
                }
            }
        }
    }
    // 标记 Running
    let _ = engine.state_tx.send(EngineState::Running);
    let total = engine.pool.file_size;
    let downloaded = engine.downloaded.clone();
    let cancel_flag = engine.cancel_flag.clone();

    // spawn progress loop
    let prog_engine = engine.clone();
    let prog_cb = progress_callback.clone();
    let prog_cancel = cancel_flag.clone();
    let prog_handle = tokio::spawn(async move {
        progress_loop(prog_engine, downloaded, total, prog_cb, prog_cancel).await;
    });

    // spawn workers
    let mut handles = Vec::new();
    for wid in 0..engine.worker_count {
        let e = engine.clone();
        handles.push(tokio::spawn(worker_main(e, wid)));
    }

    // 等待全部 worker 完成
    let mut final_state = EngineState::Completed;
    for h in handles {
        match h.await {
            Ok(Ok(())) => {}
            Ok(Err(DownloadError::Canceled)) => {
                final_state = EngineState::Canceled;
                let _ = engine.state_tx.send(EngineState::Canceled);
                break;
            }
            Ok(Err(e)) => {
                final_state = EngineState::Failed;
                let _ = engine.state_tx.send(EngineState::Failed);
                engine_log!("[dynamic_engine] worker error: {}", e);
                break;
            }
            Err(join_e) => {
                final_state = EngineState::Failed;
                let _ = engine.state_tx.send(EngineState::Failed);
                engine_log!("[dynamic_engine] worker panic: {}", join_e);
                break;
            }
        }
    }
    // 通知 progress loop 退出
    cancel_flag.store(true, Ordering::Relaxed);
    let _ = prog_handle.await;

    // ★ 修复 Bug: 完成完整性校验, 防止 "4g 下完 2.8g" 类问题
    //   如果所有 worker 都返回 Ok 但实际下载字节 < file_size, 说明有 chunk 被误判完成
    let actual_downloaded = engine.pool.total_downloaded();
    engine_log!(
        "[download_file] 收尾校验: final_state={:?} downloaded={}/{} chunks_total={} chunks_completed={}",
        final_state, actual_downloaded, total,
        engine.pool.chunks_count(), engine.pool.completed_count()
    );
    if final_state == EngineState::Completed && actual_downloaded < total {
        // 打印未完成 chunk 的详细信息, 便于定位 "结尾失败" 的根因
        let chunks = engine.pool.chunks.read();
        for c in chunks.iter() {
            if !c.is_completed() {
                let state = c.state.load(Ordering::Relaxed);
                let state_str = match state {
                    0 => "PENDING", 1 => "ASSIGNED", 2 => "COMPLETED", 3 => "FAILED", _ => "UNKNOWN",
                };
                engine_log!(
                    "[download_file] 未完成 chunk: id={} [{}-{}] downloaded={}/{} state={} worker={}",
                    c.id, c.start, c.end(),
                    c.downloaded.load(Ordering::Relaxed), c.size(),
                    state_str, c.worker_id.load(Ordering::Relaxed)
                );
            }
        }
        engine_log!(
            "[dynamic_engine] ⚠ 完整性校验失败: 实际下载 {} 字节 < 文件大小 {} 字节, 标记为 Failed",
            actual_downloaded, total
        );
        let _ = engine.state_tx.send(EngineState::Failed);
        return Err(DownloadError::Other(format!(
            "完整性校验失败: 实际下载 {} 字节 < 文件大小 {} 字节",
            actual_downloaded, total
        )));
    }

    // ★ 移除同步 sync_all: 4GB 文件 sync_all 可能耗时 30-100s, 期间前端停滞检测
    //   会触发 retry_download → abort 当前任务 → 状态混乱 → 误报 "下载失败"
    //   OS write cache 会在后台自动落盘, 进程正常退出时也会刷盘, 数据安全有保障.
    //   如需强制落盘, 由 downloader.rs 在发送 completed 事件后异步执行.

    // ★ 断点续传: 下载成功完成, 删除 resume 文件 (避免下次重复恢复已完成的任务)
    if final_state == EngineState::Completed {
        if let Some(output) = engine.cfg.output.as_ref() {
            ChunkPool::remove_resume(output);
        }
    } else if engine.cfg.resume_enabled {
        // 失败/取消时再保存一次进度, 确保下次可恢复
        if let Some(output) = engine.cfg.output.as_ref() {
            engine.pool.save_resume(output);
        }
    }

    let _ = engine.state_tx.send(final_state);
    Ok(())
}

// ============================================================
// 单元测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_chunk_seed_dynamic() {
        let mb = 1024 * 1024;
        // ★ 语义变更 (2026-10-02): 加入"请求预算"约束。
        //   现行语义: seed = min(max(速度档位, 并发流数, 文件大小/7MB), REQUEST_BUDGET, 文件大小/1MB)
        //     · 文件大小/7MB → 块少而大: 该 CDN 按**请求数**限流 (实测 ~130/60s),
        //       块数就是请求数; 7MB 是"请求速率不越限"与"用满连接"之间的最优解;
        //     · ≤ REQUEST_BUDGET → 整个下载的请求数落在服务器限流预算内;
        //     · ≤ 文件大小/1MB → 不产生小于 MIN_CHUNK_SIZE 的碎片。
        const MAX_STREAMS: u32 = 64;

        // 并发流数(64)与 7MB 目标(100/7=14)取大 → 64
        assert_eq!(initial_chunk_seed(100 * mb, 5 * mb, MAX_STREAMS), 64);
        assert_eq!(initial_chunk_seed(100 * mb, 64 * 1024, MAX_STREAMS), 64);

        // 并发流数少时: 档位与 7MB 目标取大
        assert_eq!(initial_chunk_seed(100 * mb, 5 * mb, 8), 32);
        assert_eq!(initial_chunk_seed(100 * mb, 1 * mb, 8), 16);
        assert_eq!(initial_chunk_seed(100 * mb, 256 * 1024, 8), 14);

        // 小文件: 受"文件大小 / MIN_CHUNK_SIZE"上限约束, 不产生碎片
        assert_eq!(initial_chunk_seed(4 * mb, 5 * mb, MAX_STREAMS), 4);
        assert_eq!(initial_chunk_seed(MIN_CHUNK_SIZE, 5 * mb, MAX_STREAMS), 1);
        assert_eq!(initial_chunk_seed(1024, 5 * mb, MAX_STREAMS), 1);

        // 大文件: 被请求预算封顶 (5GB 本可切 731 块 → 压到 128)
        assert_eq!(
            initial_chunk_seed(5 * 1024 * mb, 5 * mb, MAX_STREAMS),
            REQUEST_BUDGET
        );
        // 295MB 这类常见体积应落在"最优块数"区间 (约 40 块上下), 而不是切得过碎
        let n295 = initial_chunk_seed(295 * mb, 5 * mb, MAX_STREAMS);
        assert!(
            (30..=64).contains(&n295),
            "295MB 的种子块数 {} 偏离最优区间 (请求数过多会逼近限流额度)",
            n295
        );
        // 任何情况下都不超过请求预算, 也不超过"文件大小 / MIN_CHUNK_SIZE"
        for size in [100 * mb, 1024 * mb, 5 * 1024 * mb, 64 * 1024 * mb] {
            let seed = initial_chunk_seed(size, 5 * mb, MAX_STREAMS);
            assert!(seed <= REQUEST_BUDGET, "种子块数 {} 超过请求预算", seed);
            assert!(seed <= (size / MIN_CHUNK_SIZE) as u32);
        }
    }

    #[test]
    fn test_request_bucket_paces_requests() {
        // ★ 新增 (2026-10-02): 请求令牌桶必须能挡住突发, 并在补充后放行。
        //   这是"不撞 429"的核心保障 —— 实测该 CDN 约 140 请求/60 秒。
        let bucket = RequestBucket::new();
        // 开局可突发 CAPACITY 个, 第 CAPACITY+1 个必须等待
        let mut taken = 0u32;
        for _ in 0..RequestBucket::CAPACITY as u32 {
            assert!(bucket.try_take().is_none(), "容量内的令牌应当立即取到");
            taken += 1;
        }
        assert_eq!(taken, RequestBucket::CAPACITY as u32);
        let wait = bucket.try_take();
        assert!(wait.is_some(), "超出容量后必须要求等待");
        // 等待时间应约等于补一个令牌所需时间 (1/2.2s ≈ 455ms)
        let ms = wait.unwrap();
        assert!(
            (300..=1200).contains(&ms),
            "等待时长 {}ms 不在合理范围",
            ms
        );
        // 桶只负责挡住"开局一次性把上百个请求全丢出去", 不该限制正常下载速率:
        //   · 容量与 worker 数同量级, 即"开局每条连接各发一个请求"这一天然突发;
        //   · 补充速率取宽松值, 保证正常下载不会被自己的限速器拖慢。
        //   ★ 容量必须与 streams_for_threads 的结果同步: 桶比 worker 小,
        //     桶就会变成新的瓶颈 (实测容量 64 配 128 worker 时被限速 4988 次)。
        assert!(
            RequestBucket::CAPACITY >= streams_for_threads(4) as f64,
            "突发容量不应小于开局并发数, 否则桶自己会成为瓶颈"
        );
        assert!(
            RequestBucket::CAPACITY <= 256.0,
            "突发容量不应超过 MAX_CONNS, 否则开局会一次放出过多新连接"
        );
        assert!(
            RequestBucket::REFILL_PER_SEC >= 1.0,
            "补充速率过低会让限速器自己成为瓶颈 (实测 1.5/s 时一次下载被限速 769 次)"
        );
    }

    #[test]
    fn test_parse_retry_after() {
        // ★ 新增 (2026-10-02): 服务器给的 Retry-After 必须能被解析出来。
        //   实测该 CDN 返回 `Retry-After: 60`, 旧实现把它丢掉、只用自己的 16s 退避,
        //   于是重试永远落在惩罚窗口内 → 限流永不解除 (尾部 B/s 的根因)。
        assert_eq!(
            parse_retry_after("HTTP 429 Too Many Requests|retry_after=60"),
            Some(60)
        );
        // 没有该头 → 回退到指数退避
        assert_eq!(parse_retry_after("HTTP 429 Too Many Requests"), None);
        // 形近但非数字 → 不应误解析
        assert_eq!(parse_retry_after("HTTP 429|retry_after=abc"), None);
        assert_eq!(parse_retry_after("HTTP 503 Service Unavailable"), None);
    }

    #[test]
    fn speed_cap_derives_cap_from_measured_baseline() {
        // 预热期按 ~2MB/s 喂数据 (每 20ms 喂 40KB)
        let cap = SpeedCap::new(0.8);
        let start = Instant::now();
        let need = Duration::from_millis(SpeedCap::SKIP_MS + SpeedCap::MEASURE_MS);
        while start.elapsed() < need {
            std::thread::sleep(Duration::from_millis(20));
            cap.consume(40 * 1024);
        }
        // 预热结束后再喂一次 → 此时才会算出上限
        cap.consume(1024);

        let c = cap.cap_bps() as f64;
        let mb = 1024.0 * 1024.0;
        assert!(c > 0.0, "预热结束后应算出速率上限");
        assert!(
            c > 1.0 * mb && c < 2.6 * mb,
            "按 ~2MB/s 喂入时, 80% 上限应约 1.6MB/s, 实得 {:.2} MB/s",
            c / mb
        );
        // 上限不该被压到不可用的水平
        assert!(c >= SpeedCap::MIN_CAP_BPS);
    }

    #[test]
    fn speed_cap_ratio_can_be_changed_at_runtime() {
        // ★ 用户场景: 暂停 → 换密钥 → 继续, 必须能立刻改档位。
        //   之前引擎是启动时建好的、resume 复用旧引擎, 切档完全不生效。
        let cap = SpeedCap::new(1.0);
        assert_eq!(cap.ratio(), 1.0, "新建时按传入比例");
        // 不限速 → 无论读多少都不等待, 也不该累计测量字节
        assert_eq!(cap.consume(10 * 1024 * 1024), 0);
        assert_eq!(cap.cap_bps(), 0, "不限速时不该有上限");

        // 运行中切到 80%
        cap.set_ratio(0.8);
        assert!((cap.ratio() - 0.8).abs() < 0.001, "比例应已改为 0.8");
        // 之前没测过基准 → 重置测量窗口, 先不限制
        assert_eq!(cap.consume(1024), 0);
        assert_eq!(cap.cap_bps(), 0, "还没测出基准前不应有上限");

        // 切回不限速 → 立刻恢复
        cap.set_ratio(1.0);
        assert_eq!(cap.ratio(), 1.0);
        assert_eq!(cap.consume(10 * 1024 * 1024), 0);
    }

    #[test]
    fn speed_cap_charges_bytes_and_makes_consumer_wait() {
        // 喂入远超上限的字节 → 必须要求等待 (证明限速真的在按字节计费)
        let cap = SpeedCap::new(0.5);
        let start = Instant::now();
        let need = Duration::from_millis(SpeedCap::SKIP_MS + SpeedCap::MEASURE_MS);
        while start.elapsed() < need {
            std::thread::sleep(Duration::from_millis(20));
            cap.consume(20 * 1024);
        }
        cap.consume(1024); // 触发上限计算
        // 一次性喂入 8MB, 远超过 1 秒的额度 → 必然要求等待
        let wait = cap.consume(8 * 1024 * 1024);
        assert!(
            wait > 0,
            "一次性读入 8MB 后应要求等待 (按字节计费的限速必须生效)"
        );
        assert!(wait < 60_000, "等待时长不应离谱, 实得 {}ms", wait);
    }

    #[test]
    fn test_streams_for_threads() {
        // ★ 更新 (2026-10-02): 倍率 16 → 32。实测聚合吞吐随并发次线性增长
        //   (1条 0.54MB/s → 16条 4.1MB/s → 64条 11.7MB/s), 64 条远未到顶,
        //   故把开局并发推到 128 条。
        assert_eq!(streams_for_threads(4), 64);
        assert_eq!(streams_for_threads(2), 32);
        assert_eq!(streams_for_threads(1), 16);
        // 上限仍由 MAX_CONNS 封顶
        assert_eq!(streams_for_threads(16), MAX_CONNS);
    }

    #[test]
    fn test_chunk_pool_init() {
        let pool = ChunkPool::new(100 * 1024 * 1024, 4, 256);
        assert_eq!(pool.chunks_count(), 4);
        // 4 个 chunk 覆盖 100MB, 每个 25MB
        let chunks = pool.chunks.read();
        for c in chunks.iter() {
            assert!(c.size() > 0);
        }
        let total: u64 = chunks.iter().map(|c| c.size()).sum();
        assert_eq!(total, 100 * 1024 * 1024);
    }

    #[test]
    fn test_chunk_pool_init_small_file() {
        let pool = ChunkPool::new(1024, 4, 256);
        // 小文件: 1 个 chunk 即可 (但 current impl 按 count 切, 故可能有多个)
        // 简化: 检查 total = 1024
        let chunks = pool.chunks.read();
        let total: u64 = chunks.iter().map(|c| c.size()).sum();
        assert_eq!(total, 1024);
    }

    #[test]
    fn test_chunk_pool_acquire_release() {
        let pool = ChunkPool::new(100 * 1024 * 1024, 4, 256);
        let c = pool.acquire(0).unwrap();
        assert_eq!(c.state.load(Ordering::Relaxed), CHUNK_ASSIGNED);
        pool.release_complete(&c);
        assert!(c.is_completed());
        assert!(pool.all_completed() == false); // 还有 3 个
    }

    #[test]
    fn test_chunk_pool_max_chunks_guard() {
        // ★ 完全动态分块安全阀: 达到上限后 split_half / steal_from_slowest 不再派生新块
        let pool = ChunkPool::new(100 * 1024 * 1024, 2, 4);
        assert_eq!(pool.max_chunks(), 4);
        assert_eq!(pool.chunks_count(), 2);
        let c0 = pool.chunks.read()[0].clone();
        assert!(pool.split_half(&c0, MIN_CHUNK_SIZE).is_some());
        assert_eq!(pool.chunks_count(), 3);
        // 再切一次到 4 个, 之后应被拒绝
        let c1 = pool.chunks.read()[1].clone();
        assert!(pool.split_half(&c1, MIN_CHUNK_SIZE).is_some());
        assert_eq!(pool.chunks_count(), 4);
        let c2 = pool.chunks.read()[2].clone();
        assert!(pool.split_half(&c2, MIN_CHUNK_SIZE).is_none(), "达到上限后不应再切分");
        assert_eq!(pool.chunks_count(), 4);
    }

    #[test]
    fn test_engine_state_as_str() {
        assert_eq!(EngineState::Starting.as_str(), "starting");
        assert_eq!(EngineState::Running.as_str(), "running");
        assert_eq!(EngineState::Paused.as_str(), "paused");
        assert_eq!(EngineState::Completed.as_str(), "completed");
        assert_eq!(EngineState::Failed.as_str(), "failed");
        assert_eq!(EngineState::Canceled.as_str(), "canceled");
    }

    #[test]
    fn test_dynamic_scheduler_conservative_adjust() {
        // ★ 修正 (2026-09-30): 本测试此前按 HIGH_STREAK=3 / LOW_STREAK=5 断言,
        //   但常量已于 2026-09-11 调整为 HIGH_STREAK=2 / LOW_STREAK=4
        //   (注释明确写"3 → 2, 更快加连接" / "5 → 4"), 测试未同步 → 长期失败.
        //   现按实际常量断言.

        // 高速测试: baseline=10MB/s, 高速=20MB/s (ratio=2.0)
        let mut s = DynamicScheduler::new(16, 10_000_000);
        assert!(s.tick(20_000_000, MAX_CONNS).is_none(), "高速: 第 1 次仅累计 streak, 不应调整");
        let adj = s.tick(20_000_000, MAX_CONNS);
        assert_eq!(adj, Some(17), "高速: 第 2 次达到 HIGH_STREAK=2, +1 到 17");
        // 冷却期内不应再调
        s.tick(20_000_000, MAX_CONNS);
        assert_eq!(s.current_permits, 17);

        // 低速测试: 用全新 scheduler (避免 EMA 残留影响)
        // baseline=10MB/s, 低速=3MB/s (ratio=0.3)
        let mut s2 = DynamicScheduler::new(17, 10_000_000);
        // 跳过冷却 (last_adjust_at 默认已 -60s)
        for i in 0..3 {
            let r = s2.tick(3_000_000, MAX_CONNS);
            assert!(r.is_none(), "低速: 第 {} 次不应调整 (实际 {:?})", i + 1, r);
        }
        let adj = s2.tick(3_000_000, MAX_CONNS);
        assert_eq!(adj, Some(16), "低速: 第 4 次达到 LOW_STREAK=4, -1 到 16 (实际 {:?})", adj);
    }

    #[test]
    fn test_scheduler_respects_429_ceiling() {
        // ★ 429 自适应上限: ceiling=16 时不得加过 16
        let mut s = DynamicScheduler::new(16, 10_000_000);
        for _ in 0..10 {
            s.tick(20_000_000, 16);
        }
        assert_eq!(s.current_permits, 16, "ceiling=16 时连接数不应超过 16");
        // ceiling 下调后, current_permits 应被压回上限以内
        s.tick(20_000_000, 8);
        assert_eq!(s.current_permits, 8, "ceiling 下调后应同步压回 8");
    }

    #[test]
    fn test_speed_smoother() {
        let mut s = SpeedSmoother::new();
        // 第一次 tick
        std::thread::sleep(Duration::from_millis(50));
        let sp = s.tick(1000, 10000);
        assert!(sp > 0);
    }

    #[test]
    fn speed_smoother_does_not_report_resume_jump_as_speed() {
        // ★ 用户实测 bug (2026-10-03): 刚开始下载界面显示"几千 MB"。
        //   根因是断点续传让 downloaded 从 0 瞬间跳到已恢复的几 GB, 而
        //   SpeedSmoother::new() 的 last_downloaded 是 0 → 第一帧把这几 GB
        //   当成 0.1 秒内下载的量 → 5GB/0.1s ≈ 50GB/s。
        //   progress_loop 现在会把 last_downloaded 初始化成当前已下载量。
        let mut s = SpeedSmoother::new();
        s.last_downloaded = 5 * 1024 * 1024 * 1024; // 模拟恢复 5GB 后的起点
        std::thread::sleep(Duration::from_millis(50));
        let sp = s.tick(5 * 1024 * 1024 * 1024, 10 * 1024 * 1024 * 1024);
        assert_eq!(sp, 0, "没有新增字节时速度必须是 0, 不能把恢复量当成本帧下载量");
    }

    #[test]
    fn test_speed_smoother_zero_delta() {
        // ★ 修复验证: delta=0 时不应衰减残留 EMA, 应立即归零
        let mut s = SpeedSmoother::new();
        std::thread::sleep(Duration::from_millis(10));
        // 第一次: 1000 字节
        let sp1 = s.tick(1000, 10000);
        assert!(sp1 > 0, "首次应有速度");
        // 第二次: 同 1000 字节 (delta=0), 应归零
        std::thread::sleep(Duration::from_millis(10));
        let sp2 = s.tick(1000, 10000);
        assert_eq!(sp2, 0, "delta=0 时必须归零, 不应残留 EMA");
        // 第三次: 又有进度, 应恢复
        std::thread::sleep(Duration::from_millis(10));
        let sp3 = s.tick(2000, 10000);
        assert!(sp3 > 0, "恢复进度后应有速度");
    }
}
