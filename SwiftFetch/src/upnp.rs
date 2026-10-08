//! UPnP IGD + NAT-PMP 端口映射 (2026-10, t78)
//!
//! 背景: BT 引擎此前只 bind 本地端口并 announce 给 tracker, 但从不向路由器申请
//!       端口映射. 在 NAT 网络下该端口从公网不可达 → 只能主动连出, 无法接受入站
//!       peer. 实测 (bt_t11/bt_t12): tracker 报 95/112/89/76 seeders, DHT 找到
//!       最多 130 peers, 但 1825 次连接仅 32 次握手成功 (1.8%) —— 大量 NAT 后的
//!       做种者无法回连我们, 吞吐被死死压在 ~0.5-0.7 MB/s.
//!
//! 本模块实现两条端口映射路径 (无需新增第三方依赖):
//!   1. UPnP IGD (BEP 无关, 通用): SSDP M-SEARCH 发现网关 → 拉取设备描述 XML →
//!      解析 WANIPConnection/WANPPPConnection 服务的 controlURL → SOAP AddPortMapping.
//!   2. NAT-PMP (RFC 6886): 向默认网关 5351 端口发送 Map TCP 请求 (网关 IP 由
//!      本机出网 IP 推断 x.x.x.1 / x.x.x.254).
//!
//! 两者均失败时静默降级 (不影响 BT 主流程).

use anyhow::{anyhow, Result};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::Notify;

const SSDP_ADDR: &str = "239.255.255.250:1900";
const NATPMP_PORT: u16 = 5351;
const NATPMP_LIFETIME: u32 = 3600;
const RENEW_SECS: u64 = 1500;

/// 端口映射结果 (供日志/诊断使用)
#[derive(Debug, Clone)]
pub struct PortMappingInfo {
    /// 外网端口 (通常等于内网端口)
    pub external_port: u16,
    /// 外网 IP (若网关返回)
    pub external_ip: Option<Ipv4Addr>,
    /// 映射方式: "UPnP" / "NAT-PMP"
    pub method: &'static str,
    /// 租约秒数 (0 = 永久)
    pub lifetime_secs: u32,
}

/// 尝试为 `local_port` (TCP) 建立端口映射: 先 UPnP, 后 NAT-PMP.
pub async fn try_map_port(local_port: u16) -> Option<PortMappingInfo> {
    if let Some(info) = try_upnp(local_port).await {
        return Some(info);
    }
    if let Some(info) = try_natpmp(local_port).await {
        return Some(info);
    }
    None
}

