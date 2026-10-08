//! SwiftFetch v3 - 纯CLI高性能下载内核入口 (插件化解耦版)
//!
//! 新增参数: --torrent, --magnet, --mirror, --sequential, --peer-limit,
//!           --5g, --wired-2g5, --wired-1g, --bt-port, --ratio, --seed-minutes,
//!           --max-conns, --http-only, --bt-only, --no-cross-protocol
//! v3 插件化解耦新增:
//!           --list-plugins, --disable-plugin <NAME>, --plugin-arg <NAME>=<VAL>

use clap::Parser;
use parking_lot::RwLock as PRwLock;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use swiftfetch::*;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "swiftfetch",
    version,
    about = "SwiftFetch v3 - 纯CLI高性能无UI下载内核 (HTTP+BT混合, 插件化解耦版)",
    long_about = "多模块并行架构 + 插件化解耦: 多源镜像, 分片预取, 慢块重调度, BT/HTTP混合协同, 5G自适应, NAT保护\n\
                  双模式插件: AsyncThread (同进程, 高性能) + IsolatedProcess (子进程IPC, 故障隔离)"
)]
struct Cli {
    #[arg(short = 'u', long = "url", help = "HTTP(S) 下载链接")]
    url: Option<String>,

    #[arg(long = "torrent", help = ".torrent 文件路径 (与 -u 互斥)")]
    torrent: Option<PathBuf>,

    #[arg(long = "magnet", help = "magnet 链接 (与 -u / --torrent 互斥)")]
    magnet: Option<String>,

    #[arg(short = 'o', long = "output", help = "输出路径/文件名")]
    output: Option<PathBuf>,

    #[arg(short = 'c', long = "connections", help = "HTTP 并发连接数 (5G默认18/上限18, 1G默认16/上限24, 2.5G默认32/上限48)")]
    connections: Option<u32>,

    #[arg(long = "base-chunk", help = "覆盖基底块大小 (字节, 如 4194304 = 4MB)")]
    base_chunk: Option<u64>,

    #[arg(long = "no-resume", help = "禁用断点续传")]
    no_resume: bool,

    #[arg(long = "proxy", help = "代理 URL (如 http://127.0.0.1:7890)")]
    proxy: Option<String>,

    #[arg(short = 'q', long = "quiet", help = "静默模式, 不输出进度条")]
    quiet: bool,

    #[arg(long = "json", help = "JSON 格式进度输出 (每行一个JSON)")]
    json: bool,

    #[arg(long = "mirror", help = "HTTP 镜像节点 (可多次重复)")]
    mirror: Vec<String>,

    #[arg(long = "sequential", help = "BT 顺序播放模式 (默认稀疏优先)")]
    sequential: bool,

    #[arg(long = "peer-limit", help = "BT 活跃 Peer 上限 (默认 64 / 5G模式 24)")]
    peer_limit: Option<u32>,

    #[arg(long = "5g", help = "强制 5G 移动网络优化模式")]
    five_g: bool,

    #[arg(long = "wired-2g5", help = "强制 2.5G 有线模式")]
    wired_2g5: bool,

    #[arg(long = "wired-1g", help = "强制 1G 有线模式")]
    wired_1g: bool,

    #[arg(long = "bt-port", default_value_t = DEFAULT_BT_PORT_START, help = "BT 监听端口 (默认 6881)")]
    bt_port: u16,

    #[arg(long = "ratio", default_value_t = DEFAULT_RATIO as f32, help = "BT 分享率目标 (默认 1.0, 达到自动停止做种)")]
    ratio: f32,

    #[arg(long = "seed-minutes", default_value_t = DEFAULT_SEED_MINUTES, help = "最小做种时间 (默认 0, 下载完成即停)")]
    seed_minutes: u32,

    #[arg(long = "max-conns", help = "全局总连接上限 (默认 96 / 5G模式 48)")]
    max_conns: Option<u32>,

    #[arg(long = "http-only", help = "禁止 BT (纯 HTTP 模式)")]
    http_only: bool,

    #[arg(long = "bt-only", help = "禁止 HTTP (纯 BT 模式)")]
    bt_only: bool,

    #[arg(long = "no-cross-protocol", help = "关闭 HTTP/BT 混合协同 (纯单协议)")]
    no_cross_protocol: bool,

    #[arg(value_name = "URL", help = "位置参数 URL (当 --url/-u 省略时直接传)")]
    pos_url: Option<String>,

    // ================ v3 插件化解耦 新增参数 ================
    #[arg(long = "list-plugins", help = "列出所有已注册插件(id/名称/版本/类型/健康状态)后退出")]
    list_plugins: bool,

    #[arg(long = "disable-plugin", help = "启动时禁用指定插件(可重复指定多个)", value_name = "NAME")]
    disable_plugin: Vec<String>,

    #[arg(long = "plugin-arg", help = "传递启动参数给指定插件, 格式: <PLUGIN>.<KEY>=<VAL> (可重复)", value_name = "NAME=VAL")]
    plugin_arg: Vec<String>,

    // ================ v3 多协议支持 新增参数 ================
    #[arg(long = "list-providers", help = "列出所有可用的 Protocol Provider (名称/能力/支持scheme)后退出")]
    list_providers: bool,

    #[arg(long = "username", short = 'U', help = "用户名 (HTTP-Basic/FTP/WebDAV/SFTP/rsync 通用)", value_name = "USER")]
    username: Option<String>,

    #[arg(long = "password", short = 'P', help = "密码 (HTTP-Basic/FTP/WebDAV/SFTP 通用, 建议使用环境变量避免明文)", value_name = "PASS")]
    password: Option<String>,

    #[arg(long = "ssh-key", help = "SSH 私钥 PEM 文件路径 (SFTP/rsync over SSH)", value_name = "PATH")]
    ssh_key: Option<PathBuf>,

    #[arg(long = "ssh-passphrase", help = "SSH 私钥解密口令 (可选, 与 --ssh-key 搭配)", value_name = "PHRASE")]
    ssh_passphrase: Option<String>,

    #[arg(long = "kubo-rpc", help = "IPFS Kubo 节点 HTTP RPC 地址 (默认 http://127.0.0.1:5001/api/v0)", value_name = "URL")]
    kubo_rpc: Option<String>,

