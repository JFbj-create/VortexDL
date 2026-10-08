//! 游戏站下载链接抓取（galgamex.net）。
//!
//! # 为什么非要用浏览器 + 网络钩子（2026-10-07 全流程实测）
//!
//! 这个站不能纯 HTTP 抓，也不能只抓 DOM：
//!
//! | 环节 | 实测结果 |
//! |---|---|
//! | 列表页 `/games` | 服务端渲染，能直接拿到 80 条 `/game/<slug>` 详情链接 |
//! | 详情页资源列表 | **点了「资源下载」标签**之后才由 server action 拉回来，HTML 里一个字都没有 |
//! | 「RAR 7.8 GB」按钮 | 是个**空 `<button>`**（无 href、无 data-url），链接只在 JS 内存里 |
//! | 真链接来源 | `POST /api/game/resource/{id}/download` → 返回**签名 S3 直链** |
//! | 该接口是否需要登录 | **不需要**（站点开了游客下载，`useAllowGuestDownload()`） |
//!
//! 所以流程是：浏览器点标签 → 网络钩子截下 server action 响应 → 解析出 resource id
//! → 纯 HTTP POST 拿签名直链 → 排除网盘 → 随机挑一条。
//!
//! 另外两个实测坑（都在 `browser.rs` 里修掉了，这里记一笔免得回退）：
//! - 点击必须用 CDP 真实鼠标事件：Radix 的 Tabs.Trigger 只监听 `mousedown`，
//!   `el.click()` 无效
//! - 必须显式设桌面视口：chromiumoxide 默认 800x600，标签栏会掉到折叠线以下，
//!   量出来的坐标在视口外，点击直接落空

use crate::browser::{self, BrowserSession, NetEntry};
use std::time::Duration;
use serde::Serialize;

/// 默认站点入口
pub const DEFAULT_LIST_URL: &str = "https://www.galgamex.net/games";

/// 一条抓到的资源（已把直链和网盘分开）
#[derive(Debug, Clone, Serialize)]
pub struct ScrapedResource {
    pub resource_id: u64,
    pub game_url: String,
    pub game_slug: String,
    pub size: Option<String>,
    pub version: Option<String>,
    pub unzip_code: Option<String>,
    /// 直链（已签名，可直接下载）
    pub direct_urls: Vec<String>,
    /// 网盘链接（按用户要求排除，仅作展示）
    pub pan_urls: Vec<String>,
}

/// 随机挑中的那一条（前端直接拿去下载）
#[derive(Debug, Clone, Serialize)]
pub struct PickedDownload {
    pub game_url: String,
    pub game_slug: String,
    pub resource_id: u64,
    pub url: String,
    pub size: Option<String>,
    pub unzip_code: Option<String>,
    /// 一共找到几条直链候选
    pub candidates: usize,
    /// 被排除掉的网盘链接数
    pub pan_skipped: usize,
}

/// 从 server action 响应体里解析资源条目。
///
/// 响应体形如（Next.js flight 格式，第一行是元信息，第二行是数组）：
/// ```text
/// 0:{"a":"$@1","f":"","q":"","i":false,"b":"..."}
/// 1:[{"id":9432,"name":null,"urls":["https://...rar","https://pan.baidu.com/..."],
///     "linkSizes":["7.8 GB"],"code":null,"unzipCode":null,"version":"AI汉化版",
///     "size":"7.8 GB","type":"galgame",...}]
/// ```
pub fn parse_resource_ids(body: &str) -> Vec<u64> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // 只认 `{"id":<数字>,"name":` 这种资源对象的开头，避免抓到别的 id
    let re = match regex::Regex::new(r#"\{"id":(\d{1,12}),"name":"#) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for cap in re.captures_iter(body) {
        if let Ok(id) = cap[1].parse::<u64>() {
            if seen.insert(id) {
                out.push(id);
            }
        }
    }
    out
}

