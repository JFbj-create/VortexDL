//! SwiftFetch v3 - 统一下载协议抽象层 (Protocol Provider Trait)
//!
//! 目标: 将 HTTP1/2/3 / FTP(S) / SFTP / WebDAV / rsync / IPFS / BitTorrent 所有下载源
//!       抽象为 **同一份字节流接口**, 由 HybridChunkManager + SmoothScheduler 统一调度.
//!
//! 设计原则:
//!   1. **能力位标志** (ProtocolCapability): 每个插件声明自己支持什么 (断点/分片/并行/目录列表)
//!   2. **元数据先行**: `fetch_metadata()` 必须在首次下载前拿到 file_size / etag / mtime
//!   3. **统一 Range 拉取**: `fetch_range(start, end)` 返回 Bytes Stream; 若协议不支持断点, 自动 fallback 全量
//!   4. **鉴权注入**: AuthInfo 封装 Basic/Digest/OAuth2/SSH-Key/TLS-Cert 等所有协议需要的认证
//!   5. **Feature-gated**: 每个协议 impl 都用 `#[cfg(feature = "xxx")]` 包起来, 默认只编 http + bittorrent

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::SystemTime;
use bitflags::bitflags;

// ============================================================================
// 1. 类型枚举
// ============================================================================

/// URL Scheme → 协议路由
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum UrlScheme {
    #[default]
    Http, Https,           // HTTP/1.1 HTTP/2 HTTP/3 共用, 实际版本在 capability 里声明
    Ftp, Ftps,             // FTP / FTP over TLS (explicit AUTH TLS)
    Sftp,                  // SSH File Transfer Protocol
    Webdav, Webdavs,       // WebDAV (HTTP 扩展), dav:// / davs:// 或 http(s):// 带 dav:// prefix
    Rsync, RsyncSsh,       // rsync:// 原生 daemon 或 rsync over SSH (user@host:path)
    Ipfs, Ipns,            // ipfs://<CID> / ipns://<name> 或对接 Kubo RPC
    Ed2k,                  // ed2k://|file|<name>|<size>|<md4-hash>|/|sources,...|
    Torrent, Magnet,       // .torrent 文件 / magnet:?xt=urn:btih:...
    Unknown,
}

impl UrlScheme {
    pub fn from_url(url: &str) -> Self {
        let lower = url.trim().to_ascii_lowercase();
        if lower.starts_with("https://") { Self::Https }
        else if lower.starts_with("http://") { Self::Http }
        else if lower.starts_with("ftps://") { Self::Ftps }
        else if lower.starts_with("ftp://") { Self::Ftp }
        else if lower.starts_with("sftp://") { Self::Sftp }
        else if lower.starts_with("davs://") { Self::Webdavs }
        else if lower.starts_with("webdavs://") { Self::Webdavs }
        else if lower.starts_with("dav://") { Self::Webdav }
        else if lower.starts_with("webdav://") { Self::Webdav }
        else if lower.starts_with("rsync+ssh://") { Self::RsyncSsh }
        else if lower.starts_with("rsync://") { Self::Rsync }
        else if lower.starts_with("ipfs://") { Self::Ipfs }
        else if lower.starts_with("ipns://") { Self::Ipns }
        else if lower.starts_with("ed2k://") { Self::Ed2k }
        else if lower.starts_with("magnet:") { Self::Magnet }
        else if lower.ends_with(".torrent") { Self::Torrent }
        else { Self::Unknown }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Http => "http", Self::Https => "https",
            Self::Ftp => "ftp", Self::Ftps => "ftps",
            Self::Sftp => "sftp",
            Self::Webdav => "dav", Self::Webdavs => "davs",
            Self::Rsync => "rsync", Self::RsyncSsh => "rsync+ssh",
            Self::Ipfs => "ipfs", Self::Ipns => "ipns",
            Self::Ed2k => "ed2k",
            Self::Torrent => "torrent", Self::Magnet => "magnet",
            Self::Unknown => "unknown",
        }
    }
}

// ============================================================================
// 2. 能力位标志 (Bitflags)
// ============================================================================

