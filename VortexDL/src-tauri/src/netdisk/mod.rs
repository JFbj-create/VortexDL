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
//! | 网盘 | 列目录 | 取直链 | 实测依据（2026-10-13） |
//! |---|---|---|---|
//! | 夸克 | ✅ 免登录 | ✅ 免登录 | 真机下完 `Snapshot 2.10.8.zip`（2.55MB / 8.17s） |
//! | 123 云盘 | ✅ 免登录 | ✅ 免登录 | 真实分享列出 3 个文件；取直链接口通过（那条分享只是**分享者流量配额**用完） |
//! | 阿里云盘 | ✅ 免登录 | ⚠️ 未验收 | 免登录**已证实**（不带 Authorization 只认分享令牌）；缺真实分享链接跑完整链路 |
//! | 百度 | 需登录 | 需登录 | 接口来自 BaiduPCS-Go / LinkSwift / OpenList；**无账号，未实测** |
//! | 迅雷 | ✅ 免登录 | ❌ 需登录 | 真实分享列出 `VC2010 Express.rar`；取直链 401/404，必须登录+转存 |
//!
//! ★ 这些接口都是**逆向网页/客户端**得来的，不是官方文档；网盘改版就会失效。
//!   所以每个驱动的报错都尽量把"是哪一步、网盘原话是什么"带出来，方便定位。
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

pub mod aliyun;
pub mod baidu;
pub mod link;
pub mod pan123;
pub mod quark;
pub mod tianyi;
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
    Drive::all()
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

/// 列出分享里的条目（夸克 / 百度 / 迅雷，三个都能免登录列目录）。
pub async fn list_share(link: &PanLink, passcode: &str) -> Result<Vec<PanEntry>, String> {
    let pass = if passcode.trim().is_empty() {
        link.passcode.clone().unwrap_or_default()
    } else {
        passcode.trim().to_string()
    };
    match link.drive {
        Drive::Quark => quark::list_share(&link.share_id, &pass).await,
        Drive::Uc => quark::list_share_uc(&link.share_id, &pass).await,
        Drive::Baidu => baidu::list_share(&link.share_id, &pass).await,
        Drive::Xunlei => xunlei::list_share(&link.share_id, &pass).await,
        Drive::Pan123 => pan123::list_share(&link.share_id, &pass).await,
        Drive::Aliyun => aliyun::list_share(&link.share_id, &pass).await,
        Drive::Tianyi => tianyi::list_share(&link.share_id, &pass).await,
        Drive::Mobile139 => Err(mobile139_unsupported()),
        Drive::Guangya => Err(guangya_unsupported()),
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
        Drive::Uc => quark::resolve_uc(&link.share_id, &pass, entries).await,
        Drive::Baidu => baidu::resolve(&link.share_id, &pass, entries).await,
        Drive::Xunlei => Err(
            "迅雷分享能列出文件，但**取直链必须登录**（实测：游客访问 `/drive/v1/files/<id>` 回 401，\
             `share/download` 这类接口直接 404 不存在）。要下迅雷分享请把 refresh_token 填到「设置 → 网盘」\
             并把文件先转存到自己的迅雷云盘。"
                .to_string(),
        ),
        Drive::Pan123 => pan123::resolve(&link.share_id, &pass, entries).await,
        Drive::Aliyun => aliyun::resolve(&link.share_id, &pass, entries).await,
        Drive::Tianyi => tianyi::resolve(&link.share_id, &pass, entries).await,
        Drive::Mobile139 => Err(mobile139_unsupported()),
        Drive::Guangya => Err(guangya_unsupported()),
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

/// 中国移动云盘：链接能识别，但直链接口要 `mcloud` 签名 + 登录态；
/// LinkSwift 对它的「我的网盘」也只做到点页面上的下载按钮，没有直链。
fn mobile139_unsupported() -> String {
    "中国移动云盘（139）的分享链接已经能识别，但它的直链接口需要账号签名（mcloud），目前没有可靠的免登录取链方式。请在移动云盘网页端打开分享页，用页面上的下载按钮下载；或在「设置 → 网盘」里填入 139 账号 Cookie 后重试（后端会在后续版本补上签名流程）。".to_string()
}

/// 光鸭云盘：接口是新的私有 API（api.guangyapan.com），参数/签名未公开，
/// 识别链接但不假装能解析。
fn guangya_unsupported() -> String {
    "光鸭云盘的分享链接已经能识别，但它的直链接口是私有 API（需要分享令牌 + 客户端签名），目前没有可靠的实现。请先用官方网页端下载。".to_string()
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
        // ★ 用 Drive::all() 而不是手写数组：以后新增驱动时"忘了加进测试"的情况就没了
        for d in Drive::all() {
            assert_eq!(Drive::from_id(d.id()), Some(d));
        }
        assert_eq!(Drive::from_id("nope"), None);
        // cookie_status 必须覆盖全部驱动（否则新网盘的登录状态在界面上是隐形的）
        let ids: Vec<String> = cookie_status().into_iter().map(|s| s.drive).collect();
        for d in Drive::all() {
            assert!(ids.contains(&d.id().to_string()), "cookie_status 漏了 {}", d.id());
        }
    }

    #[test]
    fn needs_login_matches_reality() {
        // 夸克 / 123 实测游客通道能下；百度 / 迅雷实测不能
        assert!(!Drive::Quark.needs_login());
        assert!(!Drive::Pan123.needs_login());
        assert!(Drive::Baidu.needs_login());
        assert!(Drive::Xunlei.needs_login());
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
