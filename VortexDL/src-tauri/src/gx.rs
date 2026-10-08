//! galgamex 游戏库（GX）模块。
//!
//! # 全流程纯 HTTP —— 不需要 chromiumoxide
//!
//! 2026-10-07 把站点协议完整摸清后确认：列表、分类、标签、详情、资源列表、
//! 签名直链**全部可以用普通 HTTP 拿到**（见 `_gx_protocol.md`）。
//! 站点没有对这几个接口做反爬，只是数据藏在 Next.js server action 里而已。
//! 所以这里不用浏览器：一次全量同步 7052 个游戏只要 **6 秒**，
//! 而用浏览器点分页要几分钟且容易被打断。
//!
//! （早先 `game_scrape.rs` 里的浏览器方案保留着，作为接口失效时的兜底思路。）
//!
//! # 协议要点（细节见 `_gx_protocol.md`）
//!
//! 1. `POST /api/content-filter {"value":"all"}` → cookie `content_filter=all`
//!    （不设这个 cookie 只有 SFW 数据，同人 188 vs 4793）
//! 2. 列表：`POST /games` + `next-action: <getGames id>` + body `[{limit:6000,...}]`
//!    —— **limit 能开到 6000，一次拉全量**
//! 3. 资源：`POST /game/<slug>` + `next-action: <getGameResources id>` + body `[gameId]`
//! 4. 签名直链：`POST /api/game/resource/<resourceId>/download`（无需登录）
//! 5. action id 会随站点重新部署变化 → 从页面 chunk 里正则动态发现

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

pub const ORIGIN: &str = "https://www.galgamex.net";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36";
/// 站点右上角齿轮「全部」对应的 cookie 值
const CONTENT_COOKIE: &str = "content_filter=all";

// ============================================================
// 数据模型
// ============================================================

/// 索引里的一条游戏卡片（只留前端要用的字段，压体积）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GxCard {
    pub id: u64,
    /// 详情页用的 8 位 slug
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub short_desc: String,
    #[serde(default)]
    pub cover: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub size: String,
    #[serde(default)]
    pub version: String,
    /// 年龄分级：true = R18
    pub nsfw: bool,
    /// 标签名（不含 id）
    #[serde(default)]
    pub tags: Vec<String>,
    /// "doujin" | "galgame"
    pub kind: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub view_count: u64,
    #[serde(default)]
    pub download_count: u64,
    #[serde(default)]
    pub screenshots: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GxTag {
    pub id: u64,
    pub name: String,
    pub count: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GxIndex {
    pub cards: Vec<GxCard>,
    pub tags: Vec<GxTag>,
    pub synced_at: i64,
    /// 发现到的 server action id（存下来，下次直接用；失败再重新发现）
    #[serde(default)]
    pub action_games: String,
    #[serde(default)]
    pub action_resources: String,
}

static INDEX: OnceLock<Mutex<GxIndex>> = OnceLock::new();

fn index_lock() -> &'static Mutex<GxIndex> {
    INDEX.get_or_init(|| Mutex::new(GxIndex::default()))
}

/// 站点返回的简介/名称里带 HTML 实体（`&quot;` `&#39;` `&amp;` …），
/// 不解码前端就会原样显示成 `&quot;Clappy Cheeks&quot;`。
pub fn unescape_html(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'&' {
            if let Some(semi) = s[i..].find(';') {
                if semi <= 12 {
                    let ent = &s[i + 1..i + semi];
                    let rep = match ent {
                        "quot" => Some("\"".to_string()),
                        "apos" | "#39" => Some("'".to_string()),
                        "amp" => Some("&".to_string()),
                        "lt" => Some("<".to_string()),
                        "gt" => Some(">".to_string()),
                        "nbsp" => Some(" ".to_string()),
                        _ => {
                            let num = ent.strip_prefix('#').map(|n| {
                                if let Some(hex) = n.strip_prefix('x').or_else(|| n.strip_prefix('X')) {
                                    u32::from_str_radix(hex, 16).ok()
                                } else {
                                    n.parse::<u32>().ok()
                                }
                            });
                            match num {
                                Some(Some(cp)) => char::from_u32(cp).map(|c| c.to_string()),
                                _ => None,
                            }
                        }
                    };
                    if let Some(r) = rep {
                        out.push_str(&r);
                        i += semi + 1;
                        continue;
                    }
                }
            }
        }
        // 逐字符推进（多字节 UTF-8 不能按字节切）
        let ch_len = if b[i] < 0x80 { 1 } else if b[i] < 0xE0 { 2 } else if b[i] < 0xF0 { 3 } else { 4 };
        out.push_str(&s[i..(i + ch_len).min(s.len())]);
        i += ch_len;
    }
    out
}

fn index_path() -> std::path::PathBuf {
    crate::paths::data_dir().join("gx").join("index.json")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 启动时把磁盘索引读进内存（没有就空着）
pub fn load_index_from_disk() {
    let p = index_path();
    let Ok(txt) = std::fs::read_to_string(&p) else {
        return;
    };
    match serde_json::from_str::<GxIndex>(&txt) {
        Ok(idx) => {
            crate::app_logger::log_command(
                "gx_load_index",
                &format!("从磁盘读入 {} 个游戏, {} 个标签", idx.cards.len(), idx.tags.len()),
            );
            if let Ok(mut g) = index_lock().lock() {
                *g = idx;
            }
        }
        Err(e) => crate::app_logger::log_command("gx_load_index", &format!("索引解析失败: {e}")),
    }
}

fn save_index_to_disk(idx: &GxIndex) {
    let p = index_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match serde_json::to_string(idx) {
        Ok(txt) => {
            if let Err(e) = std::fs::write(&p, txt) {
                crate::app_logger::log_command("gx_save_index", &format!("写盘失败: {e}"));
            }
        }
        Err(e) => crate::app_logger::log_command("gx_save_index", &format!("序列化失败: {e}")),
    }
}

// ============================================================
// HTTP 客户端
// ============================================================

/// 专给这个站用的客户端。
/// ★ 不能用 `search_engine::build_client()`：它带 `Sec-Fetch-*` 和 HTML 的 Accept，
///   而 server action 要求 `Accept: text/x-component` + `Content-Type: text/plain`。
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(UA)
        .gzip(true)
        .brotli(true)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .pool_max_idle_per_host(8)
        .build()
        .unwrap_or_default()
}

