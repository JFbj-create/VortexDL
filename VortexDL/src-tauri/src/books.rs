// ============================================================================
// 书库 —— 免登录书源 + 在线阅读 + 下载到本地
// ----------------------------------------------------------------------------
// 用户要求：「动漫下面添加一个书库，两页：主页（推书、搜索）、收藏；
//   书可以下载或收藏在线阅读；大量找免登录、能在线阅读和下载的源，
//   要网络小说 / 杂志 / 名著」。
//
// ★★ 源的选择全部按**本机实测**，不靠想当然。2026-10-09 复测结论：
//
//   可用（都实测走通"搜索 → 书页 → 正文/下载"）：
//     · **无忧书城 wyshu.com** —— 中文**网络小说在线阅读**（龙族 1/4/5、九州缥缈录、
//       江南 / 我吃西红柿 / 天蚕土豆 …）。搜索 `GET /?s=<词>`、分类 `/yq//ds//kh/`、
//       目录 `/wl/{slug}/`、正文 `/wl/{slug}/{id}.html` 的 `<div class="article-post">`。
//       ★ 加这个源是因为用户点名要《龙族》，而苦瓜书盘上搜「龙族」「火之晨曦」
//       「九州缥缈录」**全是 0 条**（它只收正式出版物）。
//     · **苦瓜书盘 kgbook.com** —— 中文电子书（现代/古典文学、武侠、网络小说、科幻、
//       历史、期刊杂志…），**能直接下到 PDF / mobi / epub / txt 文件**。
//       搜索是 POST `/e/search/index.php`（隐藏字段 tbname=download），
//       结果页 `/e/search/result/?searchid=N`，下载 `e/DownSys/GetDown?classid=&id=&pathid=`
//       → 302 到真实文件（实测 application/pdf, 1.24MB）。
//       ★ **封面只有书页有**（分类页/结果页都没有图），所以列表出来后要并发补一次详情页。
//     · **Project Gutenberg 官网直连**（www.gutenberg.org）—— 79k 外文名著 +
//       **444 本中文公版书**（/browse/languages/zh）。★ 不再走 gutendex.com：
//       那个域名从这条网络**连不上**，30s×3 重试要 92 秒才报错，
//       就是用户报的「卡在搜索源出不来」；而官网本身是通的。
//     · **Standard Ebooks**（standardebooks.org）—— 排版精校的外文公版书，
//       epub 直链 + `/text/single-page` 一次给整本正文。
//     · **5000yan.com** —— 国学经典全文（道德经 / 论语 / 诗经）。
//     · **书格 shuge.org** —— 古籍善本，正文可读 + 页内 PDF。
//     · **本地导入**（txt / epub / **mobi / azw3**）—— 源全挂了也能用。
//
// ★ 在线阅读支持的格式（2026-10-09 起）：txt / epub / **mobi（含 azw3）**。
//   mobi 是用户报的「读不了 mobi 格式」—— 自己按 PalmDOC 格式解（`src/mobi.rs`，无新依赖），
//   苦瓜书盘上一大半中文书是 mobi（例如《窄门》）。只有 6寸pdf 还读不了（没有 PDF 文本层解析器），
//   那种会提示"点下载到本地看"。
//
//   不可达/不可用（都实测过，别再往回加）：
//     · openlibrary、archive.org、wikisource、libgen、anna's archive（.org/.se/.li 超时，
//       .gl 只 HEAD 通、GET 超时）、好读 haodoo —— 连不上/超时；
//     · 主流中文网文站：起点/番茄/七猫/掌阅/纵横/17K/塔读/小说阅读网 —— 要登录或纯 JS 渲染；
//     · 笔趣阁系（bqg128 / bqgui / b520 / bige7 / 365 / xbiquge / 69shuba / 23qb /
//       23us / bxwx / qbwx / bibqg / 飘天 / 快眼 / 书迷楼 …）—— 404 / 403 / DNS 失败 / 连接被拒；
//     · 追书神器公开 API —— 已改成要 token；
//     · **文潮小说 wcxs.net** —— 2026-10-09 按用户要求**移除**（"质量不高"）：内容是
//       SEO 聚合站，书名一堆"XX笔趣阁无弹窗免费阅读"，正文也是抓来的；
//     · 鸠摩搜书 jiumodiary.com —— 搜索框 `disabled`，**要加微信公众号拿验证码**（反爬），不做绕过；
//     · sobooks.cc —— 搜索有**算术验证码**（"28 + 41 = ?"）；搬书匠连接被强制关闭；
//     · 三秋书屋 d4j.cn —— 超时；MAGAZINELIB —— 下载链在 `/login/` 下，要登录；
//     · 读者阁 duzhege.cn —— 文章页 404、下载走 OneDrive 外链；DOAJ —— 403；
//     · 国家哲社文献中心 ncpssd.cn —— 是学术**检索**站，没有整本下载。
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

/// 抓整本书文件用：**绝对不能设总超时**。
///
/// 实测（2026-10-09）：苦瓜书盘的源站只有 **0.32 MB/s**，一本 7.2MB 的 epub 要跑
/// **22.3 秒**，而 `client_body()` 是 25 秒**总**超时 —— 正好卡在边界上，快一点就成功、
/// 慢一点就在读到一半时被掐断，reqwest 报 `error decoding response body`。
/// 这个错看着像"文件坏了"，其实是超时，而且重试 3 次会 3 次都撞同一面墙（因为总时长不变）。
///
/// 改成 `read_timeout`：它按**两次读到数据之间的间隔**计时，只要还在往下传就不算超时。
fn client_download() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .read_timeout(Duration::from_secs(40))
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
// 源 3.5：苦瓜书盘 kgbook.com（中文电子书，能直接下 PDF / mobi / epub / txt）
// ----------------------------------------------------------------------------
// ★ 2026-10-09 实测（用户给的源里唯一完整可用的中文电子书站）：
//   搜索   POST /e/search/index.php
//          form: keyboard=<词> show=title,booksay,bookwriter tbname=download tempid=1
//          ★ tbname 必须是 **download**（写 news 会返回"没有搜索到相关内容"）
//          → 302 到 /e/search/result/?searchid=N，那一页才是结果
//   书页   GET  /{分类拼音}/{id}.html   （如 /kehuanxuanhuan/513.html）
//          页面里有 作者/格式/语言/大小/简介
//   下载   GET  /e/DownSys/GetDown?classid={数字}&id={id}&pathid=0
//          → 302 到真实文件（实测 application/pdf, Content-Length 1.24MB）
//          ★ classid 是**数字**（科幻玄幻=5），不是分类拼音，只能从书页里抠
//
// 内容质量比"笔趣阁系"那种 SEO 聚合站高得多：都是正式出版物（三体/刘慈欣 这类）。
// ============================================================================

const KGBOOK_BASE: &str = "https://www.kgbook.com";

/// 书籍详情（书页里才有：作者/格式/大小/简介/真实下载链）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KgDetail {
    pub author: String,
    pub format: String,
    pub lang: String,
    pub size: String,
    pub desc: String,
    /// 下载入口（GetDown，会 302 到真实文件）
    pub dl_url: String,
    /// 封面图绝对地址（书页里的 `<img src="/d/file/....jpg" width="130">`）
    pub cover: String,
}

