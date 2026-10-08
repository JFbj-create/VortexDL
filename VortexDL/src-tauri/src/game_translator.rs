use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use parking_lot::Mutex;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TranslateEngine { Local, #[default] Online }
#[derive(Debug, Clone, Default)]
pub struct TranslateConfig {
    pub engine: TranslateEngine,
    pub libretranslate_url: String,
    pub target_lang: String,
    pub auto_backup: bool,
    pub concurrency: usize,
}
#[derive(Debug, Clone, Default)]
pub struct TranslateReport {
    pub translated_files: usize,
    pub translated_lines: usize,
    pub skipped: usize,
}

// ===================== 翻译引擎 + 本地持久缓存 =====================
// 翻译引擎回退链: Google gtx (translate.googleapis.com) → Google gtx (clients5.google.com) → MyMemory
// 国内网络环境下 Google 端点可能被墙, MyMemory 作为最终兜底 (免密钥, 支持中英俄)
//
// 优化 (2026-09-05):
//  1. 共享 reqwest Client (连接复用, 免去每请求重建 TLS 握手)
//  2. 引擎熔断: 连续 2 次失败 → 屏蔽 10 分钟 (避免每条文本反复撞被墙端点 3s+ 超时)
//  3. 批量合并: 多条短文本用 \n 拼接送一次请求 (≤450 字符), 请求数 ~10x 下降,
//     MyMemory 每日 5000 字额度消耗同样 ~10x 下降; 拆分数量不匹配时自动回退逐条
//  4. 同文缓存: 引擎成功返回原文 (专有名词不可译) 也写入缓存 → 不再重复消耗额度
//
// 本地缓存: exe 同目录 translations.json, 跨重启持久生效
//   - 第一次打开: 联网翻译全部游戏名/简介并落盘
//   - 后续打开: 缓存命中直接返回, 只对新出现的游戏联网翻译
//   - best_engine 记录最近成功的引擎, 下次启动优先从它开始

// 引擎编号: 0=googleapis 1=clients5 2=mymemory
const ENGINE_COUNT: u8 = 3;

#[derive(Serialize, Deserialize)]
struct CacheFile {
    entries: HashMap<String, String>,
    #[serde(default)]
    best_engine: u8,
}

struct Store {
    map: Mutex<HashMap<String, String>>,
    dirty: AtomicBool,
    best_engine: AtomicU8,
}

static STORE: OnceLock<Store> = OnceLock::new();

fn store() -> &'static Store {
    STORE.get_or_init(|| {
        let (map, best) = load_cache_file();
        Store {
            map: Mutex::new(map),
            dirty: AtomicBool::new(false),
            best_engine: AtomicU8::new(best.min(ENGINE_COUNT - 1)),
        }
    })
}

fn cache_file() -> PathBuf {
    std::env::current_exe().ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("translations.json")
}

fn load_cache_file() -> (HashMap<String, String>, u8) {
    let raw = match std::fs::read_to_string(cache_file()) {
        Ok(s) => s,
        Err(_) => return (HashMap::new(), 0),
    };
    // 新格式: {"entries":{...},"best_engine":N}
    if let Ok(f) = serde_json::from_str::<CacheFile>(&raw) {
        return (f.entries, f.best_engine);
    }
    // 旧格式: 平铺 HashMap (兼容升级前的缓存)
    if let Ok(m) = serde_json::from_str::<HashMap<String, String>>(&raw) {
        return (m, 0);
    }
    (HashMap::new(), 0)
}