/// 后台守护: 建立映射并周期性续约 (路由器重启 / 租约到期后自动重建).
pub async fn port_mapping_keeper(local_port: u16, stop: Arc<Notify>) {
    loop {
        let info = try_map_port(local_port).await;
        match &info {
            Some(i) => {
                eprintln!(
                    "[BT_UPNP] 端口映射成功: method={} external_port={} external_ip={:?} lifetime={}s",
                    i.method, i.external_port, i.external_ip, i.lifetime_secs
                );
            }
            None => {
                eprintln!("[BT_UPNP] 端口映射失败: 未发现可用 IGD / NAT-PMP 网关 (仅能主动连出)");
            }
        }
        let wait = if info.is_some() {
            Duration::from_secs(RENEW_SECS)
        } else {
            Duration::from_secs(60)
        };
        tokio::select! {
            _ = stop.notified() => return,
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

// ============================================================
// UPnP IGD
// ============================================================

async fn try_upnp(local_port: u16) -> Option<PortMappingInfo> {
    let local_ip = local_ipv4().await?;
    let locations = ssdp_discover().await;
    if locations.is_empty() {
        return None;
    }
    let client = build_client()?;
    for loc in locations {
        let xml = match http_get_text(&client, &loc).await {
            Ok(x) => x,
            Err(_) => continue,
        };
        let base = tag_inner(&xml, "URLBase").unwrap_or_else(|| derive_base(&loc));
        let (service_type, control_url) = match find_wan_service(&xml, &base) {
            Some(x) => x,
            None => continue,
        };
        if soap_add_port(&client, &control_url, &service_type, local_ip, local_port, 0)
            .await
            .is_ok()
        {
            let external_ip = get_external_ip(&client, &control_url, &service_type).await;
            return Some(PortMappingInfo {
                external_port: local_port,
                external_ip,
                method: "UPnP",
                lifetime_secs: 0,
            });
        }
    }
    None
}

/// SSDP M-SEARCH 组播发现, 返回响应中的 LOCATION URL 列表.
async fn ssdp_discover() -> Vec<String> {
    let mut locations = Vec::new();
    let socket = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(_) => return locations,
    };
    let _ = socket.set_broadcast(true);
    let target: SocketAddr = match SSDP_ADDR.parse() {
        Ok(a) => a,
        Err(_) => return locations,
    };
    let msg = concat!(
        "M-SEARCH * HTTP/1.1\r\n",
        "HOST: 239.255.255.250:1900\r\n",
        "MAN: \"ssdp:discover\"\r\n",
        "MX: 2\r\n",
        "ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n",
        "\r\n"
    );
    for _ in 0..3 {
        let _ = socket.send_to(msg.as_bytes(), target).await;
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut buf = vec![0u8; 8192];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => {
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if let Some(loc) = extract_header(&text, "LOCATION") {
                    if !locations.contains(&loc) {
                        locations.push(loc);
                    }
                }
            }
            _ => break,
        }
    }
    locations
}

/// 提取 HTTP 头 (大小写不敏感)
fn extract_header(text: &str, name: &str) -> Option<String> {
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case(name) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// 解析 XML 标签内文本
fn tag_inner(xml: &str, tag: &str) -> Option<String> {
    let re = regex::Regex::new(&format!(
        r"(?is)<{}[^>]*>(.*?)</{}>",
        regex::escape(tag),
        regex::escape(tag)
    ))
    .ok()?;
    re.captures(xml).map(|c| c[1].trim().to_string())
}

/// 从 LOCATION 推导 base URL (scheme://host[:port])
fn derive_base(loc: &str) -> String {
    match url::Url::parse(loc) {
        Ok(u) => {
            let mut b = format!("{}://{}", u.scheme(), u.host_str().unwrap_or(""));
            if let Some(p) = u.port() {
                b.push_str(&format!(":{}", p));
            }
            b
        }
        Err(_) => String::new(),
    }
}

fn join_url(base: &str, rel: &str) -> String {
    let rel = rel.trim();
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    let base = base.trim_end_matches('/');
    if rel.starts_with('/') {
        format!("{}{}", base, rel)
    } else {
        format!("{}/{}", base, rel)
    }
}

/// 在设备描述中查找 WANIPConnection / WANPPPConnection 服务, 返回 (serviceType, 绝对 controlURL)
fn find_wan_service(xml: &str, base: &str) -> Option<(String, String)> {
    for block in xml.split("<service>").skip(1) {
        let b = block.split("</service>").next().unwrap_or("");
        let st = match tag_inner(b, "serviceType") {
            Some(s) => s,
            None => continue,
        };
        if st.contains("WANIPConnection") || st.contains("WANPPPConnection") {
            if let Some(cu) = tag_inner(b, "controlURL") {
                return Some((st, join_url(base, &cu)));
            }
        }
    }
    None
}

fn build_client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .build()
        .ok()
}

async fn http_get_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!("GET {} -> {}", url, resp.status()));
    }
    Ok(resp.text().await?)
}

async fn soap_add_port(
    client: &reqwest::Client,
    control_url: &str,
    service_type: &str,
    local_ip: Ipv4Addr,
    port: u16,
    lease: u32,
) -> Result<()> {
    let body = format!(
        r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:AddPortMapping xmlns:u="{st}">
<NewRemoteHost></NewRemoteHost>
<NewExternalPort>{port}</NewExternalPort>
<NewProtocol>TCP</NewProtocol>
<NewInternalPort>{port}</NewInternalPort>
<NewInternalClient>{ip}</NewInternalClient>
<NewEnabled>1</NewEnabled>
<NewPortMappingDescription>VortexDL</NewPortMappingDescription>
<NewLeaseDuration>{lease}</NewLeaseDuration>
</u:AddPortMapping>
</s:Body>
</s:Envelope>"#,
        st = service_type,
        port = port,
        ip = local_ip,
        lease = lease
    );
    let resp = client
        .post(control_url)
        .header("Content-Type", "text/xml; charset=\"utf-8\"")
        .header("SOAPAction", format!("\"{}#AddPortMapping\"", service_type))
        .body(body)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.is_success() && !text.contains("<s:Fault>") && !text.contains("UPnPError") {
        Ok(())
    } else {
        Err(anyhow!(
            "SOAP AddPortMapping {} : {}",
            status,
            text.chars().take(200).collect::<String>()
        ))
    }
}

