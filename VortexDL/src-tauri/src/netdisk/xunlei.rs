//! 迅雷云盘驱动。
//!
//! # ⚠️ 能力边界（如实说明，别指望它做分享解析）
//!
//! 迅雷的**分享链接解析**目前**全网没有公开可用的实现**：
//!   · `OpenListTeam/OpenList` 的 `drivers/thunder` 只有"自己网盘"；
//!   · `hmjz100/LinkSwift` 的迅雷部分遇到分享页**直接抛错**（只实现了 home 页）；
//!   · `cnk3x/xunlei` 是把迅雷官方二进制封成容器，不是 API 客户端。
//!
//! 所以这里只做**自己网盘**（列表 + 取直链），这两步在 OpenList 里是实测可用的。
//! 分享链接会由 `netdisk::list_share` 明确拒绝，不会假装成功。
//!
//! # 认证
//! 迅雷是 OAuth2，不是 Cookie：
//! ```text
//!   POST https://xluser-ssl.xunlei.com/v1/auth/token
//!        {grant_type:"refresh_token", refresh_token, client_id, client_secret}
//!     -> access_token（有效期短，每次用前刷新）
//!   POST https://xluser-ssl.xunlei.com/v1/shield/captcha/init
//!        -> captcha_token（每个 drive 请求都要带 X-Captcha-Token）
//!   GET  https://api-pan.xunlei.com/drive/v1/files?parent_id=...
//!   GET  https://api-pan.xunlei.com/drive/v1/files/<id>   -> web_content_link
//! ```
//! 用户在设置里只需要贴 **refresh_token**（从浏览器登录后的请求里拿），
//! device_id 由我们自己生成并持久化（换掉就要重新过验证码）。
use super::{client, friendly_err, PanEntry, PanResolved, Drive};

const API: &str = "https://api-pan.xunlei.com/drive/v1";
const XLUSER: &str = "https://xluser-ssl.xunlei.com/v1";
/// OpenList 里的默认客户端凭据（公开常量，不是用户隐私）
const CLIENT_ID: &str = "Xp6vsxz_7IYVw2BB";
const CLIENT_SECRET: &str = "Xp6vsy4tN9toTVdMSpomVdXpRmES";
const CLIENT_VERSION: &str = "8.31.0.9726";
const PACKAGE_NAME: &str = "com.xunlei.downloadprovider";
/// 下载直链必须用这个 UA（OpenList 的 DownloadUserAgent）
const DL_UA: &str = "Dalvik/2.1.0 (Linux; U; Android 12; SM-G9810 Build/SP1A.210812.016)";

/// captcha 签名：md5(client_id + client_version + package_name + device_id + timestamp)
/// ★ 纯函数，可单测。
pub fn captcha_sign(client_id: &str, client_version: &str, package_name: &str, device_id: &str, ts: &str) -> String {
    let raw = format!("{}{}{}{}{}", client_id, client_version, package_name, device_id, ts);
    md5_hex(raw.as_bytes())
}

/// 自带 md5（RFC 1321）—— 只为这一个签名用，不值得引 crate。
pub fn md5_hex(input: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64)
        .map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32)
        .collect();
    let mut msg = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    let (mut a0, mut b0, mut c0, mut d0) = (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    for chunk in msg.chunks(64) {
        let m: Vec<u32> = (0..16)
            .map(|i| u32::from_le_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]]))
            .collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let tmp = d;
            d = c;
            c = b;
            let x = a
                .wrapping_add(f)
                .wrapping_add(k[i])
                .wrapping_add(m[g]);
            b = b.wrapping_add(x.rotate_left(S[i]));
            a = tmp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = String::new();
    for v in [a0, b0, c0, d0] {
        out.push_str(&format!("{:02x}{:02x}{:02x}{:02x}", v & 0xff, (v >> 8) & 0xff, (v >> 16) & 0xff, (v >> 24) & 0xff));
    }
    out
}

