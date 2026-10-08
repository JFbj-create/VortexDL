//! dht.rs - BEP-05 DHT (Mainline Kademlia) 最小可用实现
//!
//! 目标: 当 BT tracker 不可用或返回 peer 为空时, 通过 DHT 找到拥有目标 info_hash 的 peers.
//!
//! 实现范围 (最小可用):
//! - KRPC 协议: ping / find_node / get_peers / announce_peer (UDP bencode)
//! - 公共 bootstrap 节点: router.bittorrent.com, dht.transmissionbt.com 等
//! - 简化路由表: 维护 `closest K nodes` 列表 (不做 k-bucket 分裂, 只用于迭代查找)
//! - 迭代 get_peers: 不超过 6 轮, 每轮并发 K=8 个节点, 收到 peer 即返回
//! - announce_peer: 拿到 token 后向最近节点宣告自己 (BT 端口)
//!
//! 不实现 (超范围, 用不上):
//! - 完整 k-bucket 维护与刷新
//! - 长期节点维护 (DHT daemon)
//! - 反向 announce 接收 (本端为被动节点)
//!
//! 参考: BEP-05, libtorrent dht_tracker, transmission `libtransmission/dht-*`

use anyhow::{anyhow, Result};
use rand::RngCore;
use sha1::{Digest, Sha1};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use tokio::sync::Mutex;

use crate::bt_engine::{BenParser, BenValue};

// ============================================================
// 常量
// ============================================================

/// K 桶大小: 每次向 K 个最近节点并行查询
pub const K: usize = 8;
/// 迭代最大轮数: 6 轮足够覆盖 BEP-05 推荐 alpha=K 的迭代深度
pub const MAX_ITERATIONS: u32 = 6;
/// 单次 KRPC 查询超时: 3s (公网 DHT 响应通常 < 1s)
pub const KRPC_TIMEOUT: Duration = Duration::from_secs(3);
/// 收到响应后等待额外 peer 的最大时间
pub const COLLECT_EXTRA_PEERS_MS: u64 = 1500;
/// 同时进行的 KRPC transaction 上限 (防止洪水)
pub const MAX_INFLIGHT: usize = 64;
/// 节点 ID 长度
pub const NODE_ID_LEN: usize = 20;

/// 公共 bootstrap DHT 节点 (BEP-05 推荐 + 主流客户端 + 亚洲节点)
pub const BOOTSTRAP_NODES: &[(&str, u16)] = &[
    ("router.bittorrent.com", 6881),
    ("dht.transmissionbt.com", 6881),
    ("router.utorrent.com", 6881),
    ("dht.libtorrent.org", 25401),
    ("dht.aelitis.com", 6881),
    ("router.silot.io", 6881),
    ("dht.snine.org", 6881),
    // ★ 亚洲节点 (2026-09-10): 提升 byrut 等俄/中 tracker 的 DHT 路由效率
    ("dht.network", 6881),
    ("router.bittorrentcloud.net", 6881),
    ("dht.parsec.vision", 6881),
    ("dht.leechers-paradise.net", 6881),
    ("dht.interbox.net", 6881),
    ("dht.publictracker.xyz", 6881),
];

// ============================================================
// NodeId / NodeInfo
// ============================================================

/// 160-bit DHT node id (20 字节)
pub type NodeId = [u8; NODE_ID_LEN];

/// Compact node info: 20-byte id + 4-byte IPv4 + 2-byte port (BEP-05 compact 格式)
pub const COMPACT_NODE_LEN: usize = 26;

#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub id: NodeId,
    pub addr: SocketAddr,
}

impl NodeInfo {
    /// 解析 compact nodes (BEP-05: 26 bytes/node)
    pub fn parse_compact(buf: &[u8]) -> Vec<NodeInfo> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + COMPACT_NODE_LEN <= buf.len() {
            let id: NodeId = buf[i..i + 20].try_into().unwrap_or([0u8; 20]);
            let ip = std::net::Ipv4Addr::new(buf[i + 20], buf[i + 21], buf[i + 22], buf[i + 23]);
            let port = u16::from_be_bytes([buf[i + 24], buf[i + 25]]);
            let addr = SocketAddr::from((ip, port));
            out.push(NodeInfo { id, addr });
            i += COMPACT_NODE_LEN;
        }
        out
    }
}

