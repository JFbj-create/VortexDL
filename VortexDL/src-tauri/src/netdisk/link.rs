//! 网盘分享链接识别 —— **纯函数，不碰网络**，所以能直接写单测。
//!
//! 单独一个文件的原因：识别规则是最容易写错、又最容易被"改版"打坏的一层，
//! 必须能在没有账号、没有网络的情况下验证。网络部分在 quark.rs / baidu.rs / xunlei.rs。
use serde::{Deserialize, Serialize};

/// 支持的网盘
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Drive {
    Quark,
    Uc,
    Baidu,
    Xunlei,
    Pan123,
    Aliyun,
    Tianyi,
    Mobile139,
    Guangya,
}

impl Drive {
    pub fn id(&self) -> &'static str {
        match self {
            Drive::Quark => "quark",
            Drive::Uc => "uc",
            Drive::Baidu => "baidu",
            Drive::Xunlei => "xunlei",
            Drive::Pan123 => "pan123",
            Drive::Aliyun => "aliyun",
            Drive::Tianyi => "tianyi",
            Drive::Mobile139 => "mobile139",
            Drive::Guangya => "guangya",
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Drive::Quark => "夸克网盘",
            Drive::Uc => "UC 网盘",
            Drive::Baidu => "百度网盘",
            Drive::Xunlei => "迅雷云盘",
            Drive::Pan123 => "123 云盘",
            Drive::Aliyun => "阿里云盘",
            Drive::Tianyi => "天翼云盘",
            Drive::Mobile139 => "中国移动云盘",
            Drive::Guangya => "光鸭云盘",
        }
    }
    /// 这个网盘取直链**是否必须登录**。
    /// 前端据此提前提示"去设置里填 Cookie"，而不是让用户白等一个必然失败的解析。
    pub fn needs_login(&self) -> bool {
        match self {
            // 夸克 / 123 的游客通道实测能直接下，不需要账号
            Drive::Quark => false,
            Drive::Uc => false,
            Drive::Pan123 => false,
            Drive::Aliyun => false, // 分享令牌免登录，实测确认（见 aliyun.rs 的说明）
            Drive::Baidu => true,
            Drive::Xunlei => true,
            Drive::Tianyi => true,
            Drive::Mobile139 => true,
            Drive::Guangya => true,
        }
    }
    pub fn from_id(s: &str) -> Option<Drive> {
        match s.trim().to_ascii_lowercase().as_str() {
            "quark" => Some(Drive::Quark),
            "uc" | "ucpan" => Some(Drive::Uc),
            "baidu" | "bd" => Some(Drive::Baidu),
            "xunlei" | "xl" => Some(Drive::Xunlei),
            "pan123" | "123" | "123pan" => Some(Drive::Pan123),
            "aliyun" | "ali" | "alipan" | "aliyundrive" => Some(Drive::Aliyun),
            "tianyi" | "189" | "tcloud" => Some(Drive::Tianyi),
            "mobile139" | "139" | "caiyun" | "mcloud" => Some(Drive::Mobile139),
            "guangya" | "guangyapan" => Some(Drive::Guangya),
            _ => None,
        }
    }
    /// 全部驱动（顺序 = 界面里的展示顺序）
    pub fn all() -> [Drive; 9] {
        [
            Drive::Quark,
            Drive::Uc,
            Drive::Pan123,
            Drive::Aliyun,
            Drive::Baidu,
            Drive::Xunlei,
            Drive::Tianyi,
            Drive::Mobile139,
            Drive::Guangya,
        ]
    }
}

/// 识别出来的分享链接
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanLink {
    pub drive: Drive,
    /// 网盘自己的分享 id：夸克是 `pwd_id`，百度是 `surl`，迅雷是 share id
    pub share_id: String,
    /// 提取码（有就带上）
    pub passcode: Option<String>,
    /// 用户原始输入
    pub raw: String,
}