pub async fn kgbook_search(kw: &str) -> Result<Vec<Book>, String> {
    let html = post_form(
        &format!("{KGBOOK_BASE}/e/search/index.php"),
        &[
            ("keyboard", kw.trim()),
            ("show", "title,booksay,bookwriter"),
            ("tbname", "download"),
            ("tempid", "1"),
        ],
        Some(KGBOOK_BASE),
    )
    .await?;
    let v = parse_kgbook_books(&html);
    if v.is_empty() {
        // 站内没搜到时它会返回一句"没有搜索到相关的内容"
        return Err(format!("苦瓜书盘没搜到「{}」（换关键词试试）", kw.trim()));
    }
    Ok(v)
}

/// 苦瓜书盘「中文小说」首页要展示的分类（站点是按分类分开的，单个分类只有 20~30 本）
pub const KGBOOK_NOVEL_CATS: &[&str] = &[
    "kehuanxuanhuan",  // 科幻玄幻
    "wuxiaxiaoshuo",   // 武侠小说
    "wangluoxiaoshuo", // 网络小说
    "xiandaiwenxue",   // 现代文学
    "gudianwenxue",    // 古典文学
    "waiguowenxue",    // 外国文学
];
/// 苦瓜书盘「期刊杂志」分类
pub const KGBOOK_MAG_CATS: &[&str] = &["qikanzazhi"];

/// 多个分类合并成一个首页列表（每个分类页独立缓存 30 分钟，第一次之后很快）
pub async fn kgbook_home_multi(cats: &[&str]) -> Result<Vec<Book>, String> {
    let futs: Vec<_> = cats.iter().map(|c| kgbook_home(c)).collect();
    let mut out: Vec<Book> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut ok_any = false;
    for r in futures::future::join_all(futs).await {
        match r {
            Ok(v) => {
                ok_any = true;
                for b in v {
                    if seen.insert(b.key.clone()) {
                        out.push(b);
                    }
                }
            }
            Err(e) => println!("[kgbook] 分类拉取失败: {e}"),
        }
    }
    if !ok_any {
        return Err("苦瓜书盘的分类页都拉不到（网络问题）".into());
    }
    out.truncate(60);
    Ok(out)
}

/// 带分类倾向的搜索：**站内搜索 + 分类页本地匹配，只排序、不过滤**。
///
/// 为什么不能只靠站内搜索：苦瓜书盘的 `/e/search/` 是按 `tbname=download` 全站搜，
/// 实测搜「读者」返回 15 条，**一条杂志都没有**（命中的是简介里带"读者"的科普书），
/// 而杂志《读者》2009年合订本 明明在站内、搜「合订本」就能搜到。
/// 所以这里再补一层：把目标分类页的条目按书名本地匹配一遍，命中的排最前面。
/// 其它分类的结果一律保留 —— 一旦做硬过滤，书被归到别的分类就变成"搜不到"。
pub async fn kgbook_search_scoped(kw: &str, cats: &[&str]) -> Result<Vec<Book>, String> {
    let k = kw.trim().to_lowercase();
    // 分类页本地命中（并行拉，各自有缓存）
    let futs: Vec<_> = cats.iter().map(|c| kgbook_home(c)).collect();
    let mut local: Vec<Book> = Vec::new();
    for r in futures::future::join_all(futs).await {
        if let Ok(v) = r {
            for b in v {
                if b.title.to_lowercase().contains(&k) {
                    local.push(b);
                }
            }
        }
    }
    let mut remote = kgbook_search(kw).await.unwrap_or_default();
    let mut out: Vec<Book> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for b in local.into_iter().chain(remote.drain(..)) {
        if seen.insert(b.key.clone()) {
            out.push(b);
        }
    }
    if out.is_empty() {
        return Err(format!("苦瓜书盘没搜到「{}」（换关键词试试）", kw.trim()));
    }
    Ok(out)
}

/// 分类页（首页"推荐"用）。cat 传分类拼音，如 kehuanxuanhuan / xiandaiwenxue。
pub async fn kgbook_home(cat: &str) -> Result<Vec<Book>, String> {
    let cat = if cat.trim().is_empty() { "kehuanxuanhuan" } else { cat.trim() };
    let url = format!("{KGBOOK_BASE}/{cat}/");
    let key = format!("kg:home:{cat}");
    let html = match cache().get(&key, Duration::from_secs(1800)) {
        Some(b) => b,
        None => {
            let b = get_text(&url, Some(KGBOOK_BASE)).await?;
            cache().put(&key, &b);
            b
        }
    };
    let mut v = parse_kgbook_books(&html);
    v.truncate(48);
    Ok(v)
}

/// 从搜索结果页 / 分类页抽书籍卡片。
/// 链接形如 `https://kgbook.com/kehuanxuanhuan/513.html`；导航链接（e/、page/、skin/、list/）要排掉。
fn parse_kgbook_books(html: &str) -> Vec<Book> {
    let re = regex::Regex::new(
        r#"(?is)<a[^>]+href="(https?://(?:www\.)?kgbook\.com/([a-z]+)/(\d+)\.html)"[^>]*>(.*?)</a>"#,
    )
    .unwrap();
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for c in re.captures_iter(html) {
        let url = c[1].to_string();
        let cat = c[2].to_string();
        let id = c[3].to_string();
        let title = html_unescape(&strip_tags(&c[4]));
        let title = title.trim().to_string();
        // 结果页底部"热门下载"那一块也会被匹配到，书名太短的当噪声丢掉
        if title.chars().count() < 2 || !seen.insert(url.clone()) {
            continue;
        }
        out.push(Book {
            key: format!("kgbook:{cat}/{id}"),
            source: "kgbook".into(),
            source_id: format!("{cat}/{id}"),
            title: title.chars().take(70).collect(),
            author: String::new(),
            cover: String::new(),
            lang: "zh".into(),
            tags: vec!["中文电子书".into()],
            desc: String::new(),
            read_url: url,
            dl_txt: String::new(),
            dl_epub: String::new(),
            popularity: 0,
            local_path: String::new(),
        });
    }
    out
}

/// 拉书页，抠出作者/格式/大小/简介/下载入口。
pub async fn kgbook_detail(book: &Book) -> Result<KgDetail, String> {
    let html = get_text(&book.read_url, Some(KGBOOK_BASE)).await?;
    Ok(parse_kgbook_detail(&html))
}

