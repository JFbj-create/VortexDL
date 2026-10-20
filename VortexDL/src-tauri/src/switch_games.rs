//! Switch 游戏库：把「槿篱游戏盒」后端与 NS711 两个来源合并成一份本地目录。
//!
//! # 数据来源
//!
//! | 来源 | 怎么拿 | 说明 |
//! |---|---|---|
//! | 槿篱（hzhd.steamcdk.xin） | `POST /index/Appcp/s17443723143643`，`gamename=%` 分页 | `%` 是 SQL LIKE 通配，一次能翻全量 1.5 万条，按 `child_plat=="Switch"` 过滤 |
//! | NS711 | `https://www.ns711.com/page/N` 列表页 | WordPress；卡片 `a.ns-game`；`is-sticky` 是置顶，按要求跳过 |
//!
//! 两边的分类合并去重；同名游戏合并成一条，优先保留 NS711 的封面/简介。
//!
//! # 下载
//!
//! · NS711 的下载按钮是一个 `data-resource` 令牌，POST `admin-ajax.php`
//!   （`action=skqr_private_resolve`）换出 `pan.quark.cn/s/...` 分享链接，
//!   再交给本软件已有的网盘解析（`netdisk::list_share` / `resolve`）。
//! · 槿篱的下载目录 id 在详情接口的 `alone_file` 里，分卷直链由
//!   `1.117.71.138:8813` 现签（15 分钟有效），这里只在真正下载时签名。
//!
//! # 模拟器
//!
//! 不内置、不打包模拟器（Switch 模拟器的授权与分发风险很高，且需要用户自己的密钥）。
//! 这里只做：扫描软件目录下的 `emulator` 里已安装的模拟器 → 用游戏文件一键启动。
//! 没有模拟器时给 NS711 的「模拟器」分类页链接，让用户自己装。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const NS_BASE: &str = "https://www.ns711.com";
const MM_API: &str = "https://hzhd.steamcdk.xin";
const MM_SEARCH_PATH: &str = "/index/Appcp/s17443723143643";
const MM_DETAILS_PATH: &str = "/index/Appcp/s17443723152613";
const MM_DL: &str = "http://1.117.71.138:8813";
const MM_SECRET: &str = "Lu3aJo5in8vsDu7sj5wm";
const MM_DLID: &str = "381";

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

pub fn switch_dir() -> PathBuf {
    PathBuf::from(r"D:\game\switch")
}

/// ★ 2026-10-19：压缩包下载到**独立目录**，解压后再把游戏本体放进模拟器目录。
///   原来直接压进 `D:\game\switch`，一个包解出 3444 个文件（switch/outrun2/…），
///   整个目录一片乱，模拟器也扫不出游戏。
pub fn download_dir() -> PathBuf {
    PathBuf::from(r"D:\game\switch-downloads")
}

fn catalog_path() -> PathBuf {
    crate::paths::data_dir().join("switch").join("catalog.json")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SwitchGame {
    /// `ns:<postid>` / `mm:<gameid>`
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub title_en: String,
    #[serde(default)]
    pub cats: Vec<String>,
    #[serde(default)]
    pub cover: String,
    #[serde(default)]
    pub size: String,
    #[serde(default)]
    pub date: String,
    /// ns711 / mm / both
    #[serde(default)]
    pub source: String,
    /// NS711 详情页
    #[serde(default)]
    pub page: String,
    #[serde(default)]
    pub desc: String,
    /// 安装说明。「[移植游戏]」那批条目在资源站的「详情」标签里只写了怎么
    /// 解压、怎么拖进机器，那不是游戏简介 —— 单独存这里，详情页单列一块。
    #[serde(default)]
    pub install: String,
    /// 已经抓过详情页、确认站点没写简介（避免每次打开都白抓一遍）
    #[serde(default)]
    pub desc_missing: bool,
    /// 已解析的夸克分享链接（缓存）
    #[serde(default)]
    pub quark: String,
    #[serde(default)]
    pub mm_id: i64,
    /// 槿篱的下载目录 id（懒加载）
    #[serde(default)]
    pub dirid: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub ns_count: usize,
    #[serde(default)]
    pub mm_count: usize,
    #[serde(default)]
    pub games: Vec<SwitchGame>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogStats {
    pub updated_at: i64,
    pub total: usize,
    pub ns_count: usize,
    pub mm_count: usize,
    pub cats: Vec<(String, usize)>,
}

pub fn load_catalog() -> Catalog {
    if let Some(c) = std::fs::read_to_string(catalog_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Catalog>(&s).ok())
    {
        if !c.games.is_empty() {
            return c;
        }
    }
    // ★ 首次安装：data 目录还没有目录文件 → 用安装包自带的种子目录（exe 同级）
    let seed = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("switch_catalog.json")))
        .unwrap_or_else(|| PathBuf::from("switch_catalog.json"));
    std::fs::read_to_string(seed)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_catalog(c: &Catalog) -> Result<(), String> {
    let p = catalog_path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("创建目录失败: {}", e))?;
    }
    let s = serde_json::to_string(c).map_err(|e| e.to_string())?;
    std::fs::write(&p, s).map_err(|e| format!("写入失败: {}", e))
}

pub fn stats() -> CatalogStats {
    let c = load_catalog();
    CatalogStats {
        updated_at: c.updated_at,
        total: c.games.len(),
        ns_count: c.ns_count,
        mm_count: c.mm_count,
        cats: categories_of(&c.games),
    }
}

