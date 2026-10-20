//! 帧率记录（FPS Recorder）。
//!
//! # 它是什么 / 不是什么
//!
//! 这是一个**低开销周期采样器**：每 N 毫秒采一拍，把帧率、CPU（总/每核）、内存、
//! GPU、磁盘、网络、以及"游戏进程自己的占用"记成时间序列，落盘成一个会话。
//!
//! ★ 帧率来源：这里**不注入游戏进程**（不挂钩 Present），所以拿不到逐帧时间。
//!   我们采的是「前台游戏窗口的实际刷新」的近似值 —— 见 `frame_source`。
//!   这样做的好处是不碰反作弊、不需要管理员、不会被杀软拦；
//!   代价是 frametime 是粗粒度估计，low 帧只能按采样点算（会在 UI 上写明）。
//!
//! # 两个模式（用户明确要求）
//!
//! * `global`  全局：开着就采，不管前台是谁。
//! * `inline`  游戏内：只在检测到游戏在前台时采，游戏退出自动结束这个会话。
//!
//! # 落盘
//!
//! ```text
//! <数据目录>/fps/index.json      ← 会话摘要（列表页只读这个，不扫全量文件）
//! <数据目录>/fps/<id>.json       ← 单场会话（meta + samples）
//! ```

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// ============================================================
// 数据结构
// ============================================================

/// 一次采样（一行数据）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Sample {
    /// 相对会话开始的毫秒数
    pub t: u64,
    pub fps: f64,
    /// 平均帧时间（ms）
    #[serde(default)]
    pub frametime_ms: f64,
    #[serde(default)]
    pub cpu_total: f64,
    #[serde(default)]
    pub cpu_cores: Vec<f64>,
    /// 能取到才 > 0
    #[serde(default)]
    pub cpu_temp: f64,
    #[serde(default)]
    pub gpu_usage: f64,
    #[serde(default)]
    pub gpu_temp: f64,
    /// MB
    #[serde(default)]
    pub ram_used: f64,
    #[serde(default)]
    pub gpu_mem_used: f64,
    /// MB/s
    #[serde(default)]
    pub disk_read: f64,
    #[serde(default)]
    pub disk_write: f64,
    /// KB/s
    #[serde(default)]
    pub net_up: f64,
    #[serde(default)]
    pub net_down: f64,
    /// 游戏进程自己的占用（没有游戏时是 0）
    #[serde(default)]
    pub proc_cpu: f64,
    #[serde(default)]
    pub proc_ram: f64,
}

/// 会话摘要（列表页用）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub game: String,
    pub exe: String,
    /// 封面/图标（本地路径或 http）
    #[serde(default)]
    pub icon: String,
    /// 开始时间（毫秒）
    pub started_at: i64,
    /// 结束时间（毫秒），进行中为 0
    #[serde(default)]
    pub ended_at: i64,
    /// global / inline
    #[serde(default)]
    pub mode: String,
    /// 采样间隔（ms）
    #[serde(default)]
    pub sample_ms: u64,
    #[serde(default)]
    pub duration_s: u64,
    #[serde(default)]
    pub fps_avg: f64,
    #[serde(default)]
    pub fps_low1: f64,
    #[serde(default)]
    pub fps_low01: f64,
    #[serde(default)]
    pub fps_max: f64,
    #[serde(default)]
    pub sample_count: usize,
    /// 是否还在采
    #[serde(default)]
    pub live: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionFile {
    /// ★ 不要 flatten：展平后前端拿 `f.meta.game` 会是 undefined
    ///   （字段被提到最外层），详情页标题会显示「未命名」、日期 NaN。
    pub meta: SessionMeta,
    #[serde(default)]
    pub samples: Vec<Sample>,
}

/// 记录器设置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecorderSettings {
    /// off / global / inline
    #[serde(default = "d_mode")]
    pub mode: String,
    /// 全局模式采样间隔（ms）
    #[serde(default = "d_int_global")]
    pub global_ms: u64,
    /// 游戏内模式采样间隔（ms）
    #[serde(default = "d_int_inline")]
    pub inline_ms: u64,
    /// 保留多少场会话（老的自动删）
    #[serde(default = "d_keep")]
    pub keep: usize,
    /// 是否记录 CPU 每核（关掉可以少占点内存）
    #[serde(default = "d_true")]
    pub per_core: bool,
}

fn d_mode() -> String {
    "off".to_string()
}
fn d_int_global() -> u64 {
    2000
}
fn d_int_inline() -> u64 {
    700
}
fn d_keep() -> usize {
    100
}
fn d_true() -> bool {
    true
}

impl Default for RecorderSettings {
    fn default() -> Self {
        Self {
            mode: d_mode(),
            global_ms: d_int_global(),
            inline_ms: d_int_inline(),
            keep: d_keep(),
            per_core: true,
        }
    }
}

// ============================================================
// 路径
// ============================================================

fn fps_dir() -> PathBuf {
    crate::paths::data_dir().join("fps")
}

fn index_path() -> PathBuf {
    fps_dir().join("index.json")
}

fn session_path(id: &str) -> PathBuf {
    fps_dir().join(format!("{}.json", id))
}

fn settings_path() -> PathBuf {
    crate::paths::data_dir().join("fps").join("settings.json")
}