fn parse_kgbook_detail(html: &str) -> KgDetail {
    let grab = |label: &str| -> String {
        // 页面是 "作者：刘慈欣 格式：6寸pdf 语言：简体中文 大小：1.18 MB"
        let pat = format!(r#"{}\s*[:：]\s*([^<>"'\s]{{1,24}})"#, label);
        regex::Regex::new(&pat)
            .ok()
            .and_then(|re| re.captures(html).map(|c| c[1].to_string()))
            .unwrap_or_default()
    };
    // 简介：从"简介："到下一个区块标题（或页面尾部的推荐区）
    let desc = {
        let re = regex::Regex::new(r#"(?is)简\s*介\s*[:：]\s*(.{0,1200}?)(?:<div|<p\s+class="(?:down|tag)|热门下载|购买正版)"#).unwrap();
        re.captures(html)
            .map(|c| html_unescape(&strip_tags(&c[1])))
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    // 下载入口（注意 HTML 里是 &amp; 转义）
    let dl_url = regex::Regex::new(r#"href="([^"]*DownSys/GetDown[^"]*)""#)
        .unwrap()
        .captures(html)
        .map(|c| c[1].replace("&amp;", "&"))
        .map(|u| if u.starts_with("http") { u } else { format!("{KGBOOK_BASE}{u}") })
        .unwrap_or_default();
    // 封面：书页里 `<img src="/d/file/201103/xxx.jpg" border="0" width="130" />`
    // （★ 分类页和搜索结果页**都没有图**，只有书页有 —— 所以封面必须靠详情页补）
    let cover = regex::Regex::new(r#"<img[^>]+src="([^"]*?/d/file/[^"]+?\.(?:jpg|jpeg|png|gif|webp))""#)
        .unwrap()
        .captures(html)
        .map(|c| c[1].to_string())
        .map(|u| if u.starts_with("http") { u } else { format!("{KGBOOK_BASE}{u}") })
        .unwrap_or_default();
    KgDetail {
        author: grab("作者"),
        format: grab("格式"),
        lang: grab("语言"),
        size: grab("大小"),
        desc: desc.chars().take(400).collect(),
        dl_url,
        cover,
    }
}

/// 给一批书补封面（列表接口拿不到封面，只有书页有）。
///
/// 为什么要并发：一本一个详情页，串行 18 本要 10 秒以上；并发 8 路大概 1~2 秒。
/// 详情页本身有 30 分钟 TTL 缓存，所以同一批书再进一次不会再打网络。
/// `max` 用来限制"只给可见的前 N 本补"，避免翻到 60 本时打 60 个请求。
pub async fn kgbook_fill_covers(books: &mut [Book], max: usize) {
    let todo: Vec<usize> = books
        .iter()
        .enumerate()
        .filter(|(_, b)| b.cover.is_empty())
        .map(|(i, _)| i)
        .take(max)
        .collect();
    if todo.is_empty() {
        return;
    }
    let futs: Vec<_> = todo
        .iter()
        .map(|&i| {
            let b = books[i].clone();
            async move { (i, kgbook_detail(&b).await.map(|d| d.cover).unwrap_or_default()) }
        })
        .collect();
    for (i, cover) in futures::future::join_all(futs).await {
        if !cover.is_empty() {
            books[i].cover = cover;
        }
    }
}

/// 这本书能不能在阅读器里直接读？
/// txt / epub / **mobi**（含 azw3，同一套 PalmDOC 解压）可以解出纯文本；
/// pdf / 6寸pdf 不行（没有 PDF 文本层解析器），只能下载到本地看。
fn kgbook_readable(detail: &KgDetail) -> bool {
    let f = detail.format.to_lowercase();
    f.contains("txt") || f.contains("epub") || f.contains("mobi") || f.contains("azw")
}

/// 把抓到的文件字节解成纯文本。返回 None 表示这个格式读不了（交给调用方报"请下载"）。
/// 抽出来是为了让"在线阅读"和"下载后抽正文"用同一套逻辑。
fn extract_book_text(bytes: &[u8], ext: &str) -> Option<String> {
    match ext {
        "epub" => epub_to_text(bytes).ok(),
        "mobi" | "azw" | "azw3" => {
            let raw = crate::mobi::mobi_text_bytes(bytes).ok()?;
            Some(clean_book_html(&decode_body(&raw)))
        }
        "txt" => Some(decode_body(bytes)),
        _ => None,
    }
}

/// 解出来的正文是 HTML（mobi 里带 `<p>` / `<mbp:pagebreak/>` 这些），清一遍。
fn clean_book_html(html: &str) -> String {
    let mut s = html.to_string();
    // script / style 整块去掉（regex 不支持反向引用，两个分开写）
    for pat in [r"(?is)<script[^>]*>.*?</script>", r"(?is)<style[^>]*>.*?</style>"] {
        if let Ok(re) = regex::Regex::new(pat) {
            s = re.replace_all(&s, "").to_string();
        }
    }
    // 换行标签换成真换行，其余标签直接去掉
    for (pat, rep) in [
        (r"(?i)<\s*br\s*/?>", "\n"),
        (r"(?i)</\s*(?:p|div|h[1-6]|li|tr)\s*>", "\n\n"),
    ] {
        if let Ok(re) = regex::Regex::new(pat) {
            s = re.replace_all(&s, rep).to_string();
        }
    }
    let t = html_unescape(&strip_tags(&s));
    // 压缩连续空行
    let mut out = String::with_capacity(t.len());
    let mut blank = 0;
    for line in t.lines() {
        let l = line.trim_end();
        if l.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(l);
        out.push('\n');
    }
    out.trim().to_string()
}

/// 把苦瓜书盘的书下载到本地，返回落地路径。扩展名按 Content-Disposition / 最终 URL 猜。
/// 抓文件字节 + 猜扩展名（在线阅读和"下载到本地"共用）。
async fn kgbook_fetch_bytes(book: &Book, detail: &KgDetail) -> Result<(Vec<u8>, String), String> {
    if detail.dl_url.is_empty() {
        return Err("这个页面里没找到下载入口（站点可能改版了）".into());
    }
    let cli = client_download()?;
    let mut last_err = String::new();
    // 实测：站点在 Cloudflare 后面，响应头带 `Connection: close`，7MB 的 epub 偶尔会在
    // 传一半时断流，reqwest 报 `error decoding response body`（Python/curl 同一时刻是好的，
    // 说明是连接被掐而不是文件坏）。所以这里**重试 2 次**，并且显式要求
    // `Accept-Encoding: identity` —— 二进制包不要走透明 gzip，解码失败正是这句报错的头号来源。
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(400 * attempt as u64)).await;
        }
        let resp = match cli
            .get(&detail.dl_url)
            .header("Referer", &book.read_url)
            .header("Accept-Encoding", "identity")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("下载失败: {e}");
                continue;
            }
        };
        if !resp.status().is_success() {
            last_err = format!("下载失败: HTTP {}", resp.status().as_u16());
            // 4xx 重试没意义（5xx 才可能是边缘节点抽风）
            if resp.status().as_u16() < 500 {
                return Err(last_err);
            }
            continue;
        }
        // 扩展名：优先 Content-Disposition 里的 filename，其次最终 URL 的后缀，最后按"格式"字段
        let cd = resp
            .headers()
            .get("content-disposition")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let final_url = resp.url().to_string();
        let want_len = resp.content_length();
        let ext = guess_ext(&cd).or_else(|| guess_ext(&final_url)).unwrap_or_else(|| {
            let f = detail.format.to_lowercase();
            if f.contains("epub") {
                "epub".to_string()
            } else if f.contains("mobi") {
                "mobi".to_string()
            } else if f.contains("txt") {
                "txt".to_string()
            } else {
                "pdf".to_string()
            }
        });
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                last_err = if e.is_timeout() {
                    format!("下载超时（这个源很慢，传了一半停了）: {e}")
                } else {
                    format!("读取失败: {e}")
                };
                continue;
            }
        };
        if bytes.is_empty() {
            last_err = "下载到 0 字节（站点可能限流了）".into();
            continue;
        }
        // ★ 校验长度：宁可重试也不要交出被截断的包（截断的 epub/zip 解压时会报"包坏了"）
        if let Some(want) = want_len {
            if (bytes.len() as u64) < want {
                last_err = format!("文件被截断：声明 {want} 字节，实际只收到 {}", bytes.len());
                continue;
            }
        }
        return Ok((bytes.to_vec(), ext));
    }
    Err(last_err)
}