bitflags! {
    /// 协议能力位: 声明此 Provider 支持哪些高级特性
    /// (调度器会根据能力自动选择最合适的分片策略)
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct ProtocolCapability: u32 {
        /// 基础: 支持全量下载 (所有 Provider 都必须有)
        const WHOLE_DOWNLOAD    = 0b0000_0000_0000_0001;
        /// 支持字节级 Range 请求 → 可断点 / 可静态+动态分片 (HTTP/FTP REST/SFTP pread)
        const BYTE_RANGE        = 0b0000_0000_0000_0010;
        /// 支持并发多连接并行 (HTTP多连接 / FTP多控制连接 / SFTP多通道)
        const PARALLEL_STREAMS  = 0b0000_0000_0000_0100;
        /// 支持断点续传快照 (HTTP ETag / FTP REST / SFTP mtime+size / IPFS CID 不可变)
        const RESUME_SNAPSHOT   = 0b0000_0000_0000_1000;
        /// 支持目录列表 (LIST/PROPFIND/SFTP readdir/IPFS ls)
        const DIRECTORY_LIST    = 0b0000_0000_0001_0000;
        /// 支持写入 / 上传 (给未来上传功能留的, 当前下载器只读, 占位)
        const WRITE_SUPPORT     = 0b0000_0000_0010_0000;
        /// 协议内置校验和 (BT Piece SHA1, IPFS CID multihash, rsync xxhash)
        const INTEGRITY_HASH    = 0b0000_0000_0100_0000;
        /// P2P / 多源 (BT DHT/IPFS Bitswap)
        const MULTI_SOURCE_P2P  = 0b0000_0000_1000_0000;
        /// HTTP/2 多路复用
        const HTTP2_MULTIPLEX   = 0b0000_0001_0000_0000;
        /// HTTP/3 over QUIC (0-RTT, 连接迁移)
        const HTTP3_QUIC        = 0b0000_0010_0000_0000;
        /// 内置 TLS/SSL 加密 (HTTPS/FTPS/SFTP)
        const TRANSPORT_SECURE  = 0b0000_0100_0000_0000;
    }
}

impl Default for ProtocolCapability {
    fn default() -> Self { Self::WHOLE_DOWNLOAD }
}

// ============================================================================
// 3. 认证信息 (统一所有协议用一套 Auth)
// ============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum AuthInfo {
    #[default]
    Anonymous,
    /// HTTP Basic / FTP / WebDAV / SFTP 密码
    UserPass { username: String, password: String },
    /// HTTP Digest
    Digest { username: String, password: String },
    /// Bearer token (OAuth2 / Kubo RPC)
    BearerToken(String),
    /// SSH: 私钥路径 (PEM) + 可选 passphrase (SFTP / rsync over SSH)
    SshKey { key_path: PathBuf, passphrase: Option<String> },
    /// SSH Agent 转发 (从 SSH_AUTH_SOCK 获取)
    SshAgent,
    /// TLS 客户端证书 (HTTPS mTLS / FTPS 客户端认证)
    TlsClientCert { cert_pem_path: PathBuf, key_pem_path: PathBuf },
}

// ============================================================================
// 4. 资源元数据 (下载前必须拿到)
// ============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceMeta {
    /// 文件总字节数 (未知 = None, 如某些无限流)
    pub total_size: Option<u64>,
    /// ETag / Content-Hash / 强校验标识 (HTTP ETag / IPFS CID / BT InfoHash)
    pub etag: Option<String>,
    /// 最后修改时间
    pub mtime: Option<SystemTime>,
    /// 服务端声明的 MIME 类型
    pub mime: Option<String>,
    /// 建议保存的文件名 (从 Content-Disposition / FTP LIST / SFTP filename 提取)
    pub suggested_filename: Option<String>,
    /// 该资源对应的 scheme (便于调试追踪)
    pub scheme: UrlScheme,
    /// 原始资源定位符 (可包含鉴权 stripped 后的 URL)
    pub resource_id: String,
    /// 若是目录, 所含子条目列表 (DIRECTORY_LIST 能力下才填充)
    pub children: Vec<DirEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub mtime: Option<SystemTime>,
    pub resource_id: String,  // 子项的可下载 resource_id
}

// ============================================================================
// 5. 分片下载请求 / 结果
// ============================================================================