/// 把站点偏好切成「全部」（含 R18）。
///
/// ★ 不做这一步只会拿到 SFW 数据：同人 188 / Galgame 1399，
///   做了才是 4793 / 2259。用户明确要求「全部爬取」。
async fn set_content_filter_all(c: &reqwest::Client) -> Result<(), String> {
    let r = c
        .post(format!("{ORIGIN}/api/content-filter"))
        .header("Content-Type", "application/json")
        .header("Referer", format!("{ORIGIN}/games"))
        .body(r#"{"value":"all"}"#)
        .send()
        .await
        .map_err(|e| format!("设置内容过滤失败: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("设置内容过滤 HTTP {}", r.status()));
    }
    Ok(())
}

/// 从一段文本里把第一个完整 JSON 对象抠出来（括号配平，跳过字符串内的括号）
fn extract_json_object(s: &str, from: usize) -> Option<String> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    let mut started = false;
    for i in from..bytes.len() {
        let ch = bytes[i] as char;
        if in_str {
            if esc {
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' => {
                depth += 1;
                started = true;
            }
            '}' => {
                depth -= 1;
                if started && depth == 0 {
                    return Some(s[from..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// 从 flight 响应里取 `{"games":[...],"total":N}` 那个对象
fn parse_games_payload(resp: &str) -> Result<serde_json::Value, String> {
    let at = resp
        .find("{\"games\":[")
        .ok_or_else(|| "响应里没有 games 数组（action id 可能已失效）".to_string())?;
    let js = extract_json_object(resp, at).ok_or_else(|| "games JSON 不完整".to_string())?;
    serde_json::from_str(&js).map_err(|e| format!("games JSON 解析失败: {e}"))
}

/// 从 flight 响应里取 `1:[{...资源...}]` 那个数组
fn parse_array_payload(resp: &str) -> Result<serde_json::Value, String> {
    let at = resp
        .find("1:[")
        .or_else(|| resp.find(":["))
        .ok_or_else(|| "响应里没有数组（action id 可能已失效）".to_string())?;
    let start = resp[at..].find('[').map(|i| at + i).ok_or("找不到数组起点")?;
    // 数组也可能嵌在对象里，直接用 serde 的流式解析器扫第一个完整数组
    let mut de = serde_json::Deserializer::from_str(&resp[start..]);
    let v = serde_json::Value::deserialize(&mut de).map_err(|e| format!("数组解析失败: {e}"))?;
    Ok(v)
}

// ============================================================
// action id 发现
// ============================================================

/// 从页面 HTML 里取 chunk 列表
fn chunk_urls(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let Ok(re) = regex::Regex::new(r#"src="(/_next/static/chunks/[^"]+\.js)""#) else {
        return out;
    };
    for cap in re.captures_iter(html) {
        let u = format!("{ORIGIN}{}", &cap[1]);
        if seen.insert(u.clone()) {
            out.push(u);
        }
    }
    out
}

/// 在 chunk 文本里找 `createServerReference("id",...,"name")`
fn scan_actions(js: &str, found: &mut std::collections::HashMap<String, String>) {
    let Ok(re) = regex::Regex::new(r#"createServerReference\)\("([0-9a-f]{40,42})"[^)]*?,([^)]*?)\)"#)
    else {
        return;
    };
    let Ok(nre) = regex::Regex::new(r#""([A-Za-z][A-Za-z0-9_]*)""#) else {
        return;
    };
    for cap in re.captures_iter(js) {
        let id = cap[1].to_string();
        for n in nre.captures_iter(&cap[2]) {
            found.entry(n[1].to_string()).or_insert_with(|| id.clone());
        }
    }
}

/// 扫一个页面的所有 chunk，把 action id 找出来。
///
/// 两页的 chunk 列表不同（`getGames` 在游戏库页、`getGameResources` 在详情页），
/// 所以 `want` 没凑齐就要再扫另一页。
async fn discover_from_page(
    c: &reqwest::Client,
    page_url: &str,
    want: &[&str],
    found: &mut std::collections::HashMap<String, String>,
) -> Result<(), String> {
    let html = c
        .get(page_url)
        .header("Accept", "text/html,application/xhtml+xml,*/*;q=0.8")
        .header("Cookie", CONTENT_COOKIE)
        .send()
        .await
        .map_err(|e| format!("取页面失败: {e}"))?
        .text()
        .await
        .map_err(|e| format!("读页面失败: {e}"))?;

    let urls = chunk_urls(&html);
    if urls.is_empty() {
        return Err("页面里没有 chunk 链接（站点可能改版）".to_string());
    }

    // 并发抓 chunk，凑齐 want 就提前停
    use futures::StreamExt;
    let mut stream = futures::stream::iter(urls)
        .map(|u| {
            let c = c.clone();
            async move { c.get(&u).send().await.ok()?.text().await.ok() }
        })
        .buffer_unordered(10);

    while let Some(txt) = stream.next().await {
        if let Some(js) = txt {
            scan_actions(&js, found);
            if want.iter().all(|w| found.contains_key(*w)) {
                break;
            }
        }
    }
    Ok(())
}

/// 发现两个 action id（先试缓存，失败再扫）
async fn resolve_actions(
    c: &reqwest::Client,
    cached: &GxIndex,
    log: &(dyn Fn(&str) + Send + Sync),
) -> Result<(String, String), String> {
    if !cached.action_games.is_empty() && !cached.action_resources.is_empty() {
        // 先验证缓存还能用（用一次极小的列表请求）
        if probe_games_action(c, &cached.action_games).await {
            log("action id 缓存可用");
            return Ok((cached.action_games.clone(), cached.action_resources.clone()));
        }
        log("action id 缓存失效，重新发现");
    }

    let mut found = std::collections::HashMap::new();
    let want = ["getGames", "getGameResources"];
    discover_from_page(c, &format!("{ORIGIN}/games"), &want, &mut found).await?;
    if !found.contains_key("getGameResources") {
        // 详情页 chunk 里才有
        discover_from_page(c, &format!("{ORIGIN}/game/013baqnj"), &want, &mut found).await?;
    }

    let g = found.get("getGames").cloned();
    let r = found.get("getGameResources").cloned();
    match (g, r) {
        (Some(g), Some(r)) => {
            log(&format!("发现 action: getGames={} getGameResources={}", &g[..12], &r[..12]));
            Ok((g, r))
        }
        (g, r) => Err(format!(
            "没能发现 server action（getGames={:?} getGameResources={:?}）—— 站点可能改版",
            g.map(|x| x[..12].to_string()),
            r.map(|x| x[..12].to_string())
        )),
    }
}

/// 用一个极小的请求验证 action id 是否还有效
async fn probe_games_action(c: &reqwest::Client, action: &str) -> bool {
    let body = r#"[{"limit":5,"pageIndex":1,"resourceType":"doujin","onlyWithResources":true,"onlyWithCover":false,"onlyWithHeader":true,"sortBy":"resource_updated","keyword":"$undefined","tagSlug":"$undefined","tagSlugs":"$undefined","excludeTagSlugs":"$undefined","seriesSlug":"$undefined","developerSlug":"$undefined","year":"$undefined","platform":"$undefined","language":"$undefined"}]"#;
    match call_action(c, &format!("{ORIGIN}/games"), action, body).await {
        Ok(t) => t.contains("\"games\":["),
        Err(_) => false,
    }
}

/// 调一次 server action
async fn call_action(
    c: &reqwest::Client,
    url: &str,
    action: &str,
    body: &str,
) -> Result<String, String> {
    let r = c
        .post(url)
        .header("Accept", "text/x-component")
        .header("Content-Type", "text/plain;charset=UTF-8")
        .header("next-action", action)
        .header("Origin", ORIGIN)
        .header("Referer", url)
        .header("Cookie", CONTENT_COOKIE)
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| format!("action 请求失败: {e}"))?;
    let st = r.status();
    let t = r.text().await.map_err(|e| format!("action 读响应失败: {e}"))?;
    if !st.is_success() {
        return Err(format!("action HTTP {st}: {}", t.chars().take(200).collect::<String>()));
    }
    Ok(t)
}

// ============================================================
// 同步索引
// ============================================================

/// 一个分类的抓取参数 —— 两个区块**参数不一样**，照抄站点才能拿到同样的数量
struct SectionSpec {
    kind: &'static str,
    resource_type: &'static str,
    only_with_cover: bool,
    only_with_header: bool,
}

const SECTIONS: &[SectionSpec] = &[
    // 同人游戏：站点 `<div id="tr">` 的配置（ALL 模式 4793）
    SectionSpec { kind: "doujin", resource_type: "doujin", only_with_cover: false, only_with_header: true },
    // Galgame：站点 `<div id="gal">` 的配置（ALL 模式 2259）
    SectionSpec { kind: "galgame", resource_type: "galgame", only_with_cover: true, only_with_header: false },
];

fn games_body(spec: &SectionSpec, limit: u32) -> String {
    format!(
        r#"[{{"limit":{limit},"pageIndex":1,"resourceType":"{}","onlyWithResources":true,"onlyWithCover":{},"onlyWithHeader":{},"sortBy":"resource_updated","keyword":"$undefined","tagSlug":"$undefined","tagSlugs":"$undefined","excludeTagSlugs":"$undefined","seriesSlug":"$undefined","developerSlug":"$undefined","year":"$undefined","platform":"$undefined","language":"$undefined"}}]"#,
        spec.resource_type, spec.only_with_cover, spec.only_with_header
    )
}

/// 把站点返回的原始游戏对象转成紧凑卡片
fn card_from_json(v: &serde_json::Value, kind: &str) -> Option<GxCard> {
    let id = v.get("id").and_then(|x| x.as_u64())?;
    let slug = v.get("uniqueId").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if slug.is_empty() {
        return None;
    }
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let tags = v
        .get("tags")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let screenshots = v
        .get("media")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let t = m.get("mediaType").and_then(|x| x.as_str()).unwrap_or("");
                    if t == "screenshot" {
                        m.get("urlFull").and_then(|x| x.as_str()).map(|s| s.to_string())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let res0 = v.get("resources").and_then(|x| x.as_array()).and_then(|a| a.first());
    let (size, version) = match res0 {
        Some(r) => (
            r.get("size").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            r.get("version").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        ),
        None => (String::new(), String::new()),
    };
    // 封面优先 coverImage，退回 headerImage
    let mut cover = s("coverImage");
    if cover.is_empty() {
        cover = s("headerImage");
    }
    Some(GxCard {
        id,
        slug,
        name: unescape_html(&s("name")),
        short_desc: unescape_html(&s("shortDesc")),
        cover,
        header: s("headerImage"),
        size,
        version,
        nsfw: v.get("isNsfw").and_then(|x| x.as_bool()).unwrap_or(false),
        tags,
        kind: kind.to_string(),
        updated_at: s("resourceUpdatedAt"),
        view_count: v.get("viewCount").and_then(|x| x.as_u64()).unwrap_or(0),
        download_count: v
            .pointer("/stats/downloadCount")
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        screenshots,
    })
}

/// 抓标签表（`/game-tags`）
async fn fetch_tags(c: &reqwest::Client) -> Vec<GxTag> {
    let mut out = Vec::new();
    let Ok(html) = c
        .get(format!("{ORIGIN}/game-tags"))
        .header("Accept", "text/html,*/*;q=0.8")
        .header("Cookie", CONTENT_COOKIE)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map(|r| r)
    else {
        return out;
    };
    let Ok(html) = html.text().await else {
        return out;
    };
    // <a ... href="/game-tag/3"><span>2D</span><span ...>6.0k</span></a>
    let Ok(re) = regex::Regex::new(
        r#"href="/game-tag/(\d+)"[^>]*>\s*<span[^>]*>([^<]+)</span>\s*<span[^>]*>([^<]*)</span>"#,
    ) else {
        return out;
    };
    for cap in re.captures_iter(&html) {
        let id: u64 = cap[1].parse().unwrap_or(0);
        let name = cap[2].trim().to_string();
        if name.is_empty() {
            continue;
        }
        // "6.0k" / "1.2w" / "123" 都能解析
        let raw = cap[3].trim().to_lowercase();
        let count = if let Some(x) = raw.strip_suffix('k') {
            (x.parse::<f64>().unwrap_or(0.0) * 1000.0) as u64
        } else if let Some(x) = raw.strip_suffix('w') {
            (x.parse::<f64>().unwrap_or(0.0) * 10000.0) as u64
        } else {
            raw.parse().unwrap_or(0)
        };
        out.push(GxTag { id, name, count });
    }
    out.sort_by(|a, b| b.count.cmp(&a.count));
    out
}

/// 全量同步（用户在界面上点「同步」时调用）
pub async fn sync(log: &(dyn Fn(&str) + Send + Sync)) -> Result<GxIndex, String> {
    let c = client();
    log("设置内容过滤为「全部」…");
    set_content_filter_all(&c).await?;

    let cached = index_lock().lock().map(|g| g.clone()).unwrap_or_default();
    let (action_games, action_resources) = resolve_actions(&c, &cached, log).await?;

    let mut cards: Vec<GxCard> = Vec::new();
    for spec in SECTIONS {
        log(&format!("抓取 {} 列表…", spec.kind));
        let body = games_body(spec, 6000);
        let txt = call_action(&c, &format!("{ORIGIN}/games"), &action_games, &body).await?;
        let v = parse_games_payload(&txt)?;
        let arr = v.get("games").and_then(|x| x.as_array()).cloned().unwrap_or_default();
        let total = v.get("total").and_then(|x| x.as_u64()).unwrap_or(0);
        let mut n = 0usize;
        for g in &arr {
            if let Some(cd) = card_from_json(g, spec.kind) {
                cards.push(cd);
                n += 1;
            }
        }
        log(&format!("  {} : 拿到 {n} 条（站点 total={total}）", spec.kind));
    }

    // 去重（两个分类可能有重叠）
    let mut seen = HashSet::new();
    cards.retain(|c| seen.insert(c.id));

    log("抓取标签表…");
    let tags = fetch_tags(&c).await;
    log(&format!("  标签 {} 个", tags.len()));

    let idx = GxIndex {
        cards,
        tags,
        synced_at: now_ms(),
        action_games,
        action_resources,
    };
    if let Ok(mut g) = index_lock().lock() {
        *g = idx.clone();
    }
    save_index_to_disk(&idx);
    log(&format!("同步完成：{} 个游戏", idx.cards.len()));
    Ok(idx)
}

// ============================================================
// 浏览 / 详情 / 资源
// ============================================================

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GxQuery {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub keyword: String,
    /// "all" | "sfw" | "r18"
    #[serde(default)]
    pub nsfw: String,
    /// "updated" | "views" | "downloads" | "name"
    #[serde(default)]
    pub sort: String,
    #[serde(default)]
    pub page: u32,
    #[serde(default)]
    pub page_size: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct GxPage {
    pub items: Vec<GxCard>,
    pub total: usize,
    pub page: u32,
    pub page_size: u32,
}

pub fn browse(q: &GxQuery) -> GxPage {
    let g = index_lock().lock().map(|x| x.clone()).unwrap_or_default();
    let kw = q.keyword.trim().to_lowercase();
    let mut items: Vec<GxCard> = g
        .cards
        .iter()
        .filter(|c| {
            if !q.kind.is_empty() && q.kind != "all" && c.kind != q.kind {
                return false;
            }
            if !q.tag.is_empty() && !c.tags.iter().any(|t| t == &q.tag) {
                return false;
            }
            match q.nsfw.as_str() {
                "sfw" => {
                    if c.nsfw {
                        return false;
                    }
                }
                "r18" => {
                    if !c.nsfw {
                        return false;
                    }
                }
                _ => {}
            }
            if !kw.is_empty() {
                let hit = c.name.to_lowercase().contains(&kw)
                    || c.short_desc.to_lowercase().contains(&kw)
                    || c.tags.iter().any(|t| t.to_lowercase().contains(&kw));
                if !hit {
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect();

    match q.sort.as_str() {
        "views" => items.sort_by(|a, b| b.view_count.cmp(&a.view_count)),
        "downloads" => items.sort_by(|a, b| b.download_count.cmp(&a.download_count)),
        "name" => items.sort_by(|a, b| a.name.cmp(&b.name)),
        // 默认按资源更新时间倒序（站点默认）
        _ => items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at)),
    }

    let total = items.len();
    let page_size = if q.page_size == 0 { 60 } else { q.page_size.min(200) };
    let page = if q.page == 0 { 1 } else { q.page };
    let start = ((page - 1) as usize) * (page_size as usize);
    let items = if start >= total {
        Vec::new()
    } else {
        items.into_iter().skip(start).take(page_size as usize).collect()
    };
    GxPage { items, total, page, page_size }
}

pub fn index_info() -> (usize, usize, usize, i64) {
    let g = index_lock().lock().map(|x| x.clone()).unwrap_or_default();
    let doujin = g.cards.iter().filter(|c| c.kind == "doujin").count();
    let gal = g.cards.iter().filter(|c| c.kind == "galgame").count();
    (g.cards.len(), doujin, gal, g.synced_at)
}

pub fn all_tags() -> Vec<GxTag> {
    index_lock().lock().map(|g| g.tags.clone()).unwrap_or_default()
}

/// 游戏详情：简介/开发商/发售日等从详情页的 JSON-LD 里取
#[derive(Debug, Clone, Default, Serialize)]
pub struct GxDetail {
    pub card: GxCard,
    pub description: String,
    pub developer: String,
    pub release_date: String,
    pub genres: Vec<String>,
}

pub async fn detail(slug: &str) -> Result<GxDetail, String> {
    let card = index_lock()
        .lock()
        .map(|g| g.cards.iter().find(|c| c.slug == slug).cloned())
        .unwrap_or(None)
        .unwrap_or_default();
    if card.slug.is_empty() {
        return Err(format!("索引里没有 {slug}（先同步一次）"));
    }

    let c = client();
    let html = c
        .get(format!("{ORIGIN}/game/{slug}"))
        .header("Accept", "text/html,application/xhtml+xml,*/*;q=0.8")
        .header("Cookie", CONTENT_COOKIE)
        .send()
        .await
        .map_err(|e| format!("取详情页失败: {e}"))?
        .text()
        .await
        .map_err(|e| format!("读详情页失败: {e}"))?;

    let mut d = GxDetail { card, ..Default::default() };
    // 详情页里有 JSON-LD（schema.org VideoGame），name/description/genre/author 都在里面
    let Ok(re) = regex::Regex::new(r#"<script type="application/ld\+json">(.*?)</script>"#) else {
        return Ok(d);
    };
    for cap in re.captures_iter(&html) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&cap[1]) else {
            continue;
        };
        let nodes: Vec<&serde_json::Value> = match v.get("@graph").and_then(|x| x.as_array()) {
            Some(a) => a.iter().collect(),
            None => vec![&v],
        };
        for n in nodes {
            let ty = n.get("@type").and_then(|x| x.as_str()).unwrap_or("");
            if ty != "VideoGame" {
                continue;
            }
            if let Some(x) = n.get("description").and_then(|x| x.as_str()) {
                d.description = unescape_html(x);
            }
            if let Some(x) = n.get("datePublished").and_then(|x| x.as_str()) {
                d.release_date = x.to_string();
            }
            if let Some(a) = n.get("author").and_then(|x| x.as_array()) {
                d.developer = a
                    .iter()
                    .filter_map(|o| o.get("name").and_then(|x| x.as_str()))
                    .collect::<Vec<_>>()
                    .join(" / ");
            }
            if let Some(g) = n.get("genre").and_then(|x| x.as_array()) {
                d.genres = g
                    .iter()
                    .filter_map(|x| x.as_str())
                    .map(|s| s.to_string())
                    .collect();
            }
        }
    }
    // ★ 兜底：详情页的 JSON-LD 里**没有 VideoGame 节点**（只有 Organization/WebSite），
    //   所以上面那条路拿不到简介 —— 实测简介在 <meta name="description"> 里。
    if d.description.trim().is_empty() {
        if let Ok(re2) = regex::Regex::new(
            r#"<meta[^>]+name="description"[^>]+content="([^"]*)""#,
        ) {
            if let Some(c) = re2.captures(&html) {
                d.description = unescape_html(&c[1]);
            }
        }
    }
    Ok(d)
}

/// 一条下载资源
#[derive(Debug, Clone, Default, Serialize)]
pub struct GxResource {
    pub id: u64,
    pub version: String,
    pub size: String,
    pub kind: String,
    pub remark: String,
    pub unzip_code: String,
    pub tested: String,
    pub folder: String,
    /// 可直连下载的 zip/rar/7z/apk（只排除网盘）
    pub files: Vec<GxFile>,
    /// 被排除掉的（网盘 / apk），仅作展示
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct GxFile {
    pub url: String,
    pub name: String,
    pub size: String,
}

/// 可直连下载的扩展名白名单。
///
/// ★ 用户要求「把 apk 的也提供」，而实测站点**大量资源是 .7z**
///   （抽样那条就是 `...Clappy Cheeks....7z`），原来只认 zip/rar 会把它们全丢掉，
///   前端就显示「这条资源没有 zip/rar 直链」。解压器本来就支持 7z（sevenz-rust2），
///   所以这里放开成 zip/rar/7z/apk —— 只有网盘链接仍然排除。
fn is_direct_file(path: &str) -> bool {
    [".zip", ".rar", ".7z", ".apk"].iter().any(|e| path.ends_with(e))
}

/// 把 GX 卡片映射成成人页通用的 `GameCard` 形状（成人页要合并三个来源）。
///
/// ★ 用户要求「r18 和全年龄都要、不要自己选」—— 所以这里**不做年龄过滤**，
///   用标签把分级标出来让用户自己分辨（nsfw → `成人游戏`，否则 → `全年龄`）。
pub fn card_to_gamecard(c: &GxCard) -> crate::search_engine::GameCard {
    let mut tags = c.tags.clone();
    tags.insert(0, if c.nsfw { "成人游戏".to_string() } else { "全年龄".to_string() });
    crate::search_engine::GameCard {
        name: c.name.clone(),
        appid: format!("gx-{}", c.id),
        source: "galgamex".into(),
        detail_url: c.slug.clone(),
        header_image: if c.cover.is_empty() { c.header.clone() } else { c.cover.clone() },
        category: c.kind.clone(),
        update_time: parse_time(&c.updated_at),
        extra: serde_json::json!({
            "gx_id": c.id, "slug": c.slug, "size": c.size,
            "version": c.version, "kind": c.kind, "nsfw": c.nsfw,
        }),
        tags,
        ..Default::default()
    }
}

/// 相关推荐（GX 详情页右侧用）：同标签最多的其它游戏，同分按热度排。
pub fn related(slug: &str, limit: usize) -> Vec<crate::search_engine::GameCard> {
    let g = index_lock().lock().map(|x| x.clone()).unwrap_or_default();
    let cur = match g.cards.iter().find(|c| c.slug == slug) {
        Some(c) => c,
        None => return Vec::new(),
    };
    let cur_tags: std::collections::HashSet<&String> = cur.tags.iter().collect();
    let mut scored: Vec<(usize, &GxCard)> = g
        .cards
        .iter()
        .filter(|c| c.slug != slug)
        .map(|c| (c.tags.iter().filter(|t| cur_tags.contains(t)).count(), c))
        .filter(|(n, _)| *n > 0)
        .collect();
    scored.sort_unstable_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.view_count.cmp(&a.1.view_count))
            .then(a.1.id.cmp(&b.1.id))
    });
    scored.into_iter().take(limit).map(|(_, c)| card_to_gamecard(c)).collect()
}

/// 把 GX 卡片映射成成人页通用的 `GameCard` 形状（成人页要合并三个来源）。
pub fn adult_cards(category: &str, keyword: &str) -> Vec<crate::search_engine::GameCard> {
    let g = index_lock().lock().map(|x| x.clone()).unwrap_or_default();
    let kw = keyword.trim().to_lowercase();
    let want_cat = !category.is_empty() && category != "全部类型";
    let mut out = Vec::new();
    for c in &g.cards {
        if want_cat && !c.tags.iter().any(|t| t == category) {
            continue;
        }
        if !kw.is_empty() {
            let hit = c.name.to_lowercase().contains(&kw)
                || c.short_desc.to_lowercase().contains(&kw)
                || c.tags.iter().any(|t| t.to_lowercase().contains(&kw));
            if !hit {
                continue;
            }
        }
        out.push(card_to_gamecard(c));
    }
    out
}

/// 标签名 + 热度（成人页的分类下拉要用）
pub fn tag_names_with_count() -> Vec<(String, u64)> {
    let g = index_lock().lock().map(|x| x.clone()).unwrap_or_default();
    g.tags.iter().map(|t| (t.name.clone(), t.count)).collect()
}

/// 索引里的游戏总数（首页统计用）
pub fn total_cards() -> u64 {
    index_lock().lock().map(|x| x.cards.len() as u64).unwrap_or(0)
}

/// `$D2026-10-06T08:02:18.283Z` → unix 秒
fn parse_time(s: &str) -> i64 {
    let t = s.trim().trim_start_matches("$D");
    chrono::DateTime::parse_from_rfc3339(t)
        .map(|d| d.timestamp())
        .unwrap_or(0)
}

/// 取某个游戏的资源列表。
///
/// ★ 用户要求：下载图标有四种（1=zip 2=rar 3=百度网盘 4=apk），**只要 1、2**。
///   所以这里把 `.apk` 和网盘链接全部丢到 `skipped`，`files` 里只留 zip/rar。
pub async fn resources(slug: &str, game_id: u64) -> Result<Vec<GxResource>, String> {
    let cached = index_lock().lock().map(|g| g.clone()).unwrap_or_default();
    let c = client();
    let action = if cached.action_resources.is_empty() {
        let (_g, r) = resolve_actions(&c, &cached, &|_m| {}).await?;
        r
    } else {
        cached.action_resources.clone()
    };

    let url = format!("{ORIGIN}/game/{slug}");
    let txt = call_action(&c, &url, &action, &format!("[{game_id}]")).await?;
    let v = parse_array_payload(&txt)?;
    let arr = v.as_array().cloned().unwrap_or_default();

    let mut out = Vec::new();
    for r in &arr {
        let id = r.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
        if id == 0 {
            continue;
        }
        let urls: Vec<String> = r
            .get("urls")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).map(|s| s.to_string()).collect())
            .unwrap_or_default();
        let sizes: Vec<String> = r
            .get("linkSizes")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .map(|x| x.as_str().unwrap_or("").to_string())
                    .collect()
            })
            .unwrap_or_default();
        let s = |k: &str| r.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();

        let mut files = Vec::new();
        let mut skipped = Vec::new();
        for (i, u) in urls.iter().enumerate() {
            if crate::browser::is_pan_link(u) {
                skipped.push(format!("[网盘] {u}"));
                continue;
            }
            let path = u.split('?').next().unwrap_or(u).to_lowercase();
            if is_direct_file(&path) {
                // ★ 必须百分号解码：站点给的是 %e8%a6%8b... 这种，不解码前端就是一堆乱码
                let name = percent_decode(path.rsplit('/').next().unwrap_or(""));
                files.push(GxFile {
                    url: u.clone(),
                    name,
                    size: sizes.get(i).cloned().unwrap_or_default(),
                });
            } else {
                let ext = path.rsplit('.').next().unwrap_or("?").to_uppercase();
                skipped.push(format!("[{ext}] {u}"));
            }
        }

        out.push(GxResource {
            id,
            version: s("version"),
            size: s("size"),
            kind: s("type"),
            remark: unescape_html(&s("remark")),
            unzip_code: s("unzipCode"),
            tested: s("tested"),
            folder: s("folder"),
            files,
            skipped,
        });
    }
    Ok(out)
}

/// 挑中的下载
#[derive(Debug, Clone, Default, Serialize)]
pub struct GxPicked {
    pub resource_id: u64,
    pub url: String,
    pub name: String,
    pub size: String,
    pub unzip_code: String,
    pub candidates: usize,
    pub pan_skipped: usize,
}

/// 把某条资源换成**签名直链**并随机挑一个 zip/rar。
///
/// 签名 URL 只活 1 小时，所以这里每次都重新换。
/// ★ `index`：下载弹窗里用户挑的是第几个文件（前端 `files` 的顺序 = 这里的 `cands`
///   顺序，两边用的是同一份 urls 白名单过滤）。None 就随机挑一条（旧行为）。
pub async fn pick_download(resource_id: u64, index: Option<usize>) -> Result<GxPicked, String> {
    let c = client();
    let r = c
        .post(format!("{ORIGIN}/api/game/resource/{resource_id}/download"))
        .header("Accept", "application/json")
        .header("Referer", format!("{ORIGIN}/games"))
        .header("Cookie", CONTENT_COOKIE)
        .send()
        .await
        .map_err(|e| format!("换签名链接失败: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("下载接口 HTTP {}", r.status()));
    }
    let v: serde_json::Value = r.json().await.map_err(|e| format!("下载接口不是 JSON: {e}"))?;
    if let Some(e) = v.get("error").and_then(|x| x.as_str()) {
        return Err(format!("下载接口报错: {e}"));
    }
    let urls: Vec<String> = v
        .pointer("/data/resource/urls")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.replace("&amp;", "&"))
                .collect()
        })
        .unwrap_or_default();
    let unzip = v
        .pointer("/data/resource/unzipCode")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let sizes: Vec<String> = v
        .pointer("/data/resource/fileSizes")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect())
        .unwrap_or_default();

    let mut cands: Vec<(String, String, String)> = Vec::new();
    let mut pan = 0usize;
    for (i, u) in urls.iter().enumerate() {
        if crate::browser::is_pan_link(u) {
            pan += 1;
            continue;
        }
        let path = u.split('?').next().unwrap_or(u).to_lowercase();
        if is_direct_file(&path) {
            let name = path.rsplit('/').next().unwrap_or("").to_string();
            let name = percent_decode(&name);
            let sz = sizes.get(i).cloned().unwrap_or_default();
            cands.push((u.clone(), if name.is_empty() { sz.clone() } else { name }, sz));
        } else {
            pan += 1; // 只剩网盘和认不出的格式
        }
    }
    if cands.is_empty() {
        return Err("这条资源没有可直连的文件（只剩网盘或认不出的格式）".to_string());
    }
    let seed = now_ms() as usize ^ std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0);
    // ★ 候选顺序必须与 `resources()` 交给前端的顺序**逐条一致**（同一套过滤、同一遍历序），
    //   否则前端回传的 index 会落到别的文件上。
    let (url, name, size) = match index {
        Some(i) if i < cands.len() => cands[i].clone(),
        _ => cands[seed % cands.len()].clone(),
    };
    Ok(GxPicked {
        resource_id,
        url,
        name,
        size,
        unzip_code: if unzip.is_empty() { "galgamex.com".into() } else { unzip },
        candidates: cands.len(),
        pan_skipped: pan,
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| -> Option<u8> {
                match c {
                    b'0'..=b'9' => Some(c - b'0'),
                    b'a'..=b'f' => Some(c - b'a' + 10),
                    b'A'..=b'F' => Some(c - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_object_extraction_handles_nested_and_strings() {
        let s = r#"0:{"a":1}
1:{"games":[{"name":"a}b","x":{"y":2}}],"total":7} trailing"#;
        let at = s.find("{\"games\":[").unwrap();
        let js = extract_json_object(s, at).unwrap();
        let v: serde_json::Value = serde_json::from_str(&js).unwrap();
        assert_eq!(v["total"], 7);
        assert_eq!(v["games"][0]["name"], "a}b");
    }

    #[test]
    fn card_parsing_picks_cover_and_tags() {
        let raw = serde_json::json!({
            "id": 2931, "uniqueId": "caf5aa1d", "name": "大学生活",
            "shortDesc": "desc", "headerImage": "h.webp", "coverImage": "",
            "isNsfw": false, "resourceUpdatedAt": "2026-10-06T07:39:45.645Z",
            "viewCount": 62812,
            "tags": [{"id":1,"name":"SLG","slug":"slg"},{"id":2,"name":"2D","slug":"2d"}],
            "stats": {"downloadCount": 4833},
            "resources": [{"size":"4.6 GB","version":"0.67.130b","type":"doujin"}],
            "media": [{"mediaType":"screenshot","urlFull":"s1.webp","urlThumbnail":"s1m.webp"},
                      {"mediaType":"video","urlFull":"v.mp4"}]
        });
        let c = card_from_json(&raw, "doujin").unwrap();
        assert_eq!(c.id, 2931);
        assert_eq!(c.slug, "caf5aa1d");
        assert_eq!(c.cover, "h.webp", "coverImage 空时要退回 headerImage");
        assert_eq!(c.size, "4.6 GB");
        assert_eq!(c.version, "0.67.130b");
        assert_eq!(c.tags, vec!["SLG", "2D"]);
        assert_eq!(c.download_count, 4833);
        assert_eq!(c.screenshots, vec!["s1.webp"], "只收 screenshot");
        assert!(!c.nsfw);
    }

    #[test]
    fn section_bodies_match_site_config() {
        // 照抄站点两个区块的参数，写错会少一大半数据
        let tr = games_body(&SECTIONS[0], 6000);
        assert!(tr.contains(r#""resourceType":"doujin""#));
        assert!(tr.contains(r#""onlyWithCover":false"#));
        assert!(tr.contains(r#""onlyWithHeader":true"#));
        let gal = games_body(&SECTIONS[1], 6000);
        assert!(gal.contains(r#""resourceType":"galgame""#));
        assert!(gal.contains(r#""onlyWithCover":true"#));
        assert!(gal.contains(r#""onlyWithHeader":false"#));
    }

    // ============================================================
    // 联网实测（默认 #[ignore]）
    //   cargo test --target x86_64-pc-windows-msvc --bin vortex-dl     //     gx::tests::live_ -- --ignored --nocapture
    // ============================================================
    #[tokio::test]
    #[ignore]
    async fn live_full_sync_and_browse() {
        let t0 = std::time::Instant::now();
        let idx = sync(&|m| println!("  {m}")).await.expect("同步失败");
        println!("
=== 同步完成 {:.1}s ===", t0.elapsed().as_secs_f64());
        let doujin = idx.cards.iter().filter(|c| c.kind == "doujin").count();
        let gal = idx.cards.iter().filter(|c| c.kind == "galgame").count();
        let r18 = idx.cards.iter().filter(|c| c.nsfw).count();
        let sfw = idx.cards.len() - r18;
        println!("  总数={}  同人={}  Galgame={}", idx.cards.len(), doujin, gal);
        println!("  全年龄={}  R18={}", sfw, r18);
        println!("  标签={}", idx.tags.len());
        println!("  前 12 个标签: {:?}",
            idx.tags.iter().take(12).map(|t| format!("{}({})", t.name, t.count)).collect::<Vec<_>>());
        assert!(idx.cards.len() > 6000, "总数应该 > 6000, 实际 {}", idx.cards.len());
        assert!(doujin > 4000, "同人应该 > 4000, 实际 {doujin}");
        assert!(gal > 2000, "Galgame 应该 > 2000, 实际 {gal}");
        assert!(r18 > 3000, "R18 应该 > 3000, 实际 {r18}");

        // 抽样看一张卡片
        let c = &idx.cards[0];
        println!("
  样本卡片: {} [{}] {} {} 标签={:?}",
            c.name, c.slug, c.size, if c.nsfw {"R18"} else {"全年龄"}, c.tags.iter().take(4).collect::<Vec<_>>());
        assert!(!c.slug.is_empty());
        assert!(!c.cover.is_empty());

        // 浏览 + 标签过滤
        let p = browse(&GxQuery { kind: "galgame".into(), page: 1, page_size: 10, ..Default::default() });
        println!("  浏览 Galgame 第 1 页: total={} 返回={}", p.total, p.items.len());
        assert_eq!(p.items.len(), 10);
        assert!(p.items.iter().all(|x| x.kind == "galgame"));

        let tag = idx.tags.iter().find(|t| t.name == "2D").cloned();
        if let Some(tg) = tag {
            let q = browse(&GxQuery { tag: tg.name.clone(), page: 1, page_size: 5, ..Default::default() });
            println!("  按标签「{}」过滤: total={}", tg.name, q.total);
            assert!(q.items.iter().all(|x| x.tags.contains(&tg.name)));
        }
    }

    /// 详情 + 资源列表 + 签名直链（用真实游戏跑一遍）
    #[tokio::test]
    #[ignore]
    async fn live_detail_resources_and_signed_url() {
        let idx = sync(&|_m| {}).await.expect("同步失败");
        // 找第一个有 zip/rar 直链的游戏
        let mut found = None;
        for c in idx.cards.iter().take(40) {
            match resources(&c.slug, c.id).await {
                Ok(rs) => {
                    if rs.iter().any(|r| !r.files.is_empty()) {
                        println!("
=== {} [{}] {} ===", c.name, c.slug, c.size);
                        for r in &rs {
                            println!("  资源 {} v={} size={} 解压码={:?} zip/rar={} 排除={}",
                                r.id, r.version, r.size, r.unzip_code, r.files.len(), r.skipped.len());
                            for f in &r.files { println!("      [可下] {} ({})", f.name, f.size); }
                            for s in r.skipped.iter().take(3) { println!("      [排除] {}", &s.chars().take(90).collect::<String>()); }
                        }
                        found = Some((c.clone(), rs));
                        break;
                    }
                }
                Err(e) => println!("  {} 资源失败: {e}", c.slug),
            }
        }
        let (card, rs) = found.expect("前 40 个游戏里没找到带 zip/rar 直链的");
        let rid = rs.iter().find(|r| !r.files.is_empty()).unwrap().id;
        let picked = pick_download(rid, None).await.expect("换签名链接失败");
        println!("
★ 随机挑中: {}", picked.name);
        println!("   {}", &picked.url.chars().take(140).collect::<String>());
        println!("   候选={} 排除={} 解压码={}", picked.candidates, picked.pan_skipped, picked.unzip_code);
        assert!(!crate::browser::is_pan_link(&picked.url));
        assert!(picked.url.contains("X-Amz-Signature"), "应该是签名直链");

        // 验证真的能下（只取 1 字节，别拉整个包）
        let c = client();
        let r = c.get(&picked.url).header("Range", "bytes=0-0")
            .header("Referer", format!("{ORIGIN}/")).send().await.expect("请求失败");
        println!("   HTTP {} content-length={:?}", r.status(), r.headers().get("content-length"));
        assert!(r.status().is_success(), "签名直链取不到: HTTP {}", r.status());
        let _ = (card,);
    }

    #[test]
    fn unescape_html_decodes_site_entities() {
        assert_eq!(unescape_html("&quot;Clappy Cheeks&quot; 是一款情色游戏"),
                   "\"Clappy Cheeks\" 是一款情色游戏");
        assert_eq!(unescape_html("A &amp; B &#39;x&#39; &lt;y&gt;"), "A & B 'x' <y>");
        assert_eq!(unescape_html("&#x4e2d;&#25991;"), "中文");
        assert_eq!(unescape_html("没有实体的中文"), "没有实体的中文");
        assert_eq!(unescape_html("a & b"), "a & b", "裸 & 不能吃掉后面的字符");
    }

    #[test]
    fn percent_decode_handles_chinese_name() {
        assert_eq!(percent_decode("%23A9830.zip"), "#A9830.zip");
        assert_eq!(percent_decode("A%20B.rar"), "A B.rar");
    }
}
