// ============================================================
// mm 来源（槿篱游戏盒 / D:\mmese291 逆向）
// ------------------------------------------------------------
// 协议要点（2026-10-26 实测）：
//   · 搜索/详情: https://hzhd.steamcdk.xin  POST /index/Appcp/s<编号>
//   · 文件列表/直链签名: http://1.117.71.138:8814（旧端口 8813 仍可用，按序探活）
//   · 每个请求体都要带 auth_key = ts-rand-381-md5("{path}-{ts}-{rand}-381-{secret}")
//   · 返回的直链是 base64 且在第 2、5 个字符位各插了一个干扰字符
//   · 资源是多分卷（app.7z.001..NNN），必须全部下完再解压第一个分卷
//   · Switch 游戏用 child_plat == "Switch" 标识，本模块一律过滤掉
//
// 符号表见 tests 里的 mm_* 用例，全部是纯函数（不联网）所以能离线跑。
// ============================================================
use serde::{Deserialize, Serialize};

/// 搜索 / 详情后端
const MM_API: &str = "https://hzhd.steamcdk.xin";
/// 文件列表 / 直链签名后端（多端口候选，按序探活）
const MM_DL_BASES: &[&str] = &["http://1.117.71.138:8814", "http://1.117.71.138:8813"];
const MM_SECRET: &str = "Lu3aJo5in8vsDu7sj5wm";
const MM_DLID: &str = "381";
const MM_SEARCH_EP: &str = "/index/Appcp/s17443723143643";
const MM_DETAIL_EP: &str = "/index/Appcp/s17443723152613";
const MM_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                      (KHTML, like Gecko) Chrome/109.0.0.0 Safari/537.36";
/// 探活结果缓存（秒）：签名服务换端口/抖动时不要每次都重新探
const MM_BASE_TTL_SECS: i64 = 120;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MmItem {
    pub gameid: i64,
    pub platform: String,
    pub name_zh: String,
    pub name_en: String,
    pub alias: String,
    /// 竖版封面（Steam header.jpg），列表卡片用
    pub cover: String,
    /// 横版大图（capsule_616x353），详情头图用
    pub banner: String,
    pub tags: Vec<String>,
    pub update_time: i64,
    pub child_plat: String,
}

impl MmItem {
    /// Switch 游戏不要：`child_plat` 为 Switch，或 platform 标记为 aSteam（移植机种）
    pub fn is_switch(&self) -> bool {
        let cp = self.child_plat.to_ascii_lowercase();
        cp.contains("switch") || cp.contains(" ns") || self.platform.eq_ignore_ascii_case("aSteam")
    }
}

#[derive(Debug, Clone, Default)]
pub struct MmDetail {
    pub item: MmItem,
    /// 富文本介绍（站点给的是 HTML）
    pub desc_html: String,
    /// 镜像目录 id 列表（alone_file / sky_yun 等，第三段）
    pub dir_ids: Vec<String>,
    pub volume_hint: String,
    pub total_size: u64,
    pub unzip_code: String,
}

#[derive(Debug, Clone, Default)]
pub struct MmVolume {
    pub file_id: String,
    pub name: String,
    pub size: u64,
}

// ============================================================
// 纯函数（可离线测试）
// ============================================================

/// auth_key = "{ts}-{rand}-381-{md5(path-ts-rand-381-secret)}"
pub fn mm_auth_key(path: &str, ts: i64, rand: u32) -> String {
    let raw = format!("{path}-{ts}-{rand}-{MM_DLID}-{MM_SECRET}");
    format!("{ts}-{rand}-{MM_DLID}-{}", md5_hex(raw.as_bytes()))
}

fn md5_hex(b: &[u8]) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(b);
    h.finalize().iter().map(|x| format!("{x:02x}")).collect()
}

/// 站点直链加扰：base64 的第 2、5 个字符位各插入一个干扰字符。
/// 两个变体都试（历史上有过 2/4 位），解出来的必须像 URL。
pub fn mm_deobfuscate_url(s: &str) -> Option<String> {
    use base64::Engine;
    let eng = base64::engine::general_purpose::STANDARD;
    for skip in [vec![2usize, 5], vec![2, 4]] {
        let kept: String = s
            .char_indices()
            .filter(|(i, _)| !skip.contains(i))
            .map(|(_, c)| c)
            .collect();
        let trimmed = kept.trim_end_matches('=');
        let padded = format!("{}{}", trimmed, "=".repeat((4 - trimmed.len() % 4) % 4));
        if let Ok(raw) = eng.decode(padded.as_bytes()) {
            if let Ok(txt) = String::from_utf8(raw) {
                if txt.starts_with("http://") || txt.starts_with("https://") {
                    return Some(txt);
                }
            }
        }
    }
    None
}

