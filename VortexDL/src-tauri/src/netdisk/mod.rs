//! 网盘解析模块 —— 让下载器能直接吃网盘分享链接。
//!
//! # 设计
//!
//! ```text
//!   用户粘贴分享链接
//!        │
//!        ▼
//!   link::recognize()            ← 纯函数，识别是哪个网盘 + 分享 id + 提取码
//!        │
//!        ▼
//!   list_share()                 ← 各驱动自己去拿文件列表（PanEntry）
//!        │
//!        ▼
//!   resolve()                    ← 拿到**带签名的真实 CDN 直链** + 必须带的请求头
//!        │
//!        ▼
//!   交给现有的 start_download()   ← 多线程下载 / 断点续传 / 解压 / 快捷方式全都复用
//! ```
//!
//! # 为什么解析要带 cookie
//!
//! 网盘的下载直链是**按账号签名**的，没有登录态拿不到（夸克的游客通道还有大小上限）。
//! 所以这里有个 cookie 仓库：用户在「设置 → 网盘」里粘贴一次自己的登录 Cookie，
//! 之后解析都带着它。**我们不存账号密码**，只存用户自己贴的那串 Cookie / token。
//!
//! # 各驱动的实测状态（2026-10-12）
//!
//! | 网盘 | 分享解析 | 说明 |
//! |---|---|---|
//! | 夸克 | ✅ 已实现 | 接口来自 quarkpan-rs / OpenList / LinkSwift 的实证代码 |
//! | 百度 | ✅ 已实现 | 接口来自 BaiduPCS-Go / LinkSwift / OpenList 的实证代码 |
//! | 迅雷 | ⚠️ 仅自己网盘 | **分享链接解析全网没有公开的可用实现**（LinkSwift 遇到分享页直接抛错），
//! |      |            | 所以迅雷只支持「自己网盘里的文件」，分享链接会明确提示不支持 |
//!
//! ★ 这些接口都是**逆向网页/客户端**得来的，不是官方文档；网盘改版就会失效。
//!   所以每个驱动的报错都尽量把"是哪一步、网盘原话是什么"带出来，方便定位。
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

pub mod baidu;
pub mod link;
pub mod quark;
pub mod xunlei;

pub use link::{Drive, PanLink};

/// 各驱动统一用这个 UA 里的浏览器部分；网盘对 UA 挑得很细，别乱改。
pub const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/130.0.0.0 Safari/537.36";

/// 分享里的一个条目（文件或目录）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanEntry {
    /// 网盘内部 id
    pub fid: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// 转存/下载时需要的令牌（夸克的 share_fid_token、百度的 sekey 等）
    #[serde(default)]
    pub token: String,
    /// 目录层级路径，仅用于展示
    #[serde(default)]
    pub path: String,
}

/// 解析出来的真实下载直链
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanResolved {
    pub fid: String,
    pub name: String,
    pub size: u64,
    /// 带签名的 CDN 直链（会过期）
    pub url: String,
    /// 下载这个直链**必须**带的请求头。百度不伪装 `User-Agent: pan.baidu.com` 的话，
    /// 大于 50MB 的文件直接 403 —— 这是踩过的坑，别省。
    pub headers: Vec<(String, String)>,
}

// ============================================================
// Cookie 仓库
// ============================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PanCookie {
    #[serde(default)]
    pub cookie: String,
    /// 迅雷用 OAuth，存 refresh_token
    #[serde(default)]
    pub refresh_token: String,
    /// 迅雷需要稳定的 device_id（换了就要重新拿 captcha）
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub updated_at: i64,
}

fn store_path() -> PathBuf {
    crate::paths::data_dir().join("netdisk").join("cookies.json")
}

/// 读取全部 cookie
pub fn load_cookies() -> HashMap<String, PanCookie> {
    let p = store_path();
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_cookies(m: &HashMap<String, PanCookie>) -> Result<(), String> {
    let p = store_path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("创建目录失败: {}", e))?;
    }
    let s = serde_json::to_string_pretty(m).map_err(|e| e.to_string())?;
    std::fs::write(&p, s).map_err(|e| format!("写入失败: {}", e))
}

pub fn get_cookie(drive: Drive) -> PanCookie {
    load_cookies().get(drive.id()).cloned().unwrap_or_default()
}

pub fn set_cookie(drive: Drive, cookie: &str, refresh_token: &str) -> Result<(), String> {
    let mut m = load_cookies();
    let e = m.entry(drive.id().to_string()).or_default();
    if !cookie.trim().is_empty() {
        e.cookie = cookie.trim().to_string();
    }
    if !refresh_token.trim().is_empty() {
        e.refresh_token = refresh_token.trim().to_string();
    }
    if e.device_id.trim().is_empty() {
        // 迅雷的 device_id 必须稳定，第一次生成后一直用
        e.device_id = format!("{:032x}", crate::paths::now_ms() as u128 ^ 0x9e3779b97f4a7c15u128);
    }
    e.updated_at = crate::paths::now_ms();
    save_cookies(&m)
}

pub fn clear_cookie(drive: Drive) -> Result<(), String> {
    let mut m = load_cookies();
    m.remove(drive.id());
    save_cookies(&m)
}

/// 给前端看的状态（**不回传 cookie 本身**，只回长度和更新时间，避免明文在日志/界面里乱飞）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CookieStatus {
    pub drive: String,
    pub name: String,
    pub has_cookie: bool,
    pub has_refresh_token: bool,
    pub cookie_len: usize,
    pub updated_at: i64,
}