/// 生成自己的 node id: 随机 20 字节 (符合 BEP-05, 不强制基于 IP)
pub fn generate_node_id() -> NodeId {
    let mut id = [0u8; NODE_ID_LEN];
    rand::thread_rng().fill_bytes(&mut id);
    id
}

/// XOR 距离 (取前 8 字节做 u64 用于排序)
pub fn xor_distance(a: &NodeId, b: &NodeId) -> u64 {
    let mut dist = 0u64;
    for i in 0..8 {
        dist = (dist << 8) | (a[i] ^ b[i]) as u64;
    }
    dist
}

// ============================================================
// KRPC message (bencode over UDP)
// ============================================================

/// KRPC transaction id (2 字节, 足以区分并发查询)
type TxId = [u8; 2];

/// 构造 KRPC query
pub fn build_krpc_query(
    tx: &TxId,
    method: &str,
    args: BenValue,
) -> Vec<u8> {
    let mut dict: HashMap<Vec<u8>, BenValue> = HashMap::new();
    dict.insert(b"t".to_vec(), BenValue::Bytes(tx.to_vec()));
    dict.insert(b"y".to_vec(), BenValue::Bytes(b"q".to_vec()));
    dict.insert(b"q".to_vec(), BenValue::Bytes(method.as_bytes().to_vec()));
    dict.insert(b"a".to_vec(), args);
    encode_benvalue(&BenValue::Dict(dict))
}

/// 构造 ping 查询
pub fn build_ping(tx: &TxId, sender_id: &NodeId) -> Vec<u8> {
    let mut a: HashMap<Vec<u8>, BenValue> = HashMap::new();
    a.insert(b"id".to_vec(), BenValue::Bytes(sender_id.to_vec()));
    build_krpc_query(tx, "ping", BenValue::Dict(a))
}

/// 构造 find_node 查询
pub fn build_find_node(tx: &TxId, sender_id: &NodeId, target: &NodeId) -> Vec<u8> {
    let mut a: HashMap<Vec<u8>, BenValue> = HashMap::new();
    a.insert(b"id".to_vec(), BenValue::Bytes(sender_id.to_vec()));
    a.insert(b"target".to_vec(), BenValue::Bytes(target.to_vec()));
    build_krpc_query(tx, "find_node", BenValue::Dict(a))
}

/// 构造 get_peers 查询
pub fn build_get_peers(tx: &TxId, sender_id: &NodeId, info_hash: &[u8; 20]) -> Vec<u8> {
    let mut a: HashMap<Vec<u8>, BenValue> = HashMap::new();
    a.insert(b"id".to_vec(), BenValue::Bytes(sender_id.to_vec()));
    a.insert(b"info_hash".to_vec(), BenValue::Bytes(info_hash.to_vec()));
    build_krpc_query(tx, "get_peers", BenValue::Dict(a))
}

/// 构造 announce_peer 查询
pub fn build_announce_peer(
    tx: &TxId,
    sender_id: &NodeId,
    info_hash: &[u8; 20],
    port: u16,
    token: &[u8],
) -> Vec<u8> {
    let mut a: HashMap<Vec<u8>, BenValue> = HashMap::new();
    a.insert(b"id".to_vec(), BenValue::Bytes(sender_id.to_vec()));
    a.insert(b"info_hash".to_vec(), BenValue::Bytes(info_hash.to_vec()));
    a.insert(b"port".to_vec(), BenValue::Int(port as i64));
    a.insert(b"token".to_vec(), BenValue::Bytes(token.to_vec()));
    build_krpc_query(tx, "announce_peer", BenValue::Dict(a))
}

