//! 天翼云盘（189）分享解析。
//!
//! 用的是天翼云盘**网页端自己的开放接口**（`cloud.189.cn/api/open/...`），
//! 不是逆向私有协议：
//!
//! | 步骤 | 接口 |
//! |---|---|
//! | 分享信息 | `GET /api/open/share/getShareInfoByCodeV2.action?shareCode=` |
//! | 列目录   | `GET /api/open/share/listShareDirV2.action?shareId=&fileId=&shareCode=` |
//! | 取直链   | `GET /api/open/file/getFileDownloadUrl.action?fileId=` |
//!
//! ★ 关键点（实测）：
//!   · 这些 `.action` 接口默认吐 **XML**；必须带 `Accept: application/json;charset=UTF-8`
//!     才会返回 JSON（只写 `application/json` 仍然吐 XML）。
//!   · 取直链**必须登录**：游客只会拿到 `ShareInfoNotFound` / 空 downloadUrl。
//!     登录态就是用户自己的 `cloud.189.cn` Cookie（设置 → 网盘里填）。
//!   · 列目录前先访问一次分享页，把服务端下发的会话 Cookie 带上，成功率更高。
//!
//! ⚠️ 本驱动按公开接口实现，但**没有真实分享链接/账号做端到端验收**（见 2026-10-18 说明）。
//!   错误信息会把天翼返回的原始 code/message 带出来，方便定位。

use super::link::Drive;
use super::{friendly_err, get_cookie, PanEntry, PanResolved};
use serde_json::Value;

const API: &str = "https://cloud.189.cn/api/open";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

fn req(url: &str, cookie: &str) -> reqwest::RequestBuilder {
    let mut r = super::http()
        .get(url)
        .header("User-Agent", UA)
        .header("Accept", "application/json;charset=UTF-8")
        .header("Referer", "https://cloud.189.cn/");
    if !cookie.trim().is_empty() {
        r = r.header("Cookie", cookie.trim());
    }
    r
}

/// 访问分享页收会话 Cookie（游客也有一份，带上更容易通过风控）
async fn guest_cookie(code: &str) -> String {
    let url = format!("https://cloud.189.cn/t/{}", code);
    let resp = match super::http()
        .get(&url)
        .header("User-Agent", UA)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            crate::app_logger::log_command(
                "netdisk",
                &format!("[天翼] 取游客 cookie 失败: {}", e),
            );
            return String::new();
        }
    };
    let mut parts: Vec<String> = Vec::new();
    for v in resp.headers().get_all(reqwest::header::SET_COOKIE).iter() {
        if let Ok(sv) = v.to_str() {
            let first = sv.split(';').next().unwrap_or("").trim();
            if !first.is_empty() {
                parts.push(first.to_string());
            }
        }
    }
    parts.join("; ")
}

fn merge_cookie(user: &str, guest: &str) -> String {
    match (user.trim().is_empty(), guest.trim().is_empty()) {
        (true, true) => String::new(),
        (false, true) => user.trim().to_string(),
        (true, false) => guest.to_string(),
        (false, false) => format!("{}; {}", user.trim_end_matches(';'), guest),
    }
}

async fn get_json(url: &str, cookie: &str, step: &str) -> Result<Value, String> {
    let resp = req(url, cookie)
        .send()
        .await
        .map_err(|e| friendly_err(Drive::Tianyi, step, &e.to_string()))?;
    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text).map_err(|_| {
        // 少数情况下仍会吐 XML：把 <code>/<message> 抠出来给人看
        let code = tag_of(&text, "code").unwrap_or_default();
        let msg = tag_of(&text, "message").unwrap_or_default();
        friendly_err(Drive::Tianyi, step, &format!("{} {}", code, msg).trim().to_string())
    })?;
    let rc = v
        .get("res_code")
        .map(|x| x.as_str().map(|t| t.to_string()).unwrap_or_else(|| x.to_string()))
        .unwrap_or_else(|| "0".into());
    let ok = v.get("res_code").map(|x| x.as_i64() == Some(0) || x.as_str() == Some("0")).unwrap_or(true);
    if !ok {
        let msg = v
            .get("res_message")
            .and_then(|x| x.as_str())
            .unwrap_or("天翼接口返回错误");
        return Err(friendly_err(Drive::Tianyi, step, &format!("{} {}", rc, msg)));
    }
    Ok(v)
}

