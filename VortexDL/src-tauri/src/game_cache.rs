//! 浏览缓存（持久层）
//!
//! 用户要求：**只要浏览到的游戏就缓存**——封面图、下载链接、中英文名。
//! 之前只有内存里按页缓存的列表，重启就没了；详情页每次点进去都重新爬一遍，
//! 弱网下体感非常慢。这里把浏览过的数据落到 `data/cache/`：
//!
//! ```text
//! data/cache/game_cache.json   详情 + 下载链接（按 source|detail_url 索引）
//! data/cache/names.json        中英文名索引（搜索用的本地别名表）
//! data/cache/images/<sha>.bin  封面图（附带 index.json 记录 url→文件）
//! ```
//!
//! 读路径全部是内存命中，写盘做了节流（30 秒或 50 次写入才落盘一次），
//! 不会因为浏览列表就频繁写 JSON。

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::search_engine::{DownloadLink, GameCard, GameDetail};

/// 详情缓存有效期：7 天。详情页的标题/简介/封面基本不变。
const DETAIL_TTL_SECS: i64 = 7 * 24 * 3600;
/// 下载链接有效期：6 小时。链接是带签名的，过期后引擎会重取，但缓存里
/// 先给一份旧的能让详情页立刻可用（过期由下载时的 412/403 兜底）。
const LINKS_TTL_SECS: i64 = 6 * 3600;
/// 名称索引不过期（游戏名不会变）
const NAMES_TTL_SECS: i64 = 365 * 24 * 3600;
/// 封面图最多缓存多少个（按 LRU 时间淘汰）
const IMAGE_MAX_ENTRIES: usize = 3000;
/// 单张封面图上限（超过不缓存）
const IMAGE_MAX_BYTES: usize = 12 * 1024 * 1024;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn cache_dir() -> PathBuf {
    crate::paths::data_dir().join("cache")
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct DetailEntry {
    #[serde(default)] detail: GameDetail,
    #[serde(default)] updated_at: i64,
    #[serde(default)] downloads: Vec<DownloadLink>,
    #[serde(default)] downloads_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct NameEntry {
    #[serde(default)] name: String,
    #[serde(default)] name_original: String,
    #[serde(default)] name_cn: String,
    // 预计算的小写副本：4 万条名字每次搜索都现 to_lowercase() 太慢
    #[serde(default)] name_lc: String,
    #[serde(default)] orig_lc: String,
    #[serde(default)] cn_lc: String,
    #[serde(default)] source: String,
    #[serde(default)] detail_url: String,
    #[serde(default)] cover: String,
    #[serde(default)] appid: String,
    #[serde(default)] updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ImageEntry {
    #[serde(default)] file: String,
    #[serde(default)] content_type: String,
    #[serde(default)] bytes: u64,
    #[serde(default)] accessed_at: i64,
    #[serde(default)] url: String,
}

#[derive(Serialize, Deserialize, Default)]
struct CacheFile {
    #[serde(default)] details: HashMap<String, DetailEntry>,
    #[serde(default)] names: HashMap<String, NameEntry>,
    #[serde(default)] images: HashMap<String, ImageEntry>,
    #[serde(default)] version: u32,
}

struct Store {
    data: Mutex<CacheFile>,
    dirty: AtomicBool,
    writes: AtomicU64,
    last_save: AtomicU64,
}

static STORE: OnceLock<Store> = OnceLock::new();

fn store() -> &'static Store {
    STORE.get_or_init(|| {
        let data = load();
        Store {
            data: Mutex::new(data),
            dirty: AtomicBool::new(false),
            writes: AtomicU64::new(0),
            last_save: AtomicU64::new(now_secs() as u64),
        }
    })
}

fn load() -> CacheFile {
    let p = cache_dir().join("game_cache.json");
    match std::fs::read(&p) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
        Err(_) => CacheFile::default(),
    }
}

/// 落盘节流：写满 50 次或距上次落盘 30 秒才真正写文件。
fn mark_dirty() {
    let st = store();
    st.dirty.store(true, Ordering::Relaxed);
    let n = st.writes.fetch_add(1, Ordering::Relaxed) + 1;
    let now = now_secs() as u64;
    let last = st.last_save.load(Ordering::Relaxed);
    if n % 500 == 0 || now.saturating_sub(last) >= 30 {
        save_now();
    }
}