    #[arg(long = "protocol", help = "强制协议覆盖: http/https/http2/http3/ftp/ftps/sftp/dav/davs/rsync/ipfs (默认按URL scheme)", value_name = "SCHEME")]
    force_protocol: Option<String>,
}

fn main() {
    let cli = Cli::parse();

    // ★ CPU 优化 (2026-09-28): 下载引擎限定 2 核 4 线程 (SF_WORKER_THREADS 可覆盖)
    //   下载以网络 I/O 为主, 4 个 worker 足够; 相比 available_parallelism() 的 16 线程
    //   少 12 个线程栈, 且 blocking 池上限收紧 → 显著降低内存占用
    // ★ 2026-09-30: 改由 dynamic_engine::runtime_worker_threads() 统一解析,
    //   与 dynamic_engine 的"分块粒度/并发流数"推导共用同一来源, 避免两处口径不一致.
    let worker_threads = runtime_worker_threads();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .max_blocking_threads(8)
        .enable_all()
        .build()
        .expect("创建 tokio runtime 失败");

    if !cli.quiet && !cli.json && !cli.list_plugins {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")))
            .with_target(false)
            .init();
    }

    // ---- 处理 --list-plugins 立即退出 ----
    if cli.list_plugins {
        let registry = Arc::new(PluginRegistry::new());
        for name in &cli.disable_plugin {
            registry.set_disabled(vec![name.clone()]);
        }
        let runtime = PluginHostRuntime::new(
            registry.clone(),
            DownloadConfig::default(),
            DEFAULT_GLOBAL_MAX_CONNS,
        );
        runtime.register_builtins();
        let external = runtime.scan_external_plugins();
        if external > 0 {
            eprintln!("[plugin] 从 plugins/ 目录加载了 {} 个外部插件", external);
        }
        print_plugins_table(&registry);
        return;
    }

    // ---- 处理 --list-providers 立即退出 (打印所有开启了feature的协议provider) ----
    if cli.list_providers {
        let prov_reg = ProviderRegistry::new();
        register_all_feature_providers(&prov_reg);
        let list = prov_reg.list_all();
        println!("=== SwiftFetch v3 ProtocolProviders (按编译 features 启用) ===");
        println!("{:<14} {:<42} {:<30}", "NAME", "CAPABILITY FLAGS", "SUPPORTED SCHEMES");
        println!("{}", "-".repeat(90));
        let mut total = 0usize;
        for (name, caps, schemes) in list {
            let flag_str = format_capability_flags(caps);
            let schemes_str: Vec<&str> = schemes.iter().map(|s| s.as_str()).collect();
            println!("{:<14} {:<42} {:<30}", name, flag_str, schemes_str.join(", "));
            total += 1;
        }
        println!("{}", "-".repeat(90));
        println!("共注册 {} 个 ProtocolProvider (可用 features: http http2 http3 ftp ftps sftp webdav rsync ipfs all-protocols)", total);
        println!("提示: 开启更多协议: cargo build --features all-protocols");
        return;
    }

    let source_url = cli.url.clone().or_else(|| cli.pos_url.clone());
    let source_count = [source_url.is_some(), cli.torrent.is_some(), cli.magnet.is_some()]
        .iter().filter(|&&b| b).count();

    if cli.http_only && cli.bt_only {
        eprintln!("错误: --http-only 与 --bt-only 不能同时开启");
        std::process::exit(2);
    }

    if !cli.mirror.is_empty() && source_url.is_none() {
        eprintln!("错误: --mirror 必须配合 -u URL 主链接使用 (HTTP-only 模式)");
        std::process::exit(2);
    }

    let protocol = if cli.http_only { ProtocolMode::HttpOnly }
        else if cli.bt_only { ProtocolMode::BtOnly }
        // ★ 自动识别: 指定了 --torrent / --magnet 但没有 -u URL → 纯 BT 模式
        //   (否则 Hybrid 模式下 cfg.url 为空会走 HTTP 路径导致 BT 下载无输出)
        else if (cli.torrent.is_some() || cli.magnet.is_some()) && source_url.is_none() { ProtocolMode::BtOnly }
        else { ProtocolMode::Hybrid };

    if protocol == ProtocolMode::HttpOnly && (cli.torrent.is_some() || cli.magnet.is_some()) {
        eprintln!("错误: --http-only 模式下不能使用 --torrent 或 --magnet");
        std::process::exit(2);
    }
    if protocol == ProtocolMode::BtOnly && source_url.is_some() {
        eprintln!("错误: --bt-only 模式下不能使用 -u URL 或 --mirror");
        std::process::exit(2);
    }

    if source_count == 0 {
        eprintln!("错误: 必须只选其一的下载源: -u <URL> | --torrent <FILE> | --magnet <URI>");
        eprintln!("用法: swiftfetch -u <URL> [--mirror <URL1> --mirror <URL2> ...] [OPTIONS]");
        eprintln!("或:   swiftfetch --torrent <file.torrent> [OPTIONS]");
        eprintln!("或:   swiftfetch --magnet \"magnet:?xt=urn:btih:...\" [OPTIONS]");
        eprintln!("提示: --mirror 表示 HTTP 镜像混合, 必须配合主 URL 使用");
        eprintln!("提示: 插件参数 --list-plugins / --disable-plugin / --plugin-arg 可独立使用");
        std::process::exit(2);
    }
    if source_count > 1 {
        eprintln!("错误: 只能提供一种下载源 (-u / --torrent / --magnet). 若 HTTP 混合请用 --mirror");
        std::process::exit(2);
    }

    let net_mode = if cli.five_g { NetworkMode::FiveG }
        else if cli.wired_2g5 { NetworkMode::Wired25G }
        else if cli.wired_1g { NetworkMode::Wired1G }
        else { NetworkMode::Auto };

    let download_mode = if cli.sequential { DownloadMode::SequentialStream }
        else { DownloadMode::SparseRareFirst };

    let mut cfg = DownloadConfig::default();
    cfg.url = source_url.clone().unwrap_or_default();
    cfg.output = cli.output.clone();
    cfg.network_mode = net_mode;
    cfg.user_connections = cli.connections;
    cfg.connections = DownloadConfig::calc_http_connections(net_mode, cli.connections);
    cfg.base_chunk_size = cli.base_chunk;
    cfg.resume_enabled = !cli.no_resume;
    cfg.proxy = cli.proxy.clone();
    cfg.mirrors = cli.mirror.clone();

    let torrent_abs: Option<PathBuf> = match cli.torrent.clone() {
        Some(p) => match std::fs::canonicalize(&p) {
            Ok(abs) => Some(abs),
            Err(e) => {
                eprintln!("错误: 无法解析 torrent 路径 {}: {}", p.display(), e);
                std::process::exit(1);
            }
        }
        None => None,
    };
    if let Some(tp) = &torrent_abs {
        if !tp.exists() {
            eprintln!("错误: torrent 文件不存在: {}", tp.display());
            std::process::exit(1);
        }
        match std::fs::read(tp) {
            Ok(bytes) => {
                if bytes.is_empty() {
                    eprintln!("错误: torrent 文件为空: {}", tp.display());
                    std::process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("错误: 无法读取 torrent 文件 {}: {}", tp.display(), e);
                std::process::exit(1);
            }
        }
    }

    // ===== ProtocolProvider 快速路径: SFTP/FTP/FTPS/rsync/WebDAV 等非 HTTP/BT 协议 =====
    // 直接使用 ProtocolProvider trait 统一下载，不进入复杂的 HTTP/BT 混合引擎
    if protocol != ProtocolMode::BtOnly {
        if let Some(url) = source_url.as_ref() {
            if needs_provider_dispatch(url) {
                let mut prov_reg = ProviderRegistry::new();
                register_all_feature_providers(&prov_reg);
                let auth = if let (Some(u), Some(p)) = (cli.username.clone(), cli.password.clone()) {
                    AuthInfo::UserPass { username: u, password: p }
                } else if let Some(key_path) = cli.ssh_key.clone() {
                    AuthInfo::SshKey { key_path, passphrase: None }
                } else {
                    AuthInfo::Anonymous
                };
                let out_final = cfg.output.clone().unwrap_or_else(|| PathBuf::from("download.bin"));
                let start = std::time::Instant::now();
                let r: anyhow::Result<(u64, _, String)> = rt.block_on(async {
                    simple_provider_download(&prov_reg, url, &auth, &out_final).await
                });
                match r {
                    Ok((bytes, _dur, _fname)) => {
                        let elapsed = start.elapsed().as_secs_f64();
                        let speed = if elapsed > 0.0 { bytes as f64 / elapsed } else { 0.0 };
                        println!("✓ 下载完成");
                        println!("  输出: {}", out_final.display());
                        let size_str = if bytes < 1024 { format!("{} B", bytes) }
                            else if bytes < 1024*1024 { format!("{:.2} KB", bytes as f64/1024.0) }
                            else { format!("{:.2} MB", bytes as f64/(1024.0*1024.0)) };
                        let sp_str = if speed < 1024.0 { format!("{:.0} B/s", speed) }
                            else if speed < 1024.0*1024.0 { format!("{:.2} KB/s", speed/1024.0) }
                            else { format!("{:.2} MB/s", speed/(1024.0*1024.0)) };
                        println!("  大小: {}", size_str);
                        println!("  平均速度: {}", sp_str);
                        return;
                    },
                    Err(e) => {
                        eprintln!("× 下载错误: {:#}", e);
                        std::process::exit(1);
                    }
                }
            }
        }
    }

    let result: anyhow::Result<swiftfetch::DownloadResult> = rt.block_on(async move {
        if protocol == ProtocolMode::BtOnly {
            let seq_counter_c = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let prev_state_c = Arc::new(parking_lot::Mutex::new(String::new()));
            let cb_cli_json = cli.json;
            let on_progress_raw: Arc<dyn Fn(ProgressInfo) + Send + Sync> = if cb_cli_json {
                let seq = seq_counter_c.clone();
                let ps = prev_state_c.clone();
                Arc::new(move |info: ProgressInfo| {
                    use std::io::Write;
                    let seqv = seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    let state_str = info.state.clone();
                    let eta_json = if info.eta_sec.is_some() {
                        serde_json::json!(info.eta_sec.unwrap())
                    } else {
                        serde_json::Value::Null
                    };
                    let json_obj = serde_json::json!({
                        "seq": seqv,
                        "task": info.task,
                        "state": info.state,
                        "progress": info.progress,
                        "downloaded": info.downloaded,
                        "total": info.total,
                        "speed_bps": info.speed_bps,
                        "eta_sec": eta_json,
                        "active_conns": info.active_conns,
                        "slow_bases": info.slow_bases,
                        "ts": ts,
                    });
                    if let Ok(line) = serde_json::to_string(&json_obj) {
                        let mut out = std::io::stdout();
                        let _ = writeln!(out, "{}", line);
                        let _ = out.flush();
                    }
                    let is_terminal = state_str == "completed" || state_str == "failed";
                    if is_terminal {
                        let mut guard = ps.lock();
                        if guard.as_str() != state_str {
                            *guard = state_str.clone();
                            let exit_code = if state_str == "completed" { 0 } else { 1 };
                            std::thread::spawn(move || {
                                std::thread::sleep(std::time::Duration::from_millis(200));
                                std::thread::sleep(std::time::Duration::from_millis(500));
                                std::process::exit(exit_code);
                            });
                        }
                    }
                })
            } else if cli.quiet {
                Arc::new(|_info: ProgressInfo| {})
            } else {
                Arc::new(|_info: ProgressInfo| {})
            };
            return run_with_progress_bar(
                cfg, protocol, net_mode, download_mode,
                torrent_abs, cli.magnet,
                cli.peer_limit, cli.bt_port,
                cli.ratio, cli.seed_minutes,
                cli.max_conns, cli.no_cross_protocol,
                Some(on_progress_raw), cli.json, cli.quiet,
            ).await;
        }
        if cli.json {
            let sf = SwiftFetch::new(cfg.clone());
            let seq_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let prev_state = Arc::new(parking_lot::Mutex::new(String::new()));
            let stop_notify_json = Arc::new(tokio::sync::Notify::new());
            let stop_notify_c = stop_notify_json.clone();
            let on_progress = move |info: ProgressInfo| {
                use std::io::Write;
                let seq = seq_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let state_str = info.state.clone();
                let eta_val = info.eta_sec.unwrap_or(0);
                let eta_json = if info.eta_sec.is_some() {
                    serde_json::json!(eta_val)
                } else {
                    serde_json::Value::Null
                };
                let json_obj = serde_json::json!({
                    "seq": seq,
                    "task": info.task,
                    "state": info.state,
                    "progress": info.progress,
                    "downloaded": info.downloaded,
                    "total": info.total,
                    "speed_bps": info.speed_bps,
                    "eta_sec": eta_json,
                    "active_conns": info.active_conns,
                    "slow_bases": info.slow_bases,
                    "ts": ts,
                });
                if let Ok(line) = serde_json::to_string(&json_obj) {
                    let mut out = std::io::stdout();
                    let _ = writeln!(out, "{}", line);
                    let _ = out.flush();
                }
                let is_terminal = state_str == "completed" || state_str == "failed";
                if is_terminal {
                    let mut guard = prev_state.lock();
                    if guard.as_str() != state_str {
                        *guard = state_str.clone();
                        let exit_code = if state_str == "completed" { 0 } else { 1 };
                        let stop_c = stop_notify_c.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(200));
                            stop_c.notify_waiters();
                            std::thread::sleep(std::time::Duration::from_millis(500));
                            std::process::exit(exit_code);
                        });
                    }
                }
            };
            sf.download(on_progress).await
        } else if cli.quiet {
            let sf = SwiftFetch::new(cfg.clone());
            let prev_state = std::sync::Arc::new(parking_lot::Mutex::new(String::from("starting")));
            let stop_notify = Arc::new(tokio::sync::Notify::new());
            let stop_notify_c = stop_notify.clone();
            sf.download(move |info: ProgressInfo| {
                let state_str = info.state.clone();
                let is_terminal = state_str == "completed" || state_str == "failed";
                if is_terminal {
                    let mut guard = prev_state.lock();
                    if guard.as_str() != state_str {
                        *guard = state_str.clone();
                        let exit_code = if state_str == "completed" { 0 } else { 1 };
                        let stop_c = stop_notify_c.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(200));
                            stop_c.notify_waiters();
                            std::thread::sleep(std::time::Duration::from_millis(500));
                            std::process::exit(exit_code);
                        });
                    }
                }
            }).await
        } else {
            println!("SwiftFetch v{} 启动...", env!("CARGO_PKG_VERSION"));
            println!("模式: {:?} | 下载顺序: {:?} | 网络: {:?}", protocol, download_mode, net_mode);
            if let Some(u) = &source_url {
                println!("主URL : {}", &u[..u.len().min(100)]);
            }
            if let Some(t) = &cli.torrent {
                println!("种子  : {}", t.display());
            }
            if cli.magnet.is_some() {
                println!("磁力链: [已提供]");
            }
            if !cfg.mirrors.is_empty() {
                println!("镜像数: {}", cfg.mirrors.len());
            }
            println!("并发 {} | 断点续传 {} | 代理 {} | 跨协议 {}",
                cfg.connections,
                if cli.no_resume { "关" } else { "开" },
                cfg.proxy.as_deref().unwrap_or("系统/无"),
                if cli.no_cross_protocol { "关" } else { "开" });
            if !cli.disable_plugin.is_empty() {
                println!("禁用插件: {}", cli.disable_plugin.join(", "));
            }
            if !cli.plugin_arg.is_empty() {
                println!("插件参数: {} 项", cli.plugin_arg.len());
            }
            println!();

            run_with_progress_bar(
                cfg, protocol, net_mode, download_mode,
                torrent_abs, cli.magnet,
                cli.peer_limit, cli.bt_port,
                cli.ratio, cli.seed_minutes,
                cli.max_conns, cli.no_cross_protocol,
                None, false, false,
            ).await
        }
    });

    match result {
        Ok(r) if r.success => {
            if !cli.json {
                if !cli.quiet { println!(); }
                println!("✓ 下载完成");
                println!("  输出: {}", r.output_path.display());
                println!("  大小: {}", format_bytes(r.file_size));
                println!("  平均速度: {}", format_speed(r.avg_speed_bps));
            }
        }
        Ok(r) => {
            eprintln!("× 下载失败: {}", r.message);
            std::process::exit(1);
        }
        Err(e) => {
            if cli.json {
                let json = serde_json::json!({"state": "failed", "error": e.to_string()});
                eprintln!("{}", json);
            } else {
                eprintln!("× 错误: {:#}", e);
            }
            std::process::exit(1);
        }
    }
}