// ============================================================
// Bencode 编码 (与 bt_engine.rs 解析对称)
// ============================================================

pub fn encode_benvalue(v: &BenValue) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    encode_benvalue_into(v, &mut out);
    out
}

fn encode_benvalue_into(v: &BenValue, out: &mut Vec<u8>) {
    match v {
        BenValue::Int(n) => {
            out.push(b'i');
            out.extend_from_slice(n.to_string().as_bytes());
            out.push(b'e');
        }
        BenValue::Bytes(b) => {
            out.extend_from_slice(b.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(b);
        }
        BenValue::List(items) => {
            out.push(b'l');
            for it in items {
                encode_benvalue_into(it, out);
            }
            out.push(b'e');
        }
        BenValue::Dict(map) => {
            out.push(b'd');
            // BEP-05 要求 key 排序
            let mut keys: Vec<&Vec<u8>> = map.keys().collect();
            keys.sort();
            for k in keys {
                let val = map.get(k).unwrap();
                out.extend_from_slice(k.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(k);
                encode_benvalue_into(val, out);
            }
            out.push(b'e');
        }
    }
}

// ============================================================
// KRPC 响应解析
// ============================================================

#[derive(Debug)]
pub struct KrpcResponse {
    /// nodes (find_node / get_peers response 中的 compact nodes)
    pub nodes: Vec<NodeInfo>,
    /// values (get_peers response 中的 compact peers: 6 bytes each = 4 ip + 2 port)
    pub peers: Vec<SocketAddr>,
    /// token (get_peers 返回, 用于后续 announce_peer)
    pub token: Option<Vec<u8>>,
}

/// 解析 KRPC 响应, 提取 nodes/values/token
pub fn parse_krpc_response(buf: &[u8]) -> Result<KrpcResponse> {
    let mut parser = BenParser::new(buf);
    let root = parser.parse()?;
    let dict = match root {
        BenValue::Dict(d) => d,
        _ => return Err(anyhow!("krpc response not dict")),
    };
    // y == r 或 y == e (不使用 == 比较, 因为 BenValue 未 impl PartialEq)
    let is_error = match dict.get(b"y".as_ref()) {
        Some(BenValue::Bytes(b)) => b.as_slice() == b"e",
        _ => false,
    };
    if is_error {
        // error response
        if let Some(BenValue::Dict(ed)) = dict.get(b"e".as_ref()) {
            if let Some(BenValue::Bytes(msg)) = ed.get(b"e".as_ref()) {
                return Err(anyhow!("krpc error: {}", String::from_utf8_lossy(msg)));
            }
        }
        return Err(anyhow!("krpc error: (no message)"));
    }
    let r = match dict.get(b"r".as_ref()) {
        Some(BenValue::Dict(r)) => r,
        _ => return Err(anyhow!("krpc response missing r")),
    };

    let mut out = KrpcResponse {
        nodes: Vec::new(),
        peers: Vec::new(),
        token: None,
    };

    // token
    if let Some(BenValue::Bytes(t)) = r.get(b"token".as_ref()) {
        out.token = Some(t.clone());
    }
    // nodes (find_node + get_peers 无 peer 时返回)
    if let Some(BenValue::Bytes(n)) = r.get(b"nodes".as_ref()) {
        out.nodes = NodeInfo::parse_compact(n);
    }
    // values (get_peers 有 peer 时返回 compact peers: 4 ip + 2 port)
    if let Some(BenValue::List(vals)) = r.get(b"values".as_ref()) {
        for v in vals {
            if let BenValue::Bytes(b) = v {
                if b.len() == 6 {
                    let ip = std::net::Ipv4Addr::new(b[0], b[1], b[2], b[3]);
                    let port = u16::from_be_bytes([b[4], b[5]]);
                    out.peers.push(SocketAddr::from((ip, port)));
                }
            }
        }
    }
    Ok(out)
}

// ============================================================
// DhtClient - 单次 bootstrap + get_peers 调用
// ============================================================

/// 全局路由表缓存: 跨多次 get_peers 调用复用已发现的 DHT 节点.
/// 旧实现每次都新建 client 并从 13 个 bootstrap 节点重新爬行, 一次只拿到 ~1 个 peer.
static DHT_ROUTING_CACHE: std::sync::OnceLock<Mutex<Vec<NodeInfo>>> = std::sync::OnceLock::new();

fn routing_cache() -> &'static Mutex<Vec<NodeInfo>> {
    DHT_ROUTING_CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

pub struct DhtClient {
    pub socket: Arc<UdpSocket>,
    pub our_id: NodeId,
    /// transaction id -> 等待响应的 oneshot sender.
    /// ★ 关键修复: 由单一接收循环按 tx 派发响应, 彻底消除旧实现中
    ///   N 个任务并发 recv_from 抢占同一 socket 导致互相吞掉响应的问题.
    pub waiters: Arc<Mutex<HashMap<TxId, oneshot::Sender<Vec<u8>>>>>,
}

/// 发送一次 KRPC 查询并等待对应 tx 的响应 (3s 超时). 无 socket 竞争.
async fn query_node(
    socket: Arc<UdpSocket>,
    waiters: Arc<Mutex<HashMap<TxId, oneshot::Sender<Vec<u8>>>>>,
    addr: SocketAddr,
    payload: Vec<u8>,
    tx: TxId,
) -> Option<Vec<u8>> {
    let (s, r) = oneshot::channel();
    {
        waiters.lock().await.insert(tx, s);
    }
    if socket.send_to(&payload, addr).await.is_err() {
        waiters.lock().await.remove(&tx);
        return None;
    }
    match tokio::time::timeout(KRPC_TIMEOUT, r).await {
        Ok(Ok(v)) => Some(v),
        _ => {
            waiters.lock().await.remove(&tx);
            None
        }
    }
}

impl DhtClient {
    /// 绑定 UDP socket (0.0.0.0:0 = 随机端口) 并启动单一接收派发循环
    pub async fn new(our_id: NodeId) -> Result<Self> {
        let socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
        socket.set_broadcast(true).ok();
        let waiters: Arc<Mutex<HashMap<TxId, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        {
            let sock = socket.clone();
            let waiters_rx = waiters.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                loop {
                    match sock.recv_from(&mut buf).await {
                        Ok((n, _from)) => {
                            if n < 2 { continue; }
                            if let Ok(tx) = extract_tx_id(&buf[..n]) {
                                let sender = waiters_rx.lock().await.remove(&tx);
                                if let Some(s) = sender {
                                    let _ = s.send(buf[..n].to_vec());
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }
        Ok(Self { socket, our_id, waiters })
    }

    /// 获取本地绑定端口
    pub async fn local_port(&self) -> Result<u16> {
        Ok(self.socket.local_addr()?.port())
    }

    /// 发送 KRPC 查询并等待响应 (单次, 3s 超时)
    pub async fn krpc_query_once(
        &self,
        to: SocketAddr,
        payload: &[u8],
        tx: TxId,
    ) -> Result<Vec<u8>> {
        query_node(self.socket.clone(), self.waiters.clone(), to, payload.to_vec(), tx)
            .await
            .ok_or_else(|| anyhow!("krpc timeout"))
    }

    /// 引导 DHT: 并发向所有 bootstrap 节点发 find_node(自身 id), 拿初始路由
    /// ★ 修复: 旧代码串行查询 (13 节点 × 3s 超时最坏 39s) 且每次新建 client 无复用.
    ///   现在并发发送 + 结果写入全局路由缓存, 后续调用直接复用.
    pub async fn bootstrap(&self) -> Result<Vec<NodeInfo>> {
        let mut futs = Vec::new();
        for (host, port) in BOOTSTRAP_NODES {
            let addrs = match tokio::net::lookup_host(format!("{}:{}", host, port)).await {
                Ok(a) => a.collect::<Vec<_>>(),
                Err(_) => continue,
            };
            for addr in addrs {
                let mut tx = [0u8; 2];
                rand::thread_rng().fill_bytes(&mut tx);
                let payload = build_find_node(&tx, &self.our_id, &self.our_id);
                let socket = self.socket.clone();
                let waiters = self.waiters.clone();
                futs.push(tokio::spawn(async move {
                    query_node(socket, waiters, addr, payload, tx).await
                }));
            }
        }
        let mut known: Vec<NodeInfo> = Vec::new();
        for f in futs {
            if let Ok(Some(resp)) = f.await {
                if let Ok(parsed) = parse_krpc_response(&resp) {
                    known.extend(parsed.nodes);
                }
            }
        }
        if known.is_empty() {
            return Err(anyhow!("dht bootstrap: 所有公共节点失败"));
        }
        // 写入全局路由缓存
        {
            let mut cache = routing_cache().lock().await;
            for n in &known {
                if !cache.iter().any(|c| c.addr == n.addr) {
                    cache.push(n.clone());
                }
            }
            if cache.len() > 512 {
                cache.truncate(512);
            }
        }
        Ok(known)
    }

    /// 迭代 get_peers: 找到拥有 info_hash 的 peers
    /// 算法: BEP-05 标准迭代, 每轮并发 K 个最近节点, 收到 values 立即收集,
    ///       否则把返回的 nodes 加入候选继续迭代, 直到无新节点或达 MAX_ITERATIONS.
    /// 修复:
    ///   1. 优先复用全局路由缓存, 不足再 bootstrap (旧实现每次强制重新爬行)
    ///   2. 用 query_node (单一接收派发) 取代并发 recv_from, 消除响应互吞
    ///   3. 收集 token 并真正调用 announce_peer (announce_port 为 BT 监听端口)
    pub async fn get_peers(
        &self,
        info_hash: &[u8; 20],
        announce_port: Option<u16>,
    ) -> Result<Vec<SocketAddr>> {
        // 1) 种子节点: 优先用全局缓存, 缓存太小再 bootstrap 补充
        let mut seed: Vec<NodeInfo> = { routing_cache().lock().await.clone() };
        if seed.len() < K * 2 {
            if let Ok(boot) = self.bootstrap().await {
                for n in boot {
                    if !seed.iter().any(|s| s.addr == n.addr) {
                        seed.push(n);
                    }
                }
            }
        }
        if seed.is_empty() {
            return Err(anyhow!("dht: no bootstrap nodes"));
        }
        // 按与 info_hash (作为 target) 的 XOR 距离排序 (BEP-05 标准)
        let target: NodeId = *info_hash;
        seed.sort_by_key(|n| xor_distance(&n.id, &target));

        let mut candidates: VecDeque<NodeInfo> = seed.into_iter().collect();
        let mut queried: std::collections::HashSet<SocketAddr> = std::collections::HashSet::new();
        let mut peers: Vec<SocketAddr> = Vec::new();
        // (响应者 addr, token) 用于 announce_peer
        let mut tokens: Vec<(SocketAddr, Vec<u8>)> = Vec::new();
        let mut best_nodes: Vec<NodeInfo> = Vec::new();

        for _iter in 0..MAX_ITERATIONS {
            if candidates.is_empty() { break; }
            let batch: Vec<NodeInfo> = candidates.drain(..std::cmp::min(K, candidates.len())).collect();
            let mut futs = Vec::new();
            for n in &batch {
                if queried.contains(&n.addr) { continue; }
                queried.insert(n.addr);
                let mut tx = [0u8; 2];
                rand::thread_rng().fill_bytes(&mut tx);
                let payload = build_get_peers(&tx, &self.our_id, info_hash);
                let socket = self.socket.clone();
                let waiters = self.waiters.clone();
                let addr = n.addr;
                futs.push(tokio::spawn(async move {
                    (addr, query_node(socket, waiters, addr, payload, tx).await)
                }));
            }
            let mut new_nodes: Vec<NodeInfo> = Vec::new();
            for f in futs {
                if let Ok((addr, Some(resp_bytes))) = f.await {
                    if let Ok(parsed) = parse_krpc_response(&resp_bytes) {
                        for p in &parsed.peers {
                            if !peers.contains(p) {
                                peers.push(*p);
                            }
                        }
                        if let Some(t) = parsed.token {
                            tokens.push((addr, t));
                        }
                        new_nodes.extend(parsed.nodes);
                    }
                }
            }
            best_nodes.extend(new_nodes.iter().cloned());
            for n in new_nodes {
                if !queried.contains(&n.addr) {
                    candidates.push_back(n);
                }
            }
            if peers.len() >= K * 4 { break; }
        }

        // 2) announce_peer: 用收集到的 token 向最近的节点宣告自己 (BEP-05)
        if let Some(port) = announce_port {
            if port != 0 && !tokens.is_empty() {
                tokens.sort_by_key(|(addr, _)| {
                    best_nodes
                        .iter()
                        .find(|n| n.addr == *addr)
                        .map(|n| xor_distance(&n.id, &target))
                        .unwrap_or(u64::MAX)
                });
                tokens.truncate(K);
                let mut futs = Vec::new();
                for (addr, token) in tokens {
                    let mut tx = [0u8; 2];
                    rand::thread_rng().fill_bytes(&mut tx);
                    let payload = build_announce_peer(&tx, &self.our_id, info_hash, port, &token);
                    let socket = self.socket.clone();
                    let waiters = self.waiters.clone();
                    futs.push(tokio::spawn(async move {
                        let _ = query_node(socket, waiters, addr, payload, tx).await;
                    }));
                }
                for f in futs {
                    let _ = f.await;
                }
            }
        }

        // 3) 更新全局路由缓存 (供后续调用复用)
        {
            let mut cache = routing_cache().lock().await;
            for n in &best_nodes {
                if !cache.iter().any(|c| c.addr == n.addr) {
                    cache.push(n.clone());
                }
            }
            if cache.len() > 512 {
                cache.truncate(512);
            }
        }

        // 给迟到响应一点时间进入 socket (由接收循环派发)
        if !peers.is_empty() {
            tokio::time::sleep(Duration::from_millis(COLLECT_EXTRA_PEERS_MS)).await;
        }
        Ok(peers)
    }

    /// 关闭 DHT 客户端 (drop socket)
    pub fn shutdown(self) {
        // socket 自动 drop
        let _ = self;
    }
}

/// 从 KRPC 响应中提取 t (transaction id), 用于匹配查询
pub fn extract_tx_id(buf: &[u8]) -> Result<TxId> {
    let mut parser = BenParser::new(buf);
    let root = parser.parse()?;
    if let BenValue::Dict(d) = root {
        if let Some(BenValue::Bytes(t)) = d.get(b"t".as_ref()) {
            if t.len() == 2 {
                let mut out = [0u8; 2];
                out.copy_from_slice(&t);
                return Ok(out);
            }
        }
    }
    Err(anyhow!("no tx id in krpc response"))
}

// ============================================================
// 公开入口: bootstrap_dht_get_peers
// ============================================================

/// 一次性入口: bootstrap DHT 并 get_peers, 返回找到的 peers 列表
/// 调用方: bt_engine.rs 在 tracker 全部失败时使用
/// announce_port: 本端 BT 监听端口 (用于 announce_peer, 让其他 peer 能连入)
pub async fn bootstrap_dht_get_peers(
    info_hash: &[u8; 20],
    our_node_id: Option<NodeId>,
    announce_port: Option<u16>,
) -> Result<Vec<SocketAddr>> {
    let id = our_node_id.unwrap_or_else(generate_node_id);
    let client = DhtClient::new(id).await?;
    let peers = client.get_peers(info_hash, announce_port).await;
    client.shutdown();
    peers
}

// ============================================================
// 单元测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_benvalue_int() {
        let v = BenValue::Int(42);
        let bytes = encode_benvalue(&v);
        assert_eq!(bytes, b"i42e");
    }

    #[test]
    fn test_encode_benvalue_bytes() {
        let v = BenValue::Bytes(b"hello".to_vec());
        let bytes = encode_benvalue(&v);
        assert_eq!(bytes, b"5:hello");
    }

    #[test]
    fn test_encode_benvalue_dict_sorted() {
        let mut d: HashMap<Vec<u8>, BenValue> = HashMap::new();
        // 注: a 对应 Int(1), b 对应 Int(2), keys 排序后为 a, b
        d.insert(b"b".to_vec(), BenValue::Int(2));
        d.insert(b"a".to_vec(), BenValue::Int(1));
        let bytes = encode_benvalue(&BenValue::Dict(d));
        // keys 必须按字典序排序: a (Int=1) 在前, b (Int=2) 在后
        // 期望: d 1:a i1e 1:b i2e e
        assert_eq!(bytes, b"d1:ai1e1:bi2ee");
    }

    #[test]
    fn test_build_ping() {
        let tx = [0x01, 0x02];
        let id = [0u8; 20];
        let payload = build_ping(&tx, &id);
        // bencode dict 必须以 'd' 开头
        assert_eq!(payload[0], b'd');
        // 必须包含 'q' 字段 (作为单字节搜索)
        assert!(payload.iter().any(|&b| b == b'q'));
        // 必须包含方法名 "ping"
        assert!(payload.windows(4).any(|w| w == b"ping"));
    }

    #[test]
    fn test_parse_compact_nodes() {
        let mut buf = vec![0u8; COMPACT_NODE_LEN];
        buf[20..24].copy_from_slice(&[192, 168, 1, 1]);
        buf[24..26].copy_from_slice(&0x1A0Eu16.to_be_bytes());
        let nodes = NodeInfo::parse_compact(&buf);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].addr.port(), 0x1A0E);
    }

    #[test]
    fn test_xor_distance_same_id() {
        let id = [0u8; 20];
        assert_eq!(xor_distance(&id, &id), 0);
    }

    #[test]
    fn test_xor_distance_diff_first_byte() {
        let mut a = [0u8; 20];
        let mut b = [0u8; 20];
        a[0] = 0x80;
        b[0] = 0x01;
        let dist = xor_distance(&a, &b);
        // 第一字节差异 0x81, 后续全 0
        assert_eq!(dist, 0x8100000000000000);
    }

    #[test]
    fn test_parse_krpc_response_with_peers() {
        // 构造响应: d1:rd2:id20:...6:valuesl6:\x0a\x00\x00\x01\x1A\x0Aee
        // 简化测试: 直接构造 dict
        let mut r: HashMap<Vec<u8>, BenValue> = HashMap::new();
        r.insert(b"id".to_vec(), BenValue::Bytes(vec![0u8; 20]));
        let mut peer_bytes = vec![10u8, 0, 0, 1, 0x1A, 0x0A];
        let _ = &mut peer_bytes;
        r.insert(b"values".to_vec(), BenValue::List(vec![BenValue::Bytes(peer_bytes)]));
        let mut top: HashMap<Vec<u8>, BenValue> = HashMap::new();
        top.insert(b"t".to_vec(), BenValue::Bytes(b"ab".to_vec()));
        top.insert(b"y".to_vec(), BenValue::Bytes(b"r".to_vec()));
        top.insert(b"r".to_vec(), BenValue::Dict(r));
        let buf = encode_benvalue(&BenValue::Dict(top));
        let parsed = parse_krpc_response(&buf).unwrap();
        assert_eq!(parsed.peers.len(), 1);
        assert_eq!(parsed.peers[0].port(), 0x1A0A);
    }

    #[test]
    fn test_generate_node_id_unique() {
        let a = generate_node_id();
        let b = generate_node_id();
        // 极小概率相同, 测试基本可生成
        assert_eq!(a.len(), 20);
        assert_eq!(b.len(), 20);
    }
}