/// 分片字节流: `BoxStream<'static, std::io::Result<Bytes>>`
/// 要求: Stream 必须按字节序严格顺序产出 Bytes (不能乱序)
pub type ByteStream = BoxStream<'static, std::io::Result<Bytes>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeRequest {
    pub start: u64,      // 闭区间起始字节
    pub end_inclusive: u64,  // 闭区间结束字节 (若 == total_size-1 则到文件尾)
    pub priority: u8,    // 0-255, 255 = 最高优先级 (给 BT 稀有块先下载使用)
    pub req_id: String,  // 追踪用
}

// ============================================================================
// 6. 核心 Trait: ProtocolProvider
// ============================================================================

#[async_trait]
pub trait ProtocolProvider: Send + Sync + 'static {
    /// Provider 名称 (如 "http1", "http3", "ftp", "sftp", "webdav", "rsync", "ipfs", "bittorrent")
    fn name(&self) -> &'static str;
    /// 支持哪些 scheme (一个 Provider 可覆盖多个, 如 "http" provider 同时管 http+https)
    fn supported_schemes(&self) -> &'static [UrlScheme];
    /// 能力位标志 (调度器决策依据)
    fn capabilities(&self) -> ProtocolCapability;

    /// 探测 + 握手 + 认证 + 获取元数据 (**必须在 fetch_range 前至少调用一次**)
    /// 返回: (ResourceMeta, 内部连接句柄是否已建立 OK)
    async fn connect_and_probe(&self, resource_id: &str, auth: &AuthInfo)
        -> anyhow::Result<ResourceMeta>;

    /// 拉取一个字节区间 [start, end_inclusive]
    /// 返回: 按字节顺序产出的 ByteStream (必须保证顺序; 长度必须 = end_inclusive - start + 1)
    /// 若 capability 不含 BYTE_RANGE: 自动忽略 start/end, 返回 WHOLE 流, 调度器自行丢弃前面字节
    async fn fetch_range(&self, resource_id: &str, range: RangeRequest, auth: &AuthInfo)
        -> anyhow::Result<ByteStream>;

    /// (可选) 列出目录内容 → 填充 ResourceMeta.children
    async fn list_directory(&self, resource_id: &str, auth: &AuthInfo)
        -> anyhow::Result<Vec<DirEntry>> {
        let _ = (resource_id, auth);
        anyhow::bail!("此 Provider {} 不支持目录列表", self.name());
    }

    /// (可选) 优雅关闭底层连接池 / SSH session / BT swarm
    async fn shutdown(&self) -> anyhow::Result<()> { Ok(()) }
}

// ============================================================================
// 7. 协议注册中心 (给 PluginHost 在启动时扫描 feature 并自动注册)
// ============================================================================

pub type ProviderBox = std::sync::Arc<dyn ProtocolProvider>;

pub struct ProviderRegistry {
    inner: parking_lot::Mutex<Vec<ProviderBox>>,
    by_scheme: parking_lot::Mutex<std::collections::HashMap<UrlScheme, Vec<usize>>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            inner: parking_lot::Mutex::new(Vec::new()),
            by_scheme: parking_lot::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn register(&self, provider: ProviderBox) {
        let mut inner = self.inner.lock();
        let idx = inner.len();
        let schemes: Vec<UrlScheme> = provider.supported_schemes().to_vec();
        inner.push(provider);
        let mut by = self.by_scheme.lock();
        for s in schemes {
            by.entry(s).or_insert_with(Vec::new).push(idx);
        }
    }

    /// 为某个 URL 选择最佳 Provider (按 URL scheme 匹配, 取第一个已注册的 Provider)
    pub fn select_for_url(&self, url: &str) -> Option<ProviderBox> {
        let scheme = UrlScheme::from_url(url);
        let indices = self.by_scheme.lock().get(&scheme)?.clone();
        let inner = self.inner.lock();
        indices.first().and_then(|&i| inner.get(i).cloned())
    }

    pub fn list_all(&self) -> Vec<(&'static str, ProtocolCapability, &'static [UrlScheme])> {
        let inner = self.inner.lock();
        inner.iter().map(|p| (p.name(), p.capabilities(), p.supported_schemes())).collect()
    }
}

impl Default for ProviderRegistry { fn default() -> Self { Self::new() } }
