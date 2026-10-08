//! 极速下载内核 v3 - 分层混合静态-动态分片架构
//!
//! v3 新增:
//! - 多源镜像聚合加速 (30ms 竞态预连接)
//! - 慢分片提前重调度 (镜像分叉并发下载)
//! - 分片预取预测 (16KB socket 预热)
//! - TCP 参数自动调优 (HTTP2 大窗口, 5G 模式)
//! - 模块架构: HttpDownloaderModule / ProbeModule / PrefetchModule

use async_trait::async_trait;
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use futures_util::StreamExt;
use parking_lot::{Mutex as PMutex, RwLock as PRwLock};
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Semaphore, Notify};

use crate::modules::*;

// ============================================================
// 常量定义
// ============================================================

/// ★ 极限优化 (2026-09-11): 128 → 256, 对齐 dynamic_engine MAX_CONNS
pub const MAX_CONNECTIONS_PER_HOST: u32 = 256;
pub const DEFAULT_CONNECTIONS: u32 = 32;
pub const TIMEOUT_CONNECT: u64 = 10;
pub const TIMEOUT_READ: u64 = 180;
pub const TIMEOUT_REQUEST: u64 = 300;
pub const SUBCHUNK_READ_TIMEOUT: u64 = 15;
pub const MIN_SUBCHUNK_SIZE: u64 = 256 * 1024;
pub const WORK_STEAL_REMAIN: u64 = 512 * 1024;
pub const PROBE_SAMPLE_BYTES: u64 = 8192;
/// 进度采样间隔: 100ms (从 250ms 降低, 与 dynamic_engine SPEED_SAMPLE_INTERVAL 一致)
/// 这样前端速度/进度刷新更流畅, 5s 窗口内采样点从 20 → 50, EMA 更平滑
pub const SPEED_SAMPLE_MS: u64 = 100;
pub const SCHEDULER_COOLDOWN_MS: u64 = 3000;
pub const OSCILLATION_WINDOW_MS: u64 = 10000;
pub const OSCILLATION_THRESHOLD: f64 = 0.60;
pub const OSCILLATION_UNFREEZE: f64 = 0.35;
pub const FREEZE_DURATION_MS: u64 = 5000;
pub const EMA_ALPHA: f64 = 0.96;
pub const SLOW_CHUNK_FACTOR: f64 = 0.75;
pub const MAX_REDIRECTS: usize = 10;
pub const MAX_RETRIES: u32 = 4;
pub const RESUME_EXT: &str = ".swiftfetch-resume";
pub const SLOW_SUB_ELAPSED_SEC: u64 = 5;
pub const SLOW_SUB_PROGRESS_THRESHOLD: f64 = 0.30;
pub const MIRROR_RACE_MS: u64 = 30;

// ============================================================
// A) 下载配置 DownloadConfig (v3: 新增 mirrors)
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadConfig {
    pub url: String,
    pub output: Option<PathBuf>,
    pub connections: u32,
    pub base_chunk_size: Option<u64>,
    pub auto_adjust: bool,
    pub resume_enabled: bool,
    pub proxy: Option<String>,
    pub headers: Vec<(String, String)>,
    pub timeout_connect: Duration,
    pub timeout_read: Duration,
    pub timeout_request: Duration,
    #[serde(default)]
    pub mirrors: Vec<String>,
    #[serde(default)]
    pub network_mode: NetworkMode,
    #[serde(default)]
    pub user_connections: Option<u32>,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            output: None,
            connections: DEFAULT_CONNECTIONS,
            base_chunk_size: None,
            auto_adjust: true,
            resume_enabled: true,
            proxy: None,
            headers: Self::default_headers(),
            timeout_connect: Duration::from_secs(TIMEOUT_CONNECT),
            timeout_read: Duration::from_secs(TIMEOUT_READ),
            timeout_request: Duration::from_secs(TIMEOUT_REQUEST),
            mirrors: Vec::new(),
            network_mode: NetworkMode::Auto,
            user_connections: None,
        }
    }
}

impl DownloadConfig {
    pub fn default_headers() -> Vec<(String, String)> {
        vec![
            // ★ IDM 风格 User-Agent: IE11 UA 被大多数镜像/CDN 接受, Chrome UA 被部分站点(如清华镜像)403
            ("User-Agent".into(), "Mozilla/5.0 (Windows NT 10.0; Win64; x64; Trident/7.0; rv:11.0) like Gecko".into()),
            ("Accept".into(), "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8".into()),
            ("Accept-Language".into(), "zh-CN,zh;q=0.9,en;q=0.8".into()),
            ("Accept-Encoding".into(), "gzip, deflate".into()),
            ("Connection".into(), "keep-alive".into()),
            ("Referer".into(), "about:blank".into()),
        ]
    }

    pub fn calc_base_chunk_size_v3(&self, file_size: u64) -> u64 {
        if let Some(s) = self.base_chunk_size {
            return s.max(MIN_SUBCHUNK_SIZE);
        }
        if file_size >= MIN_BASE_SIZE_FOR_BT_ALIGN {
            return HYBRID_ALIGNED_BASE;
        }
        if file_size < 100 * 1024 * 1024 {
            4 * 1024 * 1024
        } else if file_size < 2 * 1024 * 1024 * 1024 {
            8 * 1024 * 1024
        } else if file_size < 10 * 1024 * 1024 * 1024 {
            16 * 1024 * 1024
        } else {
            32 * 1024 * 1024
        }
    }

    #[deprecated]
    pub fn calc_base_chunk_size(&self, file_size: u64) -> u64 {
        self.calc_base_chunk_size_v3(file_size)
    }

    pub fn calc_http_connections(mode: NetworkMode, user_specified: Option<u32>) -> u32 {
        use NetworkMode::*;
        match mode {
            FiveG => {
                let val = user_specified.unwrap_or(FIVEG_HTTP_MAX_CONNS);
                val.clamp(4, FIVEG_HTTP_MAX_CONNS)
            }
            Wired25G => {
                const WIRED25G_DEFAULT: u32 = 32;
                const WIRED25G_MAX: u32 = 32;
                let val = user_specified.unwrap_or(WIRED25G_DEFAULT);
                val.clamp(4, WIRED25G_MAX)
            }
            Wired1G => {
                const WIRED1G_DEFAULT: u32 = 16;
                const WIRED1G_MAX: u32 = 24;
                let val = user_specified.unwrap_or(WIRED1G_DEFAULT);
                val.clamp(4, WIRED1G_MAX)
            }
            Auto => {
                let val = user_specified.unwrap_or(DEFAULT_CONNECTIONS);
                val.clamp(4, MAX_CONNECTIONS_PER_HOST)
            }
        }
    }
}

// ============================================================
// 进度回调输出结构
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressInfo {
    pub task: String,
    pub progress: f64,
    pub downloaded: u64,
    pub total: u64,
    pub speed_bps: u64,
    pub eta_sec: Option<u64>,
    pub active_conns: u32,
    pub slow_bases: u32,
    pub state: String,
}

// ============================================================
// 下载结果
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadResult {
    pub success: bool,
    pub message: String,
    pub output_path: PathBuf,
    pub file_size: u64,
    pub elapsed_ms: u128,
    pub avg_speed_bps: u64,
}

// ============================================================
// B) 前置探测模块 ProbeResult
// ============================================================

#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub file_size: u64,
    pub supports_range: bool,
    pub probe_latency_ms: u128,
    pub probe_throughput_bps: u64,
    pub final_url: String,
    pub loss_rate_guess: f64,
}

// ============================================================
// C) 混合分片管理器
// ============================================================

#[derive(Debug, Clone)]
pub struct SubChunk {
    pub id: u64,
    pub base_id: u32,
    pub start: u64,
    pub end: u64,
    pub assigned: bool,
    pub completed: bool,
    pub slow_helper_started: bool,
    pub done: Arc<std::sync::atomic::AtomicBool>,
}

pub struct BaseChunk {
    pub id: u32,
    pub start: u64,
    pub end: u64,
    pub size: u64,
    pub completed: bool,
    pub downloaded_atomic: Arc<AtomicU64>,
    pub sub_chunks: PMutex<VecDeque<SubChunk>>,
    pub assigned_workers: AtomicU64,
    pub start_time: PRwLock<Option<Instant>>,
    pub bytes_before: AtomicU64,
    pub first_downloaded_at: PRwLock<Option<Instant>>,
}

impl BaseChunk {
    pub fn downloaded(&self) -> u64 {
        self.downloaded_atomic.load(Ordering::Relaxed)
    }

    pub fn speed_bps(&self) -> Option<u64> {
        let dl = self.downloaded();
        let started = *self.start_time.read();
        started.and_then(|t| {
            let e = t.elapsed().as_secs_f64();
            if e > 0.0 && dl > 0 {
                Some((dl as f64 / e) as u64)
            } else { None }
        })
    }

    pub fn remaining(&self) -> u64 {
        self.size.saturating_sub(self.downloaded())
    }

    pub fn is_slow(&self) -> bool {
        let start = self.first_downloaded_at.read().and_then(|t| Some(t));
        if start.is_none() { return false; }
        let t = start.unwrap();
        let elapsed = t.elapsed().as_secs();
        if elapsed < SLOW_SUB_ELAPSED_SEC { return false; }
        let prog = self.downloaded() as f64 / self.size as f64;
        prog < SLOW_SUB_PROGRESS_THRESHOLD
    }
}

pub struct HybridChunkManager {
    pub bases: Vec<Arc<BaseChunk>>,
    pub file_size: u64,
    pub base_chunk_size: u64,
    original_base_size: u64,
    slow_chunk_threshold: PRwLock<u64>,
    next_sub_id: AtomicU64,
}

