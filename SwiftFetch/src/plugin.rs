//! SwiftFetch v3 插件化解耦核心 - Plugin 层
//!
//! 双模式插件:
//! - AsyncThreadPlugin: 同进程 tokio 任务, 高性能 (内置 HTTP/BT/Probe/...)
//! - IsolatedProcessPlugin: 独立子进程 IPC, 故障隔离 (DLL/EXE)

use async_trait::async_trait;
use flume::{Receiver, Sender};
use parking_lot::Mutex as PMutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use crate::speed_engine::DownloadConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IpcFrame {
    #[serde(rename = "REQ")]
    Request {
        v: u32,
        req_id: String,
        method: String,
        #[serde(default)]
        payload: Option<serde_json::Value>,
        #[serde(default)]
        deadline_ms: Option<u64>,
    },
    #[serde(rename = "REP")]
    Reply {
        req_id: String,
        status: String,
        #[serde(default)]
        payload: Option<serde_json::Value>,
    },
    #[serde(rename = "EVT")]
    Event {
        topic: String,
        #[serde(default)]
        payload: Option<serde_json::Value>,
    },
    #[serde(rename = "HS1")]
    Handshake {
        protocol: String,
        name: String,
        version: [u32; 3],
    },
    #[serde(rename = "HS_ACK")]
    HandshakeAck {
        host_version: [u32; 3],
        assign_id: u64,
        #[serde(default)]
        feature_flags: u32,
    },
    #[serde(rename = "SHUT")]
    ShutdownV1,
    #[serde(rename = "PING")]
    Ping,
    #[serde(rename = "PONG")]
    Pong,
}

pub type AsyncThreadPlugin = Arc<dyn SwiftPlugin>;
pub type IsolatedProcessPlugin = Arc<dyn SwiftPlugin>;

pub type PluginResult<T> = anyhow::Result<T>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PluginId(pub u64);

