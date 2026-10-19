//! 夸克网盘驱动。
//
//! 接口形状来自三个独立实现的交叉验证（都是实测能跑的代码，不是文档）：
//!   · `niuhuan/quarkpan-rs`（Rust，自己网盘）
//!   · `OpenListTeam/OpenList` 的 `drivers/quark_uc`（Go）
//!   · `hmjz100/LinkSwift`（JS，唯一实现了**分享链接**下载的）
//
//! # 流程
//! ```text
//!   POST /share/sharepage/token      {pwd_id, passcode}      -> stoken
//!   GET  /share/sharepage/detail     pwd_id,stoken,pdir_fid  -> list[]（带 share_fid_token）
//!   POST /file/download              {fids, fids_token, pwd_id, stoken}   ← 先试这条（不落盘）
//!        └ 失败就转存：
//!           POST /share/sharepage/save  -> task_id -> 轮询 GET /task 直到 status==2
//!           POST /file/download {fids:[新 fid]}  -> data[].download_url
//! ```
//
//! ★ 下载直链是**按账号签名**的，必须带 Cookie 去 GET；不登录只有游客通道（有大小上限）。
use super::{client, friendly_err, PanEntry, PanResolved, Drive};

// 夸克系驱动：**夸克网盘 + UC 网盘**。
//
// UC 网盘（drive.uc.cn / fast.uc.cn）和夸克是同一套云盘后端，接口路径、参数、
// 返回结构一致，差异只有三处：域名（pc-api.uc.cn）、客户端 UA、以及 `pr` 参数
// （夸克 `ucpro` / UC `UCBrowser`）。所以这里用 `Fam` 把差异收拢，流程只写一份。
//
// ★ 2026-10-18：以前 `pan.uc.cn/s/xxx` 被当成夸克直接打到夸克接口上，必然失败。
//   现在按域名分流到各自的 `Fam`。

/// 夸克系里一个具体网盘的差异参数
#[derive(Clone, Copy)]
pub struct Fam {
    pub drive: Drive,
    pub base: &'static str,
    pub ua: &'static str,
    pub pr: &'static str,
    pub origin: &'static str,
    pub referer: &'static str,
    /// 分享页前缀（取游客 cookie 用）
    pub guest_page: &'static str,
    /// 客户端版本号（`ve=` 参数）
    pub ve: &'static str,
    /// ★★ 免登录取直链的备用入口：`drive.quark.cn` + `pr=ucpro_new`。
    ///   `drive-pc` + `pr=ucpro` 对游客超过一定大小会直接回 23018 "download file size limit"，
    ///   而同一套接口换成 `drive.quark.cn/...&pr=ucpro_new` 就能拿到 download_url
    ///   （实测 1GB 的包：取链成功，带本次下发 `__pugs` 的 Range 请求返回 206）。
    pub guest_base: &'static str,
    pub guest_pr: &'static str,
}

/// 夸克对 UA 挑得厉害：普通浏览器 UA 会被判成游客。
pub const QUARK: Fam = Fam {
    drive: Drive::Quark,
    base: "https://drive-pc.quark.cn/1/clouddrive",
    ua: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36 QuarkPC/6.9.7.761 \
     QuarkCloudDrive/6.9.7.761 quark-cloud-drive/2.5.40 Channel/pckk_other_ch",
    pr: "ucpro",
    origin: "https://pan.quark.cn",
    referer: "https://pan.quark.cn/",
    guest_page: "https://pan.quark.cn/s/",
    ve: "6.9.7.761",
    guest_base: "https://drive.quark.cn/1/clouddrive",
    guest_pr: "ucpro_new",
};

/// UC 网盘：同一套后端，客户端是 uc-cloud-drive。
pub const UC: Fam = Fam {
    drive: Drive::Uc,
    base: "https://pc-api.uc.cn/1/clouddrive",
    ua: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) uc-cloud-drive/1.8.8 Chrome/100.0.4896.160 \
     Electron/18.3.5.16-b62cf9c50d Safari/537.36 Channel/ucpan_other_ch",
    pr: "UCBrowser",
    origin: "https://drive.uc.cn",
    referer: "https://drive.uc.cn/",
    guest_page: "https://drive.uc.cn/s/",
    ve: "1.8.8",
    // UC 没有对应的游客分支，填自己（失败会自然回退）
    guest_base: "https://pc-api.uc.cn/1/clouddrive",
    guest_pr: "UCBrowser",
};