/// 从一段文本里抠提取码。用户经常整段粘贴，形如：
///   `https://pan.quark.cn/s/xxxx 提取码：abcd`
///   `链接: https://pan.baidu.com/s/1xxxx 密码: 1234`
///   `https://pan.baidu.com/s/1xxxx?pwd=1234`
fn pick_passcode(input: &str, query_pwd: Option<&str>) -> Option<String> {
    // 1) ?pwd=xxxx
    if let Some(p) = query_pwd {
        let p = p.trim();
        if !p.is_empty() {
            return Some(p.to_string());
        }
    }
    // 2) 片段 #abcd（百度旧式分享链接把提取码放在 # 后面）
    if let Some(i) = input.find('#') {
        let frag: String = input[i + 1..].chars().take(8).collect();
        let code: String = frag.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        if (3..=6).contains(&code.len()) {
            return Some(code);
        }
    }
    // 3) 中文标签后面的 4 位码
    for tag in ["提取码", "密码", "访问码", "pwd", "passcode", "code"] {
        if let Some(i) = input.find(tag) {
            let rest = &input[i + tag.len()..];
            let code: String = rest
                .trim_start_matches(|c: char| c == ':' || c == '：' || c == '=' || c == ' ' || c == '\t')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .take(8)
                .collect();
            if (3..=6).contains(&code.len()) {
                return Some(code);
            }
        }
    }
    None
}