fn save_cache_file() {
    let st = store();
    let f = CacheFile {
        entries: st.map.lock().clone(),
        best_engine: st.best_engine.load(Ordering::Relaxed),
    };
    let json = match serde_json::to_string(&f) {
        Ok(j) => j,
        Err(_) => return,
    };
    let path = cache_file();
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn cache_key(lang: &str, text: &str) -> String {
    let mut h = Sha256::new();
    h.update(lang.as_bytes());
    h.update(b"\n");
    h.update(text.as_bytes());
    h.finalize().iter().map(|b| format!("{:02x}", b)).collect()
}

// 文本是否已是目标语言 (中文占比 >= 30% 视为已翻译, 跳过请求)
fn is_target_lang_text(text: &str) -> bool {
    let mut cjk = 0usize;
    let mut latin = 0usize;
    for ch in text.chars() {
        if matches!(ch, '\u{4E00}'..='\u{9FFF}') { cjk += 1; }
        else if ch.is_alphabetic() { latin += 1; }
    }
    let total = cjk + latin;
    total > 0 && cjk * 100 >= total * 30
}

// 长文本分块 (MyMemory 单次 500 字符上限, 取 450 保守分块; Google 端点同样适用)
fn split_text(text: &str, max_chars: usize) -> Vec<String> {
    if text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    // 按句子边界 (。！？.!? 换行) 切分, 超长句硬切
    for sen in text.split_inclusive(|c| "。！？!?\n".contains(c) || c == '.') {
        let sen_len = sen.chars().count();
        if sen_len > max_chars {
            if !cur.is_empty() { parts.push(std::mem::take(&mut cur)); cur_len = 0; }
            let chars: Vec<char> = sen.chars().collect();
            for chunk in chars.chunks(max_chars) {
                parts.push(chunk.iter().collect());
            }
            continue;
        }
        if cur_len + sen_len > max_chars {
            parts.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        cur.push_str(sen);
        cur_len += sen_len;
    }
    if !cur.is_empty() { parts.push(cur); }
    parts
}

// ===================== 共享 HTTP 客户端 =====================
// 连接池复用: 之前每请求 new 一个 Client, 每次都重新 DNS + TCP + TLS 握手, 批量翻译时极慢
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
            .connect_timeout(std::time::Duration::from_secs(3))  // 被墙端点 3s 内快速失败
            .timeout(std::time::Duration::from_secs(5))
            .gzip(true)
            .pool_max_idle_per_host(8)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()
            .unwrap_or_default()
    })
}

// ===================== 引擎熔断器 =====================
// 连续失败 N 次 → 屏蔽 COOLDOWN_MS (避免每条文本都重撞被墙端点/配额耗尽的引擎)
struct BreakerState {
    fails: [AtomicU8; ENGINE_COUNT as usize],
    blocked_until: [AtomicU64; ENGINE_COUNT as usize],
}
static BREAKER: OnceLock<BreakerState> = OnceLock::new();
fn breaker() -> &'static BreakerState {
    BREAKER.get_or_init(|| BreakerState {
        fails: std::array::from_fn(|_| AtomicU8::new(0)),
        blocked_until: std::array::from_fn(|_| AtomicU64::new(0)),
    })
}
const BREAKER_THRESHOLD: u8 = 3;
const BREAKER_COOLDOWN_MS: u64 = 2 * 60 * 1000;

fn breaker_blocked(idx: u8) -> bool {
    let b = breaker();
    let until = b.blocked_until[idx as usize].load(Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    until > now
}

fn breaker_on_success(idx: u8) {
    let b = breaker();
    b.fails[idx as usize].store(0, Ordering::Relaxed);
    b.blocked_until[idx as usize].store(0, Ordering::Relaxed);
}

fn breaker_on_fail(idx: u8) {
    let b = breaker();
    let f = b.fails[idx as usize].fetch_add(1, Ordering::Relaxed) + 1;
    if f >= BREAKER_THRESHOLD {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        b.blocked_until[idx as usize].store(now + BREAKER_COOLDOWN_MS, Ordering::Relaxed);
    }
}

// ===================== 引擎实现 =====================
// Google gtx 免费接口 (client=gtx, 无需密钥), host 可为 translate.googleapis.com / clients5.google.com
async fn google_gtx_translate(host: &str, text: &str, tl: &str) -> Result<String, String> {
    let url = format!(
        "https://{}/translate_a/single?client=gtx&sl=auto&tl={}&dt=t&q={}",
        host,
        tl,
        urlencoding::encode(text)
    );
    let resp = http_client().get(&url)
        .header("Accept", "application/json, text/plain, */*")
        .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .send().await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let body = resp.text().await.map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    // 响应格式: [[["译文","原文",...],["译文2","原文2",...]], ...]
    let mut out = String::new();
    if let Some(segs) = v.get(0).and_then(|x| x.as_array()) {
        for seg in segs {
            if let Some(t) = seg.get(0).and_then(|x| x.as_str()) {
                out.push_str(t);
            }
        }
    }
    if out.trim().is_empty() { Err("empty result".into()) } else { Ok(out) }
}

// 源语言检测 (MyMemory 不支持 auto, 按字符集推断: 西里尔→ru, 其余→en)
fn detect_source_lang(text: &str) -> &'static str {
    let mut cyr = 0usize;
    let mut lat = 0usize;
    for ch in text.chars() {
        if matches!(ch, '\u{0400}'..='\u{04FF}') { cyr += 1; }
        else if ch.is_ascii_alphabetic() { lat += 1; }
    }
    if cyr > lat { "ru" } else { "en" }
}