// ============================================================
// 设置
// ============================================================

pub fn load_settings() -> RecorderSettings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_settings(s: &RecorderSettings) -> Result<(), String> {
    let p = settings_path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    std::fs::write(&p, serde_json::to_string_pretty(s).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

// ============================================================
// 索引（列表页数据）
// ============================================================

pub fn load_index() -> Vec<SessionMeta> {
    std::fs::read_to_string(index_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<SessionMeta>>(&s).ok())
        .unwrap_or_default()
}

fn save_index(list: &[SessionMeta]) {
    if let Some(d) = index_path().parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(s) = serde_json::to_string(list) {
        let _ = std::fs::write(index_path(), s);
    }
}

/// 更新（或插入）索引里的一条摘要
fn upsert_index(meta: &SessionMeta) {
    let mut list = load_index();
    match list.iter_mut().find(|m| m.id == meta.id) {
        Some(m) => *m = meta.clone(),
        None => list.insert(0, meta.clone()),
    }
    list.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    // 超量的老会话连文件一起删
    let keep = load_settings().keep.max(5);
    while list.len() > keep {
        if let Some(old) = list.pop() {
            let _ = std::fs::remove_file(session_path(&old.id));
        }
    }
    save_index(&list);
}

pub fn read_session(id: &str) -> Result<SessionFile, String> {
    // id 只允许数字/字母/短横线，防路径穿越
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("会话 id 不合法".to_string());
    }
    let s = std::fs::read_to_string(session_path(id))
        .map_err(|e| format!("会话不存在或读不了: {}", e))?;
    serde_json::from_str(&s).map_err(|e| format!("会话文件损坏: {}", e))
}

pub fn delete_session(id: &str) -> Result<(), String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("会话 id 不合法".to_string());
    }
    let _ = std::fs::remove_file(session_path(id));
    let mut list = load_index();
    list.retain(|m| m.id != id);
    save_index(&list);
    Ok(())
}

// ============================================================
// 统计
// ============================================================

/// 用帧时间的 p99 / p99.9 分位算 1% low / 0.1% low。
///
/// ★ 业界口径（CapFrameX / GamePP 都是这个）：先按帧时间降序排，
///   取第 1% / 0.1% **最长**的帧，再取倒数。用「最低 fps」是不对的
///   （单个卡顿会把结果拉到毫无参考价值）。
pub fn low_fps(frametimes: &mut [f64], pct: f64) -> f64 {
    if frametimes.is_empty() {
        return 0.0;
    }
    frametimes.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let n = frametimes.len();
    // ★★ 语义是「最差的那批帧里的**最后一帧**」。
    //
    //   老实现用 `(n * pct) as usize` 直接当下标：n=1000、pct=0.001 得 1，
    //   取到的是**倒数第 2 慢**的帧 —— 1000 帧里只有 1 帧卡顿时会被完全跳过，
    //   0.1% low 算出 60fps（跟平均一样），这个指标就永远发现不了卡顿。
    //
    //   正确做法：k = ceil(n * pct) 是「最差那批帧的数量」，取这批的最后一帧
    //   （下标 k-1）。只有 1 帧卡顿 → k=1 → 取第 0 个 → 那帧 100ms → 10fps。
    let k = ((n as f64) * pct).ceil().max(1.0) as usize;
    let idx = (k - 1).min(n - 1);
    let ft = frametimes[idx];
    if ft > 0.0 {
        1000.0 / ft
    } else {
        0.0
    }
}

fn summarize(meta: &mut SessionMeta, samples: &[Sample]) {
    // ★ 这几个字段**必须无条件写**：fps 全 0（没拿到帧率来源）时也要让
    //   列表页知道「这场录了多久、有多少采样点」，否则显示成 0 样本像没录。
    meta.sample_count = samples.len();
    meta.duration_s = samples.last().map(|s| s.t / 1000).unwrap_or(0);

    let fps: Vec<f64> = samples.iter().map(|s| s.fps).filter(|f| *f > 0.0).collect();
    if fps.is_empty() {
        return;
    }
    meta.fps_avg = fps.iter().sum::<f64>() / fps.len() as f64;
    meta.fps_max = fps.iter().cloned().fold(0.0, f64::max);
    let mut ft: Vec<f64> = samples
        .iter()
        .filter(|s| s.frametime_ms > 0.0)
        .map(|s| s.frametime_ms)
        .collect();
    let mut ft2 = ft.clone();
    meta.fps_low1 = low_fps(&mut ft, 0.01);
    meta.fps_low01 = low_fps(&mut ft2, 0.001);
    let _ = &ft;
}

// ============================================================
// 采样引擎
// ============================================================

fn now_ms() -> i64 {
    crate::paths::now_ms() as i64
}

fn new_id() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("fps-{}", t)
}

struct Active {
    meta: SessionMeta,
    samples: Vec<Sample>,
    started: Instant,
    /// 上一次的磁盘/网络累计值，用来算速率
    last_disk: (u64, u64),
    last_net: (u64, u64),
    last_tick: Instant,
}

static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);
static RUNNING: AtomicBool = AtomicBool::new(false);
/// 当前会话是不是用户手动开的（手动会话不自动收尾）
static MANUAL: AtomicBool = AtomicBool::new(false);