struct Auth {
    access_token: String,
    token_type: String,
    captcha_token: String,
}

/// 刷新 access_token（refresh_token 存不住就报错让人话提示）
async fn auth() -> Result<Auth, String> {
    let ck = super::get_cookie(Drive::Xunlei);
    if ck.refresh_token.trim().is_empty() {
        return Err("迅雷需要 refresh_token：请到「设置 → 网盘」粘贴（从浏览器登录迅雷后的请求里取）".to_string());
    }
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": ck.refresh_token,
        "client_id": CLIENT_ID,
        "client_secret": CLIENT_SECRET,
    });
    let v: serde_json::Value = super::http()
        .post(format!("{}/auth/token", XLUSER))
        .json(&body)
        .send()
        .await
        .map_err(|e| friendly_err(Drive::Xunlei, "auth/token", &e.to_string()))?
        .json()
        .await
        .map_err(|e| friendly_err(Drive::Xunlei, "auth/token", &e.to_string()))?;
    let access = v.get("access_token").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if access.is_empty() {
        return Err(friendly_err(Drive::Xunlei, "auth/token", &v.to_string()));
    }
    let tt = v.get("token_type").and_then(|x| x.as_str()).unwrap_or("Bearer").to_string();
    // 刷新成功后把新的 refresh_token 存回去（迅雷会轮换）
    if let Some(rt) = v.get("refresh_token").and_then(|x| x.as_str()) {
        let _ = super::set_cookie(Drive::Xunlei, "", rt);
    }

    let device_id = if ck.device_id.trim().is_empty() {
        format!("{:032x}", crate::paths::now_ms() as u128 ^ 0x9e3779b97f4a7c15u128)
    } else {
        ck.device_id.clone()
    };
    let ts = (crate::paths::now_ms() / 1000).to_string();
    let sign = captcha_sign(CLIENT_ID, CLIENT_VERSION, PACKAGE_NAME, &device_id, &ts);
    let cap_body = serde_json::json!({
        "client_id": CLIENT_ID,
        "action": "GET:/drive/v1/files",
        "device_id": device_id,
        "meta": {
            "captcha_sign": sign,
            "client_version": CLIENT_VERSION,
            "package_name": PACKAGE_NAME,
            "user_id": "0",
            "timestamp": ts,
        }
    });
    let cv: serde_json::Value = super::http()
        .post(format!("{}/shield/captcha/init", XLUSER))
        .json(&cap_body)
        .send()
        .await
        .map_err(|e| friendly_err(Drive::Xunlei, "captcha/init", &e.to_string()))?
        .json()
        .await
        .map_err(|e| friendly_err(Drive::Xunlei, "captcha/init", &e.to_string()))?;
    let captcha = cv
        .get("captcha_token")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();

    Ok(Auth { access_token: access, token_type: tt, captcha_token: captcha })
}

fn drive_req(url: &str, a: &Auth) -> reqwest::RequestBuilder {
    let mut r = super::http()
        .get(url)
        .header("Authorization", format!("{} {}", a.token_type, a.access_token))
        .header("User-Agent", DL_UA);
    if !a.captcha_token.is_empty() {
        r = r.header("X-Captcha-Token", &a.captcha_token);
    }
    r
}

