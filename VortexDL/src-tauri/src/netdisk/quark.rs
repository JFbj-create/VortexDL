//! 夸克网盘驱动。
//!
//! 接口形状来自三个独立实现的交叉验证（都是实测能跑的代码，不是文档）：
//!   · `niuhuan/quarkpan-rs`（Rust，自己网盘）
//!   · `OpenListTeam/OpenList` 的 `drivers/quark_uc`（Go）
//!   · `hmjz100/LinkSwift`（JS，唯一实现了**分享链接**下载的）
//!
//! # 流程
//! ```text
//!   POST /share/sharepage/token      {pwd_id, passcode}      -> stoken
//!   GET  /share/sharepage/detail     pwd_id,stoken,pdir_fid  -> list[]（带 share_fid_token）
//!   POST /file/download              {fids, fids_token, pwd_id, stoken}   ← 先试这条（不落盘）
//!        └ 失败就转存：
//!           POST /share/sharepage/save  -> task_id -> 轮询 GET /task 直到 status==2
//!           POST /file/download {fids:[新 fid]}  -> data[].download_url
//! ```
//!
//! ★ 下载直链是**按账号签名**的，必须带 Cookie 去 GET；不登录只有游客通道（有大小上限）。
use super::{client, friendly_err, PanEntry, PanResolved, Drive};

const BASE: &str = "https://drive-pc.quark.cn/1/clouddrive";
const SOCIAL_BASE: &str = "https://drive-social-api.quark.cn/1/clouddrive";

/// 夸克对 UA 挑得厉害：普通浏览器 UA 会被判成游客。
/// 这个串来自 LinkSwift / quarkpan-rs，两边一致。
const QUARK_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36 QuarkPC/6.9.7.761 \
     QuarkCloudDrive/6.9.7.761 quark-cloud-drive/2.5.40 Channel/pckk_other_ch";

/// 反爬参数：每次请求都要新的随机值
fn anti_bot() -> String {
    let t = crate::paths::now_ms();
    let dt = (t as u64).wrapping_mul(1103515245).wrapping_add(12345) % 100000;
    format!("pr=ucpro&fr=pc&uc_param_str=&__dt={}&__t={}", dt, t)
}

fn req(method: reqwest::Method, url: &str, cookie: &str) -> reqwest::RequestBuilder {
    let mut r = super::http()
        .request(method, url)
        .header("User-Agent", QUARK_UA)
        .header("Origin", "https://pan.quark.cn")
        .header("Referer", "https://pan.quark.cn/")
        .header("Accept", "application/json, text/plain, */*");
    if !cookie.trim().is_empty() {
        r = r.header("Cookie", cookie.trim());
    }
    r
}

fn check(body: &serde_json::Value) -> Result<(), String> {
    let status = body.get("status").and_then(|x| x.as_i64()).unwrap_or(-1);
    let code = body.get("code").and_then(|x| x.as_i64()).unwrap_or(0);
    if status == 200 || code == 0 {
        return Ok(());
    }
    let msg = body.get("message").and_then(|x| x.as_str()).unwrap_or("");
    Err(format!("code={} status={} {}", code, status, msg))
}

/// 从响应头里收 Set-Cookie（只要 __pugs / __pus / __puus —— 这些是夸克下发的会话态）
fn pick_set_cookie(resp: &reqwest::Response) -> Vec<String> {
    let mut out = Vec::new();
    for v in resp.headers().get_all(reqwest::header::SET_COOKIE).iter() {
        if let Ok(sv) = v.to_str() {
            let first = sv.split(';').next().unwrap_or("").trim();
            if first.starts_with("__pugs=") || first.starts_with("__pus=") || first.starts_with("__puus=") {
                out.push(first.to_string());
            }
        }
    }
    out
}