impl HybridChunkManager {
    pub fn new(file_size: u64, base_chunk_size: u64) -> Self {
        let mut bases = Vec::new();
        let mut id = 0u32;
        let mut offset = 0u64;
        while offset < file_size {
            let end = std::cmp::min(offset + base_chunk_size - 1, file_size - 1);
            let size = end - offset + 1;
            bases.push(Arc::new(BaseChunk {
                id,
                start: offset,
                end,
                size,
                completed: false,
                downloaded_atomic: Arc::new(AtomicU64::new(0)),
                sub_chunks: PMutex::new(VecDeque::new()),
                assigned_workers: AtomicU64::new(0),
                start_time: PRwLock::new(None),
                bytes_before: AtomicU64::new(0),
                first_downloaded_at: PRwLock::new(None),
            }));
            id += 1;
            offset += base_chunk_size;
        }
        Self {
            bases,
            file_size,
            base_chunk_size,
            original_base_size: base_chunk_size,
            slow_chunk_threshold: PRwLock::new(0),
            next_sub_id: AtomicU64::new(0),
        }
    }

    pub fn total_downloaded(&self) -> u64 {
        self.bases.iter().map(|b| b.downloaded()).sum()
    }

    pub fn completed_count(&self) -> usize {
        self.bases.iter().filter(|b| b.downloaded() >= b.size).count()
    }

    /// 关键修复: BT 模式下, 解析 .torrent 得到真实 total_size 和 piece_aligned_base 后,
    /// 必须重新切分 base chunks, 否则 piece_to_base 计算的 idx 会越界,
    /// completed_count() 永远追不上 bases.len(), 导致进度卡死.
    /// 必须在启动任何 worker 前调用 (此时 Arc<Self> 的 refcount=1, 才能通过 Arc::make_mut 拿到 &mut).
    pub fn rebuild_for_bt(&mut self, real_file_size: u64, aligned_base: u64) {
        // 重新按 new() 逻辑生成 bases 向量, 并更新元数据字段
        let mut bases = Vec::new();
        let mut id = 0u32;
        let mut offset = 0u64;
        while offset < real_file_size {
            let end = std::cmp::min(offset + aligned_base - 1, real_file_size - 1);
            let size = end - offset + 1;
            bases.push(Arc::new(BaseChunk {
                id,
                start: offset,
                end,
                size,
                completed: false,
                downloaded_atomic: Arc::new(AtomicU64::new(0)),
                sub_chunks: PMutex::new(VecDeque::new()),
                assigned_workers: AtomicU64::new(0),
                start_time: PRwLock::new(None),
                bytes_before: AtomicU64::new(0),
                first_downloaded_at: PRwLock::new(None),
            }));
            id += 1;
            offset += aligned_base;
        }
        self.bases = bases;
        self.file_size = real_file_size;
        self.base_chunk_size = aligned_base;
        self.original_base_size = aligned_base;
    }

    pub fn update_slow_threshold(&self, avg_bps: u64) {
        *self.slow_chunk_threshold.write() = (avg_bps as f64 * SLOW_CHUNK_FACTOR) as u64;
    }

    pub fn slow_bases_count(&self) -> u32 {
        let th = *self.slow_chunk_threshold.read();
        let mut count = 0u32;
        for b in &self.bases {
            if b.downloaded() >= b.size { continue; }
            if b.is_slow() { count += 1; continue; }
            if th == 0 { continue; }
            if b.start_time.read().is_none() { continue; }
            if let Some(sp) = b.speed_bps() {
                if sp < th && b.remaining() > MIN_SUBCHUNK_SIZE { count += 1; }
            }
        }
        count
    }

    pub fn scan_split_slow_chunks(&self) {
        let th = *self.slow_chunk_threshold.read();
        for b in &self.bases {
            if b.downloaded() >= b.size { continue; }
            if b.is_slow() { self.split_subchunk(b); continue; }
            if th == 0 { continue; }
            if let Some(sp) = b.speed_bps() {
                if sp < th { self.split_subchunk(b); }
            }
        }
    }

