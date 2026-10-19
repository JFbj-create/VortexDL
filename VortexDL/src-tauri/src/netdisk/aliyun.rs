//! 阿里云盘（阿里云盘 / 阿里云盘分享）驱动。
//!
//! # 实测状态（2026-10-13，打的是真实 HTTP 接口）
//!
//! | 步骤 | 结果 |
//! |---|---|
//! | `POST /v2/share_link/get_share_token` | ✅ 形状已验证：对不存在的分享返回结构化 `NotFound.ShareLink` |
//! | `POST /adrive/v3/file/list` | ✅ **免登录已证实**：只带 `x-share-token`、不带 `Authorization` 时，错误从 `AccessTokenInvalid` 变成 `ShareLinkTokenInvalid` —— 说明服务端只校验分享令牌，不要求账号 |
//! | `POST /v2/file/get_share_link_download_url` | ⚠️ 形状来自 OpenList，**没拿到真实分享链接，未跑通** |
//!
//! ★ 也就是说：**分享令牌这一步不需要登录**，这是它和百度/迅雷最大的区别。
//!   但要真正"取到直链并下完"还需要一条真实分享链接来验收（我手上没有）。
//!
//! # 两个必须记住的坑
//!
//! 1. **`Authorization` 头在阿里云盘是 `Bearer\t<token>`，中间是 TAB 不是空格**。
//!    这是它前端的历史包袱，用空格会被判无效。分享场景下我们干脆不发这个头。
//! 2. 必须带 `x-share-token`（分享令牌）和 `X-Canary: client=web,app=share,version=v2.3.1`
//!    （后者是"我是网页版分享"的自报家门，能放宽限流）。
use super::{friendly_err, PanEntry, PanResolved, Drive};

const API: &str = "https://api.alipan.com";
const CANARY: &str = "client=web,app=share,version=v2.3.1";

/// 阿里云盘的错误体形如 `{"code":"NotFound.ShareLink","message":"..."}`
fn check(v: &serde_json::Value) -> Result<(), String> {
    let code = v.get("code").and_then(|x| x.as_str()).unwrap_or("");
    if code.is_empty() {
        return Ok(());
    }
    let msg = v.get("message").and_then(|x| x.as_str()).unwrap_or("");
    let human = match code {
        "NotFound.ShareLink" | "ShareLinkNotFound" => "分享不存在或已失效",
        "ShareLinkTokenInvalid" => "分享令牌失效了（重新解析一次即可）",
        "ShareLinkExpired" => "分享已过期",
        "ShareLinkPasswordWrong" | "WrongPassword" => "提取码不对",
        "ShareLinkCancelled" => "分享已被取消",
        "AccessTokenInvalid" => "需要登录（请到「设置 → 网盘」填阿里云盘的 refresh_token）",
        _ => msg,
    };
    Err(friendly_err(Drive::Aliyun, "api", &format!("code={} {}", code, human)))
}

fn json_post(url: &str, body: &serde_json::Value, share_token: Option<&str>) -> reqwest::RequestBuilder {
    let mut r = super::http()
        .post(url)
        .header("content-type", "application/json")
        .header("Origin", "https://www.alipan.com")
        .header("Referer", "https://www.alipan.com/")
        .header("X-Canary", CANARY)
        .header("User-Agent", super::BROWSER_UA);
    if let Some(t) = share_token {
        if !t.is_empty() {
            r = r.header("x-share-token", t);
        }
    }
    r.json(body)
}

/// 拿分享令牌。**不需要登录**（实测）。
pub async fn get_share_token(share_id: &str, passcode: &str) -> Result<String, String> {
    let mut body = serde_json::json!({ "share_id": share_id });
    // ★ 提取码为空时**不要**塞 share_pwd 字段（塞空串会被判成"密码错误"）
    if !passcode.trim().is_empty() {
        body["share_pwd"] = serde_json::Value::String(passcode.trim().to_string());
    }
    let v: serde_json::Value = json_post(&format!("{}/v2/share_link/get_share_token", API), &body, None)
        .send()
        .await
        .map_err(|e| friendly_err(Drive::Aliyun, "get_share_token", &e.to_string()))?
        .json()
        .await
        .map_err(|e| friendly_err(Drive::Aliyun, "get_share_token", &e.to_string()))?;
    check(&v)?;
    let t = v.get("share_token").and_then(|x| x.as_str()).unwrap_or("");
    if t.is_empty() {
        return Err(friendly_err(Drive::Aliyun, "get_share_token", &v.to_string()));
    }
    Ok(t.to_string())
}