pub fn save_now() {
    let st = store();
    if !st.dirty.swap(false, Ordering::Relaxed) {
        return;
    }
    let dir = cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    let snapshot = {
        let g = st.data.lock();
        serde_json::to_vec(&*g).unwrap_or_default()
    };
    if snapshot.is_empty() {
        return;
    }
    let tmp = dir.join("game_cache.json.tmp");
    if std::fs::write(&tmp, &snapshot).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join("game_cache.json"));
    }
    st.last_save.store(now_secs() as u64, Ordering::Relaxed);
}

fn key_of(source: &str, detail_url: &str) -> String {
    format!("{}|{}", source.trim(), detail_url.trim())
}

// ===================== 详情 / 下载链接 =====================

/// 命中且未过期的详情。`allow_stale=true` 时过期也返回（用于先展示、后刷新）。
pub fn get_detail(source: &str, detail_url: &str, allow_stale: bool) -> Option<GameDetail> {
    let st = store();
    let g = st.data.lock();
    let e = g.details.get(&key_of(source, detail_url))?;
    if e.detail.title.is_empty() && e.detail.cover.is_empty() && e.detail.downloads.is_empty() {
        return None;
    }
    let age = now_secs() - e.updated_at;
    if !allow_stale && age > DETAIL_TTL_SECS {
        return None;
    }
    let mut d = e.detail.clone();
    if d.downloads.is_empty() {
        d.downloads = e.downloads.clone();
    }
    Some(d)
}

pub fn put_detail(source: &str, detail_url: &str, detail: &GameDetail) {
    if detail.title.trim().is_empty() && detail.cover.trim().is_empty() {
        return;
    }
    let k = key_of(source, detail_url);
    let mut g = store().data.lock();
    let e = g.details.entry(k).or_default();
    e.detail = detail.clone();
    e.updated_at = now_secs();
    if !detail.downloads.is_empty() {
        e.downloads = detail.downloads.clone();
        e.downloads_at = now_secs();
    }
    drop(g);
    mark_dirty();
}

pub fn get_downloads(source: &str, detail_url: &str, allow_stale: bool) -> Option<Vec<DownloadLink>> {
    let st = store();
    let g = st.data.lock();
    let e = g.details.get(&key_of(source, detail_url))?;
    if e.downloads.is_empty() {
        return None;
    }
    if !allow_stale && now_secs() - e.downloads_at > LINKS_TTL_SECS {
        return None;
    }
    Some(e.downloads.clone())
}

pub fn put_downloads(source: &str, detail_url: &str, links: &[DownloadLink]) {
    if links.is_empty() {
        return;
    }
    let k = key_of(source, detail_url);
    let mut g = store().data.lock();
    let e = g.details.entry(k).or_default();
    e.downloads = links.to_vec();
    e.downloads_at = now_secs();
    // 详情里也同步一份，详情页直接就能拿到链接
    if e.detail.downloads.is_empty() {
        e.detail.downloads = links.to_vec();
    }
    drop(g);
    mark_dirty();
}

// ===================== 中英文名索引 =====================

fn name_key(source: &str, detail_url: &str, appid: &str) -> String {
    if !appid.trim().is_empty() {
        return format!("id:{}", appid.trim());
    }
    if !detail_url.trim().is_empty() {
        return format!("u:{}", detail_url.trim());
    }
    format!("n:{}|{}", source.trim(), appid)
}

/// 记住一张卡片的中英文名（单条入口，内部转批量）。
pub fn remember_card(card: &GameCard) {
    remember_cards(std::slice::from_ref(card));
}

/// ★ 批量记住（2026-10-18）：一次搜索可能有 4 万张卡，逐条加锁 + 逐条
///   mark_dirty 会触发几百次 12MB 的 JSON 落盘 —— 实测把一次搜索拖到 23 秒。
///   这里只加一次锁、只在真的有字段变化时算一次 dirty。
pub fn remember_cards(cards: &[GameCard]) {
    if cards.is_empty() {
        return;
    }
    let now = now_secs();
    let mut g = store().data.lock();
    let mut changed = 0usize;
    for card in cards {
        if card.name.trim().is_empty() && card.name_cn.trim().is_empty() {
            continue;
        }
        let k = name_key(&card.source, &card.detail_url, &card.appid);
        let e = g.names.entry(k).or_default();
        let mut dirty = false;
        macro_rules! set {
            ($field:ident, $val:expr) => {{
                let v = $val;
                if !v.is_empty() && e.$field != v {
                    e.$field = v.to_string();
                    dirty = true;
                }
            }};
        }
        set!(name, card.name.trim());
        set!(name_original, card.name_original.trim());
        set!(name_cn, card.name_cn.trim());
        set!(source, card.source.trim());
        set!(detail_url, card.detail_url.trim());
        set!(cover, card.header_image.trim());
        set!(appid, card.appid.trim());
        if dirty {
            e.updated_at = now;
            e.name_lc = e.name.to_lowercase();
            e.orig_lc = e.name_original.to_lowercase();
            e.cn_lc = e.name_cn.to_lowercase();
            changed += 1;
        }
    }
    drop(g);
    if changed > 0 {
        mark_dirty();
    }
}

