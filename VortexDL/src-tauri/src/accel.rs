//! hosts 加速。
//!
//! # 机制（逆向 Steam++ 后自己实现，未复制其代码）
//!
//! Steam++ 的 hosts 加速是「改 hosts 把域名指到 127.0.0.1 + 本地起 443 反代」两件套。
//! 那需要安装一个 **LocalMachine\Root 根证书** —— 等于让本软件具备中间人解密
//! 任意 HTTPS 流量的能力，风险极高（它的 pfx 甚至是空密码存在用户目录里）。
//!
//! 本模块**不做 MITM**，只做第一半：把选定的域名在 hosts 里指向**可用的真实 IP**，
//! 靠 CDN 就近节点改善连通性。具体做法：
//!   1. 用公开 DoH（doh.pub / alidns）解析每个启用项目的域名；
//!   2. 对解析出的 IP 做 TCP 443 延迟探测，选最快的；
//!   3. 写进 hosts，并用标记块包裹，随时可一键还原；
//!   4. 写完调 `DnsFlushResolverCache` 刷新系统 DNS 缓存。
//!
//! # 为什么不做证书那一半（写在代码里，避免以后有人「补全」）
//!
//! 装根证书会带来三个真实风险：①中间人面无限扩大；②用户 Cookie/登录态经手；
//! ③绕过访问限制（Steam++ 用假 Googlebot UA 骗 age gate）违反服务条款。
//! 本软件不做这三件事。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 一个可加速的项目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccelItem {
    pub id: String,
    pub name: String,
    /// 分组（Steam / GitHub / 公共 CDN …）
    pub group: String,
    /// 要写进 hosts 的域名
    pub domains: Vec<String>,
    /// 一句话说明（给用户看为什么需要这个）
    #[serde(default)]
    pub note: String,
}

/// 用户的选择与结果
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccelState {
    /// 已启用的项目 id
    #[serde(default)]
    pub enabled: Vec<String>,
    /// 当前是否已写入 hosts
    #[serde(default)]
    pub active: bool,
    /// 上次解析结果：域名 -> IP
    #[serde(default)]
    pub resolved: Vec<(String, String)>,
    /// 上次操作结果说明
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub updated_at: i64,
}

fn state_path() -> PathBuf {
    crate::paths::data_dir().join("accel").join("state.json")
}

pub fn load_state() -> AccelState {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(s: &AccelState) {
    if let Some(d) = state_path().parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(t) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(state_path(), t);
    }
}

fn hosts_path() -> PathBuf {
    PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()))
        .join("System32")
        .join("drivers")
        .join("etc")
        .join("hosts")
}

const MARK_START: &str = "# VortexDL Accelerate Start";
const MARK_END: &str = "# VortexDL Accelerate End";
const BACKUP_START: &str = "# VortexDL Accelerate Backup Start";
const BACKUP_END: &str = "# VortexDL Accelerate Backup End";

/// 内置的加速项目清单。
///
/// ★ 这份清单是**自己整理**的公开域名（Valve/Steam 官方 CDN、GitHub 常用端点、
///   公共 CDN），不是从 Steam++ 的私有 API 抄的。域名本身是公开事实，
///   但整表复制别人的配置仍有条款风险，所以这里只放真正必要的一小批。
pub fn builtin_items() -> Vec<AccelItem> {
    fn it(id: &str, name: &str, group: &str, note: &str, domains: &[&str]) -> AccelItem {
        AccelItem {
            id: id.into(),
            name: name.into(),
            group: group.into(),
            note: note.into(),
            domains: domains.iter().map(|s| s.to_string()).collect(),
        }
    }
    vec![
        it(
            "steam-community",
            "Steam 社区",
            "Steam",
            "社区页面、讨论区、创意工坊",
            &["steamcommunity.com", "www.steamcommunity.com"],
        ),
        it(
            "steam-store",
            "Steam 商店",
            "Steam",
            "商店页、结算、愿望单",
            &["store.steampowered.com", "checkout.steampowered.com"],
        ),
        it(
            "steam-api",
            "Steam 接口 / 登录",
            "Steam",
            "API、登录、库存",
            &["api.steampowered.com", "login.steampowered.com", "help.steampowered.com"],
        ),
        it(
            "steam-cdn",
            "Steam 图片与 CDN",
            "Steam",
            "封面、截图、视频、下载 CDN",
            &[
                "cdn.akamai.steamstatic.com",
                "steamcdn-a.akamaihd.net",
                "steamuserimages-a.akamaihd.net",
                "shared.akamai.steamstatic.com",
            ],
        ),
        it(
            "github-web",
            "GitHub 网站",
            "GitHub",
            "仓库页、Issues、Raw 文件",
            &["github.com", "raw.githubusercontent.com", "gist.github.com"],
        ),
        it(
            "github-api",
            "GitHub API / 下载",
            "GitHub",
            "API、Release 附件下载（本软件自动更新也走这里）",
            &[
                "api.github.com",
                "objects.githubusercontent.com",
                "codeload.github.com",
            ],
        ),
        it(
            "google-cdn",
            "公共前端 CDN",
            "公共 CDN",
            "网页字体、图标库（很多站点的静态资源）",
            &[
                "fonts.googleapis.com",
                "fonts.gstatic.com",
                "ajax.googleapis.com",
                "cdn.jsdelivr.net",
                "unpkg.com",
            ],
        ),
        it(
            "common-avatars",
            "头像 / 图床",
            "公共 CDN",
            "Gravatar、常见头像与图片源",
            &["gravatar.com", "secure.gravatar.com", "i.imgur.com"],
        ),
    ]
}