fn format_capability_flags(caps: ProtocolCapability) -> String {
    use ProtocolCapability as PC;
    let mut flags = Vec::new();
    if caps.contains(PC::WHOLE_DOWNLOAD)    { flags.push("WHOLE"); }
    if caps.contains(PC::BYTE_RANGE)        { flags.push("RANGE"); }
    if caps.contains(PC::PARALLEL_STREAMS)  { flags.push("PARA"); }
    if caps.contains(PC::RESUME_SNAPSHOT)   { flags.push("RESUME"); }
    if caps.contains(PC::DIRECTORY_LIST)    { flags.push("LS"); }
    if caps.contains(PC::INTEGRITY_HASH)    { flags.push("HASH"); }
    if caps.contains(PC::MULTI_SOURCE_P2P)  { flags.push("P2P"); }
    if caps.contains(PC::HTTP2_MULTIPLEX)   { flags.push("H2-MUX"); }
    if caps.contains(PC::HTTP3_QUIC)        { flags.push("H3-QUIC"); }
    if caps.contains(PC::TRANSPORT_SECURE)  { flags.push("TLS"); }
    flags.join("|")
}

fn print_plugins_table(registry: &Arc<PluginRegistry>) {
    println!();
    println!("╔══════════════════════════════════════════════════════════════════════════════╗");
    println!("║          SwiftFetch v3 插件化架构 - 已注册插件列表                            ║");
    println!("╚══════════════════════════════════════════════════════════════════════════════╝");
    println!();
    println!("{:>4} │ {:<22} │ {:>10} │ {:>9} │ {:<14}", "ID", "NAME", "VERSION", "KIND", "HEALTH");
    println!("─────┼────────────────────────┼────────────┼───────────┼────────────────");
    let list = registry.list();
    for p in list {
        let v = p.version();
        let v_str = format!("{}.{}.{}", v.0, v.1, v.2);
        let kind = match p.kind() {
            PluginKind::AsyncThread => "Thread",
            PluginKind::IsolatedProcess => "Process",
        };
        println!("{:>4} │ {:<22} │ {:>10} │ {:>9} │ {:<14}",
            p.id().0, p.name(), v_str, kind, "Healthy");
    }
    println!();
    println!("共 {} 个插件", registry.list().len());
    println!("  - Thread : 同进程 tokio 任务 (高性能, 内置 HTTP/BT/Probe/Scheduler)");
    println!("  - Process: 独立子进程 IPC (故障隔离, plugins/*.exe / *.dll)");
    println!();
}