pub async fn kgbook_download(book: &Book, dest_dir: &str) -> Result<String, String> {
    let detail = kgbook_detail(book).await?;
    let (bytes, ext) = kgbook_fetch_bytes(book, &detail).await?;
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
    let safe = if safe.is_empty() { book.source_id.replace('/', "_") } else { safe.to_string() };
    let path = dir.join(format!("{safe}.{ext}"));
    std::fs::write(&path, &bytes).map_err(|e| format!("写文件失败: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}

fn guess_ext(s: &str) -> Option<String> {
    let l = s.to_lowercase();
    for e in ["epub", "mobi", "azw3", "txt", "pdf", "zip", "rar"] {
        if l.contains(&format!(".{e}")) {
            return Some(e.to_string());
        }
    }
    None
}

// ============================================================================
// 源 3.6：无忧书城 wyshu.com（中文网络小说在线阅读）
// ----------------------------------------------------------------------------
// ★ 2026-10-09 加：用户要「龙族」这种中文小说，苦瓜书盘上没有（搜「龙族」「火之晨曦」
//   「九州缥缈录」全是 0 条）。实测这个站能拿到，而且结构很规整：
//
//   搜索   GET /?s=<词>            → 结果列表
//          `<a href="/wl/{slug}/">书名</a> <span>作者</span>`
//   分类   GET /yq/ /ds/ /kh/      （言情 / 都市文学 / 科幻小说）
//   目录   GET /wl/{slug}/         → `<a href="/wl/{slug}/{id}.html" title="章节名">`
//   正文   GET /wl/{slug}/{id}.html → `<div class="article-post">…<p>…</p>…</div>`
//
//   ★ 各卷是**独立的书**（longzu1huozhichenxi / longzu4aodingzhiyuan /
//     longzu5daowangzhedeguilai …），搜索「龙族」会一次列出全部卷。
//   ★ 目录页只给站点已有的章节（龙族1 只有 12 章、龙族4 全 17 章、龙族5 全 153 章），
//     缺章是站点本身没有，不是解析问题。
//   ★ 翻页不用站点的"上一章/下一章"链接（那套标记不稳定），直接用目录里的下标 ±1，
//     所以 `content()` 每次都把整份目录一起返回给前端。
// ============================================================================

const WYSHU_BASE: &str = "https://www.wyshu.com";

/// 分类拼音 → 中文名（首页用）
/// ★ `wl` 就是"网络小说"分类（100 本，江南的龙族 1/4/5、九州缥缈录 都在最前面）
pub const WYSHU_CATS: &[(&str, &str)] = &[
    ("wl", "网络小说"),
    ("kh", "科幻小说"),
    ("yq", "言情小说"),
    ("ds", "都市文学"),
];

/// 这本书自己的路径前缀（书页和章节页的路径是 `{前缀}{id}.html`）。
/// ★ 前缀**不是固定的**：龙族在 `/wl/longzu4.../`，安德的影子在 `/kh/andedeyingzi/` ——
///   首页/搜索给 `/wl/` 形状，分类页给 `/{分类}/{slug}/` 形状，而且 `/wl/{slug}/`
///   对后者是 **404**。所以只能从 book.read_url 里取前缀，不能硬编码。
fn wyshu_prefix(read_url: &str) -> String {
    let p = read_url.strip_prefix(WYSHU_BASE).unwrap_or(read_url);
    let p = p.split(['?', '#']).next().unwrap_or(p);
    if p.ends_with('/') { p.to_string() } else { format!("{p}/") }
}

/// ★★ 这个站的**站内搜索接口是坏的**，必须自己建索引：
///   · `/?s=<词>` 是**假搜索** —— 实测搜「龙族」和搜「zzzzqqqq」返回的是**同一页**
///     （都是全站目录 185 本），拿它当搜索用会"搜什么都返回一大堆"；
///   · 真搜索表单指向 `POST /e/search/index.php`，但**对非浏览器客户端一律 403**
///     （带 cookie 会话、带 Referer 都试过，还是 403 —— 是 WAF 挡的，不做绕过）。
///
/// 好在全站书目**可枚举**：9 个分类页（wl/wx/xd/kh/wg/ds/yq/ys/xy）每个正好 100 本，
/// `index_2.html` 不再新增 → 实测去重后 **898 本**。所以把 9 个分类页合成一份索引
/// （缓存 30 分钟），搜索就在本地按书名/作者做包含匹配。第一次搜多花 1~2 秒建索引。
async fn wyshu_index() -> Result<Vec<Book>, String> {
    const KEY: &str = "wyshu:index";
    if let Some(s) = cache().get(KEY, Duration::from_secs(1800)) {
        if let Ok(v) = serde_json::from_str::<Vec<Book>>(&s) {
            if !v.is_empty() {
                return Ok(v);
            }
        }
    }
    let futs: Vec<_> = WYSHU_CATS.iter().map(|(c, _)| wyshu_home(c)).collect();
    let mut out: Vec<Book> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut ok_any = false;
    for r in futures::future::join_all(futs).await {
        if let Ok(v) = r {
            ok_any = true;
            for b in v {
                if seen.insert(b.source_id.clone()) {
                    out.push(b);
                }
            }
        }
    }
    if !ok_any {
        return Err("无忧书城一个分类页都拉不到（网络问题）".into());
    }
    if let Ok(s) = serde_json::to_string(&out) {
        cache().put(KEY, &s);
    }
    Ok(out)
}

/// 本地搜索：按书名 / 作者包含匹配。
pub async fn wyshu_search(kw: &str) -> Result<Vec<Book>, String> {
    let k = kw.trim().to_lowercase();
    if k.is_empty() {
        return Ok(Vec::new());
    }
    let idx = wyshu_index().await?;
    let hits: Vec<Book> = idx
        .into_iter()
        .filter(|b| b.title.to_lowercase().contains(&k) || b.author.to_lowercase().contains(&k))
        .collect();
    if hits.is_empty() {
        return Err(format!(
            "无忧书城（{} 本）里没搜到「{}」—— 换关键词，或去「中文电子书」分类看看",
            WYSHU_CATS.len() * 100,
            kw.trim()
        ));
    }
    Ok(hits)
}

/// 分类页
pub async fn wyshu_home(cat: &str) -> Result<Vec<Book>, String> {
    let cat = if cat.trim().is_empty() { "wl" } else { cat.trim() };
    let url = format!("{WYSHU_BASE}/{cat}/");
    let key = format!("wyshu:home:{cat}");
    let html = match cache().get(&key, Duration::from_secs(1800)) {
        Some(b) => b,
        None => {
            let b = get_text(&url, Some(WYSHU_BASE)).await?;
            cache().put(&key, &b);
            b
        }
    };
    let mut v = parse_wyshu_books(&html);
    // ★ 不要截断太狠：分类页就是 100 本一页，而全站索引（wyshu_index）要靠这里拿全，
    //   截到 60 会每个分类少 40 本（用户搜的书可能正好在被截掉的那部分）。
    v.truncate(100);
    Ok(v)
}

/// 从搜索结果 / 分类页抽书。结果项形如：
///   `<h3 class="h5 text-truncate mw-100"><a href="/wl/{slug}/" class="text-dark">书名</a>
///    <span class="text-black-50 h6">作者</span></h3>`
/// ★ 书页路径有两种形状，都要认：
///   `/wl/{slug}/`（首页 / 搜索结果）与 `/{2字母分类}/{slug}/`（分类页，如 `/kh/andedeyingzi/`）。
fn parse_wyshu_books(html: &str) -> Vec<Book> {
    const NOT_BOOK: &[&str] = &["js", "cs", "im", "us", "as", "st", "fo", "up"];
    let re = regex::Regex::new(
        r#"(?is)<a\s+href="/([a-z0-9]{2})/([A-Za-z0-9_\-]{2,60})/"[^>]*>(.*?)</a>(?:\s*<span[^>]*>(.*?)</span>)?"#,
    )
    .unwrap();
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for c in re.captures_iter(html) {
        let cat = c[1].to_string();
        let slug = c[2].to_string();
        if NOT_BOOK.contains(&cat.as_str()) || !seen.insert(slug.clone()) {
            continue;
        }
        let title = html_unescape(&strip_tags(&c[3])).trim().to_string();
        if title.chars().count() < 2 || title.chars().count() > 60 {
            continue;
        }
        let author = c
            .get(4)
            .map(|m| html_unescape(&strip_tags(m.as_str())).trim().to_string())
            .unwrap_or_default();
        out.push(Book {
            key: format!("wyshu:{slug}"),
            source: "wyshu".into(),
            source_id: slug.clone(),
            title,
            author,
            cover: String::new(),
            lang: "zh".into(),
            tags: vec!["网络小说".into()],
            desc: String::new(),
            read_url: format!("{WYSHU_BASE}/{cat}/{slug}/"),
            dl_txt: String::new(),
            dl_epub: String::new(),
            popularity: 0,
            local_path: String::new(),
        });
    }
    out
}

/// 目录：`<a href="{前缀}{id}.html" title="章节名">章节名</a>`
pub async fn wyshu_chapters(book: &Book) -> Result<Vec<Chapter>, String> {
    let prefix = wyshu_prefix(&book.read_url);
    let key = format!("wyshu:toc:{}", book.source_id);
    let html = match cache().get(&key, Duration::from_secs(3600)) {
        Some(b) => b,
        None => {
            let b = get_text(&book.read_url, Some(WYSHU_BASE)).await?;
            cache().put(&key, &b);
            b
        }
    };
    let re = regex::Regex::new(&format!(
        r#"(?is)<a\s+href="{}([0-9]+)\.html"[^>]*?(?:title="([^"]*)")?[^>]*>(.*?)</a>"#,
        regex::escape(&prefix)
    ))
    .unwrap();
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for c in re.captures_iter(&html) {
        let id = c[1].to_string();
        if !seen.insert(id.clone()) {
            continue;
        }
        let name = c
            .get(2)
            .filter(|m| !m.as_str().trim().is_empty())
            .map(|m| m.as_str().to_string())
            .unwrap_or_else(|| html_unescape(&strip_tags(&c[3])));
        let name = html_unescape(&strip_tags(&name)).trim().to_string();
        out.push(Chapter {
            index: out.len(),
            name,
            url: format!("{WYSHU_BASE}{prefix}{id}.html"),
        });
    }
    if out.is_empty() {
        return Err("这个书页里没找到目录（站点可能改版了）".into());
    }
    Ok(out)
}

/// 按 class 名取出一个容器元素的内容（**按标签配平**，不是"匹配到第一个 </div>"）。
///
/// 为什么不能只写正则：无忧书城同一套模板里，正文容器**有的页是 `<div>` 有的页是
/// `<article>`**（龙族1 是 div、龙族4 是 article —— 实测踩到），而且惰性匹配
/// `([\s\S]*?)</div>` 在容器是 article 时会一路吃穿到后面的 div 里去。
/// 这里先由 `class="..."` 回溯出真正的标签名，再数 `<tag` / `</tag` 配对。
fn extract_container(html: &str, class_name: &str) -> Option<String> {
    let needle = format!("class=\"");
    let mut from = 0usize;
    loop {
        let ci = html[from..].find(&needle)? + from;
        let vstart = ci + needle.len();
        let vend = html[vstart..].find('"')? + vstart;
        let classes = &html[vstart..vend];
        let hit = classes.split_whitespace().any(|c| c == class_name);
        // 开标签起点：从 ci 往前找最近的 '<'
        let tag_open = html[..ci].rfind('<')?;
        // 只在同一标签内找 class（避免跨标签误命中）
        if hit && !html[tag_open..ci].contains('>') {
            let rest = &html[tag_open + 1..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_lowercase();
            if name.is_empty() {
                return None;
            }
            // 跳过开标签本身
            let gt = html[tag_open..].find('>')? + tag_open + 1;
            let open_pat = format!("<{name}");
            let close_pat = format!("</{name}");
            let mut depth = 1i32;
            let mut i = gt;
            while i < html.len() {
                let no = html[i..].find(&open_pat).map(|k| k + i);
                let nc = html[i..].find(&close_pat).map(|k| k + i);
                match (no, nc) {
                    (_, None) => break,
                    (Some(o), Some(c)) if o < c => {
                        // `<name` 后面必须是空白或 '>' 才算同一个标签（避免 <articleX>）
                        let after = html.as_bytes().get(o + open_pat.len()).copied();
                        if matches!(after, Some(b' ') | Some(b'>') | Some(b'\n') | Some(b'\r') | Some(b'\t')) {
                            depth += 1;
                        }
                        i = o + open_pat.len();
                    }
                    (_, Some(c)) => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(html[gt..c].to_string());
                        }
                        i = c + close_pat.len();
                    }
                }
            }
            return Some(html[gt..].to_string());
        }
        from = vend;
    }
}

async fn wyshu_content(book: &Book, chapter_index: usize) -> Result<BookText, String> {
    let chapters = wyshu_chapters(book).await?;
    let idx = chapter_index.min(chapters.len().saturating_sub(1));
    let ch = &chapters[idx];
    let html = get_text(&ch.url, Some(&book.read_url)).await?;
    let body = wyshu_body(&html);
    if body.trim().is_empty() {
        return Err("这一章没抽到正文（站点可能改版了）".into());
    }
    let mut text = clean_book_html(&body);
    // 砍掉站点挂在正文尾部的推广/导航
    for cut in ["无忧书城", "上一章", "下一章", "加入书签", "推荐阅读", "章节报错"] {
        if let Some(i) = text.find(cut) {
            text.truncate(i);
        }
    }
    let text = text.trim().to_string();
    if text.chars().count() < 20 {
        return Err("这一章正文太短，可能没抽对".into());
    }
    Ok(BookText {
        title: ch.name.clone(),
        author: book.author.clone(),
        text,
        has_prev: idx > 0,
        has_next: idx + 1 < chapters.len(),
        chapter_index: idx,
        chapters,
    })
}

/// 章节页 → 正文 HTML（在线读和整本下载共用一套）
fn wyshu_body(html: &str) -> String {
    extract_container(html, "article-post").unwrap_or_default()
}

/// 整本下载：逐章抓下来拼成一个 txt（站点没有整本直链）。
/// ★ 上限 300 章：龙族5 有 153 章，够用；再长的不下，免得打几百个请求。
async fn wyshu_download_txt(book: &Book, dest_dir: &str) -> Result<String, String> {
    let chapters = wyshu_chapters(book).await?;
    let total = chapters.len().min(300);
    let mut parts: Vec<String> = Vec::with_capacity(total);
    parts.push(format!("{}\n作者：{}\n来源：无忧书城\n\n", book.title, book.author));
    for ch in chapters.iter().take(total) {
        match get_text(&ch.url, Some(&book.read_url)).await {
            Ok(html) => {
                let mut t = clean_book_html(&wyshu_body(&html));
                for cut in ["无忧书城", "上一章", "下一章", "加入书签", "推荐阅读", "章节报错"] {
                    if let Some(i) = t.find(cut) {
                        t.truncate(i);
                    }
                }
                parts.push(format!("\n\n{}\n\n{}", ch.name, t.trim()));
            }
            Err(e) => parts.push(format!("\n\n{}\n\n（这一章没抓到：{e}）", ch.name)),
        }
    }
    let all = parts.join("");
    let dir = if dest_dir.trim().is_empty() { book_dir().join("local") } else { PathBuf::from(dest_dir) };
    std::fs::create_dir_all(&dir).map_err(|e| format!("建目录失败: {e}"))?;
    let safe: String = book.title.chars().map(|c| if r#"<>:"/\|?*"#.contains(c) { '_' } else { c }).collect();
    let path = dir.join(format!("{}.txt", safe.trim()));
    std::fs::write(&path, all.as_bytes()).map_err(|e| format!("写文件失败: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}

// ============================================================================
// 源 3.7：Standard Ebooks（外文名著，排版精校 + epub 直链）
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
// 源 4：本地导入（txt / epub / mobi）—— 源全挂了也能用
// ============================================================================

/// 本地书认这些扩展名（mobi 是本轮加的：用户报"读不了 mobi"，自己下的 mobi 也该能读）
pub const LOCAL_EXTS: &[&str] = &["txt", "epub", "mobi", "azw3"];

pub fn local_scan(dir: &Path) -> Vec<Book> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
        if !LOCAL_EXTS.contains(&ext.as_str()) {
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
            tags: vec![ext.to_uppercase()],
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
    let text = match ext.as_str() {
        "epub" => epub_to_text(&raw)?,
        "mobi" | "azw" | "azw3" => {
            let b = crate::mobi::mobi_text_bytes(&raw)?;
            clean_book_html(&decode_body(&b))
        }
        _ => decode_body(&raw),
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
        // 中文电子书（苦瓜书盘）—— 正式出版物，科幻玄幻分类打底
        "novel" => kgbook_home_multi(KGBOOK_NOVEL_CATS).await,
        // 网络小说（无忧书城）—— 龙族 / 九州缥缈录 这类
        "webnovel" => wyshu_home("wl").await,
        // 期刊杂志（苦瓜书盘的期刊杂志分类，PDF）
        "magazine" => kgbook_home_multi(KGBOOK_MAG_CATS).await,
        "local" => Ok(local_scan(&book_dir().join("local"))),
        // ★ 默认首页：网络小说 + 中文电子书各一半。
        //   原来默认是 Gutenberg 外文名著，用户反馈"所有书都是英文"，所以改成中文优先。
        _ => {
            let (a, b) = futures::join!(wyshu_home("wl"), kgbook_home_multi(&["kehuanxuanhuan", "xiandaiwenxue"]));
            let mut out = Vec::new();
            if let Ok(mut v) = a {
                v.truncate(10);
                out.append(&mut v);
            }
            if let Ok(mut v) = b {
                v.truncate(8);
                out.append(&mut v);
            }
            if out.is_empty() {
                return Err("推荐列表拉不到（网络问题），直接搜书名试试".into());
            }
            Ok(out)
        }
    }
}

pub async fn search(source: &str, kw: &str, page: u32) -> Result<Vec<Book>, String> {
    match source {
        // ★ 默认搜索：**一次搜所有源**再合并。
        //   用户报"要搜的搜不到"，根因是每个源各搜各的、用户不知道该选哪个；
        //   三个源并行打，最慢的那个决定总耗时（~2 秒），但一次就能搜到。
        "all" => {
            let kw = kw.trim();
            let jobs = vec![
                Box::pin(kgbook_search(kw)) as std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Book>, String>> + Send>>,
                Box::pin(wyshu_search(kw)),
                Box::pin(gutenberg_list(kw, None, page)),
            ];
            let mut out = Vec::new();
            let mut errs: Vec<String> = Vec::new();
            for r in futures::future::join_all(jobs).await {
                match r {
                    Ok(mut v) => out.append(&mut v),
                    Err(e) => errs.push(e),
                }
            }
            if out.is_empty() {
                return Err(if errs.is_empty() {
                    format!("没搜到「{kw}」")
                } else {
                    errs.join("；")
                });
            }
            // ★ 书名里带关键词的排最前面。
            //   苦瓜书盘的站内搜索是**模糊**的（搜「龙族」会把简介里带"龙"的魔兽世界、
            //   创龙传一起返回），而用户要的是"龙族"本身。按"书名命中 > 作者命中 > 其它"
            //   排一遍，用户一眼就能看到自己要的那本。
            let k = kw.to_lowercase();
            out.sort_by_key(|b| {
                let t = b.title.to_lowercase();
                let a = b.author.to_lowercase();
                if t.contains(&k) {
                    0
                } else if a.contains(&k) {
                    1
                } else {
                    2
                }
            });
            Ok(out)
        }
        "kgbook" => kgbook_search(kw).await,
        "novel" => kgbook_search_scoped(kw, KGBOOK_NOVEL_CATS).await,
        "webnovel" => wyshu_search(kw).await,
        "magazine" => kgbook_search_scoped(kw, KGBOOK_MAG_CATS).await,
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

/// 章节表：单文件源给一章；无忧书城给真目录。
pub async fn chapters(book: &Book) -> Result<Vec<Chapter>, String> {
    match book.source.as_str() {
        "wyshu" => wyshu_chapters(book).await,
        "kgbook" => Ok(vec![Chapter {
            index: 0,
            name: "整本（txt/epub/mobi 可在线读，PDF 请下载）".into(),
            url: book.read_url.clone(),
        }]),
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
        // 苦瓜书盘：txt / epub / mobi 都能解出纯文本直接读；
        // 6寸pdf 没有文本层解析器，只能提示去下载。
        "kgbook" => {
            let detail = kgbook_detail(book).await?;
            if !kgbook_readable(&detail) {
                return Err(format!(
                    "这本是 {} 格式，阅读器读不了 —— 点「下载」拿到文件后用本地阅读器打开（作者：{}）",
                    if detail.format.is_empty() { "PDF".to_string() } else { detail.format.clone() },
                    if detail.author.is_empty() { "未知" } else { &detail.author }
                ));
            }
            let (bytes, ext) = kgbook_fetch_bytes(book, &detail).await?;
            let text = extract_book_text(&bytes, &ext).ok_or_else(|| {
                format!("这个 {ext} 文件没能抽出正文（可能是扫描版或加密的）")
            })?;
            let text = text.trim().to_string();
            if text.chars().count() < 50 {
                return Err("这个文件里没抽出正文（可能是扫描版）".into());
            }
            Ok(BookText {
                title: book.title.clone(),
                author: detail.author.clone(),
                text: text.chars().take(400_000).collect(),
                chapters: Vec::new(),
                chapter_index: 0,
                has_next: false,
                has_prev: false,
            })
        }
        "se" => se_content(book).await,
        "wyshu" => wyshu_content(book, chapter_index).await,
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
    // 苦瓜书盘：书页里才有真实下载链，单独走一条
    if book.source == "kgbook" {
        return kgbook_download(book, dest_dir).await;
    }
    // 无忧书城：没有整本直链，逐章抓下来拼成一本 txt
    if book.source == "wyshu" {
        return wyshu_download_txt(book, dest_dir).await;
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
    let cli = client_download()?;
    let resp = cli.get(&url).send().await.map_err(|e| format!("下载失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("下载失败: HTTP {}", resp.status().as_u16()));
    }
    let want_len = resp.content_length();
    let bytes = resp.bytes().await.map_err(|e| {
        if e.is_timeout() { format!("下载超时（这个源很慢）: {e}") } else { format!("读取失败: {e}") }
    })?;
    if let Some(w) = want_len {
        if (bytes.len() as u64) < w {
            return Err(format!("文件被截断：声明 {w} 字节，实际只收到 {}", bytes.len()));
        }
    }
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

/// 首页一次给几本。用户反馈"推荐这么多太麻烦了"，从 60 收到 18。
const HOME_LIMIT: usize = 18;
/// 补封面的上限：一本一个详情页请求，只给可见的前 N 本补
const COVER_LIMIT: usize = 18;

#[tauri::command]
pub async fn book_home(section: String) -> Result<Vec<Book>, String> {
    let mut v = home(&section).await?;
    v.truncate(HOME_LIMIT);
    kgbook_fill_covers(&mut v, COVER_LIMIT).await;
    Ok(v)
}

#[tauri::command]
pub async fn book_search(source: String, keyword: String, page: Option<u32>) -> Result<Vec<Book>, String> {
    if keyword.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut v = search(&source, &keyword, page.unwrap_or(1)).await?;
    // 合并搜索可能一次给几十上百条，留 60 条够翻；封面只给前 18 本补
    v.truncate(60);
    kgbook_fill_covers(&mut v, COVER_LIMIT).await;
    Ok(v)
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
        if !LOCAL_EXTS.contains(&ext.as_str()) {
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
        "kgbook" => ("三体", "kgbook"),
        "novel" => ("三体", "novel"),
        "webnovel" => ("龙族", "webnovel"),
        "magazine" => ("读者", "magazine"),
        "all" => ("龙族", "all"),
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

/// 把正文翻译成目标语言（默认中文）—— 用户要的"英文书要能翻译"。
///
/// 复用 `game_translator` 那套引擎链与本地缓存，所以：
///   · 引擎回退：腾讯 transmart → 有道 aidemo → MyMemory → 有道词典 → Google（被墙会熔断跳过）
///   · 译文按 `lang+原文` 的 SHA-256 落盘到 translations.json，**同一章再看是秒开**
///   · 已经是中文的段落会被跳过（不会把中文再翻一遍）
///
/// ★ 按**行**提交而不是整章提交：既保住原文段落结构，也让引擎的"批量合并"
///   生效（多条短文本拼成一次请求，请求数下降一个数量级）。
#[tauri::command]
pub async fn book_translate(text: String, target: Option<String>) -> Result<String, String> {
    let target = target.unwrap_or_else(|| "zh-CN".into());
    if text.trim().is_empty() {
        return Ok(text);
    }
    // 逐行提交；空行会被原样透传（引擎层对空白文本直接返回原文）
    let lines: Vec<String> = text.split('\n').map(|l| l.to_string()).collect();
    let out = crate::game_translator::translate_texts(&lines, &target).await;
    if out.len() != lines.len() {
        // 理论上不会发生；真发生了就原样返回，别把正文搞乱
        return Ok(text);
    }
    Ok(out.join("\n"))
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

    /// 苦瓜书盘：搜索 → 书页详情 → 抓文件 → 抽正文
    #[test]
    #[ignore]
    fn live_kgbook_read_one() {
        // 三体那几本都是 pdf/mobi（阅读器读不了），所以再搜几个关键词，
        // 专门找一本 txt/epub 的把"抓文件 → 抽正文"这条链路也跑通。
        let mut list = Vec::new();
        for kw in ["三体", "红楼梦", "鲁迅"] {
            match rt().block_on(kgbook_search(kw)) {
                Ok(v) => {
                    println!("[live] 苦瓜书盘搜「{kw}」到 {} 本", v.len());
                    list.extend(v);
                }
                Err(e) => println!("[live] 搜「{kw}」失败: {e}"),
            }
        }
        assert!(!list.is_empty(), "苦瓜书盘一个关键词都搜不到？");
        for b in list.iter().take(8) {
            println!("   {} -> {}", b.title, b.read_url);
        }
        let mut got_detail = false;
        let mut read_ok = false;
        // 先把 txt/epub 的挑出来（pdf/mobi 阅读器读不了，不测那条）
        let mut readable: Vec<(Book, KgDetail)> = Vec::new();
        for b in list.iter().take(24) {
            let d = match rt().block_on(kgbook_detail(b)) {
                Ok(d) => d,
                Err(e) => { println!("   详情失败 {}: {e}", b.title); continue; }
            };
            println!("   {} | 作者={} 格式={} 大小={} 下载链={}",
                b.title, d.author, d.format, d.size,
                if d.dl_url.is_empty() { "无" } else { "有" });
            assert!(!d.dl_url.is_empty(), "书页里应该能抠到下载入口");
            got_detail = true;
            if kgbook_readable(&d) {
                readable.push((b.clone(), d));
            }
        }
        // 挑一本文件大一点的 epub —— 大文件才容易碰到"传一半断流"，正是要覆盖的场景
        readable.sort_by_key(|(_, d)| std::cmp::Reverse(d.size.len()));
        for (b, d) in readable.iter().take(4) {
            match rt().block_on(kgbook_fetch_bytes(b, d)) {
                Ok((bytes, ext)) => {
                    println!("   ★ 抓到 {} 字节, ext={}", bytes.len(), ext);
                    assert!(bytes.len() > 2000, "文件太小，肯定不对");
                    let text = if ext == "epub" {
                        epub_to_text(&bytes).unwrap_or_default()
                    } else {
                        decode_body(&bytes)
                    };
                    println!("   ★ 抽到正文 {} 字：{}", text.chars().count(),
                        text.chars().take(60).collect::<String>().replace('\n', " "));
                    if text.chars().count() > 100 {
                        read_ok = true;
                        break;
                    }
                }
                Err(e) => println!("   抓文件失败 {}: {e}", b.title),
            }
        }
        assert!(got_detail, "至少要有一本拿到详情");
        assert!(read_ok, "txt/epub 在线阅读链路必须通（抓文件 → 抽正文）");
        println!("[live] txt/epub 在线阅读链路: ✅ 通");
    }

    /// 无忧书城（中文网络小说）：搜索「龙族」→ 目录 → 抽正文
    #[test]
    #[ignore]
    fn live_wyshu_read_one() {
        let list = match rt().block_on(wyshu_search("龙族")) {
            Ok(v) => v,
            Err(e) => panic!("[live] 无忧书城搜「龙族」失败: {e}"),
        };
        println!("[live] 无忧书城搜「龙族」到 {} 本", list.len());
        for b in list.iter().take(8) {
            println!("   {} / {} -> {}", b.title, b.author, b.read_url);
        }
        assert!(
            list.iter().any(|b| b.title.contains("龙族")),
            "搜「龙族」必须能搜到龙族本体"
        );
        let Some(b) = list.iter().find(|b| b.title.contains("龙族")) else { return };
        let toc = match rt().block_on(wyshu_chapters(b)) {
            Ok(v) => v,
            Err(e) => panic!("[live] 取目录失败: {e}"),
        };
        println!("[live] 《{}》目录 {} 章，首={} 末={}", b.title, toc.len(), toc[0].name, toc.last().unwrap().name);
        assert!(toc.len() >= 5, "目录太短，肯定不对");
        let t = match rt().block_on(wyshu_content(b, 0)) {
            Ok(t) => t,
            Err(e) => panic!("[live] 取正文失败: {e}"),
        };
        println!("[live] 第 1 章「{}」{} 字：{}", t.title, t.text.chars().count(),
            t.text.chars().take(60).collect::<String>().replace('\n', " "));
        assert!(t.text.chars().count() > 300, "正文太短，肯定没抽对");
        assert_eq!(t.chapters.len(), toc.len(), "正文里要带整份目录（前端靠它翻页）");
        // 翻到第 2 章也要能读到
        let t2 = rt().block_on(wyshu_content(b, 1)).expect("第 2 章要能读");
        println!("[live] 第 2 章「{}」{} 字", t2.title, t2.text.chars().count());
        assert!(t2.text.chars().count() > 300);
    }

    /// mobi 解析：在苦瓜书盘找一本 mobi 的书（《窄门》就是 mobi），走"抓文件 → 抽正文"
    #[test]
    #[ignore]
    fn live_mobi_read_one() {
        let mut list = Vec::new();
        for kw in ["窄门", "红楼梦", "呐喊"] {
            if let Ok(v) = rt().block_on(kgbook_search(kw)) {
                list.extend(v);
            }
        }
        assert!(!list.is_empty(), "苦瓜书盘搜不到书？");
        let mut ok = false;
        for b in list.iter().take(24) {
            let Ok(d) = rt().block_on(kgbook_detail(b)) else { continue };
            if !d.format.to_lowercase().contains("mobi") {
                continue;
            }
            println!("[live] mobi 样本: {} | 格式={} 大小={}", b.title, d.format, d.size);
            match rt().block_on(kgbook_fetch_bytes(b, &d)) {
                Ok((bytes, ext)) => {
                    println!("   抓到 {} 字节, ext={}", bytes.len(), ext);
                    match crate::mobi::mobi_text_bytes(&bytes) {
                        Ok(raw) => {
                            let text = clean_book_html(&decode_body(&raw));
                            println!("   ★ mobi 解出正文 {} 字：{}", text.chars().count(),
                                text.chars().take(60).collect::<String>().replace('\n', " "));
                            if text.chars().count() > 200 {
                                ok = true;
                                break;
                            }
                        }
                        Err(e) => println!("   mobi 解析失败: {e}"),
                    }
                }
                Err(e) => println!("   抓文件失败: {e}"),
            }
        }
        assert!(ok, "至少要有本 mobi 能解出正文（用户报「读不了 mobi」就是这条）");
    }

    /// 封面：列表接口拿不到图（分类页/结果页都没有 <img>），只能靠详情页补。
    /// 这条就是防"封面又变成空"的回归。
    #[test]
    #[ignore]
    fn live_kgbook_covers() {
        let mut v = rt().block_on(kgbook_home("kehuanxuanhuan")).expect("分类页要能拉到");
        assert!(!v.is_empty());
        let before = v.iter().filter(|b| !b.cover.is_empty()).count();
        println!("[live] 补封面之前有图的: {before}/{}", v.len());
        rt().block_on(kgbook_fill_covers(&mut v, 12));
        let got: Vec<_> = v.iter().filter(|b| !b.cover.is_empty()).collect();
        println!("[live] 补完之后有图的: {}/{}", got.len(), v.len());
        for b in got.iter().take(5) {
            println!("   {} -> {}", b.title, b.cover);
        }
        assert!(!got.is_empty(), "一本书的封面都补不到 = 详情页解析坏了");
        assert!(got[0].cover.starts_with("http"), "封面必须是绝对地址: {}", got[0].cover);
        // 封面图本身要能下（200 + 是图片）。★ 整个请求链都要在 block_on 里 —— 
        // reqwest 的 send() 必须在 tokio 运行时上下文里调用，否则报 "there is no reactor running"。
        let r = rt().block_on(async {
            let cli = client_fast()?;
            cli.get(&got[0].cover)
                .header("Referer", KGBOOK_BASE)
                .send()
                .await
                .map_err(|e| format!("{e}"))
        });
        match r {
            Ok(resp) => {
                println!("[live] 封面 HTTP {} content-type={:?}", resp.status().as_u16(),
                    resp.headers().get("content-type").and_then(|v| v.to_str().ok()));
                assert!(resp.status().is_success(), "封面图要能下");
            }
            Err(e) => panic!("封面图请求失败: {e}"),
        }
    }

    /// 前端 chip 对应的两条链路：中文小说(novel) / 期刊杂志(magazine)
    /// 首页拉分类 + 搜索（搜索只排序不过滤，保证"搜得到"）
    #[test]
    #[ignore]
    fn live_book_chips() {
        for sec in ["novel", "magazine"] {
            match rt().block_on(home(sec)) {
                Ok(v) => {
                    println!("[live] 首页 {sec}: {} 本", v.len());
                    for b in v.iter().take(3) { println!("   {} -> {}", b.title, b.read_url); }
                    assert!(!v.is_empty(), "{sec} 首页不该是空的");
                }
                Err(e) => panic!("[live] {sec} 首页失败: {e}"),
            }
        }
        // 期刊：搜"读者"。站内搜索对"读者"一条杂志都不返回（命中的是简介里带"读者"的科普书），
        // 必须靠分类页本地匹配补上 —— 这条断言就是防这个回归的。
        match rt().block_on(search("magazine", "读者", 1)) {
            Ok(v) => {
                println!("[live] 期刊搜「读者」: {} 条", v.len());
                for b in v.iter().take(5) { println!("   {} ({})", b.title, b.key); }
                let mag = v.iter().filter(|b| b.key.starts_with("kgbook:qikanzazhi/")).count();
                println!("[live] 其中杂志分类 {mag} 条，第一条 = {}", v.first().map(|b| b.title.clone()).unwrap_or_default());
                assert!(mag > 0, "「读者」搜不到任何杂志分类的书 = 分类页本地匹配失效");
                assert!(v[0].key.starts_with("kgbook:qikanzazhi/"), "杂志命中必须排最前面");
            }
            Err(e) => panic!("[live] 期刊搜「读者」失败: {e}"),
        }
        // 小说：搜"三体"必须有结果（这是用户最直接的抱怨：搜不到）
        match rt().block_on(search("novel", "三体", 1)) {
            Ok(v) => {
                println!("[live] 小说搜「三体」: {} 条", v.len());
                for b in v.iter().take(4) { println!("   {} ({})", b.title, b.key); }
                assert!(!v.is_empty(), "「三体」都搜不到就说明搜源又坏了");
            }
            Err(e) => panic!("[live] 小说搜「三体」失败: {e}"),
        }
    }

    /// 翻译链：英文 → 中文（复用 game_translator 的引擎链）
    #[test]
    #[ignore]
    fn live_translate_chain() {
        let en = "It was the best of times, it was the worst of times.".to_string();
        let out = rt().block_on(crate::game_translator::translate_texts(&[en.clone()], "zh-CN"));
        println!("[live] 原文: {en}");
        println!("[live] 译文: {}", out.first().cloned().unwrap_or_default());
        let t = out.first().cloned().unwrap_or_default();
        assert!(!t.is_empty(), "翻译不该为空");
        assert!(t.chars().any(|c| matches!(c, '\u{4E00}'..='\u{9FFF}')), "译文里应该有汉字: {t}");
        // 多行也要能保住结构
        let multi: Vec<String> = vec!["Hello world.".into(), "".into(), "Good night.".into()];
        let o2 = rt().block_on(crate::game_translator::translate_texts(&multi, "zh-CN"));
        println!("[live] 多行: {:?}", o2);
        assert_eq!(o2.len(), 3, "行数要保住");
        assert_eq!(o2[1], "", "空行原样透传");
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
