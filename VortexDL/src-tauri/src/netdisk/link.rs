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
    Baidu,
    Xunlei,
}

impl Drive {
    pub fn id(&self) -> &'static str {
        match self {
            Drive::Quark => "quark",
            Drive::Baidu => "baidu",
            Drive::Xunlei => "xunlei",
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Drive::Quark => "夸克网盘",
            Drive::Baidu => "百度网盘",
            Drive::Xunlei => "迅雷云盘",
        }
    }
    pub fn from_id(s: &str) -> Option<Drive> {
        match s.trim().to_ascii_lowercase().as_str() {
            "quark" | "uc" => Some(Drive::Quark),
            "baidu" | "bd" => Some(Drive::Baidu),
            "xunlei" | "xl" => Some(Drive::Xunlei),
            _ => None,
        }
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

    // ---------- 夸克 / UC ----------
    // 分享页形如 pan.quark.cn/s/<id>、drive.quark.cn/s/<id>、pan.uc.cn/s/<id>
    if host.contains("quark.cn") || host.contains("uc.cn") {
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

    // ---------- 百度 ----------
    // /s/<surl>（surl 一般以 1 开头），以及旧式 /share/init?surl=<surl>
    if host.contains("baidu.com") {
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
    if host.contains("xunlei.com") {
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
    fn quark_uc_host() {
        assert_eq!(recognize("https://pan.uc.cn/s/xyz").unwrap().drive, Drive::Quark);
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