async fn get_external_ip(
    client: &reqwest::Client,
    control_url: &str,
    service_type: &str,
) -> Option<Ipv4Addr> {
    let body = format!(
        r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:GetExternalIPAddress xmlns:u="{st}"></u:GetExternalIPAddress>
</s:Body>
</s:Envelope>"#,
        st = service_type
    );
    let resp = client
        .post(control_url)
        .header("Content-Type", "text/xml; charset=\"utf-8\"")
        .header(
            "SOAPAction",
            format!("\"{}#GetExternalIPAddress\"", service_type),
        )
        .body(body)
        .send()
        .await
        .ok()?;
    let text = resp.text().await.ok()?;
    tag_inner(&text, "NewExternalIPAddress").and_then(|s| s.parse::<Ipv4Addr>().ok())
}

// ============================================================
// NAT-PMP (RFC 6886)
// ============================================================

async fn try_natpmp(local_port: u16) -> Option<PortMappingInfo> {
    let local = local_ipv4().await?;
    let o = local.octets();
    let candidates = [
        Ipv4Addr::new(o[0], o[1], o[2], 1),
        Ipv4Addr::new(o[0], o[1], o[2], 254),
    ];
    for gw in candidates {
        if let Ok((ext_port, ext_ip)) = natpmp_map_tcp(gw, local_port, NATPMP_LIFETIME).await {
            return Some(PortMappingInfo {
                external_port: ext_port,
                external_ip: if ext_ip.is_unspecified() {
                    None
                } else {
                    Some(ext_ip)
                },
                method: "NAT-PMP",
                lifetime_secs: NATPMP_LIFETIME,
            });
        }
    }
    None
}

async fn natpmp_map_tcp(
    gateway: Ipv4Addr,
    port: u16,
    lifetime: u32,
) -> Result<(u16, Ipv4Addr)> {
    let sock = UdpSocket::bind(("0.0.0.0", 0)).await?;
    sock.connect((gateway, NATPMP_PORT)).await?;
    let mut buf = [0u8; 32];

    // opcode 0: 请求外网地址
    let req_pub = [0u8, 0u8];
    sock.send(&req_pub).await?;
    let mut ext_ip = Ipv4Addr::UNSPECIFIED;
    if let Ok(Ok(n)) = tokio::time::timeout(Duration::from_secs(2), sock.recv(&mut buf)).await {
        if n >= 12 && buf[1] == 128 && buf[2] == 0 {
            ext_ip = Ipv4Addr::new(buf[8], buf[9], buf[10], buf[11]);
        }
    }

    // opcode 2: 映射 TCP
    let mut req = [0u8; 12];
    req[1] = 2;
    req[4..6].copy_from_slice(&port.to_be_bytes());
    req[6..8].copy_from_slice(&port.to_be_bytes());
    req[8..12].copy_from_slice(&lifetime.to_be_bytes());
    sock.send(&req).await?;
    let n = tokio::time::timeout(Duration::from_secs(3), sock.recv(&mut buf))
        .await
        .map_err(|_| anyhow!("natpmp timeout"))??;
    if n < 16 {
        return Err(anyhow!("natpmp short response"));
    }
    if buf[1] != 130 {
        return Err(anyhow!("natpmp bad opcode {}", buf[1]));
    }
    let result = u16::from_be_bytes([buf[2], buf[3]]);
    if result != 0 {
        return Err(anyhow!("natpmp result code {}", result));
    }
    let ext_port = u16::from_be_bytes([buf[10], buf[11]]);
    Ok((ext_port, ext_ip))
}

// ============================================================
// 工具
// ============================================================

/// 取本机出网 IPv4 (不实际发包, 仅让 OS 选路)
async fn local_ipv4() -> Option<Ipv4Addr> {
    let s = UdpSocket::bind("0.0.0.0:0").await.ok()?;
    s.connect("8.8.8.8:80").await.ok()?;
    match s.local_addr().ok()? {
        SocketAddr::V4(a) => Some(*a.ip()),
        _ => None,
    }
}