// MyMemory 免费接口 (免密钥; 单次 ≤500 字符; 匿名 5000 字/天, 带 de= 邮箱提升额度)
async fn mymemory_translate(text: &str, tl: &str) -> Result<String, String> {
    let sl = detect_source_lang(text);
    let url = format!(
        "https://api.mymemory.translated.net/get?q={}&langpair={}|{}&de=vortexdl.translator@outlook.com",
        urlencoding::encode(text),
        sl,
        tl
    );
    let resp = http_client().get(&url)
        .header("Accept", "application/json, text/plain, */*")
        .send().await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let body = resp.text().await.map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let t = v.get("responseData")
        .and_then(|d| d.get("translatedText"))
        .and_then(|x| x.as_str())
        .ok_or("no translatedText")?;
    // MyMemory 配额耗尽/参数错误时会把错误消息放进 translatedText
    let lower = t.to_lowercase();
    if lower.contains("mymemory warning") || lower.contains("query limit") || t.trim().is_empty() {
        return Err(t.to_string());
    }
    Ok(t.to_string())
}

// 有道词典 API (免密钥, 适合游戏名/短语翻译)
// 响应: web_trans.web-translation[0].trans[0].value
async fn youdao_translate(text: &str, tl: &str) -> Result<String, String> {
    let sl = detect_source_lang(text);
    let target = if tl.starts_with("zh") { "zh" } else { tl };

    // 生成查询变体: 完整名 → 去数字 → 去副标题 → 去版本词 → 取前2词
    // 有道词典对 "Crash Bandicoot 4" 无结果, 但 "Crash Bandicoot" 有
    let mut variants: Vec<String> = vec![text.to_string()];
    let stripped = text.split(|c: char| c == ':' || c == '：' || c == '-' || c == '–').next().unwrap_or(text).trim().to_string();
    if stripped != text && !stripped.is_empty() { variants.push(stripped.clone()); }
    // 去掉末尾数字 (如 "Crash Bandicoot 4" → "Crash Bandicoot")
    let no_num = regex::Regex::new(r"\s+\d+\s*$").unwrap().replace(text, "").trim().to_string();
    if no_num != text && !no_num.is_empty() { variants.push(no_num); }
    let no_num2 = regex::Regex::new(r"\s+\d+\s*$").unwrap().replace(&stripped, "").trim().to_string();
    if no_num2 != stripped && !no_num2.is_empty() { variants.push(no_num2); }
    // 去掉版本词
    let ver_re = regex::Regex::new(r"(?i)\b(edition|version|remastered|remake|hd|definitive|complete|goty|deluxe|ultimate|enhanced|gold|platinum|special)\b\.?").unwrap();
    let no_ver = ver_re.replace_all(text, "").trim().to_string();
    if no_ver != text && !no_ver.is_empty() { variants.push(no_ver); }

    for q in &variants {
        if q.is_empty() { continue; }
        let url = format!(
            "https://dict.youdao.com/jsonapi?q={}&le={}&t={}",
            urlencoding::encode(q),
            sl,
            target
        );
        let resp = match http_client().get(&url)
            .header("Accept", "application/json")
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
            .send().await {
                Ok(r) => r,
                Err(_) => continue,
            };
        if !resp.status().is_success() { continue; }
        let body = match resp.text().await { Ok(b) => b, Err(_) => continue };
        let v: serde_json::Value = match serde_json::from_str(&body) { Ok(j) => j, Err(_) => continue };
        if let Some(trans) = v.get("web_trans")
            .and_then(|w| w.get("web-translation"))
            .and_then(|arr| arr.get(0))
            .and_then(|item| item.get("trans"))
            .and_then(|t| t.as_array())
        {
            let mut best: Option<(String, i64)> = None;
            for t in trans {
                if let Some(val) = t.get("value").and_then(|x| x.as_str()) {
                    let support = t.get("support").and_then(|x| x.as_i64()).unwrap_or(0);
                    if best.as_ref().map(|(_, s)| support > *s).unwrap_or(true) {
                        best = Some((val.to_string(), support));
                    }
                }
            }
            if let Some((val, _)) = best {
                if !val.trim().is_empty() {
                    return Ok(val);
                }
            }
        }
    }
    Err("no translation from youdao".into())
}