/// 解析 `/adrive/v3/file/list` 的返回（纯函数，可单测）
pub fn parse_files(v: &serde_json::Value) -> Vec<PanEntry> {
    v.get("items")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    let fid = f.get("file_id").and_then(|x| x.as_str())?.to_string();
                    Some(PanEntry {
                        fid,
                        name: f.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        // ★ 阿里云盘用字符串 "folder"/"file"，不是布尔也不是数字
                        is_dir: f.get("type").and_then(|x| x.as_str()).unwrap_or("") == "folder",
                        size: f.get("size").and_then(|x| x.as_i64()).unwrap_or(0).max(0) as u64,
                        token: String::new(),
                        path: String::new(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 列分享里某个目录（`parent` 传 `"root"` 是根目录）。免登录。
pub async fn list_dir(share_id: &str, passcode: &str, parent: &str, share_token: &str) -> Result<Vec<PanEntry>, String> {
    let body = serde_json::json!({
        "share_id": share_id,
        "parent_file_id": parent,
        "limit": 200,
        "order_by": "name",
        "order_direction": "ASC",
        "marker": "first",
        "image_thumbnail_process": "image/resize,w_160/format,jpeg",
        "image_url_process": "image/resize,w_1920/format/jpeg",
        "video_thumbnail_process": "video/snapshot,t_1000,f_jpg,ar_auto,w_300",
    });
    let v: serde_json::Value = json_post(&format!("{}/adrive/v3/file/list", API), &body, Some(share_token))
        .send()
        .await
        .map_err(|e| friendly_err(Drive::Aliyun, "file/list", &e.to_string()))?
        .json()
        .await
        .map_err(|e| friendly_err(Drive::Aliyun, "file/list", &e.to_string()))?;
    check(&v)?;
    Ok(parse_files(&v))
}

/// 列分享根目录。只有一个文件夹时自动进一层（和 123 那边同样的理由）。
pub async fn list_share(share_id: &str, passcode: &str) -> Result<Vec<PanEntry>, String> {
    let token = get_share_token(share_id, passcode).await?;
    let root = list_dir(share_id, passcode, "root", &token).await?;
    let files = root.iter().filter(|e| !e.is_dir).count();
    let dirs: Vec<&PanEntry> = root.iter().filter(|e| e.is_dir).collect();
    if files == 0 && dirs.len() == 1 {
        let inner = list_dir(share_id, passcode, &dirs[0].fid, &token).await?;
        if !inner.is_empty() {
            return Ok(inner);
        }
    }
    if root.is_empty() {
        return Err("这个分享里没有文件（或分享已失效）".to_string());
    }
    Ok(root)
}

/// 取直链：`POST /v2/file/get_share_link_download_url` → `url`
pub async fn resolve(share_id: &str, passcode: &str, entries: &[PanEntry]) -> Result<Vec<PanResolved>, String> {
    let token = get_share_token(share_id, passcode).await?;
    let mut out = Vec::new();
    for e in entries {
        let body = serde_json::json!({
            "share_id": share_id,
            "file_id": e.fid,
            "expire_sec": 900,
        });
        let v: serde_json::Value =
            json_post(&format!("{}/v2/file/get_share_link_download_url", API), &body, Some(&token))
                .send()
                .await
                .map_err(|err| friendly_err(Drive::Aliyun, "get_share_link_download_url", &err.to_string()))?
                .json()
                .await
                .map_err(|err| friendly_err(Drive::Aliyun, "get_share_link_download_url", &err.to_string()))?;
        check(&v)?;
        let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if url.is_empty() {
            return Err("阿里云盘没有返回下载地址".to_string());
        }
        out.push(PanResolved {
            fid: e.fid.clone(),
            name: e.name.clone(),
            size: e.size,
            url,
            // 阿里云盘的 CDN 认 Referer，不给会被拒
            headers: vec![("Referer".to_string(), "https://www.alipan.com/".to_string())],
        });
    }
    if out.is_empty() {
        return Err("阿里云盘没有返回可用的下载地址".to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_files_reads_folder_and_file() {
        // 结构按官方接口的字段名裁剪
        let v = serde_json::json!({"items":[
            {"file_id":"fld1","name":"电影","type":"folder","size":0},
            {"file_id":"fil1","name":"a.mkv","type":"file","size":123456}
        ]});
        let f = parse_files(&v);
        assert_eq!(f.len(), 2);
        assert!(f[0].is_dir);
        assert!(!f[1].is_dir);
        assert_eq!(f[1].size, 123456);
    }

    #[test]
    fn parse_files_missing_type_is_file_not_dir() {
        // ★ 缺 type 时必须当文件（夸克就栽在这种兜底上）
        let v = serde_json::json!({"items":[{"file_id":"x","name":"x.bin","size":1}]});
        let f = parse_files(&v);
        assert_eq!(f.len(), 1);
        assert!(!f[0].is_dir);
    }

    #[test]
    fn parse_files_skips_without_id() {
        assert!(parse_files(&serde_json::json!({"items":[{"name":"x"}]})).is_empty());
    }

    #[test]
    fn check_maps_codes_to_human() {
        assert!(check(&serde_json::json!({})).is_ok());
        assert!(check(&serde_json::json!({"items":[]})).is_ok());
        let e = check(&serde_json::json!({"code":"NotFound.ShareLink","message":"The resource share_link cannot be found. Please check."})).unwrap_err();
        assert!(e.contains("分享不存在"), "got {}", e);
        let e2 = check(&serde_json::json!({"code":"ShareLinkPasswordWrong","message":"x"})).unwrap_err();
        assert!(e2.contains("提取码"), "got {}", e2);
    }
}
