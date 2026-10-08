//! 应用级多文件日志器 (2026-09-15)
//!
//! 设计目标:
//! - 按类别写入不同日志文件, 方便定位问题 (而非全部塞进 stderr.log)
//! - 全部追加写入, 不做轮转 (完整保留历史, 方便排查 bug)
//! - 线程安全: 每个日志文件用独立的 Mutex<File>, 避免并发写入交错
//!
//! 日志文件清单 (位于 exe 同级 logs/ 目录):
//! - task_state.log   下载任务状态转换 (starting→running→paused→completed→failed→canceled)
//! - config.log       配置加载/保存/变更
//! - search.log       搜索/浏览活动 (查询词/结果数/分页)
//! - extract.log      解压详细进度 (每个文件/格式/回退)
//! - system.log       系统资源快照 (CPU/内存/磁盘/活跃任务数)
//! - window.log       窗口事件 (显示/隐藏/关闭/最小化/托盘)
//! - cache.log        缓存操作 (命中/未命中/更新/淘汰)
//! - translate.log    翻译批处理 (进度/失败/引擎状态)
//! - command.log      Tauri 命令调用跟踪 (哪个命令何时被调用)
//! - pause.log        暂停/恢复/取消操作
//! - network.log      网络层 (HTTP 探测/重定向/超时/连接复用)

use std::io::Write;
use std::sync::OnceLock;

/// 单个日志文件的句柄 (Mutex 保护)
struct LogHandle {
    file: std::sync::Mutex<std::fs::File>,
}

/// 全局日志句柄表 (按文件名索引)
static LOG_HANDLES: OnceLock<std::sync::Mutex<std::collections::HashMap<String, &'static LogHandle>>> = OnceLock::new();

/// 获取 logs 目录路径
fn logs_dir() -> std::path::PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let dir = exe_dir.join("logs");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 获取或创建指定日志文件的句柄 (惰性初始化, 进程生命周期内复用)
fn get_handle(log_name: &str) -> Option<&'static LogHandle> {
    let table = LOG_HANDLES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut guard = table.lock().ok()?;
    if let Some(h) = guard.get(log_name) {
        return Some(h);
    }
    // 新建句柄
    let path = logs_dir().join(log_name);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    // ★ Box::leak 让 LogHandle 获得静态生命周期, 可安全存入 OnceLock
    let boxed: Box<LogHandle> = Box::new(LogHandle {
        file: std::sync::Mutex::new(file),
    });
    let leaked: &'static LogHandle = Box::leak(boxed);
    guard.insert(log_name.to_string(), leaked);
    Some(leaked)
}

/// 当前时间戳 (本地时间, 毫秒精度)
fn timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

/// 写入一行日志到指定文件 (核心函数)
/// - log_name: 文件名 (如 "task_state.log")
/// - tag: 标签 (如 "STATE_CHANGE")
/// - message: 日志内容
pub fn write(log_name: &str, tag: &str, message: &str) {
    let Some(h) = get_handle(log_name) else { return };
    let line = format!("[{}] [{}] {}\n", timestamp(), tag, message);
    if let Ok(mut f) = h.file.lock() {
        let _ = f.write_all(line.as_bytes());
    }
}

// ============================================================
// 便捷函数: 按日志类型封装
// ============================================================

/// 写入 task_state.log — 下载任务状态转换
/// state: starting|running|paused|completed|failed|canceled
pub fn log_task_state(task_id: &str, old_state: &str, new_state: &str, detail: &str) {
    write("task_state.log", "STATE",
        &format!("task_id={} {} → {} {}", task_id, old_state, new_state, detail));
}

/// 写入 config.log — 配置加载/保存
pub fn log_config(action: &str, detail: &str) {
    write("config.log", "CONFIG",
        &format!("action={} {}", action, detail));
}

/// 写入 search.log — 搜索/浏览活动
pub fn log_search(action: &str, detail: &str) {
    write("search.log", "SEARCH",
        &format!("action={} {}", action, detail));
}

/// 写入 extract.log — 解压详细进度
pub fn log_extract(tag: &str, detail: &str) {
    write("extract.log", tag, detail);
}

/// 写入 system.log — 系统资源快照
pub fn log_system(tag: &str, detail: &str) {
    write("system.log", tag, detail);
}

/// 写入 window.log — 窗口事件 (show/hide/close/minimize/tray)
pub fn log_window(tag: &str, detail: &str) {
    write("window.log", tag, detail);
}

/// 写入 cache.log — 缓存操作 (hit/miss/update/evict)
pub fn log_cache(tag: &str, detail: &str) {
    write("cache.log", tag, detail);
}

/// 写入 translate.log — 翻译批处理 (progress/fail/engine)
pub fn log_translate(tag: &str, detail: &str) {
    write("translate.log", tag, detail);
}

/// 写入 command.log — Tauri 命令调用跟踪
pub fn log_command(cmd: &str, detail: &str) {
    write("command.log", "CMD", &format!("{} {}", cmd, detail));
}

/// 写入 pause.log — 暂停/恢复/取消操作
pub fn log_pause(tag: &str, detail: &str) {
    write("pause.log", tag, detail);
}

/// 写入 network.log — 网络层 (probe/redirect/timeout/reuse)
pub fn log_network(tag: &str, detail: &str) {
    write("network.log", tag, detail);
}