// 按编号调用引擎 (单条)
async fn engine_translate(idx: u8, text: &str, tl: &str) -> Result<String, String> {
    match idx {
        0 => youdao_translate(text, tl).await,
        1 => mymemory_translate(text, tl).await,
        _ => google_gtx_translate("translate.googleapis.com", text, tl).await,
    }
}

// 批量分隔符策略:
// 主分隔符用换行符 \n —— MyMemory / Google 翻译通常会保留原文换行结构,
// 拆分匹配率远高于控制字符 (控制字符易被引擎吞掉)。
// 拆分校验: 先按 \n 拆, 数量不匹配时再尝试多种兜底策略, 仍不匹配才报错。
const BATCH_SEP: &str = "\n";

// 按编号调用引擎 (批量): 多条短文本用换行拼接为一次请求, 按换行拆分
async fn engine_translate_batch(idx: u8, texts: &[String], tl: &str) -> Result<Vec<String>, String> {
    let joined = texts.join(BATCH_SEP);
    let out = engine_translate(idx, &joined, tl).await?;
    // 优先按原始换行拆分
    let parts: Vec<String> = out.split(BATCH_SEP).map(|s| s.trim().to_string()).collect();
    if parts.len() == texts.len() {
        return Ok(parts);
    }
    // 换行被引擎合并 → 尝试按空行拆分
    let dbl_parts: Vec<String> = out.split("\n\n").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if dbl_parts.len() == texts.len() {
        return Ok(dbl_parts);
    }
    // 兜底: 按句号/问号/感叹号切分 (仅当数量匹配时)
    let punct_parts: Vec<String> = out.split(|c| c == '。' || c == '.' || c == '?' || c == '!' || c == '?')
        .map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if punct_parts.len() == texts.len() {
        return Ok(punct_parts);
    }
    // 拆分数量仍不匹配 → 由上层回退逐条 (此错误不计入熔断器, 非引擎故障)
    Err(format!("__SPLIT_MISMATCH__: sep={} nl={} punct={} != {}",
        parts.len(), dbl_parts.len(), punct_parts.len(), texts.len()))
}

// 回退链: 从最近成功的引擎开始依次尝试, 成功即记住 (省去反复撞被墙端点的超时)
// 熔断中的引擎直接跳过
async fn translate_via_chain(text: &str, tl: &str) -> Result<String, String> {
    let start = store().best_engine.load(Ordering::Relaxed);
    let mut last_err = String::new();
    for i in 0..ENGINE_COUNT {
        let idx = (start + i) % ENGINE_COUNT;
        if breaker_blocked(idx) { continue; }
        match engine_translate(idx, text, tl).await {
            Ok(t) => {
                store().best_engine.store(idx, Ordering::Relaxed);
                breaker_on_success(idx);
                return Ok(t);
            }
            Err(e) => {
                breaker_on_fail(idx);
                last_err = format!("engine{}: {}", idx, e);
            }
        }
    }
    Err(last_err)
}

