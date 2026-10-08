//! 修改器 (trainer) 模块 — 数据源 flingtrainer.com。
//!
//! 该站是 WordPress + Yoast, **没有 API/RSS**, 只有 HTML 与 sitemap:
//! - 索引: `https://flingtrainer.com/post-sitemap.xml` (允许抓取)
//! - 详情页: `https://flingtrainer.com/trainer/{slug}-trainer/` (允许抓取)
//! - 下载: `https://flingtrainer.com/downloads/{token},,`
//!
//! ⚠️ 该站 robots.txt 明确 `Disallow: /downloads/`。因此本模块:
//!   1. 只用 sitemap 建索引 (合规)
//!   2. **下载仅在用户点击某一个修改器时逐个进行**, 全局串行 + 最小间隔,
//!      **绝不批量遍历 /downloads/** —— 相当于"替用户点一次下载", 而非抓站。
//!   3. 界面会明示这一限制。
//!
//! 现有同类开源项目 (game-trainer-manager / FLiNG-Downloader 等) 全是 GPL/AGPL,
//! 无法嵌入本项目, 这里按同样的抓取契约独立实现。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use crate::paths::{ensure_dir, now_ms};

const SITE: &str = "https://flingtrainer.com";
/// 两次下载之间的最小间隔 (毫秒) —— 明确的限速护栏
const MIN_DOWNLOAD_GAP_MS: i64 = 3000;

static LAST_DOWNLOAD_MS: AtomicI64 = AtomicI64::new(0);
/// 同一时刻只允许一个下载在跑 (真正的"全局串行")
static DOWNLOADING: AtomicBool = AtomicBool::new(false);