    pub fn split_subchunk(&self, base: &Arc<BaseChunk>) {
        let dl = base.downloaded();
        let remain_start = base.start + dl;
        let remain_end = base.end;
        if remain_start >= remain_end { return; }
        let remain_len = remain_end - remain_start + 1;
        if remain_len < MIN_SUBCHUNK_SIZE { return; }

        let subs = base.sub_chunks.lock();
        let any_unassigned = subs.iter().any(|s| !s.assigned && !s.completed);
        drop(subs);
        if any_unassigned { return; }

        let half = remain_len / 2;
        if half < MIN_SUBCHUNK_SIZE { return; }

        let mid = remain_start + half - 1;
        let id1 = self.next_sub_id.fetch_add(2, Ordering::Relaxed);
        let sc1 = SubChunk {
            id: id1,
            base_id: base.id,
            start: remain_start,
            end: mid,
            assigned: false,
            completed: false,
            slow_helper_started: false,
            done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let sc2 = SubChunk {
            id: id1 + 1,
            base_id: base.id,
            start: mid + 1,
            end: remain_end,
            assigned: false,
            completed: false,
            slow_helper_started: false,
            done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let mut subs = base.sub_chunks.lock();
        subs.push_back(sc1);
        subs.push_back(sc2);
    }

    pub fn work_steal(&self, _idle_worker_id: u32, total_workers: usize) -> Option<SubChunk> {
        let mut slowest: Option<(Arc<BaseChunk>, u64)> = None;
        for b in &self.bases {
            if b.downloaded() >= b.size { continue; }
            if b.remaining() < WORK_STEAL_REMAIN { continue; }
            if let Some(sp) = b.speed_bps() {
                match slowest {
                    None => slowest = Some((b.clone(), sp)),
                    Some((_, cur)) if sp < cur => slowest = Some((b.clone(), sp)),
                    _ => {}
                }
            }
        }
        let (slow_base, _) = slowest?;
        {
            let subs = slow_base.sub_chunks.lock();
            let unassigned = subs.iter().filter(|s| !s.assigned && !s.completed).count();
            if unassigned >= 2 * total_workers { return None; }
        }
        self.split_subchunk(&slow_base);
        let mut subs = slow_base.sub_chunks.lock();
        for s in subs.iter_mut() {
            if !s.assigned && !s.completed {
                s.assigned = true;
                return Some(s.clone());
            }
        }
        None
    }

    pub fn acquire_work(&self, source_prefer: SourceHint, no_cross: bool) -> Option<AcquiredWork> {
        for b in &self.bases {
            if b.downloaded() >= b.size { continue; }
            {
                let mut subs = b.sub_chunks.lock();
                for s in subs.iter_mut() {
                    if s.assigned || s.completed { continue; }
                    if no_cross {
                        let compat = match source_prefer {
                            SourceHint::Http | SourceHint::Any => true,
                            SourceHint::Bitorrent => false,
                        };
                        if !compat { continue; }
                    }
                    s.assigned = true;
                    let sc = s.clone();
                    drop(subs);
                    b.assigned_workers.fetch_add(1, Ordering::Relaxed);
                    if b.start_time.read().is_none() {
                        *b.start_time.write() = Some(Instant::now());
                    }
                    if b.first_downloaded_at.read().is_none() {
                        *b.first_downloaded_at.write() = Some(Instant::now());
                    }
                    return Some(AcquiredWork::Sub(sc));
                }
            }
            if b.assigned_workers.load(Ordering::Relaxed) == 0 {
                let dl = b.downloaded();
                if dl == 0 {
                    b.assigned_workers.fetch_add(1, Ordering::Relaxed);
                    if b.start_time.read().is_none() {
                        *b.start_time.write() = Some(Instant::now());
                    }
                    if b.first_downloaded_at.read().is_none() {
                        *b.first_downloaded_at.write() = Some(Instant::now());
                    }
                    return Some(AcquiredWork::Whole(b.clone()));
                }
            }
        }
        None
    }

    pub fn release_whole_on_split(&self, base: &Arc<BaseChunk>) {
        base.assigned_workers.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn set_base_chunk_size(&mut self, new_size: u64) {
        if new_size < self.original_base_size / 4 { return; }
        if new_size > self.original_base_size * 2 { return; }
        self.base_chunk_size = new_size;
    }
}

pub enum AcquiredWork {
    Whole(Arc<BaseChunk>),
    Sub(SubChunk),
}

// ============================================================
// D) SmoothScheduler 动态带宽测速 + 自适应调参
// ============================================================

pub struct SchedulerDecision {
    pub target_connections: u32,
    pub base_chunk_size_adjust: Option<i64>,
}

pub struct SmoothScheduler {
    pub estimated_max_bps: u64,
    pub recent_peak_ema: u64,
    pub ema_speed: u64,
    pub idle_workers: u32,
    pub target_connections: u32,
    pub current_base_size: u64,
    pub original_base_size: u64,
    pub last_adjust_time: Instant,
    pub high_bandwidth_count: u32,
    pub low_bandwidth_count: u32,
    pub very_high_count: u32,
}

impl SmoothScheduler {
    pub fn new(initial_conns: u32, probe_max_bps: u64, base_size: u64) -> Self {
        Self {
            estimated_max_bps: probe_max_bps.max(1024 * 1024),
            recent_peak_ema: 0,
            ema_speed: 0,
            idle_workers: 0,
            target_connections: initial_conns,
            current_base_size: base_size,
            original_base_size: base_size,
            last_adjust_time: Instant::now() - Duration::from_secs(10),
            high_bandwidth_count: 0,
            low_bandwidth_count: 0,
            very_high_count: 0,
        }
    }

    pub fn tick(&mut self) -> Option<SchedulerDecision> {
        if self.last_adjust_time.elapsed().as_millis() < SCHEDULER_COOLDOWN_MS as u128 {
            return None;
        }
        if self.ema_speed == 0 { return None; }

        let ratio = self.ema_speed as f64 / self.estimated_max_bps as f64;
        let mut decision: Option<SchedulerDecision> = None;

        if ratio > 0.95 {
            self.very_high_count += 1;
            self.high_bandwidth_count = 0;
            self.low_bandwidth_count = 0;
            if self.very_high_count >= 2 {
                let new_size = (self.current_base_size as f64 * 1.5) as u64;
                let capped = new_size.min(self.original_base_size * 2);
                if capped > self.current_base_size {
                    self.current_base_size = capped;
                    decision = Some(SchedulerDecision {
                        target_connections: self.target_connections,
                        base_chunk_size_adjust: Some(capped as i64),
                    });
                    self.very_high_count = 0;
                    self.last_adjust_time = Instant::now();
                }
            }
        } else if ratio > 0.85 {
            self.high_bandwidth_count += 1;
            self.very_high_count = 0;
            self.low_bandwidth_count = 0;
            if self.high_bandwidth_count >= 2 && self.idle_workers < 2 {
                if self.target_connections < MAX_CONNECTIONS_PER_HOST {
                    self.target_connections = (self.target_connections + 2).min(MAX_CONNECTIONS_PER_HOST);
                    decision = Some(SchedulerDecision {
                        target_connections: self.target_connections,
                        base_chunk_size_adjust: None,
                    });
                    self.high_bandwidth_count = 0;
                    self.last_adjust_time = Instant::now();
                }
            }
        } else if ratio < 0.60 {
            self.low_bandwidth_count += 1;
            self.high_bandwidth_count = 0;
            self.very_high_count = 0;
            // 速度低时不减连接: 速度低通常是服务器限速, 减连接只会更慢
            // 仅调整 base_chunk_size 来适应低速环境
            if self.low_bandwidth_count >= 3 {
                let load_per_conn = if self.target_connections > 0 {
                    self.original_base_size as f64 / self.target_connections as f64
                } else { 0.0 };
                if load_per_conn > 8.0 * 1024.0 * 1024.0 {
                    let new_size = (self.current_base_size as f64 / 2.0) as u64;
                    let floor = self.original_base_size / 4;
                    let capped = new_size.max(floor);
                    if capped < self.current_base_size {
                        self.current_base_size = capped;
                        decision = Some(SchedulerDecision {
                            target_connections: self.target_connections,
                            base_chunk_size_adjust: Some(capped as i64),
                        });
                        self.low_bandwidth_count = 0;
                        self.last_adjust_time = Instant::now();
                    }
                }
            }
        } else {
            self.high_bandwidth_count = 0;
            self.low_bandwidth_count = 0;
            self.very_high_count = 0;
        }
        decision
    }
}

// ============================================================
// E) SpeedSmoother 速度平滑 (滑动窗口 + 轻量 EMA)
// ============================================================

/// 速度采样窗口时长 (ms): 5 秒内的累计字节 / 时长 = 稳定瞬时速度
/// 5s 能覆盖 bursty 下载模式 (2s突发 + 3s空闲) 的完整周期, 避免窗口只捕捉到空闲期
const SPEED_WINDOW_MS: u64 = 5000;
/// 轻量 EMA α: 0.30 吸收 70% 窗口速度, 配合 5s 窗口进一步去抖
const SPEED_EMA_ALPHA: f64 = 0.30;

pub struct SpeedSmoother {
    pub ema_speed: f64,
    /// (采样时刻, 累计已下载字节) — 仅保留 SPEED_WINDOW_MS 内的样本
    samples: VecDeque<(Instant, u64)>,
    last_reported_progress: f64,
}

impl SpeedSmoother {
    pub fn new() -> Self {
        Self {
            ema_speed: 0.0,
            samples: VecDeque::new(),
            last_reported_progress: 0.0,
        }
    }

    pub fn tick(&mut self, current_bytes: u64, _file_size: u64) -> u64 {
        let now = Instant::now();
        // 推入当前样本
        self.samples.push_back((now, current_bytes));
        // 清理过期样本 (只保留窗口内)
        let cutoff = now - Duration::from_millis(SPEED_WINDOW_MS);
        while let Some((t, _)) = self.samples.front() {
            if *t < cutoff { self.samples.pop_front(); } else { break; }
        }
        // 计算窗口内的平均速度
        //
        // ★ 2026-10-07 修「刚开始下载速度虚高」:
        //   窗口刚建立时 front 和 back 只隔**一个采样间隔** (SPEED_SAMPLE_MS),
        //   而启动瞬间几十条并发流会同时吐出各自的第一批数据 —— 一个极小的 dt 去除一个
        //   很大的 delta, 算出来是真实值几十倍的虚高速度, 再靠 EMA 慢慢收敛,
        //   用户看到的就是"刚开始速度虚高, 过几秒才掉回正常"。
        //   要求窗口跨度至少 SPEED_MIN_SPAN_MS 才认这个值; 不够就沿用上一次估计
        //   (首次为 0 → 显示"连接中"而不是一个假的高速度)。
        const SPEED_MIN_SPAN_MS: u128 = 900;
        let window_speed = if self.samples.len() >= 2 {
            let (t0, b0) = self.samples.front().unwrap();
            let (t1, b1) = self.samples.back().unwrap();
            let span = t1.duration_since(*t0);
            if span.as_millis() >= SPEED_MIN_SPAN_MS {
                let delta = b1.saturating_sub(*b0) as f64;
                delta / span.as_secs_f64()
            } else {
                // 窗口还太短, 不引入虚高; 保留上一次估计
                self.ema_speed
            }
        } else {
            0.0
        };
        // 轻量 EMA 平滑 (α=0.5 → 1 秒内收敛到真实值)
        self.ema_speed = SPEED_EMA_ALPHA * self.ema_speed + (1.0 - SPEED_EMA_ALPHA) * window_speed;
        // 极小值归零, 避免 EMA 残留几十 B/s 干扰 ETA
        if self.ema_speed < 1.0 { self.ema_speed = 0.0; }
        self.ema_speed as u64
    }

    pub fn clamp_progress(&mut self, new_progress: f64) -> f64 {
        if new_progress < self.last_reported_progress { return self.last_reported_progress; }
        self.last_reported_progress = new_progress;
        new_progress
    }
}

// ============================================================
// F) OscillationGuard 智能防震荡
// ============================================================

pub enum OscillationState {
    Normal,
    Freeze(Instant),
}

pub struct OscillationGuard {
    window: VecDeque<(Instant, u64)>,
    pub state: OscillationState,
}

impl OscillationGuard {
    pub fn new() -> Self {
        Self { window: VecDeque::new(), state: OscillationState::Normal }
    }

    pub fn sample(&mut self, speed: u64) {
        let now = Instant::now();
        self.window.push_back((now, speed));
        let cutoff = now - Duration::from_millis(OSCILLATION_WINDOW_MS);
        while let Some((t, _)) = self.window.front() {
            if *t < cutoff { self.window.pop_front(); } else { break; }
        }
    }

    pub fn check(&mut self) -> bool {
        match self.state {
            OscillationState::Freeze(since) => {
                if since.elapsed().as_millis() >= FREEZE_DURATION_MS as u128 {
                    let cv = self.variation_coeff();
                    if cv < OSCILLATION_UNFREEZE {
                        self.state = OscillationState::Normal;
                        return false;
                    } else {
                        self.state = OscillationState::Freeze(Instant::now());
                    }
                }
                true
            }
            OscillationState::Normal => {
                let cv = self.variation_coeff();
                if cv > OSCILLATION_THRESHOLD && self.window.len() >= 10 {
                    self.state = OscillationState::Freeze(Instant::now());
                    true
                } else { false }
            }
        }
    }

    fn variation_coeff(&self) -> f64 {
        if self.window.len() < 5 { return 0.0; }
        let vals: Vec<f64> = self.window.iter().map(|(_, s)| *s as f64).collect();
        let mean: f64 = vals.iter().sum::<f64>() / vals.len() as f64;
        if mean < 1.0 { return 0.0; }
        let variance: f64 = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64;
        let std_dev = variance.sqrt();
        std_dev / mean
    }
}

// ============================================================
// G) 断点续传缓存 (v3: bt_piece_map_completed / base_chunk_done)
// ============================================================

#[derive(Debug, Serialize, Deserialize)]
pub struct ResumeFile {
    pub file_size: u64,
    pub supports_range: bool,
    pub completed_base_chunk_ids: Vec<u32>,
    pub completed_bytes_per_base_chunk: HashMap<u32, u64>,
    #[serde(default)]
    pub bt_piece_map_completed: Vec<u32>,
    #[serde(default)]
    pub base_chunk_done: Vec<u32>,
}

// ============================================================
// 客户端构建 (v3: TCP/HTTP2 自动调优)
// ============================================================

pub fn build_reqwest_client(
    cfg: &DownloadConfig,
    is_5g: bool,
    is_2_5g: bool,
) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(cfg.timeout_connect)
        .read_timeout(cfg.timeout_read)
        .timeout(cfg.timeout_request)
        .tcp_nodelay(true)
        // ★ IDM 式: 强制 HTTP/1.1 + 多独立 TCP 连接
        //   HTTP/2 会把所有请求多路复用到一个 TCP 连接, 服务器对单连接限速时
        //   即使开 N 个 worker 也只有 1 个连接的速度.
        .http1_only()
        .http1_title_case_headers()
        // ★ BitComet 式优化 (2026-09-10): 连接池 48/64/128 → 128/256, 配合更多 worker
        //   BitComet network.max_connecting_connections=1000
        .pool_max_idle_per_host(256)
        .pool_idle_timeout(Duration::from_secs(90))
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS));

    if is_5g {
        builder = builder
            .pool_max_idle_per_host(256);
    }
    if is_2_5g {
        builder = builder.pool_max_idle_per_host(256);
    }

    if let Some(ref proxy_str) = cfg.proxy {
        let proxy = reqwest::Proxy::all(proxy_str)
            .map_err(|e| anyhow!("Proxy 解析失败: {}", e))?;
        builder = builder.proxy(proxy);
    } else {
        if let Some(http) = get_sys_proxy_url("http") {
            if let Ok(p) = reqwest::Proxy::http(&http) {
                builder = builder.proxy(p);
            }
        }
        if let Some(https) = get_sys_proxy_url("https") {
            if let Ok(p) = reqwest::Proxy::https(&https) {
                builder = builder.proxy(p);
            }
        }
        if let Some(all) = get_sys_proxy_url("all") {
            if let Ok(p) = reqwest::Proxy::all(&all) {
                builder = builder.proxy(p);
            }
        }
    }

    Ok(builder.build()?)
}

// ============================================================
// SwiftFetch v3 核心
// ============================================================

pub struct SwiftFetch {
    pub config: DownloadConfig,
}