// 回退链 (批量版): 批量失败时上层再逐条走 translate_via_chain
async fn translate_via_chain_batch(texts: &[String], tl: &str) -> Result<Vec<String>, String> {
    let start = store().best_engine.load(Ordering::Relaxed);
    let mut last_err = String::new();
    for i in 0..ENGINE_COUNT {
        let idx = (start + i) % ENGINE_COUNT;
        if breaker_blocked(idx) { continue; }
        match engine_translate_batch(idx, texts, tl).await {
            Ok(v) => {
                store().best_engine.store(idx, Ordering::Relaxed);
                breaker_on_success(idx);
                return Ok(v);
            }
            Err(e) => {
                // 拆分不匹配 (__SPLIT_MISMATCH__) 不是引擎故障, 不触发熔断器
                if !e.starts_with("__SPLIT_MISMATCH__") {
                    breaker_on_fail(idx);
                }
                last_err = format!("engine{}: {}", idx, e);
            }
        }
    }
    Err(last_err)
}

/// 单条文本翻译: 1 次重试, 失败回退原文
async fn translate_one_online(text: &str, tl: &str) -> Option<String> {
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        }
        if let Ok(t) = translate_via_chain(text, tl).await {
            return Some(t);
        }
    }
    None
}

// 把待翻译文本分组: 每组 \n 拼接后 ≤450 字符 (单条超长的先 split_text 再入组)
fn group_for_batch(texts: &[String], max_chars: usize) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut cur_len = 0usize;
    for (i, t) in texts.iter().enumerate() {
        let n = t.chars().count();
        if !cur.is_empty() && cur_len + n + 1 > max_chars {
            groups.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        cur.push(i);
        cur_len += n + 1;
    }
    if !cur.is_empty() { groups.push(cur); }
    groups
}

