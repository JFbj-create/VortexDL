// 基于 SwiftFetch 内核的下载任务管理 (HTTP 多连接 + BT 自研引擎)

pub const VX_AUX_B: &str = "0000000000000000"; // 公开版占位（豪华版校验已移除）
// - HTTP: 新 dynamic_engine (单层 Chunk + IDM 对半切分 + 真暂停 + 动态连接调度)
// - BT (magnet/.torrent): SwiftFetch EngineContext + EngineBuilder (自研 wire 协议,
//                         已集成 pick_bt_port + tracker_concurrent_announce + DHT 兜底)
// - 事件: download-started / download-progress / download-finished (与前端契约一致)
// - 暂停/恢复/取消: 统一走 PauseController (HTTP 用 watch::channel 广播状态, BT 用 stop_notify)
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use swiftfetch::{
    format_speed, BandwidthEMA, BandwidthPoolModule, BtDownloaderModule, DownloadConfig,
    DownloadMode, DownloadResult, EngineBuilder, EngineContext, EngineEvent, HybridChunkManager,
    NATSessionGuardModule, NetworkMode, OscillationGuard, OscillationGuardModule, ProgressInfo,
    ProgressModule, ProtocolMode, RwLockContainer, SchedulerModule, SmoothScheduler, SpeedSmoother,
    SwiftFetch, TorrentMeta, calc_aligned_bt_base, pre_resolve_bt_meta, DEFAULT_GLOBAL_MAX_CONNS,
    DEFAULT_PEER_LIMIT, DEFAULT_RATIO, DEFAULT_SEED_MINUTES, MAX_CONNECTIONS_PER_HOST,
    // 新引擎: dynamic_engine + PauseController
    Chunk, ChunkPool, DownloadEngine, DownloadError as DynDownloadError,
    DynEngineState, DynamicScheduler, SpeedSmoother as DynSpeedSmoother,
    worker_main as dyn_worker_main, progress_loop as dyn_progress_loop,
    download_file as dyn_download_file, MAX_CHUNK_SIZE as DYN_MAX_CHUNK_SIZE,
    MIN_CHUNK_SIZE as DYN_MIN_CHUNK_SIZE,
    // ★ 完全动态分块 + 2核4线程智能调度: 线程数与并发流数由 SwiftFetch 统一推导
    runtime_worker_threads, streams_for_threads,
    log_engine as sf_log_engine,
};
use crate::download_engine::pause_controller::PauseController;
use tauri::{AppHandle, Emitter};
use tokio::sync::{watch, Mutex as TMutex, Notify, Semaphore};

pub type DownloadTasksMap = Arc<TMutex<HashMap<String, Arc<DownloadTask>>>>;
pub fn create_tasks_map() -> DownloadTasksMap { Arc::new(TMutex::new(HashMap::new())) }

// BT 监听端口分配 (并发任务错开, 避免端口冲突)
static BT_PORT_SEQ: AtomicU32 = AtomicU32::new(0);

pub fn is_bt_url(url: &str) -> bool {
    let u = url.trim().to_lowercase();
    u.starts_with("magnet:")
        || u.ends_with(".torrent")
        // byrut 下载页返回 .torrent 种子 (Content-Disposition: attachment)
        || (u.contains("byrutgame.org") && u.contains("do=download"))
}

/// byrut 下载页链接 (返回 .torrent 种子, 需先 HTTP 下载种子再启动 BT)
pub fn is_byrut_torrent_url(url: &str) -> bool {
    let u = url.trim().to_lowercase();
    u.contains("byrutgame.org") && u.contains("do=download")
}

/// 单个下载任务 (所有进度字段均为原子量, 供轮询与事件双通道读取)
pub struct DownloadTask {
    pub id: String,
    pub url: String,
    pub file_path: String,
    pub file_name: String,
    pub engine: String, // "speed" (HTTP) | "bt"
    pub headers: Vec<(String, String)>, // 站点自定义头 (Referer 等, 覆盖默认值)
    pub cancel_flag: Arc<AtomicBool>,
    pub handle: TMutex<Option<tokio::task::JoinHandle<()>>>,
    pub state: TMutex<String>, // starting|running|paused|completed|failed|canceled
    pub total: AtomicU64,
    pub downloaded: AtomicU64,
    pub speed_bps: AtomicU64,
    pub eta_secs: AtomicU64, // u64::MAX = 未知
    pub active_conns: AtomicU32,
    pub bt_peers: AtomicU32,
    pub bt_seeders: AtomicU32,
    /// BT 分片信息: 已完成 pieces 数 (用于前端显示 "Pieces: 120/500")
    pub bt_pieces_done: AtomicU32,
    /// BT 分片信息: 总 pieces 数
    pub bt_pieces_total: AtomicU32,
    pub error: TMutex<String>,
    pub finished_notified: AtomicBool,
    /// 真暂停/恢复/取消控制器 (HTTP 用 watch::channel 广播, BT 用 stop_notify)
    /// None: 任务尚未启动或已结束 (引擎已释放)
    pub pause_controller: TMutex<Option<Arc<PauseController>>>,
    /// ★ 授权限速器 (HTTP 任务)。存一份引用是为了让"暂停 → 换密钥 → 继续"能就地
    ///   改限速比例 —— 引擎是下载启动时一次性建好的, resume 复用旧引擎,
    ///   不重新设置的话切档完全不生效 (实测过: 切到免费档速度毫无变化)。
    pub speed_cap: TMutex<Option<Arc<swiftfetch::dynamic_engine::SpeedCap>>>,
}

