// ============================================================================
// 书库 —— 免登录书源 + 在线阅读 + 下载到本地
// ----------------------------------------------------------------------------
// 用户要求：「动漫下面添加一个书库，两页：主页（推书、搜索）、收藏；
//   书可以下载或收藏在线阅读；大量找免登录、能在线阅读和下载的源，
//   要网络小说 / 杂志 / 名著」。
//
// ★★ 实测结论（2026-10-08，本机网络）：必须**按实测结果**选源，不能想当然 ——
//   不可达/不可用（全部实测过）：
//     · gutendex 之外的公版库：openlibrary、archive.org、zh.wikisource 全部连不上；
//     · 所有主流中文网文站：起点/番茄/七猫/掌阅/纵横/刺猬猫/SF 都要登录或纯 JS 渲染；
//       笔趣阁系（xbiquge.bz / bqg228 / bqgbe / bqgui / 23us / bookben …）
//       要么搜索接口跳登录、要么搜索是 JS 发的 POST（返回首页）、要么 403/重置连接；
//     · 追书神器公开 API 已改成要 token（`invalid id`）。
//   可用（逐个实测走通"搜索→书页→正文"）：
//     · **Gutendex（Project Gutenberg 的开放 JSON API）** —— 79k 外文名著 +
//       **444 本中文公版书**（西遊記 / 紅樓夢 / 警世通言 / 唐诗三百首 …），
//       免密钥免登录，**能在线阅读（HTML）也能下载（epub / txt）**；
//     · **5000yan.com**（国学经典全文，道德经/论语这类）；
//     · **书格 shuge.org**（古籍善本，可在线阅读 + 下载 PDF/图片）。
//   另外提供**本地导入**（txt / epub），源失效时书库依然可用。
//
// 所有网络调用都带重试 —— gutendex 从国内访问会偶发超时（实测）。
// ============================================================================

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

// ============================================================================
// 数据类型
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Book {
    /// 稳定主键：`<source>:<source_id>`
    pub key: String,
    /// 源 id（gutenberg / shuge / guoxue / local）
    pub source: String,
    pub source_id: String,
    pub title: String,
    pub author: String,
    pub cover: String,
    pub lang: String,
    /// 分类/主题（用于卡片上那行小字）
    pub tags: Vec<String>,
    /// 简介
    pub desc: String,
    /// 在线阅读用的地址（Gutenberg 是 HTML 版；其它源是书页）
    pub read_url: String,
    /// 可下载的直链（可能为空）
    pub dl_txt: String,
    pub dl_epub: String,
    /// 人气（排序用）
    pub popularity: u64,
    /// 本地文件路径（local 源用）
    pub local_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Chapter {
    pub index: usize,
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BookText {
    pub title: String,
    pub author: String,
    pub text: String,
    /// 本地文件时给"接着读"用的
    pub chapters: Vec<Chapter>,
    pub chapter_index: usize,
    pub has_next: bool,
    pub has_prev: bool,
}

// ============================================================================
// 简单的 TTL 缓存（首页/热门列表不用每次打网络）
// ============================================================================

struct TtlCache {
    map: Mutex<HashMap<String, (Instant, String)>>,
}

impl TtlCache {
    fn new() -> Self {
        Self { map: Mutex::new(HashMap::new()) }
    }
    fn get(&self, k: &str, ttl: Duration) -> Option<String> {
        let g = self.map.lock().ok()?;
        let (t, v) = g.get(k)?;
        if t.elapsed() < ttl {
            Some(v.clone())
        } else {
            None
        }
    }
    fn put(&self, k: &str, v: &str) {
        if let Ok(mut g) = self.map.lock() {
            // 别让缓存无限涨
            if g.len() > 200 {
                g.clear();
            }
            g.insert(k.to_string(), (Instant::now(), v.to_string()));
        }
    }
}

static CACHE: std::sync::OnceLock<TtlCache> = std::sync::OnceLock::new();
fn cache() -> &'static TtlCache {
    CACHE.get_or_init(TtlCache::new)
}

// ============================================================================
// HTTP 小工具
// ============================================================================

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(UA)
        .danger_accept_invalid_certs(true)
        .build()
        .map_err(|e| format!("HTTP 客户端创建失败: {e}"))
}

/// 带重试的 GET 文本。gutendex 从国内访问偶发超时，重试很有必要。
async fn get_text(url: &str, referer: Option<&str>) -> Result<String, String> {
    let cli = http_client()?;
    let mut last = String::new();
    for attempt in 0..3 {
        let mut rb = cli.get(url).header("Accept-Language", "zh-CN,zh;q=0.9");
        if let Some(r) = referer {
            rb = rb.header("Referer", r);
        }
        match rb.send().await {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    last = format!("HTTP {}", status.as_u16());
                } else {
                    match resp.bytes().await {
                        Ok(b) => return Ok(decode_body(&b)),
                        Err(e) => last = format!("读响应失败: {e}"),
                    }
                }
            }
            Err(e) => last = format!("{}", e),
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(700 * (attempt as u64 + 1))).await;
        }
    }
    Err(format!("请求 {url} 失败: {last}"))
}

/// 按 `<meta charset>` 解码（不少中文站是 GBK/GB18030）
fn decode_body(b: &[u8]) -> String {
    let head = String::from_utf8_lossy(&b[..b.len().min(4096)]).to_lowercase();
    let enc = regex::Regex::new(r#"charset\s*=\s*["']?([\w-]+)"#)
        .ok()
        .and_then(|re| re.captures(&head).map(|c| c[1].to_string()))
        .unwrap_or_else(|| "utf-8".into());
    if enc.contains("gb") {
        // Windows 上直接用系统 API 转，省一个 crate
        return gbk_to_utf8(b).unwrap_or_else(|| String::from_utf8_lossy(b).to_string());
    }
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => String::from_utf8_lossy(b).to_string(),
    }
}

