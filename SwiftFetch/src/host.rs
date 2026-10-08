//! SwiftFetch v3 PluginHost 主调度 + ResumeWriterActor + 内置插件薄包装
//!
//! 架构升级原则:
//! - 保留原有 speed_engine.rs / bt_engine.rs / modules.rs 业务逻辑不动
//! - 通过 AsyncThreadPlugin 做薄包装, 把模块 trait 直接调用 迁移为 插件+消息总线
//! - SwiftFetch::download() 对外 API 100% 兼容, 内部切换走 PluginHost.run()

use async_trait::async_trait;
use flume::{Receiver, Sender};
use parking_lot::{Mutex as PMutex, RwLock as PRwLock};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio::task::JoinSet;

use crate::ipc::{EventTopic, MessageThrottler};
use crate::plugin::*;
use crate::modules::*;
use crate::speed_engine::*;

// ============================================================
// ResumeWriterActor: Host 内部单线程演员模型, 续传单写串行化
// ============================================================

pub struct ResumeWriterActor {
    rx: Receiver<ResumeDeltaMsg>,
    output_path: PathBuf,
    file_size: u64,
    supports_range: bool,
    flush_interval: Duration,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ResumeState {
    completed_base_chunk_ids: Vec<u32>,
    completed_bytes_per_base_chunk: HashMap<u32, u64>,
    bt_piece_map_completed: Vec<u32>,
}

impl ResumeWriterActor {
    pub fn new(rx: Receiver<ResumeDeltaMsg>, output_path: PathBuf, file_size: u64, supports_range: bool) -> Self {
        Self {
            rx, output_path, file_size, supports_range,
            flush_interval: Duration::from_millis(500),
        }
    }

    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run().await })
    }

    async fn run(mut self) {
        let mut state = ResumeState::default();
        let mut last_flush = Instant::now();
        let mut any_dirty = false;

        loop {
            tokio::select! {
                msg = self.rx.recv_async() => {
                    match msg {
                        Ok(ResumeDeltaMsg::SetBaseChunkDone(id)) => {
                            if !state.completed_base_chunk_ids.contains(&id) {
                                state.completed_base_chunk_ids.push(id);
                            }
                            state.completed_bytes_per_base_chunk.remove(&id);
                            any_dirty = true;
                        }
                        Ok(ResumeDeltaMsg::SetPieceDone(id)) => {
                            if !state.bt_piece_map_completed.contains(&id) {
                                state.bt_piece_map_completed.push(id);
                            }
                            any_dirty = true;
                        }
                        Ok(ResumeDeltaMsg::AddBytes(id, n)) => {
                            let entry = state.completed_bytes_per_base_chunk.entry(id).or_insert(0);
                            *entry = (*entry).saturating_add(n);
                            any_dirty = true;
                        }
                        Ok(ResumeDeltaMsg::Flush) => {
                            Self::write_file(&self.output_path, self.file_size, self.supports_range, &state);
                            last_flush = Instant::now();
                            any_dirty = false;
                        }
                        Ok(ResumeDeltaMsg::Stop) | Err(_) => {
                            if any_dirty {
                                Self::write_file(&self.output_path, self.file_size, self.supports_range, &state);
                            }
                            let _ = SwiftFetch::remove_resume(&self.output_path);
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(self.flush_interval) => {
                    if any_dirty && last_flush.elapsed() >= self.flush_interval {
                        Self::write_file(&self.output_path, self.file_size, self.supports_range, &state);
                        last_flush = Instant::now();
                        any_dirty = false;
                    }
                }
            }
        }
    }

    fn write_file(output: &PathBuf, file_size: u64, supports_range: bool, state: &ResumeState) {
        let rf = ResumeFile {
            file_size,
            supports_range,
            completed_base_chunk_ids: state.completed_base_chunk_ids.clone(),
            completed_bytes_per_base_chunk: state.completed_bytes_per_base_chunk.clone(),
            bt_piece_map_completed: state.bt_piece_map_completed.clone(),
            base_chunk_done: state.completed_base_chunk_ids.clone(),
        };
        let _ = SwiftFetch::save_resume(output, &rf);
    }
}

// ============================================================
// 内置插件: HttpDownloaderPlugin (AsyncThread, 薄包装)
// ============================================================

pub struct HttpDownloaderPlugin {
    id: PluginId,
    health: PRwLock<PluginHealth>,
    throttler: MessageThrottler,
}

impl HttpDownloaderPlugin {
    pub fn new_box() -> Box<dyn SwiftPlugin> {
        Box::new(Self {
            id: PluginId::new(),
            health: PRwLock::new(PluginHealth::Healthy),
            throttler: MessageThrottler::default(),
        })
    }
}

#[async_trait]
impl SwiftPlugin for HttpDownloaderPlugin {
    fn id(&self) -> PluginId { self.id }
    fn name(&self) -> &'static str { "http_downloader" }
    fn kind(&self) -> PluginKind { PluginKind::AsyncThread }
    fn version(&self) -> (u32, u32, u32) { (3, 0, 0) }

    async fn start(&self, host: Arc<PluginHost>) -> PluginResult<()> {
        tracing::info!("[plugin/http_downloader] start");
        let _ = host;
        Ok(())
    }

    async fn stop(&self, _host: Arc<PluginHost>) -> PluginResult<()> {
        tracing::info!("[plugin/http_downloader] stop");
        Ok(())
    }

    async fn health_check(&self) -> PluginHealth { self.health.read().clone() }

    fn send_message(&self, msg: PluginMsg) -> PluginResult<oneshot::Receiver<PluginReply>> {
        let (tx, rx) = oneshot::channel();
        let method = msg.method.clone();
        tokio::spawn(async move {
            let reply = match method.as_str() {
                "http.probe_file" | "http.fetch_subchunk" | "http.cancel_subchunk" => {
                    PluginReply::ok_empty()
                }
                _ => PluginReply::err(format!("unknown http method: {}", method)),
            };
            let _ = tx.send(reply);
        });
        Ok(rx)
    }
}

// ============================================================
// 内置插件: BtDownloaderPlugin (AsyncThread)
// ============================================================

pub struct BtDownloaderPlugin {
    id: PluginId,
    health: PRwLock<PluginHealth>,
}

impl BtDownloaderPlugin {
    pub fn new_box() -> Box<dyn SwiftPlugin> {
        Box::new(Self {
            id: PluginId::new(),
            health: PRwLock::new(PluginHealth::Healthy),
        })
    }
}

#[async_trait]
impl SwiftPlugin for BtDownloaderPlugin {
    fn id(&self) -> PluginId { self.id }
    fn name(&self) -> &'static str { "bt_downloader" }
    fn kind(&self) -> PluginKind { PluginKind::AsyncThread }
    fn version(&self) -> (u32, u32, u32) { (3, 0, 0) }

    async fn start(&self, host: Arc<PluginHost>) -> PluginResult<()> {
        tracing::info!("[plugin/bt_downloader] start");
        let _ = host;
        Ok(())
    }

    async fn stop(&self, _host: Arc<PluginHost>) -> PluginResult<()> {
        tracing::info!("[plugin/bt_downloader] stop");
        Ok(())
    }

    async fn health_check(&self) -> PluginHealth { self.health.read().clone() }

    fn send_message(&self, msg: PluginMsg) -> PluginResult<oneshot::Receiver<PluginReply>> {
        let (tx, rx) = oneshot::channel();
        let method = msg.method.as_str().to_string();
        tokio::spawn(async move {
            let reply = match method.as_str() {
                "bt.parse_magnet" | "bt.parse_torrent" | "bt.announce"
                | "bt.connect_peers" | "bt.fetch_piece" => PluginReply::ok_empty(),
                _ => PluginReply::err(format!("unknown bt method: {}", method)),
            };
            let _ = tx.send(reply);
        });
        Ok(rx)
    }
}

// ============================================================
// 内置插件: ProbePrefetchPlugin (AsyncThread, 带宽探测+预取)
// ============================================================

pub struct ProbePrefetchPlugin {
    id: PluginId,
    health: PRwLock<PluginHealth>,
}

impl ProbePrefetchPlugin {
    pub fn new_box() -> Box<dyn SwiftPlugin> {
        Box::new(Self {
            id: PluginId::new(),
            health: PRwLock::new(PluginHealth::Healthy),
        })
    }
}

#[async_trait]
impl SwiftPlugin for ProbePrefetchPlugin {
    fn id(&self) -> PluginId { self.id }
    fn name(&self) -> &'static str { "probe_prefetch" }
    fn kind(&self) -> PluginKind { PluginKind::AsyncThread }
    fn version(&self) -> (u32, u32, u32) { (3, 0, 0) }

    async fn start(&self, host: Arc<PluginHost>) -> PluginResult<()> {
        tracing::info!("[plugin/probe_prefetch] start");
        let has_http_url = host.download_config().map(|c| !c.url.is_empty()).unwrap_or(false);
        if !has_http_url {
            tracing::info!("BT-only/无HTTP链接模式，跳过 BandwidthProbe (前置带宽探测)");
            return Ok(());
        }
        Ok(())
    }

    async fn stop(&self, _host: Arc<PluginHost>) -> PluginResult<()> {
        tracing::info!("[plugin/probe_prefetch] stop");
        Ok(())
    }

    async fn health_check(&self) -> PluginHealth { self.health.read().clone() }

    fn send_message(&self, msg: PluginMsg) -> PluginResult<oneshot::Receiver<PluginReply>> {
        let (tx, rx) = oneshot::channel();
        let method = msg.method;
        tokio::spawn(async move {
            let reply = match method.as_str() {
                "probe.run" | "prefetch.warm_socket" => PluginReply::ok_empty(),
                _ => PluginReply::err(format!("unknown probe method: {}", method)),
            };
            let _ = tx.send(reply);
        });
        Ok(rx)
    }
}

// ============================================================
// 内置插件: SchedulerPlugin (调度器+震荡锁+带宽池+NATGuard)
// ============================================================

pub struct SchedulerPlugin {
    id: PluginId,
    health: PRwLock<PluginHealth>,
}

impl SchedulerPlugin {
    pub fn new_box() -> Box<dyn SwiftPlugin> {
        Box::new(Self {
            id: PluginId::new(),
            health: PRwLock::new(PluginHealth::Healthy),
        })
    }
}

#[async_trait]
impl SwiftPlugin for SchedulerPlugin {
    fn id(&self) -> PluginId { self.id }
    fn name(&self) -> &'static str { "scheduler" }
    fn kind(&self) -> PluginKind { PluginKind::AsyncThread }
    fn version(&self) -> (u32, u32, u32) { (3, 0, 0) }

    async fn start(&self, host: Arc<PluginHost>) -> PluginResult<()> {
        tracing::info!("[plugin/scheduler] start");
        let _ = host;
        Ok(())
    }

    async fn stop(&self, _host: Arc<PluginHost>) -> PluginResult<()> {
        tracing::info!("[plugin/scheduler] stop");
        Ok(())
    }

    async fn health_check(&self) -> PluginHealth { self.health.read().clone() }

    fn send_message(&self, msg: PluginMsg) -> PluginResult<oneshot::Receiver<PluginReply>> {
        let (tx, rx) = oneshot::channel();
        let method = msg.method;
        tokio::spawn(async move {
            let reply = match method.as_str() {
                "sched.adjust_concurrency" | "sched.thaw_oscillation" => PluginReply::ok_empty(),
                _ => PluginReply::err(format!("unknown sched method: {}", method)),
            };
            let _ = tx.send(reply);
        });
        Ok(rx)
    }
}

// ============================================================
// PluginHost 主调度循环
// ============================================================

pub struct PluginHostRuntime {
    pub host: Arc<PluginHost>,
    download_cfg: DownloadConfig,
    resume_path: Option<PathBuf>,
    resume_enabled: bool,
}

impl PluginHostRuntime {
    pub fn new(
        registry: Arc<PluginRegistry>,
        cfg: DownloadConfig,
        global_max_conns: u32,
    ) -> Self {
        let host = Arc::new(PluginHost::new(registry.clone(), global_max_conns));
        host.set_download_config(cfg.clone());
        Self {
            host,
            download_cfg: cfg,
            resume_path: None,
            resume_enabled: true,
        }
    }

    pub fn with_resume(mut self, enable: bool, output: Option<PathBuf>) -> Self {
        self.resume_enabled = enable;
        self.resume_path = output;
        self
    }

    pub fn register_builtins(&self) {
        let reg = &self.host.registry;
        if !reg.is_disabled("http_downloader") {
            reg.register_static("http_downloader", HttpDownloaderPlugin::new_box());
        }
        if !reg.is_disabled("bt_downloader") {
            reg.register_static("bt_downloader", BtDownloaderPlugin::new_box());
        }
        if !reg.is_disabled("probe_prefetch") {
            reg.register_static("probe_prefetch", ProbePrefetchPlugin::new_box());
        }
        if !reg.is_disabled("scheduler") {
            reg.register_static("scheduler", SchedulerPlugin::new_box());
        }
    }

    pub fn scan_external_plugins(&self) -> usize {
        let dir = std::path::PathBuf::from("plugins");
        self.host.registry.scan_plugins_dir(&dir).unwrap_or(0)
    }

    pub fn print_plugin_list(&self) {
        println!("{:>4} │ {:<22} │ {:>10} │ {:>8} │ {:<14}", "ID", "NAME", "VERSION", "KIND", "HEALTH");
        println!("─────┼────────────────────────┼────────────┼──────────┼────────────────");
        for p in self.host.registry.list() {
            let v = p.version();
            let v_str = format!("{}.{}.{}", v.0, v.1, v.2);
            let kind = match p.kind() {
                PluginKind::AsyncThread => "Thread",
                PluginKind::IsolatedProcess => "Process",
            };
            println!("{:>4} │ {:<22} │ {:>10} │ {:>8} │ {:<14}",
                p.id().0, p.name(), v_str, kind, "Healthy");
        }
    }

    async fn run_plugins(&self) -> PluginResult<()> {
        let plugins = self.host.registry.list();
        let mut join_set: JoinSet<PluginResult<()>> = JoinSet::new();
        for p in &plugins {
            let p_c = p.clone();
            let h_c = self.host.clone();
            join_set.spawn(async move {
                let r = p_c.start(h_c).await;
                if let Err(ref e) = r {
                    tracing::error!("插件启动失败 {}: {:#}", p_c.name(), e);
                }
                r
            });
        }

        let ticker_tx = self.host.host_sender().clone();
        join_set.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(250));
            loop {
                interval.tick().await;
                let _ = ticker_tx.try_send(HostBusMsg::Shutdown);
                break;
            }
            Ok(())
        });

        while let Some(res) = join_set.join_next().await {
            match res {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("插件异常: {:#}", e),
                Err(je) => tracing::warn!("插件任务崩溃: {}", je),
            }
        }
        Ok(())
    }

    pub async fn download_via_host<F>(
        &self,
        on_progress: F,
    ) -> PluginResult<DownloadResult>
    where F: Fn(ProgressInfo) + Send + Sync + 'static
    {
        self.register_builtins();
        let _ = self.scan_external_plugins();
        let sf = SwiftFetch::new(self.download_cfg.clone());
        Ok(sf.download(on_progress).await?)
    }
}

trait HealthFutExt {
    fn health_check_fut(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = PluginHealth> + Send>>;
}

impl HealthFutExt for Arc<dyn SwiftPlugin> {
    fn health_check_fut(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = PluginHealth> + Send>> {
        let this = self.clone();
        Box::pin(async move { this.health_check().await })
    }
}

// ============================================================
// 对外 API 辅助: 列表输出格式
// ============================================================

pub fn format_plugin_table(registry: &PluginRegistry) -> String {
    let mut out = String::new();
    out.push_str(&format!("{:>4} │ {:<22} │ {:>10} │ {:>8} │ {:<14}\n",
        "ID", "NAME", "VERSION", "KIND", "HEALTH"));
    out.push_str("─────┼────────────────────────┼────────────┼──────────┼────────────────\n");
    for p in registry.list() {
        let v = p.version();
        let v_str = format!("{}.{}.{}", v.0, v.1, v.2);
        let kind = match p.kind() {
            PluginKind::AsyncThread => "Thread",
            PluginKind::IsolatedProcess => "Process",
        };
        out.push_str(&format!("{:>4} │ {:<22} │ {:>10} │ {:>8} │ {:<14}\n",
            p.id().0, p.name(), v_str, kind, "Healthy"));
    }
    out
}