/// 记住详情页拿到的名字（详情页的标题通常比列表更准确）。
pub fn remember_detail(source: &str, detail_url: &str, detail: &GameDetail, appid: &str) {
    if detail.title.trim().is_empty() {
        return;
    }
    let k = name_key(source, detail_url, appid);
    let mut g = store().data.lock();
    let e = g.names.entry(k).or_default();
    if e.name.trim().is_empty() { e.name = detail.title.clone(); }
    if !source.trim().is_empty() { e.source = source.to_string(); }
    if !detail_url.trim().is_empty() { e.detail_url = detail_url.to_string(); }
    if !detail.cover.trim().is_empty() { e.cover = detail.cover.clone(); }
    if !appid.trim().is_empty() { e.appid = appid.to_string(); }
    e.updated_at = now_secs();
    e.name_lc = e.name.to_lowercase();
    e.orig_lc = e.name_original.to_lowercase();
    e.cn_lc = e.name_cn.to_lowercase();
    drop(g);
    mark_dirty();
}

/// 在本地名字索引里搜（中/英/原名都匹配），返回合成卡片。
/// 这是「翻译慢导致中文搜不到」的兜底：只要这个游戏被浏览过，本地就一定有它的中文名。
pub fn search_names(query: &str, limit: usize) -> Vec<GameCard> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let st = store();
    let g = st.data.lock();
    let now = now_secs();
    let mut out: Vec<(i32, GameCard)> = Vec::new();
    for (k, e) in g.names.iter() {
        if now - e.updated_at > NAMES_TTL_SECS {
            continue;
        }
        // 优先用预计算的小写副本；老数据没有时现算一次（只影响升级后的第一次搜索）
        let name = if e.name_lc.is_empty() { e.name.to_lowercase() } else { e.name_lc.clone() };
        let orig = if e.orig_lc.is_empty() { e.name_original.to_lowercase() } else { e.orig_lc.clone() };
        let cn = if e.cn_lc.is_empty() { e.name_cn.to_lowercase() } else { e.cn_lc.clone() };
        let score = if cn == q || name == q || orig == q {
            100
        } else if cn.starts_with(&q) || name.starts_with(&q) || orig.starts_with(&q) {
            80
        } else if cn.contains(&q) || name.contains(&q) || orig.contains(&q) {
            60
        } else {
            0
        };
        if score == 0 {
            continue;
        }
        let appid = if e.appid.is_empty() {
            k.trim_start_matches("id:").to_string()
        } else {
            e.appid.clone()
        };
        out.push((
            score,
            GameCard {
                name: e.name.clone(),
                name_original: e.name_original.clone(),
                name_cn: e.name_cn.clone(),
                appid,
                source: e.source.clone(),
                detail_url: e.detail_url.clone(),
                header_image: e.cover.clone(),
                category: String::new(),
                update_time: e.updated_at,
                extra: serde_json::Value::Null,
                tags: Vec::new(),
            },
        ));
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().take(limit.max(1)).map(|(_, c)| c).collect()
}

/// 给一批卡片补上缓存里的中文名（列表里 name_cn 为空时用）。只加一次锁。
pub fn enrich_cards(cards: &mut [GameCard]) {
    let g = store().data.lock();
    for card in cards.iter_mut() {
        if !card.name_cn.trim().is_empty() {
            continue;
        }
        let k = name_key(&card.source, &card.detail_url, &card.appid);
        if let Some(e) = g.names.get(&k) {
            if !e.name_cn.trim().is_empty() {
                card.name_cn = e.name_cn.clone();
            }
            if card.header_image.trim().is_empty() && !e.cover.trim().is_empty() {
                card.header_image = e.cover.clone();
            }
        }
    }
}

/// 给一张卡片补上缓存里的中文名（单条入口）。
pub fn enrich_card(card: &mut GameCard) {
    enrich_cards(std::slice::from_mut(card));
}