/// 解析主机名（DoH JSON 接口）
async fn resolve_doh(host: &str) -> Option<String> {
    let endpoints = [
        "https://doh.pub/resolve",
        "https://dns.alidns.com/resolve",
    ];
    for ep in endpoints {
        let url = format!("{}?name={}&type=A", ep, host);
        let r = match crate::netdisk::http()
            .get(&url)
            .header("accept", "application/dns-json")
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => continue,
        };
        let v: serde_json::Value = match r.json().await {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(ans) = v.get("Answer").and_then(|x| x.as_array()) {
            for a in ans {
                if a.get("type").and_then(|x| x.as_u64()) == Some(1) {
                    if let Some(ip) = a.get("data").and_then(|x| x.as_str()) {
                        if !ip.is_empty() {
                            return Some(ip.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

/// 测一个 IP 的 443 连通延迟（毫秒）。不可达返回 None。
async fn probe(ip: &str, host: &str) -> Option<u128> {
    let t0 = std::time::Instant::now();
    let addr = format!("{}:443", ip);
    let f = tokio::net::TcpStream::connect(&addr);
    match tokio::time::timeout(std::time::Duration::from_millis(2500), f).await {
        Ok(Ok(_)) => {
            let _ = host;
            Some(t0.elapsed().as_millis())
        }
        _ => None,
    }
}

/// 把 hosts 里我们自己的块整段去掉（保留用户其他内容）
fn strip_block(text: &str) -> String {
    let mut out = String::new();
    let mut skip = false;
    for line in text.lines() {
        let t = line.trim();
        if t == MARK_START || t == BACKUP_START {
            skip = true;
            continue;
        }
        if t == MARK_END || t == BACKUP_END {
            skip = false;
            continue;
        }
        if skip {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out
}

/// 读取当前 hosts 内容（不存在就返回空）
pub fn read_hosts() -> Result<String, String> {
    std::fs::read_to_string(hosts_path()).map_err(|e| format!("读 hosts 失败: {}", e))
}

fn write_hosts(text: &str) -> Result<(), String> {
    let p = hosts_path();
    // 只读属性会导致写入失败
    if let Ok(md) = std::fs::metadata(&p) {
        let mut perm = md.permissions();
        if perm.readonly() {
            perm.set_readonly(false);
            let _ = std::fs::set_permissions(&p, perm);
        }
    }
    std::fs::write(&p, text).map_err(|e| {
        format!(
            "写 hosts 失败: {}（改 hosts 需要管理员权限，请用「以管理员身份运行」重开本软件）",
            e
        )
    })
}

/// 检测 hosts 里有没有**别人**写的加速块（Steam++ / Watt Toolkit 等）。
///
/// # 为什么要检测
///
/// 实测：Steam++ 启动加速后会往 hosts 写 `# Steam++ Start .. End` 块，全部指向
/// **127.0.0.1**，并且由它的 `Steam++.Accelerator.exe` 在 443 端口做本地反代。
/// 这种情况下再让本软件写「域名 → 真实 IP」，只会让两套规则互相打架：
/// 同名域名后写的生效，而且 Steam++ 一旦改写 hosts（它有 FileSystemWatcher
/// 会重新写回自己的块）我们的记录就被挤掉了。
///
/// 所以**检测到就提醒用户先关掉那一边**，而不是闷头覆盖。
pub fn detect_conflicts() -> Vec<String> {
    let mut out = Vec::new();
    let text = match read_hosts() {
        Ok(t) => t,
        Err(_) => return out,
    };
    let others = [
        ("Steam++", "# Steam++ Start"),
        ("Watt Toolkit", "# Watt Toolkit Start"),
        ("UsbEAm Hosts Editor", "# UsbEAm Hosts Editor"),
        ("SwitchHosts", "# SwitchHosts"),
    ];
    for (name, mark) in others {
        if text.contains(mark) {
            out.push(format!(
                "hosts 里已经有 {} 写的加速块（指向 127.0.0.1）。两套同时开启会互相覆盖，\
                 请先在那边停止加速再应用这里的设置。",
                name
            ));
        }
    }
    // 端口 443 被占用（多半是某个本地反代在跑）
    #[cfg(windows)]
    {
        if port_in_use(443) && out.is_empty() {
            out.push(
                "本机 443 端口已被占用（可能是另一个加速器在做本地反代）。\
                 本软件的加速只改 hosts、不监听端口，通常不受影响；\
                 但如果那个程序正在接管这些域名，效果会互相抵消。"
                    .to_string(),
            );
        }
    }
    out
}

#[cfg(windows)]
fn port_in_use(port: u16) -> bool {
    use std::net::{SocketAddr, TcpListener};
    TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).is_err()
}

/// 应用加速：解析 + 测速 + 写 hosts
pub async fn apply(enabled: Vec<String>) -> Result<AccelState, String> {
    let items = builtin_items();
    let picked: Vec<&AccelItem> = items.iter().filter(|i| enabled.contains(&i.id)).collect();
    if picked.is_empty() {
        return Err("没有选中任何加速项目".into());
    }

    // 1) 解析 + 测速（同一个域名可能解析出多个 IP 时取最快的那个；DoH 只给一个，
    //    所以这里对同一域名不做多 IP 比较，只验证可达性）
    let mut resolved: Vec<(String, String)> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    for item in &picked {
        for d in &item.domains {
            let ip = match resolve_doh(d).await {
                Some(ip) => ip,
                None => continue,
            };
            if probe(&ip, d).await.is_none() {
                continue;
            }
            resolved.push((d.clone(), ip.clone()));
            lines.push(format!("{} {}", ip, d));
        }
    }
    if lines.is_empty() {
        return Err("解析不到任何可用 IP（可能是网络本身不通，或 DoH 被拦截）".into());
    }

    // 2) 写 hosts：先去掉旧块，再追加新块
    let cur = read_hosts()?;
    let base = strip_block(&cur);
    let mut text = base;
    if !text.ends_with("\r\n") && !text.is_empty() {
        text.push_str("\r\n");
    }
    text.push_str(MARK_START);
    text.push_str("\r\n");
    for l in &lines {
        text.push_str(l);
        text.push_str("\r\n");
    }
    text.push_str(MARK_END);
    text.push_str("\r\n");
    write_hosts(&text)?;
    flush_dns();

    let st = AccelState {
        enabled,
        active: true,
        resolved,
        message: format!("已加速 {} 个域名", lines.len()),
        updated_at: crate::paths::now_ms(),
    };
    save_state(&st);
    crate::app_logger::log_command(
        "accel",
        &format!("hosts 加速已启用：{} 个域名", lines.len()),
    );
    Ok(st)
}

/// 关闭加速：删掉我们写的块
pub fn clear() -> Result<AccelState, String> {
    let cur = read_hosts()?;
    let text = strip_block(&cur);
    write_hosts(&text)?;
    flush_dns();
    let mut st = load_state();
    st.active = false;
    st.resolved.clear();
    st.message = "已还原 hosts".to_string();
    st.updated_at = crate::paths::now_ms();
    save_state(&st);
    crate::app_logger::log_command("accel", "hosts 加速已关闭，hosts 已还原");
    Ok(st)
}

/// hosts 是否被我们改过
pub fn is_active() -> bool {
    read_hosts().map(|t| t.contains(MARK_START)).unwrap_or(false)
}

/// 刷新系统 DNS 缓存（不改这个，写完 hosts 几分钟内还走旧记录）
fn flush_dns() {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut c = std::process::Command::new("ipconfig");
        c.args(["/flushdns"]).creation_flags(0x0800_0000);
        let _ = c.output();
    }
}

/// 只解析不写盘（界面上「测试」按钮用）
pub async fn test(enabled: Vec<String>) -> Result<Vec<(String, String)>, String> {
    let items = builtin_items();
    let mut out = Vec::new();
    for item in items.iter().filter(|i| enabled.contains(&i.id)) {
        for d in &item.domains {
            if let Some(ip) = resolve_doh(d).await {
                out.push((d.clone(), ip));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_block_keeps_user_lines() {
        let t = "127.0.0.1 localhost\r\n# 我的注释\r\n# VortexDL Accelerate Start\r\n1.2.3.4 a.com\r\n# VortexDL Accelerate End\r\n5.6.7.8 b.com\r\n";
        let o = strip_block(t);
        assert!(o.contains("127.0.0.1 localhost"), "{o}");
        assert!(o.contains("# 我的注释"), "{o}");
        assert!(o.contains("5.6.7.8 b.com"), "{o}");
        assert!(!o.contains("1.2.3.4 a.com"), "块内行没删掉: {o}");
        assert!(!o.contains(MARK_START), "{o}");
    }

    #[test]
    fn strip_block_is_idempotent() {
        let t = "a\r\n";
        let once = strip_block(t);
        let twice = strip_block(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn builtin_items_have_unique_ids() {
        let items = builtin_items();
        let mut ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        ids.sort();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "加速项目 id 有重复");
        assert!(n >= 6, "项目太少: {n}");
    }
}