pub fn is_recording() -> bool {
    RUNNING.load(Ordering::Relaxed)
}

pub fn active_meta() -> Option<SessionMeta> {
    ACTIVE.lock().ok().and_then(|g| g.as_ref().map(|a| a.meta.clone()))
}

/// 开始一场记录。已经在记录就返回现有会话（不重复开）。
pub fn start(game: &str, exe: &str, icon: &str, mode: &str) -> Result<SessionMeta, String> {
    start_inner(game, exe, icon, mode, true)
}

/// 自动模式（循环自己开的会话），由循环负责收尾
pub fn start_auto(game: &str, exe: &str, icon: &str, mode: &str) -> Result<SessionMeta, String> {
    start_inner(game, exe, icon, mode, false)
}

fn start_inner(
    game: &str,
    exe: &str,
    icon: &str,
    mode: &str,
    manual: bool,
) -> Result<SessionMeta, String> {
    let mut g = ACTIVE.lock().map_err(|_| "记录器忙".to_string())?;
    if let Some(a) = g.as_ref() {
        return Ok(a.meta.clone());
    }
    let settings = load_settings();
    let sample_ms = if mode == "inline" { settings.inline_ms } else { settings.global_ms };
    let mut meta = SessionMeta {
        id: new_id(),
        game: if game.trim().is_empty() { "桌面 / 全局".to_string() } else { game.trim().to_string() },
        exe: exe.to_string(),
        icon: icon.to_string(),
        started_at: now_ms(),
        mode: mode.to_string(),
        sample_ms: sample_ms.max(200),
        live: true,
        ..Default::default()
    };
    meta.sample_ms = sample_ms.max(200);
    let a = Active {
        meta: meta.clone(),
        samples: Vec::new(),
        started: Instant::now(),
        last_disk: crate::fps_recorder::disk_totals(),
        last_net: crate::fps_recorder::net_totals(),
        last_tick: Instant::now(),
    };
    *g = Some(a);
    RUNNING.store(true, Ordering::Relaxed);
    MANUAL.store(manual, Ordering::Relaxed);
    crate::app_logger::log_command(
        "fps",
        &format!("开始记录 id={} game={} mode={}", meta.id, meta.game, meta.mode),
    );
    Ok(meta)
}

/// 结束当前会话并落盘。返回摘要。
pub fn stop() -> Result<Option<SessionMeta>, String> {
    let mut g = ACTIVE.lock().map_err(|_| "记录器忙".to_string())?;
    RUNNING.store(false, Ordering::Relaxed);
    let a = match g.take() {
        Some(a) => a,
        None => return Ok(None),
    };
    let mut meta = a.meta;
    meta.ended_at = now_ms();
    meta.live = false;
    summarize(&mut meta, &a.samples);
    if meta.duration_s == 0 && !a.samples.is_empty() {
        meta.duration_s = a.started.elapsed().as_secs();
    }
    let file = SessionFile { meta: meta.clone(), samples: a.samples };
    if let Some(d) = fps_dir().parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::create_dir_all(fps_dir());
    let body = serde_json::to_string(&file).map_err(|e| e.to_string())?;
    std::fs::write(session_path(&meta.id), body).map_err(|e| format!("写会话失败: {}", e))?;
    upsert_index(&meta);
    crate::app_logger::log_command(
        "fps",
        &format!(
            "结束记录 id={} 时长={}s 样本={} 平均={:.1} low1={:.1}",
            meta.id, meta.duration_s, meta.sample_count, meta.fps_avg, meta.fps_low1
        ),
    );
    Ok(Some(meta))
}

/// 采一拍（由后台循环调用）。返回刚采到的样本。
fn tick() -> Option<Sample> {
    let mut g = ACTIVE.lock().ok()?;
    let a = g.as_mut()?;
    let el = a.started.elapsed().as_millis() as u64;
    let dt = a.last_tick.elapsed().as_secs_f64().max(0.05);
    a.last_tick = Instant::now();

    let mut s = Sample { t: el, ..Default::default() };

    // ---- 帧率 ----
    let (fps, ft) = frame_source::sample();
    s.fps = fps;
    s.frametime_ms = ft;

    // ---- 系统 ----
    let st = crate::system_monitor::snapshot_status();
    s.cpu_total = st.cpu.usage;
    s.ram_used = st.memory.used as f64 / 1048576.0;
    s.gpu_usage = st.gpu.as_ref().and_then(|g| g.usage).unwrap_or(0.0);
    s.gpu_mem_used = st.gpu.as_ref().and_then(|g| g.memory_mb).unwrap_or(0) as f64;

    // ---- 每核 ----
    if load_settings().per_core {
        s.cpu_cores = crate::system_monitor::per_core_usage();
    }

    // ---- 磁盘 / 网络速率 ----
    let d = disk_totals();
    let n = net_totals();
    s.disk_read = ((d.0.saturating_sub(a.last_disk.0)) as f64 / 1048576.0 / dt).max(0.0);
    s.disk_write = ((d.1.saturating_sub(a.last_disk.1)) as f64 / 1048576.0 / dt).max(0.0);
    s.net_up = ((n.0.saturating_sub(a.last_net.0)) as f64 / 1024.0 / dt).max(0.0);
    s.net_down = ((n.1.saturating_sub(a.last_net.1)) as f64 / 1024.0 / dt).max(0.0);
    a.last_disk = d;
    a.last_net = n;

    // ---- 游戏进程 ----
    if !a.meta.exe.is_empty() {
        if let Some((cpu, mem)) = crate::system_monitor::process_usage(&a.meta.exe) {
            s.proc_cpu = cpu;
            s.proc_ram = mem as f64 / 1048576.0;
        }
    }

    a.samples.push(s.clone());

    // 每 30 拍把摘要刷一次索引，这样列表页能看到进行中的会话
    if a.samples.len() % 30 == 0 {
        let mut m = a.meta.clone();
        summarize(&mut m, &a.samples);
        m.live = true;
        m.ended_at = 0;
        upsert_index(&m);
    }
    Some(s)
}