/// 详情里的 `1,app.7z.001,3717330v20260125` → 目录 id（第三段）
pub fn mm_dir_ids(fields: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in fields {
        let parts: Vec<&str> = v.split(',').collect();
        if parts.len() >= 3 {
            let cand = parts[parts.len() - 1].trim();
            if !cand.is_empty() && !out.iter().any(|x| x == cand) {
                out.push(cand.to_string());
            }
        }
    }
    out
}

/// 分卷顺序：app.7z.001 → .002 → …（字符串排序对 3 位补零的卷名是正确的）
pub fn mm_sort_volumes(v: &mut [MmVolume]) {
    v.sort_by(|a, b| a.name.cmp(&b.name));
}

/// 目标文件夹名：只用 ASCII。中文会把一部分老游戏（以及 7z 的某些版本）搞挂。
/// 规则：英文名 → 去掉非 [A-Za-z0-9 ._-] → 折叠空白/重复分隔符 → 兜底 mm-<gameid>。
pub fn mm_safe_folder(name_en: &str, name_zh: &str, gameid: i64) -> String {
    let src = if name_en.trim().is_empty() { name_zh } else { name_en };
    let mut out = String::new();
    let mut prev_sep = false;
    for ch in src.chars() {
        let ok = ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-';
        if ok {
            out.push(ch);
            prev_sep = false;
        } else if ch.is_ascii_whitespace() || !ch.is_ascii() {
            // 中文/空格/其它符号 → 单个下划线
            if !prev_sep && !out.is_empty() {
                out.push('_');
                prev_sep = true;
            }
        }
    }
    let out = out.trim_matches(|c| c == '_' || c == '.' || c == ' ').to_string();
    // 纯中文名会被压成空串，用 gameid 兜底
    let out = if out.is_empty() { format!("mm-{gameid}") } else { out };
    // Windows 路径长度与保留名保护
    let mut s: String = out.chars().take(80).collect();
    let upper = s.to_ascii_uppercase();
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&upper.as_str()) {
        s = format!("{s}_{gameid}");
    }
    s
}

// ============================================================
// HTTP
// ============================================================

fn mm_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(MM_UA)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default()
}

fn mm_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn mm_rand() -> u32 {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0)
        .wrapping_mul(1664525)
        .wrapping_add(1013904223);
    n % 1_000_000
}

async fn mm_post(path: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
    let mut b = body;
    let key = mm_auth_key(path, mm_now_ms(), mm_rand());
    if let Some(o) = b.as_object_mut() {
        o.insert("auth_key".into(), serde_json::Value::String(key));
    }
    let r = mm_client()
        .post(format!("{MM_API}{path}"))
        .header("Content-Type", "application/json;charset=UTF-8")
        .header("Accept-Encoding", "identity")
        .json(&b)
        .send()
        .await
        .map_err(|e| format!("mm 接口连不上: {e}"))?;
    let st = r.status();
    let txt = r.text().await.unwrap_or_default();
    if !st.is_success() {
        return Err(format!("mm 接口 HTTP {st}: {}", &txt.chars().take(120).collect::<String>()));
    }
    serde_json::from_str(&txt).map_err(|e| format!("mm 接口不是 JSON: {e}"))
}

fn mm_search_body(kw: &str, page: u32, num: u32, platform: &str) -> serde_json::Value {
    serde_json::json!({
        "token": "", "Uuid": "", "page": page, "sign": "", "num": num,
        "gamename": kw, "platform": platform, "flag": "", "type": "",
        "ispricetop": 0, "onemoney": "", "no_alone": 0, "tb_label": "", "istb": 1,
        "channel_url": "", "dlid": MM_DLID, "svip": 0, "is_t3": 0,
        "category": "", "rkcw": 0, "is_zhuan": 0
    })
}

fn mm_item_from(v: &serde_json::Value) -> MmItem {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let cover = {
        let a = s("ex_list");
        if a.is_empty() { s("ex_myGame") } else { a }
    };
    let banner = {
        let a = s("ex_banner");
        if a.is_empty() { cover.clone() } else { a }
    };
    let tags: Vec<String> = s("gamelabel")
        .split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .take(12)
        .collect();
    MmItem {
        gameid: v.get("gameid").and_then(|x| x.as_i64()).unwrap_or(0),
        platform: s("platform"),
        name_zh: s("gamename_zh"),
        name_en: s("gamename_en"),
        alias: s("aliasname"),
        cover,
        banner,
        tags,
        update_time: parse_mm_time(&s("adddate")),
        child_plat: s("child_plat"),
    }
}