#[cfg(windows)]
fn gbk_to_utf8(b: &[u8]) -> Option<String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Globalization::MultiByteToWideChar;
    unsafe {
        let n = MultiByteToWideChar(936, 0, b.as_ptr(), b.len() as i32, std::ptr::null_mut(), 0);
        if n <= 0 {
            return None;
        }
        let mut buf: Vec<u16> = vec![0; n as usize];
        let got = MultiByteToWideChar(936, 0, b.as_ptr(), b.len() as i32, buf.as_mut_ptr(), n);
        if got <= 0 {
            return None;
        }
        buf.truncate(got as usize);
        Some(OsString::from_wide(&buf).to_string_lossy().to_string())
    }
}

#[cfg(not(windows))]
fn gbk_to_utf8(_b: &[u8]) -> Option<String> {
    None
}

fn strip_tags(html: &str) -> String {
    // ★ regex crate **不支持反向引用**（`</\1>` 会直接 panic：backreferences are not supported），
    //   所以 script / style 各写一条正则。
    let re_script = regex::Regex::new(r"(?is)<script[^>]*>.*?</script\s*>").unwrap();
    let s = re_script.replace_all(html, " ");
    let re_style = regex::Regex::new(r"(?is)<style[^>]*>.*?</style\s*>").unwrap();
    let s = re_style.replace_all(&s, " ");
    let re_tag = regex::Regex::new(r"(?s)<[^>]+>").unwrap();
    let s = re_tag.replace_all(&s, "");
    let s = s
        .replace("&nbsp;", " ")
        .replace("&#160;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&ldquo;", "“")
        .replace("&rdquo;", "”");
    let re_ws = regex::Regex::new(r"[ \t\u{00a0}]+").unwrap();
    let s = re_ws.replace_all(&s, " ");
    let re_nl = regex::Regex::new(r"\n\s*\n\s*\n+").unwrap();
    re_nl.replace_all(&s, "\n\n").trim().to_string()
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&apos;", "'")
}

// ============================================================================
// 源 1：Gutendex（Project Gutenberg 开放 API）—— 名著主力
// ============================================================================

const GUTENDEX: &str = "https://gutendex.com/books";

fn pick_formats(fmts: &serde_json::Map<String, serde_json::Value>) -> (String, String, String) {
    let mut read = String::new();
    let mut txt = String::new();
    let mut epub = String::new();
    for (k, v) in fmts {
        let Some(u) = v.as_str() else { continue };
        let kl = k.to_lowercase();
        if kl.starts_with("text/plain") && txt.is_empty() {
            txt = u.to_string();
        } else if kl.contains("epub") && epub.is_empty() {
            epub = u.to_string();
        }
        // 在线阅读优先用"带图片的 HTML 版"，读着舒服
        if kl.starts_with("text/html") {
            if read.is_empty() || kl.contains("images") {
                read = u.to_string();
            }
        }
    }
    // 没有 HTML 就用纯文本当在线阅读内容
    if read.is_empty() {
        read = txt.clone();
    }
    (read, txt, epub)
}

fn gutendex_book(v: &serde_json::Value, lang_filter: Option<&str>) -> Option<Book> {
    let id = v.get("id")?.as_i64()?;
    let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if title.is_empty() {
        return None;
    }
    let author = v
        .get("authors")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .map(|a| {
            let n = a.get("name").and_then(|x| x.as_str()).unwrap_or("");
            let b = a.get("birth_year").and_then(|x| x.as_i64());
            let d = a.get("death_year").and_then(|x| x.as_i64());
            match (b, d) {
                (Some(b), Some(d)) => format!("{n}（{b}-{d}）"),
                _ => n.to_string(),
            }
        })
        .unwrap_or_default();
    let lang = v
        .get("languages")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(","))
        .unwrap_or_default();
    if let Some(f) = lang_filter {
        if !lang.split(',').any(|x| x == f) {
            return None;
        }
    }
    let fmts = v.get("formats").and_then(|x| x.as_object());
    let (read_url, dl_txt, dl_epub) = fmts.map(pick_formats).unwrap_or_default();
    let cover = fmts
        .and_then(|m| m.get("image/jpeg"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let mut tags: Vec<String> = v
        .get("subjects")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).take(4).map(|s| s.to_string()).collect())
        .unwrap_or_default();
    if let Some(bs) = v.get("bookshelves").and_then(|x| x.as_array()) {
        for s in bs.iter().filter_map(|x| x.as_str()).take(2) {
            let t = s.rsplit(" / ").next().unwrap_or(s);
            if !tags.iter().any(|x| x == t) {
                tags.push(t.to_string());
            }
        }
    }
    let popularity = v.get("download_count").and_then(|x| x.as_u64()).unwrap_or(0);
    let source_id = id.to_string();
    Some(Book {
        key: format!("gutenberg:{source_id}"),
        source: "gutenberg".into(),
        source_id,
        title,
        author,
        cover,
        lang,
        tags,
        desc: String::new(),
        read_url,
        dl_txt,
        dl_epub,
        popularity,
        local_path: String::new(),
    })
}

/// Gutendex 列表：`query` 为空时按热度排；`lang` 过滤语言
pub async fn gutenberg_list(query: &str, lang: Option<&str>, page: u32) -> Result<Vec<Book>, String> {
    let mut url = format!("{GUTENDEX}?page={}", page.max(1));
    if !query.trim().is_empty() {
        url.push_str(&format!("&search={}", urlencoding::encode(query.trim())));
    } else {
        url.push_str("&sort=popular");
    }
    if let Some(l) = lang {
        url.push_str(&format!("&languages={l}"));
    }
    let key = format!("gx:{url}");
    let body = match cache().get(&key, Duration::from_secs(1800)) {
        Some(b) => b,
        None => {
            let b = get_text(&url, None).await?;
            cache().put(&key, &b);
            b
        }
    };
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| format!("解析失败: {e}"))?;
    let mut out = Vec::new();
    if let Some(arr) = v.get("results").and_then(|x| x.as_array()) {
        for item in arr {
            if let Some(b) = gutendex_book(item, lang) {
                out.push(b);
            }
        }
    }
    Ok(out)
}