pub fn categories_of(games: &[SwitchGame]) -> Vec<(String, usize)> {
    let mut m: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for g in games {
        for c in &g.cats {
            let c = c.trim();
            if c.is_empty() {
                continue;
            }
            *m.entry(c.to_string()).or_insert(0) += 1;
        }
    }
    let mut v: Vec<(String, usize)> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

fn core_title(s: &str) -> String {
    let mut t = s.trim();
    // 去掉开头的 [移植游戏] / 【汉化】 这类标签
    while t.starts_with('[') || t.starts_with('【') {
        if let Some(i) = t.find(']').or_else(|| t.find('】')) {
            t = t[i + 1..].trim_start();
        } else {
            break;
        }
    }
    // 去掉 | 后面的英文名
    if let Some(i) = t.find('|') {
        t = &t[..i];
    }
    t.trim().to_string()
}

fn norm_title(s: &str) -> String {
    core_title(s).chars()
        .filter(|c| !c.is_whitespace() && !"，。！？：；、·-—_|/\\()（）[]【】".contains(*c))
        .collect::<String>()
        .to_lowercase()
}

// ============================================================
// NS711 列表
// ============================================================

/// 旧版（2026-10-19 及以前）抓进缓存里的「简介」其实是网盘按钮文字，
/// 命中这些特征就当作垃圾、重新抓一次。
fn desc_is_junk(d: &str) -> bool {
    let t = d.trim();
    if t.chars().count() < 20 {
        return true;
    }
    if t.contains("暂未补充详情介绍") {
        return true;
    }
    let bad = ["版本信息与网盘资源", "夸克网盘下载", "百度网盘下载", "下载前，请先扫码转存"];
    bad.iter().any(|b| t.contains(b)) && !t.contains("安装") && !t.contains("游戏")
}

/// 这段文字到底是「游戏简介」还是「怎么把游戏装进机器」？
///
/// 资源站的 `[移植游戏]` 那批条目，详情标签里只写了
/// 「1.下载压缩包解压后 Switch 文件夹覆盖到卡根目录 2.用 DBI 把 nsp 拖入 5 盘…」，
/// 老实现直接把它当简介显示，用户看到的「介绍」就是安装步骤。
fn is_install_steps(d: &str) -> bool {
    let t = d.trim();
    if t.is_empty() {
        return false;
    }
    let low = t.to_lowercase();
    let markers = [
        "解压", "覆盖到", "拖入", "拖到", "读卡器", "dbi", "按 a", "按a", "前端", "根目录",
        "复制到", "存到", "安装到", "放进去", "放入游戏",
    ];
    let hits = markers.iter().filter(|m| low.contains(*m)).count();
    let steps = t.matches("1.").count()
        + t.matches("2.").count()
        + t.matches("3.").count()
        + t.matches("①").count()
        + t.matches("1、").count();
    hits >= 3 || (hits >= 2 && steps >= 2)
}

#[derive(Default)]
struct NsDesc {
    intro: String,
    install: String,
}

/// 把「详情」面板的正文拆成 简介 / 安装说明 两段。
fn split_ns_desc(text: &str) -> NsDesc {
    let t = text.trim();
    if t.is_empty() {
        return NsDesc::default();
    }
    if is_install_steps(t) {
        return NsDesc {
            intro: String::new(),
            install: t.chars().take(1200).collect(),
        };
    }
    for h in ["安装说明", "安装方法", "安装教程", "安装步骤", "如何安装"] {
        if let Some(i) = t.find(h) {
            let (a, b) = t.split_at(i);
            if a.trim().chars().count() >= 30 {
                return NsDesc {
                    intro: a.trim().chars().take(1200).collect(),
                    install: b.trim().chars().take(1200).collect(),
                };
            }
        }
    }
    NsDesc {
        intro: t.chars().take(1200).collect(),
        install: String::new(),
    }
}

/// 从 NS711 详情页抽「详情」面板正文。
///
/// 页面结构（2026-10-20 实测 `117813.html`）：
/// ```html
/// <div class="ns-entry-content">
///   ...下载按钮 / 二维码弹窗（skqr-*）...
///   <h2>游戏简介</h2>
///   <section class="nstp-game-detail-tabs">
///     <div ... data-nstp-panel="detail">  ← 真正的介绍文字 + 截图在这里
/// ```
/// ★ 旧实现从 `ns-entry-content` 开始只取到第一个 `</div>`，正好截在
///   「版本信息与网盘资源」那一行 —— 用户看到的简介就变成了
///   「版本信息与网盘资源（最新版本：1.0.0）夸克网盘下载」。改成优先取
///   `data-nstp-panel="detail"`，取不到再退回 og:description 兜底。
fn extract_ns_panel(html: &str) -> String {
    // 1) 详情标签面板
    if let Some(i) = html.find(r#"data-nstp-panel="detail""#) {
        let tail = &html[i..];
        // 面板本体从这一行的 '>' 之后开始
        let body_start = tail.find('>').map(|x| x + 1).unwrap_or(0);
        let body = &tail[body_start..];
        // 结束于下一个面板（或整段结束）
        let end = body
            .find(r#"data-nstp-panel=""#)
            .or_else(|| body.find("</section>"))
            .unwrap_or(body.len());
        let text = clean_ns_html(&body[..end]);
        if text.chars().count() >= 20 {
            return text;
        }
    }
    // 2) og:description（站点自己写的一句话介绍，比空着好）
    if let Some(c) = regex::Regex::new(r#"<meta property="og:description" content="([^"]*)""#)
        .unwrap()
        .captures(html)
    {
        let t = strip_tags(c.get(1).unwrap().as_str());
        if !t.is_empty() && !t.contains("暂未补充详情介绍") {
            return t;
        }
    }
    String::new()
}


/// 去掉脚本/样式/iframe/图片，压成一段可读文本（保留换行便于前端 <br>）
fn clean_ns_html(block: &str) -> String {
    let mut s = block.to_string();
    for pat in [
        r"(?is)<script[^>]*>.*?</script>",
        r"(?is)<style[^>]*>.*?</style>",
        r"(?is)<iframe[^>]*>.*?</iframe>",
        r"(?is)<figure[^>]*>.*?</figure>",
        r"(?is)<svg[^>]*>.*?</svg>",
        r"(?i)<br\s*/?>",
        r"(?i)</p>",
        r"(?i)</h[1-6]>",
        r"(?i)</li>",
    ] {
        let rep = if pat.starts_with("(?i)<br") || pat.contains("</p") || pat.contains("</h")
            || pat.contains("</li")
        {
            "\n"
        } else {
            ""
        };
        if let Ok(re) = regex::Regex::new(pat) {
            s = re.replace_all(&s, rep).to_string();
        }
    }
    let text = strip_tags(&s);
    // ★ 有的面板正文是 JS 字符串（`&lt;div id=...`）转义过的：strip_tags 之后
    //   实体解码又会把它变回 <div>，所以再剥一次
    let text = strip_tags(&text);
    // 合并空行 + 去行首尾空白
    let mut out = String::new();
    let mut blank = 0;
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(l);
    }
    out.trim().to_string()
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&nbsp;", " ")
        .replace("&#8217;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .trim()
        .to_string()
}

async fn get(url: &str) -> Result<String, String> {
    let r = crate::netdisk::http()
        .get(url)
        .header("User-Agent", UA)
        .send()
        .await
        .map_err(|e| format!("请求失败: {}", e))?;
    r.text().await.map_err(|e| format!("读取失败: {}", e))
}

fn parse_ns_cards(html: &str) -> Vec<SwitchGame> {
    let mut out = Vec::new();
    let re_title = regex::Regex::new(r"(?s)<h3>(.*?)</h3>").unwrap();
    let re_img = regex::Regex::new(r#"<img[^>]*src="([^"]+)""#).unwrap();
    let re_tag = regex::Regex::new(r#"<span class="ns-game-tag"[^>]*>([^<]+)</span>"#).unwrap();
    let re_size = regex::Regex::new(r#"<span class="ns-game-tag is-size"[^>]*>([^<]+)</span>"#).unwrap();
    let re_date = regex::Regex::new(r#"<time[^>]*>([^<]+)</time>"#).unwrap();
    let re_href = regex::Regex::new(r#"href="(https://www\.ns711\.com/(\d+)\.html)""#).unwrap();

    for chunk in html.split(r#"<a class="ns-game""#).skip(1) {
        let chunk = match chunk.split("</a>").next() {
            Some(c) => c,
            None => continue,
        };
        if chunk.contains("is-sticky") {
            continue; // 置顶的不要
        }
        let href = match re_href.captures(chunk) {
            Some(c) => (c.get(1).unwrap().as_str().to_string(), c.get(2).unwrap().as_str().to_string()),
            None => continue,
        };
        let title = re_title
            .captures(chunk)
            .map(|c| strip_tags(c.get(1).unwrap().as_str()))
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let cover = re_img
            .captures(chunk)
            .map(|c| c.get(1).unwrap().as_str().to_string())
            .unwrap_or_default();
        let size = re_size
            .captures(chunk)
            .map(|c| strip_tags(c.get(1).unwrap().as_str()))
            .unwrap_or_default();
        let date = re_date
            .captures(chunk)
            .map(|c| strip_tags(c.get(1).unwrap().as_str()))
            .unwrap_or_default();
        let mut cats: Vec<String> = Vec::new();
        for c in re_tag.captures_iter(chunk) {
            let t = strip_tags(c.get(1).unwrap().as_str());
            if !t.is_empty() && !cats.contains(&t) {
                cats.push(t);
            }
        }
        out.push(SwitchGame {
            id: format!("ns:{}", href.1),
            title,
            cats,
            cover,
            size,
            date,
            source: "ns711".into(),
            page: href.0,
            ..Default::default()
        });
    }
    out
}

/// NS711 列表全量抓取。
///
/// ★★ 2026-10-20 提速：原来是**一页一页串行**等（300 页 ≈ 好几分钟）。
///   改成「先并发探到底有多少页，再分批并发抓」：
///     1. 并发抓前 24 页，看哪一页开始空（正常站点的页数远少于 300）；
///     2. 剩下的页按 8 路并发抓完。
///   同样是真实请求，只是不再一条一条排队。
async fn crawl_ns711(progress: &(dyn Fn(&str, usize, usize) + Send + Sync)) -> Result<Vec<SwitchGame>, String> {
    use futures::stream::{self, StreamExt};

    const BURST: usize = 24;   // 先并发探这么多页
    const CONC: usize = 8;     // 后续并发度

    fn page_url(page: usize) -> String {
        if page == 1 {
            format!("{}/", NS_BASE)
        } else {
            format!("{}/page/{}", NS_BASE, page)
        }
    }

    progress("NS711 探测页数", 0, 0);
    let first: Vec<(usize, Option<String>)> = stream::iter(1..=BURST)
        .map(|p| async move {
            let r = get(&page_url(p)).await.ok();
            (p, r)
        })
        .buffer_unordered(6)
        .collect()
        .await;

    let mut pages: Vec<(usize, String)> = first
        .into_iter()
        .filter_map(|(p, h)| h.map(|h| (p, h)))
        .filter(|(p, h)| !parse_ns_cards(h).is_empty())
        .collect();
    pages.sort_by_key(|(p, _)| *p);

    // 第 1 页都抓不到 = 站点不通
    if pages.is_empty() {
        return Err("连不上 NS711（第 1 页没拿到内容）".to_string());
    }

    let mut all: Vec<SwitchGame> = Vec::new();
    for (_, html) in &pages {
        all.extend(parse_ns_cards(html));
    }
    progress(
        &format!("NS711 已抓 {} 页", pages.len()),
        all.len(),
        0,
    );

    // 后面的页继续并发抓，直到连续两页都空
    let mut next = BURST + 1;
    let mut empty_streak = 0usize;
    while next <= 300 && empty_streak < 2 && all.len() <= 20000 {
        let batch: Vec<usize> = (next..(next + CONC).min(301)).collect();
        let got: Vec<(usize, Option<String>)> = stream::iter(batch.clone())
            .map(|p| async move {
                let r = get(&page_url(p)).await.ok();
                (p, r)
            })
            .buffer_unordered(CONC)
            .collect()
            .await;
        let mut batch_pages: Vec<(usize, String)> = got
            .into_iter()
            .filter_map(|(p, h)| h.map(|h| (p, h)))
            .filter(|(_, h)| !parse_ns_cards(h).is_empty())
            .collect();
        batch_pages.sort_by_key(|(p, _)| *p);
        if batch_pages.is_empty() {
            empty_streak += 1;
        } else {
            empty_streak = 0;
            for (_, html) in &batch_pages {
                all.extend(parse_ns_cards(html));
            }
        }
        progress(
            &format!("NS711 第 {} 页", next),
            all.len(),
            0,
        );
        next += CONC;
    }
    Ok(all)
}

// ============================================================
// 槿篱（mmese）列表
// ============================================================

fn mm_auth(path: &str) -> String {
    let ts = crate::paths::now_ms();
    let rnd = (ts as u64 % 1_000_000) as u32;
    let raw = format!("{}-{}-{}-{}-{}", path, ts, rnd, MM_DLID, MM_SECRET);
    let digest = crate::netdisk::xunlei::md5_hex(raw.as_bytes());
    format!("{}-{}-{}-{}", ts, rnd, MM_DLID, digest)
}

async fn mm_post(path: &str, mut body: serde_json::Value) -> Result<serde_json::Value, String> {
    if let Some(o) = body.as_object_mut() {
        o.insert("auth_key".into(), serde_json::json!(mm_auth(path)));
    }
    let r = crate::netdisk::http()
        .post(format!("{}{}", MM_API, path))
        .header("User-Agent", UA)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("槿篱接口请求失败: {}", e))?;
    let t = r.text().await.unwrap_or_default();
    serde_json::from_str(&t).map_err(|_| format!("槿篱接口返回异常: {}", t.chars().take(200).collect::<String>()))
}

async fn crawl_mm(progress: &(dyn Fn(&str, usize, usize) + Send + Sync)) -> Result<Vec<SwitchGame>, String> {
    let mut out = Vec::new();
    let mut page = 1u64;
    let mut total = 0u64;
    loop {
        progress(&format!("槿篱第 {} 页", page), out.len(), total as usize);
        let body = serde_json::json!({
            "token": "", "Uuid": "", "page": page, "sign": "", "num": 100,
            "gamename": "%", "platform": "", "flag": "", "type": "",
            "ispricetop": 0, "onemoney": "", "no_alone": 0, "tb_label": "",
            "istb": 1, "channel_url": "", "dlid": MM_DLID, "svip": 0, "is_t3": 0,
            "category": "", "rkcw": 0, "is_zhuan": 0
        });
        let v = mm_post(MM_SEARCH_PATH, body).await?;
        let list = v.get("list").and_then(|x| x.as_array()).cloned().unwrap_or_default();
        if list.is_empty() {
            break;
        }
        if total == 0 {
            total = v
                .get("num")
                .and_then(|x| x.as_array())
                .and_then(|a| a.first())
                .and_then(|x| x.get("count(*)"))
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
        }
        for it in &list {
            if it.get("child_plat").and_then(|x| x.as_str()).unwrap_or("") != "Switch" {
                continue;
            }
            let gameid = it.get("gameid").and_then(|x| x.as_i64()).unwrap_or(0);
            if gameid == 0 {
                continue;
            }
            let title = it
                .get("gamename_zh")
                .and_then(|x| x.as_str())
                .or_else(|| it.get("gamename").and_then(|x| x.as_str()))
                .unwrap_or("")
                .to_string();
            if title.is_empty() {
                continue;
            }
            let mut cats: Vec<String> = Vec::new();
            if let Some(labels) = it.get("gamelabel").and_then(|x| x.as_str()) {
                for t in labels.split(|c| c == ',' || c == '，' || c == '|') {
                    let t = t.trim();
                    if !t.is_empty() && !cats.iter().any(|x| x == t) {
                        cats.push(t.to_string());
                    }
                }
            }
            let cover = it
                .get("ex_myGame")
                .and_then(|x| x.as_str())
                .filter(|s| s.starts_with("http"))
                .or_else(|| it.get("ex_banner").and_then(|x| x.as_str()).filter(|s| s.starts_with("http")))
                .unwrap_or("")
                .to_string();
            out.push(SwitchGame {
                id: format!("mm:{}", gameid),
                title,
                title_en: it.get("gamename_en").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                cats,
                cover,
                size: it.get("game_area").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                date: it.get("indate").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                source: "mm".into(),
                mm_id: gameid,
                ..Default::default()
            });
        }
        let got = list.len() as u64;
        if total > 0 && page * 100 >= total {
            break;
        }
        if got < 100 {
            break;
        }
        page += 1;
        if page > 400 {
            break;
        }
    }
    Ok(out)
}

// ============================================================
// 合并 + 刷新
// ============================================================

fn merge(mut ns: Vec<SwitchGame>, mm: Vec<SwitchGame>) -> Vec<SwitchGame> {
    let mut by_key: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, g) in ns.iter().enumerate() {
        by_key.insert(norm_title(&g.title), i);
    }
    for g in mm {
        let key = norm_title(&g.title);
        if let Some(&i) = by_key.get(&key) {
            // 同名：NS711 那条补上槿篱 id / 英文名 / 分类
            let e = &mut ns[i];
            e.mm_id = g.mm_id;
            e.source = "both".into();
            if e.title_en.is_empty() {
                e.title_en = g.title_en;
            }
            for c in g.cats {
                if !e.cats.contains(&c) {
                    e.cats.push(c);
                }
            }
        } else {
            by_key.insert(key, ns.len());
            ns.push(g);
        }
    }
    ns
}

/// 刷新目录。
///
/// ★★ 2026-10-20 改成**增量合并**：
///   老实现每次都从零重建目录，于是「已经花时间抓到的简介/封面/分类/夸克链接」
///   全部被清空，下次打开详情又要重新抓一遍 —— 这正是用户说的「游戏资源就不做
///   缓存了吗」。现在按 id 继承旧条目上的这些字段。
pub async fn refresh(progress: &(dyn Fn(&str, usize, usize) + Send + Sync)) -> Result<CatalogStats, String> {
    let old = load_catalog();
    let old_by_id: std::collections::HashMap<String, SwitchGame> = old
        .games
        .into_iter()
        .map(|g| (g.id.clone(), g))
        .collect();

    let ns = crawl_ns711(progress).await?;
    let ns_count = ns.len();
    let mm = crawl_mm(progress).await.unwrap_or_default();
    let mm_count = mm.len();
    let mut games = merge(ns, mm);

    // 把旧条目上「花时间抓来的」字段搬回来（列表页本身没有这些数据）
    let mut kept = 0usize;
    for g in games.iter_mut() {
        if let Some(o) = old_by_id.get(&g.id) {
            if g.desc.is_empty() && !o.desc.is_empty() {
                g.desc = o.desc.clone();
                kept += 1;
            }
            if g.install.is_empty() {
                g.install = o.install.clone();
            }
            if g.cover.is_empty() {
                g.cover = o.cover.clone();
            }
            if g.cats.is_empty() {
                g.cats = o.cats.clone();
            }
            if g.quark.is_empty() {
                g.quark = o.quark.clone();
            }
            if g.dirid.is_empty() {
                g.dirid = o.dirid.clone();
            }
            g.desc_missing = o.desc_missing;
        }
    }
    if kept > 0 {
        crate::app_logger::log_command(
            "switch",
            &format!("refresh 继承旧详情字段 {} 条（不重复抓）", kept),
        );
    }

    let c = Catalog {
        updated_at: crate::paths::now_ms(),
        ns_count,
        mm_count,
        games,
    };
    save_catalog(&c)?;
    Ok(stats())
}

// ============================================================
// 列表 / 详情
// ============================================================

pub fn list(cat: &str, query: &str, offset: usize, limit: usize) -> (Vec<SwitchGame>, usize) {
    let c = load_catalog();
    let q = query.trim().to_lowercase();
    let cat = cat.trim();
    let mut hit: Vec<&SwitchGame> = c
        .games
        .iter()
        .filter(|g| {
            if !cat.is_empty() && cat != "全部" && !g.cats.iter().any(|x| x == cat) {
                return false;
            }
            if !q.is_empty() {
                let hay = format!("{} {} {}", g.title, g.title_en, g.cats.join(" ")).to_lowercase();
                if !hay.contains(&q) {
                    return false;
                }
            }
            true
        })
        .collect();
    hit.sort_by(|a, b| b.date.cmp(&a.date));
    let total = hit.len();
    let page: Vec<SwitchGame> = hit.into_iter().skip(offset).take(limit).cloned().collect();
    (page, total)
}

/// 详情：简介 / 安装说明 / 封面 / 分类 / 夸克分享链接（按需解析并写回目录）
pub async fn detail(id: &str) -> Result<SwitchGame, String> {
    let mut cat = load_catalog();
    let idx = cat
        .games
        .iter()
        .position(|g| g.id == id)
        .ok_or_else(|| "目录里没有这个游戏（先刷新一次）".to_string())?;
    let mut g = cat.games[idx].clone();

    // ① 老缓存清洗：简介字段里存的是安装步骤 → 搬到 install，简介留空重抓
    if is_install_steps(&g.desc) || desc_is_junk(&g.desc) {
        if is_install_steps(&g.desc) && g.install.is_empty() {
            g.install = g.desc.trim().chars().take(1200).collect();
        }
        g.desc.clear();
    }

    // ② NS711 页面：补简介 / 安装说明 / 封面 / 分类 / 夸克
    let need_page = g.source.contains("ns711")
        && !g.page.is_empty()
        && ((g.desc.is_empty() && !g.desc_missing)
            || g.install.is_empty()
            || g.cover.is_empty()
            || g.cats.is_empty()
            || g.quark.is_empty());
    if need_page {
        if let Ok(html) = get(&g.page).await {
            if g.desc.is_empty() || g.install.is_empty() {
                let parts = split_ns_desc(&extract_ns_panel(&html));
                if g.install.is_empty() && !parts.install.is_empty() {
                    g.install = parts.install;
                }
                if g.desc.is_empty() && !parts.intro.is_empty() {
                    g.desc = parts.intro;
                }
            }
            if g.cover.is_empty() {
                if let Some(c) = regex::Regex::new(r#"<meta property="og:image" content="([^"]+)""#)
                    .unwrap()
                    .captures(&html)
                {
                    g.cover = c.get(1).unwrap().as_str().to_string();
                }
            }
            if g.cats.is_empty() {
                let re = regex::Regex::new(r#"rel="tag">([^<]+)<"#).unwrap();
                for c in re.captures_iter(&html) {
                    let t = strip_tags(c.get(1).unwrap().as_str());
                    if !t.is_empty() && !g.cats.contains(&t) {
                        g.cats.push(t);
                    }
                }
            }
            if g.quark.is_empty() {
                if let Some(c) = regex::Regex::new(r#"data-resource="([0-9a-fA-F]{16,64})""#)
                    .unwrap()
                    .captures(&html)
                {
                    let token = c.get(1).unwrap().as_str().to_string();
                    if let Ok(url) = resolve_skqr(&token).await {
                        g.quark = url;
                    }
                }
            }
        }
    }

    // ③ 资源站没写简介（[移植游戏] 那批只有安装步骤）→ 用槿篱的详情补一段真实介绍
    if g.desc.is_empty() && g.mm_id > 0 {
        if let Some(t) = mm_intro(g.mm_id).await {
            g.desc = t;
        }
    }
    // 抓过页面还是没有简介：记下来，别每次打开详情都再抓一遍
    if g.desc.is_empty() && !g.desc_missing {
        g.desc_missing = true;
    } else if !g.desc.is_empty() {
        g.desc_missing = false;
    }

    cat.games[idx] = g.clone();
    let _ = save_catalog(&cat);
    Ok(g)
}

/// 槿篱详情里的游戏介绍。`gameinfo` 是短介绍，`infodetail` 是长一点的图文
/// 描述（带 <p> 标签），两个都空就返回 None。
async fn mm_intro(gameid: i64) -> Option<String> {
    let v = mm_post(
        MM_DETAILS_PATH,
        serde_json::json!({"gameid": gameid, "platform": "Alone", "dlid": MM_DLID, "Uuid": "", "sign": ""}),
    )
    .await
    .ok()?;
    let short = v.get("gameinfo").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let long = clean_ns_html(v.get("infodetail").and_then(|x| x.as_str()).unwrap_or(""));
    let text = if short.chars().count() >= 40 {
        short
    } else if !long.is_empty() {
        long
    } else {
        short
    };
    if text.trim().is_empty() {
        None
    } else {
        Some(text.chars().take(1200).collect())
    }
}

async fn resolve_skqr(token: &str) -> Result<String, String> {
    let body = format!("action=skqr_private_resolve&resource={}", token);
    let r = crate::netdisk::http()
        .post(format!("{}/wp-admin/admin-ajax.php", NS_BASE))
        .header("User-Agent", UA)
        .header("Content-Type", "application/x-www-form-urlencoded; charset=UTF-8")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("解析下载令牌失败: {}", e))?;
    let v: serde_json::Value = r.json().await.map_err(|e| format!("解析返回失败: {}", e))?;
    v.pointer("/data/url")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "站点没有返回下载链接".to_string())
}

// ============================================================
// 下载
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchFile {
    pub name: String,
    pub url: String,
    pub size: u64,
    pub headers: Vec<(String, String)>,
}

pub async fn download(id: &str) -> Result<Vec<SwitchFile>, String> {
    let g = detail(id).await?;

    // 路线 1：NS711 的夸克分享
    if !g.quark.is_empty() {
        let link = crate::netdisk::link::recognize(&g.quark)
            .ok_or_else(|| format!("不认识的分享链接: {}", g.quark))?;
        let entries = crate::netdisk::list_share(&link, "").await?;
        let files: Vec<crate::netdisk::PanEntry> = entries.into_iter().filter(|e| !e.is_dir).collect();
        if files.is_empty() {
            return Err("夸克分享里没有可下载的文件".into());
        }
        let resolved = crate::netdisk::resolve(&link, "", &files).await?;
        return Ok(resolved
            .into_iter()
            .map(|r| SwitchFile {
                name: r.name,
                url: r.url,
                size: r.size,
                headers: r.headers,
            })
            .collect());
    }

    // 路线 2：槿篱分卷
    if g.mm_id > 0 {
        let dirid = if !g.dirid.is_empty() {
            g.dirid.clone()
        } else {
            let v = mm_post(
                MM_DETAILS_PATH,
                serde_json::json!({"gameid": g.mm_id, "platform": "Alone", "dlid": MM_DLID, "Uuid": "", "sign": ""}),
            )
            .await?;
            let mut found = String::new();
            for key in ["alone_file", "alone_file1", "sky_yun", "alone_file2", "sky_yun1"] {
                if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
                    let parts: Vec<&str> = s.split(',').collect();
                    if parts.len() >= 3 {
                        found = parts[2].trim().to_string();
                        break;
                    }
                }
            }
            if found.is_empty() {
                return Err("槿篱详情里没有下载目录（资源可能已下架）".into());
            }
            found
        };
        let list = get(&format!("{}/geturl/{}", MM_DL, dirid)).await?;
        let v: serde_json::Value = serde_json::from_str(&list).map_err(|_| "槿篱文件列表解析失败".to_string())?;
        let maps = v.get("maps").and_then(|x| x.as_array()).cloned().unwrap_or_default();
        if maps.is_empty() {
            return Err("槿篱文件列表为空（资源可能已失效）".into());
        }
        let mut out = Vec::new();
        for m in &maps {
            let name = m.get("utf_name").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let fid = m.get("fileId").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if name.is_empty() || fid.is_empty() {
                continue;
            }
            let size = m.get("fileSize").and_then(|x| x.as_u64()).unwrap_or(0);
            let signed = get(&format!("{}/geturl/{}?fileid={}", MM_DL, dirid, fid)).await?;
            let sv: serde_json::Value = serde_json::from_str(&signed).map_err(|_| "槿篱签名返回异常".to_string())?;
            let raw = sv
                .pointer("/maps/0/url")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let url = deobfuscate(raw).ok_or_else(|| "槿篱直链解扰失败".to_string())?;
            out.push(SwitchFile {
                name,
                url,
                size,
                headers: vec![],
            });
        }
        if out.is_empty() {
            return Err("槿篱没有可下载的分卷".into());
        }
        return Ok(out);
    }

    Err("这条记录没有可用的下载来源".into())
}

fn deobfuscate(raw: &str) -> Option<String> {
    for drop in [vec![2usize, 5], vec![2, 4]] {
        let kept: String = raw
            .chars()
            .enumerate()
            .filter(|(i, _)| !drop.contains(i))
            .map(|(_, c)| c)
            .collect();
        let mut s = kept;
        while s.len() % 4 != 0 {
            s.push('=');
        }
        if let Ok(bytes) = base64_decode(&s) {
            if let Ok(t) = String::from_utf8(bytes) {
                if t.starts_with("http://") || t.starts_with("https://") {
                    return Some(t);
                }
            }
        }
    }
    None
}

fn base64_decode(s: &str) -> Result<Vec<u8>, ()> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = match T.iter().position(|&x| x == c) {
            Some(v) => v as u32,
            None => return Err(()),
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

// ============================================================
// 模拟器（实现见 emulator 模块）
// ============================================================

pub fn emu_scan() -> Option<PathBuf> {
    crate::emulator::scan().map(|(p, _)| p)
}


pub fn emu_launch(game_path: &str) -> Result<String, String> {
    let exe = emu_scan().ok_or_else(|| {
        "没有找到 Switch 模拟器。请把模拟器解压到 D:\\game\\switch\\emulator（或先下载模拟器）".to_string()
    })?;
    if !std::path::Path::new(game_path).exists() {
        return Err(format!("游戏文件不存在: {}", game_path));
    }
    std::process::Command::new(&exe)
        .arg(game_path)
        .current_dir(exe.parent().unwrap_or(std::path::Path::new(".")))
        .spawn()
        .map_err(|e| format!("启动模拟器失败: {}", e))?;
    Ok(format!("{} {}", exe.display(), game_path))
}


/// 在 `D:\game\switch` 下找某个游戏已经下载的 ROM 文件（.nsp/.xci/.nsz/.nca）
pub fn find_local(title: &str) -> Option<String> {
    let root = switch_dir();
    let want = norm_title(title);
    for g in downloaded() {
        let dn = norm_title(&g.title);
        if dn.contains(&want) || want.contains(&dn) {
            return Some(g.file);
        }
    }
    // 兜底：再扫一遍根目录（含 .nro）
    find_rom_in(&root).map(|(f, _)| f)
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadedGame {
    pub title: String,
    pub dir: String,
    pub file: String,
    pub size: u64,
    /// 游戏目录里的图标/封面图（没有就空，前端用占位）
    #[serde(default)]
    pub cover: String,
}

/// 扫描 `D:\game\switch` 下已下载的游戏。
///
/// ★ 2026-10-19 重写：以前只看「一层子目录里的第一个 .nsp/.xci」，而 NS711 的包解出来
///   是 `switch/outrun2/OutRun2006-NX.nro` 这种多层结构 → 一个都扫不到（模拟器页空白）。
///   现在递归找，并把 **.nro（自制程序/移植游戏）** 也算作可启动项。
pub fn downloaded() -> Vec<DownloadedGame> {
    let root = switch_dir();
    let mut out: Vec<DownloadedGame> = Vec::new();
    if !root.exists() {
        return out;
    }
    // 顶层目录 = 一部游戏（模拟器目录除外）
    let rd = match std::fs::read_dir(&root) {
        Ok(r) => r,
        Err(_) => return out,
    };
    // ★ 2026-10-20: 同一个游戏可能同时出现在「旧目录」和「新目录」里
    //   （例如 超越2006 既在 `D:\game\switch\超越2006 海岸到海岸`，
    //   又在下载解压出的 `D:\game\switch\[移植游戏]超越2006：海岸到海岸|OutRun...`）。
    //   以前两个都会列出来 → 模拟器页出现两张一样的卡片。
    //   这里按 ROM 文件名 + 体积去重，保留体积大的那份。
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let name = p.file_name().and_then(|x| x.to_str()).unwrap_or("").to_string();
        if name.eq_ignore_ascii_case("emulator") {
            continue;
        }
        if let Some((file, size)) = find_rom_in(&p) {
            // 图标优先级：游戏目录里的图 > 目录索引里的网络封面（NS711 抓的）
            let cover = match find_cover_in(&p) {
                Some(c) => c,
                None => catalog_cover_for(&name),
            };
            let key = format!(
                "{}|{}",
                Path::new(&file)
                    .file_name()
                    .and_then(|x| x.to_str())
                    .unwrap_or("")
                    .to_lowercase(),
                size
            );
            let item = DownloadedGame {
                title: name,
                dir: p.to_string_lossy().to_string(),
                file,
                size,
                cover,
            };
            match seen.get(&key) {
                Some(&i) => {
                    if out[i].title.len() < item.title.len() {
                        out[i] = item;
                    }
                }
                None => {
                    seen.insert(key, out.len());
                    out.push(item);
                }
            }
        }
    }
    out.sort_by(|a, b| a.title.cmp(&b.title));
    out
}

fn rom_rank(name: &str) -> Option<u8> {
    let n = name.to_lowercase();
    if n.ends_with(".nro") { return Some(0); }   // 自制/移植游戏，优先
    if n.ends_with(".xci") { return Some(1); }
    if n.ends_with(".nsp") { return Some(2); }
    if n.ends_with(".nsz") { return Some(3); }
    if n.ends_with(".nca") { return Some(4); }
    None
}

/// 按目录名去目录索引里找封面 URL（NS711 封面），用于没有自带图标的游戏
fn catalog_cover_for(title: &str) -> String {
    let want = norm_title(title);
    if want.is_empty() {
        return String::new();
    }
    let cat = load_catalog();
    for g in &cat.games {
        if g.cover.is_empty() {
            continue;
        }
        let n = norm_title(&g.title);
        if n.is_empty() {
            continue;
        }
        if n.contains(&want) || want.contains(&n) {
            return g.cover.clone();
        }
    }
    String::new()
}

/// 在游戏目录里找一张能当封面的图（icon.png / logo.* / *.jpg 优先，其次任意图片）
fn find_cover_in(dir: &Path) -> Option<String> {
    let mut best: Option<(u8, PathBuf)> = None;
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    let mut seen = 0usize;
    while let Some((d, depth)) = stack.pop() {
        if seen > 3000 {
            break;
        }
        let rd = std::fs::read_dir(&d).ok()?;
        for e in rd.flatten() {
            seen += 1;
            let p = e.path();
            if p.is_dir() {
                if depth < 4 {
                    stack.push((p, depth + 1));
                }
                continue;
            }
            let n = p.file_name().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
            let rank = if n.starts_with("icon") && (n.ends_with(".png") || n.ends_with(".jpg")) {
                0
            } else if n.starts_with("logo") && (n.ends_with(".png") || n.ends_with(".jpg")) {
                1
            } else if n.ends_with(".png") && depth == 0 {
                2
            } else if n.ends_with(".jpg") || n.ends_with(".jpeg") {
                3
            } else {
                continue;
            };
            if best.as_ref().map(|(br, _)| rank < *br).unwrap_or(true) {
                best = Some((rank, p));
            }
        }
    }
    best.map(|(_, p)| p.to_string_lossy().to_string())
}

/// 在目录里递归找最像"游戏本体"的文件：优先 .nro，其次 .xci/.nsp，
/// 同一优先级里取最大的那个（补丁/DLC 的 nsp 通常很小）。
fn find_rom_in(dir: &Path) -> Option<(String, u64)> {
    let mut best: Option<(u8, u64, std::path::PathBuf)> = None;
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    let mut seen = 0usize;
    while let Some((d, depth)) = stack.pop() {
        if seen > 4000 {
            break;
        }
        let rd = match std::fs::read_dir(&d) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.flatten() {
            seen += 1;
            let p = e.path();
            if p.is_dir() {
                if depth < 5 {
                    stack.push((p, depth + 1));
                }
                continue;
            }
            let n = p.file_name().and_then(|x| x.to_str()).unwrap_or("");
            if let Some(rank) = rom_rank(n) {
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                let better = match &best {
                    None => true,
                    Some((br, bs, _)) => rank < *br || (rank == *br && size > *bs),
                };
                if better {
                    best = Some((rank, size, p));
                }
            }
        }
    }
    best.map(|(_, size, p)| (p.to_string_lossy().to_string(), size))
}

/// 解压 Switch 压缩包到 `D:\game\switch\<游戏名>`，并返回找到的 ROM 文件
///
/// 下载目录是 `D:\game\switch-downloads`（压缩包放这里，模拟器不会去扫），
/// 解压完把「包含 ROM 的那一层」整体搬进模拟器目录，所以 `switch/outrun2/xxx.nro`
/// 这种结构也能直接启动。
pub fn install_archive(archive: &str, title: &str) -> Result<String, String> {
    let arc = Path::new(archive);
    if !arc.exists() {
        return Err(format!("压缩包不存在: {}", archive));
    }
    let dest = switch_dir().join(sanitize(title));
    std::fs::create_dir_all(&dest).map_err(|e| format!("创建目录失败: {}", e))?;

    let r = crate::extractor::extract(arc, &dest, None, |_p, _b, _t, _f| {});
    if !r.success {
        return Err(format!("解压失败: {}", r.error));
    }
    // 解压完在目标目录里找 ROM；找不到就说清楚（有的包是模拟器/补丁，不是游戏）
    match find_rom_in(&dest) {
        Some((file, _size)) => {
            crate::app_logger::log_command(
                "switch",
                &format!("install_archive ok title={} rom={}", title, file),
            );
            Ok(file)
        }
        None => Err(format!(
            "解压完成，但目录里没有 .nsp/.xci/.nro 游戏文件（可能是补丁或模拟器资源）: {}",
            dest.display()
        )),
    }
}

pub fn sanitize(s: &str) -> String {
    const BAD: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|', '\n', '\r', '\t'];
    let out: String = s.chars().map(|c| if BAD.contains(&c) { '_' } else { c }).collect();
    let t = out.trim().to_string();
    if t.is_empty() {
        "switch_game".to_string()
    } else {
        t.chars().take(80).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_tags_works() {
        assert_eq!(strip_tags("<p>hello &amp; <b>world</b></p>"), "hello & world");
    }

    #[test]
    fn norm_title_merges_punct() {
        assert_eq!(norm_title("塞尔达传说：旷野之息"), norm_title("塞尔达传说 旷野之息"));
    }

    #[test]
    fn base64_roundtrip_known() {
        let v = base64_decode("aHR0cHM6Ly9leGFtcGxlLmNvbQ==").unwrap();
        assert_eq!(String::from_utf8(v).unwrap(), "https://example.com");
    }

    /// 详情页正文必须取到 data-nstp-panel="detail" 里的内容，
    /// 而不是 ns-entry-content 开头那行「版本信息与网盘资源」。
    /// 而且「怎么装游戏」要落到 install，不能当简介。
    #[test]
    fn ns_desc_uses_detail_panel() {
        let html = r#"<div class="ns-entry-content">
            <h2>版本信息与网盘资源（最新版本：1.0.0）</h2>
            <a class="skqr-btn" data-resource="abc">夸克网盘下载</a>
            <h2>游戏简介</h2>
          </div>
          <section class="nstp-game-detail-tabs">
            <div id="t-detail" class="nstp-detail-tab-panel" data-nstp-panel="detail">
              <p class="ns-game-video"><iframe src="https://player.bilibili.com/x"></iframe></p>
              <p>1.下载压缩包解压后把 Switch 文件夹覆盖到卡根目录</p>
              <p>2.回到桌面选择游戏按 <b>A</b> 键打开</p>
              <figure><img src="https://x/y.webp"></figure>
            </div>
            <div id="t-help" data-nstp-panel="help-4"><div>点击标签后加载内容</div></div>
          </section>"#;
        let parts = split_ns_desc(&extract_ns_panel(html));
        assert!(parts.intro.is_empty(), "安装步骤不能当简介: {}", parts.intro);
        assert!(parts.install.contains("覆盖到卡根目录"), "got: {}", parts.install);
        assert!(!parts.install.contains("版本信息与网盘资源"), "got: {}", parts.install);
        assert!(!parts.install.contains("夸克网盘下载"), "got: {}", parts.install);
        assert!(!parts.install.contains("点击标签后加载内容"), "got: {}", parts.install);
    }

    /// 正常游戏页：一段真简介，必须留在 intro，install 为空
    #[test]
    fn ns_desc_keeps_real_intro() {
        let html = r#"<section>
            <div data-nstp-panel="detail">
              <p>《白之旅》在唯美世界中体验狼崽与幼鹿的诗意故事。因为突如其来的暴雪而迷失方向，它们必须学会彼此信赖。</p>
              <p>纯手工打造的画面，全程没有一句对白。</p>
            </div>
            <div data-nstp-panel="help-4">.</div>
          </section>"#;
        let parts = split_ns_desc(&extract_ns_panel(html));
        assert!(parts.intro.contains("狼崽与幼鹿"), "got: {}", parts.intro);
        assert!(parts.install.is_empty(), "got: {}", parts.install);
    }

    /// 「该文章暂未补充详情介绍。」是占位文字，不能当简介
    #[test]
    fn placeholder_desc_rejected() {
        assert!(desc_is_junk("该文章暂未补充详情介绍。"));
    }

    /// 老缓存里存的是网盘按钮文字 → 必须判成垃圾并重抓
    #[test]
    fn junk_desc_detected() {
        assert!(desc_is_junk("版本信息与网盘资源（最新版本：1.0.0） 夸克网盘下载 夸克网盘下载"));
        assert!(!desc_is_junk("1.下载压缩包解压后把 Switch 文件夹覆盖到卡根目录，然后按 A 键打开"));
    }
}