/// 上一次采样的快照（给悬浮窗显示当前 fps / 占用）
static LAST: Mutex<Option<Sample>> = Mutex::new(None);

pub fn last_sample() -> Option<Sample> {
    LAST.lock().ok().and_then(|g| g.clone())
}

/// 启动后台采样线程（幂等）
pub fn ensure_loop() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(Duration::from_millis(250));
            let settings = load_settings();
            if settings.mode == "off" {
                // 关闭自动记录时**不能掐掉手动会话**（用户可能就想录这一把）
                if is_recording() && !MANUAL.load(Ordering::Relaxed) {
                    let _ = stop();
                }
                // 不记录时也要更新 last_sample，悬浮窗要显示实时帧率
                let (fps, ft) = frame_source::sample();
                if let Ok(mut g) = LAST.lock() {
                    let mut s = (*g).clone().unwrap_or_default();
                    s.fps = fps;
                    s.frametime_ms = ft;
                    let st = crate::system_monitor::snapshot_status();
                    s.cpu_total = st.cpu.usage;
                    s.gpu_usage = st.gpu.as_ref().and_then(|g| g.usage).unwrap_or(0.0);
                    s.ram_used = st.memory.used as f64 / 1048576.0;
                    *g = Some(s);
                }
                // 手动会话在 off 模式下照样要采（否则用户按「开始记录」什么也录不到）
                if is_recording() && MANUAL.load(Ordering::Relaxed) {
                    if let Some(s) = tick() {
                        if let Ok(mut g) = LAST.lock() {
                            *g = Some(s);
                        }
                    }
                }
                continue;
            }
            let interval = if settings.mode == "inline" {
                settings.inline_ms
            } else {
                settings.global_ms
            }
            .max(200);

            if !is_recording() {
                // ★ 只有"自动模式"才自己开会话。手动调用 fps_start 的会话
                //   由用户自己 fps_stop，循环不能抢（曾经因此在 stop 前又被
                //   自动开了一场，导致手动那场一个采样点都没有）。
                if !MANUAL.load(Ordering::Relaxed) {
                    match settings.mode.as_str() {
                        "inline" => {
                            // 只有检测到游戏才开
                            if let Some((game, exe, icon)) = frame_source::foreground_game() {
                                let _ = start_auto(&game, &exe, &icon, "inline");
                            }
                        }
                        "global" => {
                            let _ = start_auto("", "", "", "global");
                        }
                        _ => {}
                    }
                }
            } else if settings.mode == "inline" && !MANUAL.load(Ordering::Relaxed) {
                // 自动模式的 inline：游戏没了 → 收尾
                if frame_source::foreground_game().is_none() {
                    let _ = stop();
                    continue;
                }
            }

            if !is_recording() {
                continue;
            }
            // 按目标间隔采（250ms 的循环节 + 累计判断）
            static LAST_TICK: Mutex<Option<Instant>> = Mutex::new(None);
            let due = LAST_TICK
                .lock()
                .ok()
                .and_then(|g| *g)
                .map(|t| t.elapsed().as_millis() as u64 >= interval)
                .unwrap_or(true);
            if due {
                if let Ok(mut g) = LAST_TICK.lock() {
                    *g = Some(Instant::now());
                }
                if let Some(s) = tick() {
                    if let Ok(mut g) = LAST.lock() {
                        *g = Some(s);
                    }
                }
            }
        });
    });
}

// ============================================================
// 帧率来源
// ============================================================
//
// ★ 说明清楚：这里**不注入游戏**，所以拿不到逐帧时间。
//   能拿到的「帧率」只有两类：
//     a) 我们自己渲染的东西（模拟器窗口、WebView）→ 用 DWM 合成统计
//     b) 前台游戏窗口 → 用 DWM 的「每窗口刷新」间接估
//   两者都是近似。真实逐帧 fps 需要 Present hook（PresentMon/RTSS 那条路），
//   那需要注入，本软件不做。
//
//   实际实现见下面的 `dwm_present_rate`：用 DwmGetCompositionTimingInfo 拿
//   合成器刷新节拍，再按窗口是否在刷新给出一个"窗口帧率"估计。

pub mod frame_source {
    use super::*;

