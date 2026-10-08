//! SwiftFetch v3 IPC 异步消息协议层
//!
//! 协议格式: JSON Lines (UTF-8, \n 分隔)
//! - REQ:  Host <-> Plugin 同步请求/响应 (oneshot)
//! - EVT:  广播事件 (无 req_id, pub/sub)
//!
//! 覆盖业务:
//!   Host -> HTTP:  http.probe_file / http.fetch_subchunk / http.cancel_subchunk
//!   Host -> BT:    bt.parse_magnet / bt.parse_torrent / bt.announce / bt.connect_peers / bt.fetch_piece
//!   Host -> Sched: sched.adjust_concurrency / sched.thaw_oscillation
//!   Host -> Resume: resume.save_delta / resume.load_full
//!   Any  -> Host:  evt.progress.* / evt.connection_* / evt.chunk_done / evt.chunk_failed / evt.sysinfo

use parking_lot::Mutex as PMutex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

pub use crate::plugin::{IpcFrame, PluginMsg, PluginReply, generate_req_id};

// ============================================================
// 业务消息强类型枚举
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HttpMethod {
    ProbeFile { url: String, timeout_ms: u64 },
    FetchSubchunk { url: String, range: [u64; 2], timeout_ms: u64, mirror_urls: Vec<String> },
    CancelSubchunk { req_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BtMethod {
    ParseMagnet { magnet: String },
    ParseTorrent { file_path: String, #[serde(default)] bytes_b64: Option<String> },
    Announce { info_hash_hex: String, port: u16, uploaded: u64, downloaded: u64, left: u64 },
    ConnectPeers { peers: Vec<String> },
    FetchPiece { piece_idx: u32, piece_hash_hex: String, piece_size: u32, timeout_ms: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SchedMethod {
    AdjustConcurrency { target_connections: u32, base_chunk_size: Option<u64> },
    ThawOscillation { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ResumeMethod {
    SaveDelta { base_id: u32, bytes_added: Option<u64>, completed: bool },
    LoadFull { output_path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventTopic {
    #[serde(rename = "evt.progress.downloaded")]
    ProgressDownloaded { base_chunk_id: u32, bytes_added: u64, total_downloaded: u64 },
    #[serde(rename = "evt.progress.speed")]
    ProgressSpeed { http_bps: u64, bt_bps: u64, global_bps: u64 },
    #[serde(rename = "evt.connection.new")]
    ConnectionNew { protocol: String, addr: String, conn_id: u64 },
    #[serde(rename = "evt.connection.drop")]
    ConnectionDrop { conn_id: u64, reason: String },
    #[serde(rename = "evt.chunk_done")]
    ChunkDone { base_chunk_id: u32, elapsed_ms: u128 },
    #[serde(rename = "evt.chunk_failed")]
    ChunkFailed { base_chunk_id: u32, reason: String, retries: u32 },
    #[serde(rename = "evt.sysinfo")]
    SysInfo { key: String, value: String },
    #[serde(rename = "evt.scheduler_tick")]
    SchedulerTick { ema_bps: u64, active_conns: u32, slow_bases: u32 },
    #[serde(rename = "internal.ping")]
    InternalPing,
}

// ============================================================
// MessageThrottler: progress事件 100ms 窗口聚合
// ============================================================

pub struct MessageThrottler {
    window: Duration,
    state: PMutex<ThrottlerState>,
}

struct ThrottlerState {
    last_flush: Instant,
    buffered_progress: Option<BufferedProgress>,
}

struct BufferedProgress {
    base_chunk_id: u32,
    bytes_sum: u64,
    total_downloaded: u64,
    first_seen: Instant,
}

impl MessageThrottler {
    pub fn new(window_ms: u64) -> Self {
        Self {
            window: Duration::from_millis(window_ms),
            state: PMutex::new(ThrottlerState {
                last_flush: Instant::now(),
                buffered_progress: None,
            }),
        }
    }

    pub fn on_progress(&self, base_chunk_id: u32, bytes_added: u64, total_downloaded: u64) -> Option<(u32, u64, u64)> {
        let mut s = self.state.lock();
        let now = Instant::now();
        if let Some(buf) = s.buffered_progress.as_mut() {
            buf.bytes_sum = buf.bytes_sum.saturating_add(bytes_added);
            buf.total_downloaded = total_downloaded;
        } else {
            s.buffered_progress = Some(BufferedProgress {
                base_chunk_id,
                bytes_sum: bytes_added,
                total_downloaded,
                first_seen: now,
            });
        }

        let since_flush = now.duration_since(s.last_flush);
        if since_flush >= self.window {
            if let Some(buf) = s.buffered_progress.take() {
                s.last_flush = now;
                return Some((buf.base_chunk_id, buf.bytes_sum, buf.total_downloaded));
            }
        }
        None
    }

    pub fn flush(&self) -> Option<(u32, u64, u64)> {
        let mut s = self.state.lock();
        let buf = s.buffered_progress.take()?;
        s.last_flush = Instant::now();
        Some((buf.base_chunk_id, buf.bytes_sum, buf.total_downloaded))
    }

    pub fn should_throttle(topic: &str) -> bool {
        topic.starts_with("evt.progress.")
    }
}

impl Default for MessageThrottler {
    fn default() -> Self { Self::new(100) }
}

// ============================================================
// Windows Named Pipe 异步读写 (Framed LinesCodec)
// ============================================================

pub type IpcFramedReader<R> = FramedRead<R, LinesCodec>;
pub type IpcFramedWriter<W> = FramedWrite<W, LinesCodec>;

pub fn make_ipc_reader<R: AsyncRead>(reader: R) -> IpcFramedReader<R> {
    FramedRead::new(reader, LinesCodec::new_with_max_length(16 * 1024 * 1024))
}

pub fn make_ipc_writer<W: AsyncWrite>(writer: W) -> IpcFramedWriter<W> {
    FramedWrite::new(writer, LinesCodec::new_with_max_length(16 * 1024 * 1024))
}

// ============================================================
// req_id 格式校验
// ============================================================

pub fn validate_req_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('_').collect();
    if parts.len() < 3 { return false; }
    u64::from_str_radix(parts[parts.len() - 2], 16).is_ok()
        && u64::from_str_radix(parts[parts.len() - 1], 16).is_ok()
}

// ============================================================
// 通用构建 Helper
// ============================================================

pub fn make_request(plugin_short_name: &str, method: &str, payload: Option<serde_json::Value>) -> (String, IpcFrame) {
    let req_id = generate_req_id(plugin_short_name);
    let frame = IpcFrame::Request {
        v: 1,
        req_id: req_id.clone(),
        method: method.to_string(),
        payload,
        deadline_ms: None,
    };
    (req_id, frame)
}

pub fn make_reply(req_id: String, ok: bool, payload: Option<serde_json::Value>) -> IpcFrame {
    IpcFrame::Reply {
        req_id,
        status: if ok { "OK".into() } else { "ERR".into() },
        payload,
    }
}

pub fn make_event(topic: &str, payload: Option<serde_json::Value>) -> IpcFrame {
    IpcFrame::Event { topic: topic.to_string(), payload }
}

// ============================================================
// 环形崩溃历史 (helper)
// ============================================================

pub struct CrashBackoff {
    window: Duration,
    threshold: usize,
    backoff: Duration,
    history: PMutex<VecDeque<Instant>>,
    backoff_until: PMutex<Option<Instant>>,
}

impl CrashBackoff {
    pub fn new(window_secs: u64, threshold: usize, backoff_secs: u64) -> Self {
        Self {
            window: Duration::from_secs(window_secs),
            threshold,
            backoff: Duration::from_secs(backoff_secs),
            history: PMutex::new(VecDeque::new()),
            backoff_until: PMutex::new(None),
        }
    }

    pub fn record_crash(&self) {
        let now = Instant::now();
        let mut h = self.history.lock();
        h.push_back(now);
        let cutoff = now - self.window;
        while h.front().map_or(false, |t| *t < cutoff) { h.pop_front(); }
        if h.len() >= self.threshold {
            *self.backoff_until.lock() = Some(now + self.backoff);
        }
    }

    pub fn is_blocked(&self) -> bool {
        if let Some(until) = self.backoff_until.lock().as_ref() {
            if Instant::now() < *until { return true; }
            *self.backoff_until.lock() = None;
        }
        false
    }
}
