//! 百度网盘驱动（分享链接）。
//!
//! 接口形状来自 `qjfoidnh/BaiduPCS-Go`（Go）、`OpenListTeam/OpenList` 的
//! `drivers/baidu_netdisk`、`hmjz100/LinkSwift`（JS）三份实测代码的交叉对照。
//!
//! # 流程
//! ```text
//!   GET  /s/<surl>                    → 页面 HTML 里抠 bdstoken / shareid / uk / sekey
//!   POST /share/verify                → randsk（这就是 BDCLND cookie）
//!   GET  /share/list?shorturl=<surl>  → list[]（fs_id / server_filename / isdir / path）
//!   GET  /share/tplconfig             → sign, timestamp
//!   POST /api/sharedownload           → list[0].dlink
//!   GET  <dlink>  User-Agent: pan.baidu.com   → 302 到真实 CDN
//! ```
//!
//! ★★ **`User-Agent: pan.baidu.com` 是硬要求**：不带它，大于 50MB 的文件直接 403。
//!    LinkSwift 和 OpenList 都靠这一招，别"优化"掉。
//!
//! ⚠️ 这个驱动**没有条件做真机验证**（需要真实百度账号 + 有效分享链接）。
//!    纯逻辑部分（HTML 抠字段、参数拼装、响应解析）有单测；网络流程按上面三份
//!    实现照抄。第一次用如果报错，请把 stderr.log 里 `[百度网盘 …]` 那几行发出来。
use super::{client, client_no_redirect, friendly_err, PanEntry, PanResolved, Drive};

const PAN: &str = "https://pan.baidu.com";
/// BaiduPCS-Go 用的 netdisk UA（比浏览器 UA 更不容易被拦）
const NETDISK_UA: &str =
    "netdisk;P2SP;3.0.0.8;netdisk;11.12.3;ANG-AN00;android-android;10.0;JSbridge4.4.0;jointBridge;1.1.0;";
const APP_ID: &str = "250528";

fn req(method: reqwest::Method, url: &str, cookie: &str) -> reqwest::RequestBuilder {
    let mut r = super::http()
        .request(method, url)
        .header("User-Agent", NETDISK_UA)
        .header("Referer", "https://pan.baidu.com/")
        .header("Accept", "application/json, text/plain, */*");
    if !cookie.trim().is_empty() {
        r = r.header("Cookie", cookie.trim());
    }
    r
}