/// 翻译完成后把中文名回写到名字索引（只更新**已存在**的条目）。
/// 不新建条目：只有 appid + 中文名、没有详情链接的卡片点了会打开空白页。
pub fn set_name_cn(appid: &str, name_cn: &str) {
    if appid.trim().is_empty() || name_cn.trim().is_empty() {
        return;
    }
    let mut g = store().data.lock();
    if let Some(e) = g.names.get_mut(&format!("id:{}", appid.trim())) {
        if e.name_cn != name_cn {
            e.name_cn = name_cn.to_string();
            e.cn_lc = e.name_cn.to_lowercase();
            e.updated_at = now_secs();
            drop(g);
            mark_dirty();
        }
    }
}

// ===================== 封面图 =====================

fn image_hash(url: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(url.as_bytes());
    let d = h.finalize();
    d.iter().take(16).map(|b| format!("{:02x}", b)).collect()
}

fn image_dir() -> PathBuf {
    cache_dir().join("images")
}

/// 读本地图片缓存：返回 (bytes, content_type)
pub fn get_image(url: &str) -> Option<(Vec<u8>, String)> {
    let st = store();
    let file = {
        let mut g = st.data.lock();
        let e = g.images.get_mut(url)?;
        e.accessed_at = now_secs();
        e.file.clone()
    };
    let p = image_dir().join(file);
    let bytes = std::fs::read(&p).ok()?;
    let ct = {
        let g = st.data.lock();
        g.images
            .get(url)
            .map(|e| e.content_type.clone())
            .unwrap_or_else(|| "image/jpeg".to_string())
    };
    Some((bytes, ct))
}

/// 写本地图片缓存（超过上限不写）
pub fn put_image(url: &str, bytes: &[u8], content_type: &str) {
    if bytes.is_empty() || bytes.len() > IMAGE_MAX_BYTES {
        return;
    }
    let dir = image_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let ext = match content_type {
        ct if ct.contains("png") => "png",
        ct if ct.contains("webp") => "webp",
        ct if ct.contains("gif") => "gif",
        ct if ct.contains("avif") => "avif",
        _ => "jpg",
    };
    let file = format!("{}.{}", image_hash(url), ext);
    if std::fs::write(dir.join(&file), bytes).is_err() {
        return;
    }
    let st = store();
    let mut g = st.data.lock();
    g.images.insert(
        url.to_string(),
        ImageEntry {
            file,
            content_type: content_type.to_string(),
            bytes: bytes.len() as u64,
            accessed_at: now_secs(),
            url: url.to_string(),
        },
    );
    // LRU 淘汰
    if g.images.len() > IMAGE_MAX_ENTRIES {
        let mut v: Vec<(String, i64)> = g.images.iter().map(|(k, e)| (k.clone(), e.accessed_at)).collect();
        v.sort_by_key(|(_, t)| *t);
        let drop_n = g.images.len() - IMAGE_MAX_ENTRIES;
        for (k, _) in v.into_iter().take(drop_n) {
            if let Some(e) = g.images.remove(&k) {
                let _ = std::fs::remove_file(image_dir().join(e.file));
            }
        }
    }
    drop(g);
    mark_dirty();
}

// ===================== 统计 / 清理 =====================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CacheStats {
    pub details: usize,
    pub names: usize,
    pub images: usize,
    pub image_bytes: u64,
    pub disk_bytes: u64,
}

pub fn stats() -> CacheStats {
    let st = store();
    let g = st.data.lock();
    let mut s = CacheStats {
        details: g.details.len(),
        names: g.names.len(),
        images: g.images.len(),
        image_bytes: g.images.values().map(|e| e.bytes).sum(),
        disk_bytes: 0,
    };
    for p in [
        cache_dir().join("game_cache.json"),
        cache_dir().join("names.json"),
    ] {
        if let Ok(md) = std::fs::metadata(p) {
            s.disk_bytes += md.len();
        }
    }
    // 封面图文件也计入磁盘占用（之前只统计 JSON，设置页会显示 0）
    if let Ok(rd) = std::fs::read_dir(image_dir()) {
        for e in rd.flatten() {
            if let Ok(md) = e.metadata() {
                if md.is_file() {
                    s.disk_bytes += md.len();
                }
            }
        }
    }
    s
}

pub fn clear() -> String {
    let st = store();
    let (nd, ni) = {
        let mut g = st.data.lock();
        let nd = g.details.len();
        let ni = g.images.len();
        g.details.clear();
        g.images.clear();
        g.names.clear();
        (nd, ni)
    };
    let _ = std::fs::remove_dir_all(image_dir());
    st.dirty.store(true, Ordering::Relaxed);
    save_now();
    crate::app_logger::log_cache("GAME_CACHE_CLEAR", &format!("details={} images={}", nd, ni));
    format!("已清理浏览缓存：{} 个详情、{} 张封面", nd, ni)
}