impl SwiftFetch {
    pub fn new(config: DownloadConfig) -> Self { Self { config } }

    pub fn build_client(&self) -> Result<reqwest::Client> {
        build_reqwest_client(&self.config, false, false)
    }

    pub fn build_client_static(cfg: &DownloadConfig, is_5g: bool, is_25g: bool) -> Result<reqwest::Client> {
        build_reqwest_client(cfg, is_5g, is_25g)
    }

    fn build_headers(cfg: &DownloadConfig, extra: &[(String, String)]) -> reqwest::header::HeaderMap {
        let mut map = reqwest::header::HeaderMap::new();
        let all = cfg.headers.iter().chain(extra.iter());
        for (k, v) in all {
            if let (Ok(key), Ok(val)) = (
                reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                reqwest::header::HeaderValue::from_str(v),
            ) {
                let _ = map.try_append(key, val);
            }
        }
        map
    }

    pub async fn probe(&self, client: &reqwest::Client) -> Result<ProbeResult> {
        Self::probe_static(&self.config, client).await
    }

    pub async fn probe_static(cfg: &DownloadConfig, client: &reqwest::Client) -> Result<ProbeResult> {
        let headers = Self::build_headers(cfg, &[]);
        let start = Instant::now();
        let mut req = client
            .get(&cfg.url)
            .headers(headers.clone())
            .header("range", "bytes=0-0");
        if let Some(p) = &cfg.proxy {
            req = req.header("x-swiftfetch-proxy", p);
        }
        let resp = req.send().await
            .with_context(|| "Probe GET Range 请求失败")?;
        let latency = start.elapsed().as_millis();
        let final_url = resp.url().to_string();

        let accepts_range = resp
            .headers()
            .get("accept-ranges")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_lowercase().contains("bytes"))
            .unwrap_or(false);