fn tag_of(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let i = xml.find(&open)? + open.len();
    let j = xml[i..].find(&close)? + i;
    Some(xml[i..j].trim().to_string())
}

fn s(v: &Value, keys: &[&str]) -> String {
    for k in keys {
        if let Some(x) = v.get(*k) {
            if let Some(t) = x.as_str() {
                if !t.trim().is_empty() {
                    return t.trim().to_string();
                }
            }
            if let Some(n) = x.as_i64() {
                return n.to_string();
            }
        }
    }
    String::new()
}

fn u64_of(v: &Value, keys: &[&str]) -> u64 {
    for k in keys {
        if let Some(x) = v.get(*k) {
            if let Some(n) = x.as_u64() {
                return n;
            }
            if let Some(t) = x.as_str() {
                if let Ok(n) = t.parse::<u64>() {
                    return n;
                }
            }
        }
    }
    0
}

fn bool_of(v: &Value, keys: &[&str]) -> bool {
    for k in keys {
        if let Some(x) = v.get(*k) {
            if let Some(b) = x.as_bool() {
                return b;
            }
            if let Some(n) = x.as_i64() {
                return n != 0;
            }
            if let Some(t) = x.as_str() {
                return t == "true" || t == "1";
            }
        }
    }
    false
}

struct ShareInfo {
    share_id: String,
    file_id: String,
    is_dir: bool,
}

async fn share_info(code: &str, access: &str, cookie: &str) -> Result<ShareInfo, String> {
    let mut url = format!(
        "{}/share/getShareInfoByCodeV2.action?shareCode={}",
        API,
        urlenc(code)
    );
    if !access.trim().is_empty() {
        url.push_str(&format!("&accessCode={}", urlenc(access)));
    }
    let v = get_json(&url, cookie, "getShareInfoByCodeV2").await?;
    let share_id = s(&v, &["shareId", "shareID", "share_id"]);
    let file_id = s(&v, &["fileId", "fileID", "file_id", "id"]);
    let mode = v.get("shareMode").and_then(|x| x.as_i64()).unwrap_or(1);
    if share_id.is_empty() {
        return Err(friendly_err(
            Drive::Tianyi,
            "getShareInfoByCodeV2",
            "返回里没有 shareId（分享可能已失效）",
        ));
    }
    Ok(ShareInfo {
        share_id,
        file_id,
        // shareMode: 1=文件 2=目录（天翼网页端约定）
        is_dir: mode != 1,
    })
}

async fn list_dir(
    code: &str,
    access: &str,
    share_id: &str,
    file_id: &str,
    cookie: &str,
) -> Result<Vec<PanEntry>, String> {
    let mut out = Vec::new();
    let mut page = 1;
    loop {
        let mut url = format!(
            "{}/share/listShareDirV2.action?shareId={}&fileId={}&isHand=true&pageNum={}&pageSize=100&orderBy=filename&desc=false",
            API,
            urlenc(share_id),
            urlenc(file_id),
            page
        );
        if !access.trim().is_empty() {
            url.push_str(&format!("&accessCode={}", urlenc(access)));
        }
        url.push_str(&format!("&shareCode={}", urlenc(code)));
        let v = get_json(&url, cookie, "listShareDirV2").await?;
        let ao = v
            .get("fileListAO")
            .cloned()
            .unwrap_or(Value::Null);
        let files = ao
            .get("fileList")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        let folders = ao
            .get("folderList")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        let before = out.len();
        for f in &files {
            out.push(entry(f, false));
        }
        for f in &folders {
            out.push(entry(f, true));
        }
        let count = ao.get("count").and_then(|x| x.as_u64()).unwrap_or(out.len() as u64);
        if out.len() as u64 >= count || out.len() == before || (files.len() + folders.len()) < 100 {
            break;
        }
        page += 1;
        if page > 50 {
            break;
        }
    }
    Ok(out)
}

fn entry(v: &Value, is_dir: bool) -> PanEntry {
    let name = s(v, &["name", "fileName", "file_name"]);
    PanEntry {
        fid: s(v, &["id", "fileId", "file_id"]),
        path: name.clone(),
        name,
        is_dir,
        size: u64_of(v, &["size", "fileSize", "file_size"]),
        token: String::new(),
    }
}