async fn run_with_progress_bar(
    cfg: DownloadConfig,
    protocol: ProtocolMode,
    net_mode: NetworkMode,
    download_mode: DownloadMode,
    torrent: Option<PathBuf>,
    magnet: Option<String>,
    peer_limit: Option<u32>,
    bt_port: u16,
    _ratio: f32,
    seed_minutes: u32,
    max_conns: Option<u32>,
    no_cross_protocol: bool,
    on_progress_override: Option<Arc<dyn Fn(ProgressInfo) + Send + Sync>>,
    use_json_mode: bool,
    use_quiet_mode: bool,
) -> anyhow::Result<DownloadResult> {
    use parking_lot::Mutex as PMutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::{Notify, Mutex as TMutex, Semaphore};
    use tokio::fs::{File, OpenOptions};
    use tokio::io::AsyncWriteExt;

    let start_instant = Instant::now();

    let (ev_tx, ev_rx) = flume::unbounded::<EngineEvent>();
    let (stop_tx, stop_rx) = flume::bounded::<()>(1);
    let stop_notify = Arc::new(Notify::new());

    let task_id = format!("sf3_{}", start_instant.elapsed().as_millis());

    let output_path = cfg.output.clone().unwrap_or_else(|| PathBuf::from("download.bin"));

    let (event_tx, _event_rx) = flume::unbounded::<EngineEvent>();

    let is_5g = net_mode == NetworkMode::FiveG;
    let bt_peer_default = if is_5g { FIVEG_PEER_LIMIT } else { DEFAULT_PEER_LIMIT };
    let global_max_default = if is_5g { FIVEG_GLOBAL_MAX_CONNS } else { DEFAULT_GLOBAL_MAX_CONNS };
    let http_limit_default = if is_5g { FIVEG_HTTP_MAX_CONNS } else { MAX_CONNECTIONS_PER_HOST };

    let bt_peer_limit_val = peer_limit.unwrap_or(bt_peer_default);
    let global_max_conns_val = max_conns.unwrap_or(global_max_default);

    let chunk_mgr_placeholder = Arc::new(HybridChunkManager::new(1, 1024 * 1024));

    let ctx0 = Arc::new(EngineContext {
        config: cfg.clone(),
        protocol,
        network_mode: net_mode,
        download_mode,
        probe: RwLockContainer::new(None),
        output_path: output_path.clone(),
        file_size: AtomicU64::new(0),
        base_chunk_size: AtomicU64::new(HYBRID_ALIGNED_BASE),
        chunk_mgr: chunk_mgr_placeholder,
        downloaded: Arc::new(AtomicU64::new(0)),
        http_downloaded: AtomicU64::new(0),
        bt_downloaded: AtomicU64::new(0),
        bt_total_received: AtomicU64::new(0),
        bt_dup_blocks: AtomicU64::new(0),
        file: Arc::new(TMutex::new(None)),
        active_http_conns: std::sync::atomic::AtomicU32::new(0),
        active_bt_conns: std::sync::atomic::AtomicU32::new(0),
        http_conn_limit: std::sync::atomic::AtomicU32::new(http_limit_default),
        bt_peer_limit: std::sync::atomic::AtomicU32::new(bt_peer_limit_val),
        global_max_conns: std::sync::atomic::AtomicU32::new(global_max_conns_val),
        sem_http: Arc::new(Semaphore::new(http_limit_default as usize)),
        sem_bt: Arc::new(Semaphore::new(bt_peer_limit_val as usize)),
        bandwidth_ema: Arc::new(BandwidthEMA::new()),
        event_tx: event_tx.clone(),
        event_rx: ev_rx,
        stop_notify: stop_notify.clone(),
        stop_event_tx: stop_tx.clone(),
        stop_event_rx: stop_rx.clone(),
        scheduler: PMutex::new(SmoothScheduler::new(
            cfg.connections, 10_000_000, 32 * 1024 * 1024
        )),
        speed_smoother: PMutex::new(SpeedSmoother::new()),
        bt_ema_speed: AtomicU64::new(0),
        oscillation_guard: PMutex::new(OscillationGuard::new()),
        base_chunk_done: PMutex::new(Vec::new()),
        bt_piece_map_completed: PMutex::new(Vec::new()),
        bt_blocks_done: crate::modules::new_sharded_block_set(),
        bt_blocks_inflight: crate::modules::new_sharded_block_set(),
        bt_piece_block_counts: PMutex::new(Vec::new()),
        bt_piece_size: AtomicU64::new(256 * 1024),
        bt_total_pieces: std::sync::atomic::AtomicU32::new(0),
        bt_request_block: std::sync::atomic::AtomicU64::new(crate::modules::choose_bt_request_block(256 * 1024)),
        peer_scores: PMutex::new(std::collections::HashMap::new()),
        bt_seeders: std::sync::atomic::AtomicU32::new(0),
        bt_peers: std::sync::atomic::AtomicU32::new(0),
        http_weight: std::sync::atomic::AtomicU64::new(1000),
        bt_weight: std::sync::atomic::AtomicU64::new(1000),
        http_ratio_target: std::sync::atomic::AtomicU64::new(0.6f64.to_bits()),
        bt_ratio_target: std::sync::atomic::AtomicU64::new(0.4f64.to_bits()),
        last_reset_count: std::sync::atomic::AtomicU32::new(0),
        last_reset_window: PRwLock::new(std::collections::VecDeque::new()),
        conn_delay_ms: AtomicU64::new(0),
        completed_time_series: PMutex::new(Vec::new()),
        prefetch_warmed: PMutex::new(std::collections::HashMap::new()),
        slow_subchunks: PMutex::new(std::collections::HashMap::new()),
        mirrors: cfg.mirrors.clone(),
        peer_port: std::sync::atomic::AtomicU32::new(bt_port as u32),
        ratio_target: std::sync::atomic::AtomicU64::new(1.0f64.to_bits()),
        seed_minutes: std::sync::atomic::AtomicU32::new(seed_minutes),
        task_id: task_id.clone(),
        start_instant,
        no_cross_protocol,
        bt_dht_node_id: PRwLock::new(None),
        bt_listen_port: std::sync::atomic::AtomicU32::new(bt_port as u32),
        bt_incoming_listener: PMutex::new(None),
        bt_file_handles: PMutex::new(std::collections::HashMap::new()),
        bt_have_txs: PMutex::new(Vec::new()),
        bt_uploaded: std::sync::atomic::AtomicU64::new(0),
        bt_upload_requests: std::sync::atomic::AtomicU64::new(0),
        bt_unchoke_sent: std::sync::atomic::AtomicU64::new(0),
        bt_conn_tcp_ok: std::sync::atomic::AtomicU64::new(0),
        bt_conn_utp_ok: std::sync::atomic::AtomicU64::new(0),
        bt_conn_fail: std::sync::atomic::AtomicU64::new(0),
        bt_unchoked_now: std::sync::atomic::AtomicI32::new(0),
        bt_pex_tx: PMutex::new(None),
        bt_endgame_mode: std::sync::atomic::AtomicBool::new(false),
        bt_piece_availability: PMutex::new(Vec::new()),
        bt_piece_verify_fails: PMutex::new(std::collections::HashMap::new()),
        bt_live_peers: PMutex::new(None),
    });
    let _ = (ev_tx, stop_rx.clone(), _event_rx);

    if protocol != ProtocolMode::BtOnly {
        let sf = SwiftFetch::new(cfg.clone());
        if let Some(override_cb) = on_progress_override.clone() {
            return sf.download(move |info| override_cb(info)).await;
        }
        if use_quiet_mode || use_json_mode {
            let prev_state = std::sync::Arc::new(parking_lot::Mutex::new(String::from("starting")));
            let stop_notify_c = stop_notify.clone();
            return sf.download(move |info: ProgressInfo| {
                let state_str = info.state.clone();
                let is_terminal = state_str == "completed" || state_str == "failed";
                if is_terminal {
                    let mut guard = prev_state.lock();
                    if guard.as_str() != state_str {
                        *guard = state_str.clone();
                        let exit_code = if state_str == "completed" { 0 } else { 1 };
                        let stop_c = stop_notify_c.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(200));
                            stop_c.notify_waiters();
                            std::thread::sleep(std::time::Duration::from_millis(500));
                            std::process::exit(exit_code);
                        });
                    }
                }
            }).await;
        }
        let last_line = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let last_line_c = last_line.clone();
        let prev_state = std::sync::Arc::new(parking_lot::Mutex::new(String::from("starting")));
        let stop_notify_c = stop_notify.clone();
        let on_progress = move |info: ProgressInfo| {
            let bar = format_progress_bar(info.progress, 20);
            let speed = format_speed(info.speed_bps);
            let dl = format_bytes(info.downloaded);
            let total = format_bytes(info.total);
            let eta = info.eta_sec
                .map(|s| format!("{}:{:02}", s / 60, s % 60))
                .unwrap_or_else(|| "--:--".to_string());
            let slow_tag = if info.slow_bases > 0 {
                format!(" 慢块:{}", info.slow_bases)
            } else { String::new() };
            let line = format!(
                "\r {} {:>6.2}% | ETA {} | 速度 {:>11} | {}/{} | 活跃:{:>2}{}   ",
                bar, info.progress, eta, speed, dl, total, info.active_conns, slow_tag
            );
            let mut stored = last_line_c.lock().unwrap();
            if *stored != line {
                *stored = line.clone();
                let line = if info.progress >= 100.0 {
                    format!("{}\n", line.trim_end())
                } else { line };
                let _ = std::io::Write::write_all(&mut std::io::stdout(), line.as_bytes());
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            if info.progress >= 100.0 {
                let elapsed = start_instant.elapsed().as_secs_f64();
                println!();
                println!("耗时 {:.1}s", elapsed);
            }
            let state_str = info.state.clone();
            let is_terminal = state_str == "completed" || state_str == "failed";
            if is_terminal {
                let mut guard = prev_state.lock();
                if guard.as_str() != state_str {
                    *guard = state_str.clone();
                    let exit_code = if state_str == "completed" { 0 } else { 1 };
                    let stop_c = stop_notify_c.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        stop_c.notify_waiters();
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        std::process::exit(exit_code);
                    });
                }
            }
        };
        return sf.download(on_progress).await;
    }

    let final_url = cfg.url.clone();
    let _ = final_url;
    let client = SwiftFetch::build_client_static(&cfg, is_5g, net_mode == NetworkMode::Wired25G)?;
    let probe = if protocol != ProtocolMode::BtOnly {
        Some(SwiftFetch::probe_static(&cfg, &client).await?)
    } else { None };

    let file_size = if let Some(p) = &probe { p.file_size }
        else {
            // ---- CLI BT 路径: 预解析 meta (对齐 downloader_manager.rs 的修复逻辑) ----
            // Hybrid模式下, HttpOnly 或 BtOnly 都会走这里读 meta 用 real total_size 创建 chunk_mgr
            let mut fs_from_bt = 0u64;
            let rt = tokio::runtime::Handle::try_current();
            if let Ok(handle) = rt {
                // 已经在 tokio 上下文里: 直接 .await
                let meta_res: anyhow::Result<TorrentMeta> =
                    crate::bt_engine::pre_resolve_bt_meta(torrent.as_deref(), magnet.as_deref())
                        .await;
                if let Ok(tm) = &meta_res {
                    fs_from_bt = tm.total_size;
                }
            } else {
                // Fallback: 非 tokio 上下文则阻塞读 (罕见情况, 如纯 CLI 还没进入 runtime)
                if let Some(tp) = &torrent {
                    if let Ok(data) = std::fs::read(tp) {
                        if let Ok(tm) = TorrentMeta::from_torrent_bytes(&data) {
                            fs_from_bt = tm.total_size;
                        }
                    }
                }
            }
            fs_from_bt
        };

    // ---- 关键修复: 预解析 BT meta → 用真实 total_size + aligned_base 创建 chunk_mgr
    // 对齐 downloader_manager.rs 中 #1~#4 的修复思路，避免 piece/base 错位
    let (base_chunk_size, actual_fs, base_chunk_size_fs) = if protocol == ProtocolMode::BtOnly {
        // 注意: run_with_progress_bar 是 async fn, 当前就在 tokio runtime 内 → 直接 .await
        let meta_res: anyhow::Result<TorrentMeta> =
            crate::bt_engine::pre_resolve_bt_meta(torrent.as_deref(), magnet.as_deref()).await;
        if let Ok(tm) = meta_res {
            let aligned = crate::bt_engine::calc_aligned_bt_base(&tm);
            eprintln!("[BT] 预解析 OK: total_size={}MB pieces={} aligned_base={}KB",
                tm.total_size / 1024 / 1024, tm.pieces.len(), aligned / 1024);
            (aligned, tm.total_size, tm.total_size)
        } else {
            // 降级 (magnet 或解析失败): 用 file_size, 无则 1MB 假值
            let fs = file_size.max(1024 * 1024);
            (cfg.calc_base_chunk_size_v3(fs), file_size, fs)
        }
    } else {
        // HTTP/Hybrid 用原来的逻辑
        let fs = file_size.max(1024 * 1024);
        (cfg.calc_base_chunk_size_v3(fs), file_size, fs)
    };
    let mgr = Arc::new(HybridChunkManager::new(base_chunk_size_fs, base_chunk_size));

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if output_path.file_name().is_some() && protocol != ProtocolMode::BtOnly {
        std::fs::create_dir_all(output_path.parent().unwrap_or(&output_path)).ok();
    } else if protocol == ProtocolMode::BtOnly {
        std::fs::create_dir_all(&output_path).ok();
    }
    let file_arc = Arc::new(TMutex::new(None));
    if protocol != ProtocolMode::BtOnly {
        let f = OpenOptions::new()
            .create(true).read(true).write(true)
            .open(&output_path).await?;
        f.set_len(actual_fs).await.ok();
        *file_arc.lock().await = Some(f);
    }
    let downloaded = Arc::new(AtomicU64::new(0));

    let bt_mod = Arc::new(BtDownloaderModule::new(
        None,
        magnet.clone(),
        torrent.clone(),
        bt_port,
    ));

    let callback: Arc<dyn Fn(ProgressInfo) + Send + Sync> = if let Some(override_cb) = on_progress_override.clone() {
        override_cb
    } else if use_quiet_mode || use_json_mode {
        let prev_state = std::sync::Arc::new(parking_lot::Mutex::new(String::from("starting")));
        let stop_notify_c = stop_notify.clone();
        Arc::new(move |info: ProgressInfo| {
            let state_str = info.state.clone();
            let is_terminal = state_str == "completed" || state_str == "failed";
            if is_terminal {
                let mut guard = prev_state.lock();
                if guard.as_str() != state_str {
                    *guard = state_str.clone();
                    let exit_code = if state_str == "completed" { 0 } else { 1 };
                    let stop_c = stop_notify_c.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        stop_c.notify_waiters();
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        std::process::exit(exit_code);
                    });
                }
            }
        })
    } else {
        let last_line = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let last_line_c = last_line.clone();
        let prev_state = std::sync::Arc::new(parking_lot::Mutex::new(String::from("starting")));
        let stop_notify_c = stop_notify.clone();
        Arc::new(move |info: ProgressInfo| {
            let bar = format_progress_bar(info.progress, 20);
            let speed = format_speed(info.speed_bps);
            let dl = format_bytes(info.downloaded);
            let total = format_bytes(info.total);
            let eta = info.eta_sec
                .map(|s| format!("{}:{:02}", s / 60, s % 60))
                .unwrap_or_else(|| "--:--".to_string());
            let slow_tag = if info.slow_bases > 0 {
                format!(" 慢块:{}", info.slow_bases)
            } else { String::new() };
            let line = format!(
                "\r {} {:>6.2}% | ETA {} | 速度 {:>11} | {}/{} | 活跃:{:>2}{}   ",
                bar, info.progress, eta, speed, dl, total, info.active_conns, slow_tag
            );
            let mut stored = last_line_c.lock().unwrap();
            if *stored != line {
                *stored = line.clone();
                let line = if info.progress >= 100.0 {
                    format!("{}\n", line.trim_end())
                } else { line };
                let _ = std::io::Write::write_all(&mut std::io::stdout(), line.as_bytes());
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            let state_str = info.state.clone();
            let is_terminal = state_str == "completed" || state_str == "failed";
            if is_terminal {
                let mut guard = prev_state.lock();
                if guard.as_str() != state_str {
                    *guard = state_str.clone();
                    let exit_code = if state_str == "completed" { 0 } else { 1 };
                    let stop_c = stop_notify_c.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        stop_c.notify_waiters();
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        std::process::exit(exit_code);
                    });
                }
            }
        })
    };

    let ctx_real = Arc::new(EngineContext {
        config: cfg.clone(),
        protocol,
        network_mode: net_mode,
        download_mode,
        probe: RwLockContainer::new(probe.clone()),
        output_path: output_path.clone(),
        file_size: AtomicU64::new(actual_fs),
        base_chunk_size: AtomicU64::new(base_chunk_size),
        chunk_mgr: mgr.clone(),
        downloaded: downloaded.clone(),
        http_downloaded: AtomicU64::new(0),
        bt_downloaded: AtomicU64::new(0),
        bt_total_received: AtomicU64::new(0),
        bt_dup_blocks: AtomicU64::new(0),
        file: file_arc.clone(),
        active_http_conns: std::sync::atomic::AtomicU32::new(0),
        active_bt_conns: std::sync::atomic::AtomicU32::new(0),
        http_conn_limit: std::sync::atomic::AtomicU32::new(http_limit_default),
        bt_peer_limit: std::sync::atomic::AtomicU32::new(bt_peer_limit_val),
        global_max_conns: std::sync::atomic::AtomicU32::new(global_max_conns_val),
        sem_http: Arc::new(Semaphore::new(http_limit_default as usize)),
        sem_bt: Arc::new(Semaphore::new(bt_peer_limit_val as usize)),
        bandwidth_ema: Arc::new(BandwidthEMA::new()),
        event_tx: event_tx.clone(),
        event_rx: flume::unbounded().1,
        stop_notify: stop_notify.clone(),
        stop_event_tx: stop_tx.clone(),
        stop_event_rx: stop_rx.clone(),
        scheduler: PMutex::new(SmoothScheduler::new(
            cfg.connections, 10_000_000, base_chunk_size
        )),
        speed_smoother: PMutex::new(SpeedSmoother::new()),
        bt_ema_speed: AtomicU64::new(0),
        oscillation_guard: PMutex::new(OscillationGuard::new()),
        base_chunk_done: PMutex::new(Vec::new()),
        bt_piece_map_completed: PMutex::new(Vec::new()),
        bt_blocks_done: crate::modules::new_sharded_block_set(),
        bt_blocks_inflight: crate::modules::new_sharded_block_set(),
        bt_piece_block_counts: PMutex::new(Vec::new()),
        bt_piece_size: AtomicU64::new(256 * 1024),
        bt_total_pieces: std::sync::atomic::AtomicU32::new(0),
        bt_request_block: std::sync::atomic::AtomicU64::new(crate::modules::choose_bt_request_block(256 * 1024)),
        peer_scores: PMutex::new(std::collections::HashMap::new()),
        bt_seeders: std::sync::atomic::AtomicU32::new(0),
        bt_peers: std::sync::atomic::AtomicU32::new(0),
        http_weight: std::sync::atomic::AtomicU64::new(1000),
        bt_weight: std::sync::atomic::AtomicU64::new(1000),
        http_ratio_target: std::sync::atomic::AtomicU64::new(0.6f64.to_bits()),
        bt_ratio_target: std::sync::atomic::AtomicU64::new(0.4f64.to_bits()),
        last_reset_count: std::sync::atomic::AtomicU32::new(0),
        last_reset_window: PRwLock::new(std::collections::VecDeque::new()),
        conn_delay_ms: AtomicU64::new(0),
        completed_time_series: PMutex::new(Vec::new()),
        prefetch_warmed: PMutex::new(std::collections::HashMap::new()),
        slow_subchunks: PMutex::new(std::collections::HashMap::new()),
        mirrors: cfg.mirrors.clone(),
        peer_port: std::sync::atomic::AtomicU32::new(bt_port as u32),
        ratio_target: std::sync::atomic::AtomicU64::new(1.0f64.to_bits()),
        seed_minutes: std::sync::atomic::AtomicU32::new(seed_minutes),
        task_id: task_id.clone(),
        start_instant,
        no_cross_protocol,
        bt_dht_node_id: PRwLock::new(None),
        bt_listen_port: std::sync::atomic::AtomicU32::new(bt_port as u32),
        bt_incoming_listener: PMutex::new(None),
        bt_file_handles: PMutex::new(std::collections::HashMap::new()),
        bt_have_txs: PMutex::new(Vec::new()),
        bt_uploaded: std::sync::atomic::AtomicU64::new(0),
        bt_upload_requests: std::sync::atomic::AtomicU64::new(0),
        bt_unchoke_sent: std::sync::atomic::AtomicU64::new(0),
        bt_conn_tcp_ok: std::sync::atomic::AtomicU64::new(0),
        bt_conn_utp_ok: std::sync::atomic::AtomicU64::new(0),
        bt_conn_fail: std::sync::atomic::AtomicU64::new(0),
        bt_unchoked_now: std::sync::atomic::AtomicI32::new(0),
        bt_pex_tx: PMutex::new(None),
        bt_endgame_mode: std::sync::atomic::AtomicBool::new(false),
        bt_piece_availability: PMutex::new(Vec::new()),
        bt_piece_verify_fails: PMutex::new(std::collections::HashMap::new()),
        bt_live_peers: PMutex::new(None),
    });

    let prog_mod = ProgressModule { callback };
    let builder = EngineBuilder::new()
        .register(BtDownloaderModule::new(None, magnet, torrent, bt_port))
        .register_arc(Arc::new(prog_mod))
        .register(SchedulerModule)
        .register(BandwidthPoolModule)
        .register(NATSessionGuardModule)
        .register(OscillationGuardModule);

    eprintln!("[MAIN_DEBUG] registered {} modules, protocol={:?}, starting run_all...", builder.modules().len(), protocol);
    let run_res = builder.run_all(ctx_real.clone()).await;
    eprintln!("[MAIN_DEBUG] run_all returned: {:?}", run_res.is_ok());
    let _ = run_res;

    {
        let mut f_guard = file_arc.lock().await;
        if let Some(f) = f_guard.as_mut() {
            let _ = f.flush().await;
            let _ = f.sync_all().await;
        }
        *f_guard = None;
    }
    if cfg.resume_enabled {
        let _ = SwiftFetch::remove_resume(&output_path);
    }

    let total_dl = downloaded.load(Ordering::Relaxed);
    let elapsed = start_instant.elapsed();
    let avg_speed = if elapsed.as_secs() > 0 { total_dl / elapsed.as_secs() } else { total_dl };

    // ★ 关键修复: BT 模式下 total_dl=0 说明没下载任何数据, 不能标记 success
    if protocol == ProtocolMode::BtOnly && total_dl == 0 && actual_fs > 0 {
        eprintln!("[MAIN_DEBUG] BT 下载失败: total_dl=0, file_size={}", actual_fs);
        return Ok(DownloadResult {
            success: false,
            message: format!("BT 下载未获取到任何数据 (file_size={})", actual_fs),
            output_path,
            file_size: actual_fs,
            elapsed_ms: elapsed.as_millis(),
            avg_speed_bps: 0,
        });
    }

    Ok(DownloadResult {
        success: total_dl > 0 || actual_fs == 0,
        message: "下载完成".into(),
        output_path,
        file_size: actual_fs,
        elapsed_ms: elapsed.as_millis(),
        avg_speed_bps: avg_speed,
    })
}