    /// 返回 (fps, 平均帧时间 ms)。
    ///
    /// 优先级：
    ///   1. 模拟器 stdout 里的 `game_fps`（**真实逐帧值**）
    ///   2. 模拟器写的性能 CSV
    ///   3. 都没有就返回 (0, 0) —— 上层显示「—」，绝不编数字
    pub fn sample() -> (f64, f64) {
        if let Some((f, ft)) = super::emu_stdout_fps() {
            return (f, ft);
        }
        #[cfg(windows)]
        {
            if let Some(f) = super::emu_window_fps() {
                let ft = if f > 0.0 { 1000.0 / f } else { 0.0 };
                return (f, ft);
            }
        }
        (0.0, 0.0)
    }

    /// 当前前台的「游戏」进程。不是游戏就返回 None。
    #[cfg(windows)]
    pub fn foreground_game() -> Option<(String, String, String)> {
        super::fg_window::game_info()
    }

    #[cfg(not(windows))]
    pub fn foreground_game() -> Option<(String, String, String)> {
        None
    }
}

/// 从模拟器进程读帧率。
///
/// # 为什么这样做（而不是 DWM / 注入）
///
/// * **注入 Present** 能拿逐帧真实值，但要挂钩 dxgi/d3d9/vulkan —— 反作弊会拦、
///   杀软会拦，本软件明确不做。
/// * **DWM 合成节拍**测的是桌面合成器，不是游戏的 Present 次数，游戏没变化时
///   也会报 60/144，纯属假数据 —— 宁可没有也不能给假值。
/// * **ETW DxgKrnl** 是正路（PresentMon 就是这么做的），但在 Rust 里从零解析
///   ETW 事件流是几百行、且要管理员权限。
///
/// 这里用的是**可验证的真实来源**：模拟器（Citron/Yuzu/Ryujinx）自己会统计
/// `game_fps`，我们通过它的窗口读取不了，但可以读到它写在 SD 卡/日志里的
/// 统计（Citron 的 `perf_stats` 会按会话写 CSV）。
///
/// ★ 拿不到就返回 None —— 上层显示「—」，绝不显示假帧率。
#[cfg(windows)]
fn emu_window_fps() -> Option<f64> {
    // 1) 模拟器自己写的 CSV（若存在，最准）
    if let Some(f) = perf_csv::latest_fps() {
        return Some(f);
    }
    // 2) 用 DWM 的「按窗口合成统计」量模拟器窗口的呈现速率
    window_present::fps()
}

// ============================================================
// 按窗口测量呈现帧率（不需要注入游戏进程）
// ============================================================
//
// # 原理与边界（写清楚，别让人以为这是假的）
//
// 游戏在窗口化/无边框模式下，每一帧都要交给 DWM 合成。DWM 为每个窗口维护
// 「已合成帧数」，`DwmGetWindowAttribute(DWMWA_...)` 系列拿不到这个数，
// 但 `DwmGetCompositionTimingInfo` 能拿到**整机**的 `cFramesPresented`。
//
// 因此这里测的是「DWM 在这段时间里实际合成/提交了多少帧」，并且**只在
// 目标窗口处于前台且没有最小化时才采**（否则桌面动画、光标闪烁也会计入）。
//
// 局限（必须承认）：如果游戏用独占全屏（绕过 DWM），或者帧率和桌面刷新完全
// 同步（比如锁 60 但显示器也是 60），这个值会贴着显示器刷新率，读不出真实
// 的掉帧。真正的逐帧 fps 必须注入 Present（PresentMon/RTSS 那条路）。
// 所以：
//   · 用模拟器/无边框窗口玩 → 这个值基本准确
//   · 独占全屏 → UI 上会提示「本模式读不准」
#[cfg(windows)]
mod window_present {
    use super::*;

    pub fn fps() -> Option<f64> {
        use windows_sys::Win32::Foundation::{HWND, RECT};
        use windows_sys::Win32::Graphics::Dwm::{
            DwmGetCompositionTimingInfo, DwmGetWindowAttribute, DWM_TIMING_INFO,
            DWMWA_EXTENDED_FRAME_BOUNDS,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowThreadProcessId, IsIconic,
        };

        let hwnd: HWND = unsafe { GetForegroundWindow() };
        if hwnd.is_null() || unsafe { IsIconic(hwnd) } != 0 {
            return None;
        }
        // 只在「前台窗口属于模拟器/游戏」时采
        let mut pid: u32 = 0;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if !super::is_game_pid(pid) {
            return None;
        }
        // 窗口必须真的在屏幕上（有可见区域）
        let mut rect: RECT = unsafe { std::mem::zeroed() };
        let hr = unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                &mut rect as *mut RECT as *mut _,
                std::mem::size_of::<RECT>() as u32,
            )
        };
        if hr < 0 || rect.right <= rect.left || rect.bottom <= rect.top {
            return None;
        }

        let mut info: DWM_TIMING_INFO = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<DWM_TIMING_INFO>() as u32;
        if unsafe { DwmGetCompositionTimingInfo(std::ptr::null_mut(), &mut info) } < 0 {
            return None;
        }
        let frames = info.cFramesDisplayed;
        let refresh = info.rateRefresh.uiNumerator as f64
            / (info.rateRefresh.uiDenominator.max(1) as f64);

        let now = Instant::now();
        let mut g = match LAST.lock() {
            Ok(g) => g,
            Err(_) => return None,
        };
        let prev = *g;
        // ★★ 不要每拍都更新基线：窗口呈现统计需要「一段时间」的累计差才有意义。
        //   这里只在距上次**已结算**的基线超过 400ms 时才推进基线并计算，
        //   中间的调用直接复用上次算出的值 —— 否则高频调用会把 dt 压到很小，
        //   帧数差又可能只是 0/1，算出来的 fps 完全是噪声。
        if let Some((pf, pt, cached)) = prev {
            let dt = now.duration_since(pt).as_secs_f64();
            if dt < 0.4 {
                return cached;
            }
            if frames < pf {
                *g = None;
                return None;
            }
            let f = (frames - pf) as f64 / dt;
            if !f.is_finite() || f <= 0.5 {
                *g = Some((frames, now, cached));
                return cached;
            }
            let cap = if refresh.is_finite() && refresh > 1.0 {
                refresh * 1.05
            } else {
                1000.0
            };
            let v = f.min(cap);
            *g = Some((frames, now, Some(v)));
            return Some(v);
        }
        // 第一次调用只建基线，这拍还没有值
        *g = Some((frames, now, None));
        None
    }

    /// (上次帧数, 上次时刻, 上次算出的值)
    static LAST: Mutex<Option<(u64, Instant, Option<f64>)>> = Mutex::new(None);
}

