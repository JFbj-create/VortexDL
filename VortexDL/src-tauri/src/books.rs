// ============================================================================
// 书库 —— 免登录书源 + 在线阅读 + 下载到本地
// ----------------------------------------------------------------------------
// 用户要求：「动漫下面添加一个书库，两页：主页（推书、搜索）、收藏；
//   书可以下载或收藏在线阅读；大量找免登录、能在线阅读和下载的源，
//   要网络小说 / 杂志 / 名著」。
//
// ★★ 源的选择全部按**本机实测**，不靠想当然。2026-10-09 复测结论：
//
//   可用（都实测走通"搜索 → 书页 → 正文"）：
//     · **文潮小说 wcxs.net** —— 中文网络小说（玄幻/都市/穿越/军史…），
//       免登录，能读能下。★ 正文是 Base64 混淆（`document.writeln(qsbs.bb('...'))`），
//       必须解 base64 再 strip_tags，直接抓 HTML 只会得到导航。
//     · **Project Gutenberg 官网直连**（www.gutenberg.org）—— 79k 外文名著 +
//       **444 本中文公版书**（/browse/languages/zh）。★ 不再走 gutendex.com：
//       那个域名从这条网络**连不上**，30s×3 重试要 92 秒才报错，
//       就是用户报的「卡在搜索源出不来」；而官网本身是通的。
//     · **Standard Ebooks**（standardebooks.org）—— 排版精校的外文公版书，
//       epub 直链 + `/text/single-page` 一次给整本正文。
//     · **5000yan.com** —— 国学经典全文（道德经 / 论语 / 诗经）。
//     · **书格 shuge.org** —— 古籍善本，正文可读 + 页内 PDF。
//     · **本地导入**（txt / epub）—— 源全挂了也能用。
//
//   不可达/不可用（都实测过，别再往回加）：
//     · openlibrary、archive.org、wikisource、libgen、anna's archive、好读 haodoo —— 连不上/超时；
//     · 主流中文网文站：起点/番茄/七猫/掌阅/纵横/17K/塔读/小说阅读网 —— 要登录或纯 JS 渲染；
//     · 笔趣阁系（bqg128 / bqgui / b520 / bige7 / 365 / xbiquge / 69shuba / 23qb /
//       23us / bxwx / qbwx / bibqg / 飘天 / 快眼 / 书迷楼 …）—— 404 / 403 / DNS 失败 / 连接被拒；
//     · 追书神器公开 API —— 已改成要 token。
//
// ★ 超时策略（2026-10-09 改）：搜索/列表 **8 秒 × 2 次**，正文/下载 25 秒；
//   并且**连接层错误不重试**（DNS 失败、连接被拒、TLS 握手失败都是确定性的，
//   重试只是白等）。原来 30s×3 让一次失败要等 92 秒。
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

/// 搜索/列表用：**短超时**。
/// ★ 2026-10-09 用户报「卡在搜索源出不来」：原实现是 30s 超时 × 3 次重试 ≈ 92 秒
///   才报错（gutendex 从这条网络连不上），页面上只有一句"正在搜索…"，
///   看着就是死掉了。现在压到 8s × 2 次 ≈ 17 秒上限，并且**连接层错误不重试**
///   （DNS 失败 / 连接被拒 / TLS 握手失败都是确定性的，重试只是白等）。
fn client_fast() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .connect_timeout(Duration::from_secs(5))
        .user_agent(UA)
        .danger_accept_invalid_certs(true)
        .build()
        .map_err(|e| format!("HTTP 客户端创建失败: {e}"))
}

/// 正文/下载用：整本书可能几百 KB，给长一点。
fn client_body() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(25))
        .connect_timeout(Duration::from_secs(6))
        .user_agent(UA)
        .danger_accept_invalid_certs(true)
        .build()
        .map_err(|e| format!("HTTP 客户端创建失败: {e}"))
}

/// 这个错误值不值得重试？连接层错误（DNS/拒绝/握手）不值得。
fn worth_retry(e: &reqwest::Error) -> bool {
    if e.is_timeout() || e.is_body() || e.is_decode() {
        return true;
    }
    // 连接错误里，只有"超时"值得再试一次；DNS 失败、connection refused 直接放弃
    if e.is_connect() {
        let s = format!("{e}");
        return s.contains("timed out") || s.contains("timeout");
    }
    false
}