        let content_range = resp.headers().get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| {
                let parts: Vec<&str> = s.split('/').collect();
                if parts.len() == 2 { parts[1].parse::<u64>().ok() } else { None }
            });

        let content_length = resp.content_length().unwrap_or(0);
        let file_size = content_range.unwrap_or(if content_length == 1 { 0 } else { content_length });

        let tp_start = Instant::now();
        let sample_bytes = std::cmp::min(PROBE_SAMPLE_BYTES, file_size.max(PROBE_SAMPLE_BYTES));
        let mut probe_headers = headers.clone();
        let _ = probe_headers.insert(
            reqwest::header::HeaderName::from_static("range"),
            reqwest::header::HeaderValue::from_str(&format!("bytes=0-{}", sample_bytes - 1)).unwrap(),
        );
        let probe_resp = client.get(&final_url).headers(probe_headers).send().await;
        let (probe_throughput, loss_guess) = match probe_resp {
            Ok(r) if r.status().is_success() => {
                let mut bytes_rcvd = 0u64;
                let mut stream = r.bytes_stream();
                let mut bad_chunks = 0u64;
                let mut total_chunks = 0u64;
                while let Some(chunk_r) = stream.next().await {
                    total_chunks += 1;
                    match chunk_r {
                        Ok(chunk) => {
                            bytes_rcvd += chunk.len() as u64;
                            if bytes_rcvd >= sample_bytes { break; }
                        }
                        Err(_) => bad_chunks += 1,
                    }
                }
                let secs = tp_start.elapsed().as_secs_f64().max(0.001);
                let tp = (bytes_rcvd as f64 / secs) as u64;
                let loss = if total_chunks > 0 { (bad_chunks as f64 / total_chunks as f64) * 100.0 } else { 0.0 };
                (tp, loss)
            }
            _ => (1 * 1024 * 1024, 0.0),
        };

        Ok(ProbeResult {
            file_size,
            supports_range: accepts_range || content_range.is_some(),
            probe_latency_ms: latency,
            probe_throughput_bps: probe_throughput,
            final_url,
            loss_rate_guess: loss_guess,
        })
    }

    pub fn resume_path(output: &Path) -> PathBuf {
        let mut s = output.as_os_str().to_os_string();
        s.push(RESUME_EXT);
        PathBuf::from(s)
    }

    pub fn load_resume(output: &Path) -> Option<ResumeFile> {
        let p = Self::resume_path(output);
        let data = std::fs::read_to_string(p).ok()?;
        serde_json::from_str(&data).ok()
    }

    pub fn save_resume(output: &Path, rf: &ResumeFile) -> Result<()> {
        let p = Self::resume_path(output);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let s = serde_json::to_string(rf)?;
        std::fs::write(p, s)?;
        Ok(())
    }

    pub fn remove_resume(output: &Path) -> Result<()> {
        let p = Self::resume_path(output);
        if p.exists() { std::fs::remove_file(p)?; }
        Ok(())
    }

    pub fn resolve_output(cfg: &DownloadConfig, probe: &ProbeResult) -> PathBuf {
        if let Some(ref o) = cfg.output { return o.clone(); }
        let url = url::Url::parse(&probe.final_url)
            .unwrap_or_else(|_| url::Url::parse(&cfg.url).unwrap());
        let fname = url.path_segments()
            .and_then(|segs| segs.filter(|s| !s.is_empty()).last())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "download.bin".to_string());
        let decoded = percent_decode(&fname);
        PathBuf::from(decoded)
    }

    pub async fn download<F>(&self, on_progress: F) -> Result<DownloadResult>
    where
        F: Fn(ProgressInfo) + Send + Sync + 'static,
    {
        let start_instant = Instant::now();
        let on_progress = Arc::new(on_progress);
        if self.config.url.is_empty() {
            tracing::info!("BT-only/无HTTP链接模式, 跳过 HTTP probe 与 HTTP 下载流程");
            return Ok(DownloadResult {
                success: true,
                message: "BT-only 模式, HTTP 流程跳过".into(),
                output_path: self.config.output.clone().unwrap_or_else(|| PathBuf::from("bt_output")),
                file_size: 0,
                elapsed_ms: 0,
                avg_speed_bps: 0,
            });
        }
        let client = self.build_client()?;
        let probe = self.probe(&client).await
            .with_context(|| "前置探测失败")?;
        let output = Self::resolve_output(&self.config, &probe);
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        let file_size = if probe.file_size == 0 {
            tracing::warn!("文件大小未知, 使用单连接流式下载");
            return self.stream_unknown_size(
                &client, &probe.final_url, &output, on_progress, start_instant
            ).await;
        } else { probe.file_size };

        if !probe.supports_range {
            tracing::warn!("服务器不支持 Range, 降级单连接");
            return self.stream_unknown_size(
                &client, &probe.final_url, &output, on_progress, start_instant
            ).await;
        }

        let base_chunk_size = self.config.calc_base_chunk_size_v3(file_size);
        let mgr = HybridChunkManager::new(file_size, base_chunk_size);

        if self.config.resume_enabled {
            if let Some(rf) = Self::load_resume(&output) {
                if rf.file_size == file_size {
                    for id in &rf.completed_base_chunk_ids {
                        if let Some(b) = mgr.bases.get(*id as usize) {
                            b.downloaded_atomic.store(b.size, Ordering::Relaxed);
                        }
                    }
                    for (id, bytes) in &rf.completed_bytes_per_base_chunk {
                        if let Some(b) = mgr.bases.get(*id as usize) {
                            let cur = b.downloaded_atomic.load(Ordering::Relaxed);
                            if *bytes > cur {
                                b.downloaded_atomic.store(*bytes, Ordering::Relaxed);
                            }
                        }
                    }
                }
            }
        }

        let mgr = Arc::new(mgr);
        let downloaded = Arc::new(AtomicU64::new(mgr.total_downloaded()));

        let file = Arc::new(tokio::sync::Mutex::new(None));
        {
            let mut f = OpenOptions::new()
                .create(true).read(true).write(true)
                .open(&output).await
                .with_context(|| format!("无法创建输出文件: {}", output.display()))?;
            f.set_len(file_size).await.ok();
            *file.lock().await = Some(f);
        }

        let final_net_mode = if self.config.network_mode == NetworkMode::Auto {
            let latency_ms = probe.probe_latency_ms;
            let loss = probe.loss_rate_guess;
            if latency_ms > 40 && loss > 0.5 {
                NetworkMode::FiveG
            } else if probe.probe_throughput_bps > 2_500_000_000 / 8 {
                NetworkMode::Wired25G
            } else if probe.probe_throughput_bps > 1_000_000_000 / 8 {
                NetworkMode::Wired1G
            } else {
                NetworkMode::Wired1G
            }
        } else {
            self.config.network_mode
        };

        let target_conns = DownloadConfig::calc_http_connections(final_net_mode, self.config.user_connections);
        let sem = Arc::new(Semaphore::new(target_conns as usize));
        let effective_conns = Arc::new(AtomicU64::new(target_conns as u64));
        let active_conns = Arc::new(AtomicU64::new(0));
        let stop_notify = Arc::new(Notify::new());
        let task_id = format!("sf_{}", start_instant.elapsed().as_millis());
        let mirrors: Vec<String> = std::iter::once(probe.final_url.clone())
            .chain(self.config.mirrors.clone().into_iter()).collect();

        let mut scheduler = SmoothScheduler::new(
            target_conns, probe.probe_throughput_bps, base_chunk_size,
        );
        // 不从 MAX 开始, 让调度器从 target_conns 逐步上调, 避免连接数暴涨导致速度突发

        let progress_mgr = mgr.clone();
        let progress_downloaded = downloaded.clone();
        let progress_sem = sem.clone();
        let progress_eff = effective_conns.clone();
        let progress_active = active_conns.clone();
        let progress_on_prog = on_progress.clone();
        let progress_stop = stop_notify.clone();
        let progress_output = output.clone();
        let progress_task_id = task_id.clone();
        let resume_enabled = self.config.resume_enabled;
        let base_chunk_done = Arc::new(parking_lot::Mutex::new(std::collections::HashSet::<u32>::new()));

        let progress_handle = tokio::spawn(async move {
            let mut smoother = SpeedSmoother::new();
            let mut osc_guard = OscillationGuard::new();
            let interval = Duration::from_millis(SPEED_SAMPLE_MS);
            loop {
                let current = progress_mgr.total_downloaded().min(file_size);
                progress_downloaded.store(current, Ordering::Relaxed);
                let speed = smoother.tick(current, file_size);
                scheduler.ema_speed = speed;
                if speed > scheduler.recent_peak_ema {
                    scheduler.recent_peak_ema = speed;
                    let est = (scheduler.estimated_max_bps as f64 * 0.5 + speed as f64 * 0.5) as u64;
                    scheduler.estimated_max_bps = est.max(scheduler.estimated_max_bps);
                }
                osc_guard.sample(speed);
                let frozen = osc_guard.check();
                let avail_permits = progress_sem.available_permits() as u32;
                let eff = progress_eff.load(Ordering::Relaxed) as u32;
                scheduler.idle_workers = avail_permits;

                if !frozen {
                    if let Some(decision) = scheduler.tick() {
                        let target = decision.target_connections;
                        progress_eff.store(target as u64, Ordering::Relaxed);
                        let current_permits = progress_sem.available_permits() as u64
                            + (eff as u64 - progress_sem.available_permits() as u64);
                        let delta = target as i64 - current_permits as i64;
                        if delta > 0 { progress_sem.add_permits(delta as usize); }
                        else if delta < 0 {
                            let forget = (-delta) as usize;
                            for _ in 0..forget {
                                if let Ok(p) = progress_sem.clone().try_acquire_owned() { p.forget(); }
                            }
                        }
                    }
                }

                progress_mgr.update_slow_threshold(speed);
                progress_mgr.scan_split_slow_chunks();

                let raw_progress = if file_size > 0 {
                    (current as f64 / file_size as f64) * 100.0
                } else { 0.0 };
                let progress = smoother.clamp_progress(raw_progress.min(100.0));

                let slow = progress_mgr.slow_bases_count();
                let active = progress_active.load(Ordering::Relaxed) as u32;
                let eta = if speed > 0 && file_size > current {
                    Some((file_size - current) / speed)
                } else { None };

                if resume_enabled {
                    let mut completed_ids = Vec::new();
                    let mut bytes_map: HashMap<u32, u64> = HashMap::new();
                    for b in &progress_mgr.bases {
                        let dl = b.downloaded();
                        if dl >= b.size { completed_ids.push(b.id); }
                        else if dl > 0 { bytes_map.insert(b.id, dl); }
                    }
                    let rf = ResumeFile {
                        file_size,
                        supports_range: probe.supports_range,
                        completed_base_chunk_ids: completed_ids,
                        completed_bytes_per_base_chunk: bytes_map,
                        bt_piece_map_completed: Vec::new(),
                        base_chunk_done: Vec::new(),
                    };
                    let _ = Self::save_resume(&progress_output, &rf);
                }

                let state = if progress >= 100.0 - f64::EPSILON {
                    "completed".to_string()
                } else { "running".to_string() };

                progress_on_prog(ProgressInfo {
                    task: progress_task_id.clone(),
                    progress, downloaded: current, total: file_size,
                    speed_bps: speed, eta_sec: eta, active_conns: active,
                    slow_bases: slow, state,
                });

                if progress >= 100.0 - f64::EPSILON { break; }

                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = progress_stop.notified() => { break; }
                }
            }
        });

        let mut worker_handles = Vec::new();
        let initial_workers = target_conns as usize;
        for wid in 0..MAX_CONNECTIONS_PER_HOST {
            let permit = if (wid as usize) < initial_workers {
                Some(sem.clone().acquire_owned().await.unwrap())
            } else { None };
            let mgr = mgr.clone();
            let client = client.clone();
            let file = file.clone();
            let downloaded = downloaded.clone();
            let active = active_conns.clone();
            let mirrors_c = mirrors.clone();
            let extra_headers = self.config.headers.clone();
            let effective = effective_conns.clone();
            let sem = sem.clone();
            let stop_c = stop_notify.clone();
            let base_done = base_chunk_done.clone();

            let handle = tokio::spawn(async move {
                let mut permit_owned = permit;
                loop {
                    let effective_now = effective.load(Ordering::Relaxed) as u32;
                    if wid >= effective_now {
                        if permit_owned.is_some() { permit_owned.take(); }
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                            _ = stop_c.notified() => { break; }
                        }
                        continue;
                    }
                    if permit_owned.is_none() {
                        match sem.clone().try_acquire_owned() {
                            Ok(p) => permit_owned = Some(p),
                            Err(_) => {
                                tokio::select! {
                                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                                    _ = stop_c.notified() => { break; }
                                }
                                continue;
                            }
                        }
                    }

                    let work = {
                        let direct = mgr.acquire_work(SourceHint::Http, false);
                        if direct.is_some() { direct }
                        else {
                            let steal = mgr.work_steal(wid, effective_now as usize);
                            steal.map(AcquiredWork::Sub)
                        }
                    };
                    let work = match work {
                        Some(w) => w,
                        None => {
                            if mgr.completed_count() == mgr.bases.len() { break; }
                            let total = mgr.total_downloaded();
                            if total >= mgr.file_size { break; }
                            tokio::select! {
                                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                                _ = stop_c.notified() => { break; }
                            }
                            continue;
                        }
                    };

                    let (range_start, range_end, base_arc, sub_done_opt, whole_base_opt) = match work {
                        AcquiredWork::Whole(b) => {
                            let dl = b.downloaded();
                            if dl >= b.size {
                                b.assigned_workers.fetch_sub(1, Ordering::Relaxed);
                                continue;
                            }
                            if b.size > MIN_SUBCHUNK_SIZE * 2 {
                                mgr.split_subchunk(&b);
                                let has_subs = b.sub_chunks.lock().len() > 0;
                                if has_subs {
                                    mgr.release_whole_on_split(&b);
                                    continue;
                                }
                            }
                            (b.start + dl, b.end, b.clone(), None, Some((b.clone(), dl)))
                        }
                        AcquiredWork::Sub(sc) => {
                            let done = sc.done.clone();
                            let base = mgr.bases.get(sc.base_id as usize).cloned().unwrap();
                            (sc.start, sc.end, base, Some((done, sc.id)), None)
                        }
                    };

                    let range_size = range_end.saturating_sub(range_start).saturating_add(1);
                    active.fetch_add(1, Ordering::Relaxed);
                    let res = download_subchunk_with_mirror_race(
                        &client, &mirrors_c, &extra_headers,
                        range_start, range_end, &file, &base_arc, wid,
                    ).await;
                    active.fetch_sub(1, Ordering::Relaxed);

                    match &sub_done_opt {
                        Some((_done, sc_id)) => {
                            let mut subs = base_arc.sub_chunks.lock();
                            for s in subs.iter_mut() {
                                if s.id == *sc_id {
                                    s.assigned = false;
                                    s.completed = res.is_ok();
                                    break;
                                }
                            }
                        }
                        None => {
                            if let Some((b, _dl)) = &whole_base_opt {
                                b.assigned_workers.fetch_sub(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
            });
            worker_handles.push(handle);
        }

        for h in worker_handles { let _ = h.await; }
        // ★ Bug 修复: 全局 downloaded 计数器由进度线程周期性采样(250ms), 最后一小段字节可能未同步
        // 直接从 mgr.total_downloaded() 读取最新总和, 避免验证时误报"下载不完整"
        let fresh_total = mgr.total_downloaded().min(file_size);
        downloaded.store(fresh_total, Ordering::Relaxed);
        let total_dl_now = downloaded.load(Ordering::Relaxed);
        if total_dl_now < file_size {
            stop_notify.notify_waiters();
        } else {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
            loop {
                if progress_handle.is_finished() { break; }
                if tokio::time::Instant::now() >= deadline {
                    stop_notify.notify_waiters();
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        let _ = progress_handle.await;

        {
            let mut f_guard = file.lock().await;
            if let Some(f) = f_guard.as_mut() {
                let _ = f.flush().await;
                let _ = f.sync_all().await;
            }
            *f_guard = None;
        }
        if resume_enabled { let _ = Self::remove_resume(&output); }

        // ★ 再次同步: progress_handle.await 后可能又有更新
        let final_fresh = mgr.total_downloaded().min(file_size);
        downloaded.store(final_fresh, Ordering::Relaxed);
        let total_dl = downloaded.load(Ordering::Relaxed);
        let elapsed = start_instant.elapsed();
        let avg_speed = if elapsed.as_secs() > 0 { total_dl / elapsed.as_secs() } else { total_dl };

        if total_dl < file_size {
            let mgr_ref = &mgr;
            let mut missing_bases: Vec<u32> = Vec::new();
            let mut missing_subchunks: Vec<serde_json::Value> = Vec::new();
            let mut failed_subchunks: Vec<serde_json::Value> = Vec::new();
            for b in &mgr_ref.bases {
                let dl = b.downloaded();
                if dl < b.size {
                    missing_bases.push(b.id);
                    let subs = b.sub_chunks.lock();
                    for s in subs.iter() {
                        if !s.completed {
                            missing_subchunks.push(serde_json::json!({
                                "base_id": s.base_id,
                                "sub_id": s.id,
                                "start": s.start,
                                "end": s.end,
                                "assigned": s.assigned,
                                "completed": s.completed,
                            }));
                        }
                    }
                }
            }
            let err_detail = serde_json::json!({
                "error": "下载不完整",
                "downloaded": total_dl,
                "total": file_size,
                "missing_bytes": file_size - total_dl,
                "missing_base_chunks": missing_bases,
                "missing_base_count": missing_bases.len(),
                "missing_subchunks": missing_subchunks,
                "missing_subchunk_count": missing_subchunks.len(),
                "failed_subchunks": failed_subchunks,
            });
            if let Ok(line) = serde_json::to_string(&err_detail) {
                eprintln!("{}", line);
            }
            return Err(anyhow!("下载不完整: {}/{} bytes, 缺失 base chunks: {}, subchunks: {}",
                total_dl, file_size, missing_bases.len(), missing_subchunks.len()));
        }

        on_progress(ProgressInfo {
            task: task_id, progress: 100.0, downloaded: total_dl, total: file_size,
            speed_bps: 0, eta_sec: Some(0), active_conns: 0, slow_bases: 0,
            state: "completed".to_string(),
        });

        Ok(DownloadResult {
            success: true, message: "下载完成".into(),
            output_path: output, file_size,
            elapsed_ms: elapsed.as_millis(), avg_speed_bps: avg_speed,
        })
    }

    async fn stream_unknown_size<F>(
        &self,
        client: &reqwest::Client,
        url: &str,
        output: &Path,
        on_progress: Arc<F>,
        start_instant: Instant,
    ) -> Result<DownloadResult>
    where F: Fn(ProgressInfo) + Send + Sync + 'static
    {
        let mut f = OpenOptions::new().create(true).write(true)
            .open(output).await.with_context(|| "无法创建输出文件")?;
        let headers = Self::build_headers(&self.config, &[]);
        let resp = client.get(url).headers(headers).send().await
            .with_context(|| "流式下载请求失败")?;
        let total = resp.content_length().unwrap_or(0);
        let mut downloaded: u64 = 0;
        let mut last_report = Instant::now();
        // 滑动窗口速度采样 (与 SpeedSmoother 一致: 2s 窗口 + 轻量 EMA)
        let mut stream_samples: VecDeque<(Instant, u64)> = VecDeque::new();
        let mut stream_ema: f64 = 0.0;
        let task_id = format!("stream_{}", start_instant.elapsed().as_millis());
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let data = chunk.map_err(|e| anyhow!("读取流失败: {}", e))?;
            f.write_all(&data).await?;
            downloaded += data.len() as u64;
            let now = Instant::now();
            let dt = now.duration_since(last_report).as_secs_f64();
            if dt >= (SPEED_SAMPLE_MS as f64 / 1000.0) {
                stream_samples.push_back((now, downloaded));
                let cutoff = now - Duration::from_millis(SPEED_WINDOW_MS);
                while let Some((t, _)) = stream_samples.front() {
                    if *t < cutoff { stream_samples.pop_front(); } else { break; }
                }
                let win_speed = if stream_samples.len() >= 2 {
                    let (t0, b0) = stream_samples.front().unwrap();
                    let (t1, b1) = stream_samples.back().unwrap();
                    let wdt = t1.duration_since(*t0).as_secs_f64();
                    if wdt > 0.0 { b1.saturating_sub(*b0) as f64 / wdt } else { 0.0 }
                } else { 0.0 };
                stream_ema = SPEED_EMA_ALPHA * stream_ema + (1.0 - SPEED_EMA_ALPHA) * win_speed;
                let speed = if stream_ema < 1.0 { 0u64 } else { stream_ema as u64 };
                last_report = now;
                let prog = if total > 0 { (downloaded as f64 / total as f64) * 100.0 } else { 0.0 };
                let eta = if speed > 0 && total > downloaded {
                    Some((total - downloaded) / speed)
                } else { None };
                on_progress(ProgressInfo {
                    task: task_id.clone(), progress: prog.min(100.0),
                    downloaded, total, speed_bps: speed, eta_sec: eta,
                    active_conns: 1, slow_bases: 0, state: "running".to_string(),
                });
            }
        }
        f.flush().await.ok();
        f.sync_all().await.ok();
        let elapsed = start_instant.elapsed();
        let avg_speed = if elapsed.as_secs() > 0 { downloaded / elapsed.as_secs() } else { downloaded };
        Ok(DownloadResult {
            success: true, message: "流式下载完成".into(),
            output_path: output.to_path_buf(), file_size: downloaded,
            elapsed_ms: elapsed.as_millis(), avg_speed_bps: avg_speed,
        })
    }
}

// ============================================================
// v3: 多源镜像竞态预连接 + 失败转移
// ============================================================

async fn download_subchunk_with_mirror_race(
    client: &reqwest::Client,
    urls: &[String],
    extra_headers: &[(String, String)],
    start: u64, end: u64,
    file: &Arc<tokio::sync::Mutex<Option<File>>>,
    base: &Arc<BaseChunk>,
    _wid: u32,
) -> Result<u64> {
    let mut last_err: Option<anyhow::Error> = None;
    let mut candidates: Vec<String> = if urls.len() <= 3 {
        urls.to_vec()
    } else {
        let mut rng = rand::thread_rng();
        let mut shuffled: Vec<String> = urls.to_vec();
        shuffled.shuffle(&mut rng);
        shuffled.into_iter().take(3).collect()
    };
    if candidates.is_empty() {
        anyhow::bail!("无可用镜像");
    }
    for attempt in 0..MAX_RETRIES {
        let url_idx = attempt as usize % candidates.len();
        let url = &candidates[url_idx];
        match download_subchunk(client, url, extra_headers, start, end, file, base).await {
            Ok(b) => return Ok(b),
            Err(e) => {
                last_err = Some(e);
                if attempt < MAX_RETRIES - 1 {
                    tokio::time::sleep(Duration::from_millis(500 * (attempt as u64 + 1))).await;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("下载分片失败")))
}

async fn download_subchunk(
    client: &reqwest::Client,
    url: &str,
    extra_headers: &[(String, String)],
    start: u64, end: u64,
    file: &Arc<tokio::sync::Mutex<Option<File>>>,
    base: &Arc<BaseChunk>,
) -> Result<u64> {
    let mut map = reqwest::header::HeaderMap::new();
    for (k, v) in DownloadConfig::default_headers().iter().chain(extra_headers.iter()) {
        if let (Ok(key), Ok(val)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) { let _ = map.try_append(key, val); }
    }
    map.insert(
        reqwest::header::RANGE,
        reqwest::header::HeaderValue::from_str(&format!("bytes={}-{}", start, end))?,
    );

    let req_fut = client.get(url).headers(map).send();
    let resp = tokio::time::timeout(
        Duration::from_secs(TIMEOUT_CONNECT + 30), req_fut,
    ).await
        .map_err(|_| anyhow!("分片请求超时"))?
        .with_context(|| "分片HTTP请求失败")?;
    let status = resp.status();
    if !(status == reqwest::StatusCode::PARTIAL_CONTENT || status.is_success()) {
        return Err(anyhow!("分片HTTP状态码错误: {}", status));
    }

    let expected = end - start + 1;

    // ★ 流式写入: 一次性 seek 到分片起点, 然后边读边写, 不再预分配整个分片到内存
    //   (避免 16-32MB 大分片 × 48 worker = 1.5GB 内存峰值, 显著提升稳定性)
    let mut bytes_read: u64 = 0;
    let mut write_offset = start;
    let mut stream = resp.bytes_stream();
    let mut consecutive_timeouts = 0u32;

    loop {
        let next = tokio::time::timeout(
            Duration::from_secs(SUBCHUNK_READ_TIMEOUT), stream.next(),
        );
        match next.await {
            Ok(Some(Ok(bytes))) => {
                consecutive_timeouts = 0;
                let chunk_len = bytes.len() as u64;
                // 边读边写 (流式落盘): 拿到锁就 seek + write, 写完立即释放
                {
                    let mut f_guard = file.lock().await;
                    if let Some(f) = f_guard.as_mut() {
                        f.seek(std::io::SeekFrom::Start(write_offset)).await
                            .map_err(|e| anyhow!("seek 失败: {}", e))?;
                        f.write_all(&bytes).await
                            .map_err(|e| anyhow!("写入失败: {}", e))?;
                    }
                }
                write_offset += chunk_len;
                bytes_read += chunk_len;

                // ★ 增量更新 base.downloaded_atomic (单调递增), 让进度条/速度采样更平滑
                let base_start = base.start;
                let new_rel = (write_offset - base_start);
                let mut prev = base.downloaded_atomic.load(Ordering::Relaxed);
                loop {
                    let new_dl = prev.max(new_rel);
                    match base.downloaded_atomic.compare_exchange_weak(
                        prev, new_dl, Ordering::Relaxed, Ordering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(x) => prev = x,
                    }
                }
            }
            Ok(Some(Err(e))) => { return Err(anyhow!("读取分片流失败: {}", e)); }
            Ok(None) => { break; }
            Err(_) => {
                consecutive_timeouts += 1;
                // ★ 连续 3 次读取超时才放弃 (单次超时可能是网络抖动, 流式可以继续)
                if consecutive_timeouts >= 3 {
                    return Err(anyhow!("读取分片连续超时 ({}-{}, 已读 {})", start, end, bytes_read));
                }
                // 短暂等待后继续尝试读取剩余字节
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }

    if bytes_read != expected {
        tracing::debug!("分片字节不匹配: expected {}, got {}", expected, bytes_read);
    }

    Ok(bytes_read)
}

// ============================================================
// 辅助函数
// ============================================================

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (from_hex(bytes[i+1]), from_hex(bytes[i+2])) {
                out.push((h << 4) | l);
                i += 3; continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn get_sys_proxy_url(kind: &str) -> Option<String> {
    let env_key = match kind {
        "http" => ["HTTP_PROXY", "http_proxy"],
        "https" => ["HTTPS_PROXY", "https_proxy"],
        _ => ["ALL_PROXY", "all_proxy"],
    };
    for k in &env_key {
        if let Ok(v) = std::env::var(k) {
            if !v.is_empty() { return Some(normalize_proxy_url(&v)); }
        }
    }
    None
}

fn normalize_proxy_url(raw: &str) -> String {
    if raw.starts_with("http://") || raw.starts_with("https://")
        || raw.starts_with("socks5://") || raw.starts_with("socks4://") {
        raw.to_string()
    } else {
        format!("http://{}", raw)
    }
}

pub fn format_speed(bps: u64) -> String {
    if bps >= 1024 * 1024 * 1024 { format!("{:.2} GB/s", bps as f64 / (1024.0 * 1024.0 * 1024.0)) }
    else if bps >= 1024 * 1024 { format!("{:.2} MB/s", bps as f64 / (1024.0 * 1024.0)) }
    else if bps >= 1024 { format!("{:.2} KB/s", bps as f64 / 1024.0) }
    else { format!("{} B/s", bps) }
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 { format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0)) }
    else if bytes >= 1024 * 1024 { format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0)) }
    else if bytes >= 1024 { format!("{:.2} KB", bytes as f64 / 1024.0) }
    else { format!("{} B", bytes) }
}

pub fn format_progress_bar(progress: f64, width: usize) -> String {
    let filled = ((progress / 100.0) * width as f64) as usize;
    let filled = filled.min(width);
    let empty = width - filled;
    format!("[{}{}]", "#".repeat(filled), "-".repeat(empty))
}

#[allow(dead_code)]
fn _consume_bytes(_: Bytes) {}

// ============================================================
// v3 Modules: HttpDownloaderModule / ProbeModule / PrefetchModule
// / OscillationGuardModule / SchedulerModule / BandwidthPoolModule
// ============================================================

pub struct HttpDownloaderModule;

#[async_trait]
impl DownloadModule for HttpDownloaderModule {
    fn name(&self) -> &'static str { "HttpDownloaderModule" }

    async fn start(self: Arc<Self>, ctx: Arc<EngineContext>) -> anyhow::Result<()> {
        if ctx.protocol == ProtocolMode::BtOnly {
            tracing::info!("HTTP 模块: BtOnly 模式, 跳过");
            return Ok(());
        }
        let client = SwiftFetch::build_client_static(
            &ctx.config,
            ctx.network_mode == NetworkMode::FiveG,
            ctx.network_mode == NetworkMode::Wired25G,
        )?;
        let mirrors: Vec<String> = std::iter::once(ctx.config.url.clone())
            .chain(ctx.mirrors.clone().into_iter()).collect();
        let max_workers = if ctx.network_mode == NetworkMode::FiveG {
            FIVEG_HTTP_MAX_CONNS
        } else { MAX_CONNECTIONS_PER_HOST };
        let max_workers = max_workers.min(ctx.http_conn_limit.load(Ordering::Relaxed));

        let mut handles = Vec::new();
        for wid in 0..max_workers {
            let client_c = client.clone();
            let mirrors_c = mirrors.clone();
            let headers_c = ctx.config.headers.clone();
            let ctx_c = ctx.clone();
            handles.push(tokio::spawn(async move {
                let _wid = wid;
                let mut idle_count = 0;
                loop {
                    tokio::select! {
                        _ = ctx_c.stop_notify.notified() => { break; }
                        _ = tokio::time::sleep(Duration::from_millis(0)) => {}
                    }
                    let sem = ctx_c.sem_http.clone();
                    let permit = sem.clone().try_acquire_owned().ok();
                    if permit.is_none() {
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        continue;
                    }
                    let permit = permit.unwrap();

                    let work = ctx_c.chunk_mgr.acquire_work(SourceHint::Http, ctx_c.no_cross_protocol);
                    let work = if work.is_none() {
                        ctx_c.chunk_mgr.work_steal(_wid, max_workers as usize).map(AcquiredWork::Sub)
                    } else { work };
                    let Some(work) = work else {
                        drop(permit);
                        idle_count += 1;
                        if idle_count > 50 {
                            if ctx_c.chunk_mgr.completed_count() >= ctx_c.chunk_mgr.bases.len() { break; }
                            idle_count = 0;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    };
                    idle_count = 0;

                    let (range_start, range_end, base_arc, sub_done_opt) = match work {
                        AcquiredWork::Whole(b) => {
                            let dl = b.downloaded();
                            if dl >= b.size {
                                b.assigned_workers.fetch_sub(1, Ordering::Relaxed);
                                drop(permit);
                                continue;
                            }
                            (b.start + dl, b.end, b.clone(), None)
                        }
                        AcquiredWork::Sub(sc) => {
                            let done = sc.done.clone();
                            let sc_id = sc.id;
                            let base = ctx_c.chunk_mgr.bases.get(sc.base_id as usize).cloned().unwrap();
                            (sc.start, sc.end, base, Some((done, sc_id)))
                        }
                    };

                    let range_size = range_end.saturating_sub(range_start).saturating_add(1);
                    ctx_c.active_http_conns.fetch_add(1, Ordering::Relaxed);
                    let res = download_subchunk_with_mirror_race(
                        &client_c, &mirrors_c, &headers_c,
                        range_start, range_end,
                        &ctx_c.file, &base_arc, _wid,
                    ).await;
                    ctx_c.active_http_conns.fetch_sub(1, Ordering::Relaxed);

                    match &sub_done_opt {
                        Some((_done, sc_id)) => {
                            let mut subs = base_arc.sub_chunks.lock();
                            for s in subs.iter_mut() {
                                if s.id == *sc_id {
                                    s.assigned = false;
                                    s.completed = res.is_ok();
                                    break;
                                }
                            }
                        }
                        None => {
                            let b = &base_arc;
                            b.assigned_workers.fetch_sub(1, Ordering::Relaxed);
                            if base_arc.downloaded() >= base_arc.size {
                                let mut done = ctx_c.base_chunk_done.lock();
                                if !done.contains(&base_arc.id) { done.push(base_arc.id); }
                            }
                        }
                    }
                    if res.is_ok() {
                        ctx_c.http_downloaded.fetch_add(range_size, Ordering::Release);
                        let mut series = ctx_c.completed_time_series.lock();
                        series.push((base_arc.id, Instant::now()));
                        let cur_len = series.len();
                        if cur_len > 20 { series.drain(0..cur_len-20); }
                    }
                    drop(permit);
                }
            }));
        }
        for h in handles { let _ = h.await; }
        Ok(())
    }
}

pub struct ProbeModule;

#[async_trait]
impl DownloadModule for ProbeModule {
    fn name(&self) -> &'static str { "ProbeModule" }

    async fn start(self: Arc<Self>, ctx: Arc<EngineContext>) -> anyhow::Result<()> {
        if ctx.protocol == ProtocolMode::BtOnly && ctx.config.url.is_empty() {
            return Ok(());
        }
        if ctx.config.url.is_empty() {
            return Ok(());
        }
        let is_5g = ctx.network_mode == NetworkMode::FiveG;
        let is_25g = ctx.network_mode == NetworkMode::Wired25G;
        let client = SwiftFetch::build_client_static(&ctx.config, is_5g, is_25g)?;
        let probe = SwiftFetch::probe_static(&ctx.config, &client).await?;
        ctx.probe.write(|p| *p = Some(probe.clone()));
        if ctx.file_size.load(Ordering::Relaxed) == 0 {
            ctx.file_size.store(probe.file_size, Ordering::Relaxed);
        }

        let latency_ms = probe.probe_latency_ms;
        let loss = probe.loss_rate_guess;
        let mut final_nm = ctx.network_mode;
        if final_nm == NetworkMode::Auto {
            if latency_ms > 40 && loss > 0.5 {
                let _ = ctx.event_tx.send(EngineEvent::SysInfo(
                    "检测到 5G 模式特征(高延迟+丢包). Windows建议: netsh interface tcp set global ecncapability=enabled".into()
                ));
                final_nm = NetworkMode::FiveG;
            } else if probe.probe_throughput_bps > 2_500_000_000 / 8 {
                final_nm = NetworkMode::Wired25G;
            } else if probe.probe_throughput_bps > 1_000_000_000 / 8 {
                final_nm = NetworkMode::Wired1G;
            }
        }
        if final_nm == NetworkMode::FiveG {
            ctx.bt_peer_limit.store(FIVEG_PEER_LIMIT, Ordering::Relaxed);
            ctx.global_max_conns.store(FIVEG_GLOBAL_MAX_CONNS, Ordering::Relaxed);
            ctx.http_conn_limit.store(FIVEG_HTTP_MAX_CONNS, Ordering::Relaxed);
        }

        let _ = ctx.event_tx.send(EngineEvent::SysInfo(format!(
            "Probe完成: size={}, latency={}ms, throughput={}, loss={:.2}%",
            format_bytes(probe.file_size), latency_ms,
            format_speed(probe.probe_throughput_bps), loss
        )));
        Ok(())
    }
}

pub struct PrefetchModule;

#[async_trait]
impl DownloadModule for PrefetchModule {
    fn name(&self) -> &'static str { "PrefetchModule" }

    async fn start(self: Arc<Self>, ctx: Arc<EngineContext>) -> anyhow::Result<()> {
        if ctx.protocol == ProtocolMode::BtOnly { return Ok(()); }
        let client = SwiftFetch::build_client_static(
            &ctx.config,
            ctx.network_mode == NetworkMode::FiveG,
            ctx.network_mode == NetworkMode::Wired25G,
        )?;
        loop {
            tokio::select! {
                _ = ctx.stop_notify.notified() => { break; }
                _ = tokio::time::sleep(Duration::from_millis(1000)) => {}
            }
            let series = ctx.completed_time_series.lock().clone();
            if series.len() < 5 { continue; }
            let n = series.len();
            let (t0, _) = series[0];
            let (tn, _) = series[n - 1];
            let _ = (t0, tn);
            let mut next_base_pred: Option<u32> = None;
            for b in &ctx.chunk_mgr.bases {
                if b.downloaded() >= b.size { continue; }
                next_base_pred = Some(b.id);
                break;
            }
            let Some(base_id) = next_base_pred else { continue; };
            if ctx.prefetch_warmed.lock().contains_key(&base_id) { continue; }
            let Some(base) = ctx.chunk_mgr.bases.get(base_id as usize) else { continue; };
            if base.assigned_workers.load(Ordering::Relaxed) > 0 { continue; }
            let warm_len = std::cmp::min(PREFETCH_WARM_BYTES as u64, base.size);
            let start = base.start;
            let end = start + warm_len - 1;
            let url = if ctx.config.url.is_empty() { continue; } else { ctx.config.url.clone() };
            let client_c = client.clone();
            let headers_c = ctx.config.headers.clone();
            let ctx_c = ctx.clone();
            tokio::spawn(async move {
                let res = download_subchunk_for_warm(&client_c, &url, &headers_c, start, end).await;
                if let Ok(data) = res {
                    let mut warmed = ctx_c.prefetch_warmed.lock();
                    warmed.insert(base_id, data.freeze());
                }
            });
        }
        Ok(())
    }
}

async fn download_subchunk_for_warm(
    client: &reqwest::Client,
    url: &str,
    extra_headers: &[(String, String)],
    start: u64, end: u64,
) -> Result<bytes::BytesMut> {
    let mut map = reqwest::header::HeaderMap::new();
    for (k, v) in DownloadConfig::default_headers().iter().chain(extra_headers.iter()) {
        if let (Ok(key), Ok(val)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) { let _ = map.try_append(key, val); }
    }
    map.insert(
        reqwest::header::RANGE,
        reqwest::header::HeaderValue::from_str(&format!("bytes={}-{}", start, end))?,
    );
    let resp = tokio::time::timeout(
        Duration::from_secs(TIMEOUT_CONNECT + 10),
        client.get(url).headers(map).send(),
    ).await.map_err(|_| anyhow!("warm timeout"))??;
    if !(resp.status() == reqwest::StatusCode::PARTIAL_CONTENT || resp.status().is_success()) {
        anyhow::bail!("warm status: {}", resp.status());
    }
    let mut buffer = bytes::BytesMut::with_capacity((end - start + 1) as usize);
    let mut stream = resp.bytes_stream();
    while let Some(next) = stream.next().await {
        match next {
            Ok(b) => buffer.extend_from_slice(&b),
            Err(_) => break,
        }
    }
    Ok(buffer)
}

pub struct OscillationGuardModule;

#[async_trait]
impl DownloadModule for OscillationGuardModule {
    fn name(&self) -> &'static str { "OscillationGuardModule" }

    async fn start(self: Arc<Self>, ctx: Arc<EngineContext>) -> anyhow::Result<()> {
        let mut guard = OscillationGuard::new();
        loop {
            tokio::select! {
                _ = ctx.stop_notify.notified() => { break; }
                _ = tokio::time::sleep(Duration::from_millis(SPEED_SAMPLE_MS)) => {}
            }
            let dl = ctx.downloaded.load(Ordering::Relaxed);
            let fs = ctx.file_size.load(Ordering::Relaxed);
            let speed = if fs > 0 {
                let mut s = ctx.speed_smoother.lock();
                s.tick(dl, fs)
            } else { 0 };
            guard.sample(speed);
            if guard.check() {
                tracing::debug!("震荡保护: 冻结调参");
            }
        }
        Ok(())
    }
}

pub struct SchedulerModule;

#[async_trait]
impl DownloadModule for SchedulerModule {
    fn name(&self) -> &'static str { "SchedulerModule" }

    async fn start(self: Arc<Self>, ctx: Arc<EngineContext>) -> anyhow::Result<()> {
        loop {
            tokio::select! {
                _ = ctx.stop_notify.notified() => { break; }
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
            let seeders = ctx.bt_seeders.load(Ordering::Relaxed);
            let bt_weight = if seeders > 50 {
                let _ = ctx.event_tx.send(EngineEvent::HotResource {
                    protocol: ProtocolMode::BtOnly, weight: 1.5,
                });
                1.5
            } else if seeders < 5 && seeders > 0 {
                let _ = ctx.event_tx.send(EngineEvent::ColdResource {
                    protocol: ProtocolMode::BtOnly, weight: 0.2,
                });
                let cur = ctx.http_conn_limit.load(Ordering::Relaxed);
                let new = ((cur as f64) * 1.3) as u32;
                ctx.http_conn_limit.store(new, Ordering::Relaxed);
                0.2
            } else { 1.0 };
            ctx.bt_weight.store((bt_weight * 1000.0) as u64, Ordering::Relaxed);

            let total = ctx.chunk_mgr.bases.len();
            let done = ctx.chunk_mgr.completed_count();
            if done >= total {
                let _ = ctx.stop_event_tx.send(());
                ctx.stop_notify.notify_waiters();
                break;
            }
            let fs = ctx.file_size.load(Ordering::Relaxed);
            let dl = ctx.downloaded.load(Ordering::Relaxed);
            if dl >= fs && fs > 0 {
                let _ = ctx.stop_event_tx.send(());
                ctx.stop_notify.notify_waiters();
                break;
            }
        }
        Ok(())
    }
}

pub struct BandwidthPoolModule;

#[async_trait]
impl DownloadModule for BandwidthPoolModule {
    fn name(&self) -> &'static str { "BandwidthPoolModule" }

    async fn start(self: Arc<Self>, ctx: Arc<EngineContext>) -> anyhow::Result<()> {
        loop {
            tokio::select! {
                _ = ctx.stop_notify.notified() => { break; }
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
            let (http_ema, bt_ema) = ctx.bandwidth_ema.tick(
                ctx.http_downloaded.load(Ordering::Relaxed),
                ctx.bt_downloaded.load(Ordering::Relaxed),
                0.8,
            );
            let global = http_ema.saturating_add(bt_ema);
            let seeders = ctx.bt_seeders.load(Ordering::Relaxed);

            let (mut http_ratio, mut bt_ratio) = (0.6f64, 0.4f64);
            if seeders > 50 { http_ratio = 0.35; bt_ratio = 0.65; }
            else if seeders < 5 && seeders > 0 { http_ratio = 0.85; bt_ratio = 0.15; }
            f64_to_atomic_store(&ctx.http_ratio_target, http_ratio);
            f64_to_atomic_store(&ctx.bt_ratio_target, bt_ratio);

            let bt_target = (global as f64 * bt_ratio) as u64;
            if global > 0 && (bt_ema as f64) < (bt_target as f64) * 0.75 && bt_ratio > 0.2 {
                let _ = ctx.event_tx.send(EngineEvent::BtBoost(8));
                let limit = ctx.bt_peer_limit.load(Ordering::Relaxed);
                ctx.bt_peer_limit.store(limit.saturating_add(8), Ordering::Relaxed);
                let hl = ctx.http_conn_limit.load(Ordering::Relaxed);
                ctx.http_conn_limit.store(hl.saturating_sub(4).max(4), Ordering::Relaxed);
            } else if global > 0 && (http_ema as f64) < (global as f64 * http_ratio) * 0.75 {
                let _ = ctx.event_tx.send(EngineEvent::HttpBoost(4));
                let hl = ctx.http_conn_limit.load(Ordering::Relaxed);
                ctx.http_conn_limit.store(hl.saturating_add(4).min(MAX_CONNECTIONS_PER_HOST), Ordering::Relaxed);
                let bl = ctx.bt_peer_limit.load(Ordering::Relaxed);
                ctx.bt_peer_limit.store(bl.saturating_sub(4).max(4), Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

pub struct NATSessionGuardModule;

#[async_trait]
impl DownloadModule for NATSessionGuardModule {
    fn name(&self) -> &'static str { "NATSessionGuardModule" }

    async fn start(self: Arc<Self>, ctx: Arc<EngineContext>) -> anyhow::Result<()> {
        loop {
            tokio::select! {
                _ = ctx.stop_notify.notified() => { break; }
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
            let http = ctx.active_http_conns.load(Ordering::Relaxed) as u32;
            let bt = ctx.active_bt_conns.load(Ordering::Relaxed) as u32;
            let total = http.saturating_add(bt);
            let max = ctx.global_max_conns.load(Ordering::Relaxed);
            let ratio = total as f64 / max as f64;
            if ratio > 0.9 {
                let _ = ctx.event_tx.send(EngineEvent::NatOverload);
                let delay = ctx.conn_delay_ms.load(Ordering::Relaxed);
                ctx.conn_delay_ms.store(delay.saturating_add(500), Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

pub struct ProgressModule {
    pub callback: Arc<dyn Fn(ProgressInfo) + Send + Sync>,
}

#[async_trait]
impl DownloadModule for ProgressModule {
    fn name(&self) -> &'static str { "ProgressModule" }

    async fn start(self: Arc<Self>, ctx: Arc<EngineContext>) -> anyhow::Result<()> {
        let mut smoother = SpeedSmoother::new();
        let interval = Duration::from_millis(SPEED_SAMPLE_MS);
        loop {
            let fs = ctx.file_size.load(Ordering::Relaxed);
            let current = ctx.downloaded.load(Ordering::Relaxed);
            let speed = smoother.tick(current, fs);
            let raw = if fs > 0 { (current as f64 / fs as f64) * 100.0 } else { 0.0 };
            let prog = smoother.clamp_progress(raw.min(100.0));
            let active = ctx.active_http_conns.load(Ordering::Relaxed)
                .saturating_add(ctx.active_bt_conns.load(Ordering::Relaxed));
            let slow = ctx.chunk_mgr.slow_bases_count();
            let eta = if speed > 0 && fs > current { Some((fs - current) / speed) } else { None };

            if ctx.config.resume_enabled {
                let mut completed_ids: Vec<u32> = Vec::new();
                let mut bytes_map: HashMap<u32, u64> = HashMap::new();
                for b in &ctx.chunk_mgr.bases {
                    let dl = b.downloaded();
                    if dl >= b.size { completed_ids.push(b.id); }
                    else if dl > 0 { bytes_map.insert(b.id, dl); }
                }
                let rf = ResumeFile {
                    file_size: fs,
                    supports_range: true,
                    completed_base_chunk_ids: completed_ids.clone(),
                    completed_bytes_per_base_chunk: bytes_map,
                    bt_piece_map_completed: ctx.bt_piece_map_completed.lock().clone(),
                    base_chunk_done: completed_ids,
                };
                let _ = SwiftFetch::save_resume(&ctx.output_path, &rf);
            }

            let state = if prog >= 100.0 - f64::EPSILON { "completed".to_string() } else { "running".to_string() };
            (self.callback)(ProgressInfo {
                task: ctx.task_id.clone(), progress: prog, downloaded: current, total: fs,
                speed_bps: speed, eta_sec: eta, active_conns: active,
                slow_bases: slow, state: state.clone(),
            });

            if prog >= 100.0 - f64::EPSILON {
                let _ = ctx.stop_event_tx.send(());
                ctx.stop_notify.notify_waiters();
                break;
            }

            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = ctx.stop_notify.notified() => { break; }
            }
        }
        Ok(())
    }
}