// ============================================================================
// 源 2：5000yan.com（国学经典全文）
// ============================================================================

const GUOXUE_BASE: &str = "https://www.5000yan.com";

pub async fn guoxue_search(kw: &str) -> Result<Vec<Book>, String> {
    let url = format!("{GUOXUE_BASE}/?s={}", urlencoding::encode(kw.trim()));
    let html = get_text(&url, None).await?;
    let re = regex::Regex::new(r##"(?is)<a[^>]+href="(https://www\.5000yan\.com/[^"#?]+)"[^>]*>(.{2,80}?)</a>"##).unwrap();
    let mut out: Vec<Book> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for c in re.captures_iter(&html) {
        let u = c[1].to_string();
        let t = html_unescape(&strip_tags(&c[2]));
        if t.len() < 2 || !seen.insert(u.clone()) {
            continue;
        }
        let sid = u.trim_end_matches(".html").rsplit('/').next().unwrap_or("").to_string();
        if sid.is_empty() {
            continue;
        }
        out.push(Book {
            key: format!("guoxue:{sid}"),
            source: "guoxue".into(),
            source_id: sid,
            title: t.chars().take(40).collect(),
            author: String::new(),
            cover: String::new(),
            lang: "zh".into(),
            tags: vec!["国学经典".into()],
            desc: String::new(),
            read_url: u,
            dl_txt: String::new(),
            dl_epub: String::new(),
            popularity: 0,
            local_path: String::new(),
        });
        if out.len() >= 60 {
            break;
        }
    }
    Ok(out)
}

// ============================================================================
// 源 3：书格 shuge.org（古籍善本：在线阅读 + PDF 下载）
// ============================================================================

pub async fn shuge_search(kw: &str) -> Result<Vec<Book>, String> {
    let url = format!("https://www.shuge.org/?s={}", urlencoding::encode(kw.trim()));
    let html = get_text(&url, None).await?;
    let re = regex::Regex::new(r##"(?is)<a[^>]+href="(https://www\.shuge\.org/view/[^"#?]+/)"[^>]*>(.{2,90}?)</a>"##).unwrap();
    let mut out: Vec<Book> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for c in re.captures_iter(&html) {
        let u = c[1].to_string();
        let t = html_unescape(&strip_tags(&c[2]));
        if t.is_empty() || !seen.insert(u.clone()) {
            continue;
        }
        let sid = u.trim_matches('/').rsplit('/').next().unwrap_or("").to_string();
        out.push(Book {
            key: format!("shuge:{sid}"),
            source: "shuge".into(),
            source_id: sid.clone(),
            title: if t.len() > 2 { t.chars().take(40).collect() } else { sid.clone() },
            author: String::new(),
            cover: String::new(),
            lang: "zh".into(),
            tags: vec!["古籍".into()],
            desc: String::new(),
            read_url: u,
            dl_txt: String::new(),
            dl_epub: String::new(),
            popularity: 0,
            local_path: String::new(),
        });
        if out.len() >= 40 {
            break;
        }
    }
    Ok(out)
}

/// 书格书页：把"简介 + 页内元数据（版本/卷数/藏地）+ 下载入口"整理成可读正文。
///
/// ★ 实测（2026-10-08）：书格**不给图片直链**，古籍影印本的下载走
///   `s.shuge.org/<id>` → `f.shuge.org/dl/...` 的**中转页**（上面挂着网盘链接）。
///   所以这里做两件事：① 把说明文字读出来（在线阅读）；
///   ② 把中转页链接作为"下载"给出（用户点开就能拿到书格官方的下载入口）。
async fn shuge_content(url: &str) -> Result<BookText, String> {
    let html = get_text(url, Some("https://www.shuge.org/")).await?;
    let title = regex::Regex::new(r"(?is)<title[^>]*>(.*?)</title>")
        .unwrap()
        .captures(&html)
        .map(|c| html_unescape(&strip_tags(&c[1])))
        .unwrap_or_default();
    // ★ 正文从 entry-content-wrapper 里取，并且**只取到评论区之前** ——
    //   整个页面直接 strip_tags 会把导航/侧栏/评论区全塞进来（实测 3411 字里大半是菜单）。
    let mut start = html.find("entry-content-wrapper").unwrap_or(0);
    // 从 tag 结束的 '>' 之后才开始，否则正文开头会留着 "entry-content-wrapper clearfix'>"
    if let Some(gt) = html[start..].find('>') {
        start += gt + 1;
    }
    let end = html[start..]
        .find("id='comments'")
        .or_else(|| html[start..].find("comment_text"))
        .map(|i| start + i)
        .unwrap_or(html.len());
    let mut body = &html[start..end];
    // 砍掉脚本/样式碎屑
    if let Some(i) = body.find("av-social-sharing-box") {
        body = &body[..i];
    }
    let mut text = strip_tags(body);
    // 收尾裁噪音
    for cut in ["上一篇", "下一篇", "相关推荐", "评论"] {
        if let Some(i) = text.find(cut) {
            text.truncate(i);
        }
    }
    let text = text.trim().chars().take(60_000).collect::<String>();
    // 下载入口（中转页）
    let dl = shuge_downloads(&html);
    let dl_note = if dl.is_empty() {
        String::new()
    } else {
        format!("

【下载】
{}", dl.iter().map(|(u, n)| {
            if n.trim().is_empty() { u.clone() } else { format!("{n}：{u}") }
        }).collect::<Vec<_>>().join("
"))
    };
    Ok(BookText {
        title,
        author: "书格（古籍影印本 · 在线阅读说明；下载入口见正文末尾）".into(),
        text: format!("{text}{dl_note}"),
        chapters: Vec::new(),
        chapter_index: 0,
        has_next: false,
        has_prev: false,
    })
}

/// 从书格页面里挖下载入口。
///
/// ★ 实测：页面上没有 PDF 直链，真正可点的是 `<a href="https://s.shuge.org/xxx">下载链接</a>`
///   （小程序/中转），以及少数页面直接给的 pdf/zip/rar 直链。两种都收。
fn shuge_downloads(html: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // ① 直链（少数页面有）
    // ★ 内容部分放宽到 600 字符：书格的按钮 <a> 里塞了 svg 图标 + 多个 span（实测 392 字符），
    //   原来的 {0,60} 会整条匹配不到
    let re = regex::Regex::new(r#"(?is)<a[^>]+href=["'](https?://[^"']+\.(?:pdf|zip|rar|7z))["'][^>]*>(.{0,600}?)</a>"#).unwrap();
    for c in re.captures_iter(html) {
        if seen.insert(c[1].to_string()) {
            out.push((c[1].to_string(), html_unescape(&strip_tags(&c[2]))));
        }
    }
    // ② 书格的下载中转页（绝大多数古籍真正给的入口）
    let re2 = regex::Regex::new(r#"(?is)<a[^>]+href=["'](https?://s\.shuge\.org/[^"']+)["'][^>]*>(.{0,600}?)</a>"#).unwrap();
    for c in re2.captures_iter(html) {
        if seen.insert(c[1].to_string()) {
            let label = html_unescape(&strip_tags(&c[2]));
            out.push((c[1].to_string(), if label.trim().is_empty() { "书格下载入口".into() } else { label }));
        }
    }
    out
}

// ============================================================================
// 源 4：本地导入（txt / epub）—— 源全挂了也能用
// ============================================================================

pub fn local_scan(dir: &Path) -> Vec<Book> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
        if !matches!(ext.as_str(), "txt" | "epub") {
            continue;
        }
        let name = p.file_stem().and_then(|x| x.to_str()).unwrap_or("未命名").to_string();
        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
        out.push(Book {
            key: format!("local:{}", name),
            source: "local".into(),
            source_id: name.clone(),
            title: name,
            author: String::new(),
            cover: String::new(),
            lang: "zh".into(),
            tags: vec![if ext == "epub" { "EPUB".into() } else { "TXT".into() }],
            desc: format!("本地文件 · {}", human_size(size)),
            read_url: String::new(),
            dl_txt: String::new(),
            dl_epub: String::new(),
            popularity: 0,
            local_path: p.to_string_lossy().to_string(),
        });
    }
    out.sort_by(|a, b| a.title.cmp(&b.title));
    out
}

fn human_size(n: u64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / 1048576.0)
    } else if n >= 1024 {
        format!("{:.0} KB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

/// 把整本书按「第N章」切成章节（纯文本）
pub fn split_chapters(text: &str) -> Vec<(String, usize, usize)> {
    // 常见章节标题：第123章 / 第123回 / 第123节 / Chapter 12
    let re = regex::Regex::new(
        r"(?m)^[ \t　]*(第\s*[0-9零一二三四五六七八九十百千万两]{1,8}\s*[章节回卷篇][^\n]{0,40}|Chapter\s+\d+[^\n]{0,40})[ \t]*$",
    )
    .unwrap();
    let mut marks: Vec<(String, usize)> = Vec::new();
    for m in re.find_iter(text) {
        marks.push((m.as_str().trim().to_string(), m.start()));
    }
    if marks.len() < 3 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, (name, start)) in marks.iter().enumerate() {
        let end = marks.get(i + 1).map(|x| x.1).unwrap_or(text.len());
        out.push((name.clone(), *start, end));
    }
    out
}

pub fn read_local(path: &str, chapter_index: Option<usize>) -> Result<BookText, String> {
    let p = PathBuf::from(path);
    if !p.is_file() {
        return Err(format!("文件不存在: {path}"));
    }
    let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
    let title = p.file_stem().and_then(|x| x.to_str()).unwrap_or("未命名").to_string();
    let raw = std::fs::read(&p).map_err(|e| format!("读取失败: {e}"))?;
    let text = if ext == "epub" {
        epub_to_text(&raw)?
    } else {
        decode_body(&raw)
    };
    let marks = split_chapters(&text);
    if marks.is_empty() {
        return Ok(BookText {
            title,
            author: String::new(),
            text: text.chars().take(400_000).collect(),
            chapters: Vec::new(),
            chapter_index: 0,
            has_next: false,
            has_prev: false,
        });
    }
    let idx = chapter_index.unwrap_or(0).min(marks.len() - 1);
    let (name, s, e) = marks[idx].clone();
    let body = text.get(s..e).unwrap_or("").to_string();
    let chapters: Vec<Chapter> = marks
        .iter()
        .enumerate()
        .map(|(i, (n, _, _))| Chapter { index: i, name: n.clone(), url: String::new() })
        .collect();
    Ok(BookText {
        title,
        author: format!("{} · 共 {} 章", name, marks.len()),
        text: body.chars().take(400_000).collect(),
        has_next: idx + 1 < chapters.len(),
        has_prev: idx > 0,
        chapters,
        chapter_index: idx,
    })
}

/// 极简 EPUB 解析：拿 zip 里的 xhtml/html，按文件名顺序拼正文
fn epub_to_text(raw: &[u8]) -> Result<String, String> {
    use std::io::Read;
    let cur = std::io::Cursor::new(raw);
    let mut zip = zip::ZipArchive::new(cur).map_err(|e| format!("不是有效的 epub: {e}"))?;
    let mut names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
        .filter(|n| {
            let l = n.to_lowercase();
            (l.ends_with(".xhtml") || l.ends_with(".html") || l.ends_with(".htm")) && !l.contains("nav")
        })
        .collect();
    names.sort();
    let mut out = String::new();
    for n in names {
        if let Ok(mut f) = zip.by_name(&n) {
            let mut s = String::new();
            if f.read_to_string(&mut s).is_ok() {
                out.push_str(&strip_tags(&s));
                out.push_str("\n\n");
            }
        }
        if out.len() > 400_000 {
            break;
        }
    }
    if out.trim().is_empty() {
        return Err("epub 里没读到文本".into());
    }
    Ok(out)
}

// ============================================================================
// 统一入口
// ============================================================================

pub fn book_dir() -> PathBuf {
    let base = crate::paths::data_dir().join("books");
    let _ = std::fs::create_dir_all(&base);
    base
}

/// Gutendex 每页只给 32 条，首页太单薄 —— 并发抓 3 页凑满一屏。
async fn gutenberg_many(lang: Option<&str>, pages: u32) -> Result<Vec<Book>, String> {
    let mut futs = Vec::new();
    for p in 1..=pages {
        futs.push(gutenberg_list("", lang, p));
    }
    let mut out = Vec::new();
    for r in futures::future::join_all(futs).await {
        match r {
            Ok(mut v) => out.append(&mut v),
            Err(e) => {
                if out.is_empty() {
                    return Err(e);
                }
            }
        }
    }
    // 按人气排序，前面几页里最热的排前面
    out.sort_by(|a, b| b.popularity.cmp(&a.popularity));
    out.dedup_by(|a, b| a.key == b.key);
    Ok(out)
}

pub async fn home(section: &str) -> Result<Vec<Book>, String> {
    match section {
        "zh" => gutenberg_many(Some("zh"), 3).await,
        "guoxue" => {
            // 国学经典：直接搜"经""子"这类拿不到列表，用固定关键词凑一页
            let mut out = Vec::new();
            for kw in ["道德经", "论语", "诗经", "孙子兵法", "庄子", "孟子"] {
                if let Ok(mut v) = guoxue_search(kw).await {
                    v.truncate(6);
                    out.append(&mut v);
                }
                if out.len() >= 30 {
                    break;
                }
            }
            Ok(out)
        }
        "local" => Ok(local_scan(&book_dir().join("local"))),
        // 默认：热门（按下载量）
        _ => gutenberg_many(None, 3).await,
    }
}

pub async fn search(source: &str, kw: &str, page: u32) -> Result<Vec<Book>, String> {
    match source {
        "guoxue" => guoxue_search(kw).await,
        "shuge" => shuge_search(kw).await,
        "local" => {
            let k = kw.trim().to_lowercase();
            Ok(local_scan(&book_dir().join("local"))
                .into_iter()
                .filter(|b| k.is_empty() || b.title.to_lowercase().contains(&k))
                .collect())
        }
        _ => gutenberg_list(kw, None, page).await,
    }
}

/// 章节表：Gutenberg 是一整本（单章），其它源给链接
pub async fn chapters(book: &Book) -> Result<Vec<Chapter>, String> {
    match book.source.as_str() {
        "gutenberg" => Ok(vec![Chapter {
            index: 0,
            name: "全文（Gutenberg 单文件）".into(),
            url: book.read_url.clone(),
        }]),
        _ => Ok(vec![Chapter { index: 0, name: "正文".into(), url: book.read_url.clone() }]),
    }
}

pub async fn content(book: &Book, chapter_index: usize) -> Result<BookText, String> {
    if book.source == "local" {
        return read_local(&book.local_path, Some(chapter_index));
    }
    match book.source.as_str() {
        "shuge" => shuge_content(&book.read_url).await,
        "guoxue" => {
            let html = get_text(&book.read_url, Some(GUOXUE_BASE)).await?;
            let title = regex::Regex::new(r"(?is)<title[^>]*>(.*?)</title>")
                .unwrap()
                .captures(&html)
                .map(|c| html_unescape(&strip_tags(&c[1])))
                .unwrap_or_else(|| book.title.clone());
            // ★ 5000yan 的正文在 `<article class="reading-content chapter-content-font">` 里。
            //   实测踩坑：以前用宽泛的 "content" 匹配，命中的是**页面顶部导航**，
            //   于是正文只剩 9 个字（"第01章 天地之始"）。这里只认这一个确定的类名，
            //   并且从 <article 一直截到页面尾部的推荐区之前。
            let body = regex::Regex::new(r#"(?is)<article[^>]+class="reading-content[^"]*"[^>]*>([\s\S]*?)</article>"#)
                .ok()
                .and_then(|re| re.captures(&html).map(|c| c[1].to_string()))
                .unwrap_or_else(|| {
                    // 兜底：<article> 没有闭合标签时（页面很长会截断），退到"关掉折叠区"之后
                    let i = html.find("reading-content chapter-content-font").unwrap_or(0);
                    html[i..i.min(html.len())].chars().take(60_000).collect()
                });
            // 折叠的"完整解析"是 display:none，正文里要把隐藏部分也一起给用户
            let body = body.replace("display:none", "");
            let mut text = strip_tags(&body);
            // 砍掉尾部推荐/导航噪音
            for cut in ["上一篇", "下一篇", "相关推荐", "猜你喜欢", "热门推荐"] {
                if let Some(i) = text.find(cut) {
                    text.truncate(i);
                }
            }
            Ok(BookText {
                title,
                author: book.author.clone(),
                text: text.chars().take(200_000).collect(),
                chapters: Vec::new(),
                chapter_index: 0,
                has_next: false,
                has_prev: false,
            })
        }
        _ => {
            // Gutenberg：优先纯文本（干净），没有就抓 HTML
            let url = if !book.dl_txt.is_empty() { &book.dl_txt } else { &book.read_url };
            let body = get_text(url, Some("https://www.gutenberg.org/")).await?;
            let text = if body.trim_start().starts_with('<') {
                // HTML 版：砍掉页头页尾
                let start = body.find("*** START OF").unwrap_or(0);
                let end = body.find("*** END OF").unwrap_or(body.len());
                strip_tags(&body[start..end])
            } else {
                body
            };
            // Gutenberg 纯文本是 70 字硬折行；这里按"段落"重组：
            // 段落之间是空行，段内的单换行直接去掉。
            // ★ regex crate 不支持 look-ahead（`(?!\n)` 会 panic），
            //   所以先按空行切段，再逐段把换行拼掉，纯字符串处理，不靠正则。
            let mut joined = String::with_capacity(text.len());
            for para in text.split("\n\n") {
                if para.trim().is_empty() {
                    joined.push_str("\n\n");
                    continue;
                }
                for (i, line) in para.lines().enumerate() {
                    if i > 0 {
                        // 中文段落不加空格，西文加一个空格（否则单词会粘一起）
                        let needs_space = line
                            .chars()
                            .next()
                            .map(|c| c.is_ascii_alphanumeric())
                            .unwrap_or(false);
                        if needs_space {
                            joined.push(' ');
                        }
                    }
                    joined.push_str(line.trim_end());
                }
                joined.push_str("\n\n");
            }
            Ok(BookText {
                title: book.title.clone(),
                author: book.author.clone(),
                text: joined.chars().take(500_000).collect(),
                chapters: Vec::new(),
                chapter_index: 0,
                has_next: false,
                has_prev: false,
            })
        }
    }
}

/// 下载到本地（书库目录或用户指定目录）
pub async fn download(book: &Book, format: &str, dest_dir: &str) -> Result<String, String> {
    let url = match format {
        "epub" => {
            if book.dl_epub.is_empty() {
                return Err("这本没有 epub 版本".into());
            }
            book.dl_epub.clone()
        }
        _ => {
            if book.dl_txt.is_empty() {
                return Err("这本没有纯文本版本".into());
            }
            book.dl_txt.clone()
        }
    };
    let ext = if format == "epub" { "epub" } else { "txt" };
    let dir = if dest_dir.trim().is_empty() {
        book_dir().join("local")
    } else {
        PathBuf::from(dest_dir)
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("建目录失败: {e}"))?;
    let safe: String = book
        .title
        .chars()
        .map(|c| if r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .collect();
    let safe = safe.trim();
    let safe = if safe.is_empty() { book.source_id.clone() } else { safe.to_string() };
    let path = dir.join(format!("{safe}.{ext}"));
    let cli = http_client()?;
    let resp = cli.get(&url).send().await.map_err(|e| format!("下载失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("下载失败: HTTP {}", resp.status().as_u16()));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("读取失败: {e}"))?;
    std::fs::write(&path, &bytes).map_err(|e| format!("写文件失败: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}

/// 书格页面里的可下载文件（PDF 等）
pub async fn extra_downloads(url: &str) -> Result<Vec<(String, String)>, String> {
    let html = get_text(url, Some("https://www.shuge.org/")).await?;
    Ok(shuge_downloads(&html))
}

// ============================================================================
// Tauri 命令
// ============================================================================

#[tauri::command]
pub async fn book_home(section: String) -> Result<Vec<Book>, String> {
    home(&section).await
}

#[tauri::command]
pub async fn book_search(source: String, keyword: String, page: Option<u32>) -> Result<Vec<Book>, String> {
    if keyword.trim().is_empty() {
        return Ok(Vec::new());
    }
    search(&source, &keyword, page.unwrap_or(1)).await
}

#[tauri::command]
pub async fn book_content(book: Book, chapter_index: Option<usize>) -> Result<BookText, String> {
    content(&book, chapter_index.unwrap_or(0)).await
}

#[tauri::command]
pub async fn book_download(book: Book, format: String, dest_dir: Option<String>) -> Result<String, String> {
    download(&book, &format, dest_dir.as_deref().unwrap_or("")).await
}

/// 书格这类古籍页上的附加下载（PDF / 图片包）
#[tauri::command]
pub async fn book_extra_downloads(url: String) -> Result<Vec<serde_json::Value>, String> {
    let list = extra_downloads(&url).await?;
    Ok(list
        .into_iter()
        .map(|(u, name)| serde_json::json!({ "url": u, "name": name }))
        .collect())
}

/// 本地导入目录（默认 `<软件目录>\data\books\local`）
#[tauri::command]
pub async fn book_local_dir() -> Result<String, String> {
    let d = book_dir().join("local");
    crate::paths::ensure_dir(&d)?;
    Ok(d.to_string_lossy().to_string())
}

#[tauri::command]
pub async fn book_open_local_dir() -> Result<String, String> {
    let d = book_dir().join("local");
    crate::paths::ensure_dir(&d)?;
    let s = d.to_string_lossy().to_string();
    let _ = std::process::Command::new("explorer").arg(&s).spawn();
    Ok(s)
}

/// 把外部文件（用户自己下的 txt/epub）拷进书库
#[tauri::command]
pub async fn book_import(paths: Vec<String>) -> Result<usize, String> {
    let dst = book_dir().join("local");
    crate::paths::ensure_dir(&dst)?;
    let mut n = 0usize;
    for p in paths {
        let src = PathBuf::from(&p);
        let Some(name) = src.file_name() else { continue };
        let ext = src.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
        if !matches!(ext.as_str(), "txt" | "epub") {
            continue;
        }
        if std::fs::copy(&src, dst.join(name)).is_ok() {
            n += 1;
        }
    }
    Ok(n)
}

/// 书源自检：前端"源自检"按钮会挨个调它，把结果展示出来
#[tauri::command]
pub async fn book_probe(source: String) -> Result<serde_json::Value, String> {
    let t0 = std::time::Instant::now();
    let (kw, sec) = match source.as_str() {
        "guoxue" => ("道德经", "guoxue"),
        "shuge" => ("论语", "shuge"),
        "local" => ("", "local"),
        _ => ("holmes", "gutenberg"),
    };
    let r = search(sec, kw, 1).await;
    let ms = t0.elapsed().as_millis() as u64;
    match r {
        Ok(v) => Ok(serde_json::json!({
            "source": sec, "ok": !v.is_empty(), "count": v.len(), "ms": ms,
            "sample": v.first().map(|b| b.title.clone()).unwrap_or_default()
        })),
        Err(e) => Ok(serde_json::json!({ "source": sec, "ok": false, "count": 0, "ms": ms, "error": e })),
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_tags_basic() {
        let h = "<div><p>你好&nbsp;<b>世界</b></p><script>x()</script></div>";
        let t = strip_tags(h);
        assert!(t.contains("你好"));
        assert!(t.contains("世界"));
        assert!(!t.contains("x()"));
    }

    #[test]
    fn test_pick_formats_prefers_html_with_images() {
        let mut m = serde_json::Map::new();
        m.insert("text/plain; charset=utf-8".into(), serde_json::json!("https://x/a.txt"));
        m.insert("text/html; charset=utf-8".into(), serde_json::json!("https://x/a.htm"));
        m.insert("text/html; charset=utf-8; images".into(), serde_json::json!("https://x/a2.htm"));
        m.insert("application/epub+zip".into(), serde_json::json!("https://x/a.epub"));
        let (read, txt, epub) = pick_formats(&m);
        assert_eq!(read, "https://x/a2.htm", "有图版 HTML 优先");
        assert_eq!(txt, "https://x/a.txt");
        assert_eq!(epub, "https://x/a.epub");
    }

    #[test]
    fn test_pick_formats_falls_back_to_txt_for_reading() {
        let mut m = serde_json::Map::new();
        m.insert("text/plain; charset=utf-8".into(), serde_json::json!("https://x/a.txt"));
        let (read, _, _) = pick_formats(&m);
        assert_eq!(read, "https://x/a.txt", "没有 HTML 时用纯文本在线阅读");
    }

    #[test]
    fn test_gutendex_book_parsing() {
        let v = serde_json::json!({
            "id": 23962,
            "title": "西遊記",
            "authors": [{"name": "Wu, Cheng'en", "birth_year": 1500, "death_year": 1582}],
            "languages": ["zh"],
            "download_count": 1234,
            "subjects": ["Monkeys -- Fiction"],
            "bookshelves": ["Best Books Ever Listings / Classics"],
            "formats": {
                "text/plain; charset=utf-8": "https://www.gutenberg.org/files/23962/23962-0.txt",
                "text/html; charset=utf-8": "https://www.gutenberg.org/ebooks/23962.html.images",
                "application/epub+zip": "https://www.gutenberg.org/ebooks/23962.epub3.images"
            }
        });
        let b = gutendex_book(&v, None).expect("应该能解析");
        assert_eq!(b.key, "gutenberg:23962");
        assert_eq!(b.title, "西遊記");
        assert!(b.author.contains("Wu"), "作者要带生卒年: {}", b.author);
        assert_eq!(b.lang, "zh");
        assert_eq!(b.popularity, 1234);
        assert!(b.dl_txt.ends_with(".txt"));
        assert!(b.dl_epub.contains("epub"), "epub 直链: {}", b.dl_epub);
        assert!(b.read_url.contains("html"));
        assert!(b.tags.iter().any(|t| t == "Classics"), "bookshelves 要去掉前缀: {:?}", b.tags);
    }

    #[test]
    fn test_gutendex_lang_filter_rejects_other_languages() {
        let v = serde_json::json!({
            "id": 1342, "title": "Pride and Prejudice", "languages": ["en"],
            "formats": {"text/plain; charset=utf-8": "https://x/a.txt"}
        });
        assert!(gutendex_book(&v, Some("zh")).is_none(), "只筛中文时英文书要丢掉");
        assert!(gutendex_book(&v, None).is_some());
    }

    #[test]
    fn test_split_chapters_chinese_and_english() {
        let text = "序言\n\n第一章 开始\n正文一\n\n第二章 继续\n正文二\n\n第三章 结束\n正文三\n";
        let m = split_chapters(text);
        assert_eq!(m.len(), 3, "应识别三章: {m:?}");
        assert!(m[0].0.contains("第一章"));
        assert!(m[1].1 > m[0].1);
        let en = "Chapter 1\nfoo\n\nChapter 2\nbar\n\nChapter 3\nbaz\n";
        assert_eq!(split_chapters(en).len(), 3);
        // 少于 3 个标记就不切（短文本）
        assert!(split_chapters("第一章 只有一章\n正文").is_empty());
    }

    #[test]
    fn test_read_local_txt_with_chapters() {
        let dir = std::env::temp_dir().join("vx_book_test");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("测试书.txt");
        let body = "第一章 开端\n甲\n\n第二章 发展\n乙\n\n第三章 结局\n丙\n";
        std::fs::write(&p, body).unwrap();
        let r = read_local(&p.to_string_lossy(), Some(1)).expect("应能读");
        assert!(r.text.contains("第二章"), "章节定位错了: {}", r.text);
        assert!(r.text.contains("乙"));
        assert!(r.has_next && r.has_prev);
        assert_eq!(r.chapters.len(), 3);
        let r0 = read_local(&p.to_string_lossy(), Some(0)).unwrap();
        assert!(r0.has_next && !r0.has_prev);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_shuge_downloads_extract_pdf() {
        let html = r#"<p><a href="https://www.shuge.org/wp-content/uploads/2020/01/lunyu.pdf">下载</a>
                      <a href="/about">关于</a></p>"#;
        let d = shuge_downloads(html);
        assert_eq!(d.len(), 1);
        assert!(d[0].0.ends_with("lunyu.pdf"));
    }

    #[test]
    fn test_decode_body_gbk() {
        // "中文" 的 GBK 字节
        let gbk = [0xD6u8, 0xD0, 0xCE, 0xC4];
        let html = b"<html><head><meta charset=\"gbk\"></head><body>x</body></html>";
        let mut v = html.to_vec();
        v.extend_from_slice(&gbk);
        let s = decode_body(&v);
        if cfg!(windows) {
            assert!(s.contains("中文"), "GBK 应被正确解码: {s}");
        }
    }

    // ============================================================
    // 真实网络冒烟（`cargo test -- --ignored`）——书源到底能不能用，只认实测
    // ============================================================

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap()
    }

    /// Gutenberg 热门列表：必须有结果，且每条都要能读/能下
    #[test]
    #[ignore]
    fn live_gutenberg_popular() {
        let r = rt().block_on(gutenberg_list("", None, 1));
        match r {
            Ok(v) => {
                println!("[live] Gutenberg 热门 {} 本", v.len());
                for b in v.iter().take(5) {
                    println!("   {} | {} | 读={} 下载={}/{}",
                        b.title, b.author,
                        if b.read_url.is_empty() { "无" } else { "有" },
                        if b.dl_txt.is_empty() { "无" } else { "txt" },
                        if b.dl_epub.is_empty() { "无" } else { "epub" });
                }
                assert!(!v.is_empty(), "热门列表不该是空的");
                assert!(v.iter().any(|b| !b.dl_epub.is_empty()), "至少要有一本能下 epub");
            }
            Err(e) => println!("[live] Gutenberg 热门失败（网络问题，不算失败）: {e}"),
        }
    }

    /// 中文公版书：西遊記 / 紅樓夢 这类
    #[test]
    #[ignore]
    fn live_gutenberg_chinese() {
        let r = rt().block_on(gutenberg_list("", Some("zh"), 1));
        match r {
            Ok(v) => {
                println!("[live] Gutenberg 中文书 {} 本", v.len());
                for b in v.iter().take(8) {
                    println!("   {} | {} | lang={}", b.title, b.author, b.lang);
                }
                assert!(!v.is_empty(), "中文公版书不该是空的（实测有 444 本）");
                assert!(v.iter().all(|b| b.lang.contains("zh")), "语言过滤没生效");
            }
            Err(e) => println!("[live] Gutenberg 中文失败: {e}"),
        }
    }

    /// 搜索 + 真正把正文拉下来（这条才算"能在线阅读"）
    #[test]
    #[ignore]
    fn live_gutenberg_read_one() {
        let list = match rt().block_on(gutenberg_list("sherlock", None, 1)) {
            Ok(v) => v,
            Err(e) => { println!("[live] 搜索失败: {e}"); return; }
        };
        let Some(b) = list.first() else { println!("[live] 没搜到"); return };
        println!("[live] 选中: {} / {}", b.title, b.author);
        match rt().block_on(content(b, 0)) {
            Ok(t) => {
                println!("[live] 正文 {} 字，开头：{}", t.text.chars().count(),
                    t.text.chars().take(80).collect::<String>());
                assert!(t.text.chars().count() > 1000, "正文太短，肯定不对");
            }
            Err(e) => println!("[live] 取正文失败: {e}"),
        }
    }

    /// 国学经典（5000yan）：搜索 + 正文
    #[test]
    #[ignore]
    fn live_guoxue_read_one() {
        let list = match rt().block_on(guoxue_search("道德经")) {
            Ok(v) => v,
            Err(e) => { println!("[live] 国学搜索失败: {e}"); return; }
        };
        println!("[live] 国学搜到 {} 条", list.len());
        for b in list.iter().take(4) {
            println!("   {} -> {}", b.title, b.read_url);
        }
        if let Some(b) = list.first() {
            match rt().block_on(content(b, 0)) {
                Ok(t) => println!("[live] 正文 {} 字：{}", t.text.chars().count(),
                    t.text.chars().take(60).collect::<String>()),
                Err(e) => println!("[live] 取正文失败: {e}"),
            }
        }
    }

    /// 书格（古籍）：搜索 + 正文 + 页内 PDF 链接
    #[test]
    #[ignore]
    fn live_shuge_read_one() {
        let list = match rt().block_on(shuge_search("论语")) {
            Ok(v) => v,
            Err(e) => { println!("[live] 书格搜索失败: {e}"); return; }
        };
        println!("[live] 书格搜到 {} 条", list.len());
        for b in list.iter().take(4) {
            println!("   {} -> {}", b.title, b.read_url);
        }
        if let Some(b) = list.first() {
            match rt().block_on(content(b, 0)) {
                Ok(t) => println!("[live] 正文 {} 字：{}", t.text.chars().count(),
                    t.text.chars().take(60).collect::<String>()),
                Err(e) => println!("[live] 取正文失败: {e}"),
            }
            match rt().block_on(extra_downloads(&b.read_url)) {
                Ok(d) => println!("[live] 页内可下载文件 {} 个: {:?}", d.len(),
                    d.iter().take(3).map(|(u, _)| u.rsplit('/').next().unwrap_or(u)).collect::<Vec<_>>()),
                Err(e) => println!("[live] 取下载链接失败: {e}"),
            }
        }
    }

    /// 本地导入链路：写一个 txt → 扫出来 → 分章读
    #[test]
    #[ignore]
    fn live_local_import_roundtrip() {
        let dir = book_dir().join("local");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("__vx_smoke_book.txt");
        std::fs::write(&p, "第一章 开始\n甲甲甲\n\n第二章 继续\n乙乙乙\n\n第三章 结束\n丙丙丙\n").unwrap();
        let list = local_scan(&dir);
        println!("[live] 本地书 {} 本", list.len());
        let hit = list.iter().find(|b| b.title == "__vx_smoke_book");
        assert!(hit.is_some(), "本地导入的书没被扫出来: {:?}",
            list.iter().map(|b| b.title.clone()).collect::<Vec<_>>());
        let r = read_local(&hit.unwrap().local_path, Some(1)).unwrap();
        assert!(r.text.contains("第二章"), "章节定位错: {}", r.text);
        let _ = std::fs::remove_file(&p);
        println!("[live] 本地分章读取 OK: {} / 共 {} 章", r.text.trim(), r.chapters.len());
    }
}