/// 列自己网盘里的文件
pub async fn list_mine(parent_id: &str) -> Result<Vec<PanEntry>, String> {
    let a = auth().await?;
    let pid = if parent_id.trim().is_empty() { "" } else { parent_id };
    let url = format!(
        "{}/files?space=&__type=drive&refresh=true&__sync=true&parent_id={}&page_token=\
         &with_audit=true&limit=100&filters={}",
        API,
        pid,
        urlencode(r#"{"phase":{"eq":"PHASE_TYPE_COMPLETE"},"trashed":{"eq":false}}"#)
    );
    let v: serde_json::Value = drive_req(&url, &a)
        .send()
        .await
        .map_err(|e| friendly_err(Drive::Xunlei, "files", &e.to_string()))?
        .json()
        .await
        .map_err(|e| friendly_err(Drive::Xunlei, "files", &e.to_string()))?;
    if let Some(err) = v.get("error").and_then(|x| x.as_str()) {
        return Err(friendly_err(Drive::Xunlei, "files", err));
    }
    Ok(parse_files(&v))
}

/// 解析 `/files` 的返回（纯函数，可单测）
pub fn parse_files(v: &serde_json::Value) -> Vec<PanEntry> {
    v.get("files")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    let id = f.get("id").and_then(|x| x.as_str())?.to_string();
                    let kind = f.get("kind").and_then(|x| x.as_str()).unwrap_or("");
                    Some(PanEntry {
                        fid: id,
                        name: f.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        is_dir: kind.eq_ignore_ascii_case("drive#folder"),
                        size: f.get("size").and_then(|x| x.as_str()).and_then(|s| s.parse().ok())
                            .or_else(|| f.get("size").and_then(|x| x.as_u64()))
                            .unwrap_or(0),
                        token: String::new(),
                        path: String::new(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 取直链
pub async fn resolve_mine(entries: &[PanEntry]) -> Result<Vec<PanResolved>, String> {
    let a = auth().await?;
    let mut out = Vec::new();
    for e in entries {
        let url = format!("{}/files/{}?space=", API, e.fid);
        let v: serde_json::Value = drive_req(&url, &a)
            .send()
            .await
            .map_err(|err| friendly_err(Drive::Xunlei, "files/<id>", &err.to_string()))?
            .json()
            .await
            .map_err(|err| friendly_err(Drive::Xunlei, "files/<id>", &err.to_string()))?;
        // web_content_link 是给网页用的直链；medias[].link.url 是播放用的
        let link = v
            .get("web_content_link")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                v.get("medias")
                    .and_then(|x| x.as_array())
                    .and_then(|m| m.first())
                    .and_then(|m| m.pointer("/link/url"))
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string())
            });
        if let Some(u) = link {
            out.push(PanResolved {
                fid: e.fid.clone(),
                name: e.name.clone(),
                size: e.size,
                url: u,
                headers: vec![
                    ("User-Agent".to_string(), DL_UA.to_string()),
                    (
                        "Authorization".to_string(),
                        format!("{} {}", a.token_type, a.access_token),
                    ),
                ],
            });
        }
    }
    if out.is_empty() {
        return Err("迅雷没有返回可用的下载地址".to_string());
    }
    Ok(out)
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_rfc1321_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"a"), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(md5_hex(b"message digest"), "f96b697d7cb7938d525a2f31aaf161d0");
        assert_eq!(
            md5_hex(b"abcdefghijklmnopqrstuvwxyz"),
            "c3fcd3d76192e4007dfb496cca67e13b"
        );
    }

    #[test]
    fn captcha_sign_is_deterministic() {
        let a = captcha_sign("cid", "1.0", "pkg", "dev", "1700000000");
        let b = captcha_sign("cid", "1.0", "pkg", "dev", "1700000000");
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
        // 时间戳不同 -> 签名必须不同（否则服务端会判重放）
        assert_ne!(a, captcha_sign("cid", "1.0", "pkg", "dev", "1700000001"));
    }

    #[test]
    fn parse_files_reads_folder_and_file() {
        let v = serde_json::json!({"files":[
            {"id":"d1","name":"电影","kind":"drive#folder"},
            {"id":"f1","name":"a.mp4","kind":"drive#file","size":"1024"}
        ]});
        let f = parse_files(&v);
        assert_eq!(f.len(), 2);
        assert!(f[0].is_dir);
        assert!(!f[1].is_dir);
        assert_eq!(f[1].size, 1024);   // size 是字符串，要能转
    }

    #[test]
    fn parse_files_skips_without_id() {
        let v = serde_json::json!({"files":[{"name":"x"}]});
        assert!(parse_files(&v).is_empty());
    }
}