impl DownloadTask {
    pub fn new(id: String, url: String, file_path: String, headers: Vec<(String, String)>) -> Self {
        let engine = if is_bt_url(&url) { "bt" } else { "speed" };
        let file_name = std::path::Path::new(&file_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        Self {
            id,
            url,
            file_path,
            file_name,
            engine: engine.into(),
            headers,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            handle: TMutex::new(None),
            state: TMutex::new("starting".into()),
            total: AtomicU64::new(0),
            downloaded: AtomicU64::new(0),
            speed_bps: AtomicU64::new(0),
            eta_secs: AtomicU64::new(u64::MAX),
            active_conns: AtomicU32::new(0),
            bt_peers: AtomicU32::new(0),
            bt_seeders: AtomicU32::new(0),
            bt_pieces_done: AtomicU32::new(0),
            bt_pieces_total: AtomicU32::new(0),
            error: TMutex::new(String::new()),
            finished_notified: AtomicBool::new(false),
            pause_controller: TMutex::new(None),
            speed_cap: TMutex::new(None),
        }
    }

    pub fn cancel(&self) { self.cancel_flag.store(true, Ordering::Relaxed); }
    pub fn is_canceled(&self) -> bool { self.cancel_flag.load(Ordering::Relaxed) }

    /// 状态快照 (轮询用, 与前端契约字段对齐)
    pub async fn status(&self) -> serde_json::Value {
        let dl = self.downloaded.load(Ordering::Relaxed);
        let total = self.total.load(Ordering::Relaxed);
        let pct = if total > 0 { (dl as f64 / total as f64 * 100.0).clamp(0.0, 100.0) } else { 0.0 };
        let eta = self.eta_secs.load(Ordering::Relaxed);
        // ★ BT 分片信息: "Pieces: 120/500" (仅 BT 任务显示)
        let bt_info_text = if self.engine == "bt" {
            let pd = self.bt_pieces_done.load(Ordering::Relaxed);
            let pt = self.bt_pieces_total.load(Ordering::Relaxed);
            if pt > 0 { format!("Pieces: {}/{}", pd, pt) } else { String::new() }
        } else { String::new() };
        serde_json::json!({
            "task_id": self.id,
            "state": *self.state.lock().await,
            "file_name": self.file_name,
            "url": self.url,
            "file_path": self.file_path,
            "progress_percent": pct,
            "downloaded": dl,
            "total": total,
            "speed_bps": self.speed_bps.load(Ordering::Relaxed),
            "speed_formatted": format_speed(self.speed_bps.load(Ordering::Relaxed)),
            "eta_secs": if eta == u64::MAX { serde_json::Value::Null } else { serde_json::json!(eta) },
            "active_connections": self.active_conns.load(Ordering::Relaxed),
            "engine": self.engine,
            "bt_peers": self.bt_peers.load(Ordering::Relaxed),
            "bt_seeders": self.bt_seeders.load(Ordering::Relaxed),
            "bt_info_text": bt_info_text,
        })
    }
}

/// 进度回调 → 原子字段 + download-progress 事件 (限频 500ms)
/// 闭包内部持有独立状态, 必须只创建一次并 move 进下载函数
fn make_progress_cb(task: &Arc<DownloadTask>, app: &AppHandle) -> impl Fn(ProgressInfo) + Send + Sync + 'static {
    let task = task.clone();
    let app = app.clone();
    let last_emit = Arc::new(std::sync::Mutex::new(std::time::Instant::now() - std::time::Duration::from_secs(1)));
    move |info: ProgressInfo| {
        task.downloaded.store(info.downloaded, Ordering::Relaxed);
        if info.total > 0 { task.total.store(info.total, Ordering::Relaxed); }
        task.speed_bps.store(info.speed_bps, Ordering::Relaxed);
        // ★ 修复 (2026-10-02): 原来 None 时写 u64::MAX, 前端拿到天文数字 → "预估时间不对".
        //   None 表示"暂时算不出"(速度未知/收尾阶段), 用 0 表达, 前端显示为占位符。
        task.eta_secs.store(info.eta_sec.unwrap_or(0), Ordering::Relaxed);
        task.active_conns.store(info.active_conns, Ordering::Relaxed);

        // ★ 关键修复 (抽搐 bug): state 字段以 task.state 为准, 不再用 info.state 覆盖
        let current_task_state = task.state.try_lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        let resolved_state = if matches!(current_task_state.as_str(),
            "completed" | "failed" | "canceled" | "paused" | "extracting") {
            current_task_state.clone()
        } else if info.state == "completed" || info.state == "failed" || info.state == "canceled" {
            info.state.clone()
        } else {
            "running".to_string()
        };
        if let Ok(mut s) = task.state.try_lock() {
            if !matches!(s.as_str(), "completed" | "failed" | "canceled" | "paused" | "extracting") {
                *s = resolved_state.clone();
            }
        }

        let total = task.total.load(Ordering::Relaxed);
        let dl = task.downloaded.load(Ordering::Relaxed);
        let pct = if total > 0 { (dl as f64 / total as f64 * 100.0).clamp(0.0, 100.0) } else { 0.0 };
        let mut le = last_emit.lock().unwrap();
        // ★ 限频 500ms (旧值 200ms + 200ms 轮询 = IPC 双写过载导致 UI 卡死)
        if le.elapsed() >= std::time::Duration::from_millis(500) {
            *le = std::time::Instant::now();
            let eta = task.eta_secs.load(Ordering::Relaxed);
            // 事件 state 字段使用 resolved_state (= task.state 优先), 避免抽搐
            let _ = app.emit("download-progress", serde_json::json!({
                "task_id": task.id,
                "state": resolved_state,
                "name": task.file_name,
                "url": task.url,
                "file_path": task.file_path,
                "progress_percent": pct,
                "downloaded": dl,
                "total": total,
                "speed_bps": info.speed_bps,
                "speed_formatted": format_speed(info.speed_bps),
                "eta_secs": if eta == u64::MAX { serde_json::Value::Null } else { serde_json::json!(eta) },
                "active_connections": info.active_conns,
                "engine": task.engine,
                "bt_peers": task.bt_peers.load(Ordering::Relaxed),
                "bt_seeders": task.bt_seeders.load(Ordering::Relaxed),
                "bt_info_text": if task.engine == "bt" {
                    let pd = task.bt_pieces_done.load(Ordering::Relaxed);
                    let pt = task.bt_pieces_total.load(Ordering::Relaxed);
                    if pt > 0 { format!("Pieces: {}/{}", pd, pt) } else { String::new() }
                } else { String::new() },
            }));
        }
    }
}