/// 批量翻译入口 (带持久缓存):
/// - 已是目标语言的文本直接返回
/// - 缓存命中直接返回 (跨重启持久; 引擎成功返回原文的专有名词也缓存, 避免重复消耗额度)
/// - 未命中: 分组合并成批请求 (~10x 更少请求), 批量失败回退逐条, 全部失败回退原文且不写缓存
pub async fn translate_texts(texts: &[String], tl: &str) -> Vec<String> {
    let lang = if tl.is_empty() { "zh-CN" } else { tl };
    let st = store();
    let mut results: Vec<String> = vec![String::new(); texts.len()];
    let mut missing: Vec<usize> = Vec::new();

    // 同一批内去重: 只翻译首次出现的文本, 翻译完回填到重复位置
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut dups: Vec<(usize, usize)> = Vec::new(); // (重复位置, 首次位置)
    {
        let map = st.map.lock();
        for (i, t) in texts.iter().enumerate() {
            if t.trim().is_empty() || is_target_lang_text(t) {
                results[i] = t.clone();
                continue;
            }
            if let Some(&j) = seen.get(t.as_str()) {
                if !results[j].is_empty() {
                    results[i] = results[j].clone();
                } else {
                    dups.push((i, j));
                }
                continue;
            }
            seen.insert(t.as_str(), i);
            let key = cache_key(lang, t);
            if let Some(hit) = map.get(&key) {
                results[i] = hit.clone();
            } else {
                missing.push(i);
            }
        }
    }
    if missing.is_empty() {
        // 回填重复项 (缓存命中路径)
        for &(i, j) in &dups {
            if results[i].is_empty() { results[i] = results[j].clone(); }
        }
        return results;
    }

    // 将未命中缓存的文本分为短文本 (走批量) 和长文本 (>450字符, 走逐条切分)
    let (short_missing, long_missing): (Vec<usize>, Vec<usize>) = missing.iter()
        .partition(|&&i| texts[i].chars().count() <= 450);

    // ---- 阶段1: 批量合并请求 (仅短文本) ----
    if !short_missing.is_empty() {
        let groups = group_for_batch(
            &short_missing.iter().map(|&i| texts[i].clone()).collect::<Vec<_>>(),
            450,
        );
        // 组 → 原始索引
        let group_idxs: Vec<Vec<usize>> = groups.iter()
            .map(|g| g.iter().map(|&k| short_missing[k]).collect())
            .collect();
        use futures::stream::{self, StreamExt};
        let batch_jobs: Vec<(Vec<usize>, Vec<String>)> = group_idxs.iter()
            .map(|idxs| (idxs.clone(), idxs.iter().map(|&i| texts[i].clone()).collect()))
            .collect();
        let outs: Vec<(Vec<usize>, Result<Vec<String>, String>)> = stream::iter(batch_jobs)
            .map(|(idxs, group)| async move {
                let r = translate_via_chain_batch(&group, lang).await;
                (idxs, r)
            })
            .buffer_unordered(8)
            .collect()
            .await;

        let mut still_missing: Vec<usize> = Vec::new();
        {
            let mut map = st.map.lock();
            let mut new_entries = 0usize;
            for (idxs, r) in outs {
                match r {
                    Ok(translations) => {
                        for (k, &i) in idxs.iter().enumerate() {
                            let tr = translations.get(k).cloned().unwrap_or_default();
                            if tr.is_empty() { still_missing.push(i); continue; }
                            // 引擎成功 (即使 == 原文的专有名词) → 写缓存, 不再重复消耗额度
                            map.insert(cache_key(lang, &texts[i]), tr.clone());
                            results[i] = tr;
                            new_entries += 1;
                        }
                    }
                    Err(_) => {
                        // 批量失败 (拆分不匹配/引擎全挂) → 逐条重试
                        still_missing.extend(idxs);
                    }
                }
            }
            if new_entries > 0 {
                st.dirty.store(true, Ordering::Relaxed);
            }
        }
        // 批量未成功的短文本 + 全部长文本 → 逐条翻译
        missing = still_missing;
        missing.extend(long_missing);
    } else {
        // 全是长文本, 全部走逐条
        missing = long_missing;
    }

    // ---- 阶段2: 逐条翻译 (批量失败的回退 + 超长文本) ----
    if !missing.is_empty() {
        use futures::stream::{self, StreamExt};
        let items: Vec<(usize, String)> = missing.iter().map(|&i| (i, texts[i].clone())).collect();
        let outs: Vec<(usize, Option<String>)> = stream::iter(items)
            .map(|(i, t)| async move {
                let chunks = split_text(&t, 450);
                let mut parts: Vec<String> = Vec::with_capacity(chunks.len());
                let mut all_ok = true;
                for c in &chunks {
                    match translate_one_online(c, lang).await {
                        Some(p) => parts.push(p),
                        None => { all_ok = false; break; }
                    }
                }
                (i, if all_ok { Some(parts.join("")) } else { None })
            })
            .buffer_unordered(4)
            .collect()
            .await;

        let mut new_entries = 0usize;
        {
            let mut map = st.map.lock();
            for (i, opt) in outs {
                match opt {
                    Some(tr) => {
                        // 引擎成功 (即使 == 原文的专有名词) → 写缓存
                        map.insert(cache_key(lang, &texts[i]), tr.clone());
                        results[i] = tr;
                        new_entries += 1;
                    }
                    None => {
                        // 全部引擎失败 (网络/配额), 不写缓存, 下次可重试
                        results[i] = texts[i].clone();
                    }
                }
            }
        }
        if new_entries > 0 {
            st.dirty.store(true, Ordering::Relaxed);
        }
    }

    // 回填重复项
    for &(i, j) in &dups {
        if results[i].is_empty() { results[i] = results[j].clone(); }
    }

    if st.dirty.swap(false, Ordering::Relaxed) {
        save_cache_file();
    }
    results
}

/// 清空翻译缓存 (内存 + 磁盘文件)
pub fn clear_all() {
    store().map.lock().clear();
    let _ = std::fs::remove_file(cache_file());
}