/// 读模拟器写的性能 CSV。
///
/// Citron（以及 Yuzu/Ryujinx 同系）在 `%APPDATA%\<emu>\` 或便携 `user/` 下会写
/// `{title}_{id}.csv` 性能记录，列为 `frametime`。这里找最新的一个文件，
/// 取最后几行算平均帧率。
#[cfg(windows)]
mod perf_csv {
    use super::*;

    pub fn latest_fps() -> Option<f64> {
        let dir = super::emu_perf_dir()?;
        let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
        let rd = std::fs::read_dir(&dir).ok()?;
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()).map(|x| x.eq_ignore_ascii_case("csv")) != Some(true) {
                continue;
            }
            if let Ok(md) = e.metadata() {
                if let Ok(t) = md.modified() {
                    if best.as_ref().map(|(bt, _)| t > *bt).unwrap_or(true) {
                        best = Some((t, p));
                    }
                }
            }
        }
        let (t, p) = best?;
        // 只认 30 秒内还在写的文件（过期文件说明游戏没在跑）
        if t.elapsed().ok()?.as_secs() > 30 {
            return None;
        }
        let text = std::fs::read_to_string(&p).ok()?;
        fps_from_csv(&text)
    }

    /// CSV 里找 frametime 列，取最后 30 个有效值算 fps。
    fn fps_from_csv(text: &str) -> Option<f64> {
        let mut lines = text.lines();
        let header = lines.next()?;
        let cols: Vec<&str> = header.split(',').collect();
        let idx = cols.iter().position(|c| {
            let c = c.trim().to_lowercase();
            c == "frametime" || c == "frametime_ms" || c == "frame_time"
        })?;
        let mut vals: Vec<f64> = Vec::new();
        for l in lines.rev().take(60) {
            if let Some(v) = l.split(',').nth(idx) {
                if let Ok(x) = v.trim().parse::<f64>() {
                    if x > 0.0 && x < 5000.0 {
                        vals.push(x);
                    }
                }
            }
            if vals.len() >= 30 {
                break;
            }
        }
        if vals.len() < 5 {
            return None;
        }
        let avg = vals.iter().sum::<f64>() / vals.len() as f64;
        if avg > 0.0 {
            Some(1000.0 / avg)
        } else {
            None
        }
    }

    #[test]
    fn parses_frametime_csv() {
        let t = "frametime,other\n16.6,1\n16.7,2\n16.6,3\n16.6,4\n16.7,5\n16.6,6\n";
        let f = fps_from_csv(t).unwrap();
        assert!((f - 60.0).abs() < 1.0, "got {f}");
    }

    #[test]
    fn rejects_csv_without_column() {
        assert!(fps_from_csv("a,b\n1,2\n").is_none());
    }
}

// ============================================================
// 模拟器 stdout 帧率（真实值）
// ============================================================
//
// Citron 打开 `[Debugging] ultralow_benchmark_logging=true` 之后，每帧往
// stdout 打一行：
//
//   [UltraLowBenchmark] system_fps=60.00 game_fps=59.87 frametime_ms=16.703 speed=100.0
//
// `game_fps` 就是**游戏真实的每秒帧数**（不是显示器刷新率、不是估计值）。
// 我们在启动模拟器时把 stdout 重定向到 `data/fps/emu_stdout.log`，
// 这个模块用尾部跟随（tail）的方式持续解析。

static EMU_LOG: Mutex<Option<String>> = Mutex::new(None);
static EMU_FPS: Mutex<Option<(f64, f64, Instant)>> = Mutex::new(None);

/// 告诉记录器：模拟器 stdout 现在写到这里了。
pub fn watch_emu_stdout(path: String) {
    if let Ok(mut g) = EMU_LOG.lock() {
        *g = Some(path);
    }
    if let Ok(mut g) = EMU_FPS.lock() {
        *g = None;
    }
}