/// 从 server action 响应体里取某条资源的展示字段（大小/版本/解压码）
pub fn parse_resource_meta(body: &str, id: u64) -> (Option<String>, Option<String>, Option<String>) {
    let pat = format!(r#"\{{"id":{id},"name":"#);
    let start = match regex::Regex::new(&pat).ok().and_then(|re| re.find(body).map(|m| m.start())) {
        Some(s) => s,
        None => return (None, None, None),
    };
    // ★ 右边界必须是**下一个资源对象的开头**，不能用固定窗口。
    //   实测踩过：9432 的 unzipCode 本该是 null，固定取 2000 字会读到
    //   下一条 9440 的 "unzipCode":"abc"。
    let end = regex::Regex::new(r#"\{"id":\d{1,12},"name":"#)
        .ok()
        .and_then(|re| re.find_at(body, start + 1).map(|m| m.start()))
        .unwrap_or(body.len());
    let seg = &body[start..end];
    let grab = |key: &str| -> Option<String> {
        let re = regex::Regex::new(&format!(r#""{key}":"([^"]*)""#)).ok()?;
        let c = re.captures(seg)?;
        let v = c[1].to_string();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    };
    (grab("size"), grab("version"), grab("unzipCode"))
}

/// 从网络记录里把「资源列表」那份响应体挑出来
fn find_resource_body(net: &[NetEntry]) -> Option<String> {
    // 优先看 server action 响应（URL 是 /game/<slug>，不带 /api/）
    for e in net {
        if let Some(b) = &e.body {
            if b.contains("\"linkSizes\"") || b.contains("\"unzipCode\"") {
                return Some(b.clone());
            }
        }
    }
    None
}

/// 调 `/api/game/resource/{id}/download` 拿签名直链（无需登录）
pub async fn fetch_download_urls(
    client: &reqwest::Client,
    site_origin: &str,
    id: u64,
    referer: &str,
) -> Result<Vec<String>, String> {
    let url = format!("{site_origin}/api/game/resource/{id}/download");
    let resp = client
        .post(&url)
        .header("Accept", "application/json")
        .header("Referer", referer)
        .send()
        .await
        .map_err(|e| format!("请求下载接口失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("下载接口返回 HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("下载接口响应不是 JSON: {e}"))?;
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        return Err(format!("下载接口报错: {err}"));
    }
    let urls = v
        .pointer("/data/resource/urls")
        .and_then(|u| u.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.replace("&amp;", "&"))
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();
    if urls.is_empty() {
        return Err("下载接口没返回任何链接".to_string());
    }
    Ok(urls)
}

/// 把一组链接拆成（直链, 网盘）
pub fn split_direct_pan(urls: &[String]) -> (Vec<String>, Vec<String>) {
    let mut direct = Vec::new();
    let mut pan = Vec::new();
    for u in urls {
        if browser::is_pan_link(u) {
            pan.push(u.clone());
        } else if browser::is_direct_archive(u) {
            direct.push(u.clone());
        }
    }
    (direct, pan)
}

/// 逐个探测候选直链，只保留**真的能下到**的（并发 6）。
///
/// ★ 为什么必须做这一步：站点自己的数据里有坏条目。
///   实测 resource 3802 的 `.apk`/`.zip` 在 CDN 上都是 **404**
///   （它的 `fileSizes` 是 `[null,null,null]`，说明从来没登记过大小），
///   只有网盘那条是活的。不做存活校验的话，"随机挑一条"有一半概率挑到死链，
///   用户看到的就是"下载失败"，而且完全不知道为什么。
///
/// 用 `Range: bytes=0-0` 只取 1 字节，代价很小（每个 ~200~500ms）。
pub async fn filter_alive(client: &reqwest::Client, urls: &[String]) -> Vec<String> {
    use futures::StreamExt;
    if urls.is_empty() {
        return Vec::new();
    }
    // ★ 先把 `(序号, String)` 收集成**拥有所有权**的 Vec 再进 stream。
    //   直接 `urls.iter().enumerate()` 会让 map 闭包借用 `&String`，
    //   于是 Tauri 命令的 Send 检查报 "implementation of FnOnce is not general enough"
    //   （HRTB 推不出来）。改成 owned String 后闭包不再借任何东西。
    let items: Vec<(usize, String)> = urls.iter().cloned().enumerate().collect();
    let results: Vec<(usize, String, bool)> = futures::stream::iter(items)
        .map(|(i, u)| {
            let c = client.clone();
            async move {
                let ok = c
                    .get(&u)
                    .header("Range", "bytes=0-0")
                    .header("Referer", "https://www.galgamex.net/")
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false);
                (i, u, ok)
            }
        })
        .buffer_unordered(6)
        .collect()
        .await;

    let mut v = results;
    v.sort_by_key(|(i, _, _)| *i); // 保持原顺序（随机挑选才有意义）
    v.into_iter()
        .filter(|(_, _, ok)| *ok)
        .map(|(_, u, _)| u)
        .collect()
}

/// 抓取站点上的资源（每个详情页点「资源下载」→ 解析 id → 拿签名直链）
///
/// - `max_games`：最多看几个详情页（每个页面 1~3 秒，别设太大）
/// - `log`：进度回调（写前端日志用）
pub async fn scrape_resources<F>(
    list_url: &str,
    max_games: usize,
    log: F,
) -> Result<Vec<ScrapedResource>, String>
where
    F: Fn(&str) + Send + Sync,
{
    let origin = origin_of(list_url)
        .ok_or_else(|| format!("列表页地址不合法: {list_url}"))?;

    // ============================================================
    // 阶段 1：浏览器取数据。
    //
    // ★ 浏览器会话**不跨到阶段 2**：BrowserSession 里的 Browser 不是 Sync，
    //   一旦让它和后面的并发 HTTP 探测活在同一个 future 里，Tauri 命令的
    //   `Send` 检查就会报 "implementation of Send is not general enough"。
    //   拆成两段后这里只留下纯数据 (String)，问题自然消失。
    // ============================================================
    log(&format!("启动浏览器抓取 {list_url}"));
    let session = BrowserSession::launch().await?;

    // 列表页 → 详情链接
    let list = session.fetch(list_url, None, 6000).await?;
    let mut details: Vec<String> = list
        .urls
        .iter()
        .filter(|u| browser::is_detail_link(u))
        .cloned()
        .collect();
    details.sort();
    details.dedup();
    log(&format!("列表页 {} 条链接, 详情页 {} 条", list.urls.len(), details.len()));
    if details.is_empty() {
        session.close().await;
        return Err("列表页没抓到详情链接（站点可能改版）".to_string());
    }

    // 逐页点「资源下载」，把 server action 响应体收下来
    let scanned = details.len().min(max_games.max(1));
    let mut pages: Vec<(String, String, String)> = Vec::new(); // (详情页, slug, 响应体)
    for (i, d) in details.iter().take(scanned).enumerate() {
        let r = match session.fetch(d, Some("资源下载"), 3500).await {
            Ok(r) => r,
            Err(e) => {
                log(&format!("[{}/{}] 跳过 {d}: {e}", i + 1, scanned));
                continue;
            }
        };
        let slug = d.rsplit('/').next().unwrap_or(d).to_string();
        match find_resource_body(&r.net) {
            Some(body) => {
                let n = parse_resource_ids(&body).len();
                log(&format!("[{}/{}] {slug}: 找到 {n} 条资源", i + 1, scanned));
                pages.push((d.clone(), slug, body));
            }
            None => log(&format!("[{}/{}] {slug}: 没截到资源列表响应", i + 1, scanned)),
        }
    }
    session.close().await;

    // ============================================================
    // 阶段 2：纯 HTTP 取签名直链 + 探活
    // ============================================================
    let client = crate::search_engine::build_client();
    let mut out: Vec<ScrapedResource> = Vec::new();

    for (d, slug, body) in &pages {
        for id in parse_resource_ids(body) {
            let urls = match fetch_download_urls(&client, &origin, id, d).await {
                Ok(u) => u,
                Err(e) => {
                    log(&format!("    resource {id}: {e}"));
                    continue;
                }
            };
            let (raw_direct, pan) = split_direct_pan(&urls);
            let (size, version, unzip) = parse_resource_meta(body, id);
            // ★ 站点数据有坏条目：先探活再收（见 filter_alive 的注释）
            let direct = filter_alive(&client, &raw_direct).await;
            let dead = raw_direct.len() - direct.len();
            log(&format!(
                "    resource {id}: 直链 {} 条 (死链 {dead} 条已剔除), 网盘 {} 条",
                direct.len(),
                pan.len()
            ));
            if !direct.is_empty() {
                out.push(ScrapedResource {
                    resource_id: id,
                    game_url: d.clone(),
                    game_slug: slug.clone(),
                    size,
                    version,
                    unzip_code: unzip,
                    direct_urls: direct,
                    pan_urls: pan,
                });
            }
        }
    }

    log(&format!("抓取完成: {} 条可用资源", out.len()));
    Ok(out)
}

/// 随机挑一条直链（排除网盘）。
///
/// ★ 用户要求：「排除所有网盘链接随机挑选一个下载」。
///   随机用系统时间纳秒做种子 —— 不引第三方 rng 依赖。
pub fn pick_random(resources: &[ScrapedResource]) -> Option<PickedDownload> {
    let mut all: Vec<(&ScrapedResource, &String)> = Vec::new();
    let mut pan_skipped = 0usize;
    for r in resources {
        pan_skipped += r.pan_urls.len();
        for u in &r.direct_urls {
            all.push((r, u));
        }
    }
    if all.is_empty() {
        return None;
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize ^ (d.as_secs() as usize))
        .unwrap_or(0);
    let (r, u) = all[seed % all.len()];
    Some(PickedDownload {
        game_url: r.game_url.clone(),
        game_slug: r.game_slug.clone(),
        resource_id: r.resource_id,
        url: u.clone(),
        size: r.size.clone(),
        unzip_code: r.unzip_code.clone(),
        candidates: all.len(),
        pan_skipped,
    })
}

/// 取 `scheme://host`
fn origin_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| format!("{}://{}", u.scheme(), h)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实的 server action 响应片段（2026-10-07 抓取时截获）
    const REAL_BODY: &str = r#"0:{"a":"$@1","f":"","q":"","i":false,"b":"3iumFTheK_gSeezwsjhT3"}
1:[{"id":9432,"name":null,"urls":["https://game.galgamex.com/PC%5BCIRCUS%5D.rar","https://pan.baidu.com/1XV8s_tB-aHlMPtAI7GtaMg"],"linkSizes":["7.8 GB"],"code":null,"unzipCode":null,"version":"AI汉化版","size":"7.8 GB","type":"galgame","platforms":["Windows"]},{"id":9440,"name":"补丁","urls":["https://game.galgamex.com/x.zip"],"linkSizes":["1 MB"],"code":null,"unzipCode":"abc","version":null,"size":"1 MB","type":"patch"}]"#;

    #[test]
    fn parses_resource_ids_from_flight_payload() {
        let ids = parse_resource_ids(REAL_BODY);
        assert_eq!(ids, vec![9432, 9440], "{ids:?}");
    }

    #[test]
    fn parses_resource_meta() {
        let (size, version, unzip) = parse_resource_meta(REAL_BODY, 9432);
        assert_eq!(size.as_deref(), Some("7.8 GB"));
        assert_eq!(version.as_deref(), Some("AI汉化版"));
        assert_eq!(unzip, None, "null 应该当没有, 不是字符串 \"null\"");

        let (_, _, unzip2) = parse_resource_meta(REAL_BODY, 9440);
        assert_eq!(unzip2.as_deref(), Some("abc"));
    }

    #[test]
    fn splits_direct_from_pan() {
        let urls = vec![
            "https://game.galgamex.com/7001-8000/Gamex-007449/a.rar?X-Amz-Signature=deadbeef".to_string(),
            "https://pan.baidu.com/s/1XV8s_tB?pwd=45dx".to_string(),
            "https://mega.nz/file/abc#key".to_string(),
            "https://game.galgamex.com/x.zip".to_string(),
        ];
        let (direct, pan) = split_direct_pan(&urls);
        assert_eq!(direct.len(), 2, "{direct:?}");
        assert_eq!(pan.len(), 2, "{pan:?}");
        assert!(pan.iter().all(|u| browser::is_pan_link(u)));
        assert!(direct.iter().all(|u| !browser::is_pan_link(u)));
    }

    #[test]
    fn pick_random_excludes_pan_and_reports_counts() {
        let rs = vec![ScrapedResource {
            resource_id: 1,
            game_url: "https://www.galgamex.net/game/abc".into(),
            game_slug: "abc".into(),
            size: Some("7.8 GB".into()),
            version: None,
            unzip_code: Some("galgamex.com".into()),
            direct_urls: vec![
                "https://game.galgamex.com/a.rar".into(),
                "https://game.galgamex.com/b.zip".into(),
            ],
            pan_urls: vec!["https://pan.baidu.com/s/1x".into()],
        }];
        let p = pick_random(&rs).expect("应该能挑出来");
        assert_eq!(p.candidates, 2);
        assert_eq!(p.pan_skipped, 1);
        assert!(p.url.starts_with("https://game.galgamex.com/"), "{}", p.url);
        assert_eq!(p.game_slug, "abc");
    }

    #[test]
    fn pick_random_returns_none_when_only_pan() {
        let rs = vec![ScrapedResource {
            resource_id: 1,
            game_url: "u".into(),
            game_slug: "s".into(),
            size: None,
            version: None,
            unzip_code: None,
            direct_urls: vec![],
            pan_urls: vec!["https://pan.baidu.com/s/1x".into()],
        }];
        assert!(pick_random(&rs).is_none());
    }

    #[test]
    fn origin_extraction() {
        assert_eq!(
            origin_of("https://www.galgamex.net/games#tr").as_deref(),
            Some("https://www.galgamex.net")
        );
        assert_eq!(origin_of("not a url"), None);
    }

    // ============================================================
    // 端到端联网实测（默认 #[ignore]）：
    //   cargo test --target x86_64-pc-windows-msvc --bin vortex-dl \
    //     game_scrape::tests::live_ -- --ignored --nocapture
    // ============================================================
    #[tokio::test]
    #[ignore]
    async fn live_scrape_and_pick() {
        let rs = scrape_resources(DEFAULT_LIST_URL, 10, |m| println!("  {m}"))
            .await
            .expect("抓取失败");
        println!("\n=== 抓取到 {} 条可用资源 ===", rs.len());
        for r in rs.iter().take(5) {
            println!(
                "  resource {} [{}] {} 直链{} 网盘{}",
                r.resource_id,
                r.game_slug,
                r.size.as_deref().unwrap_or("-"),
                r.direct_urls.len(),
                r.pan_urls.len()
            );
            for u in &r.direct_urls {
                println!("      直链 {}", u.chars().take(120).collect::<String>());
            }
            for u in &r.pan_urls {
                println!("      网盘 {}", u.chars().take(100).collect::<String>());
            }
        }
        assert!(!rs.is_empty(), "没抓到任何可用资源");

        let picked = pick_random(&rs).expect("随机挑失败");
        println!("\n★ 随机选中（已排除 {} 条网盘）:", picked.pan_skipped);
        println!("   {}", picked.url);
        println!("   候选总数 {}", picked.candidates);
        assert!(!browser::is_pan_link(&picked.url), "挑中的居然是网盘: {}", picked.url);

        // 验证这条链接真的能下：只读**第一块**就行。
        // ★ 别用 resp.bytes()：这些 CDN 会忽略 Range，直接吐 2GB 全量，读到超时
        //   （实测踩过：Range: bytes=0-511 仍然返回 content-length=2284050557）。
        let client = crate::search_engine::build_client();
        let resp = client
            .get(&picked.url)
            .header("Range", "bytes=0-511")
            .send()
            .await
            .expect("请求失败");
        println!(
            "   HTTP {} content-length={:?}",
            resp.status(),
            resp.headers().get("content-length")
        );
        assert!(
            resp.status().is_success(),
            "挑中的链接取不到: HTTP {}",
            resp.status()
        );
        use futures::StreamExt;
        let mut stream = resp.bytes_stream();
        let first = stream.next().await.expect("没有数据块").expect("读块失败");
        println!("   首块 {} 字节, 前 8 字节 {}", first.len(), hex8(&first));
        drop(stream);
    }

    fn hex8(b: &[u8]) -> String {
        b.iter().take(8).map(|x| format!("{x:02x}")).collect()
    }

    /// 诊断：/games#gal 到底显示多少 galgame
    #[tokio::test]
    #[ignore]
    async fn live_gal_tab_probe() {
        let s = BrowserSession::launch().await.expect("启动浏览器失败");
        let page = s.browser.new_page("https://www.galgamex.net/games#gal").await.expect("open");
        let _ = page.wait_for_navigation().await;
        tokio::time::sleep(Duration::from_millis(9000)).await;
        let _ = page.evaluate(browser::NET_HOOK_JS).await;
        tokio::time::sleep(Duration::from_millis(4000)).await;

        let js = r#"(function(){
            var out = {url: location.href, pages: [], titles: [], counts: []};
            var b = document.querySelectorAll('button');
            for (var i=0;i<b.length;i++){
                var t = (b[i].textContent||'').trim();
                if (t.indexOf(' / ') >= 0 && t.length < 20) out.pages.push(t);
            }
            var h3 = document.querySelectorAll('h3');
            for (var i=0;i<h3.length && i<6;i++) out.titles.push((h3[i].textContent||'').trim().slice(0,30));
            out.cardCount = document.querySelectorAll('a[href^="/game/"]').length;
            return out;
        })()"#;
        let v = page.evaluate(js).await.expect("eval");
        println!("
=== /games#gal 页面状态 ===");
        println!("{}", serde_json::to_string_pretty(v.value().unwrap_or(&serde_json::Value::Null)).unwrap());

        let net = BrowserSession::read_net(&page).await;
        println!("
=== 网络请求（含 resourceType 的）===");
        for e in &net {
            if let Some(rq) = &e.req {
                if rq.contains("resourceType") {
                    if let Some(k) = rq.find("BODY:") {
                        println!("  {} :: {}", e.url, &rq[k..]);
                    }
                }
            }
        }
        let _ = page.close().await;
        s.close().await;
    }

    /// 诊断：galgamex 列表页的分页协议（纯客户端 server action）。
    /// 点一下「下一页」，把请求/响应都打出来，好改成纯 HTTP。
    #[tokio::test]
    #[ignore]
    async fn live_pagination_probe() {
        let s = BrowserSession::launch().await.expect("启动浏览器失败");
        let page = s.browser.new_page("https://www.galgamex.net/games").await.expect("open");
        let _ = page.wait_for_navigation().await;
        tokio::time::sleep(Duration::from_millis(6000)).await;
        let _ = page.evaluate(browser::NET_HOOK_JS).await;

        // 点 aria-label="下一页"
        let js = r#"(function(){
            var el = document.querySelector('[aria-label="下一页"]');
            if (!el) return null;
            el.scrollIntoView({block:'center'});
            var r = el.getBoundingClientRect();
            return {x: r.left + r.width/2, y: r.top + r.height/2};
        })()"#;
        let v = page.evaluate(js).await.expect("eval");
        match v.value().cloned() {
            Some(c) => {
                let x = c.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let y = c.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
                println!("
点下一页 @ ({x},{y})");
                let _ = page.click(chromiumoxide::layout::Point::new(x, y)).await;
            }
            None => println!("
没找到「下一页」按钮"),
        }
        tokio::time::sleep(Duration::from_millis(5000)).await;

        let net = BrowserSession::read_net(&page).await;
        // ★ 把 /games 的 server action 响应落盘，好离线看 schema
        for e in &net {
            if e.url.contains("/games") {
                if let Some(b) = &e.body {
                    if b.contains("\"games\"") {
                        let _ = std::fs::write(r"F:/vdgame/_games_resp.txt", b);
                        println!("
[落盘] /games 响应 {} 字节 -> _games_resp.txt", b.len());
                        break;
                    }
                }
            }
        }
        // ★ 再点一下「Galgame」标签，抓它的真实过滤参数
        let js2 = r#"(function(){
            var all = document.querySelectorAll('a, button, [role=tab]');
            for (var i = 0; i < all.length; i++) {
                var t = (all[i].textContent || '').trim();
                if (t === 'Galgame') {
                    all[i].scrollIntoView({block:'center'});
                    var r = all[i].getBoundingClientRect();
                    return {x: r.left + r.width/2, y: r.top + r.height/2};
                }
            }
            return null;
        })()"#;
        if let Ok(v2) = page.evaluate(js2).await {
            if let Some(c) = v2.value() {
                let x = c.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let y = c.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
                println!("
点 Galgame 标签 @ ({x},{y})");
                let _ = page.click(chromiumoxide::layout::Point::new(x, y)).await;
                tokio::time::sleep(Duration::from_millis(5000)).await;
                let net2 = BrowserSession::read_net(&page).await;
                for e in &net2 {
                    if e.url.ends_with("/games") {
                        if let Some(rq) = &e.req {
                            if let Some(i) = rq.find("BODY:") {
                                println!("  [GAL 完整请求体] {}", &rq[i..]);
                            }
                        }
                    }
                }
                println!("  点击后 URL: {}", page.url().await.ok().flatten().unwrap_or_default());
            }
        }

        println!("
=== /games 的请求详情 ===");
        for e in &net {
            if e.url.ends_with("/games") {
                println!("  url  = {}", e.url);
                println!("  req  = {}", e.req.clone().unwrap_or_default());
                println!("  body = {} 字节", e.body.as_ref().map(|b| b.len()).unwrap_or(0));
            }
        }
        let html = page.content().await.unwrap_or_default();
        let n = html.matches("/game/").count();
        println!("
点击后页面 URL: {}", page.url().await.ok().flatten().unwrap_or_default());
        println!("点击后 /game/ 出现次数: {n}");
        let _ = page.close().await;
        s.close().await;
    }

    /// 诊断：签名 URL 在引擎里被 403，但在 python 里是 200。
    /// 这里用**和引擎同一个 client** 发请求，并把 reqwest 实际用的 URL 打出来。
    #[tokio::test]
    #[ignore]
    async fn live_probe_signed_url() {
        let client = crate::search_engine::build_client();
        let urls = fetch_download_urls(
            &client,
            "https://www.galgamex.net",
            7411,
            "https://www.galgamex.net/game/1268dd1e",
        )
        .await
        .expect("拿链接失败");
        let u = urls
            .iter()
            .find(|u| u.contains("X-Amz-Signature"))
            .expect("没有签名链接")
            .clone();
        println!("\n原始 url      = {}", u.chars().take(160).collect::<String>());

        let resp = client
            .get(&u)
            .header("Range", "bytes=0-0")
            .send()
            .await
            .expect("请求失败");
        println!("status        = {}", resp.status());
        println!(
            "reqwest 实际  = {}",
            resp.url().as_str().chars().take(160).collect::<String>()
        );
        println!("content-range = {:?}", resp.headers().get("content-range"));
        println!("content-length= {:?}", resp.headers().get("content-length"));

        // 对照：直接拼一个只含 %23 的最小请求, 看 url crate 是否改写了它
        let parsed = url::Url::parse(&u).expect("parse");
        println!(
            "\nurl::Url 解析  = {}",
            parsed.as_str().chars().take(160).collect::<String>()
        );
        println!("path()        = {}", parsed.path());

        // ★ 引擎用的 client 是**自己造的**(强制 HTTP/1.1), 不是 build_client()。
        //   这里完全照抄它的关键配置复现一次。
        let engine_client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(std::time::Duration::from_secs(180))
            .timeout(std::time::Duration::from_secs(300))
            .tcp_nodelay(true)
            .http1_only()
            .pool_max_idle_per_host(64)
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .unwrap();
        let r2 = engine_client
            .get(&u)
            .header("Range", "bytes=0-0")
            .send()
            .await
            .expect("engine client 请求失败");
        println!("\n[引擎同款 client] status = {}", r2.status());
        println!("[引擎同款 client] 实际 url = {}", r2.url().as_str().chars().take(160).collect::<String>());
        println!("[引擎同款 client] content-range = {:?}", r2.headers().get("content-range"));
        if let Ok(b) = r2.text().await {
            println!("[引擎同款 client] body 前 300 字: {}", b.chars().take(300).collect::<String>());
        }
    }
}