impl PluginId {
    pub fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

impl Default for PluginId {
    fn default() -> Self { Self::new() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PluginKind {
    AsyncThread,
    IsolatedProcess,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PluginHealth {
    Healthy,
    Degraded(String),
    Unresponsive,
    Crashed,
}

impl Default for PluginHealth {
    fn default() -> Self { PluginHealth::Healthy }
}

#[async_trait]
pub trait SwiftPlugin: Send + Sync {
    fn id(&self) -> PluginId;
    fn name(&self) -> &'static str;
    fn kind(&self) -> PluginKind;
    fn version(&self) -> (u32, u32, u32);
    async fn start(&self, host: Arc<PluginHost>) -> PluginResult<()>;
    async fn stop(&self, host: Arc<PluginHost>) -> PluginResult<()>;
    async fn health_check(&self) -> PluginHealth;
    fn send_message(&self, msg: PluginMsg) -> PluginResult<oneshot::Receiver<PluginReply>>;
}

pub type PluginBox = Arc<dyn SwiftPlugin>;

struct PendingReply {
    tx: Option<oneshot::Sender<PluginReply>>,
    deadline: Option<Instant>,
}

pub struct PluginRegistry {
    plugins: PMutex<Vec<PluginBox>>,
    by_name: PMutex<HashMap<String, PluginId>>,
    next_dll_handle: PMutex<Vec<(PluginId, Arc<libloading::Library>)>>,
    plugin_args: std::sync::RwLock<HashMap<String, HashMap<String, String>>>,
    disabled: std::sync::RwLock<Vec<String>>,
}

unsafe impl Send for PluginRegistry {}
unsafe impl Sync for PluginRegistry {}

impl PluginRegistry {
    pub fn new() -> Self {
        Self {
            plugins: PMutex::new(Vec::new()),
            by_name: PMutex::new(HashMap::new()),
            next_dll_handle: PMutex::new(Vec::new()),
            plugin_args: std::sync::RwLock::new(HashMap::new()),
            disabled: std::sync::RwLock::new(Vec::new()),
        }
    }

    pub fn set_disabled(&self, names: Vec<String>) {
        let mut w = self.disabled.write().unwrap();
        *w = names;
    }

    pub fn is_disabled(&self, name: &str) -> bool {
        self.disabled.read().unwrap().iter().any(|n| n == name)
    }

    pub fn set_plugin_arg(&self, plugin_name: &str, key: &str, val: &str) {
        let mut args = self.plugin_args.write().unwrap();
        args.entry(plugin_name.to_string())
            .or_insert_with(HashMap::new)
            .insert(key.to_string(), val.to_string());
    }

    pub fn register_static(&self, name: &'static str, plugin: Box<dyn SwiftPlugin>) -> PluginId {
        let id = plugin.id();
        let p: PluginBox = Arc::from(plugin);
        self.by_name.lock().insert(name.to_string(), id);
        self.plugins.lock().push(p);
        id
    }

    pub fn unregister(&self, id: PluginId) {
        let mut plugins = self.plugins.lock();
        plugins.retain(|p| p.id() != id);
        let mut names = self.by_name.lock();
        names.retain(|_, v| *v != id);
    }

    pub fn list(&self) -> Vec<PluginBox> {
        self.plugins.lock().clone()
    }

    pub fn get(&self, id: PluginId) -> Option<PluginBox> {
        self.plugins.lock().iter().find(|p| p.id() == id).cloned()
    }

    pub fn get_by_name(&self, name: &str) -> Option<PluginBox> {
        let id = *self.by_name.lock().get(name)?;
        self.get(id)
    }

    pub fn scan_plugins_dir(&self, dir: &Path) -> PluginResult<usize> {
        let mut loaded = 0usize;
        if !dir.exists() { return Ok(0); }
        let entries = std::fs::read_dir(dir)?;
        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
            match ext.as_str() {
                "dll" => {
                    if let Ok(n) = self.load_dll_plugin(&path) { loaded += n; }
                }
                "exe" => {
                    if let Ok(n) = self.load_exe_plugin(&path) { loaded += n; }
                }
                _ => {}
            }
        }
        Ok(loaded)
    }

    fn load_dll_plugin(&self, path: &Path) -> PluginResult<usize> {
        unsafe {
            let lib = Arc::new(libloading::Library::new(path)?);
            let create_fn: libloading::Symbol<unsafe extern "C" fn() -> *mut Box<dyn SwiftPlugin>> =
                match lib.get(b"_sf_plugin_create_v1") {
                    Ok(f) => f,
                    Err(_) => return Ok(0),
                };
            let raw_ptr = create_fn();
            if raw_ptr.is_null() { return Ok(0); }
            let plugin_box: Box<dyn SwiftPlugin> = *Box::from_raw(raw_ptr);
            let name = plugin_box.name().to_string();
            if self.is_disabled(&name) { return Ok(0); }
            let id = plugin_box.id();
            let arc: PluginBox = Arc::from(plugin_box);
            self.by_name.lock().insert(name, id);
            self.plugins.lock().push(arc.clone());
            self.next_dll_handle.lock().push((id, lib));
            Ok(1)
        }
    }

    fn load_exe_plugin(&self, path: &Path) -> PluginResult<usize> {
        let file_name = path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown_plugin")
            .to_string();
        if self.is_disabled(&file_name) { return Ok(0); }
        let id = PluginId::new();
        let static_name: &'static str = Box::leak(file_name.clone().into_boxed_str());
        let iso_plugin = match IsoExePluginInner::new(id, path.to_path_buf(), file_name) {
            Ok(p) => p,
            Err(_) => return Ok(0),
        };
        let wrapper = ExePluginWrapper {
            inner: Arc::new(PMutex::new(Some(iso_plugin))),
            id,
            name_leak: static_name,
        };
        self.by_name.lock().insert(static_name.to_string(), id);
        self.plugins.lock().push(Arc::new(wrapper));
        Ok(1)
    }

    pub fn hot_reload(&self, name: &str) -> PluginResult<bool> {
        if let Some(p) = self.get_by_name(name) {
            match p.kind() {
                PluginKind::IsolatedProcess => {
                    let _ = self.unregister(p.id());
                    let dir = PathBuf::from("plugins");
                    let _ = self.scan_plugins_dir(&dir);
                    Ok(true)
                }
                _ => Ok(false),
            }
        } else { Ok(false) }
    }
}

impl Default for PluginRegistry {
    fn default() -> Self { Self::new() }
}

struct ExePluginWrapper {
    inner: Arc<PMutex<Option<IsoExePluginInner>>>,
    id: PluginId,
    name_leak: &'static str,
}

#[async_trait]
impl SwiftPlugin for ExePluginWrapper {
    fn id(&self) -> PluginId { self.id }
    fn name(&self) -> &'static str { self.name_leak }
    fn kind(&self) -> PluginKind { PluginKind::IsolatedProcess }
    fn version(&self) -> (u32, u32, u32) { (0, 1, 0) }

    async fn start(&self, host: Arc<PluginHost>) -> PluginResult<()> {
        let opt = self.inner.lock().take();
        if let Some(mut inner) = opt {
            let r = inner.start_async(host).await;
            *self.inner.lock() = Some(inner);
            r
        } else { Ok(()) }
    }

    async fn stop(&self, host: Arc<PluginHost>) -> PluginResult<()> {
        let opt = self.inner.lock().take();
        if let Some(mut inner) = opt {
            let r = inner.stop_async(host).await;
            *self.inner.lock() = Some(inner);
            r
        } else { Ok(()) }
    }

    async fn health_check(&self) -> PluginHealth {
        let opt = self.inner.lock().take();
        if let Some(inner) = opt {
            let result = tokio::spawn(async move {
                let r = inner.health_check_inner().await;
                (r, inner)
            }).await;
            match result {
                Ok((h, inner2)) => {
                    *self.inner.lock() = Some(inner2);
                    h
                }
                Err(_) => {
                    PluginHealth::Crashed
                }
            }
        } else { PluginHealth::Crashed }
    }

    fn send_message(&self, msg: PluginMsg) -> PluginResult<oneshot::Receiver<PluginReply>> {
        let (tx, rx) = oneshot::channel();
        let _ = (msg.method.clone(), msg.payload.clone());
        let mut inner_guard = self.inner.lock();
        if let Some(inner) = inner_guard.as_mut() {
            let ipc_tx_opt = inner.ipc_tx.lock().clone();
            if let Some(ipc) = ipc_tx_opt {
                let req_id = generate_req_id(self.name());
                let frame = IpcFrame::Request {
                    v: 1,
                    req_id: req_id.clone(),
                    method: msg.method,
                    payload: msg.payload,
                    deadline_ms: None,
                };
                inner.pending_replies.lock().insert(req_id, PendingReply { tx: Some(tx), deadline: None });
                let _ = ipc.try_send(frame);
                return Ok(rx);
            }
        }
        drop(inner_guard);
        let _ = tx.send(PluginReply::err("Plugin unavailable".into()));
        Ok(rx)
    }
}

struct IsoExePluginInner {
    id: PluginId,
    path: PathBuf,
    process_name: String,
    child: PMutex<Option<tokio::process::Child>>,
    ipc_tx: PMutex<Option<Sender<IpcFrame>>>,
    pending_replies: Arc<PMutex<HashMap<String, PendingReply>>>,
    health: PMutex<PluginHealth>,
    crash_history: PMutex<Vec<Instant>>,
    crash_backoff_until: PMutex<Option<Instant>>,
    last_pid: PMutex<Option<u32>>,
}

impl IsoExePluginInner {
    fn new(id: PluginId, path: PathBuf, process_name: String) -> PluginResult<Self> {
        Ok(Self {
            id, path, process_name,
            child: PMutex::new(None),
            ipc_tx: PMutex::new(None),
            pending_replies: Arc::new(PMutex::new(HashMap::new())),
            health: PMutex::new(PluginHealth::Healthy),
            crash_history: PMutex::new(Vec::new()),
            crash_backoff_until: PMutex::new(None),
            last_pid: PMutex::new(None),
        })
    }

    fn kill_child(&self) {
        if let Some(mut c) = self.child.lock().take() {
            let _ = c.start_kill();
        }
    }

    async fn start_async(&mut self, host: Arc<PluginHost>) -> PluginResult<()> {
        if let Some(backoff) = *self.crash_backoff_until.lock() {
            if Instant::now() < backoff {
                anyhow::bail!("插件在崩溃冷却期: {}", self.process_name);
            } else {
                *self.crash_backoff_until.lock() = None;
            }
        }

        let pid = std::process::id();
        let pipe_name = format!(r"\\.\pipe\swiftfetch-host-{}-{}", pid, self.id.0);

        let server = tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name)?;

        let mut cmd = tokio::process::Command::new(&self.path);
        cmd.arg("--sf-plugin").arg("--pipe").arg(&pipe_name);
        let child = cmd.spawn()?;
        *self.last_pid.lock() = child.id();
        *self.child.lock() = Some(child);

        server.connect().await
            .map_err(|e| { self.kill_child(); anyhow::anyhow!("插件管道连接失败: {}", e) })?;

        let (rx, tx) = tokio::io::split(server);
        let (line_tx, _line_rx) = flume::unbounded::<IpcFrame>();
        *self.ipc_tx.lock() = Some(line_tx.clone());

        let pending = self.pending_replies.clone();
        let plugin_id = self.id;
        let host_c = host.clone();

        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            let mut reader = BufReader::new(rx);
            let mut writer = tx;
            let mut buf = String::new();
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut ok = false;
            let mut pong = Instant::now();

            let mut last_ping = Instant::now();
            loop {
                tokio::select! {
                    r = reader.read_line(&mut buf) => {
                        match r {
                            Ok(0) => break,
                            Ok(_) => {
                                let trimmed = buf.trim();
                                if !trimmed.is_empty() {
                                    if let Ok(frame) = serde_json::from_str::<IpcFrame>(trimmed) {
                                        match frame {
                                            IpcFrame::Handshake { .. } => {
                                                let ack = IpcFrame::HandshakeAck {
                                                    host_version: [3, 0, 0],
                                                    assign_id: plugin_id.0,
                                                    feature_flags: 0,
                                                };
                                                if let Ok(s) = serde_json::to_string(&ack) {
                                                    let _ = writer.write_all(s.as_bytes()).await;
                                                    let _ = writer.write_all(b"\n").await;
                                                    let _ = writer.flush().await;
                                                }
                                                ok = true;
                                            }
                                            IpcFrame::Pong => { pong = Instant::now(); }
                                            IpcFrame::Reply { req_id, status, payload } => {
                                                if let Some(mut pr) = pending.lock().remove(&req_id) {
                                                    let reply = PluginReply::from_status_payload(&status, payload);
                                                    if let Some(tx) = pr.tx.take() {
                                                        let _ = tx.send(reply);
                                                    }
                                                }
                                            }
                                            IpcFrame::Event { topic, payload } => {
                                                host_c.broadcast_event(PluginEventMsg {
                                                    topic, payload,
                                                    from_plugin: plugin_id,
                                                });
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                buf.clear();
                            }
                            Err(_) => break,
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {
                        if !ok && Instant::now() > deadline { break; }
                        if pong.elapsed() > Duration::from_secs(35) { break; }
                        if ok && last_ping.elapsed() >= Duration::from_secs(3) {
                            if let Ok(s) = serde_json::to_string(&IpcFrame::Ping) {
                                let _ = writer.write_all(s.as_bytes()).await;
                                let _ = writer.write_all(b"\n").await;
                                let _ = writer.flush().await;
                            }
                            last_ping = Instant::now();
                        }
                    }
                }
            }
        });

        Ok(())
    }

    async fn stop_async(&mut self, _host: Arc<PluginHost>) -> PluginResult<()> {
        if let Some(tx) = self.ipc_tx.lock().as_ref() {
            let _ = tx.try_send(IpcFrame::ShutdownV1);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        self.kill_child();
        Ok(())
    }

    async fn health_check_inner(&self) -> PluginHealth {
        let mut crashed = false;
        if let Some(child) = self.child.lock().as_mut() {
            match child.try_wait() {
                Ok(Some(status)) if !status.success() => { crashed = true; }
                _ => {}
            }
        }
        if crashed {
            let mut h = self.crash_history.lock();
            h.push(Instant::now());
            let cutoff = Instant::now() - Duration::from_secs(30);
            h.retain(|t| *t > cutoff);
            if h.len() >= 3 {
                *self.crash_backoff_until.lock() = Some(Instant::now() + Duration::from_secs(60));
            }
            *self.health.lock() = PluginHealth::Crashed;
        }
        self.health.lock().clone()
    }
}

// ============================================================
// PluginMsg / PluginReply
// ============================================================

#[derive(Debug, Clone)]
pub struct PluginMsg {
    pub method: String,
    pub payload: Option<serde_json::Value>,
}

impl PluginMsg {
    pub fn new<M: Into<String>>(method: M) -> Self {
        Self { method: method.into(), payload: None }
    }
    pub fn with_payload<M: Into<String>, S: Serialize>(method: M, p: S) -> Self {
        Self { method: method.into(), payload: serde_json::to_value(p).ok() }
    }
}

#[derive(Debug, Clone)]
pub struct PluginReply {
    pub status: String,
    pub payload: Option<serde_json::Value>,
    pub error: Option<String>,
}

impl PluginReply {
    pub fn ok<S: Serialize>(payload: S) -> Self {
        Self { status: "OK".into(), payload: serde_json::to_value(payload).ok(), error: None }
    }
    pub fn ok_empty() -> Self {
        Self { status: "OK".into(), payload: None, error: None }
    }
    pub fn err(msg: String) -> Self {
        Self { status: "ERR".into(), payload: None, error: Some(msg) }
    }
    pub fn from_status_payload(status: &str, payload: Option<serde_json::Value>) -> Self {
        Self { status: status.to_string(), payload, error: None }
    }
    pub fn is_ok(&self) -> bool { self.status == "OK" }
}

pub fn generate_req_id(plugin_name: &str) -> String {
    static REQ_COUNTER: AtomicU64 = AtomicU64::new(0);
    let short: String = plugin_name.chars().take(8).collect();
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let c = REQ_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}_{:x}_{:08x}", short, ms, c)
}

// ============================================================
// PluginHost
// ============================================================

pub struct PluginHost {
    pub registry: Arc<PluginRegistry>,
    pub host_rx: Receiver<HostBusMsg>,
    host_tx: Sender<HostBusMsg>,
    pub event_bus_tx: Sender<PluginEventMsg>,
    pub conn_pool: Arc<ConnectionPool>,
    resume_tx: Sender<ResumeDeltaMsg>,
    download_cfg: std::sync::RwLock<Option<DownloadConfig>>,
}

#[derive(Debug, Clone)]
pub enum HostBusMsg {
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct PluginEventMsg {
    pub topic: String,
    pub payload: Option<serde_json::Value>,
    pub from_plugin: PluginId,
}

pub enum ResumeDeltaMsg {
    SetBaseChunkDone(u32),
    SetPieceDone(u32),
    AddBytes(u32, u64),
    Flush,
    Stop,
}

#[derive(Debug, Clone)]
pub struct ConnStats {
    pub http: u32,
    pub bt: u32,
    pub idle: u32,
    pub total: u32,
}

pub struct ConnectionPool {
    inner: PMutex<PoolInner>,
    global_max: AtomicU64,
}

struct PoolInner {
    entries: HashMap<u64, ConnEntry>,
    next_id: u64,
}

struct ConnEntry {
    kind: ConnKind,
    refcount: u32,
    idle: bool,
}

#[derive(Clone, Copy)]
enum ConnKind { Http, Bt }

impl ConnectionPool {
    pub fn new(global_max: u32) -> Self {
        Self {
            inner: PMutex::new(PoolInner { entries: HashMap::new(), next_id: 1 }),
            global_max: AtomicU64::new(global_max as u64),
        }
    }
    pub fn register_http_socket(&self, _addr: String, _info: String) -> Option<u64> {
        let mut i = self.inner.lock();
        if i.entries.len() as u64 >= self.global_max.load(Ordering::Relaxed) { return None; }
        let id = i.next_id; i.next_id += 1;
        i.entries.insert(id, ConnEntry { kind: ConnKind::Http, refcount: 1, idle: false });
        Some(id)
    }
    pub fn register_bt_peer(&self, _addr: String, _meta: String) -> Option<u64> {
        let mut i = self.inner.lock();
        if i.entries.len() as u64 >= self.global_max.load(Ordering::Relaxed) { return None; }
        let id = i.next_id; i.next_id += 1;
        i.entries.insert(id, ConnEntry { kind: ConnKind::Bt, refcount: 1, idle: false });
        Some(id)
    }
    pub fn release(&self, id: u64) {
        let mut i = self.inner.lock();
        if let Some(e) = i.entries.get_mut(&id) {
            e.refcount = e.refcount.saturating_sub(1);
            if e.refcount == 0 { e.idle = true; }
        }
    }
    pub fn stats(&self) -> ConnStats {
        let i = self.inner.lock();
        let mut http = 0u32; let mut bt = 0u32; let mut idle = 0u32;
        for e in i.entries.values() {
            match e.kind { ConnKind::Http => http += 1, ConnKind::Bt => bt += 1 }
            if e.idle { idle += 1; }
        }
        ConnStats { http, bt, idle, total: i.entries.len() as u32 }
    }
}

impl PluginHost {
    pub fn new(registry: Arc<PluginRegistry>, global_max: u32) -> Self {
        let (host_tx, host_rx) = flume::unbounded::<HostBusMsg>();
        let (event_bus_tx, _event_rx) = flume::unbounded::<PluginEventMsg>();
        let (resume_tx, _resume_rx) = flume::unbounded::<ResumeDeltaMsg>();
        Self {
            registry, host_rx, host_tx, event_bus_tx,
            conn_pool: Arc::new(ConnectionPool::new(global_max)),
            resume_tx,
            download_cfg: std::sync::RwLock::new(None),
        }
    }
    pub fn set_download_config(&self, cfg: DownloadConfig) {
        if let Ok(mut w) = self.download_cfg.write() {
            *w = Some(cfg);
        }
    }
    pub fn download_config(&self) -> Option<DownloadConfig> {
        self.download_cfg.read().ok().and_then(|g| g.clone())
    }
    pub fn primary_http_url(&self) -> Option<String> {
        self.download_config().and_then(|c| {
            if c.url.is_empty() { None } else { Some(c.url) }
        })
    }
    pub fn host_sender(&self) -> Sender<HostBusMsg> { self.host_tx.clone() }
    pub fn resume_sender(&self) -> Sender<ResumeDeltaMsg> { self.resume_tx.clone() }
    pub fn broadcast_event(&self, evt: PluginEventMsg) {
        let _ = self.event_bus_tx.try_send(evt);
    }
    pub fn send_to_plugin(&self, target: PluginId, msg: PluginMsg) -> PluginResult<oneshot::Receiver<PluginReply>> {
        let plugin = self.registry.get(target)
            .ok_or_else(|| anyhow::anyhow!("Plugin id={} not found", target.0))?;
        plugin.send_message(msg)
    }
    pub fn send_to_plugin_by_name(&self, name: &str, msg: PluginMsg) -> PluginResult<oneshot::Receiver<PluginReply>> {
        let plugin = self.registry.get_by_name(name)
            .ok_or_else(|| anyhow::anyhow!("Plugin name={} not found", name))?;
        plugin.send_message(msg)
    }
}