// ===================== 封面图代理协议 vxasset:// =====================

/// 从 `vxasset://localhost/img?u=<encoded>` 里取出真实图片地址
fn query_url(uri: &str) -> Option<String> {
    let q = uri.split_once('?')?.1;
    for pair in q.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == "u" {
            let decoded = urlencoding::decode(v).ok()?.to_string();
            if decoded.starts_with("http://") || decoded.starts_with("https://") {
                return Some(decoded);
            }
            return None;
        }
    }
    None
}

/// 从 `?f=<encoded 绝对路径>` 取本地图片路径（只允许图片扩展名，避免被当成任意文件读取）
fn query_local(uri: &str) -> Option<String> {
    let q = uri.split_once('?')?.1;
    for pair in q.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == "f" {
            let decoded = urlencoding::decode(v).ok()?.to_string();
            let low = decoded.to_lowercase();
            if low.ends_with(".png")
                || low.ends_with(".jpg")
                || low.ends_with(".jpeg")
                || low.ends_with(".webp")
                || low.ends_with(".bmp")
                || low.ends_with(".gif")
            {
                return Some(decoded);
            }
            return None;
        }
    }
    None
}

fn referer_for(url: &str) -> String {
    if url.contains("playzip.com") || url.contains("koyso") {
        "https://playzip.com/".to_string()
    } else if url.contains("byrutgame.org") {
        "https://byrutgame.org/".to_string()
    } else if url.contains("steamstatic.com") || url.contains("steamcdn") {
        "https://store.steampowered.com/".to_string()
    } else {
        // 默认用图片自身的 origin 当 Referer（很多图床只校验同源）
        match url.split_once("://").and_then(|(s, r)| r.split_once('/').map(|(h, _)| (s, h))) {
            Some((s, h)) => format!("{}://{}/", s, h),
            None => String::new(),
        }
    }
}

/// 图片代理：命中本地缓存直接返回；未命中则联网抓一次并落盘。
/// 返回 (status, body, content_type)。
pub async fn serve_image(uri: &str) -> (u16, Vec<u8>, String) {
    // ★ 2026-10-19: 支持本地文件（`?f=<绝对路径>`）—— 模拟器页要显示下载游戏自带的图标
    if let Some(local) = query_local(uri) {
        return match std::fs::read(&local) {
            Ok(bytes) => {
                let ct = match local.to_lowercase() {
                    p if p.ends_with(".png") => "image/png",
                    p if p.ends_with(".jpg") || p.ends_with(".jpeg") => "image/jpeg",
                    p if p.ends_with(".webp") => "image/webp",
                    p if p.ends_with(".gif") => "image/gif",
                    p if p.ends_with(".bmp") => "image/bmp",
                    _ => "application/octet-stream",
                };
                (200, bytes, ct.to_string())
            }
            Err(_) => (404, b"not found".to_vec(), "text/plain".to_string()),
        };
    }
    let url = match query_url(uri) {
        Some(u) => u,
        None => return (400, b"bad request".to_vec(), "text/plain".to_string()),
    };
    if let Some((b, ct)) = get_image(&url) {
        return (200, b, ct);
    }
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .connect_timeout(std::time::Duration::from_secs(8))
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
        )
        .build()
    {
        Ok(c) => c,
        Err(_) => return (500, Vec::new(), "text/plain".to_string()),
    };
    let mut req = client.get(&url);
    let rf = referer_for(&url);
    if !rf.is_empty() {
        req = req.header(reqwest::header::REFERER, rf);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            crate::app_logger::log_cache("IMG_FAIL", &format!("url={} err={}", url, e));
            return (502, Vec::new(), "text/plain".to_string());
        }
    };
    if !resp.status().is_success() {
        return (resp.status().as_u16(), Vec::new(), "text/plain".to_string());
    }
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();
    let bytes = match resp.bytes().await {
        Ok(b) => b.to_vec(),
        Err(_) => return (502, Vec::new(), "text/plain".to_string()),
    };
    if ct.starts_with("image/") {
        put_image(&url, &bytes, &ct);
        crate::app_logger::log_cache("IMG_PUT", &format!("url={} bytes={}", url, bytes.len()));
    }
    (200, bytes, ct)
}