/// 列分享里的全部文件（目录递归，最多 4 层 / 2000 条）
pub async fn list_share(code: &str, passcode: &str) -> Result<Vec<PanEntry>, String> {
    let ck = get_cookie(Drive::Tianyi);
    let guest = guest_cookie(code).await;
    let cookie = merge_cookie(&ck.cookie, &guest);
    let info = share_info(code, passcode, &cookie).await?;
    if !info.is_dir {
        return Ok(vec![PanEntry {
            fid: if info.file_id.is_empty() { info.share_id.clone() } else { info.file_id.clone() },
            name: "分享文件".into(),
            path: "分享文件".into(),
            is_dir: false,
            size: 0,
            token: String::new(),
        }]);
    }
    let top = list_dir(code, passcode, &info.share_id, &info.file_id, &cookie).await?;
    let mut files = Vec::new();
    let mut stack: Vec<(PanEntry, usize)> = top.into_iter().map(|e| (e, 0usize)).collect();
    while let Some((e, depth)) = stack.pop() {
        if files.len() > 2000 {
            break;
        }
        if e.is_dir && depth < 4 {
            match list_dir(code, passcode, &info.share_id, &e.fid, &cookie).await {
                Ok(kids) => {
                    for k in kids {
                        let mut k2 = k;
                        k2.path = format!("{}/{}", e.path, k2.name);
                        stack.push((k2, depth + 1));
                    }
                }
                Err(err) => crate::app_logger::log_command(
                    "netdisk",
                    &format!("[天翼] 展开目录 {} 失败: {}", e.path, err),
                ),
            }
        } else if !e.is_dir {
            files.push(e);
        }
    }
    if files.is_empty() {
        return Err("这个分享里没有可下载的文件（可能只有空目录）".to_string());
    }
    Ok(files)
}

/// 取直链。天翼**必须登录**才有 downloadUrl。
pub async fn resolve(
    _code: &str,
    _passcode: &str,
    entries: &[PanEntry],
) -> Result<Vec<PanResolved>, String> {
    let ck = get_cookie(Drive::Tianyi);
    if ck.cookie.trim().is_empty() {
        return Err("天翼云盘取直链必须登录：请到「设置 → 网盘」粘贴你在 cloud.189.cn 的登录 Cookie".into());
    }
    let cookie = ck.cookie;
    let mut out = Vec::new();
    for e in entries {
        let url = format!(
            "{}/file/getFileDownloadUrl.action?fileId={}",
            API,
            urlenc(&e.fid)
        );
        let v = get_json(&url, &cookie, "getFileDownloadUrl").await?;
        let mut dl = s(&v, &["downloadUrl", "download_url", "url"]);
        if dl.is_empty() {
            // 有的版本把结果放在 fileDownloadUrl 节点里
            dl = s(&v, &["fileDownloadUrl"]);
        }
        if dl.is_empty() {
            return Err(friendly_err(
                Drive::Tianyi,
                "getFileDownloadUrl",
                &format!("{} 没有 downloadUrl", e.name),
            ));
        }
        out.push(PanResolved {
            fid: e.fid.clone(),
            name: e.name.clone(),
            size: e.size,
            url: dl,
            headers: vec![
                ("Cookie".to_string(), cookie.clone()),
                ("User-Agent".to_string(), UA.to_string()),
                ("Referer".to_string(), "https://cloud.189.cn/".to_string()),
            ],
        });
    }
    Ok(out)
}

fn urlenc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_tag_extraction() {
        let x = "<error><code>ShareInfoNotFound</code><message>boom</message></error>";
        assert_eq!(tag_of(x, "code").as_deref(), Some("ShareInfoNotFound"));
        assert_eq!(tag_of(x, "message").as_deref(), Some("boom"));
    }

    #[test]
    fn entry_parses_folder_and_file() {
        let f = serde_json::json!({"id":"123","name":"a.zip","size":456});
        let e = entry(&f, false);
        assert_eq!(e.fid, "123");
        assert_eq!(e.name, "a.zip");
        assert_eq!(e.size, 456);
        assert!(!e.is_dir);
    }

    #[test]
    fn cookie_merge() {
        assert_eq!(merge_cookie("", ""), "");
        assert_eq!(merge_cookie("a=1", ""), "a=1");
        assert_eq!(merge_cookie("a=1", "b=2"), "a=1; b=2");
    }
}