/// 分享 -> (stoken, 服务端下发的会话 cookie)
pub async fn share_token(pwd_id: &str, passcode: &str, cookie: &str) -> Result<(String, Vec<String>), String> {
    let url = format!("{}/share/sharepage/token?{}", BASE, anti_bot());
    let body = serde_json::json!({ "pwd_id": pwd_id, "passcode": passcode });
    let resp = req(reqwest::Method::POST, &url, cookie)
        .json(&body)
        .send()
        .await
        .map_err(|e| friendly_err(Drive::Quark, "sharepage/token", &e.to_string()))?;
    let setck = pick_set_cookie(&resp);
    let text = resp.text().await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| friendly_err(Drive::Quark, "sharepage/token", &text))?;
    check(&v).map_err(|e| friendly_err(Drive::Quark, "sharepage/token", &e))?;
    let stoken = v
        .pointer("/data/stoken")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| friendly_err(Drive::Quark, "sharepage/token", "返回里没有 stoken"))?;
    Ok((stoken, setck))
}

/// 列一层目录
async fn list_dir(
    pwd_id: &str,
    stoken: &str,
    pdir_fid: &str,
    cookie: &str,
) -> Result<(Vec<PanEntry>, bool), String> {
    let mut out = Vec::new();
    let mut page = 1;
    let mut is_owner = false;
    loop {
        let url = format!(
            "{}/share/sharepage/detail?{}&pwd_id={}&stoken={}&pdir_fid={}\
             &force=0&_page={}&_size=100&_sort=file_type:asc,updated_at:desc",
            BASE, anti_bot(), pwd_id, urlencode(stoken), pdir_fid, page
        );
        let resp = req(reqwest::Method::GET, &url, cookie)
            .send()
            .await
            .map_err(|e| friendly_err(Drive::Quark, "sharepage/detail", &e.to_string()))?;
        let text = resp.text().await.unwrap_or_default();
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|_| friendly_err(Drive::Quark, "sharepage/detail", &text))?;
        check(&v).map_err(|e| friendly_err(Drive::Quark, "sharepage/detail", &e))?;

        is_owner = v.pointer("/data/is_owner").and_then(|x| x.as_i64()).unwrap_or(0) == 1;
        let list = v
            .pointer("/data/list")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        if list.is_empty() {
            break;
        }
        for it in &list {
            out.push(entry_of(it, ""));
        }
        let total = v
            .pointer("/metadata/_total")
            .and_then(|x| x.as_u64())
            .unwrap_or(out.len() as u64);
        if out.len() as u64 >= total || list.len() < 100 {
            break;
        }
        page += 1;
        if page > 50 {
            break; // 保险丝
        }
    }
    Ok((out, is_owner))
}

fn entry_of(it: &serde_json::Value, prefix: &str) -> PanEntry {
    let name = it.get("file_name").and_then(|x| x.as_str()).unwrap_or("").to_string();
    // ★ 只有 `dir` 字段**缺失**时才退回看 file_type。
    //   第一版写成 `dir || file_type==0`，而 file_type 缺省值是 0 → 所有没有
    //   file_type 的返回都被判成目录，结果"分享里一个文件都列不出来"。
    let dir = match it.get("dir").and_then(|x| x.as_bool()) {
        Some(b) => b,
        None => it.get("file_type").and_then(|x| x.as_i64()).unwrap_or(1) == 0,
    };
    PanEntry {
        fid: it.get("fid").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        path: if prefix.is_empty() { name.clone() } else { format!("{}/{}", prefix, name) },
        name,
        is_dir: dir,
        size: it.get("size").and_then(|x| x.as_u64()).unwrap_or(0),
        token: it
            .get("share_fid_token")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
    }
}