/// 带重试的 GET 文本（搜索/列表用短超时）。
async fn get_text(url: &str, referer: Option<&str>) -> Result<String, String> {
    get_text_with(client_fast()?, url, referer, 2).await
}

/// 正文用（长超时）。
async fn get_text_body(url: &str, referer: Option<&str>) -> Result<String, String> {
    get_text_with(client_body()?, url, referer, 2).await
}

async fn get_text_with(
    cli: reqwest::Client,
    url: &str,
    referer: Option<&str>,
    tries: u32,
) -> Result<String, String> {
    let mut last = String::new();
    for attempt in 0..tries {
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
            Err(e) => {
                last = format!("{e}");
                if !worth_retry(&e) {
                    break; // 连接层错误，重试没意义
                }
            }
        }
        if attempt + 1 < tries {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    Err(format!("请求 {url} 失败: {last}"))
}

/// POST 表单（文潮小说的搜索是 POST /search.html）。
async fn post_form(url: &str, pairs: &[(&str, &str)], referer: Option<&str>) -> Result<String, String> {
    let cli = client_fast()?;
    let mut rb = cli
        .post(url)
        .header("Accept-Language", "zh-CN,zh;q=0.9")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(pairs);
    if let Some(r) = referer {
        rb = rb.header("Referer", r);
    }
    let resp = rb.send().await.map_err(|e| format!("请求 {url} 失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("请求 {url} 失败: HTTP {}", resp.status().as_u16()));
    }
    let b = resp.bytes().await.map_err(|e| format!("读响应失败: {e}"))?;
    Ok(decode_body(&b))
}

/// 标准 base64 解码（文潮小说的章节正文是 `document.writeln(qsbs.bb('...'))` 里的 base64）。
/// 不引第三方 crate，自己写 20 行，带单测。
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| *b != b'\n' && *b != b'\r' && *b != b' ').collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in bytes {
        if c == b'=' {
            break;
        }
        let v = val(c)?;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
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
// 源 1：Project Gutenberg（官网直连）—— 名著主力
// ============================================================================

/// Gutenberg **直连**（不走 gutendex）。
///
/// ★ 2026-10-09 实测：`gutendex.com` 从这条网络**连不上**（30s×3 重试 ≈ 92 秒才报错，
///   用户报的「卡在搜索源出不来」就是它），而 `www.gutenberg.org` **本身是通的**。
///   于是改成直接抓官网搜索页 —— 反而更快更稳，也不再依赖第三方封装。
///
/// 页面结构（实测）：
///   `<li class="booklink"><a class="link" href="/ebooks/{id}">`
///     `<img class="cover-thumb" src="/cache/epub/{id}/pg{id}.cover.small.jpg">`
///     `<span class="title">书名</span><span class="subtitle">作者</span>`
///     `<span class="extra">93928 downloads</span>`
/// 下载直链（实测全部 200）：
///   txt  `https://www.gutenberg.org/ebooks/{id}.txt.utf-8`
///   epub `https://www.gutenberg.org/ebooks/{id}.epub3.images`
pub async fn gutenberg_list(query: &str, lang: Option<&str>, page: u32) -> Result<Vec<Book>, String> {
    let q = query.trim();
    // 中文公版书没有好的搜索入口（`&lang=zh` 实测被忽略，返回的还是英文书），
    // 改用官方的「按语言浏览」页，它把中文书全列出来了。
    let url = if q.is_empty() && lang == Some("zh") {
        GUTENBERG_ZH.to_string()
    } else {
        let mut u = String::from("https://www.gutenberg.org/ebooks/search/?");
        if !q.is_empty() {
            u.push_str(&format!("query={}&", urlencoding::encode(q)));
        }
        u.push_str("sort_order=downloads");
        if page > 1 {
            u.push_str(&format!("&start_index={}", 25 * (page - 1)));
        }
        u
    };
    let key = format!("gt:{url}");
    let html = match cache().get(&key, Duration::from_secs(1800)) {
        Some(b) => b,
        None => {
            let b = get_text(&url, None).await?;
            cache().put(&key, &b);
            b
        }
    };
    // ★ 「按语言浏览」页和搜索页结构**不一样**：它只有一排
    //   `<a href="/ebooks/25328">豆棚閒話</a>`，没有 li.booklink。
    //   一开始只写了一个解析器，实测中文书 0 本才发现的。
    if url == GUTENBERG_ZH {
        return Ok(parse_gutenberg_zh_list(&html));
    }
    Ok(parse_gutenberg_list(&html))
}

const GUTENBERG_ZH: &str = "https://www.gutenberg.org/browse/languages/zh";
const GUTENBERG_BASE: &str = "https://www.gutenberg.org";

/// 解析 `/browse/languages/zh`：纯 `<a href="/ebooks/{id}">书名</a>`。
fn parse_gutenberg_zh_list(html: &str) -> Vec<Book> {
    let re = regex::Regex::new(r#"(?is)<a[^>]+href="/ebooks/(\d+)"[^>]*>(.*?)</a>"#).unwrap();
    // 页面导航里也有 /ebooks/ 开头的链接，这些标题要排掉
    let noise = ["Browse By Language", "Bookshelves", "Main Categories", "Search"];
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for c in re.captures_iter(html) {
        let id = c[1].to_string();
        let title = html_unescape(&strip_tags(&c[2]));
        let title = title.trim();
        if title.is_empty() || noise.contains(&title) || !seen.insert(id.clone()) {
            continue;
        }
        let mut b = gutenberg_book_from_id(&id, title, "", 0);
        b.lang = "zh".into();
        b.tags = vec!["中文公版".into()];
        out.push(b);
    }
    out
}

/// 解析 Gutenberg 的书籍列表 HTML（搜索页和「按语言浏览」页结构一样，都是 `li.booklink`）。
fn parse_gutenberg_list(html: &str) -> Vec<Book> {
    let li_re = regex::Regex::new(r#"(?is)<li class="booklink">(.*?)</li>"#).unwrap();
    let id_re = regex::Regex::new(r#"/ebooks/(\d+)"#).unwrap();
    let title_re = regex::Regex::new(r#"(?is)<span class="title">(.*?)</span>"#).unwrap();
    let sub_re = regex::Regex::new(r#"(?is)<span class="subtitle">(.*?)</span>"#).unwrap();
    let extra_re = regex::Regex::new(r#"(?is)<span class="extra">(.*?)</span>"#).unwrap();
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for cap in li_re.captures_iter(html) {
        let li = &cap[1];
        let Some(id) = id_re.captures(li).map(|c| c[1].to_string()) else { continue };
        if !seen.insert(id.clone()) {
            continue;
        }
        let title = title_re
            .captures(li)
            .map(|c| html_unescape(&strip_tags(&c[1])))
            .unwrap_or_default();
        if title.trim().is_empty() {
            continue;
        }
        let author = sub_re
            .captures(li)
            .map(|c| html_unescape(&strip_tags(&c[1])))
            .unwrap_or_default();
        // "93928 downloads" -> 93928
        let popularity = extra_re
            .captures(li)
            .and_then(|c| {
                let t = strip_tags(&c[1]);
                t.split_whitespace()
                    .next()
                    .and_then(|n| n.replace(',', "").parse::<u64>().ok())
            })
            .unwrap_or(0);
        out.push(gutenberg_book_from_id(&id, &title, &author, popularity));
    }
    out
}

/// 用 ebook id 拼出完整的 Book（直链都是可预测的，实测全部 200）。
fn gutenberg_book_from_id(id: &str, title: &str, author: &str, popularity: u64) -> Book {
    let txt = format!("{GUTENBERG_BASE}/ebooks/{id}.txt.utf-8");
    Book {
        key: format!("gutenberg:{id}"),
        source: "gutenberg".into(),
        source_id: id.to_string(),
        title: title.to_string(),
        author: author.to_string(),
        cover: format!("{GUTENBERG_BASE}/cache/epub/{id}/pg{id}.cover.medium.jpg"),
        // 语言没法从列表页可靠地判断，交给正文里看；这里留空避免误导
        lang: String::new(),
        tags: vec!["公版名著".into()],
        desc: String::new(),
        read_url: txt.clone(),
        dl_txt: txt,
        dl_epub: format!("{GUTENBERG_BASE}/ebooks/{id}.epub3.images"),
        popularity,
        local_path: String::new(),
    }
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
// 源 3.5：文潮小说 wcxs.net（中文网络小说，免登录、能读能下）
// ----------------------------------------------------------------------------
// ★ 2026-10-09 实测：这批候选里唯一活着的中文网文站。
//   主流站（起点/番茄/七猫/掌阅/纵横/17K）全要登录或纯 JS；
//   笔趣阁系（bqg128 / bqgui / b520 / bige7 / 365 / xbiquge / 69shuba /
//   23qb / 23us / bxwx / qbwx …）实测 404 / 403 / DNS 失败 / 连接被拒。
//   文潮小说全链路走通：搜索 → 书页 → 目录 → 章节正文。
//
// 结构（实测）：
//   搜索   POST /search.html  form: keyword=xx           → 结果里是 /lishu/{slug}/
//   分类   GET  /fenlei/1/1.html  (1玄幻 2穿越 3都市 4军史 5网游 6科幻 7灵异 8其他)
//   书页   GET  /lishu/{slug}/                            → 书名 / 作者 / 简介 / 章节链接
//   目录   GET  /lishu/{slug}/mulu_1.html                 → 全量章节（书页只给最近几十章）
//   章节   GET  /lishu/{slug}/{code}.html                 → 正文在 document.writeln(qsbs.bb('BASE64')) 里
//
// ★ 正文是 **Base64 混淆**的（`qsbs.bb` 解码后是 <p> 段落），必须 base64 解出来再 strip_tags；
//   直接抓 HTML 只会得到导航和"请勿开启浏览器阅读模式"那句提示。
// ============================================================================

const WCXS_BASE: &str = "https://www.wcxs.net";

/// 从任意页面里抽出书籍卡片（搜索页 / 分类页 / 书页的"相关推荐"都是这个结构）。
fn parse_wcxs_books(html: &str) -> Vec<Book> {
    let re = regex::Regex::new(r#"(?is)<a[^>]+href="(/lishu/([a-z0-9]+)/)"[^>]*>(.*?)</a>"#).unwrap();
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for c in re.captures_iter(html) {
        let href = c[1].to_string();
        let slug = c[2].to_string();
        let title = html_unescape(&strip_tags(&c[3]));
        let title = title.trim().to_string();
        if title.chars().count() < 2 || !seen.insert(slug.clone()) {
            continue;
        }
        out.push(Book {
            key: format!("wcxs:{slug}"),
            source: "wcxs".into(),
            source_id: slug,
            title: title.chars().take(60).collect(),
            author: String::new(),
            cover: String::new(),
            lang: "zh".into(),
            tags: vec!["网络小说".into()],
            desc: String::new(),
            read_url: format!("{WCXS_BASE}{href}"),
            dl_txt: String::new(),
            dl_epub: String::new(),
            popularity: 0,
            local_path: String::new(),
        });
    }
    out
}

/// 分类首页（首页"推荐"用）。cat 见上面注释里的编号。
pub async fn wcxs_home(cat: u32) -> Result<Vec<Book>, String> {
    let cat = if (1..=8).contains(&cat) { cat } else { 1 };
    let url = format!("{WCXS_BASE}/fenlei/{cat}/1.html");
    let key = format!("wcxs:home:{cat}");
    let html = match cache().get(&key, Duration::from_secs(1800)) {
        Some(b) => b,
        None => {
            let b = get_text(&url, Some(WCXS_BASE)).await?;
            cache().put(&key, &b);
            b
        }
    };
    let mut v = parse_wcxs_books(&html);
    // 分类页把"最新章节"也放进来了，第一屏混着别的小说；截到 40 本够首页用
    v.truncate(40);
    Ok(v)
}

pub async fn wcxs_search(kw: &str) -> Result<Vec<Book>, String> {
    let html = post_form(
        &format!("{WCXS_BASE}/search.html"),
        &[("keyword", kw.trim()), ("searchtype", "all")],
        Some(WCXS_BASE),
    )
    .await?;
    let v = parse_wcxs_books(&html);
    if v.is_empty() {
        return Err(format!("「{}」没搜到结果", kw.trim()));
    }
    Ok(v)
}

/// 目录页（比书页全，书页只列最近几十章）。
pub async fn wcxs_chapters(book: &Book) -> Result<Vec<Chapter>, String> {
    let slug = book.source_id.trim_matches('/');
    let html = get_text(&format!("{WCXS_BASE}/lishu/{slug}/mulu_1.html"), Some(&book.read_url)).await?;
    Ok(parse_wcxs_chapters(&html, slug))
}

fn parse_wcxs_chapters(html: &str, slug: &str) -> Vec<Chapter> {
    let re = regex::Regex::new(r#"(?is)<a[^>]+href="(/lishu/[a-z0-9]+/([a-z0-9_]+)\.html)"[^>]*>(.*?)</a>"#).unwrap();
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for c in re.captures_iter(html) {
        let code = c[2].to_string();
        if code == "mulu_1" || !seen.insert(code.clone()) {
            continue;
        }
        let name = html_unescape(&strip_tags(&c[3]));
        let name = name.trim();
        // 目录页会混进"开始阅读""加入书架"这类按钮
        if name.is_empty() || name == "开始阅读" || name == "章节目录" || name == "加入书架" {
            continue;
        }
        out.push(Chapter {
            index: out.len(),
            name: name.chars().take(60).collect(),
            url: format!("{WCXS_BASE}{}", &c[1]),
        });
        let _ = slug;
    }
    out
}

/// 章节正文：把 `document.writeln(qsbs.bb('BASE64'))` 全部解出来拼起来。
pub async fn wcxs_content(book: &Book, chapter: Option<&Chapter>) -> Result<BookText, String> {
    let url = chapter.map(|c| c.url.clone()).unwrap_or_else(|| book.read_url.clone());
    let html = get_text_body(&url, Some(&book.read_url)).await?;
    let re = regex::Regex::new(r#"qsbs\.bb\('([A-Za-z0-9+/=\s]+)'\)"#).unwrap();
    let mut joined = String::new();
    for c in re.captures_iter(&html) {
        if let Some(raw) = b64_decode(&c[1]) {
            joined.push_str(&String::from_utf8_lossy(&raw));
        }
    }
    // 兜底：个别页面没用混淆，就直接取正文容器
    if joined.trim().is_empty() {
        if let Some(c) = regex::Regex::new(r#"(?is)<div[^>]*class="rtext"[^>]*>(.*?)</div>"#)
            .unwrap()
            .captures(&html)
        {
            joined = c[1].to_string();
        }
    }
    let mut text = strip_tags(&joined);
    // 砍掉站点塞在正文前后的提示
    for cut in ["请勿开启浏览器阅读模式", "文潮小说～", "文潮小说 >", "上一章", "下一章"] {
        if let Some(i) = text.find(cut) {
            text = text[i + cut.len()..].to_string();
        }
    }
    text = text.trim().to_string();
    if text.chars().count() < 20 {
        return Err("这一章没取到正文（站点可能改版了）".into());
    }
    Ok(BookText {
        title: chapter.map(|c| c.name.clone()).unwrap_or_else(|| book.title.clone()),
        author: book.author.clone(),
        text: text.chars().take(200_000).collect(),
        chapters: Vec::new(),
        chapter_index: chapter.map(|c| c.index).unwrap_or(0),
        has_next: false,
        has_prev: false,
    })
}

// ============================================================================
// 源 3.6：Standard Ebooks（外文名著，排版精校 + epub 直链）
// ----------------------------------------------------------------------------
// 实测：搜索 `GET /ebooks?query=x` → 结果 href `/ebooks/{author}/{slug}`；
//   详情页有 `<h1>书名`、封面 `/images/covers/{author}_{slug}/.../cover.jpg`、
//   epub 直链 `/ebooks/{a}/{s}/downloads/{a}_{s}.epub`；
//   全文 `/ebooks/{a}/{s}/text/single-page`（实测 577KB / 44 万字，一次给整本）。
// ============================================================================

const SE_BASE: &str = "https://standardebooks.org";

pub async fn se_search(kw: &str) -> Result<Vec<Book>, String> {
    let url = format!("{SE_BASE}/ebooks?query={}", urlencoding::encode(kw.trim()));
    let html = get_text(&url, None).await?;
    let books = parse_se_books(&html);
    if books.is_empty() {
        return Err(format!("Standard Ebooks 没搜到「{}」", kw.trim()));
    }
    Ok(books)
}

fn parse_se_books(html: &str) -> Vec<Book> {
    let re = regex::Regex::new(r#"href="(/ebooks/([a-z0-9-]+)/([a-z0-9-]+))""#).unwrap();
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for c in re.captures_iter(html) {
        let path = c[1].to_string();
        let author_slug = c[2].to_string();
        let slug = c[3].to_string();
        // /ebooks?query= 这个入口本身也会被匹配到，且作者页 /ebooks/{author} 不是书
        if slug == "ebooks" || !seen.insert(path.clone()) {
            continue;
        }
        let title = slug.replace('-', " ");
        let title = title
            .split_whitespace()
            .map(|w| {
                let mut cs = w.chars();
                match cs.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + cs.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        let author = author_slug.replace('-', " ");
        let author = author
            .split_whitespace()
            .map(|w| {
                let mut cs = w.chars();
                match cs.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + cs.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        out.push(Book {
            key: format!("se:{author_slug}/{slug}"),
            source: "se".into(),
            source_id: path.clone(),
            title,
            author,
            cover: String::new(),
            lang: "en".into(),
            tags: vec!["精校公版".into()],
            desc: String::new(),
            read_url: format!("{SE_BASE}{path}"),
            dl_txt: String::new(),
            dl_epub: format!("{SE_BASE}{path}/downloads/{author_slug}_{slug}.epub"),
            popularity: 0,
            local_path: String::new(),
        });
    }
    out
}

/// 整本正文（single-page 一次给全）。
pub async fn se_content(book: &Book) -> Result<BookText, String> {
    let url = format!("{SE_BASE}{}/text/single-page", book.source_id.trim_end_matches('/'));
    let html = get_text_body(&url, Some(&book.read_url)).await?;
    // 正文在 <main> 里；站点前后有导航
    let body = regex::Regex::new(r"(?is)<main[^>]*>(.*?)</main>")
        .ok()
        .and_then(|re| re.captures(&html).map(|c| c[1].to_string()))
        .unwrap_or(html);
    let text = strip_tags(&body);
    if text.chars().count() < 200 {
        return Err("Standard Ebooks 没取到正文".into());
    }
    Ok(BookText {
        title: book.title.clone(),
        author: book.author.clone(),
        text: text.chars().take(400_000).collect(),
        chapters: Vec::new(),
        chapter_index: 0,
        has_next: false,
        has_prev: false,
    })
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

/// 搜索页每页 25 条，首页太单薄 —— 并发抓 3 页凑满一屏。
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
        // 中文网络小说（文潮小说）—— 用户要的"小说"，玄幻分类打底
        "novel" => wcxs_home(1).await,
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
        "wcxs" => wcxs_search(kw).await,
        "se" => se_search(kw).await,
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

/// 章节表：单文件源给一章；文潮小说给真目录。
pub async fn chapters(book: &Book) -> Result<Vec<Chapter>, String> {
    match book.source.as_str() {
        "wcxs" => wcxs_chapters(book).await,
        "gutenberg" => Ok(vec![Chapter {
            index: 0,
            name: "全文（Gutenberg 单文件）".into(),
            url: book.read_url.clone(),
        }]),
        "se" => Ok(vec![Chapter {
            index: 0,
            name: "全文（Standard Ebooks 单页）".into(),
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
        "se" => se_content(book).await,
        "wcxs" => {
            // 先拿目录，再按 index 定位到具体那一章
            let chs = wcxs_chapters(book).await?;
            let ch = chs.get(chapter_index).ok_or_else(|| format!("没有第 {} 章", chapter_index + 1))?;
            let mut t = wcxs_content(book, Some(ch)).await?;
            t.chapters = chs;
            t.chapter_index = chapter_index;
            t.has_next = chapter_index + 1 < t.chapters.len();
            t.has_prev = chapter_index > 0;
            Ok(t)
        }
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
    // 网络小说源没有"整本直链"，只能把章节挨个抓下来拼成 txt。
    // 章数可能上千，这里限 200 章 + 并发 6，避免点一下卡几分钟。
    if book.source == "wcxs" {
        if format == "epub" {
            return Err("网络小说源没有 epub，选 txt 就行".into());
        }
        return download_wcxs_txt(book, dest_dir).await;
    }
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
    let cli = client_body()?;
    let resp = cli.get(&url).send().await.map_err(|e| format!("下载失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("下载失败: HTTP {}", resp.status().as_u16()));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("读取失败: {e}"))?;
    std::fs::write(&path, &bytes).map_err(|e| format!("写文件失败: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}

/// 网络小说：把章节抓下来拼成一本 txt（上限 200 章，并发 6）。
async fn download_wcxs_txt(book: &Book, dest_dir: &str) -> Result<String, String> {
    const MAX_CH: usize = 200;
    const CONC: usize = 6;

    let chs = wcxs_chapters(book).await?;
    if chs.is_empty() {
        return Err("这本书没抓到章节目录".into());
    }
    let take = chs.len().min(MAX_CH);
    let mut body = String::new();
    body.push_str(&format!("《{}》\n\n", book.title));
    if !book.author.is_empty() {
        body.push_str(&format!("作者：{}\n\n", book.author));
    }
    body.push_str(&format!(
        "（本文件含前 {} 章，共 {} 章；来源：文潮小说）\n\n",
        take,
        chs.len()
    ));

    for chunk in chs[..take].chunks(CONC) {
        let mut futs = Vec::new();
        for ch in chunk {
            futs.push(wcxs_content(book, Some(ch)));
        }
        for (ch, r) in chunk.iter().zip(futures::future::join_all(futs).await) {
            body.push_str(&format!("{}\n\n", ch.name));
            match r {
                Ok(t) => body.push_str(&t.text),
                Err(e) => body.push_str(&format!("（这一章没抓到：{e}）")),
            }
            body.push_str("\n\n");
        }
    }

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
    let path = dir.join(format!("{safe}.txt"));
    std::fs::write(&path, body.as_bytes()).map_err(|e| format!("写文件失败: {e}"))?;
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
        "wcxs" => ("斗破苍穹", "wcxs"),
        "se" => ("sherlock", "se"),
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
    fn test_b64_decode_roundtrip() {
        // 文潮小说的正文就是 base64 塞在 document.writeln(qsbs.bb('...')) 里
        let html = "<p>　　第44章 入职</p>";
        let enc = "PHA+44CA44CA56ysNDTnq6Ag5YWl6IGMPC9wPg==";
        assert_eq!(String::from_utf8(b64_decode(enc).unwrap()).unwrap(), html);
        // 带换行/空格的也要能解（站点会折行）
        assert_eq!(String::from_utf8(b64_decode("aGVs
bG8=").unwrap()).unwrap(), "hello");
        assert!(b64_decode("###").is_none(), "非法字符要返回 None 而不是 panic");
    }

    #[test]
    fn test_parse_gutenberg_list_real_fragment() {
        // 真站点的片段（2026-10-09 抓的），字段顺序别乱动
        let html = r#"
        <li class="booklink"><a class="link" href="/ebooks/1661" accesskey="3">
          <span class="cell leftcell with-cover"><img class="cover-thumb"
            src="/cache/epub/1661/pg1661.cover.small.jpg" alt=""></span>
          <span class="cell content"><span class="title">The Adventures of Sherlock Holmes</span>
            <span class="subtitle">Arthur Conan Doyle</span>
            <span class="extra">93928 downloads</span></span></a></li>
        <li class="booklink"><a class="link" href="/ebooks/244">
          <span class="cell content"><span class="title">A Study in Scarlet</span>
            <span class="subtitle">Arthur Conan Doyle</span>
            <span class="extra">28,752 downloads</span></span></a></li>
        "#;
        let v = parse_gutenberg_list(html);
        assert_eq!(v.len(), 2, "两本书");
        assert_eq!(v[0].key, "gutenberg:1661");
        assert_eq!(v[0].title, "The Adventures of Sherlock Holmes");
        assert_eq!(v[0].author, "Arthur Conan Doyle");
        assert_eq!(v[0].popularity, 93928);
        assert_eq!(v[0].dl_txt, "https://www.gutenberg.org/ebooks/1661.txt.utf-8");
        assert_eq!(v[0].dl_epub, "https://www.gutenberg.org/ebooks/1661.epub3.images");
        assert!(v[0].cover.ends_with("pg1661.cover.medium.jpg"));
        assert_eq!(v[1].popularity, 28752, "带千分位逗号的下载量也要能解析");
        // 同一本书出现两次只留一条
        assert_eq!(parse_gutenberg_list(&format!("{html}{html}")).len(), 2);
    }

    #[test]
    fn test_parse_gutenberg_zh_browse_fragment() {
        // /browse/languages/zh 是纯 a 链接，没有 li.booklink —— 这条锁定"抓不到就空列表而不是 panic"
        let html = r#"<a href="/ebooks/25328">豆棚閒話</a><a href="/ebooks/24225">戲中戲</a>"#;
        let v = parse_gutenberg_list(html);
        assert!(v.is_empty(), "结构不匹配时应返回空，交给上层换源: {v:?}");
    }

    #[test]
    fn test_parse_wcxs_search_fragment() {
        let html = r#"
          <div class="sr-item"><a href="/lishu/msuydce/">师妹竟是满级大佬</a></div>
          <div class="sr-item"><a href="/lishu/ciptxqc/">爱填满空白小说原版完整</a></div>
          <a href="/lishu/msuydce/">师妹竟是满级大佬</a>
        "#;
        let v = parse_wcxs_books(html);
        assert_eq!(v.len(), 2, "重复链接要去重: {v:?}");
        assert_eq!(v[0].key, "wcxs:msuydce");
        assert_eq!(v[0].title, "师妹竟是满级大佬");
        assert_eq!(v[0].source, "wcxs");
        assert!(v[0].read_url.contains("/lishu/msuydce/"));
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

    /// 文潮小说（中文网络小说）：搜索 → 目录 → 正文（Base64 混淆那条链路）
    #[test]
    #[ignore]
    fn live_wcxs_read_one() {
        let list = match rt().block_on(wcxs_search("斗破苍穹")) {
            Ok(v) => v,
            Err(e) => { println!("[live] 文潮搜索失败: {e}"); return; }
        };
        println!("[live] 文潮搜到 {} 条", list.len());
        for b in list.iter().take(3) {
            println!("   {} -> {}", b.title, b.read_url);
        }
        let Some(b) = list.first() else { println!("[live] 没搜到"); return };
        let chs = match rt().block_on(wcxs_chapters(b)) {
            Ok(v) => v,
            Err(e) => { println!("[live] 取目录失败: {e}"); return; }
        };
        println!("[live] 目录 {} 章，第 1 章: {}", chs.len(), chs.first().map(|c| c.name.clone()).unwrap_or_default());
        assert!(!chs.is_empty(), "目录不该是空的");
        // 挑中间一章（第 1 章常常是"作品相关"）
        let pick = chs.len() / 2;
        match rt().block_on(wcxs_content(b, chs.get(pick))) {
            Ok(t) => {
                println!("[live] 第 {} 章正文 {} 字：{}", pick + 1, t.text.chars().count(),
                    t.text.chars().take(80).collect::<String>());
                assert!(t.text.chars().count() > 200, "正文太短，base64 那条链路可能坏了");
            }
            Err(e) => panic!("[live] 取正文失败: {e}"),
        }
    }

    /// Standard Ebooks：搜索 → 整本正文
    #[test]
    #[ignore]
    fn live_se_read_one() {
        let list = match rt().block_on(se_search("sherlock")) {
            Ok(v) => v,
            Err(e) => { println!("[live] SE 搜索失败: {e}"); return; }
        };
        println!("[live] SE 搜到 {} 条", list.len());
        for b in list.iter().take(3) {
            println!("   {} / {} -> {}", b.title, b.author, b.dl_epub);
        }
        let Some(b) = list.first() else { println!("[live] 没搜到"); return };
        match rt().block_on(se_content(b)) {
            Ok(t) => {
                println!("[live] SE 正文 {} 字：{}", t.text.chars().count(),
                    t.text.chars().take(80).collect::<String>());
                assert!(t.text.chars().count() > 5000, "整本正文不该这么短");
            }
            Err(e) => panic!("[live] SE 取正文失败: {e}"),
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