/// "2025-12-15 00:02:12" → 秒级时间戳（解析失败返回 0，不 panic）
fn parse_mm_time(s: &str) -> i64 {
    let b: Vec<i64> = s
        .split(|c: char| !c.is_ascii_digit())
        .filter(|x| !x.is_empty())
        .filter_map(|x| x.parse::<i64>().ok())
        .collect();
    if b.len() < 3 {
        return 0;
    }
    // 粗暴但足够做排序：按 365 天 + 30 天近似，不做闰年
    let (y, mo, d) = (b[0], b[1], b[2]);
    let (h, mi, se) = (
        b.get(3).copied().unwrap_or(0),
        b.get(4).copied().unwrap_or(0),
        b.get(5).copied().unwrap_or(0),
    );
    (((y - 1970) * 365 + (mo - 1) * 30 + (d - 1)) * 86400) + h * 3600 + mi * 60 + se
}

/// 搜索 / 浏览（keyword 为空即浏览全部）。已过滤 Switch。
pub async fn search(keyword: &str, page: u32, num: u32) -> Result<(Vec<MmItem>, u64), String> {
    let v = mm_post(MM_SEARCH_EP, mm_search_body(keyword, page.max(1), num, "")).await?;
    let list = v.get("list").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let total = v
        .get("num")
        .and_then(|x| x.as_array())
        .and_then(|a| a.first())
        .and_then(|o| o.get("count(*)"))
        .and_then(|x| x.as_i64())
        .unwrap_or(0)
        .max(0) as u64;
    let items: Vec<MmItem> = list.iter().map(mm_item_from).filter(|i| !i.is_switch()).collect();
    Ok((items, total))
}

/// 详情：名称/介绍/封面/镜像目录
pub async fn detail(gameid: i64, platform: &str) -> Result<MmDetail, String> {
    let plat = if platform.is_empty() { "Alone" } else { platform };
    let body = serde_json::json!({
        "gameid": gameid, "platform": plat, "dlid": MM_DLID, "Uuid": "", "sign": ""
    });
    let v = mm_post(MM_DETAIL_EP, body).await?;
    let item = mm_item_from(&v);
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    // 介绍优先富文本 infodetail，回退 gameinfo
    let mut desc = s("infodetail");
    if desc.len() < 40 {
        let g = s("gameinfo");
        if g.len() > desc.len() {
            desc = format!("<p>{g}</p>");
        }
    }
    let dirs = mm_dir_ids(&[
        s("alone_file").as_str(),
        s("alone_file1").as_str(),
        s("alone_file2").as_str(),
        s("sky_yun").as_str(),
        s("sky_yun1").as_str(),
    ]);
    let mut d = MmDetail {
        item,
        desc_html: desc,
        dir_ids: dirs,
        volume_hint: s("alone_file"),
        total_size: 0,
        unzip_code: String::new(),
    };
    // 详情接口偶尔不带名称（部分 gameid 返回空对象），用搜索结果兜底
    if d.item.name_zh.is_empty() && d.item.name_en.is_empty() {
        d.item.name_zh = s("gamename_zh");
    }
    Ok(d)
}

// ---- 签名服务端口探活（换端口时自动切换）----
static MM_GOOD_BASE: parking_lot::Mutex<Option<(String, i64)>> = parking_lot::Mutex::new(None);

async fn mm_dl_base(force: bool) -> String {
    if !force {
        let g = MM_GOOD_BASE.lock();
        if let Some((b, at)) = g.as_ref() {
            if mm_now_ms() / 1000 - at < MM_BASE_TTL_SECS {
                return b.clone();
            }
        }
    }
    let c = mm_client();
    for base in MM_DL_BASES {
        // 必须真发一次请求并拿到 JSON —— 只连 TCP 会被「端口挂着不回包」骗过去
        let ok = match c.get(format!("{base}/geturl/713")).send().await {
            Ok(r) if r.status().is_success() => r
                .text()
                .await
                .map(|t| t.contains("\"maps\""))
                .unwrap_or(false),
            _ => false,
        };
        if ok {
            *MM_GOOD_BASE.lock() = Some((base.to_string(), mm_now_ms() / 1000));
            return base.to_string();
        }
    }
    MM_DL_BASES[0].to_string()
}