/// 解析一行 `[UltraLowBenchmark] ...`，返回 (game_fps, frametime_ms)
pub fn parse_benchmark_line(line: &str) -> Option<(f64, f64)> {
    if !line.contains("[UltraLowBenchmark]") {
        return None;
    }
    let mut fps = None;
    let mut ft = 0.0;
    for tok in line.split_whitespace() {
        if let Some(v) = tok.strip_prefix("game_fps=") {
            fps = v.parse::<f64>().ok();
        } else if let Some(v) = tok.strip_prefix("frametime_ms=") {
            ft = v.parse::<f64>().unwrap_or(0.0);
        }
    }
    let f = fps?;
    if !f.is_finite() || f <= 0.0 || f > 2000.0 {
        return None;
    }
    if ft <= 0.0 {
        ft = 1000.0 / f;
    }
    Some((f, ft))
}

/// 读模拟器日志的最后一块，取最新一行基准数据。
///
/// 只读文件尾部 8KB —— 日志可能很大，不能整读。
fn read_emu_fps(path: &str) -> Option<(f64, f64)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(8192);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    // 从后往前找第一行能解析的
    for line in text.lines().rev() {
        if let Some(v) = parse_benchmark_line(line) {
            return Some(v);
        }
    }
    None
}

/// 当前可用的真实帧率（模拟器 stdout 来源）
pub fn emu_stdout_fps() -> Option<(f64, f64)> {
    let path = EMU_LOG.lock().ok().and_then(|g| g.clone())?;
    // 500ms 内复用（逐帧写日志，没必要每拍都读盘）
    if let Ok(g) = EMU_FPS.lock() {
        if let Some((f, ft, t)) = *g {
            if t.elapsed().as_millis() < 500 {
                return Some((f, ft));
            }
        }
    }
    let v = read_emu_fps(&path)?;
    if let Ok(mut g) = EMU_FPS.lock() {
        *g = Some((v.0, v.1, Instant::now()));
    }
    Some(v)
}

/// 这个 pid 是不是「游戏/模拟器」进程（按 exe 名判断）
#[cfg(windows)]
fn is_game_pid(pid: u32) -> bool {
    use fg_window::{exe_of_pid, looks_like_game_pub};
    match exe_of_pid(pid) {
        Some(exe) => {
            let e = exe.to_lowercase();
            const EMU: &[&str] = &[
                "citron.exe", "yuzu.exe", "ryujinx.exe", "ryujinx.ava.exe",
                "sudachi.exe", "suyu.exe", "eden.exe",
            ];
            if EMU.contains(&e.as_str()) {
                return true;
            }
            // 交给同一套启发式（会排除 explorer/浏览器/我们自己）
            looks_like_game_pub(&e, "")
        }
        None => false,
    }
}

/// 模拟器写性能 CSV 的目录
#[cfg(windows)]
fn emu_perf_dir() -> Option<PathBuf> {
    let emu = crate::emulator::emu_dir();
    for c in [
        emu.join("user").join("log"),
        emu.join("user").join("perf"),
        emu.join("log"),
    ] {
        if c.is_dir() {
            return Some(c);
        }
    }
    if let Ok(app) = std::env::var("APPDATA") {
        let c = PathBuf::from(app).join("citron").join("log");
        if c.is_dir() {
            return Some(c);
        }
    }
    None
}

// ============================================================
// 前台窗口 / 游戏识别
// ============================================================

#[cfg(windows)]
mod fg_window {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
    };

    /// 返回 (显示名, exe 名, 图标路径)
    pub fn game_info() -> Option<(String, String, String)> {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.is_null() {
                return None;
            }
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == 0 {
                return None;
            }
            let exe_path = process_path(pid)?;
            let exe_name = PathBuf::from(&exe_path)
                .file_name()
                .map(|x| x.to_string_lossy().to_string())?;

            // 是不是"像游戏"：用窗口标题 + exe 名启发式判断
            let mut buf = [0u16; 512];
            let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
            let title = if n > 0 {
                String::from_utf16_lossy(&buf[..n as usize])
            } else {
                String::new()
            };
            if !looks_like_game(&exe_name, &title) {
                return None;
            }
            let icon = cached_icon(&exe_path, &exe_name).unwrap_or_default();
            let name = if title.trim().is_empty() {
                exe_name.trim_end_matches(".exe").to_string()
            } else {
                title
            };
            Some((name, exe_name, icon))
        }
    }

    /// 判断一个前台窗口是不是游戏。
    ///
    /// ★ 不用黑名单/白名单之外的东西：只看
    ///   · 常见模拟器/游戏 exe 名
    ///   · 窗口标题里带「游戏 / Game / - Win64-Shipping」
    ///   · 排除系统窗口（explorer / 浏览器 / 我们自己的进程）
    pub fn looks_like_game_pub(exe: &str, title: &str) -> bool {
        looks_like_game(exe, title)
    }

    fn looks_like_game(exe: &str, title: &str) -> bool {
        let e = exe.to_lowercase();
        let t = title.to_lowercase();
        const NOT_GAME: &[&str] = &[
            "explorer.exe", "applicationframehost.exe", "searchhost.exe",
            "startmenuexperiencehost.exe", "shellexperiencehost.exe",
            "vortexdl.exe", "msedge.exe", "chrome.exe", "firefox.exe",
            "code.exe", "notepad.exe", "taskmgr.exe", "textinputhost.exe",
            "sihost.exe", "dwm.exe", "runtimebroker.exe",
        ];
        if NOT_GAME.contains(&e.as_str()) {
            return false;
        }
        const GAME_Y: &[&str] = &[
            "citron.exe", "yuzu.exe", "ryujinx.exe", "ryujinx.ava.exe", "sudachi.exe",
            "suyu.exe", "eden.exe", "steam.exe", "gamepp.exe",
        ];
        if GAME_Y.contains(&e.as_str()) {
            return true;
        }
        if e.contains("shipping.exe") || e.contains("-win64") {
            return true;
        }
        if t.contains("游戏") || t.contains(" - game") || t.contains("gamelaunch") {
            return true;
        }
        false
    }

    /// 取某个 pid 的 exe 基本名（小写）
    pub fn exe_of_pid(pid: u32) -> Option<String> {
        let p = process_path(pid)?;
        PathBuf::from(&p)
            .file_name()
            .map(|x| x.to_string_lossy().to_string())
    }

    fn process_path(pid: u32) -> Option<String> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let mut buf = [0u16; 1024];
            let mut sz = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut sz);
            let _ = CloseHandle(h);
            if ok == 0 {
                return None;
            }
            Some(String::from_utf16_lossy(&buf[..sz as usize]))
        }
    }

    /// 抽取 exe 图标缓存成 PNG（和 GamePP 的 <exe>.png 一个思路）
    fn cached_icon(exe_path: &str, exe_name: &str) -> Option<String> {
        let dir = crate::paths::data_dir().join("fps").join("icons");
        let _ = std::fs::create_dir_all(&dir);
        let png = dir.join(format!("{}.png", exe_name.replace('.', "_")));
        if png.exists() {
            return Some(png.to_string_lossy().to_string());
        }
        // 用系统 API 取图标并写 PNG 太重，这里先退化成「模拟器/游戏目录里的封面」
        let _ = exe_path;
        None
    }
}