/// 启动下载 (入口): 按 URL 类型分流 HTTP / BT
pub async fn spawn_download(app: AppHandle, task: Arc<DownloadTask>) {
    let handle = if is_bt_url(&task.url) {
        tokio::spawn(run_bt_download(app, task.clone()))
    } else {
        tokio::spawn(run_http_download(app, task.clone()))
    };
    *task.handle.lock().await = Some(handle);
}

/// 用**主下载引擎**下载单个文件并等待完成 —— 模块内下载 (修改器 / MC 模组 /
/// 单机模组) 统一走这里, 于是它们也会出现在「下载」页的任务列表里,
/// 带实时速度 / 进度 / 连接数, 而不是在后台悄悄 reqwest 一把梭。
///
/// 任务会先入 `tasks` 表再启动, 前端轮询/事件两条路都能看到。
/// 取消、失败、超时都返回 Err。
pub async fn engine_download_wait(
    app: &AppHandle,
    tasks: &DownloadTasksMap,
    task_id: String,
    url: &str,
    dest: &std::path::Path,
    headers: Vec<(String, String)>,
) -> Result<(), String> {
    let task = Arc::new(DownloadTask::new(
        task_id.clone(),
        url.to_string(),
        dest.to_string_lossy().to_string(),
        headers,
    ));
    tasks.lock().await.insert(task_id.clone(), task.clone());
    spawn_download(app.clone(), task.clone()).await;

    let started = std::time::Instant::now();
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let state = task.state.lock().await.clone();
        match state.as_str() {
            "completed" => return Ok(()),
            "failed" => {
                let e = task.error.lock().await.clone();
                return Err(if e.is_empty() { "下载失败".to_string() } else { e });
            }
            "canceled" => {
                // 兜底: 引擎收尾时也会置 cancel_flag, 老代码会误报 canceled ——
                // 只要字节下全了就当成功 (和 run_http_download 里同款判断)
                let total = task.total.load(Ordering::Relaxed);
                let dl = task.downloaded.load(Ordering::Relaxed);
                if total > 0 && dl >= total {
                    return Ok(());
                }
                return Err("下载已取消".to_string());
            }
            _ => {}
        }
        // 兜底: 别让命令永远挂着
        if started.elapsed() > std::time::Duration::from_secs(7200) {
            task.cancel();
            return Err("下载超时".to_string());
        }
    }
}