/// 反爬参数：每次请求都要新的随机值
fn anti_bot(fam: &Fam) -> String {
    anti_bot_pr(fam, fam.pr)
}

fn anti_bot_pr(_fam: &Fam, pr: &str) -> String {
    let t = crate::paths::now_ms();
    let dt = (t as u64).wrapping_mul(1103515245).wrapping_add(12345) % 100000;
    format!("pr={}&fr=pc&uc_param_str=&__dt={}&__t={}", pr, dt, t)
}

fn req(fam: &Fam, method: reqwest::Method, url: &str, cookie: &str) -> reqwest::RequestBuilder {
    let mut r = super::http()
        .request(method, url)
        .header("User-Agent", fam.ua)
        .header("Origin", fam.origin)
        .header("Referer", fam.referer)
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
pub async fn share_token(fam: &Fam, pwd_id: &str, passcode: &str, cookie: &str) -> Result<(String, Vec<String>), String> {
    let url = format!("{}/share/sharepage/token?{}", fam.base, anti_bot(fam));
    let body = serde_json::json!({ "pwd_id": pwd_id, "passcode": passcode });
    let resp = req(fam, reqwest::Method::POST, &url, cookie)
        .json(&body)
        .send()
        .await
        .map_err(|e| friendly_err(fam.drive, "sharepage/token", &e.to_string()))?;
    let setck = pick_set_cookie(&resp);
    let text = resp.text().await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| friendly_err(fam.drive, "sharepage/token", &text))?;
    check(&v).map_err(|e| friendly_err(fam.drive, "sharepage/token", &e))?;
    let stoken = v
        .pointer("/data/stoken")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| friendly_err(fam.drive, "sharepage/token", "返回里没有 stoken"))?;
    Ok((stoken, setck))
}