/// 列出分享里的**所有文件**（目录递归展开，最多 4 层 / 2000 条）
pub async fn list_share(pwd_id: &str, passcode: &str) -> Result<Vec<PanEntry>, String> {
    let ck = super::get_cookie(Drive::Quark);
    let cookie = ck.cookie;
    let (stoken, _ck) = share_token(pwd_id, passcode, &cookie).await?;

    let (top, _is_owner) = list_dir(pwd_id, &stoken, "0", &cookie).await?;
    let mut files: Vec<PanEntry> = Vec::new();
    let mut stack: Vec<(PanEntry, usize)> =
        top.into_iter().map(|e| (e, 0usize)).collect();

    while let Some((e, depth)) = stack.pop() {
        if files.len() > 2000 {
            break;
        }
        if e.is_dir && depth < 4 {
            match list_dir(pwd_id, &stoken, &e.fid, &cookie).await {
                Ok((kids, _)) => {
                    for k in kids {
                        let mut k2 = k;
                        k2.path = format!("{}/{}", e.path, k2.name);
                        stack.push((k2, depth + 1));
                    }
                }
                // 单层失败不要整个任务失败，跳过继续
                Err(err) => {
                    crate::app_logger::log_command("netdisk", &format!("[夸克] 展开目录 {} 失败: {}", e.path, err));
                }
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

/// 游客下载需要 `__pugs` —— 访问分享页时夸克会在 Set-Cookie 里下发。
/// ★ 不登录也能拿到，只是有**大小上限**（超了会返回 code 23018）。
/// 有登录 Cookie 时这个用不着，但带上无害，所以无脑合并。
async fn guest_cookie(pwd_id: &str) -> String {
    let url = format!("https://pan.quark.cn/s/{}", pwd_id);
    let resp = match req(reqwest::Method::GET, &url, "").send().await {
        Ok(r) => r,
        Err(e) => {
            crate::app_logger::log_command("netdisk", &format!("[夸克] 取游客 cookie 失败: {}", e));
            return String::new();
        }
    };
    let mut parts: Vec<String> = Vec::new();
    for v in resp.headers().get_all(reqwest::header::SET_COOKIE).iter() {
        if let Ok(sv) = v.to_str() {
            // Set-Cookie 形如 "__pugs=xxx; Path=/; ..."，只要第一段
            let first = sv.split(';').next().unwrap_or("").trim();
            if first.starts_with("__pugs=") || first.starts_with("__pus=") {
                parts.push(first.to_string());
            }
        }
    }
    let joined = parts.join("; ");
    crate::app_logger::log_command(
        "netdisk",
        &format!("[夸克] 游客 cookie: {}", if joined.is_empty() { "没拿到".into() } else { format!("{} 项", parts.len()) }),
    );
    joined
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// 拿直链：先试"分享直下"，失败再"转存到自己网盘再下"。
pub async fn resolve(pwd_id: &str, passcode: &str, entries: &[PanEntry]) -> Result<Vec<PanResolved>, String> {
    let ck = super::get_cookie(Drive::Quark);
    // ★ 不登录时也要能试一次「分享直下」：把分享页下发的游客 cookie 合并进来。
    //   有登录 cookie 的话这段是多余的，但不会有害。
    let mut guest = guest_cookie(pwd_id).await;
    let mut cookie = if guest.is_empty() {
        ck.cookie.clone()
    } else if ck.cookie.trim().is_empty() {
        guest.clone()
    } else {
        format!("{}; {}", ck.cookie.trim_end_matches(';'), guest)
    };
    let (stoken, setck) = share_token(pwd_id, passcode, &cookie).await?;
    // ★ token 接口的 Set-Cookie 里通常才有 __pugs（游客下载要用），
    //   拿到就并进去再试一次直下 —— 这是"不登录也能下小文件"的关键。
    if !setck.is_empty() {
        let extra = setck.join("; ");
        cookie = if cookie.trim().is_empty() { extra.clone() } else { format!("{}; {}", cookie.trim_end_matches(';'), extra) };
        guest = format!("{}; {}", guest, extra);
        crate::app_logger::log_command("netdisk", &format!("[夸克] token 接口下发 cookie {} 项，已并入", setck.len()));
    }

    // ★★ 关键：`share_fid_token` 是**绑在 stoken 会话上**的。
    //   上一轮 list_share 拿到的 token 配这一轮新取的 stoken 会直接被夸克拒掉
    //   （报 code=41020 "转存文件token校验异常" —— 看起来像"没登录"，其实是 token 不匹配）。
    //   所以这里**用新 stoken 重新列一遍**，按 fid 取新鲜的 token。
    let tokens: Vec<String> = {
        let mut map = std::collections::HashMap::new();
        let mut stack = vec!["0".to_string()];
        let mut seen = 0;
        while let Some(dir) = stack.pop() {
            if seen > 40 { break; }
            seen += 1;
            match list_dir(pwd_id, &stoken, &dir, &cookie).await {
                Ok((kids, _)) => {
                    for k in kids {
                        if k.is_dir {
                            stack.push(k.fid.clone());
                        } else if !k.token.is_empty() {
                            map.insert(k.fid.clone(), k.token.clone());
                        }
                    }
                }
                Err(e) => {
                    crate::app_logger::log_command("netdisk", &format!("[夸克] 刷新 token 时列目录失败: {}", e));
                }
            }
        }
        entries
            .iter()
            .map(|e| map.get(&e.fid).cloned().unwrap_or_else(|| e.token.clone()))
            .collect()
    };
    let fids: Vec<String> = entries.iter().map(|e| e.fid.clone()).collect();

    // ---- 路线 1：带 share token 直接要下载地址（不落盘，最干净）----
    if tokens.iter().all(|t| !t.is_empty()) {
        let social = social_dl_token(&cookie).await.unwrap_or_default();
        let url = format!("{}/file/download?{}&sys=win32&ve=6.9.7.761", BASE, anti_bot());
        let body = serde_json::json!({
            "fids": fids,
            "fids_token": tokens,
            "pwd_id": pwd_id,
            "stoken": stoken,
            "speedup_session": "",
            "token": social,
        });
        match post_download(&url, &body, &cookie).await {
            Ok(list) if !list.is_empty() => {
                return Ok(merge(entries, list, &cookie));
            }
            Ok(_) => {}
            Err(e) => {
                crate::app_logger::log_command("netdisk", &format!("[夸克] 分享直下失败，改走转存: {}", e));
            }
        }
    }

    // ---- 路线 2：转存到自己网盘根目录，再用自己的 fid 拿地址 ----
    let new_fids = save_share(pwd_id, &stoken, &fids, &tokens, &cookie).await?;
    let url = format!("{}/file/download?{}&sys=win32&ve=6.9.7.761", BASE, anti_bot());
    let body = serde_json::json!({ "fids": new_fids });
    let list = post_download(&url, &body, &cookie).await?;
    if list.is_empty() {
        return Err("转存成功但没拿到下载地址（可能该文件被网盘限制下载）".to_string());
    }
    Ok(merge(entries, list, &cookie))
}

/// POST /file/download -> [(file_name, download_url, size)]
async fn post_download(
    url: &str,
    body: &serde_json::Value,
    cookie: &str,
) -> Result<Vec<(String, String, u64)>, String> {
    let resp = req(reqwest::Method::POST, url, cookie)
        .json(body)
        .send()
        .await
        .map_err(|e| friendly_err(Drive::Quark, "file/download", &e.to_string()))?;
    let text = resp.text().await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| friendly_err(Drive::Quark, "file/download", &text))?;
    check(&v).map_err(|e| friendly_err(Drive::Quark, "file/download", &e))?;
    let arr = v
        .pointer("/data")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(arr
        .iter()
        .filter_map(|x| {
            let u = x.get("download_url").and_then(|y| y.as_str())?;
            Some((
                x.get("file_name").and_then(|y| y.as_str()).unwrap_or("").to_string(),
                u.to_string(),
                x.get("size").and_then(|y| y.as_u64()).unwrap_or(0),
            ))
        })
        .collect())
}

/// 分享直下要一个"社交下载 token"；拿不到就算了（不影响转存路线）
async fn social_dl_token(cookie: &str) -> Result<String, String> {
    let url = format!(
        "{}/chat/conv/file/acquire_dl_token?{}&sys=win32&ve=6.9.7.761&fr=win&la=zh-CN&ch=pckk%40product_guanwan",
        SOCIAL_BASE, anti_bot()
    );
    let resp = req(reqwest::Method::GET, &url, cookie)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let text = resp.text().await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|_| text.clone())?;
    for p in ["/data/token", "/data/dl_token", "/data"] {
        if let Some(s) = v.pointer(p).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return Ok(s.to_string());
            }
        }
    }
    Err("没有社交下载 token".to_string())
}

/// 转存到自己的根目录，返回**新的 fid**（轮询转存任务直到完成）
async fn save_share(
    pwd_id: &str,
    stoken: &str,
    fids: &[String],
    tokens: &[String],
    cookie: &str,
) -> Result<Vec<String>, String> {
    if cookie.trim().is_empty() {
        return Err("夸克下载分享文件需要登录：请到「设置 → 网盘」粘贴你的夸克 Cookie".to_string());
    }
    let url = format!("{}/share/sharepage/save?{}", BASE, anti_bot());
    let body = serde_json::json!({
        "fid_list": fids,
        "fid_token_list": tokens,
        "to_pdir_fid": "0",
        "pwd_id": pwd_id,
        "stoken": stoken,
        "pdir_fid": "0",
        "scene": "link",
    });
    let resp = req(reqwest::Method::POST, &url, cookie)
        .json(&body)
        .send()
        .await
        .map_err(|e| friendly_err(Drive::Quark, "sharepage/save", &e.to_string()))?;
    let text = resp.text().await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| friendly_err(Drive::Quark, "sharepage/save", &text))?;
    check(&v).map_err(|e| friendly_err(Drive::Quark, "sharepage/save", &e))?;

    let task_id = v
        .pointer("/data/task_id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if task_id.is_empty() {
        return Err(friendly_err(Drive::Quark, "sharepage/save", "返回里没有 task_id"));
    }

    // 轮询任务（转存是异步的）
    for i in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let u = format!("{}/task?{}&task_id={}&retry_index={}", BASE, anti_bot(), task_id, i);
        let r = req(reqwest::Method::GET, &u, cookie)
            .send()
            .await
            .map_err(|e| friendly_err(Drive::Quark, "task", &e.to_string()))?;
        let t = r.text().await.unwrap_or_default();
        let tv: serde_json::Value = match serde_json::from_str(&t) {
            Ok(x) => x,
            Err(_) => continue,
        };
        let status = tv.pointer("/data/status").and_then(|x| x.as_i64()).unwrap_or(0);
        let msg = tv.pointer("/message").and_then(|x| x.as_str()).unwrap_or("");
        if status == 2 && msg == "ok" {
            // 转存完成后，去根目录找刚存进去的文件（按名字匹配）
            return find_saved_fids(fids.len(), cookie).await;
        }
        if status == 3 {
            return Err(friendly_err(Drive::Quark, "task", &format!("转存失败: {}", t)));
        }
    }
    Err("转存超时（夸克没在 24 秒内完成任务）".to_string())
}

