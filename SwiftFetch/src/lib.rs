//! SwiftFetch v3 - 纯CLI高性能无UI下载内核
//!
//! v3 插件化解耦架构:
//! - 插件层: src/plugin.rs (AsyncThreadPlugin / IsolatedProcessPlugin + PluginRegistry)
//! - IPC 层:  src/ipc.rs    (JSON Lines + 消息节流 + 命名管道)
//! - 调度层:  src/host.rs   (PluginHost + ResumeWriterActor + 内置薄包装插件)
//! - 业务层:  speed_engine.rs / bt_engine.rs / modules.rs (原有逻辑, 不改业务)
//! - 协议抽象: src/protocols.rs (统一 ProtocolProvider trait + capability bitflags)
//! - 协议实现: src/protocols_impls.rs (HTTP1/2/3 FTP(S) SFTP WebDAV rsync IPFS)
//!
//! HTTP 特性: 多源镜像聚合, 慢分片重调度, 分片预取, TCP/HTTP2 自动调优
//! BT 特性  : 自研 wire protocol, magnet/.torrent, HTTP/BT 基底块 32MB 对齐
//! 新协议特性: FTP/FTPS (suppaftp), SFTP (openssh-sftp-client / ssh2), WebDAV (reqwest_dav),
//!             rsync (libsync3 xxhash3 + SSH), IPFS (Kubo RPC / Gateway), HTTP/3 (reqwest+quinn)

pub mod modules;
pub mod speed_engine;
pub mod bt_engine;
pub mod dynamic_engine;
/// ★ 智能调度核心 (带宽探测 / 动态块大小 / AIMD 并发 / 429 防护)
pub mod smart_sched;
pub mod dht;
pub mod upnp;
pub mod plugin;
pub mod ipc;
pub mod host;
pub mod protocols;
pub mod protocols_impls;

pub use modules::{
    DownloadModule, EngineBuilder, EngineContext, EngineEvent, PeerScore,
    BandwidthEMA, NetworkMode, DownloadMode, ProtocolMode, SourceHint,
    HybridSubChunk, RwLockContainer,
    MIN_BASE_SIZE_FOR_BT_ALIGN, HYBRID_ALIGNED_BASE, PREFETCH_WARM_BYTES,
    BT_REQUEST_BLOCK, DEFAULT_PEER_LIMIT, FIVEG_PEER_LIMIT,
    DEFAULT_GLOBAL_MAX_CONNS, FIVEG_GLOBAL_MAX_CONNS, FIVEG_HTTP_MAX_CONNS,
    DEFAULT_BT_PORT_START, DEFAULT_BT_PORT_END, DEFAULT_RATIO, DEFAULT_SEED_MINUTES,
    f64_to_atomic_store, f64_from_atomic_load,
};

pub use speed_engine::{
    SwiftFetch,
    DownloadConfig,
    DownloadResult,
    ProgressInfo,
    ProbeResult,
    HybridChunkManager,
    BaseChunk,
    SubChunk,
    SmoothScheduler,
    SpeedSmoother,
    OscillationGuard,
    AcquiredWork,
    SchedulerDecision,
    OscillationState,
    ResumeFile,
    HttpDownloaderModule,
    ProbeModule,
    PrefetchModule,
    OscillationGuardModule,
    SchedulerModule,
    BandwidthPoolModule,
    NATSessionGuardModule,
    ProgressModule,
    build_reqwest_client,
    MAX_CONNECTIONS_PER_HOST,
    DEFAULT_CONNECTIONS,
    TIMEOUT_CONNECT,
    TIMEOUT_READ,
    TIMEOUT_REQUEST,
    SUBCHUNK_READ_TIMEOUT,
    MIN_SUBCHUNK_SIZE,
    WORK_STEAL_REMAIN,
    PROBE_SAMPLE_BYTES,
    SPEED_SAMPLE_MS,
    SCHEDULER_COOLDOWN_MS,
    OSCILLATION_WINDOW_MS,
    OSCILLATION_THRESHOLD,
    OSCILLATION_UNFREEZE,
    FREEZE_DURATION_MS,
    EMA_ALPHA,
    SLOW_CHUNK_FACTOR,
    MAX_REDIRECTS,
    MAX_RETRIES,
    RESUME_EXT,
    format_speed,
    format_bytes,
    format_progress_bar,
};