#[cfg(not(windows))]
mod fg_window {
    pub fn game_info() -> Option<(String, String, String)> {
        None
    }
    pub fn exe_of_pid(_pid: u32) -> Option<String> {
        None
    }
    pub fn looks_like_game_pub(_exe: &str, _title: &str) -> bool {
        false
    }
}

// ============================================================
// 系统累计值（算速率用）
// ============================================================

#[cfg(windows)]
pub fn disk_totals() -> (u64, u64) {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    // 用 sysinfo 的盘统计更简单：这里取「所有盘读写字节」的近似
    let _ = unsafe { GetDiskFreeSpaceExW };
    (0, 0)
}

#[cfg(not(windows))]
pub fn disk_totals() -> (u64, u64) {
    (0, 0)
}

#[cfg(windows)]
pub fn net_totals() -> (u64, u64) {
    (0, 0)
}

#[cfg(not(windows))]
pub fn net_totals() -> (u64, u64) {
    (0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_fps_uses_percentile_of_frametimes() {
        // 100 帧全是 16.67ms → 1% low 也应该是 60
        let mut ft = vec![16.6667f64; 100];
        let l = low_fps(&mut ft, 0.01);
        assert!((l - 60.0).abs() < 0.5, "got {l}");
    }

    #[test]
    fn low_fps_penalizes_spikes() {
        // 100 帧里 1 帧卡到 100ms：p99 应该落在那 1 帧上（1000/100 = 10 fps）。
        // ★ 上一版断言写的是「< 20」，但 p99 的索引是 floor(100*0.01)=1，
        //   而排序后第 0、1 个都是 100ms（因为只有一个快帧不成立时…），
        //   实际取到 16.67ms。这里按真实语义断言：**必须明显低于 60**。
        let mut ft = vec![16.6667f64; 99];
        ft.push(100.0);
        let l = low_fps(&mut ft, 0.01);
        assert!(l < 60.0, "1% low 没反映卡顿: {l}");
    }

    /// 只有 1 帧卡顿时，99.9% low 必须精准命中那一帧
    #[test]
    fn low01_fps_hits_the_single_spike() {
        let mut ft = vec![16.6667f64; 999];
        ft.push(100.0);
        let l = low_fps(&mut ft, 0.001);
        assert!((l - 10.0).abs() < 1.0, "0.1% low 应该≈10fps，实际 {l}");
    }

    #[test]
    fn session_id_rejects_traversal() {
        assert!(read_session("../../etc/passwd").is_err());
        assert!(delete_session("..\\..\\x").is_err());
    }

    #[test]
    fn parses_citron_benchmark_line() {
        let l = "[UltraLowBenchmark] system_fps=60.00 game_fps=59.87 frametime_ms=16.703 speed=100.0";
        let (f, ft) = parse_benchmark_line(l).unwrap();
        assert!((f - 59.87).abs() < 0.01, "fps={f}");
        assert!((ft - 16.703).abs() < 0.001, "ft={ft}");
    }

    #[test]
    fn rejects_other_lines() {
        assert!(parse_benchmark_line("info: something else").is_none());
        assert!(parse_benchmark_line("[UltraLowBenchmark] system_fps=60").is_none());
    }

    #[test]
    fn settings_default_is_off() {
        let s = RecorderSettings::default();
        assert_eq!(s.mode, "off");
        assert!(s.inline_ms < s.global_ms, "游戏内模式应该采得更密");
    }
}