/// 无论成功还是中途 return, 都会把 DOWNLOADING 放回 false
struct DownloadGuard;
impl Drop for DownloadGuard {
    fn drop(&mut self) {
        DOWNLOADING.store(false, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainerEntry {
    pub slug: String,
    pub name: String,
    pub url: String,
    pub updated: String,
    /// 封面图 (sitemap 里每页的第一张图, 就是该页的 og:image = 游戏封面)。
    /// `#[serde(default)]` 是为了能读旧缓存 —— 旧索引没这个字段, 读出来是空串,
    /// 由 `tr_index` 检测到后自动重同步一次。
    #[serde(default)]
    pub cover: String,
    /// 中文名 (Steam 官方译名, 见 `steam_cn_name`)。查不到时为空, 前端回退英文名。
    #[serde(default)]
    pub name_zh: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainerFile {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainerDetail {
    pub url: String,
    pub title: String,
    pub options: Vec<String>,
    pub files: Vec<TrainerFile>,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryItem {
    pub game: String,
    pub filename: String,
    pub path: String,
    pub size: u64,
    /// ⚠️ 这是**下载令牌地址** (`/downloads/xxx,,`), 不是页面地址, 反推不出 slug
    pub source_url: String,
    pub added_ms: i64,
    /// 中文名 (由页面地址的 slug 查中文名缓存得到, 不落盘)。
    /// 前端优先显示它, 用户手动改过名则显示 `game`。
    #[serde(default)]
    pub game_zh: String,
    /// 修改器页面地址 (`/trainer/xxx-trainer/`) —— 查中文名/封面要靠它反推 slug。
    /// 老条目没有这个字段, 会走"拿文件名去索引里对名字"的兜底。
    #[serde(default)]
    pub page_url: String,
    /// 封面图 (从索引里按 slug 取, 不落盘)
    #[serde(default)]
    pub cover: String,
}

// ============================================================
// 索引
// ============================================================

/// 从 slug 推导游戏名: "baldurs-gate-3-trainer" → "Baldurs Gate 3"
pub fn name_from_slug(slug: &str) -> String {
    let base = slug.trim_end_matches("-trainer");
    base.split('-')
        .filter(|s| !s.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 取 `s` 里第一个 `<tag>...</tag>` 的文本
fn tag_text(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let i = s.find(&open)?;
    let rest = &s[i + open.len()..];
    let j = rest.find(&close)?;
    Some(rest[..j].trim().to_string())
}

/// 解析 Yoast sitemap 的 `<url>` 块, 只保留 /trainer/ 页面。
///
/// ★ 必须**先按 `<url>` 切块**再解析: 旧写法是在整段 XML 上顺序找 `<loc>`,
///   然后拿 `</loc>` 之后的**剩余全文**找 `<lastmod>`/图片 —— 那会越过 `</url>`
///   读进下一个块。没有图片的页面就会"偷"到下一页的封面。
pub fn parse_sitemap(xml: &str) -> Vec<TrainerEntry> {
    let mut out = Vec::new();
    for raw in xml.split("<url>").skip(1) {
        let block = match raw.find("</url>") {
            Some(i) => &raw[..i],
            None => raw,
        };
        let Some(loc) = tag_text(block, "loc") else { continue };
        if !loc.contains("/trainer/") {
            continue;
        }
        let slug = loc
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string();
        if slug.is_empty() || slug == "trainer" {
            continue;
        }
        out.push(TrainerEntry {
            name: name_from_slug(&slug),
            slug,
            url: loc,
            updated: tag_text(block, "lastmod").unwrap_or_default(),
            // 块里第一张图 = 该页 og:image = 游戏封面 (实测 758 页里 748 个不重复)
            cover: tag_text(block, "image:loc").unwrap_or_default(),
            name_zh: String::new(), // 由 names 缓存填入
        });
    }
    out
}

/// 抓取并合并所有 post-sitemap 分页
pub async fn sync_index(client: &reqwest::Client) -> Result<Vec<TrainerEntry>, String> {
    let mut all: Vec<TrainerEntry> = Vec::new();
    for page in 1..=6 {
        let url = if page == 1 {
            format!("{SITE}/post-sitemap.xml")
        } else {
            format!("{SITE}/post-sitemap{page}.xml")
        };
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(_) => break,
        };
        if !resp.status().is_success() {
            break;
        }
        let text = resp.text().await.map_err(|e| format!("读取 sitemap 失败: {e}"))?;
        let entries = parse_sitemap(&text);
        if entries.is_empty() {
            break;
        }
        all.extend(entries);
    }
    if all.is_empty() {
        return Err("没有从 sitemap 解析到任何修改器".to_string());
    }
    Ok(finalize_index(all))
}

/// 去重 (同名 slug 只留一条) + 按更新时间倒序。抽出来是为了能单独测试。
pub fn finalize_index(mut v: Vec<TrainerEntry>) -> Vec<TrainerEntry> {
    v.sort_by(|a, b| b.updated.cmp(&a.updated));
    v.dedup_by(|a, b| a.slug == b.slug);
    v
}

fn index_path() -> PathBuf {
    crate::paths::data_dir().join("trainers").join("index.json")
}

pub fn save_index(v: &[TrainerEntry]) -> Result<(), String> {
    let p = index_path();
    ensure_dir(p.parent().unwrap_or(&p))?;
    let s = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    std::fs::write(&p, s).map_err(|e| format!("写入索引失败: {e}"))
}

pub fn load_index() -> Vec<TrainerEntry> {
    std::fs::read_to_string(index_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

// ============================================================
// 中文名 (Steam 官方译名)
// ============================================================
//
// 站点只有英文名。机翻游戏名很糟 ("Elden Ring" → "艾尔登之环"), 而 Steam 的
// storesearch 接口**免密钥**且能按语言返回官方译名 ("艾尔登法环" / "巫师3：狂猎" /
// "空战奇兵8 希孚之翼"), 所以走 Steam。查不到就回退英文名, 绝不瞎编。

fn names_path() -> PathBuf {
    crate::paths::data_dir().join("trainers").join("names_zh.json")
}

pub fn load_names() -> std::collections::HashMap<String, String> {
    std::fs::read_to_string(names_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_names(m: &std::collections::HashMap<String, String>) -> Result<(), String> {
    let p = names_path();
    ensure_dir(p.parent().unwrap_or(&p))?;
    let s = serde_json::to_string_pretty(m).map_err(|e| e.to_string())?;
    std::fs::write(&p, s).map_err(|e| format!("写入中文名缓存失败: {e}"))
}

/// 把缓存里的中文名填进索引
pub fn apply_names(v: &mut [TrainerEntry]) {
    let cache = load_names();
    for e in v.iter_mut() {
        if let Some(zh) = cache.get(&e.slug) {
            e.name_zh = zh.clone();
        }
    }
}

/// 从 `/trainer/xxx-trainer/` 这样的页面地址反推 slug 再查中文名
pub fn zh_for_url(url: &str) -> String {
    if !url.contains("/trainer/") {
        return String::new(); // 下载令牌地址反推不出东西
    }
    let slug = url.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    if slug.is_empty() {
        return String::new();
    }
    load_names().get(slug).cloned().unwrap_or_default()
}

/// 找到库条目对应的索引项 —— 中文名和封面图都靠它。
///
/// 优先用页面地址反推 slug; 老条目没存页面地址, 就拿显示名/文件名
/// 去索引里对游戏名 (英文名和已升级的中文名都算), 否则老条目永远没有中文名/封面。
pub fn index_entry_for(item: &LibraryItem) -> Option<TrainerEntry> {
    let idx = load_index();
    for u in [&item.page_url, &item.source_url] {
        if !u.contains("/trainer/") {
            continue;
        }
        let slug = u.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        if slug.is_empty() {
            continue;
        }
        if let Some(e) = idx.iter().find(|e| e.slug == slug) {
            return Some(e.clone());
        }
    }
    let cache = load_names();
    let stem = match item.filename.rsplit_once('.') {
        Some((a, _)) => a,
        None => item.filename.as_str(),
    };
    let cands = [item.game.trim(), stem.trim()];
    idx.into_iter().find(|e| {
        let zh = cache.get(&e.slug).map(|s| s.as_str()).unwrap_or("");
        cands.iter().any(|c| {
            !c.is_empty() && (c.eq_ignore_ascii_case(e.name.trim()) || *c == zh)
        })
    })
}

/// 查一个库条目该显示的中文名
pub fn zh_for_item(item: &LibraryItem) -> String {
    match index_entry_for(item) {
        Some(e) => load_names().get(&e.slug).cloned().unwrap_or_default(),
        None => String::new(),
    }
}

/// 查一个库条目该显示的封面图
pub fn cover_for_item(item: &LibraryItem) -> String {
    index_entry_for(item).map(|e| e.cover).unwrap_or_default()
}

/// 去掉末尾的纯英文括注:
/// "空战奇兵8 希孚之翼 (ACE COMBAT 8: WINGS OF THEVE)" → "空战奇兵8 希孚之翼"
pub fn strip_ascii_paren(s: &str) -> String {
    let t = s.trim();
    if !t.ends_with(')') {
        return t.to_string();
    }
    let Some(i) = t.rfind('(') else { return t.to_string() };
    let inner = &t[i + 1..t.len() - 1];
    let head = t[..i].trim();
    if !head.is_empty() && inner.is_ascii() && inner.chars().any(|c| c.is_ascii_alphabetic()) {
        head.to_string()
    } else {
        t.to_string()
    }
}

/// 拿去 Steam 搜索前先清洗游戏名:
/// - 去掉末尾的**长数字** —— 站点给重名页面加的时间戳,
///   如 "Days Gone Trainer 20210518" / "Grand Theft Auto V Trainer 1766066855"
/// - 去掉末尾的 Trainer / Trainers 字样
///
/// ⚠️ 只去 ≥7 位的纯数字, 否则会把 "Cyberpunk 2077" / "Pc Building Simulator 2"
/// 这类真名里的数字砍掉。
pub fn search_term_for(name: &str) -> String {
    let mut words: Vec<&str> = name.split_whitespace().collect();
    if let Some(last) = words.last() {
        if last.len() >= 7 && last.chars().all(|c| c.is_ascii_digit()) {
            words.pop();
        }
    }
    if let Some(last) = words.last() {
        let low = last.to_lowercase();
        if low == "trainer" || low == "trainers" {
            words.pop();
        }
    }
    let out = words.join(" ");
    if out.trim().is_empty() {
        name.trim().to_string()
    } else {
        out
    }
}

/// 查一个游戏名的 Steam 官方中文名。
///
/// 返回 `Err` = **没查成** (网络/限流/HTTP 错), `Ok(None)` = 查成了但没匹配,
/// `Ok(Some)` = 命中。区分开是为了"连续失败就收手" —— 见 `translate_names`。
/// Steam 搜索的第一个结果 —— 返回 (官方名, 小图地址, appid)。
/// 官方名给修改器页用; appid 用来拼 460x215 的**高清头图**
/// (`cdn.cloudflare.steamstatic.com/steam/apps/{appid}/header.jpg`), 给模组市场用。
pub async fn steam_search_first(
    client: &reqwest::Client,
    term: &str,
    lang: &str,
) -> Result<Option<(String, String, u64)>, String> {
    let term = term.trim();
    if term.is_empty() {
        return Ok(None);
    }
    let url = format!(
        "https://store.steampowered.com/api/storesearch/?term={}&l={}&cc=cn",
        urlencoding::encode(term),
        urlencoding::encode(lang)
    );
    let resp = client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let first = v
        .get("items")
        .and_then(|x| x.as_array())
        .and_then(|a| a.first());
    let Some(item) = first else { return Ok(None) };
    let name = item.get("name").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let img = item
        .get("tiny_image")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() {
        return Ok(None);
    }
    let appid = item.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
    Ok(Some((name, img, appid)))
}

/// 查一个游戏名的 Steam 官方中文名
pub async fn steam_cn_name(
    client: &reqwest::Client,
    term: &str,
) -> Result<Option<String>, String> {
    Ok(steam_search_first(client, term, "schinese")
        .await?
        .map(|(n, _, _)| strip_ascii_paren(&n)))
}

/// 连着失败这么多次就认为被限流/网络不通, 收手别再硬打
const NAME_FAIL_STREAK_STOP: usize = 10;
/// 每攒够这么多条就落一次盘 (中途被打断也不至于全丢)
const NAME_SAVE_EVERY: usize = 20;
/// 每次请求之间的最小间隔 —— 别把 Steam 打急了
const NAME_GAP_MS: u64 = 120;

/// 批量补齐中文名。**只缓存命中的**, 查不到的下次还能重试。
/// 返回本次新增的命中数。
///
/// ⚠️ 实测教训: 并发 6 一口气打 758 次会被 Steam 限流 (之后整段连不上,
/// `http=000`), 所以这里并发降到 3 + 请求间隔, 且**连续失败就提前收手**。
pub async fn translate_names(app: &tauri::AppHandle, client: &reqwest::Client) -> usize {
    use futures::StreamExt;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    use tauri::Emitter;

    let mut cache = load_names();
    let todo: Vec<TrainerEntry> = load_index()
        .into_iter()
        .filter(|e| !cache.contains_key(&e.slug))
        .collect();
    let total = todo.len();
    if total == 0 {
        return 0;
    }
    let _ = app.emit("tr-names-progress", serde_json::json!({ "done": 0, "total": total }));

    let done = Arc::new(AtomicUsize::new(0));
    let hits = Arc::new(AtomicUsize::new(0));
    let mut stream = futures::stream::iter(todo.into_iter().map(|e| {
        let client = client.clone();
        let app = app.clone();
        let done = done.clone();
        let hits = hits.clone();
        async move {
            // 每个请求前先歇一下, 拉平整体速率
            tokio::time::sleep(std::time::Duration::from_millis(NAME_GAP_MS)).await;
            let mut got = steam_cn_name(&client, &search_term_for(&e.name)).await;
            // ★ 兜底: slug 推出的名字查不到时 (拼音中文游戏如 "Gui Gu Ba Huang"),
            //   改用它**页面上的英文标题**再试一次 —— 例如 "Tale of Immortal" 就能查到"鬼谷八荒"。
            //   只在查不到时才多抓一次页面, 命中率提升明显。
            if matches!(got, Ok(None)) {
                if let Ok(d) = fetch_detail(&client, &e.url).await {
                    let t = d.title.trim_end_matches(" Trainer").trim().to_string();
                    if !t.is_empty() && t != e.name {
                        got = steam_cn_name(&client, &t).await;
                    }
                }
            }
            if matches!(got, Ok(Some(_))) {
                hits.fetch_add(1, Ordering::Relaxed);
            }
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            let _ = app.emit("tr-names-progress", serde_json::json!({ "done": n, "total": total }));
            (e.slug, got)
        }
    }))
    .buffer_unordered(3);

    let mut fail_streak = 0usize;
    let mut since_save = 0usize;
    let mut aborted = false;
    while let Some((slug, got)) = stream.next().await {
        match got {
            Ok(Some(zh)) => {
                cache.insert(slug, zh);
                fail_streak = 0;
                since_save += 1;
            }
            Ok(None) => fail_streak = 0, // 正常查过但没匹配, 不算失败
            Err(_) => fail_streak += 1,
        }
        if since_save >= NAME_SAVE_EVERY {
            let _ = save_names(&cache);
            since_save = 0;
        }
        if fail_streak >= NAME_FAIL_STREAK_STOP {
            aborted = true;
            break;
        }
    }
    let _ = save_names(&cache);
    let hits = hits.load(Ordering::Relaxed);
    let _ = app.emit(
        "tr-names-done",
        serde_json::json!({ "total": total, "hits": hits, "aborted": aborted }),
    );
    hits
}

// ============================================================
// 详情
// ============================================================

fn unescape(s: &str) -> String {
    // ★ 先解数字实体 (&#046; / &#x2E;) —— 页面里大量使用, 例如 "Super Damage/One&#046;&#046;&#046;"
    let s = match regex::Regex::new(r"&#(x?[0-9A-Fa-f]+);") {
        Ok(re) => re
            .replace_all(s, |c: &regex::Captures| {
                let raw = &c[1];
                let code = if let Some(h) = raw.strip_prefix('x').or_else(|| raw.strip_prefix('X')) {
                    u32::from_str_radix(h, 16).ok()
                } else {
                    raw.parse::<u32>().ok()
                };
                code.and_then(char::from_u32).map(|ch| ch.to_string()).unwrap_or_default()
            })
            .to_string(),
        Err(_) => s.to_string(),
    };
    let s = s.as_str();
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&nbsp;", " ")
        .replace("&#8217;", "'")
        .replace("&#8211;", "-")
}

pub async fn fetch_detail(client: &reqwest::Client, url: &str) -> Result<TrainerDetail, String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("打开修改器页面失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("打开修改器页面失败: HTTP {}", resp.status()));
    }
    let html = resp.text().await.map_err(|e| format!("读取页面失败: {e}"))?;

    // 标题: <h1 class="post-title">...</h1>
    let title = regex::Regex::new(r#"(?is)<h1[^>]*class="[^"]*post-title[^"]*"[^>]*>(.*?)</h1>"#)
        .ok()
        .and_then(|re| re.captures(&html))
        .map(|c| unescape(&strip_tags(&c[1])).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "未知修改器".to_string());

    // ★ 只在 <article> 正文里找, 否则会把侧边栏 "Popular Trainers" 的链接也抓进来
    let body = article_block(&html).unwrap_or_else(|| html.clone());

    // ★ 下载链接: 真实 HTML 的属性顺序是 href/title 在 class 之前 ——
    //   **href 在 class 之前**, 所以不能写"class 在前"的正则。改为先抓整个 <a ...> 标签,
    //   再各自取 class / href / title 属性。
    let a_re = regex::Regex::new(r#"(?is)<a\s+([^>]*?)>(.*?)</a>"#).map_err(|e| e.to_string())?;
    let mut files: Vec<TrainerFile> = Vec::new();
    for c in a_re.captures_iter(&body) {
        let attrs = &c[1];
        if !attrs.contains("attachment-link") {
            continue;
        }
        let href = attr_value(attrs, "href");
        if href.is_empty() {
            continue;
        }
        // 文件名优先用 title 属性 (真实文件名), 否则用链接文字
        let mut name = attr_value(attrs, "title");
        if name.is_empty() {
            name = unescape(&strip_tags(&c[2])).trim().to_string();
        }
        let full = if href.starts_with("http") {
            href
        } else {
            format!("{SITE}{}", if href.starts_with('/') { href } else { format!("/{href}") })
        };
        if !files.iter().any(|f| f.url == full) {
            files.push(TrainerFile {
                name: if name.is_empty() { "下载".to_string() } else { name },
                url: full,
            });
        }
    }

    // ★ 功能选项: 页面把完整列表放进了 og:description 元标签
    //   (形如 "Options Num 1 – God Mode/Ignore Hits Num 2 – Infinite Health ..."),
    //   比解析 <ul> 可靠得多 —— 后者会把导航栏菜单也算进来。
    let mut options = Vec::new();
    if let Ok(meta_re) = regex::Regex::new(
        r#"(?is)<meta[^>]*property="og:description"[^>]*content="([^"]*)""#,
    ) {
        if let Some(c) = meta_re.captures(&html) {
            options = parse_options(&unescape(&c[1]));
        }
    }
    if options.is_empty() {
        // 兜底: 正文里的第一个 <ul>
        options = options_from_body(&body);
    }

    Ok(TrainerDetail {
        url: url.to_string(),
        title,
        options,
        files,
        note: "该站 robots.txt 禁止自动抓取 /downloads/，因此下载只在点击时逐个进行（全局串行、≥3 秒间隔，不做批量抓取）。".to_string(),
    })
}

/// 取 <article ...>...</article> 的内容
fn article_block(html: &str) -> Option<String> {
    let i = html.find("<article")?;
    let j = html[i..].find("</article>")?;
    Some(html[i..i + j].to_string())
}

/// 从属性串里取某个属性的值 (属性顺序不定, 所以逐个匹配)
fn attr_value(attrs: &str, name: &str) -> String {
    let re = match regex::Regex::new(&format!(r#"(?is)(?:^|\s){name}\s*=\s*"([^"]*)""#)) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    re.captures(attrs)
        .map(|c| unescape(&c[1]).trim().to_string())
        .unwrap_or_default()
}

/// 解析 "Options Num 1 – X Num 2 – Y ..." 形式的选项串
pub fn parse_options(text: &str) -> Vec<String> {
    let t = text.replace("Options", " ");
    // 以 "Num <数字>" 为分隔
    let re = match regex::Regex::new(r"Num\s*\d+") {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut last_end: Option<usize> = None;
    for m in re.find_iter(&t) {
        if let Some(prev) = last_end {
            let seg = t[prev..m.start()].trim().trim_start_matches(['–', '-', '—', ':']).trim();
            if !seg.is_empty() && seg.len() < 200 {
                out.push(seg.to_string());
            }
        }
        last_end = Some(m.end());
    }
    if let Some(prev) = last_end {
        let seg = t[prev..].trim().trim_start_matches(['–', '-', '—', ':']).trim();
        if !seg.is_empty() && seg.len() < 200 {
            out.push(seg.to_string());
        }
    }
    out
}

/// 兜底: 从正文第一个 <ul> 里取 <li>
fn options_from_body(body: &str) -> Vec<String> {
    let Ok(ul_re) = regex::Regex::new(r"(?is)<ul[^>]*>(.*?)</ul>") else {
        return Vec::new();
    };
    let Ok(li_re) = regex::Regex::new(r"(?is)<li[^>]*>(.*?)</li>") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for c in ul_re.captures_iter(body) {
        for li in li_re.captures_iter(&c[1]) {
            let t = unescape(&strip_tags(&li[1])).trim().to_string();
            if t.len() > 2 && t.len() < 200 && !out.contains(&t) {
                out.push(t);
            }
        }
        if !out.is_empty() {
            break;
        }
    }
    out
}

fn strip_tags(s: &str) -> String {
    match regex::Regex::new(r"(?is)<[^>]+>") {
        Ok(re) => re.replace_all(s, "").to_string(),
        Err(_) => s.to_string(),
    }
}

// ============================================================
// 下载 (带限速护栏)
// ============================================================

/// 按**内容**判断真实类型 —— 不能信 URL 或 Content-Disposition:
/// FLiNG 的下载地址以 `.zip` 结尾、文件名叫 `.zip`, 实际发的却是一个
/// 自解压 PE 可执行文件 (Content-Type: application/x-msdownload)。
/// 只按名字存, 用户拿到的就是一个打不开的"zip"。
pub fn sniff_ext(b: &[u8]) -> &'static str {
    if b.starts_with(b"MZ") {
        "exe"
    } else if b.starts_with(b"PK\x03\x04") || b.starts_with(b"PK\x05\x06") {
        "zip"
    } else if b.starts_with(b"7z\xBC\xAF\x27\x1C") {
        "7z"
    } else if b.starts_with(b"Rar!\x1A\x07\x00") || b.starts_with(b"Rar!\x1A\x07\x01\x00") {
        "rar"
    } else {
        ""
    }
}

/// 把文件名的扩展名换成 `ext` (没有扩展名就补一个)
pub fn force_ext(name: &str, ext: &str) -> String {
    let base = match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    };
    format!("{base}.{ext}")
}

/// 从最终跳转地址里取真实文件名。
///
/// flingtrainer 的真实文件名藏在查询参数里:
/// `https://flingtrainer.com/download-trainer.php?path=%2Fwp-content%2Fuploads%2F2020%2F05%2FTerraria.v1.4-...zip`
/// (HEAD 响应没有 Content-Disposition, 只有这条 `path` 靠得住)
pub fn filename_from_url(final_url: &str) -> Option<String> {
    let u = url::Url::parse(final_url).ok()?;
    for (k, v) in u.query_pairs() {
        if k == "path" {
            let name = v.rsplit('/').next().unwrap_or("").trim().to_string();
            if !name.is_empty() {
                return Some(percent_decode_once(&name));
            }
        }
    }
    let seg = u.path().trim_end_matches(',').rsplit('/').next()?.trim().to_string();
    if seg.is_empty() || seg.ends_with(".php") {
        None
    } else {
        Some(percent_decode_once(&seg))
    }
}

/// 再解一次百分号编码。
///
/// `query_pairs()` 已经解过一层, 但实测服务端有时会**双重编码**
/// (`path=...Ace%2520Combat...` → 解一层还剩 `%20`), 于是文件名里带着
/// `%20` 落盘。带 `%` 才解, 避免把正常文件名里合法的 `%` 弄坏。
pub fn percent_decode_once(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    urlencoding::decode(s)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| s.to_string())
}

/// PE 文件是否**被截断** (节表声明的数据长度超出了实际文件大小)。
///
/// ★ 为什么需要: flingtrainer 的 CDN 对同一个文件给出的 Content-Length 有时比真实内容小
///   (实测 header 报 1,544,715, 实际发 3,271,680)。下载引擎按探测到的 total 下满就停,
///   于是文件被截断 —— 启动时报 **os error 193 (不是有效的 Win32 程序)**。
fn pe_truncated(p: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(p) else { return false };
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    if size < 0x40 {
        return false;
    }
    let mut hdr = [0u8; 0x40];
    if f.read_exact(&mut hdr).is_err() {
        return false;
    }
    if &hdr[0..2] != b"MZ" {
        return false; // 不是 PE, 交给别的判断
    }
    let e_lfanew = u32::from_le_bytes([hdr[0x3c], hdr[0x3d], hdr[0x3e], hdr[0x3f]]) as u64;
    if f.seek(SeekFrom::Start(e_lfanew)).is_err() {
        return false;
    }
    let mut ph = [0u8; 24];
    if f.read_exact(&mut ph).is_err() {
        return true; // PE 头都读不全 → 截断
    }
    if &ph[0..4] != b"PE  " {
        return false;
    }
    let nsec = u16::from_le_bytes([ph[6], ph[7]]) as u64;
    let opt_size = u16::from_le_bytes([ph[20], ph[21]]) as u64;
    let sec_start = e_lfanew + 24 + opt_size;
    let mut max_end = 0u64;
    for i in 0..nsec.min(96) {
        let off = sec_start + i * 40;
        if f.seek(SeekFrom::Start(off + 16)).is_err() {
            break;
        }
        let mut raw = [0u8; 8];
        if f.read_exact(&mut raw).is_err() {
            break;
        }
        let raw_size = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as u64;
        let raw_ptr = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as u64;
        max_end = max_end.max(raw_ptr + raw_size);
    }
    max_end > size
}

/// 只读文件头 8 字节用于判定真实类型 (不必把整个文件读进内存)
pub fn read_magic(p: &Path) -> Vec<u8> {
    use std::io::Read;
    let mut buf = vec![0u8; 8];
    match std::fs::File::open(p) {
        Ok(mut f) => match f.read(&mut buf) {
            Ok(n) => buf[..n].to_vec(),
            Err(_) => Vec::new(),
        },
        Err(_) => Vec::new(),
    }
}

/// 下载一个修改器文件到库目录。**每次调用只取一个文件**。
///
/// 字节传输交给主下载引擎 (`engine_download_wait`), 所以「下载」页能看到实时进度。
pub async fn download_trainer(
    app: &tauri::AppHandle,
    tasks: &crate::downloader::DownloadTasksMap,
    client: &reqwest::Client,
    url: &str,
    game: &str,
    page_url: &str,
) -> Result<LibraryItem, String> {
    // —— 限速护栏: 全局串行 + 最小间隔 ——
    // 串行: 同时只跑一个 (原来是"间隔不够就报错", 前端连着点两次会看到莫名其妙的失败)
    if DOWNLOADING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("上一个修改器还在下载中，请稍候再试".to_string());
    }
    let _guard = DownloadGuard;
    // 间隔不够就**等够**再下, 而不是把用户挡回去 —— 对站点一样礼貌, 体验好得多
    let last = LAST_DOWNLOAD_MS.load(Ordering::Relaxed);
    let now = now_ms();
    if last > 0 && now - last < MIN_DOWNLOAD_GAP_MS {
        let wait = (MIN_DOWNLOAD_GAP_MS - (now - last)) as u64;
        tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
    }
    LAST_DOWNLOAD_MS.store(now_ms(), Ordering::Relaxed);

    let referer = format!("{SITE}/");
    // HEAD 只为拿真实文件名 (顺带确认链接有效); 失败不致命, 用游戏名兜底
    // 一次 HEAD 同时拿两样: 真实文件名 + 文件大小 (后者用来防重复下载)
    let (head_name, head_len) = match client
        .head(url)
        .header("Referer", &referer)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => (
            filename_from_url(r.url().as_str()),
            r.headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok()),
        ),
        _ => (None, None),
    };
    let guess = head_name
        .unwrap_or_else(|| format!("{}.bin", crate::paths::sanitize_filename(game)));
    let guess = crate::paths::sanitize_filename(&guess);

    // ★ 按游戏建独立文件夹 —— 原来所有修改器都堆在 files/ 下, 几十个混在一起没法找。
    //   (和 Game-Cheats-Manager 一样的组织方式, 但目录结构是我们自己的)
    let dir = library_dir().join(crate::paths::sanitize_filename(game));
    ensure_dir(&dir)?;
    let staged = dir.join(&guess);

    // ★ 防重复下载: 目标文件已存在且大小和远端一致 → 直接复用, 不再下一遍
    if let Some(len) = head_len {
        if let Ok(meta) = std::fs::metadata(&staged) {
            // ★ 还要排除被截断的文件: 截断文件的大小恰好等于 CDN 报的长度,
            //   只看大小会把它当成缓存命中而跳过重下, 于是永远修不好。
            if meta.len() == len && len > 0 && !pe_truncated(&staged) {
                let mut lib = load_library();
                if let Some(it) = lib.iter().find(|i| i.path == staged.to_string_lossy()) {
                    return Ok(it.clone());
                }
                let item = LibraryItem {
                    game: game.to_string(),
                    filename: staged.file_name().map(|x| x.to_string_lossy().to_string())
                        .unwrap_or_else(|| guess.clone()),
                    path: staged.to_string_lossy().to_string(),
                    size: len,
                    source_url: url.to_string(),
                    added_ms: now_ms(),
                    game_zh: String::new(),
                    page_url: page_url.to_string(),
                    cover: String::new(),
                };
                lib.push(item.clone());
                save_library(&lib)?;
                return Ok(item);
            }
        }
    }

    let task_id = format!("tr-{}", now_ms());
    crate::downloader::engine_download_wait(
        app,
        tasks,
        task_id,
        url,
        &staged,
        vec![("Referer".to_string(), referer.clone())],
    )
    .await?;

    // ★ 完整性校验: CDN 报的 total 可能比真实内容小, 引擎按 total 下满就停 → 文件被截断,
    //   启动时 os error 193。这里发现截断就用普通流式 GET 重下一遍 (修改器都很小, 1~4MB)。
    if pe_truncated(&staged) {
        eprintln!("[TR] 检测到下载被截断, 改用普通 GET 重下: {}", staged.display());
        let resp = client
            .get(url)
            .header("Referer", &referer)
            .send()
            .await
            .map_err(|e| format!("重新下载失败: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("重新下载失败: HTTP {}", resp.status()));
        }
        let bytes = resp.bytes().await.map_err(|e| format!("重新下载读取失败: {e}"))?;
        if bytes.len() < 1024 {
            return Err("重新下载拿到的内容过小, 可能不是有效文件".to_string());
        }
        std::fs::write(&staged, &bytes).map_err(|e| format!("重新写入失败: {e}"))?;
        eprintln!("[TR] 重下完成: {} 字节", bytes.len());
    }

    // 以实际内容为准修正扩展名 (见 sniff_ext 的注释)
    let mut filename = guess.clone();
    let real = sniff_ext(&read_magic(&staged));
    if !real.is_empty() {
        filename = force_ext(&guess, real);
    }
    let mut dest = dir.join(&filename);
    if dest != staged {
        if dest.exists() {
            let _ = std::fs::remove_file(&dest);
        }
        match std::fs::rename(&staged, &dest) {
            Ok(()) => {}
            Err(_) => dest = staged.clone(), // 改名失败就保留原名, 不丢文件
        }
    }
    let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    let filename = dest
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or(filename);

    let item = LibraryItem {
        game: game.to_string(),
        filename,
        path: dest.to_string_lossy().to_string(),
        size,
        source_url: url.to_string(),
        added_ms: now_ms(),
        game_zh: String::new(),
        page_url: page_url.to_string(),
        cover: String::new(),
    };
    let mut lib = load_library();
    lib.retain(|i| i.path != item.path);
    lib.push(item.clone());
    save_library(&lib)?;
    Ok(item)
}

// ============================================================
// 本地库
// ============================================================

/// 老位置: `<我的世界目录的父目录>\trainers` (例如 D:\game\trainers)。
///
/// ★ 这原来是修改器库的正式位置, 但它是从"我的世界游戏目录"反推出来的 ——
///   用户一改我的世界的目录, 修改器库就会跟着悄悄搬家, 很隐蔽。
///   现在正式位置改到 `<软件目录>\data\trainers` (见下), 这个函数只用于搬迁。
fn legacy_trainer_dir() -> PathBuf {
    crate::paths::data_dir().join("trainers")
}

/// 一次性把老位置的修改器库搬到软件目录下 (只做一次, 复制不移动)。
fn migrate_legacy_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let old = legacy_trainer_dir();
        let new = crate::paths::data_dir().join("trainers");
        if !old.exists() || new.exists() || old == new {
            return;
        }
        if crate::paths::copy_tree(&old, &new).is_err() {
            return;
        }
        // library.json 里存的是**绝对路径**, 前缀得跟着改 —— 注意 JSON 里反斜杠是转义的,
        // 所以转义形式和原文都要替换一遍。
        let lp = new.join("library.json");
        if let Ok(txt) = std::fs::read_to_string(&lp) {
            let op = old.to_string_lossy().to_string();
            let np = new.to_string_lossy().to_string();
            let fixed = txt
                .replace(&op.replace('\\', "\\\\"), &np.replace('\\', "\\\\"))
                .replace(&op, &np);
            let _ = std::fs::write(&lp, fixed);
        }
        eprintln!("[TR] 修改器库已迁移到软件目录: {} -> {}", old.display(), new.display());
    });
}

/// 修改器库根目录 —— 在**主软件目录**下 (`<软件目录>\data\trainers`)。
fn library_dir() -> PathBuf {
    migrate_legacy_once();
    crate::paths::data_dir().join("trainers").join("files")
}

fn library_path() -> PathBuf {
    migrate_legacy_once();
    crate::paths::data_dir().join("trainers").join("library.json")
}

pub fn load_library() -> Vec<LibraryItem> {
    std::fs::read_to_string(library_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_library(v: &[LibraryItem]) -> Result<(), String> {
    let p = library_path();
    ensure_dir(p.parent().unwrap_or(&p))?;
    let s = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    std::fs::write(&p, s).map_err(|e| format!("写入库失败: {e}"))
}

/// 把外部文件导入库
pub fn import_to_library(src: &Path, game: &str) -> Result<LibraryItem, String> {
    if !src.is_file() {
        return Err("文件不存在".to_string());
    }
    let dir = library_dir();
    ensure_dir(&dir)?;
    let name = src
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "trainer.zip".to_string());
    let safe = crate::paths::sanitize_filename(&name);
    let dest = dir.join(&safe);
    std::fs::copy(src, &dest).map_err(|e| format!("复制失败: {e}"))?;
    let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    let item = LibraryItem {
        game: game.to_string(),
        filename: safe,
        path: dest.to_string_lossy().to_string(),
        size,
        source_url: String::new(),
        added_ms: now_ms(),
        game_zh: String::new(),
        page_url: String::new(),
        cover: String::new(),
    };
    let mut lib = load_library();
    lib.retain(|i| i.path != item.path);
    lib.push(item.clone());
    save_library(&lib)?;
    Ok(item)
}

/// 改库条目的显示名 (游戏名)。**不动文件名** —— 改文件名会让已存的路径失效。
pub fn rename_in_library(path: &str, game: &str) -> Result<(), String> {
    let mut lib = load_library();
    let mut hit = false;
    for it in lib.iter_mut() {
        if it.path == path {
            it.game = game.to_string();
            hit = true;
        }
    }
    if !hit {
        return Err("库里找不到这个条目".to_string());
    }
    save_library(&lib)
}

pub fn delete_from_library(path: &str) -> Result<(), String> {
    let p = PathBuf::from(path);
    if p.is_file() {
        std::fs::remove_file(&p).map_err(|e| format!("删除失败: {e}"))?;
    }
    let mut lib = load_library();
    lib.retain(|i| i.path != path);
    save_library(&lib)
}

/// 启动修改器: 若是压缩包则先解压到同名目录, 找到 exe 后启动
pub fn launch_from_library(path: &str) -> Result<String, String> {
    let p = PathBuf::from(path);
    if !p.is_file() {
        return Err("文件不存在".to_string());
    }
    let ext = p
        .extension()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let target = if ext == "exe" {
        p.clone()
    } else if ext == "zip" {
        let dir = p.with_extension("");
        let existed = dir.exists();
        ensure_dir(&dir)?;
        if let Err(e) = crate::paths::extract_zip(&p, &dir) {
            // 解压失败别在库里留一个空目录
            if !existed {
                let _ = std::fs::remove_dir(&dir);
            }
            return Err(e);
        }
        find_exe(&dir).ok_or_else(|| "解压后没有找到 .exe".to_string())?
    } else if ext == "rar" || ext == "7z" {
        // 交给主程序已有的解压能力
        return Err("请先用主程序的解压功能解压该压缩包，或改用 zip 版本".to_string());
    } else {
        p.clone()
    };
    let dir = target.parent().map(|d| d.to_path_buf()).unwrap_or_default();
    // 修改器几乎都要管理员权限 (要改游戏进程内存), 直接 spawn 会撞
    // os error 740 (ERROR_ELEVATION_REQUIRED)。所以先普通启动, 撞上 740 再走
    // ShellExecuteW 的 "runas" 动词 —— 系统会弹一次 UAC。
    match spawn_direct(&target, &dir) {
        Ok(()) => Ok(target.to_string_lossy().to_string()),
        Err(e) if e.raw_os_error() == Some(740) => {
            #[cfg(windows)]
            {
                spawn_elevated(&target, &dir)?;
                Ok(format!("{}（已请求管理员权限）", target.to_string_lossy()))
            }
            #[cfg(not(windows))]
            {
                Err("这个修改器需要管理员权限，请以管理员身份运行本程序".to_string())
            }
        }
        Err(e) => Err(format!("启动失败: {e}")),
    }
}

/// 普通启动 (不弹 UAC)
fn spawn_direct(target: &Path, dir: &Path) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(target);
    cmd.current_dir(dir);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd.spawn().map(|_| ())
}

/// 用 `ShellExecuteW(..., "runas", ...)` 提权启动。返回值 > 32 才算成功。
#[cfg(windows)]
fn spawn_elevated(target: &Path, dir: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    }
    let verb: Vec<u16> = "runas\0".encode_utf16().collect();
    let file = wide(target);
    let params: Vec<u16> = vec![0];
    let cwd = wide(dir);
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            cwd.as_ptr(),
            SW_SHOWNORMAL,
        )
    };
    if (r as isize) <= 32 {
        return Err(format!(
            "这个修改器需要管理员权限，提权启动被拒绝（代码 {}）。可右键本程序「以管理员身份运行」后重试。",
            r as isize
        ));
    }
    Ok(())
}

fn find_exe(dir: &Path) -> Option<PathBuf> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.is_file()
            && p.extension()
                .map(|s| s.to_string_lossy().eq_ignore_ascii_case("exe"))
                .unwrap_or(false)
        {
            return Some(p);
        }
        if p.is_dir() {
            subdirs.push(p);
        }
    }
    for d in subdirs {
        if let Some(p) = find_exe(&d) {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_to_name() {
        assert_eq!(name_from_slug("baldurs-gate-3-trainer"), "Baldurs Gate 3");
        assert_eq!(name_from_slug("elden-ring-trainer"), "Elden Ring");
        assert_eq!(name_from_slug("cyberpunk-2077-trainer"), "Cyberpunk 2077");
        assert_eq!(name_from_slug("weird"), "Weird");
    }

    #[test]
    fn sitemap_parsed_and_filtered() {
        let xml = r#"<?xml version="1.0"?><urlset>
        <url><loc>https://flingtrainer.com/</loc><lastmod>2026-10-01T15:36:02+00:00</lastmod></url>
        <url><loc>https://flingtrainer.com/trainer/elden-ring-trainer/</loc><lastmod>2026-09-30T01:00:00+00:00</lastmod></url>
        <url><loc>https://flingtrainer.com/trainer/hades-2-trainer/</loc><lastmod>2026-10-01T02:00:00+00:00</lastmod></url>
        <url><loc>https://flingtrainer.com/about/</loc><lastmod>2020-01-01T00:00:00+00:00</lastmod></url>
        </urlset>"#;
        let v = parse_sitemap(xml);
        assert_eq!(v.len(), 2);
        // parse_sitemap 保持文档顺序
        assert_eq!(v[0].slug, "elden-ring-trainer");
        assert_eq!(v[1].slug, "hades-2-trainer");
        assert_eq!(v[1].name, "Hades 2");
        // finalize_index 才做倒序 + 去重
        let f = finalize_index(v);
        assert_eq!(f[0].slug, "hades-2-trainer");
        assert_eq!(f[1].slug, "elden-ring-trainer");
        assert!(f[1].url.starts_with("https://flingtrainer.com/trainer/"));
    }

    #[test]
    fn sitemap_extracts_cover_from_first_image() {
        let xml = r#"<?xml version="1.0"?><urlset>
        <url><loc>https://flingtrainer.com/trainer/elden-ring-trainer/</loc>
          <lastmod>2026-09-30T01:00:00+00:00</lastmod>
          <image:image><image:loc>https://flingtrainer.com/wp-content/uploads/2022/02/header-4.jpg</image:loc></image:image>
          <image:image><image:loc>https://flingtrainer.com/wp-content/uploads/2022/02/1-2.png</image:loc></image:image>
        </url>
        </urlset>"#;
        let v = parse_sitemap(xml);
        assert_eq!(v.len(), 1);
        // 第一张图 = og:image = 封面; 第二张是截图, 不取
        assert_eq!(
            v[0].cover,
            "https://flingtrainer.com/wp-content/uploads/2022/02/header-4.jpg"
        );
    }

    #[test]
    fn sitemap_block_is_bounded_by_url_close_tag() {
        // ★ 回归: 旧实现在整段 XML 上找 <loc> 再拿"剩余全文"找图片,
        //   会越过 </url> 把下一页的封面算到自己头上。
        let xml = r#"<urlset>
        <url><loc>https://flingtrainer.com/trainer/no-image-trainer/</loc><lastmod>2026-01-01T00:00:00+00:00</lastmod></url>
        <url><loc>https://flingtrainer.com/trainer/has-image-trainer/</loc><lastmod>2026-01-02T00:00:00+00:00</lastmod>
          <image:image><image:loc>https://x/cover.jpg</image:loc></image:image></url>
        </urlset>"#;
        let v = parse_sitemap(xml);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].slug, "no-image-trainer");
        assert_eq!(v[0].cover, "", "没图的页面不该偷到下一页的封面");
        assert_eq!(v[1].cover, "https://x/cover.jpg");
    }

    #[test]
    fn sitemap_handles_empty_and_garbage() {
        assert!(parse_sitemap("").is_empty());
        assert!(parse_sitemap("<urlset></urlset>").is_empty());
        assert!(parse_sitemap("<url><loc>https://x/a</loc></url>").is_empty());
    }

    #[test]
    fn detail_html_extraction() {
        // 模拟真实页面的关键片段
        let html = r#"<html><body>
        <h1 class="post-title">Elden Ring Trainer</h1>
        <div class="entry-content"><ul><li>Unlimited Health</li><li>Unlimited Stamina</li></ul></div>
        <div class="download-attachments style-table"><table class="da-attachments-table">
        <tr class="zip"><td class="attachment-title">
        <a class="attachment-link" href="https://flingtrainer.com/downloads/AbC123,," title="x">Elden.Ring.v1.12-Trainer.zip</a>
        </td></tr></table></div></body></html>"#;
        let title_re =
            regex::Regex::new(r#"(?is)<h1[^>]*class="[^"]*post-title[^"]*"[^>]*>(.*?)</h1>"#).unwrap();
        assert_eq!(
            strip_tags(&title_re.captures(html).unwrap()[1]).trim(),
            "Elden Ring Trainer"
        );
        let link_re = regex::Regex::new(
            r#"(?is)<a[^>]*class="[^"]*attachment-link[^"]*"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#,
        )
        .unwrap();
        let c = link_re.captures(html).unwrap();
        assert_eq!(&c[1], "https://flingtrainer.com/downloads/AbC123,,");
        assert_eq!(strip_tags(&c[2]).trim(), "Elden.Ring.v1.12-Trainer.zip");
    }

    #[test]
    fn attr_value_order_independent() {
        // 真实页面 href 在 class 之前
        let attrs = r#"href="https://x/d/1,," title="Real.Name.zip" class="attachment-link" target="_self""#;
        assert_eq!(attr_value(attrs, "href"), "https://x/d/1,,");
        assert_eq!(attr_value(attrs, "title"), "Real.Name.zip");
        assert_eq!(attr_value(attrs, "missing"), "");
    }

    #[test]
    fn options_parsed_from_og_description() {
        let txt = "Options Num 1 \u{2013} God Mode/Ignore Hits Num 2 \u{2013} Infinite Health Num 3 \u{2013} Infinite Musou";
        let v = parse_options(txt);
        assert_eq!(v, vec!["God Mode/Ignore Hits", "Infinite Health", "Infinite Musou"]);
    }

    #[test]
    fn options_empty_on_garbage() {
        assert!(parse_options("").is_empty());
        assert!(parse_options("no options here").is_empty());
    }

    #[test]
    fn article_block_scoped() {
        let html = "<html><body><nav><a class=\"attachment-link\" href=\"nav\">x</a></nav><article><a href=\"real\" class=\"attachment-link\">y</a></article></body></html>";
        let b = article_block(html).unwrap();
        assert!(b.contains("real"));
        assert!(!b.contains("nav"));
    }

    #[test]
    fn numeric_entities_decoded() {
        assert_eq!(unescape("Super Damage/One&#046;&#046;&#046;"), "Super Damage/One...");
        assert_eq!(unescape("A&#x2E;B"), "A.B");
    }

    #[test]
    fn sniff_ext_reads_magic_not_filename() {
        // FLiNG 实际发的就是这个: 名字叫 zip, 内容是 PE 可执行文件
        assert_eq!(sniff_ext(b"MZ\x90\x00\x03\x00\x00\x00"), "exe");
        assert_eq!(sniff_ext(b"PK\x03\x04rest"), "zip");
        assert_eq!(sniff_ext(b"PK\x05\x06rest"), "zip");
        assert_eq!(sniff_ext(b"7z\xBC\xAF\x27\x1Crest"), "7z");
        assert_eq!(sniff_ext(b"Rar!\x1A\x07\x00rest"), "rar");
        assert_eq!(sniff_ext(b"<html>oops"), "");
        assert_eq!(sniff_ext(b""), "");
    }

    #[test]
    fn filename_taken_from_redirect_path_param() {
        // flingtrainer 的真实文件名在 path= 查询参数里 (HEAD 没有 Content-Disposition)
        let u = "https://flingtrainer.com/download-trainer.php?path=%2Fwp-content%2Fuploads%2F2020%2F05%2FTerraria.v1.4-v1.4.5.x.Plus.12.Trainer-FLiNG.zip";
        assert_eq!(
            filename_from_url(u).as_deref(),
            Some("Terraria.v1.4-v1.4.5.x.Plus.12.Trainer-FLiNG.zip")
        );
        // 普通地址取最后一段
        assert_eq!(filename_from_url("https://x/y/Mod.jar").as_deref(), Some("Mod.jar"));
        // php 端点 + 没有 path 参数 → 认不出来, 交给调用方兜底
        assert_eq!(filename_from_url("https://x/download-trainer.php?token=abc"), None);
    }

    #[tokio::test]
    async fn steam_cn_name_live() {
        // 真实打一次 Steam 搜索接口, 确认"官方中文名"这条路是通的
        let c = crate::paths::big_http_client();
        match steam_cn_name(&c, "Elden Ring").await {
            Ok(Some(zh)) => assert!(
                zh.chars().any(|ch| ch > '\u{2E80}'),
                "应该拿到中文名, 实际: {zh}"
            ),
            Ok(None) => panic!("Steam 有响应但没搜到 Elden Ring"),
            // Steam 被限流/网络不通时不算失败 —— 压太狠会被它封一段时间
            Err(e) => eprintln!("跳过: Steam 暂不可达 ({e})"),
        }
    }

    #[test]
    fn filename_percent_decoded_twice() {
        // 实测: 服务端双重编码, 解一层后还剩 %20, 落盘就成了 "Ace%20Combat%208..."
        let u = "https://flingtrainer.com/download-trainer.php?path=%2Fwp-content%2Fuploads%2F2026%2F10%2FAce%2520Combat%25208%2520Wings.zip";
        assert_eq!(
            filename_from_url(u).as_deref(),
            Some("Ace Combat 8 Wings.zip")
        );
        // 单层编码也照样对
        let u2 = "https://x/download-trainer.php?path=%2Fup%2FAce%20Combat.zip";
        assert_eq!(filename_from_url(u2).as_deref(), Some("Ace Combat.zip"));
        // 合法的 % 不该被弄坏
        assert_eq!(percent_decode_once("100% Save.zip"), "100% Save.zip");
    }

    #[test]
    fn zh_for_url_ignores_download_tokens() {
        // 库里的 source_url 是**下载令牌地址**, 不是页面地址 —— 拿它当 slug 是错的
        assert_eq!(
            zh_for_url("https://flingtrainer.com/downloads/tKEk43gWiT08Jlo1_HFRSg,,"),
            ""
        );
        assert_eq!(zh_for_url(""), "");
    }

    #[test]
    fn search_term_strips_timestamp_and_trainer() {
        // 站点给重名页面加的时间戳
        assert_eq!(search_term_for("Days Gone Trainer 20210518"), "Days Gone");
        assert_eq!(
            search_term_for("Grand Theft Auto V Trainer 1766066855"),
            "Grand Theft Auto V"
        );
        assert_eq!(
            search_term_for("Elden Ring Shadow Of The Edtree Trainer 1768067282"),
            "Elden Ring Shadow Of The Edtree"
        );
        // 末尾的 Trainers
        assert_eq!(search_term_for("Hogwarts Legacy Trainers"), "Hogwarts Legacy");
        // ★ 真名里的数字不能砍
        assert_eq!(search_term_for("Cyberpunk 2077"), "Cyberpunk 2077");
        assert_eq!(search_term_for("Pc Building Simulator 2"), "Pc Building Simulator 2");
        assert_eq!(search_term_for("Elden Ring"), "Elden Ring");
        // 别把自己清成空串
        assert_eq!(search_term_for("Trainer"), "Trainer");
    }

    #[test]
    fn ascii_paren_stripped() {
        assert_eq!(
            strip_ascii_paren("空战奇兵8 希孚之翼 (ACE COMBAT 8: WINGS OF THEVE)"),
            "空战奇兵8 希孚之翼"
        );
        // 中文括注要保留
        assert_eq!(strip_ascii_paren("巫师3：狂猎（重制版）"), "巫师3：狂猎（重制版）");
        assert_eq!(strip_ascii_paren("艾尔登法环"), "艾尔登法环");
        // 只有括注、没有正文 → 原样返回
        assert_eq!(strip_ascii_paren("(ABC)"), "(ABC)");
    }

    #[test]
    fn force_ext_replaces_or_appends() {
        assert_eq!(force_ext("Terraria.zip", "exe"), "Terraria.exe");
        assert_eq!(force_ext("Terraria.v1.4.Plus.12.Trainer-FLiNG.zip", "exe"),
                   "Terraria.v1.4.Plus.12.Trainer-FLiNG.exe");
        assert_eq!(force_ext("Trainer", "zip"), "Trainer.zip");
        // 点开头的隐藏文件不该被当成"有扩展名"
        assert_eq!(force_ext(".gitignore", "zip"), ".gitignore.zip");
    }

    #[test]
    fn html_unescape_works() {
        assert_eq!(unescape("A &amp; B &#039;x&#039;"), "A & B 'x'");
    }
}

// ============================================================
// Tauri 命令
// ============================================================

fn client() -> reqwest::Client {
    crate::paths::big_http_client()
}

fn require_activated() -> Result<(), String> {
    if crate::licensing::is_activated() {
        Ok(())
    } else {
        Err("未激活授权".to_string())
    }
}

/// 同步索引 (抓 sitemap)
#[tauri::command]
pub async fn tr_sync(app: tauri::AppHandle) -> Result<Vec<TrainerEntry>, String> {
    require_activated()?;
    let c = client();
    let mut list = sync_index(&c).await?;
    apply_names(&mut list);
    save_index(&list)?;
    // 中文名要在后台慢慢补 (758 次查询), 不能卡住同步的返回。
    // 只在还没有缓存时自动跑一次; 之后靠手动「重试中文名」补漏。
    if !names_path().exists() {
        let c2 = c.clone();
        tokio::spawn(async move {
            translate_names(&app, &c2).await;
        });
    }
    Ok(list)
}

/// 手动补齐 / 重试中文名 (只重试之前没查到的)
#[tauri::command]
pub async fn tr_translate_names(app: tauri::AppHandle) -> Result<usize, String> {
    let c = client();
    Ok(translate_names(&app, &c).await)
}

/// 读取已缓存的索引 (没有则返回空, 前端提示去同步)。
///
/// 旧缓存是**没有封面图**的 (cover 字段是后加的) —— 检测到就自动重同步一次,
/// 免得用户看着一屏字母占位符还得自己去点「同步索引」。
#[tauri::command]
pub async fn tr_index(app: tauri::AppHandle) -> Result<Vec<TrainerEntry>, String> {
    let mut cached = load_index();
    let stale = !cached.is_empty() && cached.iter().all(|e| e.cover.is_empty());
    if stale && crate::licensing::is_activated() {
        let c = client();
        if let Ok(mut fresh) = sync_index(&c).await {
            apply_names(&mut fresh);
            let _ = save_index(&fresh);
            return Ok(fresh);
        }
    }
    apply_names(&mut cached);
    // 索引在但中文名缓存没了 (比如清了缓存目录) → 后台补一次
    if !cached.is_empty() && !names_path().exists() && crate::licensing::is_activated() {
        let c = client();
        tokio::spawn(async move {
            translate_names(&app, &c).await;
        });
    }
    Ok(cached)
}

#[tauri::command]
pub async fn tr_detail(url: String) -> Result<TrainerDetail, String> {
    let c = client();
    // ★ 必须单独限时: mc_client 的总超时是 300 秒, 一个页面卡住会让前端
    //   "点了没反应"整整五分钟 (而且旧的 busy 守卫会把整个网格一起锁死)。
    match tokio::time::timeout(std::time::Duration::from_secs(20), fetch_detail(&c, &url)).await {
        Ok(r) => r,
        Err(_) => Err("打开修改器页面超时（20 秒），请重试".to_string()),
    }
}

#[tauri::command]
pub async fn tr_download(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::commands::AppState>,
    url: String,
    game: String,
    page_url: Option<String>,
) -> Result<LibraryItem, String> {
    require_activated()?;
    let c = client();
    download_trainer(
        &app,
        &state.download_tasks,
        &c,
        &url,
        &game,
        &page_url.unwrap_or_default(),
    )
    .await
}

#[tauri::command]
pub fn tr_library() -> Vec<LibraryItem> {
    let mut v = load_library();
    let mut changed = false;
    for it in v.iter_mut() {
        let zh = zh_for_item(it);
        it.game_zh = zh.clone();
        it.cover = cover_for_item(it);
        // 老条目存的是英文名 → 用官方中文名升级一次。
        // 用户手动改过名的不是纯 ASCII, 不会被覆盖。
        if !zh.is_empty() && zh != it.game && it.game.is_ascii() {
            it.game = zh;
            changed = true;
        }
    }
    if changed {
        let _ = save_library(&v);
    }
    v
}

#[tauri::command]
pub fn tr_import(path: String, game: String) -> Result<LibraryItem, String> {
    import_to_library(std::path::Path::new(&path), &game)
}

#[tauri::command]
pub fn tr_delete(path: String) -> Result<(), String> {
    delete_from_library(&path)
}

#[tauri::command]
pub fn tr_launch(path: String) -> Result<String, String> {
    launch_from_library(&path)
}

#[tauri::command]
pub fn tr_rename(path: String, game: String) -> Result<(), String> {
    rename_in_library(&path, &game)
}

/// 把修改器目录加进 Windows Defender 排除项。
///
/// 修改器要改游戏内存, 经常被 Defender 误判成木马直接删掉 —— GCM 有这个助手,
/// 我们按同样的思路自己做一份。`Add-MpPreference` 需要管理员权限, 所以走提权启动
/// (会弹一次 UAC)。
#[tauri::command]
pub fn tr_defender_exclude() -> Result<String, String> {
    let dir = library_dir();
    ensure_dir(&dir)?;
    // 只加一次 —— 每次下载都弹 UAC 太烦, 加过就记个标记直接跳过
    let marker = dir.join(".defender_added");
    if marker.exists() {
        return Ok("已经加过了".to_string());
    }
    let path = dir.to_string_lossy().to_string();
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let cmd = format!(
            "-NoProfile -WindowStyle Hidden -Command \"Add-MpPreference -ExclusionPath '{}'\"",
            path.replace('\'', "''")
        );
        let verb: Vec<u16> = "runas\0".encode_utf16().collect();
        let file: Vec<u16> = "powershell.exe\0".encode_utf16().collect();
        let params: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
        let r = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                params.as_ptr(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        if (r as isize) <= 32 {
            return Err(format!(
                "提权被拒（代码 {}）。可手动把 {} 加进 Defender 的排除项",
                r as isize, path
            ));
        }
        let _ = std::fs::write(&marker, "1");
        return Ok(path);
    }
    #[cfg(not(windows))]
    {
        Ok(path)
    }
}

#[tauri::command]
pub fn tr_open_folder() -> Result<(), String> {
    let d = library_dir();
    ensure_dir(&d)?;
    crate::browser::open_external(&d.to_string_lossy())
}