pub use bt_engine::{
    BtDownloaderModule,
    TorrentMeta,
    TorrentFileInfo,
    BenValue,
    BenParser,
    BtMessage,
    PeerConnState,
    generate_peer_id,
    tracker_announce_http,
    peer_connect,
    peer_handshake_as_responder,
    pre_resolve_bt_meta,
    calc_aligned_bt_base,
    pick_bt_port,
    incoming_peer_acceptor,
    tracker_concurrent_announce,
    dht_fallback_get_peers,
};

// ===== dynamic_engine (v2 单层 Chunk + IDM 对半切分 + 真暂停) =====
pub use dynamic_engine::{
    Chunk, ChunkPool, DownloadEngine, DownloadError, EngineState as DynEngineState,
    DynamicScheduler, SpeedSmoother as DynSpeedSmoother, worker_main, progress_loop,
    download_file, MAX_CHUNK_SIZE, MIN_CHUNK_SIZE, SCHEDULER_COOLDOWN_MS as DYN_SCHED_COOLDOWN_MS,
    SPEED_EMA_ALPHA as DYN_SPEED_EMA_ALPHA, HIGH_RATIO as DYN_HIGH_RATIO,
    LOW_RATIO as DYN_LOW_RATIO, HIGH_STREAK as DYN_HIGH_STREAK, LOW_STREAK as DYN_LOW_STREAK,
    MIN_CONNS as DYN_MIN_CONNS, MAX_CONNS as DYN_MAX_CONNS,
    CHUNK_READ_TIMEOUT as DYN_CHUNK_READ_TIMEOUT, READ_TIMEOUT_RETRIES,
    DEFAULT_WORKER_COUNT, log_engine,
    // ★ 完全动态分块 + 多线程调度: 线程数/并发流数的唯一权威来源
    runtime_worker_threads, streams_for_threads,
    // ★ 授权限速器: 暴露给宿主, 以便"暂停→换密钥→继续"时就地改限速比例
    SpeedCap, RequestBucket,
};

// ===== dht (BEP-05 Mainline Kademlia DHT bootstrap) =====
pub use dht::{
    NodeId, NodeInfo, DhtClient, KrpcResponse, build_ping, build_find_node,
    build_get_peers, build_announce_peer, parse_krpc_response, extract_tx_id,
    encode_benvalue, generate_node_id, xor_distance, bootstrap_dht_get_peers,
    K as DHT_K, MAX_ITERATIONS as DHT_MAX_ITERATIONS, KRPC_TIMEOUT,
    COMPACT_NODE_LEN, BOOTSTRAP_NODES as DHT_BOOTSTRAP_NODES,
};

pub use plugin::{
    PluginId, PluginKind, PluginHealth, SwiftPlugin, PluginRegistry,
    PluginResult, PluginBox, PluginHost, HostBusMsg, PluginEventMsg,
    PluginMsg, PluginReply, ConnectionPool, ConnStats, ResumeDeltaMsg,
    AsyncThreadPlugin, IsolatedProcessPlugin, generate_req_id, IpcFrame,
};

pub use ipc::{
    HttpMethod, BtMethod, SchedMethod, ResumeMethod, EventTopic,
    MessageThrottler, CrashBackoff, IpcFramedReader, IpcFramedWriter,
    make_ipc_reader, make_ipc_writer, validate_req_id,
    make_request, make_reply, make_event,
};

pub use host::{
    HttpDownloaderPlugin, BtDownloaderPlugin, ProbePrefetchPlugin,
    SchedulerPlugin, PluginHostRuntime, ResumeWriterActor, format_plugin_table,
};

// ===== protocols (协议抽象 + 实现) =====
pub use protocols::{
    ProtocolProvider, ProtocolCapability, UrlScheme, AuthInfo,
    ResourceMeta, DirEntry, RangeRequest, ByteStream, ProviderRegistry, ProviderBox,
};
pub use protocols_impls::register_all_feature_providers;
pub use protocols_impls::{simple_provider_download, needs_provider_dispatch};

#[cfg(any(feature = "http", feature = "http2", feature = "http3"))]
pub use protocols_impls::HttpFamilyProvider;
#[cfg(feature = "ftp")]
pub use protocols_impls::FtpProvider;
#[cfg(feature = "webdav")]
pub use protocols_impls::WebdavProvider;
#[cfg(feature = "sftp")]
pub use protocols_impls::SftpProvider;
#[cfg(feature = "rsync")]
pub use protocols_impls::RsyncProvider;
#[cfg(feature = "ipfs")]
pub use protocols_impls::IpfsProvider;