pub fn cookie_status() -> Vec<CookieStatus> {
    let m = load_cookies();
    [Drive::Quark, Drive::Baidu, Drive::Xunlei]
        .iter()
        .map(|d| {
            let c = m.get(d.id()).cloned().unwrap_or_default();
            CookieStatus {
                drive: d.id().to_string(),
                name: d.name().to_string(),
                has_cookie: !c.cookie.trim().is_empty(),
                has_refresh_token: !c.refresh_token.trim().is_empty(),
                cookie_len: c.cookie.len(),
                updated_at: c.updated_at,
            }
        })
        .collect()
}

// ============================================================
// 统一入口
// ============================================================

/// 列出分享里的条目（夸克/百度）。迅雷分享暂不支持，会返回明确错误。
pub async fn list_share(link: &PanLink, passcode: &str) -> Result<Vec<PanEntry>, String> {
    let pass = if passcode.trim().is_empty() {
        link.passcode.clone().unwrap_or_default()
    } else {
        passcode.trim().to_string()
    };
    match link.drive {
        Drive::Quark => quark::list_share(&link.share_id, &pass).await,
        Drive::Baidu => baidu::list_share(&link.share_id, &pass).await,
        Drive::Xunlei => Err(
            "迅雷云盘的「分享链接」目前没有可用的解析方式（全网没有公开可用的实现）。\
             如果你要下的是自己迅雷网盘里的文件，请切到「我的网盘」，并把迅雷 refresh_token 填到登录设置里。"
                .to_string(),
        ),
    }
}

/// 把选中的条目解析成真实直链。
pub async fn resolve(
    link: &PanLink,
    passcode: &str,
    entries: &[PanEntry],
) -> Result<Vec<PanResolved>, String> {
    let pass = if passcode.trim().is_empty() {
        link.passcode.clone().unwrap_or_default()
    } else {
        passcode.trim().to_string()
    };
    match link.drive {
        Drive::Quark => quark::resolve(&link.share_id, &pass, entries).await,
        Drive::Baidu => baidu::resolve(&link.share_id, &pass, entries).await,
        Drive::Xunlei => Err("迅雷云盘的分享链接解析暂不支持".to_string()),
    }
}

/// 列自己网盘里的文件（目前只有迅雷做了这一路）
pub async fn list_mine(drive: Drive, parent_id: &str) -> Result<Vec<PanEntry>, String> {
    match drive {
        Drive::Xunlei => xunlei::list_mine(parent_id).await,
        _ => Err(format!("{} 的「我的网盘」还没做，请用分享链接方式", drive.name())),
    }
}

pub async fn resolve_mine(drive: Drive, entries: &[PanEntry]) -> Result<Vec<PanResolved>, String> {
    match drive {
        Drive::Xunlei => xunlei::resolve_mine(entries).await,
        _ => Err(format!("{} 的「我的网盘」还没做", drive.name())),
    }
}

/// 各驱动共用的 http 客户端。
/// ★ 不启用 cookie jar：登录态一律由我们显式带 `Cookie` 头，避免会话互相污染。
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(BROWSER_UA)
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// ★ 共享的 http 客户端（连接池 + TLS 会话复用）。
///   千万别在每次请求时 `client()` 新建 —— 那等于每次都重建连接池、重做 TLS 握手。
pub fn http() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(client)
}

/// 不允许自动跟随跳转的客户端（百度拿 dlink 时要看 Location 头）
pub fn client_no_redirect() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(BROWSER_UA)
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// 不跟随跳转的共享客户端
pub fn http_no_redirect() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(client_no_redirect)
}

/// 把网盘返回的错误码翻成人话。原始报文一律进 stderr.log，不要甩给用户。
pub fn friendly_err(drive: Drive, step: &str, raw: &str) -> String {
    crate::app_logger::log_command("netdisk", &format!("[{} {}] {}", drive.name(), step, raw));
    let low = raw.to_ascii_lowercase();
    let hint = if raw.contains("31001") || low.contains("login") || raw.contains("未登录") {
        "需要登录：请到「设置 → 网盘」粘贴你自己的登录 Cookie"
    } else if raw.contains("23018") {
        "夸克对未登录/游客有大小限制，登录后再试（设置 → 网盘）"
    } else if raw.contains("412") {
        "网盘要求登录后才能下载（HTTP 412）"
    } else if low.contains("timeout") || low.contains("timed out") {
        "连接网盘超时，稍后重试"
    } else if raw.contains("-9") && drive == Drive::Baidu {
        "提取码不对"
    } else if raw.contains("share") && raw.contains("cancel") {
        "分享已被取消或失效"
    } else {
        "网盘接口返回了错误"
    };
    format!("{}（{}）", hint, raw.chars().take(200).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_ids_roundtrip() {
        for d in [Drive::Quark, Drive::Baidu, Drive::Xunlei] {
            assert_eq!(Drive::from_id(d.id()), Some(d));
        }
        assert_eq!(Drive::from_id("nope"), None);
    }

    #[test]
    fn cookie_store_never_leaks_cookie_in_status() {
        // CookieStatus 里只有长度，没有内容 —— 防止明文 cookie 进界面/日志
        let s = CookieStatus {
            drive: "quark".into(),
            name: "夸克网盘".into(),
            has_cookie: true,
            has_refresh_token: false,
            cookie_len: 128,
            updated_at: 0,
        };
        let j = serde_json::to_string(&s).unwrap();
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        // ★ 断言要按**字段名**判，不能用字符串包含 —— `has_cookie` 里也含 "cookie"
        //   （第一版就是这么写错的，永远误报）
        assert!(v.get("cookie").is_none(), "状态里不能有 cookie 字段: {}", j);
        assert!(v.get("cookie_len").is_some());
        assert!(v.get("has_cookie").is_some());
    }
}