async fn mm_dl_get(path: &str) -> Result<serde_json::Value, String> {
    let c = mm_client();
    let mut last = String::new();
    for attempt in 0..3 {
        let base = mm_dl_base(attempt > 0).await;
        match c
            .get(format!("{base}{path}"))
            .header("Accept-Encoding", "identity")
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                let txt = r.text().await.unwrap_or_default();
                return serde_json::from_str(&txt)
                    .map_err(|e| format!("签名服务返回的不是 JSON: {e}"));
            }
            Ok(r) => last = format!("HTTP {}", r.status()),
            Err(e) => last = format!("{e}"),
        }
        // 失败就清缓存换下一个端口
        *MM_GOOD_BASE.lock() = None;
        tokio::time::sleep(std::time::Duration::from_millis(600 * (attempt as u64 + 1))).await;
    }
    Err(format!("签名服务不可用: {last}"))
}

/// 列出某个镜像目录下的全部分卷
pub async fn volumes(dirid: &str) -> Result<Vec<MmVolume>, String> {
    let v = mm_dl_get(&format!("/geturl/{dirid}")).await?;
    let maps = v.get("maps").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for m in maps {
        let name = m.get("utf_name").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let file_id = m
            .get("fileId")
            .map(|x| {
                x.as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| x.to_string())
            })
            .unwrap_or_default();
        let size = m.get("fileSize").and_then(|x| x.as_u64()).unwrap_or(0);
        if name.is_empty() || file_id.is_empty() {
            continue;
        }
        out.push(MmVolume { file_id, name, size });
    }
    mm_sort_volumes(&mut out);
    Ok(out)
}

/// 换某个分卷的签名直链
pub async fn signed_url(dirid: &str, file_id: &str) -> Result<String, String> {
    let v = mm_dl_get(&format!("/geturl/{dirid}?fileid={file_id}")).await?;
    let raw = v
        .get("maps")
        .and_then(|x| x.as_array())
        .and_then(|a| a.first())
        .and_then(|m| m.get("url"))
        .and_then(|x| x.as_str())
        .unwrap_or("");
    if raw.is_empty() {
        return Err("签名接口没有返回 url".into());
    }
    mm_deobfuscate_url(raw).ok_or_else(|| "无法解出下载直链".to_string())
}