/// 转存完不知道新 fid，去根目录按"最新更新"取前 N 个
async fn find_saved_fids(want: usize, cookie: &str) -> Result<Vec<String>, String> {
    let url = format!(
        "{}/file/sort?{}&pdir_fid=0&_page=1&_size={}&_fetch_total=1&_fetch_sub_dirs=0\
         &_sort=file_type:asc,updated_at:desc",
        BASE,
        anti_bot(),
        want.max(1)
    );
    let resp = req(reqwest::Method::GET, &url, cookie)
        .send()
        .await
        .map_err(|e| friendly_err(Drive::Quark, "file/sort", &e.to_string()))?;
    let text = resp.text().await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| friendly_err(Drive::Quark, "file/sort", &text))?;
    let list = v
        .pointer("/data/list")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let fids: Vec<String> = list
        .iter()
        .take(want)
        .filter_map(|x| x.get("fid").and_then(|y| y.as_str()).map(|s| s.to_string()))
        .collect();
    if fids.is_empty() {
        return Err("转存完成但没在网盘根目录找到文件".to_string());
    }
    Ok(fids)
}

/// 把 download_url 列表和原始 entries 对齐（夸克按 fid 顺序返回，但名字以返回的为准）
fn merge(entries: &[PanEntry], list: Vec<(String, String, u64)>, cookie: &str) -> Vec<PanResolved> {
    let headers = vec![
        ("Cookie".to_string(), cookie.to_string()),
        ("Referer".to_string(), "https://pan.quark.cn/".to_string()),
        ("User-Agent".to_string(), QUARK_UA.to_string()),
    ];
    list.into_iter()
        .enumerate()
        .map(|(i, (name, url, size))| {
            let e = entries.get(i);
            PanResolved {
                fid: e.map(|x| x.fid.clone()).unwrap_or_default(),
                name: if name.is_empty() {
                    e.map(|x| x.name.clone()).unwrap_or_else(|| format!("quark_{}.bin", i))
                } else {
                    name
                },
                size: if size > 0 { size } else { e.map(|x| x.size).unwrap_or(0) },
                url,
                headers: headers
                    .iter()
                    .filter(|(_, v)| !v.trim().is_empty())
                    .cloned()
                    .collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anti_bot_has_required_params() {
        let s = anti_bot();
        assert!(s.contains("pr=ucpro"));
        assert!(s.contains("fr=pc"));
        assert!(s.contains("__dt="));
        assert!(s.contains("__t="));
    }

    #[test]
    fn anti_bot_changes_every_call() {
        let a = anti_bot();
        std::thread::sleep(std::time::Duration::from_millis(3));
        let b = anti_bot();
        assert_ne!(a, b, "反爬参数必须每次都不同");
    }

    #[test]
    fn entry_parses_dir_flag() {
        let v = serde_json::json!({
            "fid": "f1", "file_name": "电影", "dir": true, "size": 0,
            "share_fid_token": "tk"
        });
        let e = entry_of(&v, "");
        assert!(e.is_dir);
        assert_eq!(e.name, "电影");
        assert_eq!(e.token, "tk");
    }

    #[test]
    fn entry_parses_file() {
        let v = serde_json::json!({
            "fid": "f2", "file_name": "a.mkv", "dir": false, "size": 123,
            "share_fid_token": "tk2"
        });
        let e = entry_of(&v, "电影");
        assert!(!e.is_dir);
        assert_eq!(e.size, 123);
        assert_eq!(e.path, "电影/a.mkv");
    }

    #[test]
    fn check_rejects_error_payload() {
        let bad = serde_json::json!({"status": 500, "code": 31001, "message": "请先登录网盘"});
        assert!(check(&bad).is_err());
        let good = serde_json::json!({"status": 200, "code": 0});
        assert!(check(&good).is_ok());
    }

    #[test]
    fn merge_keeps_headers_without_empty_cookie() {
        let entries = vec![PanEntry {
            fid: "1".into(), name: "x.bin".into(), is_dir: false, size: 5,
            token: "t".into(), path: "x.bin".into(),
        }];
        let out = merge(&entries, vec![("x.bin".into(), "https://cdn/x".into(), 5)], "");
        assert_eq!(out.len(), 1);
        // 没配 cookie 时不能塞一个空的 Cookie 头
        assert!(out[0].headers.iter().all(|(k, v)| !(k == "Cookie" && v.is_empty())));
        assert!(out[0].headers.iter().any(|(k, _)| k == "Referer"));
    }

    #[test]
    fn urlencode_handles_token_chars() {
        assert_eq!(urlencode("a+b/c="), "a%2Bb%2Fc%3D");
    }
}