/// 列一层目录
async fn list_dir(
    fam: &Fam,
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
            fam.base, anti_bot(fam), pwd_id, urlencode(stoken), pdir_fid, page
        );
        let resp = req(fam, reqwest::Method::GET, &url, cookie)
            .send()
            .await
            .map_err(|e| friendly_err(fam.drive, "sharepage/detail", &e.to_string()))?;
        let text = resp.text().await.unwrap_or_default();
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|_| friendly_err(fam.drive, "sharepage/detail", &text))?;
        check(&v).map_err(|e| friendly_err(fam.drive, "sharepage/detail", &e))?;

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
pub async fn list_share_fam(fam: &Fam, pwd_id: &str, passcode: &str) -> Result<Vec<PanEntry>, String> {
    let ck = super::get_cookie(fam.drive);
    let cookie = ck.cookie;
    let (stoken, _ck) = share_token(fam, pwd_id, passcode, &cookie).await?;

    let (top, _is_owner) = list_dir(fam, pwd_id, &stoken, "0", &cookie).await?;
    let mut files: Vec<PanEntry> = Vec::new();
    let mut stack: Vec<(PanEntry, usize)> =
        top.into_iter().map(|e| (e, 0usize)).collect();

    while let Some((e, depth)) = stack.pop() {
        if files.len() > 2000 {
            break;
        }
        if e.is_dir && depth < 4 {
            match list_dir(fam, pwd_id, &stoken, &e.fid, &cookie).await {
                Ok((kids, _)) => {
                    for k in kids {
                        let mut k2 = k;
                        k2.path = format!("{}/{}", e.path, k2.name);
                        stack.push((k2, depth + 1));
                    }
                }
                // 单层失败不要整个任务失败，跳过继续
                Err(err) => {
                    crate::app_logger::log_command("netdisk", &format!("[{}] 展开目录 {} 失败: {}", fam.drive.name(), e.path, err));
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

/// 夸克分享列目录
pub async fn list_share(pwd_id: &str, passcode: &str) -> Result<Vec<PanEntry>, String> {
    list_share_fam(&QUARK, pwd_id, passcode).await
}

/// UC 网盘分享列目录（同一套后端，差异参数见 `UC`）
pub async fn list_share_uc(pwd_id: &str, passcode: &str) -> Result<Vec<PanEntry>, String> {
    list_share_fam(&UC, pwd_id, passcode).await
}

/// 游客下载需要 `__pugs` —— 访问分享页时夸克会在 Set-Cookie 里下发。
/// ★ 不登录也能拿到，只是有**大小上限**（超了会返回 code 23018）。
/// 有登录 Cookie 时这个用不着，但带上无害，所以无脑合并。
async fn guest_cookie(fam: &Fam, pwd_id: &str) -> String {
    let url = format!("{}{}", fam.guest_page, pwd_id);
    let resp = match req(fam, reqwest::Method::GET, &url, "").send().await {
        Ok(r) => r,
        Err(e) => {
            crate::app_logger::log_command("netdisk", &format!("[{}] 取游客 cookie 失败: {}", fam.drive.name(), e));
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
pub async fn resolve_fam(fam: &Fam, pwd_id: &str, passcode: &str, entries: &[PanEntry]) -> Result<Vec<PanResolved>, String> {
    let ck = super::get_cookie(fam.drive);
    // ★ 不登录时也要能试一次「分享直下」：把分享页下发的游客 cookie 合并进来。
    //   有登录 cookie 的话这段是多余的，但不会有害。
    let mut guest = guest_cookie(fam, pwd_id).await;
    let mut cookie = if guest.is_empty() {
        ck.cookie.clone()
    } else if ck.cookie.trim().is_empty() {
        guest.clone()
    } else {
        format!("{}; {}", ck.cookie.trim_end_matches(';'), guest)
    };
    let (stoken, setck) = share_token(fam, pwd_id, passcode, &cookie).await?;
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
            match list_dir(fam, pwd_id, &stoken, &dir, &cookie).await {
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
        let url = format!("{}/file/download?{}&sys=win32&ve={}", fam.base, anti_bot(fam), fam.ve);
        let has_login = !ck.cookie.trim().is_empty();
        let body = serde_json::json!({
            "fids": fids,
            "fids_token": tokens,
            "pwd_id": pwd_id,
            "stoken": stoken,
            "speedup_session": "",
            // ★ 游客取不到「社交转存令牌」（取它本身要登录），官方前端也是 catch 之后传空串
            "token": "",
        });
        // ★★ 游客路径：**首次请求一个 Cookie 都不要带** —— 带了反而拿不到 __pugs。
        let first_cookie = if has_login { cookie.clone() } else { String::new() };
        let attempt = post_download(fam, &url, &body, &first_cookie).await;
        // ★★★ 2026-10-19：游客大文件会被 `drive-pc + pr=ucpro` 拒（code 23018
        //   "download file size limit"）。同一套接口换 `drive.quark.cn + pr=ucpro_new`
        //   就能拿到直链（实测 1GB 包可用），所以这里自动重试一次。
        let need_guest_retry = match &attempt {
            Err(e) => e.contains("23018") || e.contains("size limit") || e.contains("游客"),
            Ok(r) => r.items.is_empty(),
        };
        if need_guest_retry && !has_login {
            if let Some(r) = guest_download(fam, pwd_id, &stoken, &fids, &tokens).await {
                let dl_cookie = r.pugs.clone().unwrap_or_default();
                return Ok(merge(fam, entries, r.items, &dl_cookie));
            }
        }
        match attempt {
            Ok(resp) if !resp.items.is_empty() => {
                // ★★★ 用这次响应下发的 __pugs 当下载 cookie（缺它 CDN 直接 412）。
                let dl_cookie = match &resp.pugs {
                    Some(p) => {
                        crate::app_logger::log_command("netdisk", "[夸克] 拿到游客令牌 __pugs");
                        if has_login {
                            format!("{}; {}", cookie.trim_end_matches(';'), p)
                        } else {
                            p.clone()
                        }
                    }
                    None if has_login => cookie.clone(),
                    None => {
                        crate::app_logger::log_command("netdisk", "[夸克] 没拿到 __pugs，直链可能 412");
                        String::new()
                    }
                };
                return Ok(merge(fam, entries, resp.items, &dl_cookie));
            }
            Ok(_) => {}
            Err(e) => {
                crate::app_logger::log_command("netdisk", &format!("[夸克] 分享直下失败，改走转存: {}", e));
            }
        }
    }

    // ---- 路线 2：转存到自己网盘根目录，再用自己的 fid 拿地址 ----
    let new_fids = save_share(fam, pwd_id, &stoken, &fids, &tokens, &cookie).await?;
    let url = format!("{}/file/download?{}&sys=win32&ve={}", fam.base, anti_bot(fam), fam.ve);
    let body = serde_json::json!({ "fids": new_fids });
    let resp = post_download(fam, &url, &body, &cookie).await?;
    if resp.items.is_empty() {
        return Err("转存成功但没拿到下载地址（可能该文件被网盘限制下载）".to_string());
    }
    let dl_cookie = match &resp.pugs {
        Some(p) if !cookie.trim().is_empty() => format!("{}; {}", cookie.trim_end_matches(';'), p),
        Some(p) => p.clone(),
        None => cookie.clone(),
    };
    Ok(merge(fam, entries, resp.items, &dl_cookie))
}

/// 夸克分享取直链
pub async fn resolve(pwd_id: &str, passcode: &str, entries: &[PanEntry]) -> Result<Vec<PanResolved>, String> {
    resolve_fam(&QUARK, pwd_id, passcode, entries).await
}

/// UC 网盘分享取直链
pub async fn resolve_uc(pwd_id: &str, passcode: &str, entries: &[PanEntry]) -> Result<Vec<PanResolved>, String> {
    resolve_fam(&UC, pwd_id, passcode, entries).await
}

/// ★★ 游客免登录取直链（大文件也走这条路）。
///
/// 用 `drive.quark.cn` + `pr=ucpro_new` 重新取一次 stoken（token 是按 pr 绑定的，
/// 拿 `ucpro` 的 stoken 去 `ucpro_new` 会被拒），再要 download。
/// 成功后返回的 `__pugs` **必须** 跟直链一起带走，否则 CDN 直接 412。
async fn guest_download(
    fam: &Fam,
    pwd_id: &str,
    stoken: &str,
    fids: &[String],
    tokens: &[String],
) -> Option<DlResp> {
    // 1) 用 ucpro_new 重新取 stoken
    let url = format!("{}/share/sharepage/token?{}", fam.guest_base, anti_bot_pr(fam, fam.guest_pr));
    let body = serde_json::json!({ "pwd_id": pwd_id, "passcode": "" });
    let resp = req(fam, reqwest::Method::POST, &url, "").json(&body).send().await.ok()?;
    let text = resp.text().await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    if check(&v).is_err() {
        crate::app_logger::log_command("netdisk", &format!("[夸克] 游客入口取 stoken 失败: {}", text.chars().take(160).collect::<String>()));
        return None;
    }
    let g_stoken = v.pointer("/data/stoken").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if g_stoken.is_empty() {
        return None;
    }
    crate::app_logger::log_command(
        "netdisk",
        &format!("[夸克] 游客入口(ucpro_new)取新 stoken: {}", if g_stoken == stoken { "同" } else { "不同" }),
    );

    // ★★★ `share_fid_token` 是**绑在 stoken 上**的（同夸克那边 41020 的坑）。
    //   换了 stoken 就必须用新 stoken 重新列一遍，否则报「转存文件token校验异常」。
    let mut fresh: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    {
        let mut stack = vec!["0".to_string()];
        let mut seen = 0;
        while let Some(dir) = stack.pop() {
            if seen > 40 {
                break;
            }
            seen += 1;
            let u = format!(
                "{}/share/sharepage/detail?{}&pwd_id={}&stoken={}&pdir_fid={}&force=0&_page=1&_size=50&_sort=file_type:asc,updated_at:desc",
                fam.guest_base,
                anti_bot_pr(fam, fam.guest_pr),
                pwd_id,
                urlencode(&g_stoken),
                dir
            );
            let r = match req(fam, reqwest::Method::GET, &u, "").send().await {
                Ok(r) => r,
                Err(_) => break,
            };
            let t = r.text().await.unwrap_or_default();
            crate::app_logger::log_command("netdisk", &format!("[夸克] guest url={} raw={}", u.chars().take(240).collect::<String>(), t.chars().take(200).collect::<String>()));
            let v: serde_json::Value = match serde_json::from_str(&t) {
                Ok(x) => x,
                Err(_) => break,
            };
            for it in v.pointer("/data/list").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
                let fid = it.get("fid").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let is_dir = it.get("dir").and_then(|x| x.as_bool()).unwrap_or(false);
                let tk = it.get("share_fid_token").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if is_dir {
                    stack.push(fid);
                } else if !tk.is_empty() {
                    fresh.insert(fid, tk);
                }
            }
        }
    }
    let fresh_tokens: Vec<String> = fids
        .iter()
        .map(|f| fresh.get(f).cloned().unwrap_or_default())
        .collect();
    crate::app_logger::log_command(
        "netdisk",
        &format!("[夸克] 游客入口刷新 token: 找到 {}/{} 个", fresh_tokens.iter().filter(|t| !t.is_empty()).count(), fresh_tokens.len()),
    );
    let use_tokens: &[String] = if fresh_tokens.iter().all(|t| !t.is_empty()) {
        &fresh_tokens
    } else {
        tokens
    };

    // 2) 要直链
    let dl_url = format!(
        "{}/file/download?{}&sys=win32&ve={}",
        fam.guest_base,
        anti_bot_pr(fam, fam.guest_pr),
        fam.ve
    );
    let body = serde_json::json!({
        "fids": fids,
        "fids_token": use_tokens,
        "pwd_id": pwd_id,
        "stoken": g_stoken,
        "speedup_session": "",
        "token": "",
    });
    match post_download(fam, &dl_url, &body, "").await {
        Ok(r) if !r.items.is_empty() => {
            crate::app_logger::log_command("netdisk", "[夸克] 游客入口(ucpro_new)取到直链");
            Some(r)
        }
        Ok(_) => None,
        Err(e) => {
            crate::app_logger::log_command("netdisk", &format!("[夸克] 游客入口取直链失败: {}", e));
            None
        }
    }
}

/// POST /file/download 的返回：直链列表 + 本次响应下发的游客令牌 `__pugs`
struct DlResp {
    items: Vec<(String, String, u64)>,
    pugs: Option<String>,
}

/// ★★★ `__pugs` 就在**这一次 file/download 响应的 Set-Cookie 里**，而且**首次请求不能带
///   Cookie**（带了反而拿不到）。拿到的 `__pugs` 必须回带在真正 GET `dl-guest-*` 直链的
///   请求上 —— 缺了它 CDN 直接 412 Precondition Failed。
///   这个坑花了很久才定位：错误码看起来像"需要登录"，其实只是少带了这个游客令牌。
async fn post_download(
    fam: &Fam,
    url: &str,
    body: &serde_json::Value,
    cookie: &str,
) -> Result<DlResp, String> {
    let resp = req(fam, reqwest::Method::POST, url, cookie)
        .json(body)
        .send()
        .await
        .map_err(|e| friendly_err(fam.drive, "file/download", &e.to_string()))?;
    let pugs = pick_set_cookie(&resp)
        .into_iter()
        .find(|c| c.starts_with("__pugs="));
    let text = resp.text().await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| friendly_err(fam.drive, "file/download", &text))?;
    check(&v).map_err(|e| friendly_err(fam.drive, "file/download", &e))?;
    let arr = v
        .pointer("/data")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let items = arr
        .iter()
        .filter_map(|x| {
            let u = x.get("download_url").and_then(|y| y.as_str())?;
            Some((
                x.get("file_name").and_then(|y| y.as_str()).unwrap_or("").to_string(),
                u.to_string(),
                x.get("size").and_then(|y| y.as_u64()).unwrap_or(0),
            ))
        })
        .collect();
    Ok(DlResp { items, pugs })
}


/// 转存到自己的根目录，返回**新的 fid**（轮询转存任务直到完成）
async fn save_share(
    fam: &Fam,
    pwd_id: &str,
    stoken: &str,
    fids: &[String],
    tokens: &[String],
    cookie: &str,
) -> Result<Vec<String>, String> {
    if cookie.trim().is_empty() {
        return Err("夸克下载分享文件需要登录：请到「设置 → 网盘」粘贴你的夸克 Cookie".to_string());
    }
    let url = format!("{}/share/sharepage/save?{}", fam.base, anti_bot(fam));
    let body = serde_json::json!({
        "fid_list": fids,
        "fid_token_list": tokens,
        "to_pdir_fid": "0",
        "pwd_id": pwd_id,
        "stoken": stoken,
        "pdir_fid": "0",
        "scene": "link",
    });
    let resp = req(fam, reqwest::Method::POST, &url, cookie)
        .json(&body)
        .send()
        .await
        .map_err(|e| friendly_err(fam.drive, "sharepage/save", &e.to_string()))?;
    let text = resp.text().await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| friendly_err(fam.drive, "sharepage/save", &text))?;
    check(&v).map_err(|e| friendly_err(fam.drive, "sharepage/save", &e))?;

    let task_id = v
        .pointer("/data/task_id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if task_id.is_empty() {
        return Err(friendly_err(fam.drive, "sharepage/save", "返回里没有 task_id"));
    }

    // 轮询任务（转存是异步的）
    for i in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let u = format!("{}/task?{}&task_id={}&retry_index={}", fam.base, anti_bot(fam), task_id, i);
        let r = req(fam, reqwest::Method::GET, &u, cookie)
            .send()
            .await
            .map_err(|e| friendly_err(fam.drive, "task", &e.to_string()))?;
        let t = r.text().await.unwrap_or_default();
        let tv: serde_json::Value = match serde_json::from_str(&t) {
            Ok(x) => x,
            Err(_) => continue,
        };
        let status = tv.pointer("/data/status").and_then(|x| x.as_i64()).unwrap_or(0);
        let msg = tv.pointer("/message").and_then(|x| x.as_str()).unwrap_or("");
        if status == 2 && msg == "ok" {
            // 转存完成后，去根目录找刚存进去的文件（按名字匹配）
            return find_saved_fids(fam, fids.len(), cookie).await;
        }
        if status == 3 {
            return Err(friendly_err(fam.drive, "task", &format!("转存失败: {}", t)));
        }
    }
    Err("转存超时（夸克没在 24 秒内完成任务）".to_string())
}

/// 转存完不知道新 fid，去根目录按"最新更新"取前 N 个
async fn find_saved_fids(fam: &Fam, want: usize, cookie: &str) -> Result<Vec<String>, String> {
    let url = format!(
        "{}/file/sort?{}&pdir_fid=0&_page=1&_size={}&_fetch_total=1&_fetch_sub_dirs=0\
         &_sort=file_type:asc,updated_at:desc",
        fam.base,
        anti_bot(fam),
        want.max(1)
    );
    let resp = req(fam, reqwest::Method::GET, &url, cookie)
        .send()
        .await
        .map_err(|e| friendly_err(fam.drive, "file/sort", &e.to_string()))?;
    let text = resp.text().await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| friendly_err(fam.drive, "file/sort", &text))?;
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
fn merge(fam: &Fam, entries: &[PanEntry], list: Vec<(String, String, u64)>, cookie: &str) -> Vec<PanResolved> {
    let headers = vec![
        ("Cookie".to_string(), cookie.to_string()),
        ("Referer".to_string(), fam.referer.to_string()),
        ("User-Agent".to_string(), fam.ua.to_string()),
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
        let s = anti_bot(&QUARK);
        assert!(s.contains("pr=ucpro"));
        assert!(s.contains("fr=pc"));
        assert!(s.contains("__dt="));
        assert!(s.contains("__t="));
    }

    #[test]
    fn anti_bot_changes_every_call() {
        let a = anti_bot(&QUARK);
        std::thread::sleep(std::time::Duration::from_millis(3));
        let b = anti_bot(&QUARK);
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
        let out = merge(&QUARK, &entries, vec![("x.bin".into(), "https://cdn/x".into(), 5)], "");
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