/// 从 URL 里取 `pwd` / `password` 查询参数
fn query_pwd(u: &url::Url) -> Option<String> {
    for k in ["pwd", "password", "passcode"] {
        if let Some((_, v)) = u.query_pairs().find(|(kk, _)| kk == k) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// 主机名是否属于某个域名（允许带子域）。★ 必须按"点分段"比，
/// 不能用 `contains` —— 否则 `not123pan.com` 这种也会被认成 123 云盘。
fn host_is(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{}", domain))
}

/// 123 云盘有一堆域名，都是同一家（同一套接口）。
const PAN123_DOMAINS: [&str; 4] = ["123pan.com", "123865.com", "123684.com", "123912.com"];

/// 主入口：把用户粘贴的一段文本识别成分享链接。
///
/// ★ 只认 http(s) 开头的 URL；**不认识的链接返回 None**（上层据此提示"不支持"），
///   不要"猜一个网盘" —— 猜错会把用户带到完全不相干的接口上去。
pub fn recognize(input: &str) -> Option<PanLink> {
    let raw = input.trim().to_string();
    if raw.is_empty() {
        return None;
    }

    // 用户可能连着一堆文字一起粘贴，先抠出第一个 http(s) 链接
    let start = raw.find("http://").or_else(|| raw.find("https://"))?;
    let tail = &raw[start..];
    let end = tail
        .find(|c: char| c.is_whitespace() || c == '，' || c == '。' || c == '、')
        .unwrap_or(tail.len());
    let url_str = &tail[..end];

    let u = url::Url::parse(url_str).ok()?;
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    let path = u.path().to_string();
    let qpwd = query_pwd(&u);

    // ---------- 夸克 ----------
    // 分享页形如 pan.quark.cn/s/<id>、drive.quark.cn/s/<id>
    if host_is(&host, "quark.cn") {
        if let Some(id) = path.strip_prefix("/s/") {
            let id = id.trim_matches('/').to_string();
            if !id.is_empty() {
                return Some(PanLink {
                    drive: Drive::Quark,
                    share_id: id,
                    passcode: pick_passcode(&raw, qpwd.as_deref()),
                    raw,
                });
            }
        }
        return None;
    }

    // ---------- UC 网盘 ----------
    // 分享页形如 drive.uc.cn/s/<id>、fast.uc.cn/s/<id>、pan.uc.cn/s/<id>
    // ★ UC 和夸克是同一套云盘后端，但**接口域名和 pr 参数不同**，
    //   以前把 pan.uc.cn 当夸克打过去必然失败。
    if host_is(&host, "uc.cn") {
        if let Some(id) = path.strip_prefix("/s/") {
            let id = id.trim_matches('/').to_string();
            if !id.is_empty() {
                return Some(PanLink {
                    drive: Drive::Uc,
                    share_id: id,
                    passcode: pick_passcode(&raw, qpwd.as_deref()),
                    raw,
                });
            }
        }
        return None;
    }

    // ---------- 天翼云盘 ----------
    // cloud.189.cn/t/<code>、cloud.189.cn/web/share?code=<code>、
    // h5.cloud.189.cn/share.html#/t/<code>
    if host_is(&host, "cloud.189.cn") {
        let mut code: Option<String> = None;
        if let Some(id) = path.strip_prefix("/t/") {
            code = Some(id.trim_matches('/').to_string());
        } else if path.starts_with("/web/share") || path.starts_with("/share") {
            code = u
                .query_pairs()
                .find(|(k, _)| k == "code" || k == "shareCode")
                .map(|(_, v)| v.trim().to_string());
        }
        // 有些链接把 code 放在 #/t/<code> 片段里
        if code.as_deref().unwrap_or("").is_empty() {
            if let Some(frag) = u.fragment() {
                if let Some(rest) = frag.strip_prefix("/t/") {
                    code = Some(rest.trim_matches('/').to_string());
                }
            }
        }
        if let Some(c) = code {
            if !c.is_empty() {
                return Some(PanLink {
                    drive: Drive::Tianyi,
                    share_id: c,
                    passcode: None,
                    raw,
                });
            }
        }
        return None;
    }

    // ---------- 中国移动云盘 ----------
    // caiyun.139.com/m/i?<code>（code 直接就是整段 query）、
    // yun.139.com/shareweb/#/w/i/<id>
    if host_is(&host, "139.com") {
        let mut code: Option<String> = None;
        if path.starts_with("/m/i") {
            // 形如 /m/i?2jQXnjF...，query 里没有等号，整段就是分享码
            let q = u.query().unwrap_or("").trim();
            if !q.is_empty() && !q.contains('=') {
                code = Some(q.to_string());
            } else {
                code = u
                    .query_pairs()
                    .find(|(k, _)| k == "code" || k == "id")
                    .map(|(_, v)| v.trim().to_string());
            }
        } else if let Some(frag) = u.fragment() {
            if let Some(i) = frag.rfind("/i/") {
                code = Some(frag[i + 3..].trim_matches('/').to_string());
            }
        }
        if let Some(c) = code {
            if !c.is_empty() {
                return Some(PanLink {
                    drive: Drive::Mobile139,
                    share_id: c,
                    passcode: None,
                    raw,
                });
            }
        }
        return None;
    }

    // ---------- 光鸭云盘 ----------
    // www.guangyapan.com 的分享页（格式还在变，先按常见的 /s/<id> 与 ?code= 兜住）
    if host_is(&host, "guangyapan.com") {
        let mut code: Option<String> = None;
        if let Some(id) = path.strip_prefix("/s/") {
            code = Some(id.trim_matches('/').to_string());
        } else {
            code = u
                .query_pairs()
                .find(|(k, _)| k == "code" || k == "shareCode" || k == "id")
                .map(|(_, v)| v.trim().to_string());
        }
        if let Some(c) = code {
            if !c.is_empty() {
                return Some(PanLink {
                    drive: Drive::Guangya,
                    share_id: c,
                    passcode: None,
                    raw,
                });
            }
        }
        return None;
    }

    // ---------- 百度 ----------
    // /s/<surl>（surl 一般以 1 开头），以及旧式 /share/init?surl=<surl>
    if host_is(&host, "baidu.com") {
        let surl = if let Some(id) = path.strip_prefix("/s/") {
            Some(id.trim_matches('/').to_string())
        } else if path.starts_with("/share/init") {
            u.query_pairs()
                .find(|(k, _)| k == "surl")
                .map(|(_, v)| v.trim().to_string())
        } else {
            None
        };
        if let Some(s) = surl {
            if !s.is_empty() {
                return Some(PanLink {
                    drive: Drive::Baidu,
                    share_id: s,
                    passcode: pick_passcode(&raw, qpwd.as_deref()),
                    raw,
                });
            }
        }
        return None;
    }

    // ---------- 迅雷 ----------
    // pan.xunlei.com/s/<id>（迅雷分享链接带 ?pwd=）
    if host_is(&host, "xunlei.com") {
        let id = if let Some(id) = path.strip_prefix("/s/") {
            Some(id.trim_matches('/').to_string())
        } else if path.starts_with("/s") {
            // 有的链接是 /s?id=xxx
            u.query_pairs().find(|(k, _)| k == "id").map(|(_, v)| v.trim().to_string())
        } else {
            None
        };
        if let Some(id) = id {
            if !id.is_empty() {
                return Some(PanLink {
                    drive: Drive::Xunlei,
                    share_id: id,
                    passcode: pick_passcode(&raw, qpwd.as_deref()),
                    raw,
                });
            }
        }
        return None;
    }

    // ---------- 阿里云盘 ----------
    // 分享页形如 alipan.com/s/<shareId>、aliyundrive.com/s/<shareId>
    // （阿里 2023 年从 aliyundrive 改名 alipan，老域名还在跳转，两个都要认）
    if host_is(&host, "alipan.com") || host_is(&host, "aliyundrive.com") {
        if let Some(id) = path.strip_prefix("/s/") {
            let id = id.trim_matches('/').to_string();
            if !id.is_empty() {
                return Some(PanLink {
                    drive: Drive::Aliyun,
                    share_id: id,
                    passcode: pick_passcode(&raw, qpwd.as_deref()),
                    raw,
                });
            }
        }
        return None;
    }

    // ---------- 123 云盘 ----------
    // 分享页形如 123pan.com/s/<shareKey>，shareKey 形如 `AbCdEf-GhIj`（6位-4位），
    // 有的链接尾巴带 `.html`。域名有 4 个（见 PAN123_DOMAINS），都是同一套接口。
    if PAN123_DOMAINS.iter().any(|d| host_is(&host, d)) {
        if let Some(id) = path.strip_prefix("/s/") {
            let id = id.trim_matches('/');
            let id = id.strip_suffix(".html").unwrap_or(id);
            if !id.is_empty() {
                return Some(PanLink {
                    drive: Drive::Pan123,
                    share_id: id.to_string(),
                    passcode: pick_passcode(&raw, qpwd.as_deref()),
                    raw,
                });
            }
        }
        return None;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quark_basic() {
        let l = recognize("https://pan.quark.cn/s/abc123def").unwrap();
        assert_eq!(l.drive, Drive::Quark);
        assert_eq!(l.share_id, "abc123def");
        assert_eq!(l.passcode, None);
    }

    #[test]
    fn quark_with_passcode_in_text() {
        let l = recognize("夸克链接：https://pan.quark.cn/s/abc123def 提取码：7f2a 自取").unwrap();
        assert_eq!(l.drive, Drive::Quark);
        assert_eq!(l.share_id, "abc123def");
        assert_eq!(l.passcode.as_deref(), Some("7f2a"));
    }

    #[test]
    fn uc_host_is_uc_not_quark() {
        assert_eq!(recognize("https://pan.uc.cn/s/xyz").unwrap().drive, Drive::Uc);
        assert_eq!(recognize("https://drive.uc.cn/s/xyz").unwrap().drive, Drive::Uc);
        assert_eq!(recognize("https://fast.uc.cn/s/xyz").unwrap().drive, Drive::Uc);
    }

    #[test]
    fn tianyi_forms() {
        assert_eq!(recognize("https://cloud.189.cn/t/AbCdEf123").unwrap().drive, Drive::Tianyi);
        let l = recognize("https://cloud.189.cn/web/share?code=AbCdEf123").unwrap();
        assert_eq!(l.drive, Drive::Tianyi);
        assert_eq!(l.share_id, "AbCdEf123");
        assert_eq!(
            recognize("https://h5.cloud.189.cn/share.html#/t/AbCdEf123").unwrap().share_id,
            "AbCdEf123"
        );
    }

    #[test]
    fn mobile139_forms() {
        let l = recognize("https://caiyun.139.com/m/i?2jQXnjFabcd").unwrap();
        assert_eq!(l.drive, Drive::Mobile139);
        assert_eq!(l.share_id, "2jQXnjFabcd");
        assert_eq!(
            recognize("https://yun.139.com/shareweb/#/w/i/abc123").unwrap().share_id,
            "abc123"
        );
    }

    #[test]
    fn guangya_host() {
        assert_eq!(recognize("https://www.guangyapan.com/s/abc").unwrap().drive, Drive::Guangya);
    }

    #[test]
    fn baidu_basic() {
        let l = recognize("https://pan.baidu.com/s/1AbCdEf").unwrap();
        assert_eq!(l.drive, Drive::Baidu);
        assert_eq!(l.share_id, "1AbCdEf");
    }

    #[test]
    fn baidu_pwd_query() {
        let l = recognize("https://pan.baidu.com/s/1AbCdEf?pwd=1234").unwrap();
        assert_eq!(l.passcode.as_deref(), Some("1234"));
    }

    #[test]
    fn baidu_legacy_hash_passcode() {
        // 老式分享链接把提取码放在 # 后面
        let l = recognize("https://pan.baidu.com/s/1AbCdEf#1234").unwrap();
        assert_eq!(l.passcode.as_deref(), Some("1234"));
    }

    #[test]
    fn baidu_share_init() {
        let l = recognize("https://pan.baidu.com/share/init?surl=1AbCdEf&pwd=abcd").unwrap();
        assert_eq!(l.share_id, "1AbCdEf");
        assert_eq!(l.passcode.as_deref(), Some("abcd"));
    }

    #[test]
    fn xunlei_basic() {
        let l = recognize("https://pan.xunlei.com/s/VN_abc-def?pwd=8k2m").unwrap();
        assert_eq!(l.drive, Drive::Xunlei);
        assert_eq!(l.share_id, "VN_abc-def");
        assert_eq!(l.passcode.as_deref(), Some("8k2m"));
    }

    #[test]
    fn unknown_returns_none() {
        assert!(recognize("https://example.com/s/abc").is_none());
        assert!(recognize("https://pan.quark.cn/").is_none());
        assert!(recognize("").is_none());
        assert!(recognize("没有链接的一段话").is_none());
        // ★ 关键：别的站点 /s/ 路径不能被误判成网盘分享
        assert!(recognize("https://pan.qq.com/s/abc").is_none());
    }

    #[test]
    fn pan123_basic() {
        let l = recognize("https://www.123pan.com/s/pMuDVv-vMkJ3").unwrap();
        assert_eq!(l.drive, Drive::Pan123);
        assert_eq!(l.share_id, "pMuDVv-vMkJ3");
    }

    #[test]
    fn pan123_other_domains() {
        // 123 云盘有 4 个域名，都是同一家
        for h in ["123865.com", "123684.com", "123912.com", "www.123865.com"] {
            let l = recognize(&format!("https://{}/s/AbCdEf-GhIj", h)).unwrap();
            assert_eq!(l.drive, Drive::Pan123, "host {}", h);
            assert_eq!(l.share_id, "AbCdEf-GhIj");
        }
    }

    #[test]
    fn pan123_strips_html_suffix() {
        let l = recognize("https://www.123pan.com/s/pMuDVv-vMkJ3.html").unwrap();
        assert_eq!(l.share_id, "pMuDVv-vMkJ3");
    }

    #[test]
    fn pan123_with_passcode() {
        let l = recognize("123云盘：https://www.123pan.com/s/pMuDVv-vMkJ3 提取码：a1b2").unwrap();
        assert_eq!(l.passcode.as_deref(), Some("a1b2"));
    }

    #[test]
    fn pan123_lookalike_domain_rejected() {
        // ★ `not123pan.com` / `123pan.com.evil.com` 都不是 123 云盘
        assert!(recognize("https://not123pan.com/s/AbCdEf-GhIj").is_none());
        assert!(recognize("https://123pan.com.evil.com/s/AbCdEf-GhIj").is_none());
        assert!(recognize("https://www.123pan.com/").is_none());
    }

    #[test]
    fn aliyun_basic() {
        for h in ["www.alipan.com", "alipan.com", "www.aliyundrive.com"] {
            let l = recognize(&format!("https://{}/s/AbCdEfGhIjK", h)).unwrap();
            assert_eq!(l.drive, Drive::Aliyun, "host {}", h);
            assert_eq!(l.share_id, "AbCdEfGhIjK");
        }
    }

    #[test]
    fn aliyun_lookalike_rejected() {
        assert!(recognize("https://notalipan.com/s/AbCdEfGhIjK").is_none());
    }

    #[test]
    fn host_is_is_segment_wise() {
        assert!(host_is("pan.baidu.com", "baidu.com"));
        assert!(host_is("baidu.com", "baidu.com"));
        assert!(!host_is("notbaidu.com", "baidu.com"));
        assert!(!host_is("baidu.com.evil.com", "baidu.com"));
        // ★ 这条是修 `contains` 误判的核心：`my123pan.com` 不能匹配 `123pan.com`
        assert!(!host_is("my123pan.com", "123pan.com"));
    }

    #[test]
    fn picks_first_url_from_noisy_text() {
        let l = recognize("看这个 https://pan.baidu.com/s/1zzz 后面还有 https://pan.quark.cn/s/qqq").unwrap();
        assert_eq!(l.drive, Drive::Baidu);
        assert_eq!(l.share_id, "1zzz");
    }

    #[test]
    fn chinese_punctuation_ends_url() {
        let l = recognize("链接是 https://pan.quark.cn/s/abc，提取码 a1b2").unwrap();
        assert_eq!(l.share_id, "abc");
        assert_eq!(l.passcode.as_deref(), Some("a1b2"));
    }

    #[test]
    fn passcode_not_confused_with_url_alnum() {
        // 不能把 share_id 的一部分当成提取码
        let l = recognize("https://pan.quark.cn/s/abcdef12").unwrap();
        assert_eq!(l.passcode, None);
    }
}