/// 探测直链是否真能下（预签名也会 403/404，错误体是 XML，不能当文件写下去）
pub async fn probe(url: &str) -> bool {
    match mm_client()
        .get(url)
        .header("Range", "bytes=0-0")
        .send()
        .await
    {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

// ============================================================
// 测试
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mm_auth_key_matches_reference_implementation() {
        // 参考值由 mmbox.py 的 _make_auth 算出（固定 ts/rand 后比对）
        let k = mm_auth_key(MM_SEARCH_EP, 1_760_000_000_000, 123_456);
        assert!(k.starts_with("1760000000000-123456-381-"));
        let digest = &k["1760000000000-123456-381-".len()..];
        assert_eq!(digest.len(), 32, "md5 十六进制必须是 32 位");
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
        // 同一输入必须稳定
        assert_eq!(k, mm_auth_key(MM_SEARCH_EP, 1_760_000_000_000, 123_456));
        // 换时间戳必须变
        assert_ne!(k, mm_auth_key(MM_SEARCH_EP, 1_760_000_000_001, 123_456));
    }

    #[test]
    fn mm_deobfuscate_inserts_at_2_and_5() {
        // "https://x.y/a" 的 base64 是 aHR0cHM6Ly94LnkvYQ==
        let plain = "aHR0cHM6Ly94LnkvYQ==";
        let mut obf: Vec<char> = plain.chars().collect();
        obf.insert(2, 'Z');
        obf.insert(5, 'Q');
        let s: String = obf.into_iter().collect();
        assert_eq!(mm_deobfuscate_url(&s).as_deref(), Some("https://x.y/a"));
    }

    #[test]
    fn mm_deobfuscate_rejects_garbage() {
        assert!(mm_deobfuscate_url("not-a-link").is_none());
        assert!(mm_deobfuscate_url("").is_none());
    }

    #[test]
    fn mm_dir_ids_takes_third_segment_and_dedups() {
        let ids = mm_dir_ids(&[
            "1,app.7z.001,3717330v20260125",
            "1,app.7z.001,3717330v20260125",
            "1,app.7z.001,other123",
            "garbage",
            "",
        ]);
        assert_eq!(ids, vec!["3717330v20260125", "other123"]);
    }

    #[test]
    fn mm_volume_sort_is_natural_for_zero_padded_names() {
        let mut v: Vec<MmVolume> = ["app.7z.010", "app.7z.002", "app.7z.001"]
            .iter()
            .map(|n| MmVolume { file_id: n.to_string(), name: n.to_string(), size: 0 })
            .collect();
        mm_sort_volumes(&mut v);
        assert_eq!(v[0].name, "app.7z.001");
        assert_eq!(v[1].name, "app.7z.002");
        assert_eq!(v[2].name, "app.7z.010");
    }

    #[test]
    fn mm_switch_games_are_filtered() {
        let mut it = MmItem::default();
        it.child_plat = "Switch".into();
        assert!(it.is_switch());
        it.child_plat = "".into();
        it.platform = "aSteam".into();
        assert!(it.is_switch(), "aSteam 是机种移植标记，也要排除");
        it.platform = "Alone".into();
        assert!(!it.is_switch(), "普通单机游戏不能被误杀");
    }

    #[test]
    fn mm_safe_folder_never_contains_cjk() {
        assert_eq!(mm_safe_folder("Yakuza Kiwami", "如龙：极", 3717330), "Yakuza_Kiwami");
        // 纯中文 → 兜底
        let f = mm_safe_folder("", "如龙：极重制版/人中之龙 极", 3717330);
        assert_eq!(f, "mm-3717330");
        // 混合：中文段压成下划线，但整体仍是 ASCII
        let f2 = mm_safe_folder("Summer Gather 夏日", "x", 1);
        assert!(f2.is_ascii(), "目录名必须全 ASCII: {f2}");
        assert!(!f2.contains('_') || f2.starts_with("Summer"));
        // 保留名
        assert!(mm_safe_folder("CON", "x", 7).starts_with("CON_"));
        // 长度兜底
        assert!(mm_safe_folder(&"a".repeat(300), "x", 9).chars().count() <= 80);
    }

    /// 真实链路：搜索 → 详情 → 分卷 → 签名直链 → 探活（联网，`--ignored` 才跑）
    #[tokio::test]
    #[ignore]
    async fn live_mm_search_detail_volumes_signed_url() {
        let (items, total) = search("", 1, 20).await.expect("mm 搜索失败");
        println!("总数={total} 本页={}", items.len());
        assert!(!items.is_empty(), "mm 搜索没有返回任何游戏");
        assert!(items.iter().all(|i| !i.is_switch()), "Switch 游戏没被过滤掉");
        assert!(items.iter().any(|i| !i.cover.is_empty()), "封面全空");
        assert!(items.iter().any(|i| !i.name_zh.is_empty()), "中文名全空");

        // 找一个带下载目录的
        let mut picked = None;
        for it in items.iter().take(10) {
            if let Ok(d) = detail(it.gameid, &it.platform).await {
                if !d.dir_ids.is_empty() && !d.desc_html.is_empty() {
                    picked = Some(d);
                    break;
                }
            }
        }
        let d = picked.expect("前 10 个游戏里没有带资源目录且带介绍的");
        println!("详情: {} / {} 介绍 {} 字 目录 {:?}",
                 d.item.name_zh, d.item.name_en, d.desc_html.len(), d.dir_ids);
        assert!(d.desc_html.len() > 30, "介绍太短");

        let vols = volumes(&d.dir_ids[0]).await.expect("分卷列表失败");
        println!("分卷 {} 个: {:?}", vols.len(), vols.iter().map(|v| v.name.as_str()).collect::<Vec<_>>());
        assert!(!vols.is_empty(), "分卷列表为空");

        let url = signed_url(&d.dir_ids[0], &vols[0].file_id).await.expect("签名失败");
        println!("直链: {}", &url.chars().take(110).collect::<String>());
        assert!(url.starts_with("http"), "直链不是 URL");
        assert!(probe(&url).await, "直链探活失败（整桶 403？）");
    }

    #[test]
    fn mm_time_parse_orders_correctly() {
        let a = parse_mm_time("2025-12-15 00:02:12");
        let b = parse_mm_time("2026-01-02 00:00:00");
        assert!(a > 0 && b > a, "新的时间必须更大: {a} vs {b}");
        assert_eq!(parse_mm_time(""), 0);
    }
}