/// BT 的输出是否已经可读 —— 目录里每个文件都能只读打开。
/// 引擎退出后文件句柄可能还没完全释放, 直接去解压会被 Windows 挡住。
fn bt_output_readable(p: &std::path::Path) -> bool {
    if p.is_file() {
        return std::fs::File::open(p).is_ok();
    }
    if !p.is_dir() {
        return true; // 路径还不存在, 交给后续流程处理
    }
    let mut checked = 0;
    for e in walkdir::WalkDir::new(p).into_iter().flatten() {
        if !e.file_type().is_file() {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_lowercase();
        // 引擎自己的中间文件不算
        if name.ends_with(".swiftfetch-resume") || name.ends_with(".part") || name.ends_with(".tmp") {
            continue;
        }
        // ★ 用**读写**方式打开: 只读打开在别人持有写锁时也会成功, 测不出占用。
        //   (实测: 只读检查通过后 7z 仍然报"另一个程序正在使用此文件")。
        let locked = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(e.path())
            .is_err();
        if locked {
            return false;
        }
        checked += 1;
        if checked >= 200 {
            break; // 大目录别整个走一遍
        }
    }
    true
}

fn finish_notify(app: &AppHandle, task: &DownloadTask, state: &str, error: Option<String>) {
    if task.finished_notified.swap(true, Ordering::Relaxed) { return; }
    let _ = app.emit("download-finished", serde_json::json!({
        "task_id": task.id,
        "state": state,
        "error": error.unwrap_or_default(),
        "file_path": task.file_path,
        "name": task.file_name,
    }));
}

// ============================================================
// HTTP 下载: 新 dynamic_engine (单层 Chunk + IDM 对半切分 + 真暂停 + 动态连接调度)
// 取代旧 SwiftFetch::download (HybridChunkManager + SmoothScheduler + OscillationGuard)
// ============================================================
async fn run_http_download(app: AppHandle, task: Arc<DownloadTask>) {
    let dl_start_instant = std::time::Instant::now();
    eprintln!("[DL_START] task_id={} engine=http url={} file_path={} file_name={}",
        task.id, task.url, task.file_path, task.file_name);
    crate::app_logger::log_task_state(&task.id, "queued", "starting", &format!("engine=http url={}", task.url));
    let _ = app.emit("download-started", serde_json::json!({ "task_id": task.id, "state": "started" }));
    let output = PathBuf::from(&task.file_path);
    if let Some(parent) = output.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 站点自定义头 (Referer/UA) 覆盖默认值, 防重复 append
    let mut hdr_map: std::collections::BTreeMap<String, String> =
        DownloadConfig::default_headers().into_iter().collect();
    for (k, v) in &task.headers {
        hdr_map.insert(k.clone(), v.clone());
    }
    let headers: Vec<(String, String)> = hdr_map.into_iter().collect();
    // ★ 完全动态分块 + 多线程异步调度 (2026-09-30):
    //   HTTP 下载已完全交给 dynamic_engine 的动态分块 (无静态预切分);
    //   并发流数由运行时线程数推导 (2 核 4 线程 → 64 条流), 与分块粒度同源,
    //   避免"线程少但连接数写死"导致调度与运行时能力不匹配.
    let rt_threads = runtime_worker_threads();
    let rt_streams = streams_for_threads(rt_threads);
    // ★ 授权分级 (2026-10-02 改版): 免费密钥用**占空比**限速, 不再缩放连接数。
    //
    //   原来按 0.8 缩放连接数 (64→51), 实测在真实 CDN 上完全无效:
    //   该 CDN 对单 IP 总带宽封顶 ~12~15MB/s, 51 条和 64 条撞同一个天花板,
    //   两轮实测免费档(12.74MB/s)反而比付费档(8.90MB/s)快。
    //   现在改成"每秒只给 80% 的时间收数据"(见 SwiftFetch::DutyThrottle),
    //   与源站带宽上限无关, 比例可解释。
    let speed_ratio = crate::licensing::speed_ratio();
    eprintln!("[DL_CFG] task_id={} runtime_threads={} streams={} (完全动态分块, 授权限速={:.0}%)",
        task.id, rt_threads, rt_streams, speed_ratio * 100.0);
    let cfg = DownloadConfig {
        url: task.url.clone(),
        output: Some(output),
        connections: rt_streams,
        base_chunk_size: None,
        auto_adjust: true,
        resume_enabled: true,
        proxy: None,
        headers,
        timeout_connect: std::time::Duration::from_secs(10),
        timeout_read: std::time::Duration::from_secs(180),
        timeout_request: std::time::Duration::from_secs(300),
        mirrors: Vec::new(),
        network_mode: NetworkMode::Auto,
        user_connections: None,
    };

    // 创建 PauseController 所需的 3 个组件 (与 dynamic_engine DownloadEngine 共享)
    let (state_tx, state_rx) = watch::channel(DynEngineState::Starting);
    let cancel_flag = task.cancel_flag.clone();
    let resume_notify = Arc::new(Notify::new());

    // 构造 PauseController 并存入 task (供 commands.rs pause/resume/cancel 调用)
    let pc = Arc::new(PauseController::new_http(
        state_tx.clone(),
        cancel_flag.clone(),
        resume_notify.clone(),
    ));
    *task.pause_controller.lock().await = Some(pc.clone());

    // 创建新引擎 (内部 probe 文件大小 + 初始化 ChunkPool + 打开输出文件)
    let engine_result = DownloadEngine::new(
        cfg, state_tx, state_rx, cancel_flag, resume_notify,
    ).await;
    let engine = match engine_result {
        Ok(e) => Arc::new(e.with_task_id(task.id.clone()).with_speed_ratio(speed_ratio)),
        Err(e) => {
            let msg = format!("dynamic_engine init: {}", e);
            eprintln!("[DL_FAIL] task_id={} stage=engine_init error={}", task.id, msg);
            crate::app_logger::log_task_state(&task.id, "starting", "failed", &format!("stage=engine_init error={}", msg));
            crate::app_logger::log_network("PROBE_FAIL", &format!("task_id={} url={} error={}", task.id, task.url, msg));
            *task.error.lock().await = msg.clone();
            *task.state.lock().await = "failed".into();
            finish_notify(&app, &task, "failed", Some(msg));
            return;
        }
    };
    // ★ 日志增强 (2026-09-15): 引擎创建成功后记录关键参数, 方便排查"连接中"卡死
    eprintln!("[DL_ENGINE] task_id={} file_size={} chunks={} workers={}",
        task.id, engine.pool.file_size, engine.pool.chunks_count(), engine.worker_count);
    crate::app_logger::log_task_state(&task.id, "starting", "running",
        &format!("file_size={} chunks={} workers={}", engine.pool.file_size, engine.pool.chunks_count(), engine.worker_count));
    // ★ network.log: 记录 URL 探测结果 (文件大小/分块数), 方便排查"连接中"卡死
    crate::app_logger::log_network("PROBE_OK", &format!(
        "task_id={} url={} file_size={} chunks={} workers={}",
        task.id, task.url, engine.pool.file_size, engine.pool.chunks_count(), engine.worker_count
    ));

    // ★ 把限速器存进任务, 供"暂停 → 换密钥 → 继续"时就地改比例。
    //   引擎是这里一次性建好的, resume 复用旧引擎, 不这样做切档就不生效。
    *task.speed_cap.lock().await = Some(engine.throttle.clone());

    // 进度回调 (新引擎的 progress_loop 调用)
    let cb = make_progress_cb(&task, &app);
    let progress_callback: Arc<dyn Fn(ProgressInfo) + Send + Sync> = Arc::new(move |info: ProgressInfo| {
        cb(info);
    });

    // 启动 download_file (内部 spawn N 个 worker + 1 个 progress_loop, 等待完成)
    let result = dyn_download_file(engine.clone(), progress_callback).await;

    match result {
        Ok(()) => {
            // ★ 先提取 total/downloaded, 然后立即 drop engine → 释放输出文件句柄
            //   否则前端收到 completed 事件后立即解压/删除时, 文件仍被 engine 锁定 → "权限"错误
            let total = engine.pool.file_size;
            let downloaded = engine.pool.total_downloaded();
            // ★ Bug 修复 (2026-09-13): 在 drop engine 之前先 sync_all 文件（此时句柄仍有效）,
            //   而不是 drop 之后再用 write(true) 重新打开（那是一个全新的空句柄，sync_all 毫无意义，
            //   而且 write(true) 打开会请求写权限，与杀软实时扫描冲突导致短暂锁文件 → 解压时"权限不足"）。
            //   注意: 这里用 engine.file.as_ref() 直接拿原始 Arc<File> 的同步句柄做 sync_all,
            //   SwiftFetch 内部的 File 会被所有 worker 共享, drop(engine) 后最后一个 Arc 释放句柄才关闭.
            //   但若 sync_all 对大文件耗时过长, 会触发前端停滞检测重启 → 用 try_sync_all 非阻塞版兜底
            //   (实际 Windows 下 NTFS metadata 通常 <100ms 返回). 若失败静默即可, OS cache 最终会落盘.
            let _ = engine.file.sync_all();
            drop(engine); // 显式释放所有 Arc 克隆 → 文件句柄最终关闭
            task.total.store(total, Ordering::Relaxed);
            task.downloaded.store(downloaded.max(total), Ordering::Relaxed);
            // ★ 修复文件权限问题 (2026-09-13):
            //   1) drop(engine) 后文件句柄已关闭, 但 Windows Defender 杀软会立即锁定文件扫描
            //   2) 用只读方式重开文件验证可访问性, 重试 3 次 (每次 1 秒), 确保杀软释放锁
            //   3) 验证通过后才通知前端 "completed" → 前端触发解压时文件已可正常访问
            let file_path = task.file_path.clone();
            let mut file_ready = false;
            for _ in 0..5 {
                match std::fs::File::open(&file_path) {
                    Ok(f) => { drop(f); file_ready = true; break; }
                    Err(_) => { tokio::time::sleep(std::time::Duration::from_millis(500)).await; }
                }
            }
            if !file_ready {
                eprintln!("[downloader] 警告: 文件仍被占用, 无法验证可访问性: {}", file_path);
            }
            // 检查是否被 cancel
            //
            // ★ 坑: 引擎在**正常收尾**时也会把 cancel_flag 置 true
            //   (`dynamic_engine.rs` 里 "通知 progress loop 退出" 那行), 而且用的是
            //   同一个 Arc。所以只看 is_canceled() 的话, **每一次成功下载都会被误报成
            //   canceled** (日志里能看到 downloaded == total 却 state=canceled)。
            //   前端一直靠"超量写入"的兜底把这种当成完成, 但直接轮询 state 的调用方
            //   (模块内下载走的 engine_download_wait) 就会被坑。
            //   判断依据改成"字节是否下全": 下全了就是完成, 没下全才是真取消。
            let complete = total > 0 && downloaded >= total;
            if task.is_canceled() && !complete {
                *task.state.lock().await = "canceled".into();
                eprintln!("[DL_FIN] task_id={} state=canceled total={} downloaded={} elapsed_ms={}",
                    task.id, total, downloaded, dl_start_instant.elapsed().as_millis());
                crate::app_logger::log_task_state(&task.id, "running", "canceled",
                    &format!("total={} downloaded={} elapsed_ms={}", total, downloaded, dl_start_instant.elapsed().as_millis()));
                finish_notify(&app, &task, "canceled", None);
            } else {
                *task.state.lock().await = "completed".into();
                // 文件句柄已释放 + 数据已落盘 + 杀软宽限期 → 可以安全通知前端触发解压
                eprintln!("[DL_FIN] task_id={} state=completed total={} downloaded={} elapsed_ms={}",
                    task.id, total, downloaded, dl_start_instant.elapsed().as_millis());
                crate::app_logger::log_task_state(&task.id, "running", "completed",
                    &format!("total={} downloaded={} elapsed_ms={}", total, downloaded, dl_start_instant.elapsed().as_millis()));
                finish_notify(&app, &task, "completed", None);
            }
        }
        Err(DynDownloadError::Canceled) => {
            *task.state.lock().await = "canceled".into();
            eprintln!("[DL_FIN] task_id={} state=canceled error=Canceled elapsed_ms={}",
                task.id, dl_start_instant.elapsed().as_millis());
            crate::app_logger::log_task_state(&task.id, "running", "canceled",
                &format!("error=Canceled elapsed_ms={}", dl_start_instant.elapsed().as_millis()));
            finish_notify(&app, &task, "canceled", None);
        }
        Err(e) => {
            let msg = format!("{}", e);
            sf_log_engine(&format!("[downloader] HTTP 下载失败 task_id={} error={}", task.id, msg));
            eprintln!("[DL_FIN] task_id={} state=failed error={} elapsed_ms={}",
                task.id, msg, dl_start_instant.elapsed().as_millis());
            crate::app_logger::log_task_state(&task.id, "running", "failed",
                &format!("error={} elapsed_ms={}", msg, dl_start_instant.elapsed().as_millis()));
            *task.error.lock().await = msg.clone();
            *task.state.lock().await = "failed".into();
            finish_notify(&app, &task, "failed", Some(msg));
        }
    }
}

async fn watch_cancel(task: Arc<DownloadTask>) {
    loop {
        if task.is_canceled() { return; }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

// ============================================================
// BT 下载: SwiftFetch EngineContext + 自研 BT 引擎
// (移植自 SwiftFetch CLI run_with_progress_bar 的 BtOnly 分支)
// ============================================================
async fn run_bt_download(app: AppHandle, task: Arc<DownloadTask>) {
    use parking_lot::Mutex as PMutex;
    use parking_lot::RwLock as PRwLock;
    use std::sync::atomic::AtomicU64 as A64;
    use std::sync::atomic::AtomicI32 as A32;

    let _ = app.emit("download-started", serde_json::json!({ "task_id": task.id, "state": "started" }));
    let start_instant = std::time::Instant::now();
    eprintln!("[DL_START] task_id={} engine=bt url={} file_path={}", task.id, task.url, task.file_path);
    crate::app_logger::log_task_state(&task.id, "queued", "starting", &format!("engine=bt url={}", task.url));

    let url = task.url.trim().to_lowercase();
    let magnet = if url.starts_with("magnet:") { Some(task.url.clone()) } else { None };
    let torrent = if url.ends_with(".torrent") { Some(PathBuf::from(&task.url)) } else { None };

    // BT 输出目录: file_path 作为下载内容目录
    let out_dir = PathBuf::from(&task.file_path);
    let _ = std::fs::create_dir_all(&out_dir);

    let bt_port = 6881u16 + (BT_PORT_SEQ.fetch_add(1, Ordering::Relaxed) % 200) as u16;

    // 预解析 meta (magnet 经 DHT 拿 info; .torrent 直接读) → 真实 total_size 对齐 chunk
    let meta_res: anyhow::Result<TorrentMeta> = pre_resolve_bt_meta(torrent.as_deref(), magnet.as_deref()).await;
    let (base_chunk_size, actual_fs) = match &meta_res {
        Ok(tm) => (calc_aligned_bt_base(tm), tm.total_size),
        Err(_) => (1024 * 1024u64, 1024u64 * 1024),
    };
    task.total.store(actual_fs, Ordering::Relaxed);
    let mgr = Arc::new(HybridChunkManager::new(actual_fs, base_chunk_size));
    let downloaded = Arc::new(A64::new(0));

    let (event_tx, _event_rx) = flume::unbounded::<EngineEvent>();
    let (stop_tx, stop_rx) = flume::bounded::<()>(1);
    let stop_notify = Arc::new(tokio::sync::Notify::new());

    let net_mode = NetworkMode::Auto;
    // ★ qBittorrent/BitComet 式优化 (2026-09-08): 使用 modules 中提升后的 DEFAULT_PEER_LIMIT (200)
    //   原 64 peer 太少, qBittorrent 默认 100-200, BitComet 默认 200+
    // ★ 授权分级 (2026-10-02): BT 的"并发"是 peer 数, 同样按比例缩放 →
    //   免费密钥的 BT 下载速度也约为原来的 80%。
    let bt_speed_ratio = crate::licensing::speed_ratio();
    let bt_peer_limit_val = ((DEFAULT_PEER_LIMIT as f64) * bt_speed_ratio).round().max(16.0) as u32;
    // ★ 全局连接 96 → 500 (modules 中已提升)
    let global_max_conns_val = DEFAULT_GLOBAL_MAX_CONNS;
    let seed_minutes = DEFAULT_SEED_MINUTES;

    let cfg = DownloadConfig {
        url: String::new(), // BtOnly: 无 HTTP 链接
        output: Some(out_dir.clone()),
        connections: 16,
        base_chunk_size: None,
        auto_adjust: true,
        resume_enabled: true,
        proxy: None,
        headers: DownloadConfig::default_headers(),
        timeout_connect: std::time::Duration::from_secs(10),
        timeout_read: std::time::Duration::from_secs(180),
        timeout_request: std::time::Duration::from_secs(300),
        mirrors: Vec::new(),
        network_mode: net_mode,
        user_connections: None,
    };

    let ctx = Arc::new(EngineContext {
        config: cfg.clone(),
        protocol: ProtocolMode::BtOnly,
        network_mode: net_mode,
        download_mode: DownloadMode::SparseRareFirst,
        probe: RwLockContainer::new(None),
        output_path: out_dir.clone(),
        file_size: A64::new(actual_fs),
        base_chunk_size: A64::new(base_chunk_size),
        chunk_mgr: mgr,
        downloaded: downloaded.clone(),
        http_downloaded: A64::new(0),
        bt_downloaded: A64::new(0),
        file: Arc::new(TMutex::new(None)),
        active_http_conns: AtomicU32::new(0),
        active_bt_conns: AtomicU32::new(0),
        http_conn_limit: AtomicU32::new(MAX_CONNECTIONS_PER_HOST),
        bt_peer_limit: AtomicU32::new(bt_peer_limit_val),
        global_max_conns: AtomicU32::new(global_max_conns_val),
        sem_http: Arc::new(Semaphore::new(MAX_CONNECTIONS_PER_HOST as usize)),
        sem_bt: Arc::new(Semaphore::new(bt_peer_limit_val as usize)),
        bandwidth_ema: Arc::new(BandwidthEMA::new()),
        event_tx: event_tx.clone(),
        event_rx: flume::unbounded::<EngineEvent>().1,
        stop_notify: stop_notify.clone(),
        stop_event_tx: stop_tx.clone(),
        stop_event_rx: stop_rx.clone(),
        scheduler: PMutex::new(SmoothScheduler::new(16, 10_000_000, base_chunk_size)),
        speed_smoother: PMutex::new(SpeedSmoother::new()),
        bt_ema_speed: A64::new(0),
        oscillation_guard: PMutex::new(OscillationGuard::new()),
        base_chunk_done: PMutex::new(Vec::new()),
        bt_piece_map_completed: PMutex::new(Vec::new()),
        bt_blocks_done: swiftfetch::modules::new_sharded_block_set(),
        bt_blocks_inflight: swiftfetch::modules::new_sharded_block_set(),
        bt_piece_block_counts: PMutex::new(Vec::new()),
        bt_piece_size: A64::new(256 * 1024),
        bt_request_block: A64::new(swiftfetch::modules::choose_bt_request_block(256 * 1024)),
        bt_total_pieces: AtomicU32::new(0),
        peer_scores: PMutex::new(HashMap::new()),
        bt_seeders: AtomicU32::new(0),
        bt_peers: AtomicU32::new(0),
        http_weight: A64::new(1000),
        bt_weight: A64::new(1000),
        http_ratio_target: A64::new(0.6f64.to_bits()),
        bt_ratio_target: A64::new(0.4f64.to_bits()),
        last_reset_count: AtomicU32::new(0),
        last_reset_window: parking_lot::RwLock::new(std::collections::VecDeque::new()),
        conn_delay_ms: A64::new(0),
        completed_time_series: PMutex::new(Vec::new()),
        prefetch_warmed: PMutex::new(HashMap::new()),
        slow_subchunks: PMutex::new(HashMap::new()),
        mirrors: Vec::new(),
        peer_port: AtomicU32::new(bt_port as u32),
        ratio_target: A64::new(DEFAULT_RATIO.to_bits()),
        seed_minutes: AtomicU32::new(seed_minutes),
        task_id: task.id.clone(),
        start_instant,
        no_cross_protocol: false,
        bt_dht_node_id: parking_lot::RwLock::new(None),
        bt_listen_port: AtomicU32::new(bt_port as u32),
        bt_incoming_listener: PMutex::new(None),
        bt_file_handles: PMutex::new(HashMap::new()),
        bt_have_txs: PMutex::new(Vec::new()),
        bt_uploaded: A64::new(0),
        bt_upload_requests: A64::new(0),
        bt_unchoke_sent: A64::new(0),
        bt_conn_tcp_ok: A64::new(0),
        bt_conn_utp_ok: A64::new(0),
        bt_conn_fail: A64::new(0),
        bt_unchoked_now: A32::new(0),
        bt_pex_tx: PMutex::new(None),
        bt_endgame_mode: std::sync::atomic::AtomicBool::new(false),
        bt_piece_availability: PMutex::new(Vec::new()),
        bt_piece_verify_fails: PMutex::new(HashMap::new()),
        bt_live_peers: PMutex::new(None),
        bt_dup_blocks: A64::new(0),
        bt_total_received: A64::new(0),
    });

    // 构造 PauseController 并存入 task (BT 模式: stop_notify + cancel_flag)
    let pc_bt = Arc::new(PauseController::new_bt(
        ctx.stop_notify.clone(),
        task.cancel_flag.clone(),
    ));
    *task.pause_controller.lock().await = Some(pc_bt);

    // 侧轮询: 把 BT peers/seeders/pieces 快照到任务 (事件与轮询双通道可见)
    {
        let ctx_p = ctx.clone();
        let task_p = task.clone();
        let stop_n = stop_notify.clone();
        tokio::spawn(async move {
            loop {
                task_p.bt_peers.store(ctx_p.bt_peers.load(Ordering::Relaxed), Ordering::Relaxed);
                task_p.bt_seeders.store(ctx_p.bt_seeders.load(Ordering::Relaxed), Ordering::Relaxed);
                // ★ 分片信息: 已完成 pieces / 总 pieces (用于前端进度条显示)
                let pieces_done = ctx_p.bt_piece_map_completed.lock().len() as u32;
                task_p.bt_pieces_done.store(pieces_done, Ordering::Relaxed);
                task_p.bt_pieces_total.store(ctx_p.bt_total_pieces.load(Ordering::Relaxed), Ordering::Relaxed);
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
                    _ = stop_n.notified() => break,
                }
            }
        });
    }

    // 进度回调 (ProgressModule 250ms 节拍驱动; 只创建一次保持内部限频状态)
    let prog_inner = make_progress_cb(&task, &app);
    let prog_cb: Arc<dyn Fn(ProgressInfo) + Send + Sync> = Arc::new(move |info: ProgressInfo| {
        prog_inner(info);
    });

    let builder = EngineBuilder::new()
        .register(BtDownloaderModule::new(None, magnet.clone(), torrent.clone(), bt_port))
        .register_arc(Arc::new(ProgressModule { callback: prog_cb }))
        .register(SchedulerModule)
        .register(BandwidthPoolModule)
        .register(NATSessionGuardModule)
        .register(OscillationGuardModule);

    let run_task = task.clone();
    let run_app = app.clone();
    let run_ctx = ctx.clone();
    let run_res = tokio::select! {
        r = builder.run_all(run_ctx) => r,
        _ = watch_cancel(task.clone()) => {
            let _ = ctx.stop_notify.notify_waiters();
            let _ = ctx.stop_event_tx.send(());
            *run_task.state.lock().await = "canceled".into();
            finish_notify(&run_app, &run_task, "canceled", None);
            return;
        }
    };

    let total_dl = downloaded.load(Ordering::Relaxed);
    task.downloaded.store(total_dl, Ordering::Relaxed);
    match run_res {
        Ok(()) if total_dl > 0 || actual_fs == 0 => {
            task.downloaded.store(actual_fs.max(total_dl), Ordering::Relaxed);
            // ★★ 修复 (2026-10-06): BT 完成后**必须等文件句柄真正释放**再通知前端。
            //    HTTP 路径早就有这一步 (见上面那段"文件权限问题"), BT 路径漏了 ——
            //    结果前端一收到 completed 立刻去解压, 7z 直接报
            //    "另一个程序正在使用此文件，进程无法访问"。
            let out_path = std::path::PathBuf::from(&task.file_path);
            let mut ready = false;
            for _ in 0..12 {
                if bt_output_readable(&out_path) { ready = true; break; }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            if !ready {
                eprintln!("[DL_FIN] 警告: BT 输出仍被占用, 无法验证可访问性: {}", task.file_path);
            }
            *task.state.lock().await = "completed".into();
            eprintln!("[DL_FIN] task_id={} engine=bt state=completed total={} downloaded={} elapsed_ms={}",
                task.id, actual_fs, total_dl, start_instant.elapsed().as_millis());
            crate::app_logger::log_task_state(&task.id, "running", "completed",
                &format!("engine=bt total={} downloaded={} elapsed_ms={}", actual_fs, total_dl, start_instant.elapsed().as_millis()));
            finish_notify(&app, &task, "completed", None);
        }
        Ok(()) => {
            let msg = format!("BT 下载未获取到任何数据 (file_size={})", actual_fs);
            eprintln!("[DL_FIN] task_id={} engine=bt state=failed error={} elapsed_ms={}",
                task.id, msg, start_instant.elapsed().as_millis());
            crate::app_logger::log_task_state(&task.id, "running", "failed",
                &format!("engine=bt error={} elapsed_ms={}", msg, start_instant.elapsed().as_millis()));
            *task.error.lock().await = msg.clone();
            *task.state.lock().await = "failed".into();
            finish_notify(&app, &task, "failed", Some(msg));
        }
        Err(e) => {
            let msg = format!("{:#}", e);
            eprintln!("[DL_FIN] task_id={} engine=bt state=failed error={} elapsed_ms={}",
                task.id, msg, start_instant.elapsed().as_millis());
            crate::app_logger::log_task_state(&task.id, "running", "failed",
                &format!("engine=bt error={} elapsed_ms={}", msg, start_instant.elapsed().as_millis()));
            *task.error.lock().await = msg.clone();
            *task.state.lock().await = "failed".into();
            finish_notify(&app, &task, "failed", Some(msg));
        }
    }
}