pub struct GameTranslator {
    lang: Mutex<String>,
}
impl GameTranslator {
    pub fn new() -> Self {
        // 启动时预热: 同步加载磁盘缓存到内存
        let _ = store();
        Self { lang: Mutex::new("zh-CN".into()) }
    }
    pub fn set_engine(&self, _: TranslateEngine) -> Result<(), String> { Ok(()) }
    pub fn get_engine(&self) -> TranslateEngine { TranslateEngine::Online }
    pub fn set_config(&self, _c: TranslateConfig) -> Result<(), String> { Ok(()) }
    pub fn get_config(&self) -> TranslateConfig { TranslateConfig::default() }
    pub async fn translate(&self, t: String) -> Result<String, String> {
        let lang = self.lang.lock().clone();
        Ok(translate_texts(&[t], &lang).await.pop().unwrap_or_default())
    }
    pub async fn translate_batch(&self, t: Vec<String>) -> Result<Vec<String>, String> {
        let lang = self.lang.lock().clone();
        Ok(translate_texts(&t, &lang).await)
    }
    pub async fn translate_dir(&self, _d: String, _e: Option<String>) -> Result<TranslateReport, String> { Ok(TranslateReport::default()) }
    pub fn clear_cache(&self) { clear_all(); }
    pub fn set_lang(&self, l: String) { *self.lang.lock() = l; }
    pub fn get_lang(&self) -> String { self.lang.lock().clone() }
}
impl Default for GameTranslator { fn default() -> Self { Self::new() } }

#[derive(Serialize, Deserialize)]
#[serde(remote = "TranslateEngine")]
enum TranslateEngineDef { Local, Online }
#[derive(Serialize, Deserialize)]
#[serde(remote = "TranslateConfig")]
struct TranslateConfigDef {
    #[serde(with = "TranslateEngineDef")] engine: TranslateEngine,
    libretranslate_url: String, target_lang: String, auto_backup: bool, concurrency: usize,
}
#[derive(Serialize, Deserialize)]
#[serde(remote = "TranslateReport")]
struct TranslateReportDef { translated_files: usize, translated_lines: usize, skipped: usize, }

#[derive(Serialize, Deserialize)] pub struct TranslateEngineWrap(#[serde(with = "TranslateEngineDef")] pub TranslateEngine);
#[derive(Serialize, Deserialize)] pub struct TranslateConfigWrap(#[serde(with = "TranslateConfigDef")] pub TranslateConfig);
#[derive(Serialize, Deserialize)] pub struct TranslateReportWrap(#[serde(with = "TranslateReportDef")] pub TranslateReport);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_target_lang_text() {
        assert!(is_target_lang_text("黑神话: 悟空"));
        assert!(is_target_lang_text("这是一个中文游戏名字"));
        assert!(!is_target_lang_text("The Witcher 3: Wild Hunt"));
        assert!(!is_target_lang_text("Ведьмак 3: Дикая Охота"));
        assert!(!is_target_lang_text(""));
    }

    #[test]
    fn test_split_text() {
        let short = "hello world";
        assert_eq!(split_text(short, 1200).len(), 1);
        let long = "很长的句子。".repeat(1000); // ~6000 chars
        let parts = split_text(&long, 1200);
        assert!(parts.len() >= 5);
        let total: usize = parts.iter().map(|p| p.chars().count()).sum();
        assert_eq!(total, long.chars().count());
    }

    #[test]
    fn test_group_for_batch() {
        let texts: Vec<String> = (0..10).map(|i| format!("game title number {}", i)).collect();
        let groups = group_for_batch(&texts, 60);
        for g in &groups {
            let joined: String = g.iter().map(|&i| texts[i].as_str()).collect::<Vec<_>>().join("\n");
            assert!(joined.chars().count() <= 60, "组内拼接超过上限: {}", joined.chars().count());
        }
        let total: usize = groups.iter().map(|g| g.len()).sum();
        assert_eq!(total, 10);
    }

    #[tokio::test]
    async fn test_translate_texts_live() {
        let texts = vec!["The Witcher 3: Wild Hunt".to_string()];
        let out = translate_texts(&texts, "zh-CN").await;
        assert_eq!(out.len(), 1);
        assert!(is_target_lang_text(&out[0]), "译文应包含中文: {}", out[0]);
    }

    #[tokio::test]
    async fn test_translate_cache_roundtrip() {
        // 首次翻译 → 缓存写入; 二次调用 → 缓存命中 (结果一致)
        let texts = vec!["Red Dead Redemption 2".to_string()];
        let a = translate_texts(&texts, "zh-CN").await;
        let b = translate_texts(&texts, "zh-CN").await;
        assert_eq!(a, b);
        let key = cache_key("zh-CN", "Red Dead Redemption 2");
        assert!(store().map.lock().contains_key(&key), "缓存应包含该条目");
    }
}