/// 从分享页 HTML 里抠出后面几步要用的字段。
///
/// 页面里是 `locals.mset({...})` / `yunData` 这种内联 JSON。**纯函数，可单测**。
pub fn parse_share_page(html: &str) -> SharePage {
    let mut sp = SharePage::default();
    for key in [
        "bdstoken", "shareid", "share_uk", "uk", "sekey", "surl", "shorturl", "app_id",
    ] {
        if let Some(v) = pick_json_string(html, key) {
            match key {
                "bdstoken" => sp.bdstoken = v,
                "shareid" => sp.shareid = v,
                "share_uk" => sp.share_uk = v,
                // `uk` 只在没有 share_uk 时兜底
                "uk" => { if sp.share_uk.is_empty() { sp.share_uk = v } }
                "sekey" => sp.sekey = v,
                "surl" => sp.surl = v,
                "shorturl" => sp.shorturl = v,
                "app_id" => sp.app_id = v,
                _ => {}
            }
        }
    }
    sp
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SharePage {
    pub bdstoken: String,
    pub shareid: String,
    pub share_uk: String,
    pub sekey: String,
    pub surl: String,
    pub shorturl: String,
    pub app_id: String,
}

/// 在 JSON 文本里找 `"key":"value"` 或 `"key": "value"`（也吃单引号）。
fn pick_json_string(s: &str, key: &str) -> Option<String> {
    // ★ 百度不同版本的分享页里 key 有时是双引号、有时是单引号（内联 JS 对象），
    //   两种都要认 —— 第一版只找双引号，单引号页面会一个字段都抠不出来。
    for quote in ['"', '\''] {
        if let Some(v) = pick_json_string_q(s, key, quote) {
            return Some(v);
        }
    }
    None
}

fn pick_json_string_q(s: &str, key: &str, q: char) -> Option<String> {
    let pat = format!("{}{}{}", q, key, q);
    let mut from = 0usize;
    while let Some(i) = s[from..].find(&pat) {
        let i = from + i + pat.len();
        let rest = s[i..].trim_start();
        let rest = rest.strip_prefix(':').map(|x| x.trim_start())?;
        let (quote, body) = if let Some(b) = rest.strip_prefix('"') {
            ('"', b)
        } else if let Some(b) = rest.strip_prefix('\'') {
            ('\'', b)
        } else {
            // 数字（shareid 有时是数字）
            let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if num.is_empty() {
                from = i;
                continue;
            }
            return Some(num);
        };
        if let Some(e) = body.find(quote) {
            let v = &body[..e];
            // 页面里别处也可能出现同名 key，取第一个非空的
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
        from = i;
    }
    None
}

/// 把 cookie 串解析成 map（判断有没有 BDUSS / BAIDUID 等）
pub fn parse_cookie(cookie: &str) -> std::collections::HashMap<String, String> {
    cookie
        .split(';')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

/// 标准 base64（只为 logid 用一次，不值得为它引一个 crate）
fn b64(input: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// logid = base64(BAIDUID)，百度下载接口要
fn logid_of(cookie: &str) -> String {
    let c = parse_cookie(cookie);
    let uid = c.get("BAIDUID").cloned().unwrap_or_default();
    b64(uid.as_bytes())
}

/// 列分享里的所有文件（目录递归，最多 4 层）
pub async fn list_share(surl: &str, passcode: &str) -> Result<Vec<PanEntry>, String> {
    let ck = super::get_cookie(Drive::Baidu);
    let mut cookie = ck.cookie;
    if parse_cookie(&cookie).get("BDUSS").is_none() {
        return Err("百度网盘需要登录：请到「设置 → 网盘」粘贴你的百度 Cookie（至少要有 BDUSS）".to_string());
    }

    // 1) 打开分享页拿 bdstoken / shareid / uk
    let page_url = format!("{}/s/{}", PAN, surl);
    let resp = req(reqwest::Method::GET, &page_url, &cookie)
        .send()
        .await
        .map_err(|e| friendly_err(Drive::Baidu, "打开分享页", &e.to_string()))?;
    let html = resp.text().await.unwrap_or_default();
    let sp = parse_share_page(&html);
    crate::app_logger::log_command(
        "netdisk",
        &format!(
            "[百度网盘] 分享页: bdstoken={} shareid={} uk={} sekey={}",
            !sp.bdstoken.is_empty(), sp.shareid, sp.share_uk, !sp.sekey.is_empty()
        ),
    );

    // 2) 有提取码就验一次，拿 randsk 写进 BDCLND
    if !passcode.trim().is_empty() {
        let randsk = verify_passcode(surl, passcode, &sp, &cookie).await?;
        if !randsk.is_empty() {
            cookie = format!("{}; BDCLND={}", cookie.trim_end_matches(';'), urlencode(&randsk));
        }
    }

    // 3) 列目录
    let mut files = Vec::new();
    let mut queue: Vec<(String, String, usize)> = vec![("/".to_string(), String::new(), 0)];
    while let Some((dir, prefix, depth)) = queue.pop() {
        if files.len() > 2000 {
            break;
        }
        let list = list_dir(surl, &dir, &sp, &cookie).await?;
        for it in list {
            let name = it.name.clone();
            let path = if prefix.is_empty() { name.clone() } else { format!("{}/{}", prefix, name) };
            if it.is_dir {
                if depth < 4 {
                    queue.push((it.path.clone(), path, depth + 1));
                }
            } else {
                files.push(PanEntry { path, ..it });
            }
        }
    }
    if files.is_empty() {
        return Err("这个分享里没有可下载的文件".to_string());
    }
    Ok(files)
}

async fn verify_passcode(
    surl: &str,
    passcode: &str,
    sp: &SharePage,
    cookie: &str,
) -> Result<String, String> {
    let t = crate::paths::now_ms();
    // ★ 两种入口都试：有 shareid/uk 用老式；否则用 surl 式
    let url = if !sp.shareid.is_empty() && !sp.share_uk.is_empty() {
        format!(
            "{}/share/verify?shareid={}&time={}&clienttype=1&uk={}",
            PAN, sp.shareid, t, sp.share_uk
        )
    } else {
        format!(
            "{}/share/verify?surl={}&t={}&channel=chunlei&web=1&app_id={}&clienttype=0",
            PAN, surl, t, APP_ID
        )
    };
    let body = format!("pwd={}&vcode=&vcode_str=&bdstoken={}", passcode, sp.bdstoken);
    let resp = req(reqwest::Method::POST, &url, cookie)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| friendly_err(Drive::Baidu, "share/verify", &e.to_string()))?;
    let text = resp.text().await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| friendly_err(Drive::Baidu, "share/verify", &text))?;
    let errno = v.get("errno").and_then(|x| x.as_i64()).unwrap_or(0);
    if errno != 0 {
        return Err(friendly_err(Drive::Baidu, "share/verify", &format!("errno={} {}", errno, text)));
    }
    Ok(v.get("randsk").and_then(|x| x.as_str()).unwrap_or("").to_string())
}

async fn list_dir(
    surl: &str,
    dir: &str,
    sp: &SharePage,
    cookie: &str,
) -> Result<Vec<PanEntry>, String> {
    let url = format!(
        "{}/share/list?bdstoken={}&root=1&web=5&app_id={}&shorturl={}&channel=chunlei\
         &page=1&num=1000&dir={}",
        PAN,
        sp.bdstoken,
        APP_ID,
        surl,
        urlencode(dir)
    );
    let resp = req(reqwest::Method::GET, &url, cookie)
        .send()
        .await
        .map_err(|e| friendly_err(Drive::Baidu, "share/list", &e.to_string()))?;
    let text = resp.text().await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| friendly_err(Drive::Baidu, "share/list", &text))?;
    let errno = v.get("errno").and_then(|x| x.as_i64()).unwrap_or(0);
    if errno != 0 {
        return Err(friendly_err(Drive::Baidu, "share/list", &format!("errno={} {}", errno, text)));
    }
    Ok(parse_list(&v))
}

/// 把 `/share/list` 的返回解析成 PanEntry（纯函数，可单测）
pub fn parse_list(v: &serde_json::Value) -> Vec<PanEntry> {
    v.get("list")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|it| {
                    let fs_id = it
                        .get("fs_id")
                        .map(|x| x.as_i64().map(|n| n.to_string()).unwrap_or_default())
                        .or_else(|| it.get("fs_id").and_then(|x| x.as_str()).map(|s| s.to_string()))
                        .unwrap_or_default();
                    if fs_id.is_empty() {
                        return None;
                    }
                    let name = it
                        .get("server_filename")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let path = it.get("path").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    Some(PanEntry {
                        fid: fs_id,
                        name,
                        is_dir: it.get("isdir").and_then(|x| x.as_i64()).unwrap_or(0) == 1,
                        size: it.get("size").and_then(|x| x.as_u64()).unwrap_or(0),
                        token: it
                            .get("sekey")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                        path,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 拿真实直链
pub async fn resolve(surl: &str, passcode: &str, entries: &[PanEntry]) -> Result<Vec<PanResolved>, String> {
    let ck = super::get_cookie(Drive::Baidu);
    let mut cookie = ck.cookie;
    if parse_cookie(&cookie).get("BDUSS").is_none() {
        return Err("百度网盘需要登录：请到「设置 → 网盘」粘贴你的百度 Cookie（至少要有 BDUSS）".to_string());
    }

    let page_url = format!("{}/s/{}", PAN, surl);
    let html = req(reqwest::Method::GET, &page_url, &cookie)
        .send()
        .await
        .map_err(|e| friendly_err(Drive::Baidu, "打开分享页", &e.to_string()))?
        .text()
        .await
        .unwrap_or_default();
    let sp = parse_share_page(&html);
    if !passcode.trim().is_empty() {
        let randsk = verify_passcode(surl, passcode, &sp, &cookie).await?;
        if !randsk.is_empty() {
            cookie = format!("{}; BDCLND={}", cookie.trim_end_matches(';'), urlencode(&randsk));
        }
    }

    // 1) 签名
    let logid = logid_of(&cookie);
    let sign_url = format!(
        "{}/share/tplconfig?fields=sign,timestamp&channel=chunlei&web=1&app_id={}\
         &clienttype=0&view_mode=1&surl={}&bdstoken={}&logid={}",
        PAN, APP_ID, surl, sp.bdstoken, logid
    );
    let sv: serde_json::Value = req(reqwest::Method::GET, &sign_url, &cookie)
        .send()
        .await
        .map_err(|e| friendly_err(Drive::Baidu, "share/tplconfig", &e.to_string()))?
        .json()
        .await
        .map_err(|e| friendly_err(Drive::Baidu, "share/tplconfig", &e.to_string()))?;
    let sign = sv.pointer("/data/sign").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let ts = sv.pointer("/data/timestamp").and_then(|x| x.as_i64()).unwrap_or(0);
    if sign.is_empty() {
        return Err(friendly_err(Drive::Baidu, "share/tplconfig", "没拿到 sign"));
    }

    // 2) 拿 dlink
    let fid_list: Vec<String> = entries.iter().map(|e| e.fid.clone()).collect();
    let dl_url = format!(
        "{}/api/sharedownload?channel=chunlei&clienttype=0&web=1&app_id={}\
         &sign={}&timestamp={}&bdstoken={}&logid={}",
        PAN, APP_ID, urlencode(&sign), ts, sp.bdstoken, logid
    );
    let mut form = vec![
        ("encrypt", "0".to_string()),
        ("product", "share".to_string()),
        ("uk", sp.share_uk.clone()),
        ("primaryid", sp.shareid.clone()),
        ("fid_list", serde_json::to_string(&fid_list).unwrap_or_default()),
    ];
    // 有些分享要 sekey
    if !sp.sekey.is_empty() {
        form.push(("extra", serde_json::json!({ "sekey": sp.sekey }).to_string()));
    }
    let body: String = form
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");

    let dv: serde_json::Value = req(reqwest::Method::POST, &dl_url, &cookie)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| friendly_err(Drive::Baidu, "api/sharedownload", &e.to_string()))?
        .json()
        .await
        .map_err(|e| friendly_err(Drive::Baidu, "api/sharedownload", &e.to_string()))?;

    let errno = dv.get("errno").and_then(|x| x.as_i64()).unwrap_or(0);
    if errno != 0 {
        return Err(friendly_err(
            Drive::Baidu,
            "api/sharedownload",
            &format!("errno={} {}", errno, dv),
        ));
    }

    let list = dv.get("list").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for (i, it) in list.iter().enumerate() {
        let dlink = it.get("dlink").and_then(|x| x.as_str()).unwrap_or("");
        if dlink.is_empty() {
            continue;
        }
        let e = entries.get(i);
        out.push(PanResolved {
            fid: e.map(|x| x.fid.clone()).unwrap_or_default(),
            name: e.map(|x| x.name.clone()).unwrap_or_else(|| format!("baidu_{}.bin", i)),
            size: e.map(|x| x.size).unwrap_or(0),
            url: dlink.to_string(),
            headers: vec![
                // ★★ 这两行是百度下载的硬要求，去掉就 403
                ("User-Agent".to_string(), "pan.baidu.com".to_string()),
                ("Referer".to_string(), "https://pan.baidu.com/".to_string()),
                ("Cookie".to_string(), cookie.clone()),
            ],
        });
    }
    if out.is_empty() {
        return Err("百度没有返回可用的下载地址".to_string());
    }
    Ok(out)
}

/// 把 dlink 换回真实 CDN 地址（dlink 本身是个 302）
pub async fn follow_dlink(dlink: &str, cookie: &str) -> Result<String, String> {
    let resp = super::http_no_redirect()
        .get(dlink)
        .header("User-Agent", "pan.baidu.com")
        .header("Cookie", cookie)
        .send()
        .await
        .map_err(|e| friendly_err(Drive::Baidu, "跟随 dlink", &e.to_string()))?;
    if let Some(loc) = resp.headers().get("location").and_then(|v| v.to_str().ok()) {
        return Ok(loc.to_string());
    }
    Ok(dlink.to_string())
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_page_picks_fields() {
        let html = r#"<script>locals.mset({"bdstoken":"abc123","shareid":987654,"share_uk":"1122","sekey":"sk=="});</script>"#;
        let sp = parse_share_page(html);
        assert_eq!(sp.bdstoken, "abc123");
        assert_eq!(sp.shareid, "987654");   // 数字也能抠出来
        assert_eq!(sp.share_uk, "1122");
        assert_eq!(sp.sekey, "sk==");
    }

    #[test]
    fn parse_page_uk_fallback() {
        // 只有 uk 没有 share_uk 时，uk 兜底
        let html = r#"{"uk":"42"}"#;
        assert_eq!(parse_share_page(html).share_uk, "42");
    }

    #[test]
    fn parse_page_share_uk_wins_over_uk() {
        let html = r#"{"share_uk":"7","uk":"9"}"#;
        assert_eq!(parse_share_page(html).share_uk, "7");
    }

    #[test]
    fn parse_page_handles_single_quotes_and_spaces() {
        let html = r#"<script>var x = { 'bdstoken' : 'zzz' };</script>"#;
        assert_eq!(parse_share_page(html).bdstoken, "zzz");
    }

    #[test]
    fn parse_page_single_quoted_keys() {
        // 内联 JS 对象里 key 是单引号
        let html = "<script>var o = {'bdstoken':'aaa','shareid':'5'};</script>";
        let sp = parse_share_page(html);
        assert_eq!(sp.bdstoken, "aaa");
        assert_eq!(sp.shareid, "5");
    }

    #[test]
    fn parse_page_missing_is_empty() {
        assert_eq!(parse_share_page("<html></html>"), SharePage::default());
    }

    #[test]
    fn cookie_parsing() {
        let c = parse_cookie("BDUSS=abc; BAIDUID=xyz; STOKEN=q");
        assert_eq!(c.get("BDUSS").unwrap(), "abc");
        assert_eq!(c.get("BAIDUID").unwrap(), "xyz");
        assert!(c.get("nope").is_none());
    }

    #[test]
    fn b64_matches_known_values() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"abc"), "YWJj");
        assert_eq!(b64(b"ab"), "YWI=");
        assert_eq!(b64(b"a"), "YQ==");
        assert_eq!(b64(b"hello world"), "aGVsbG8gd29ybGQ=");
        // 0xFB 0xEF 0xBE 这种高位字节（BAIDUID 里常见十六进制串）不能错
        // 0xFB 0xEF 0xBE -> 111110 111110 111110 111110 -> 62,62,62,62 -> "++++"
        // （第一版我手算成 "+--+"，是**测试写错了**，编码器本身没问题）
        assert_eq!(b64(&[0xfb, 0xef, 0xbe]), "++++");
    }

    #[test]
    fn logid_is_base64_of_baiduid() {
        let l = logid_of("BDUSS=a; BAIDUID=abc");
        assert_eq!(l, "YWJj"); // base64("abc")
    }

    #[test]
    fn list_parsing_dir_and_file() {
        let v = serde_json::json!({"errno":0,"list":[
            {"fs_id":111,"server_filename":"文件夹","isdir":1,"path":"/文件夹","size":0},
            {"fs_id":222,"server_filename":"a.mp4","isdir":0,"path":"/文件夹/a.mp4","size":99,"sekey":"k"}
        ]});
        let l = parse_list(&v);
        assert_eq!(l.len(), 2);
        assert!(l[0].is_dir);
        assert_eq!(l[0].fid, "111");
        assert!(!l[1].is_dir);
        assert_eq!(l[1].size, 99);
        assert_eq!(l[1].token, "k");
    }

    #[test]
    fn list_parsing_skips_entries_without_fs_id() {
        let v = serde_json::json!({"list":[{"server_filename":"x"}]});
        assert!(parse_list(&v).is_empty());
    }
}
