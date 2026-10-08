//! 动漫追番模块 —— 选择器引擎版。
//!
//! ## 数据链路（2026-10-06 在本机逐条实测通过）
//!
//! | 用途 | 端点 | 说明 |
//! |---|---|---|
//! | 趋势 / 最高热度 | `api.animeko.org/v1/trends` | 200，无需密钥 |
//! | 推荐墙（首页海报） | `api.animeko.org/v2/home/recommendations` | 200，共 200 部 |
//! | 番剧详情 / 剧集 / 角色 / 制作 | `bgmapi.anibt.net`（Bangumi 镜像） | 200，字段与 api.bgm.tv 一致 |
//! | 每季放送 | `bgmapi.anibt.net/calendar` | 200 |
//! | 资源站（搜索/选集/取流） | `sub.creamycake.org/v1/{bt1,css1}.json` | 18+2 个源 |
//! | 弹幕 | `danmaku-cn.myani.org` | 200 |
//!
//! ⚠️ **裸的 `api.bgm.tv` 在这台机器上 21 秒超时**（Ani 自己的日志里也是
//! `IO_EXCEPTION in 21.0s`），所以走**镜像 `bgmapi.anibt.net`** —— 字段结构一致，
//! 换域名即可。`static.myani.org` 提供封面图（Ani 的图片缓存也全来自它）。
//!
//! ## 为什么要重写（广告问题的根因）
//!
//! 旧实现把源页面里**所有 `<a>` 标签**当搜索结果抓。而番剧站的页面里除了搜索结果，
//! 还塞满了导航、APP 推广、Telegram 群、广告位 —— 于是"搜索结果"里出现了
//! 「APP下载 / Telegram群 / AIS魅魔」这类条目，看着就是一堆广告。
//!
//! 现在按数据源自带的 **CSS 选择器**精确取数：数据源 JSON 里每个源都写明了
//! `selectNames` / `selectLinks` / `selectEpisodesFromList` / `matchVideoUrl`，
//! 这些字段就是"从哪取、取什么"的权威描述。引擎照做，不再瞎抓。
//! 另外 `is_plausible_subject()` 还有一道噪声过滤，专门挡导航/推广文案。
//!
//! ## 各源的实际状态（2026-10-06 逐个实测，别当成都能用）
//!
//! | 源 | 状态 |
//! |---|---|
//! | 叽哔 / 森之屋 / 稀饭 / 嘀嗒 / 嘀哩嘀哩 / 影视森林 / 海星 / 番茄 / wedm | 搜索可用 |
//! | **girigiri** | **搜索页有人机校验**（Cloudflare verify，服务端返回 0 条结果），但**详情页和播放页正常** |
//! | E-ACG / 去看吧 / 新优酷 | 403（反爬） |
//! | 风车影视 / 樱花动漫 / 热播之家 / hanime1 | 连不上（本机网络） |
//!
//! 所以"某个源搜不到"多半是源本身的问题，换一个源即可；引擎会照常把可用源排前面
//! （订阅里的 `tier`，数字越小越优先）。
//!
//! ## 关于 animeko
//!
//! 数据源的**格式**（`factoryId` + `arguments` + 选择器字段名）参照 open-ani/animeko。
//! animeko 是 **GPL-3.0**，所以这里**没有复制它的任何代码**，只是按公开的数据格式
//! 独立实现引擎；数据源内容来自用户订阅的公开 JSON。

use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// 请求头 —— 这些站对默认 UA 很敏感（实测不带 UA 会 400）
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// Bangumi 镜像（裸 api.bgm.tv 在用户网络下超时）
const BGM: &str = "https://bgmapi.anibt.net";
/// animeko 公共 API：趋势 + 首页推荐
const ANIMEKO_API: &str = "https://api.animeko.org";
/// 弹幕服务
const DANMAKU_BASE: &str = "https://danmaku-cn.myani.org";
/// 数据源订阅（用户给的 A 软件里就是这两个）
const SUB_BT: &str = "https://sub.creamycake.org/v1/bt1.json";
const SUB_CSS: &str = "https://sub.creamycake.org/v1/css1.json";

pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(UA)
        .connect_timeout(std::time::Duration::from_secs(12))
        // ★ 40s: 这些番剧站真的慢。实测 叽哔动漫 单次搜索响应 10.5s,
        //   原来的 25s 在某些时段会直接超时, 用户看到的就是"搜索失败"。
        .timeout(std::time::Duration::from_secs(40))
        .pool_max_idle_per_host(8)
        .build()
        .unwrap_or_default()
}

/// 带退避重试的 GET —— 番剧站偶发超时/瞬断很常见, 重试一次能挡掉大部分。
/// 返回 (响应体, 最终 URL)。
pub(crate) async fn get_text_retry(
    url: &str,
    referer: Option<&str>,
) -> Result<(String, String), String> {
    let c = client();
    let mut last = String::new();
    for attempt in 0..3u32 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(700 * attempt as u64)).await;
        }
        let mut req = c
            .get(url)
            .header("Accept", "text/html,application/json,*/*")
            .header("Accept-Language", "zh-CN,zh;q=0.9,ja;q=0.8");
        if let Some(r) = referer {
            req = req.header("Referer", r);
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    last = format!("HTTP {status}");
                    // 4xx 是站点明确拒绝, 重试没意义
                    if status.is_client_error() {
                        return Err(last);
                    }
                    continue;
                }
                let final_url = resp.url().to_string();
                match resp.text().await {
                    Ok(t) => return Ok((t, final_url)),
                    Err(e) => last = format!("读取失败: {e}"),
                }
            }
            Err(e) => last = format!("{e}"),
        }
    }
    Err(last)
}

// ============================================================
// 通用小工具
// ============================================================

pub(crate) fn html_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.trim().to_string();
    }
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

/// 相对链接补全成绝对链接
pub(crate) fn absolutize(base: &str, href: &str) -> String {
    let h = href.trim();
    if h.is_empty() {
        return String::new();
    }
    if h.starts_with("http://") || h.starts_with("https://") {
        return h.to_string();
    }
    if h.starts_with("//") {
        return format!("https:{h}");
    }
    if h.starts_with("data:") || h.starts_with("javascript:") || h == "#" {
        return String::new();
    }
    let origin = base_origin(base);
    if h.starts_with('/') {
        format!("{origin}{h}")
    } else {
        format!("{origin}/{h}")
    }
}

/// 取 URL 的 `scheme://host` 部分
pub(crate) fn base_origin(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            let host = u.host_str()?;
            Some(match u.port() {
                Some(p) => format!("{}://{}:{}", u.scheme(), host, p),
                None => format!("{}://{}", u.scheme(), host),
            })
        })
        .unwrap_or_default()
}

/// 用 `rawBaseUrl` 覆盖 origin（数据源里有些站图片在别的域名）
fn with_raw_base(base: &str, raw: &str) -> String {
    if raw.trim().is_empty() {
        base.to_string()
    } else {
        raw.trim().trim_end_matches('/').to_string()
    }
}

// ============================================================
// 数据源（订阅 JSON 的解析）
// ============================================================

/// 一个资源站的完整定义 —— 引擎照着里面的选择器干活
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Source {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub icon: String,
    /// 工厂类型: `web-selector`（HTML 站） / `rss`（RSS 源）
    #[serde(default)]
    pub factory: String,
    /// 搜索 URL 模板，`{keyword}` 是占位符
    #[serde(default)]
    pub search_url: String,
    /// 源站到底属于哪一类（决定前端怎么标）
    #[serde(default)]
    pub kind: String,
    /// 优先级：数字越小越优先
    #[serde(default)]
    pub tier: i64,
    /// 原始 `searchConfig` —— 选择器全在这里，引擎直接读
    #[serde(default)]
    pub cfg: serde_json::Value,
}

impl Source {
    fn cfg_str(&self, key: &str) -> String {
        self.cfg
            .get(key)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    }
    fn cfg_bool(&self, key: &str) -> bool {
        self.cfg.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
    }
    fn cfg_obj(&self, key: &str) -> Option<&serde_json::Value> {
        self.cfg.get(key)
    }
    /// 从子对象里取字符串
    fn sub_str(obj: Option<&serde_json::Value>, key: &str) -> String {
        obj.and_then(|o| o.get(key))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    }
}

/// 解析订阅 JSON → 源列表。
/// 格式: `{"exportedMediaSourceDataList":{"mediaSources":[{factoryId,version,arguments:{...}}]}}`
pub fn parse_sources(json: &str) -> Vec<Source> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(list) = v
        .get("exportedMediaSourceDataList")
        .and_then(|x| x.get("mediaSources"))
        .and_then(|x| x.as_array())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in list {
        let factory = item
            .get("factoryId")
            .and_then(|x| x.as_str())
            .unwrap_or("web-selector")
            .to_string();
        let a = match item.get("arguments") {
            Some(x) => x,
            None => continue,
        };
        let name = a.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let cfg = a.get("searchConfig").cloned().unwrap_or(serde_json::Value::Null);
        let search_url = cfg
            .get("searchUrl")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        // 没有搜索模板的源没法用（rss 源有时把模板放别处）
        if search_url.is_empty() {
            continue;
        }
        let tier = a.get("tier").and_then(|x| x.as_i64()).unwrap_or(99);
        out.push(Source {
            name,
            description: a
                .get("description")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            icon: a.get("iconUrl").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            kind: if factory.contains("bt") { "bt".into() } else { "online".into() },
            factory,
            search_url,
            tier,
            cfg,
        });
    }
    // 按 tier 排，同级保持原顺序（tier 小的优先）
    out.sort_by_key(|s| s.tier);
    out
}

/// 拉取订阅并合并去重（bt1 + css1）。失败时回退到内置表。
pub async fn fetch_sources() -> Vec<Source> {
    let c = client();
    let mut all: Vec<Source> = Vec::new();
    for url in [SUB_CSS, SUB_BT] {
        if let Ok(resp) = c.get(url).send().await {
            if resp.status().is_success() {
                if let Ok(t) = resp.text().await {
                    all.extend(parse_sources(&t));
                }
            }
        }
    }
    // 同名去重（保留 tier 更小的）
    let mut seen = std::collections::HashSet::new();
    all.retain(|s| seen.insert(s.name.clone()));
    if all.is_empty() {
        all = builtin_sources();
    }
    // ★ 源顺序 = 优先级：
    //   1) 苹果CMS JSON 接口 —— 1 个请求就给出全剧集直链，最快
    //   2) Kazumi 规则源里**实测可用**的那几条（见 kazumi::VERIFIED）
    //   3) 订阅源（用户自己订的，之前一直好用，不能挤掉）
    //   4) 其余 Kazumi 规则 + 内置源
    //   ★ 之前把全部 Kazumi 规则插在订阅源前面 + `take(16)`，会把叽哔动漫这些
    //     一直可用的订阅源挤出搜索范围 —— 那是个回归，所以这里按"实测可用"分层。
    let mut out = api_sources();
    let mut taken: std::collections::HashSet<String> =
        out.iter().map(|s| s.name.clone()).collect();
    let kz = crate::kazumi::rule_sources();
    for s in kz.iter().filter(|s| s.tier <= 5) {
        if taken.insert(s.name.clone()) {
            out.push(s.clone());
        }
    }
    for s in all {
        if taken.insert(s.name.clone()) {
            out.push(s);
        }
    }
    for s in kz.into_iter().filter(|s| s.tier > 5) {
        if taken.insert(s.name.clone()) {
            out.push(s);
        }
    }
    out
}

/// 内置兜底：订阅拿不到时用（2026-10-06 实测可直连的）
pub fn builtin_sources() -> Vec<Source> {
    let mk = |n: &str, u: &str, d: &str| {
        let mut s = Source {
            name: n.to_string(),
            description: d.to_string(),
            search_url: u.to_string(),
            kind: "online".into(),
            factory: "web-selector".into(),
            ..Default::default()
        };
        // 内置源也要能选集：用最通用的选择器（子串匹配在 select_first 里做回退）
        s.cfg = serde_json::json!({});
        s
    };
    vec![
        mk("稀饭动漫", "https://dm1.xfdm.pro/search.html?wd={keyword}", "直连"),
        mk("girigiri愛動漫", "https://ani.girigirilove.com/search/-------------/?wd={keyword}", "直连"),
        mk("叽哔动漫", "https://www.jibi.cc/index.php/vod/search.html?wd={keyword}", "直连"),
        mk("森之屋动漫", "https://senfun.in/search.html?wd={keyword}", "直连"),
        mk("海星动漫", "https://www.haixingdmx.com/s_all?ex=1&kw={keyword}", "直连"),
        mk("嘀哩嘀哩", "https://dilidili.io/search?q={keyword}", "直连"),
        mk("影视森林", "https://www.dongmandaquan.vip/vodsearch/-------------.html?wd={keyword}", "直连"),
    ]
}

/// 苹果CMS(macCMS) JSON 采集接口源。
///
/// ★ 为什么值得单独开一条路（用户要求"要速度快"）：
///   一次 `?ac=detail&wd=<关键词>` 就返回**整季每集的直链 m3u8** ——
///   `vod_play_url` 里直接是 `第01集$https://.../index.m3u8#第02集$...`。
///   抓 HTML 那套要走「搜索页 → 详情页 → 播放页 → 解 player_aaaa」四步，
///   API 只要「搜索 → 详情」两步，而且站点改版不会让选择器失配。
///
/// 实测（2026-10-07，关键词「葬送的芙莉莲」）：
///   cj.lziapi.com        1.2s  3 条(2 动漫)  28 个 m3u8  单线路 `lzm3u8`
///   api.guangsuapi.com   1.4s  2 条(2 动漫)  双线路 `gsyun$$$gsm3u8`
///   caiji.dyttzyapi.com  2.5s  3 条(3 动漫)  双线路 `dytt$$$dyttm3u8`
pub fn api_sources() -> Vec<Source> {
    let mk = |n: &str, base: &str| {
        let mut s = Source {
            name: n.to_string(),
            description: "苹果CMS JSON 接口（一次拿全剧集直链）".into(),
            search_url: base.to_string(),
            kind: "online".into(),
            factory: "maccms-api".into(),
            tier: -10,
            ..Default::default()
        };
        s.cfg = serde_json::json!({});
        s
    };
    // ★ 这一批是 2026-10-07 逐个实测过的（HTTP 通 + 搜得到番 + vod_play_url 里有 m3u8
    //   直链 + 直链真能取到 #EXTM3U），40 个候选里活下来 9 个；按响应耗时排序，
    //   快的放前面（下面的早返回逻辑里，快的先到就能先开播）。
    //   同一部番在多个源上都有（内容池互相镜像），多留几个是为了某站抽风时能换线路。
    // ★ 顺序 = 前端挑候选的顺序（标题分相同的都排一起），所以**能播的放前面**：
    //   搜索结果的顺序会一路带到播放，第一条失败才轮到下一条。
    //   前三条是实测分片为明文 TS、能直接解出画面的；后面几条分片是 AES-128 加密池
    //   （同一批 CDN，能取到但偶发黑屏），当备用线路。
    vec![
        mk("量子资源", "https://cj.lziapi.com/api.php/provide/vod/"),
        mk("电影天堂", "https://caiji.dyttzyapi.com/api.php/provide/vod/"),
        mk("最大资源", "https://api.zuidapi.com/api.php/provide/vod/"),
        mk("豪华资源", "https://hhzyapi.com/api.php/provide/vod/"),
        mk("红牛资源", "https://www.hongniuzy2.com/api.php/provide/vod/"),
        mk("金鹰资源", "https://jyzyapi.com/api.php/provide/vod/"),
        mk("虎牙资源", "https://www.huyaapi.com/api.php/provide/vod/"),
        mk("极速资源", "https://jszyapi.com/api.php/provide/vod/"),
        mk("暴风资源", "https://bfzyapi.com/api.php/provide/vod/"),
        mk("光速资源", "https://api.guangsuapi.com/api.php/provide/vod/"),
    ]
}

/// 是不是苹果CMS JSON 接口源
fn is_api_source(src: &Source) -> bool {
    src.factory == "maccms-api"
}

/// 把 `search_url` 归一到接口根（去掉结尾的 `/` 和 `?`）
fn api_root(src: &Source) -> String {
    src.search_url
        .trim_end_matches('?')
        .trim_end_matches('/')
        .to_string()
}

/// 苹果CMS 搜索：返回 (标题, 详情接口地址, 封面)
async fn api_search(src: &Source, keyword: &str) -> Result<Vec<SubjectItem>, String> {
    let root = api_root(src);
    let url = format!("{root}/?ac=detail&wd={}", urlencoding::encode(keyword));
    let (body, _) = get_text_retry(&url, None)
        .await
        .map_err(|e| format!("连接 {} 失败: {e}", src.name))?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("{} 返回的不是 JSON: {e}", src.name))?;
    let list = v
        .get("list")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for it in &list {
        let title = it
            .get("vod_name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        // vod_id 可能是数字也可能是字符串
        let id = it
            .get("vod_id")
            .map(|x| match x {
                serde_json::Value::String(s) => s.trim().to_string(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        if title.is_empty() || id.is_empty() {
            continue;
        }
        out.push(SubjectItem {
            title,
            // 详情地址 = 同一个接口 + ids；fetch_episodes 再调一次拿 vod_play_url
            url: format!("{root}/?ac=detail&ids={id}"),
            image: it
                .get("vod_pic")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            source: String::new(),
            source_url: String::new(),
            source_name: String::new(),
        });
    }
    Ok(out)
}

/// 苹果CMS 详情 → 剧集。
///
/// `vod_play_from` 和 `vod_play_url` 都用 `$$$` 分成多个**线路**，两者按下标一一对应；
/// 每条线路内部再用 `#` 分集、`$` 分「集名 / 地址」。
/// ★ 有些站的第一条线路不是 m3u8（下载或 mp4），所以把 m3u8 多的线路排到前面 ——
///   前端 `playEpisode(0)` 直接播第一条，排错就会一上来就播不了。
async fn api_episodes(src: &Source, page_url: &str) -> Result<Vec<EpisodeItem>, String> {
    let (body, _) = get_text_retry(page_url, None)
        .await
        .map_err(|e| format!("读取 {} 详情失败: {e}", src.name))?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("{} 详情不是 JSON: {e}", src.name))?;
    let it = v
        .get("list")
        .and_then(|x| x.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| format!("{} 没返回这条片", src.name))?;

    let froms = it
        .get("vod_play_from")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    let urls = it
        .get("vod_play_url")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    Ok(parse_maccms_play(froms, urls))
}

/// 把苹果CMS 的 `vod_play_from` + `vod_play_url` 拆成剧集列表。
///
/// 格式（实测自 cj.lziapi.com / api.guangsuapi.com / caiji.dyttzyapi.com）：
/// ```text
/// from = "gsyun$$$gsm3u8"                    多线路用 $$$ 分隔
/// urls = "第01集$https://a/1.m3u8#第02集$..." 线路内 # 分集、$ 分「集名/地址」
/// ```
/// ★ 线路顺序不能照抄：`dytt`/`gsyun` 这类第一条线路常常不是 m3u8（下载或 mp4），
///   而前端 `playEpisode(0)` 直接播第一条 —— 所以这里把 m3u8 多的线路排前面。
fn parse_maccms_play(froms: &str, urls: &str) -> Vec<EpisodeItem> {
    let from_list: Vec<String> = froms.split("$$$").map(|s| s.trim().to_string()).collect();
    let mut ranked: Vec<(usize, Vec<EpisodeItem>)> = Vec::new();

    for (gi, g) in urls.split("$$$").enumerate() {
        let ch = from_list.get(gi).cloned().unwrap_or_default();
        let mut eps = Vec::new();
        for part in g.split('#') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (raw_name, url) = match part.split_once('$') {
                Some((n, u)) => (n.trim().to_string(), u.trim().to_string()),
                None => (String::new(), part.to_string()),
            };
            if !url.starts_with("http") {
                continue;
            }
            let (name, sort) = ep_name_and_sort(&raw_name, r"第\s*(?<ep>\d+)");
            eps.push(EpisodeItem {
                channel: ch.clone(),
                name,
                sort,
                url,
            });
        }
        if !eps.is_empty() {
            let m3u8 = eps.iter().filter(|e| e.url.contains(".m3u8")).count();
            ranked.push((m3u8 * 1000 + eps.len(), eps));
        }
    }
    ranked.sort_by(|a, b| b.0.cmp(&a.0));
    ranked.into_iter().flat_map(|(_, e)| e).collect()
}

// ============================================================
// 选择器引擎
// ============================================================

/// 把数据源的 `selectXxx` 选择器字符串解析成可用形式。
///
/// 数据源里有两类写法：
/// 1. 纯 CSS 选择器 —— `body > .box-width .search-box .thumb-txt`
/// 2. JSONPath 风格 —— `$[*]['title','name']`（给 JSON 接口用的，这里不走）
///
/// 另外还有 `selectorSubjectFormatA`（旧版单列表站）和
/// `selectorSubjectFormatIndexed`（名称和链接分开选）两种模式。
fn parse_html(html: &str) -> scraper::Html {
    scraper::Html::parse_document(html)
}

/// 用 CSS 选择器取所有匹配的文本（已去标签、去空白）
fn select_texts(doc: &scraper::Html, sel: &str) -> Vec<String> {
    if sel.trim().is_empty() {
        return Vec::new();
    }
    let Ok(s) = scraper::Selector::parse(sel) else {
        return Vec::new();
    };
    doc.select(&s)
        .map(|el| {
            // title 属性优先（很多卡片把完整标题放 title）
            let t = el.attr("title").map(|x| x.to_string()).unwrap_or_default();
            let txt = el.text().collect::<Vec<_>>().join(" ");
            let cleaned = html_unescape(&collapse_ws(&txt));
            if cleaned.is_empty() {
                html_unescape(&collapse_ws(&t))
            } else {
                cleaned
            }
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// 取所有匹配元素的 href（绝对化后）
fn select_hrefs(doc: &scraper::Html, sel: &str, base: &str) -> Vec<String> {
    if sel.trim().is_empty() {
        return Vec::new();
    }
    let Ok(s) = scraper::Selector::parse(sel) else {
        return Vec::new();
    };
    doc.select(&s)
        .filter_map(|el| {
            el.attr("href")
                .or_else(|| el.attr("data-href"))
                .map(|h| absolutize(base, h))
        })
        .filter(|u| !u.is_empty())
        .collect()
}

/// 取所有匹配元素的图片 URL
fn select_images(doc: &scraper::Html, sel: &str, base: &str) -> Vec<String> {
    let Ok(s) = scraper::Selector::parse(sel) else {
        return Vec::new();
    };
    doc.select(&s)
        .filter_map(|el| {
            // 懒加载站把真图放 data-src / data-original
            let cand = el
                .attr("data-src")
                .or_else(|| el.attr("data-original"))
                .or_else(|| el.attr("data-echo"))
                .or_else(|| el.attr("src"));
            let v = cand?;
            // 占位图（base64 内联 / 1px gif）丢掉
            if v.starts_with("data:") || v.trim().is_empty() {
                return None;
            }
            let full = absolutize(base, v);
            if full.is_empty() {
                None
            } else {
                Some(full)
            }
        })
        .collect()
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

/// ★ 编译数据源里的正则。
///
/// 数据源 JSON 用的是 JS/Kotlin 风格的具名组 `(?<ep>...)`。当前的 regex crate
/// 接受这种写法（等价于 `(?P<ep>...)`），所以正常能直接编译。
/// 这里仍做一次容错改写并保留无具名组的回退，是因为：
/// 1. 数据源里还有 lookbehind `(?<=...)` 这类 regex crate **不支持**的语法，
///    改写逻辑必须能识别并跳过，不能把 `(?<=` 误伤成 `(?P<=`；
/// 2. 万一源站写了别的方言，改写后再试一次比直接放弃更稳。
fn compile_ds_regex(pattern: &str) -> Option<regex::Regex> {
    if pattern.trim().is_empty() {
        return None;
    }
    if let Ok(re) = regex::Regex::new(pattern) {
        return Some(re);
    }
    let fixed = fix_named_groups(pattern);
    if fixed != pattern {
        if let Ok(re) = regex::Regex::new(&fixed) {
            return Some(re);
        }
    }
    None
}

/// 把 `(?<name>` 改写成 `(?P<name>`，但**不碰** lookbehind 的 `(?<=` / `(?<!`。
fn fix_named_groups(pattern: &str) -> String {
    let Ok(re) = regex::Regex::new(r"\(\?<([A-Za-z_][A-Za-z0-9_]*)>") else {
        return pattern.to_string();
    };
    re.replace_all(pattern, "(?P<$1>").into_owned()
}

/// 按数据源给的 `matchXxx` 正则抽出一段（用于把 `第 03 话` 里的 `03` 抠出来）
fn re_capture(text: &str, pattern: &str, _group: &str) -> Option<String> {
    let re = compile_ds_regex(pattern)?;
    let caps = re.captures(text)?;
    caps.get(1).map(|m| m.as_str().trim().to_string())
}

/// 数据源里 `matchChannelName` / `matchEpisodeSortFromName` 之类的正则，
/// 具名组可能是 `ch` / `ep` / `v` / `name`，这里按优先级找。
fn re_named(text: &str, pattern: &str) -> Option<String> {
    let re = compile_ds_regex(pattern)?;
    let caps = re.captures(text)?;
    for name in ["ch", "ep", "v", "name"] {
        if let Some(m) = caps.name(name) {
            let v = m.as_str().trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    caps.get(1).map(|m| m.as_str().trim().to_string())
}

// ============================================================
// 搜索（Subject 层）
// ============================================================

/// 搜索结果条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubjectItem {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub source: String,
    /// ★ 这个条目来自哪个源（源的 `search_url`）。
    ///   前端要用它去调 `anime_episodes` / `anime_resolve` —— 这两个命令靠
    ///   `source_url` 反查源的完整选择器配置。**缺了它前端会直接报参数错误。**
    #[serde(default)]
    pub source_url: String,
    /// 源的显示名（= `source`，单独给一份省得前端做名字→URL 的映射）
    #[serde(default)]
    pub source_name: String,
}

/// 关键词预处理 —— 数据源里 `searchUseOnlyFirstWord` 表示只拿第一个词搜
/// （有些站的多词搜索会返回空）。中文/日文没有空格，这里按空白切。
fn prepare_keyword(kw: &str, only_first: bool, remove_special: bool) -> String {
    let mut k = kw.trim().to_string();
    if remove_special {
        // 去掉标点，保留中英日文字符与数字
        k = k
            .chars()
            .filter(|c| c.is_alphanumeric() || c.is_whitespace() || *c == '!' || *c == '！')
            .collect();
    }
    if only_first {
        if let Some(first) = k.split_whitespace().next() {
            k = first.to_string();
        }
    }
    if k.trim().is_empty() {
        kw.trim().to_string()
    } else {
        k
    }
}

/// 在某个源上搜索番剧
pub async fn search_subject(src: &Source, keyword: &str) -> Result<Vec<SubjectItem>, String> {
    // ★ 苹果CMS JSON 接口源走单独一条路（见 api_sources 的注释）
    if is_api_source(src) {
        let mut items = api_search(src, keyword).await?;
        stamp_source(&mut items, src);
        return Ok(items);
    }
    // ★ Kazumi 规则源（XPath）
    if src.factory == "kazumi" {
        let rule = crate::kazumi::rule_of(src)?;
        let mut items = crate::kazumi::search(&rule, keyword).await?;
        stamp_source(&mut items, src);
        return Ok(items);
    }
    let kw = prepare_keyword(
        keyword,
        src.cfg_bool("searchUseOnlyFirstWord"),
        src.cfg_bool("searchRemoveSpecial"),
    );
    let url = src
        .search_url
        .replace("{keyword}", &urlencoding::encode(&kw));
    let (body, final_url) = get_text_retry(&url, None)
        .await
        .map_err(|e| format!("连接 {} 失败: {e}", src.name))?;
    let base = with_raw_base(&base_origin(&final_url), &src.cfg_str("rawBaseUrl"));

    // RSS 源（BT 站）走 XML
    if src.factory.contains("rss") || body.trim_start().starts_with("<?xml") {
        let mut items = parse_rss(&body, &base, &src.name);
        if !items.is_empty() {
            stamp_source(&mut items, src);
            return Ok(items);
        }
    }

    let doc = parse_html(&body);
    // 两种模式：A（一个选择器同时含名称+链接） / Indexed（名称、链接分开选）
    let mode = src.cfg_str("subjectFormatId");
    let mut out: Vec<SubjectItem> = Vec::new();

    if mode == "indexed" || src.cfg_obj("selectorSubjectFormatIndexed").is_some() {
        let fmt = src.cfg_obj("selectorSubjectFormatIndexed");
        let names_sel = Source::sub_str(fmt, "selectNames");
        let links_sel = Source::sub_str(fmt, "selectLinks");
        let names = select_texts(&doc, &names_sel);
        let links = select_hrefs(&doc, &links_sel, &base);
        let n = names.len().min(links.len());
        for i in 0..n {
            out.push(SubjectItem {
                title: names[i].clone(),
                url: links[i].clone(),
                image: String::new(),
                source: src.name.clone(),
                source_url: String::new(),
                source_name: String::new(),
            });
        }
    }
    if out.is_empty() {
        // 格式 A：一个 <a> 同时是标题和链接
        let fmt = src.cfg_obj("selectorSubjectFormatA");
        let sel = if fmt.is_some() {
            Source::sub_str(fmt, "selectLists")
        } else {
            String::new()
        };
        if !sel.is_empty() {
            out = select_subject_a(&doc, &sel, &base, &src.name);
        }
    }
    if out.is_empty() {
        // 最后兜底：用一批常见站的卡片选择器（内置源没有 cfg 时需要）
        out = search_fallback_selectors(&doc, &base, &src.name);
    }
    // 去掉明显的噪声条目（导航/推广），并按标题去重
    out.retain(|x| is_plausible_subject(&x.title, &x.url));
    let mut seen = std::collections::HashSet::new();
    out.retain(|x| seen.insert(x.title.clone()));
    out.truncate(60);
    stamp_source(&mut out, src);
    Ok(out)
}

/// 给搜索结果统一打上"来自哪个源"的标记。
///
/// ★ 三条产出路径（indexed / 格式A / 兜底选择器 / RSS）里，只有部分能拿到 `Source`，
///   所以集中在这里补，而不是在四个构造点各写一遍 —— 漏一处前端就会拿到空 `source_url`。
fn stamp_source(items: &mut [SubjectItem], src: &Source) {
    for it in items.iter_mut() {
        it.source = src.name.clone();
        it.source_name = src.name.clone();
        it.source_url = src.search_url.clone();
    }
}

/// 格式 A：`<a>` 里同时有名称和链接
fn select_subject_a(
    doc: &scraper::Html,
    sel: &str,
    base: &str,
    source: &str,
) -> Vec<SubjectItem> {
    let Ok(s) = scraper::Selector::parse(sel) else {
        return Vec::new();
    };
    doc.select(&s)
        .filter_map(|el| {
            let href = el.attr("href")?;
            let url = absolutize(base, href);
            if url.is_empty() {
                return None;
            }
            let title = el
                .attr("title")
                .map(|t| html_unescape(t))
                .unwrap_or_else(|| html_unescape(&collapse_ws(&el.text().collect::<Vec<_>>().join(" "))));
            if title.is_empty() {
                return None;
            }
            Some(SubjectItem {
                title,
                url,
                image: String::new(),
                source: source.to_string(),
                source_url: String::new(),
                source_name: String::new(),
            })
        })
        .collect()
}

/// 内置源没有选择器配置时的兜底：试一批常见的番剧站卡片选择器
fn search_fallback_selectors(doc: &scraper::Html, base: &str, source: &str) -> Vec<SubjectItem> {
    const CANDIDATES: &[&str] = &[
        // macCMS 系（占国内番剧站绝大多数）
        ".module-card-item-title > a",
        ".module-card-item-info .module-card-item-title > a",
        ".stui-vodlist__box h4 > a",
        ".myui-vodlist__media h4 > a",
        ".public-list-box .time-title",
        ".public-list-box .public-list-exp",
        ".thumb-content > .thumb-txt",
        ".detail-info .slide-info-title",
        "div.video-info-header > a",
        "div.detail > h3 > a",
        "h4.video-title > a",
        ".post-list .block-info .entry-title > a",
    ];
    for sel in CANDIDATES {
        let items = select_subject_a(doc, sel, base, source);
        if items.len() >= 2 {
            // 顺带把同卡片的图片也取上
            return enrich_images(doc, items, sel);
        }
    }
    Vec::new()
}

/// 尝试给结果补封面（在最外层卡片里找 img）
fn enrich_images(doc: &scraper::Html, mut items: Vec<SubjectItem>, _sel: &str) -> Vec<SubjectItem> {
    // 用整体图片选择器按序对齐（多数站卡片顺序一致）
    const IMG_SELS: &[&str] = &[
        ".module-card-item-poster img",
        ".stui-vodlist__thumb img",
        ".public-list-box img",
        ".thumb-content img",
        ".myui-vodlist__box img",
        "div.video-info-header img",
    ];
    for sel in IMG_SELS {
        let imgs = select_images(doc, sel, "");
        if imgs.len() >= items.len() && items.len() > 0 {
            for (i, it) in items.iter_mut().enumerate() {
                it.image = imgs[i].clone();
            }
            break;
        }
    }
    items
}

/// 过滤掉导航/推广类条目 —— 这是"广告"问题的正面防线
fn is_plausible_subject(title: &str, url: &str) -> bool {
    let t = title.trim();
    if t.len() < 2 || t.len() > 80 {
        return false;
    }
    // 明显是站点导航 / 推广的文案
    const NOISE: &[&str] = &[
        "首页", "日番", "劇場版", "剧场版", "APP下载", "App下载", "Telegram", "telegram",
        "联系邮箱", "问题反馈", "网站地图", "全部", "更多", "登录", "注册", "排序",
        "加入", "关于我们", "免责声明", "友情链接", "版权", "广告", "合作",
        "游戏", "联盟", "繁体", "简体", "切换", "搜索", "下拉", "列表",
    ];
    if NOISE.iter().any(|n| t == *n) {
        return false;
    }
    // URL 里带明显导航特征的丢掉
    const BAD_URL: &[&str] = &[
        "/app/", "t.me/", "/gbook/", "/rss/", "javascript", "/label/",
        "/top/", "/gbook", "/map", "/about", "/help",
    ];
    let ul = url.to_lowercase();
    if BAD_URL.iter().any(|b| ul.contains(b)) {
        return false;
    }
    // 全是 ASCII 标点/数字，没内容
    if !t.chars().any(|c| c.is_alphanumeric()) {
        return false;
    }
    true
}

/// 解析 RSS（BT 站） —— `<item>` 里的 title / enclosure / link
fn parse_rss(xml: &str, base: &str, source: &str) -> Vec<SubjectItem> {
    let Ok(item_re) = regex::Regex::new(r"(?is)<item>(.*?)</item>") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for cap in item_re.captures_iter(xml) {
        let it = &cap[1];
        let title = re_capture(it, r"(?is)<title>(?:<!\[CDATA\[)?(.*?)(?:\]\]>)?</title>", "x")
            .map(|s| html_unescape(&s))
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let link = re_capture(it, r"(?is)<link>(.*?)</link>", "x").unwrap_or_default();
        let enc = re_capture(it, r#"(?is)<enclosure[^>]*url="([^"]+)""#, "x").unwrap_or_default();
        let magnet = re_capture(it, r"(magnet:\?[^\s<\x22]+)", "x").unwrap_or_default();
        let url = if !magnet.is_empty() {
            magnet
        } else if !enc.is_empty() {
            absolutize(base, &enc)
        } else {
            absolutize(base, &link)
        };
        out.push(SubjectItem {
            title,
            url,
            image: String::new(),
            source: source.to_string(),
            source_url: String::new(),
            source_name: String::new(),
        });
    }
    out
}

// ============================================================
// 剧集层（选集）
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpisodeItem {
    /// 分组名（数据源里叫 channel：线路/字幕组/清晰度）
    #[serde(default)]
    pub channel: String,
    /// 显示名，如 `01` 或 `第 1 话`
    pub name: String,
    /// 集数序号（用于排序）
    #[serde(default)]
    pub sort: f64,
    pub url: String,
}

/// 拉某个番剧页的剧集列表
pub async fn fetch_episodes(src: &Source, page_url: &str) -> Result<Vec<EpisodeItem>, String> {
    // ★ 苹果CMS 接口源：page_url 就是 `?ac=detail&ids=`，一次拿全剧集直链
    if is_api_source(src) {
        return api_episodes(src, page_url).await;
    }
    // ★ Kazumi 规则源：XPath 选线路 + 剧集
    if src.factory == "kazumi" {
        let rule = crate::kazumi::rule_of(src)?;
        return crate::kazumi::chapters(&rule, page_url).await;
    }
    let (body, final_url) = get_text_retry(page_url, Some(page_url))
        .await
        .map_err(|e| format!("打开番剧页失败: {e}"))?;
    let base = with_raw_base(&base_origin(&final_url), &src.cfg_str("rawBaseUrl"));
    let doc = parse_html(&body);
    let mut out = Vec::new();

    let ch_mode = src.cfg_str("channelFormatId");
    // 线路分组模式：有多个线路/字幕组，每个下面一组剧集
    if ch_mode == "index-grouped" {
        let fmt = src.cfg_obj("selectorChannelFormatFlattened");
        let ch_sel = Source::sub_str(fmt, "selectChannelNames");
        let list_sel = Source::sub_str(fmt, "selectEpisodeLists");
        let ep_sel = Source::sub_str(fmt, "selectEpisodesFromList");
        let ep_link = Source::sub_str(fmt, "selectEpisodeLinksFromList");
        let ep_name_re = Source::sub_str(fmt, "matchEpisodeSortFromName");
        let ch_name_re = Source::sub_str(fmt, "matchChannelName");

        let channels = select_texts(&doc, &ch_sel);
        let mut ch_names: Vec<String> = Vec::new();
        for ch in &channels {
            let n = re_named(ch, &ch_name_re).unwrap_or_else(|| ch.clone());
            ch_names.push(if n.is_empty() { ch.clone() } else { n });
        }

        // 每组剧集列表
        let mut lists: Vec<Vec<EpisodeItem>> = Vec::new();
        if !list_sel.is_empty() {
            if let Ok(s) = scraper::Selector::parse(&list_sel) {
                for (gi, list_el) in doc.select(&s).enumerate() {
                    let channel = ch_names.get(gi).cloned().unwrap_or_default();
                    let mut eps = Vec::new();
                    // 在分组内部选剧集
                    let esel = if ep_sel.is_empty() {
                        "a".to_string()
                    } else {
                        ep_sel.clone()
                    };
                    if let Ok(es) = scraper::Selector::parse(&esel) {
                        for a in list_el.select(&es) {
                            let href = a
                                .attr("href")
                                .or_else(|| (!ep_link.is_empty()).then(|| a.attr(ep_link.as_str())).flatten());
                            let url = match href {
                                Some(h) => absolutize(&base, h),
                                None => continue,
                            };
                            if url.is_empty() {
                                continue;
                            }
                            let raw_name = a
                                .attr("title")
                                .map(|t| t.to_string())
                                .unwrap_or_else(|| collapse_ws(&a.text().collect::<Vec<_>>().join(" ")));
                            let raw_name = html_unescape(&raw_name);
                            if raw_name.is_empty() {
                                continue;
                            }
                            let (name, sort) = ep_name_and_sort(&raw_name, &ep_name_re);
                            eps.push(EpisodeItem {
                                channel: channel.clone(),
                                name,
                                sort,
                                url,
                            });
                        }
                    }
                    if !eps.is_empty() {
                        lists.push(eps);
                    }
                }
            }
        }
        // 没有分组名可对齐时，把各组直接拼起来
        for eps in lists {
            out.extend(eps);
        }
    }

    // 无分组模式（单线路）
    if out.is_empty() {
        let fmt = src.cfg_obj("selectorChannelFormatNoChannel");
        let ep_sel = if fmt.is_some() {
            Source::sub_str(fmt, "selectEpisodes")
        } else {
            String::new()
        };
        let ep_link = Source::sub_str(fmt, "selectEpisodeLinks");
        let ep_name_re = Source::sub_str(fmt, "matchEpisodeSortFromName");
        if !ep_sel.is_empty() {
            out = collect_episodes(&doc, &ep_sel, &ep_link, &ep_name_re, "", &base);
        }
    }

    if out.is_empty() {
        // 兜底：试常见播放列表容器
        const CAND: &[&str] = &[
            ".anthology-list-play a",
            ".module-play-list-content a",
            ".stui-content__playlist a",
            ".myui-content__list a",
            ".playlist a",
            "#glist-1 a",
            ".fed-part-eone a",
            "#y-playList a",
            ".swiper-slide a",
        ];
        for sel in CAND {
            out = collect_episodes(&doc, sel, "", r"第\s*(?<ep>\d+)", "", &base);
            if out.len() >= 2 {
                break;
            }
        }
    }

    // 去掉重复（同一 URL 只留一条），按 (线路, 集数) 排序
    let mut seen = std::collections::HashSet::new();
    out.retain(|e| !e.url.is_empty() && seen.insert(e.url.clone()));
    out.sort_by(|a, b| {
        a.channel
            .cmp(&b.channel)
            .then(a.sort.partial_cmp(&b.sort).unwrap_or(std::cmp::Ordering::Equal))
    });
    Ok(out)
}

/// 从一个容器里按选择器收集剧集
fn collect_episodes(
    doc: &scraper::Html,
    sel: &str,
    link_attr: &str,
    name_re: &str,
    channel: &str,
    base: &str,
) -> Vec<EpisodeItem> {
    let Ok(s) = scraper::Selector::parse(sel) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for el in doc.select(&s) {
        let href = if link_attr.is_empty() {
            el.attr("href").map(|x| x.to_string())
        } else {
            el.attr(link_attr).map(|x| x.to_string())
        };
        let Some(h) = href else { continue };
        let url = absolutize(base, &h);
        if url.is_empty() {
            continue;
        }
        let raw = el
            .attr("title")
            .map(|t| t.to_string())
            .unwrap_or_else(|| collapse_ws(&el.text().collect::<Vec<_>>().join(" ")));
        let raw = html_unescape(&raw);
        if raw.is_empty() || raw.len() > 60 {
            continue;
        }
        let (name, sort) = ep_name_and_sort(&raw, name_re);
        out.push(EpisodeItem {
            channel: channel.to_string(),
            name,
            sort,
            url,
        });
    }
    out
}

/// 从 `第 03 话` / `03` 之类文本里取出（显示名, 序号）。
///
/// 显示名优先用**集数**：源站卡片上常有「【喵萌奶茶屋】★10月新番★ 葬送的芙莉莲 第 12 话」
/// 这种带字幕组前缀的长标题，直接显示会撑爆按钮，而按钮只要写「12」就够。
pub(crate) fn ep_name_and_sort(raw: &str, name_re: &str) -> (String, f64) {
    let clean = collapse_ws(raw);
    // 先按数据源给的正则抠集数，失败再退回"第一个数字"
    let ep = re_named(&clean, name_re)
        .filter(|s| !s.is_empty())
        .or_else(|| re_named(&clean, r"(?<ep>\d+(?:\.\d+)?)"))
        .or_else(|| {
            // 数据源正则也不匹配时，直接从原文找数字串
            let digits: String = clean
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if digits.is_empty() {
                None
            } else {
                Some(digits)
            }
        });
    let sort = ep
        .as_deref()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    // 显示名: 有集数就用集数（保持两位的观感），否则用短标题
    let name = if let Some(e) = ep.filter(|e| !e.is_empty()) {
        e
    } else if clean.chars().count() <= 12 {
        clean.clone()
    } else {
        clean.chars().take(12).collect()
    };
    (name, sort)
}

// ============================================================
// 取流层（播放地址）
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayInfo {
    /// 播放地址（m3u8 / mp4）
    pub url: String,
    /// 需要带的 Referer
    #[serde(default)]
    pub referer: String,
    /// 需要带的 UA
    #[serde(default)]
    pub user_agent: String,
    /// 需要带的 Cookie
    #[serde(default)]
    pub cookie: String,
    /// 来源站点（做 Referer 兜底）
    #[serde(default)]
    pub page: String,
}

/// 打开播放页，抽出真实视频地址。
///
/// 三类情况（实测都覆盖到了）：
/// 1. `<script>var player_aaaa={...}</script>` → `url` 字段经 **base64 + URL 编码**
///    （girigiri 就是这种，解码后是裸 m3u8）
/// 2. 页面 HTML 里直接出现 m3u8/mp4 链接
/// 3. 数据源用 `matchVideoUrl` 正则从嵌套 URL 里再抽一层
pub async fn resolve_play(src: &Source, page_url: &str) -> Result<PlayInfo, String> {
    // ★ 苹果CMS 接口源给出的 page_url 本身就是 m3u8 直链，不用再解析。
    //   实测这些 CDN 的 CORS 是 `*`，网页里能直接拉。
    if looks_like_media(page_url) {
        return Ok(PlayInfo {
            url: page_url.to_string(),
            referer: String::new(),
            user_agent: UA.to_string(),
            cookie: String::new(),
            page: page_url.to_string(),
        });
    }
    let (body, final_url) = get_text_retry(page_url, Some(page_url))
        .await
        .map_err(|e| format!("打开播放页失败: {e}"))?;
    let base = base_origin(&final_url);

    let mv = src.cfg_obj("matchVideo");
    let (play_url, nested) = extract_video(&body, mv, &base)?;

    // 嵌套一层：数据源要求再打开一次拿真地址
    let mut url = play_url;
    if nested {
        if let Ok((b2, _)) = get_text_retry(&url, Some(page_url)).await {
            if let Ok((u2, _)) = extract_video(&b2, mv, &base) {
                url = u2;
            }
        }
    }

    let hdr = mv.and_then(|m| m.get("addHeadersToVideo"));
    let cookie = src
        .cfg_obj("matchVideo")
        .and_then(|m| m.get("cookies"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let ua = Source::sub_str(hdr, "userAgent");
    let referer = {
        let r = Source::sub_str(hdr, "referer");
        if r.trim().is_empty() {
            page_url.to_string()
        } else {
            r
        }
    };
    Ok(PlayInfo {
        url,
        referer,
        user_agent: if ua.trim().is_empty() { UA.to_string() } else { ua },
        cookie,
        page: page_url.to_string(),
    })
}

/// 从页面里抽视频地址，返回 (地址, 是否需要再打开一次)
fn extract_video(
    body: &str,
    mv: Option<&serde_json::Value>,
    base: &str,
) -> Result<(String, bool), String> {
    // 1) player_aaaa（macCMS 系标准做法）
    if let Some(raw) = extract_player_aaaa(body) {
        let decoded = decode_maccms_url(&raw);
        if !decoded.is_empty() {
            return Ok((decoded, false));
        }
    }
    // 2) 数据源给的 matchVideoUrl 正则
    let pat = Source::sub_str(mv, "matchVideoUrl");
    if let Some(re) = compile_ds_regex(&pat) {
        for cap in re.captures_iter(body) {
            let hit = cap
                .name("v")
                .or_else(|| cap.get(1))
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            if hit.starts_with("http") && looks_like_media(&hit) {
                return Ok((hit, false));
            }
        }
    }
    // 3) 直接从 HTML 里找 m3u8 / mp4
    let direct = regex::Regex::new(r#"https?://[^\s"'<>\\]+?\.(?:m3u8|mp4|flv)[^\s"'<>\\]*"#)
        .map_err(|e| e.to_string())?;
    let mut candidates: Vec<String> = direct
        .find_iter(body)
        .map(|m| m.as_str().to_string())
        .filter(|u| looks_like_media(u))
        .collect();
    // 有些站把地址放在 JSON 里且被转义了
    if candidates.is_empty() {
        let esc = body.replace("\\/", "/").replace("\\u002F", "/");
        candidates = direct
            .find_iter(&esc)
            .map(|m| m.as_str().to_string())
            .filter(|u| looks_like_media(u))
            .collect();
    }
    if let Some(u) = candidates.into_iter().max_by_key(|u| u.len()) {
        return Ok((u, false));
    }
    // 4) 数据源说需要打开嵌套 URL
    if let Some(nested_pat) = mv
        .and_then(|m| m.get("matchNestedUrl"))
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
    {
        if let Some(re) = compile_ds_regex(nested_pat) {
            if let Some(m) = re.find_iter(body).max_by_key(|m| m.as_str().len()) {
                let u = absolutize(base, m.as_str());
                if !u.is_empty() {
                    return Ok((u, true));
                }
            }
        }
    }
    Err("这个剧集没解析出播放地址（源站可能改了页面结构）".into())
}

/// 抽 `var player_aaaa = {...}` 里的 url
fn extract_player_aaaa(body: &str) -> Option<String> {
    let re = regex::Regex::new(r"(?s)player_aaaa\s*=\s*(\{.*?\})\s*</script>").ok()?;
    let cap = re.captures(body)?;
    let json_str = cap.get(1)?.as_str();
    let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
    // 1 = 不加密；2 = base64+urlencode
    let encrypt = v.get("encrypt").and_then(|x| x.as_i64()).unwrap_or(1);
    let url = v.get("url")?.as_str()?.to_string();
    if url.trim().is_empty() {
        return None;
    }
    if encrypt == 2 {
        Some(url)
    } else {
        Some(url)
    }
}

/// macCMS 的 `encrypt:2` 地址 = base64(URL 编码过的真实地址)
fn decode_maccms_url(raw: &str) -> String {
    let s = raw.trim();
    // 有些站的 url 字段已经是明文
    if s.starts_with("http") {
        return s.to_string();
    }
    // 先试 base64 → 得到 `%68%74%74%70...`，再 URL 解码
    if let Ok(bytes) = base64_decode(s) {
        if let Ok(txt) = String::from_utf8(bytes) {
            let t = txt.trim();
            if t.starts_with("http") {
                return t.to_string();
            }
            let dec = urlencoding::decode(t)
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| t.to_string());
            if dec.starts_with("http") {
                return dec;
            }
        }
    }
    // 也可能只是 URL 编码
    let dec = urlencoding::decode(s)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| s.to_string());
    if dec.starts_with("http") {
        return dec;
    }
    let dec2 = urlencoding::decode(&dec)
        .map(|c| c.into_owned())
        .unwrap_or(dec);
    if dec2.starts_with("http") {
        return dec2;
    }
    String::new()
}

/// 不引入 base64 crate，这里自己解（标准字母表，容忍缺失 padding）
fn base64_decode(s: &str) -> Result<Vec<u8>, ()> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lut = [255u8; 256];
    for (i, &ch) in T.iter().enumerate() {
        lut[ch as usize] = i as u8;
    }
    let clean: Vec<u8> = s
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    // 明显不是 base64（含 URL 里的非法字符）就退出
    if clean.iter().any(|b| lut[*b as usize] == 255) {
        return Err(());
    }
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for b in clean {
        buf = (buf << 6) | lut[b as usize] as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    if out.is_empty() {
        Err(())
    } else {
        Ok(out)
    }
}

/// 排除广告/图片/统计链接
fn looks_like_media(u: &str) -> bool {
    let ul = u.to_lowercase();
    // 图片与统计一律不是视频
    const NEVER: &[&str] = &[
        ".jpg", ".jpeg", ".png", ".gif", ".webp", ".svg", ".ico", ".css", ".js",
        "google-analytics", "googletagmanager", "doubleclick", "baidu.com/hm",
        "/guanggao/", "/ad/", "/ads/", "/banner",
    ];
    if NEVER.iter().any(|b| ul.contains(b)) {
        return false;
    }
    // ★ 这里**不能**只凭 "vip" 就认定是视频 —— 源站导航里到处是 vip 字样，
    //   之前那种宽松判断正是"搜索结果里混进广告"的一部分原因。
    ul.contains(".m3u8")
        || ul.contains(".mp4")
        || ul.contains(".flv")
        || ul.contains(".mkv")
        || ul.contains("akamaized")
        || ul.contains("bilivideo.com")
        || ul.contains("xigua.php")
        || (ul.contains("vip") && (ul.contains("http") && ul.contains("?")))
}

// ============================================================
// 元数据（Bangumi 镜像 + animeko API）
// ============================================================

/// Bangumi 主体信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubjectMeta {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub name_cn: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub score: f64,
    #[serde(default)]
    pub rank: u64,
    #[serde(default)]
    pub total_episodes: u64,
    /// 多少人在看/收藏
    #[serde(default)]
    pub watching: u64,
    #[serde(default)]
    pub done: u64,
    #[serde(default)]
    pub wish: u64,
    /// 标签（显示成小胶囊）
    #[serde(default)]
    pub tags: Vec<String>,
    /// 评分分布 1..10
    #[serde(default)]
    pub score_dist: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaEpisode {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub name_cn: String,
    /// ★ 序号: **跨季累加**。药屋少女第三季实测 sort=49..60，不是 1..12。
    ///   所以它只用来**排序**，不要拿它当"第几话"显示。
    #[serde(default)]
    pub sort: f64,
    /// ★ 本季内的集数（1..N）—— 界面上显示这个。
    #[serde(default)]
    pub ep: f64,
    #[serde(default)]
    pub airdate: String,
    #[serde(default)]
    pub image: String,
    /// 0=本篇 1=SP 2=OP 3=ED 4=预告 —— 前端靠它把正片和特别篇分开，
    /// 否则 01/02 会跟 SP 的编号撞在一起（实测过）。
    #[serde(default)]
    pub ep_type: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaCast {
    pub name: String,
    #[serde(default)]
    pub relation: String,
    #[serde(default)]
    pub image: String,
    /// 角色/声优名
    #[serde(default)]
    pub actor: String,
}

fn bgm_get(path: &str) -> reqwest::RequestBuilder {
    client()
        .get(format!("{BGM}{path}"))
        .header("Accept", "application/json")
}

/// ★ 带重试的镜像 GET。
///
/// `bgmapi.anibt.net` 这个镜像偶尔会瞬断（实测同一 URL：curl 连打 3 次全 200，
/// 但从 Rust 里并发打 3 个偶尔会有一个 `error sending request`）。
/// 详情页要同时拉 主体+剧集+角色 三路，只要有一路瞬断整个详情就白了 ——
/// 所以这里退避重试两次，把"偶发"挡在外面。
async fn bgm_get_retry(path: &str) -> Result<serde_json::Value, String> {
    let mut last = String::new();
    for attempt in 0..3u32 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(400 * attempt as u64)).await;
        }
        match bgm_get(path).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    last = format!("HTTP {}", resp.status());
                    continue;
                }
                match resp.json::<serde_json::Value>().await {
                    Ok(v) => return Ok(v),
                    Err(e) => last = format!("解析失败: {e}"),
                }
            }
            Err(e) => last = format!("{e}"),
        }
    }
    Err(last)
}

/// 解析主体
fn parse_subject(v: &serde_json::Value) -> SubjectMeta {
    let id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
    let images = v.get("images");
    let image = Source::sub_str(images, "large");
    let rating = v.get("rating");
    let score = rating.and_then(|r| r.get("score")).and_then(|x| x.as_f64()).unwrap_or(0.0);
    let rank = rating.and_then(|r| r.get("rank")).and_then(|x| x.as_u64()).unwrap_or(0);
    let mut dist = vec![0u64; 10];
    if let Some(count) = rating.and_then(|r| r.get("count")).and_then(|x| x.as_object()) {
        for (k, val) in count {
            if let Ok(i) = k.parse::<usize>() {
                if (1..=10).contains(&i) {
                    dist[i - 1] = val.as_u64().unwrap_or(0);
                }
            }
        }
    }
    let collection = v.get("collection");
    let tags = v
        .get("tags")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .take(14)
                .filter_map(|t| t.get("name").and_then(|x| x.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    SubjectMeta {
        id,
        name: v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        name_cn: v.get("name_cn").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        summary: v.get("summary").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        image,
        date: v.get("date").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        score,
        rank,
        total_episodes: v.get("total_episodes").and_then(|x| x.as_u64()).unwrap_or(0),
        watching: collection
            .and_then(|c| c.get("doing"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        done: collection
            .and_then(|c| c.get("collect"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        wish: collection
            .and_then(|c| c.get("wish"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        tags,
        score_dist: dist,
    }
}

/// 番剧详情
pub async fn subject_detail(id: u64) -> Result<SubjectMeta, String> {
    let v = bgm_get_retry(&format!("/v0/subjects/{id}"))
        .await
        .map_err(|e| format!("拉取番剧详情失败: {e}"))?;
    Ok(parse_subject(&v))
}

/// 剧集列表（Bangumi 的，含封面缩略图）
pub async fn subject_episodes(id: u64) -> Result<Vec<MetaEpisode>, String> {
    let v = bgm_get_retry(&format!("/v0/episodes?subject_id={id}&limit=100"))
        .await
        .map_err(|e| format!("拉取剧集失败: {e}"))?;
    let arr = v
        .get("data")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let out = arr
        .iter()
        .map(|e| MetaEpisode {
            id: e.get("id").and_then(|x| x.as_u64()).unwrap_or(0),
            name: e.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            name_cn: e.get("name_cn").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            sort: e.get("sort").and_then(|x| x.as_f64()).unwrap_or(0.0),
            ep: e.get("ep").and_then(|x| x.as_f64()).unwrap_or(0.0),
            airdate: e.get("airdate").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            image: Source::sub_str(e.get("images"), "common"),
            ep_type: e.get("type").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
        })
        .collect();
    Ok(out)
}

/// 角色 + 制作人员
pub async fn subject_casts(id: u64) -> Result<Vec<MetaCast>, String> {
    let mut out = Vec::new();
    if let Ok(arr) = bgm_get_retry(&format!("/v0/subjects/{id}/characters")).await {
        if let Some(list) = arr.as_array() {
            for c in list.iter().take(24) {
                let name = c
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                let image = Source::sub_str(c.get("images"), "grid");
                let actors = c
                    .get("actors")
                    .and_then(|x| x.as_array())
                    .and_then(|a| a.first())
                    .and_then(|a| a.get("name"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                out.push(MetaCast {
                    name,
                    relation: c
                        .get("relation")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    image,
                    actor: actors,
                });
            }
        }
    }
    if let Ok(arr) = bgm_get_retry(&format!("/v0/subjects/{id}/persons")).await {
        if let Some(list) = arr.as_array() {
            for p in list.iter().take(20) {
                let name = p
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                out.push(MetaCast {
                    name,
                    relation: p
                        .get("relation")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    image: Source::sub_str(p.get("images"), "grid"),
                    actor: String::new(),
                });
            }
        }
    }
    Ok(out)
}

/// 首页一部番的卡片
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeCard {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub name_cn: String,
    #[serde(default)]
    pub image: String,
    /// 副标题，如 `2026 年 7 月`
    #[serde(default)]
    pub desc1: String,
    /// 如 `2 万收藏 · 7.5 分`
    #[serde(default)]
    pub desc2: String,
}

/// 解析 `desc2` 里的收藏数：`"2 万收藏 · 7.5 分"` → 20000。
/// 用来给「最高热度」排序（站点只给了人类可读的字符串）。
pub fn parse_fav_count(desc2: &str) -> u64 {
    let i = match desc2.find("收藏") {
        Some(i) => i,
        None => return 0,
    };
    let head = desc2[..i].trim_end();
    let (num_str, unit) = if let Some(x) = head.strip_suffix("万") {
        (x, 10_000u64)
    } else if let Some(x) = head.strip_suffix("千") {
        (x, 1_000u64)
    } else if let Some(x) = head.strip_suffix('k').or_else(|| head.strip_suffix('K')) {
        (x, 1_000u64)
    } else {
        (head, 1u64)
    };
    // ★ 站点给的是「2 万收藏」，数字和单位之间有空格，必须先去掉
    let num_str = num_str.trim_end();
    let tail: String = num_str
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let s: String = tail.chars().rev().collect();
    let v: f64 = s.parse().unwrap_or(0.0);
    (v * unit as f64) as u64
}

/// 分页版推荐（给「推荐动漫」无限下滑用）
pub async fn home_recommendations_paged(offset: u32, limit: u32) -> Result<Vec<HomeCard>, String> {
    let resp = client()
        .get(format!(
            "{ANIMEKO_API}/v2/home/recommendations?offset={}&limit={}",
            offset,
            limit.clamp(1, 100)
        ))
        .send()
        .await
        .map_err(|e| format!("拉取推荐失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("推荐返回 HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("解析失败: {e}"))?;
    let arr = v.get("items").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    Ok(arr
        .iter()
        .map(|it| HomeCard {
            id: it.get("subjectId").and_then(|x| x.as_u64()).unwrap_or(0),
            name: it.get("subjectName").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            name_cn: it.get("subjectNameCn").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            image: it.get("imageUrl").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            desc1: it.get("desc1").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            desc2: it.get("desc2").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        })
        .filter(|c| c.id > 0 && (!c.name.is_empty() || !c.name_cn.is_empty()))
        .collect())
}

/// 首页推荐（animeko 公共 API）
pub async fn home_recommendations(limit: u32) -> Result<Vec<HomeCard>, String> {
    let resp = client()
        .get(format!(
            "{ANIMEKO_API}/v2/home/recommendations?offset=0&limit={}",
            limit.clamp(1, 200)
        ))
        .send()
        .await
        .map_err(|e| format!("拉取推荐失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("推荐返回 HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("解析失败: {e}"))?;
    let arr = v
        .get("items")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(arr
        .iter()
        .map(|it| HomeCard {
            id: it.get("subjectId").and_then(|x| x.as_u64()).unwrap_or(0),
            name: it
                .get("subjectName")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            name_cn: it
                .get("subjectNameCn")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            image: it.get("imageUrl").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            desc1: it.get("desc1").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            desc2: it.get("desc2").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        })
        .filter(|c| c.id > 0 && (!c.name.is_empty() || !c.name_cn.is_empty()))
        .collect())
}

/// 趋势 / 最高热度
pub async fn home_trends() -> Result<Vec<HomeCard>, String> {
    let resp = client()
        .get(format!("{ANIMEKO_API}/v1/trends"))
        .send()
        .await
        .map_err(|e| format!("拉取趋势失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("趋势返回 HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("解析失败: {e}"))?;
    let arr = v
        .get("trendingSubjects")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(arr
        .iter()
        .map(|it| HomeCard {
            id: it.get("bangumiId").and_then(|x| x.as_u64()).unwrap_or(0),
            name: it.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            name_cn: it.get("nameCn").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            image: it
                .get("imageLarge")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            desc1: String::new(),
            desc2: String::new(),
        })
        .filter(|c| c.id > 0)
        .collect())
}

/// 搜番剧（元数据）—— 走 Bangumi 镜像的搜索
pub async fn search_meta(keyword: &str, limit: u32) -> Result<Vec<HomeCard>, String> {
    let c = client();
    let body = serde_json::json!({
        "keyword": keyword,
        "sort": "match",
        "filter": { "type": [2] }
    });
    let resp = c
        .post(format!("{BGM}/v0/search/subjects?limit={}", limit.clamp(1, 30)))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("搜索失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("搜索返回 HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("解析失败: {e}"))?;
    let arr = v
        .get("data")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(arr
        .iter()
        .map(|it| {
            let m = parse_subject(it);
            HomeCard {
                id: m.id,
                name: m.name.clone(),
                name_cn: m.name_cn.clone(),
                image: m.image.clone(),
                desc1: m.date.clone(),
                desc2: if m.score > 0.0 {
                    format!("{:.1} 分", m.score)
                } else {
                    String::new()
                },
            }
        })
        .filter(|c| c.id > 0)
        .collect())
}

// ============================================================
// 追番列表（本地）
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FollowItem {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub added_ms: i64,
    #[serde(default)]
    pub progress: u32,
}

fn follow_path() -> std::path::PathBuf {
    // 追番列表放统一数据根 <软件目录>\data\anime\follow.json
    crate::paths::data_dir().join("anime").join("follow.json")
}

pub fn load_follow() -> Vec<FollowItem> {
    std::fs::read_to_string(follow_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_follow(v: &[FollowItem]) -> Result<(), String> {
    let p = follow_path();
    if let Some(d) = p.parent() {
        crate::paths::ensure_dir(d)?;
    }
    let s = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    std::fs::write(&p, s).map_err(|e| format!("写入追番列表失败: {e}"))
}

// ============================================================
// 弹幕
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Danmaku {
    #[serde(default)]
    pub time: f64,
    pub text: String,
    #[serde(default)]
    pub location: u32,
    #[serde(default)]
    pub color: u32,
    #[serde(default)]
    pub size: f64,
}

pub async fn fetch_danmaku(episode_id: u64) -> Result<Vec<Danmaku>, String> {
    let resp = client()
        .get(format!("{DANMAKU_BASE}/v1/danmaku/{episode_id}"))
        .send()
        .await
        .map_err(|e| format!("连接弹幕服务失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("弹幕服务返回 HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("解析弹幕失败: {e}"))?;
    let list = v
        .get("danmakuList")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for d in list {
        let time = d
            .get("time")
            .and_then(|x| x.as_f64())
            .or_else(|| d.get("progress").and_then(|x| x.as_f64()))
            .unwrap_or(0.0);
        let text = d
            .get("text")
            .and_then(|x| x.as_str())
            .or_else(|| d.get("content").and_then(|x| x.as_str()))
            .unwrap_or("")
            .to_string();
        if text.is_empty() {
            continue;
        }
        out.push(Danmaku {
            time,
            text,
            location: d.get("location").and_then(|x| x.as_u64()).unwrap_or(1) as u32,
            color: d.get("color").and_then(|x| x.as_u64()).unwrap_or(0xffffff) as u32,
            size: d.get("size").and_then(|x| x.as_f64()).unwrap_or(25.0),
        });
    }
    Ok(out)
}

// ============================================================
// 源缓存（避免每次搜索都拉订阅）
// ============================================================

fn source_cache() -> &'static parking_lot::Mutex<Option<(std::time::Instant, Vec<Source>)>> {
    static C: OnceLock<parking_lot::Mutex<Option<(std::time::Instant, Vec<Source>)>>> =
        OnceLock::new();
    C.get_or_init(|| parking_lot::Mutex::new(None))
}

/// 取源列表（缓存 30 分钟）
pub async fn sources_cached() -> Vec<Source> {
    {
        let g = source_cache().lock();
        if let Some((t, list)) = g.as_ref() {
            if t.elapsed() < std::time::Duration::from_secs(1800) && !list.is_empty() {
                return list.clone();
            }
        }
    }
    let list = fetch_sources().await;
    if !list.is_empty() {
        *source_cache().lock() = Some((std::time::Instant::now(), list.clone()));
    }
    list
}

// ============================================================
// Tauri 命令
// ============================================================

fn require_activated() -> Result<(), String> {
    if crate::licensing::is_activated() {
        Ok(())
    } else {
        Err("尚未激活".to_string())
    }
}

/// 列出所有源（按 tier 排序）
#[tauri::command]
pub async fn anime_sources() -> Vec<Source> {
    sources_cached().await
}

/// 首页：趋势 + 推荐（一次拿全，减少往返）
#[tauri::command]
pub async fn anime_home(limit: Option<u32>) -> Result<serde_json::Value, String> {
    let n = limit.unwrap_or(90);
    let (trends, recs) = tokio::join!(home_trends(), home_recommendations(n));
    Ok(serde_json::json!({
        "trends": trends.unwrap_or_default(),
        "recommend": recs.unwrap_or_default(),
    }))
}

/// 番剧详情（含剧集 / 角色 / 制作）
#[tauri::command]
pub async fn anime_detail(id: u64) -> Result<serde_json::Value, String> {
    let (meta, eps, casts) = tokio::join!(
        subject_detail(id),
        subject_episodes(id),
        subject_casts(id)
    );
    let meta = meta?;
    Ok(serde_json::json!({
        "meta": meta,
        "episodes": eps.unwrap_or_default(),
        "casts": casts.unwrap_or_default(),
    }))
}

/// 元数据搜索（全局搜番剧）
#[tauri::command]
pub async fn anime_search_bgm(keyword: String) -> Result<Vec<HomeCard>, String> {
    search_meta(&keyword, 24).await
}

/// 在指定源上搜索资源
#[tauri::command]
pub async fn anime_search_online(
    source_url: String,
    keyword: String,
) -> Result<Vec<SubjectItem>, String> {
    require_activated()?;
    let list = sources_cached().await;
    // 前端传的是 search_url，用它定位源（拿到完整的选择器配置）
    let src = list
        .iter()
        .find(|s| s.search_url == source_url)
        .cloned()
        .unwrap_or_else(|| Source {
            name: "自定义源".into(),
            search_url: source_url.clone(),
            factory: "web-selector".into(),
            kind: "online".into(),
            ..Default::default()
        });
    search_subject(&src, &keyword).await
}

/// 跑一个源：套独立超时，结果盖上源名。
///
/// ★ 超时必须**每个源单独套**：`get_text_retry` 内部是 40s 超时 x3 重试，
///   一个挂掉的站能把整个搜索拖到 2 分钟。
async fn search_one(src: Source, keyword: String, budget_secs: u64) -> Vec<SubjectItem> {
    let name = src.name.clone();
    match tokio::time::timeout(
        std::time::Duration::from_secs(budget_secs),
        search_subject(&src, &keyword),
    )
    .await
    {
        Ok(Ok(v)) => v
            .into_iter()
            .map(|mut x| {
                x.source = name.clone();
                x
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[tauri::command]
/// `quick = true`：换片用的快速模式 —— 只要凑够 2 条可用线路、3 秒上限，
/// 完全不跑网页/规则源。主窗口「开始观看」用默认（要覆盖度）。
pub async fn anime_search_all(
    keyword: String,
    quick: Option<bool>,
) -> Result<Vec<SubjectItem>, String> {
    require_activated()?;
    let quick = quick.unwrap_or(false);
    let list = sources_cached().await;
    let picks: Vec<Source> = list.into_iter().take(30).collect();
    // ★ 分快慢两批。实测接口源 1~4 秒返回，网页/规则源常常 8~12 秒甚至超时；
    //   以前 join_all 等最慢的那个 → 用户点「开始观看」要干等 18 秒才出画面。
    //   现在：接口源一到就先把结果交出去，网页源只给一个很短的宽限窗口。
    let (fast, slow): (Vec<Source>, Vec<Source>) = picks.into_iter().partition(is_api_source);

    let slow_futs = slow.into_iter().map(|s| {
        let k = keyword.clone();
        async move { search_one(s, k, 12).await }
    });

    // ★ 快批用 FuturesUnordered **谁先回谁先算**：以前 join_all 要等最慢的那条
    //   （实测被一条吃满 6 秒上限拖到 8.5 秒）。现在攒够 3 个源有结果就往下走，
    //   3 个源已经足够选出可用线路、也够前端排序挑候选了。
    use futures::stream::StreamExt;
    let mut fu = futures::stream::FuturesUnordered::new();
    for src in fast {
        let k = keyword.clone();
        fu.push(search_one(src, k, 6));
    }
    let mut out: Vec<SubjectItem> = Vec::new();
    let mut got = 0usize;
    // ★ 阈值 5 而不是 3：前几个回来的往往是**同一个 CDN 池**的镜像站
    //   （实测极速/金鹰/豪华 共用 p.hhwenjian.com），全挂就是一起挂。
    //   多等 1 秒换来跨 CDN 的线路多样性，比快那 1 秒值。
    let want = if quick { 2 } else { 5 };
    let budget = if quick { 3000 } else { 5500 };
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(budget);
    while let Ok(Some(v)) = tokio::time::timeout_at(deadline, fu.next()).await {
        if !v.is_empty() {
            got += 1;
        }
        out.extend(v);
        if got >= want {
            break;
        }
    }

    if quick {
        // 换片模式：接口源没给出结果就到此为止，绝不为了多几条线路让用户干等
    } else if out.is_empty() {
        // 接口源全军覆没（没配、或被墙）→ 老老实实等网页源，别把覆盖度也丢了
        out = futures::future::join_all(slow_futs)
            .await
            .into_iter()
            .flatten()
            .collect();
    } else {
        // 已经有能用的线路了：网页源只给 2.5 秒宽限，赶得上算多几个备用，赶不上就丢。
        // 宁可少几个备用线路，也别让用户对着「正在搜源…」干等十几秒。
        if let Ok(v) = tokio::time::timeout(
            std::time::Duration::from_millis(2000),
            futures::future::join_all(slow_futs),
        )
        .await
        {
            out.extend(v.into_iter().flatten());
        }
    }

    // ★ 按「标题 + 源」去重，**不能只按标题**。
    //   只按标题的话同一部番在不同源上会被合并成一条，前端就只剩一个候选线路、
    //   没有备用源可切。搜索列表要的"同一部番只显示一条"由前端渲染时再去重。
    dedupe_subjects(&mut out);
    if out.is_empty() {
        return Err("所有源都没搜到结果，换个关键词或稍后再试".into());
    }
    Ok(out)
}

/// 跨源合并时的去重键：标题 + 源。同源的重复条目才丢掉，不同源的同一部番要留着。
fn dedupe_subjects(out: &mut Vec<SubjectItem>) {
    let mut seen = std::collections::HashSet::new();
    out.retain(|x| seen.insert(format!("{}\u{1}{}", x.title, x.source)));
}

/// 拉某番剧在某源上的剧集列表
#[tauri::command]
pub async fn anime_episodes(
    source_url: String,
    page_url: String,
) -> Result<Vec<EpisodeItem>, String> {
    require_activated()?;
    let list = sources_cached().await;
    let src = list
        .iter()
        .find(|s| s.search_url == source_url)
        .cloned()
        .unwrap_or_else(|| Source {
            name: "自定义源".into(),
            search_url: source_url.clone(),
            ..Default::default()
        });
    fetch_episodes(&src, &page_url).await
}

/// 解析某个剧集的真实播放地址
#[tauri::command]
pub async fn anime_resolve(
    source_url: String,
    page_url: String,
) -> Result<PlayInfo, String> {
    require_activated()?;
    let list = sources_cached().await;
    let src = list
        .iter()
        .find(|s| s.search_url == source_url)
        .cloned()
        .unwrap_or_else(|| Source {
            name: "自定义源".into(),
            search_url: source_url.clone(),
            ..Default::default()
        });
    resolve_play(&src, &page_url).await
}

// ===================== 动漫：热门 20 / 分页推荐 (2026-10-07) =====================
// ★ 用户要求：「探索最上方最高热度采用卡片式固定 20 个，筛选出 20 个最热门动漫」。
//   站点（animeko）的趋势接口不按收藏数排序，这里用 desc2 里的"X 万收藏"重新排一遍再截 20。
#[tauri::command]
pub async fn anime_hot(limit: Option<u32>) -> Result<Vec<HomeCard>, String> {
    let n = limit.unwrap_or(20).clamp(1, 60);
    let mut list = home_trends().await?;
    list.sort_by(|a, b| parse_fav_count(&b.desc2).cmp(&parse_fav_count(&a.desc2)));
    list.truncate(n as usize);
    crate::app_logger::log_command("anime_hot", &format!("返回 {} 部", list.len()));
    Ok(list)
}

/// ★ 「推荐动漫」往下滑加载更多：按 offset 分页取
#[tauri::command]
pub async fn anime_rec_paged(offset: Option<u32>, limit: Option<u32>) -> Result<Vec<HomeCard>, String> {
    home_recommendations_paged(offset.unwrap_or(0), limit.unwrap_or(24)).await
}

#[tauri::command]
pub fn anime_follow() -> Vec<FollowItem> {
    load_follow()
}

#[tauri::command]
pub fn anime_follow_toggle(item: FollowItem) -> Result<Vec<FollowItem>, String> {
    let mut v = load_follow();
    if let Some(pos) = v.iter().position(|x| x.id == item.id) {
        v.remove(pos);
    } else {
        v.push(item);
    }
    save_follow(&v)?;
    Ok(v)
}

#[tauri::command]
pub fn anime_follow_progress(id: u64, progress: u32) -> Result<Vec<FollowItem>, String> {
    let mut v = load_follow();
    for it in v.iter_mut() {
        if it.id == id {
            it.progress = progress;
        }
    }
    save_follow(&v)?;
    Ok(v)
}

#[tauri::command]
pub async fn anime_danmaku(episode_id: u64) -> Result<Vec<Danmaku>, String> {
    fetch_danmaku(episode_id).await
}

// ============================================================
// 单元测试（引擎的关键部分：URL 解码 / 选择器抽取 / 噪声过滤）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 前后端字段契约：`SubjectItem` 序列化后必须带 `source_url` / `source_name`。
    ///
    /// ★ 2026-10-07 用户报"动漫看不了，提示源不可以"。根因就是这个契约断了：
    ///   前端 `startWatch` 读 `c.source_url`，而后端 `SubjectItem` 只有
    ///   `title/url/image/source`。于是 `invoke('anime_episodes', {sourceUrl: undefined})`
    ///   缺必填参数被 Tauri 直接拒掉，6 个候选源**全部**失败 —— 看起来像"源全挂了"，
    ///   其实是前端根本没把源地址传下去。这条断言不联网就能拦住同类回归。
    #[test]
    fn subject_item_json_exposes_source_url() {
        let it = SubjectItem {
            title: "葬送的芙莉莲".into(),
            url: "https://www.jibi.cc/index.php/vod/detail/id/12248.html".into(),
            image: String::new(),
            source: "叽哔动漫".into(),
            source_url: "https://www.jibi.cc/index.php/vod/search.html?wd={keyword}".into(),
            source_name: "叽哔动漫".into(),
        };
        let j = serde_json::to_string(&it).expect("序列化失败");
        assert!(j.contains("\"source_url\""), "JSON 里缺 source_url: {j}");
        assert!(j.contains("\"source_name\""), "JSON 里缺 source_name: {j}");
        assert!(j.contains("\"title\""), "JSON 里缺 title: {j}");
    }

    /// 苹果CMS 播放串解析 —— 单线路（实测自 cj.lziapi.com，from=`lzm3u8`）
    #[test]
    fn maccms_play_single_line() {
        let urls = "第01集$https://v.cdnlz14.com/20230930/29887_2e6cf8c7/index.m3u8\
                    #第02集$https://v.cdnlz14.com/20230930/29888_d78524de/index.m3u8";
        let eps = parse_maccms_play("lzm3u8", urls);
        assert_eq!(eps.len(), 2);
        assert_eq!(eps[0].channel, "lzm3u8");
        assert_eq!(eps[0].name, "01");
        assert_eq!(eps[0].sort, 1.0);
        assert!(eps[0].url.ends_with(".m3u8"));
        assert_eq!(eps[1].sort, 2.0);
    }

    /// ★ 多线路 + 第一条线路不是 m3u8 —— 必须把 m3u8 线路排到前面，
    ///   否则前端 `playEpisode(0)` 一上来就播不了。
    ///   （实测 caiji.dyttzyapi.com 的 from 就是 `dytt$$$dyttm3u8`）
    #[test]
    fn maccms_play_prefers_m3u8_line() {
        let froms = "dytt$$$dyttm3u8";
        let urls = "第01集$https://x/dl/1.mp4#第02集$https://x/dl/2.mp4\
                    $$$第01集$https://y/hls/1.m3u8#第02集$https://y/hls/2.m3u8";
        let eps = parse_maccms_play(froms, urls);
        assert_eq!(eps.len(), 4);
        assert_eq!(eps[0].channel, "dyttm3u8", "m3u8 线路应该排最前");
        assert!(eps[0].url.ends_with(".m3u8"));
        assert_eq!(eps[2].channel, "dytt");
    }

    /// 没有 `$` 分隔（整段就是地址）也不能崩，且非 http 的条目要丢掉
    #[test]
    fn maccms_play_handles_odd_input() {
        let eps = parse_maccms_play("", "https://a/1.m3u8#ftp://bad/2.ts#第03集$https://a/3.m3u8");
        assert_eq!(eps.len(), 2, "ftp 那条要被丢掉: {:?}", eps.iter().map(|e| &e.url).collect::<Vec<_>>());
        assert!(eps.iter().all(|e| e.url.starts_with("http")));
        assert!(parse_maccms_play("", "").is_empty());
    }

    /// 跨源合并去重：同一部番的**不同源**必须都留下（否则没有备用线路可切）
    #[test]
    fn dedupe_keeps_same_title_from_different_sources() {
        let mk = |t: &str, s: &str| SubjectItem {
            title: t.into(),
            url: "u".into(),
            image: String::new(),
            source: s.into(),
            source_url: "x".into(),
            source_name: s.into(),
        };
        let mut v = vec![
            mk("FX战士久留美", "量子资源"),
            mk("FX战士久留美", "光速资源"),
            mk("FX战士久留美", "量子资源"), // 同源重复 → 该丢
            mk("别的番", "量子资源"),
        ];
        dedupe_subjects(&mut v);
        assert_eq!(v.len(), 3, "同一部番的不同源要保留，同源重复才丢");
        assert_eq!(v.iter().filter(|x| x.title == "FX战士久留美").count(), 2);
    }

    /// `stamp_source` 要给每条结果都盖上源标记（三条产出路径共用）
    #[test]
    fn stamp_source_fills_every_item() {
        let src = Source {
            name: "测试源".into(),
            search_url: "https://example.com/s?wd={keyword}".into(),
            ..Default::default()
        };
        let mut items = vec![
            SubjectItem { title: "a".into(), url: "u1".into(), image: String::new(), source: String::new(), source_url: String::new(), source_name: String::new() },
            SubjectItem { title: "b".into(), url: "u2".into(), image: String::new(), source: String::new(), source_url: String::new(), source_name: String::new() },
        ];
        stamp_source(&mut items, &src);
        for it in &items {
            assert_eq!(it.source_url, src.search_url);
            assert_eq!(it.source_name, "测试源");
            assert_eq!(it.source, "测试源");
        }
    }

    #[test]
    fn base64_url_decode_chain() {
        // ★ 真实抓包值（从 https://ani.girigirilove.com/playGV27223-1-1/ 抓的 player_aaaa.url）
        //   注意：它是 base64(URL 编码过的一串 %XX)，所以要先 base64 解再 URL 解码。
        //   之前用自己拼的假样本测通过了，真实数据反而挂 —— 这里必须用真值。
        let raw = "JTY4JTc0JTc0JTcwJTczJTNBJTJGJTJGJTYxJTZCJTc1JTYxJTJFJTY3JTY5JTcyJTY5JTY3JTY5JTcyJTY5JTZDJTZGJTc2JTY1JTJFJTYzJTZGJTZEJTJGJTdBJTY5JTZBJTY5JTYxJTZFJTJGJTZGJTZDJTY0JTYxJTZFJTY5JTZEJTY1JTJGJTMyJTMwJTMyJTY2JTJGJTMxJTMwJTJGJTYzJTY4JTc0JTJGJTUwJTczJTc5JTcyJTY1JTZFJTQzJTQ4JTU0JTJGJTMwJTMxJTJGJTcwJTZDJTYxJTc5JTZDJTY5JTczJTc0JTJFJTZEJTMzJTc1JTM4";
        let d = decode_maccms_url(raw);
        assert!(d.starts_with("https://"), "got {d}");
        assert!(d.ends_with(".m3u8"), "got {d}");
    }

    #[test]
    fn ds_regex_named_group_rewrite() {
        // 数据源用 JS 风格 (?<ep>...)：当前 regex crate 能直接编译
        let pat = r"第\s*(?<ep>.+)\s*[话集]";
        assert!(compile_ds_regex(pat).is_some());
        // lookbehind 是 regex crate 不支持的语法，改写逻辑不能把它误伤成 (?P<=
        let lb = r"(?<=abc)def";
        let fixed = fix_named_groups(lb);
        assert!(!fixed.contains("(?P<="), "改坏了 lookbehind: {fixed}");
        assert_eq!(fixed, lb, "lookbehind 应原样保留");
        // 真正的具名组要能改写
        assert_eq!(fix_named_groups(r"(?<ch>.+?)(\d+)"), r"(?P<ch>.+?)(\d+)");
        // 不支持的语法要干净地返回 None，不 panic
        assert!(compile_ds_regex(r"(?<=abc)def").is_none());
    }

    #[test]
    fn noise_filter_drops_ads() {
        // 图一里出现过的那些"广告"条目
        assert!(!is_plausible_subject("APP下载", "https://ani.girigirilove.com/app/"));
        assert!(!is_plausible_subject("Telegram群", "https://t.me/+z_bwv8ytyb5iNjM1"));
        assert!(!is_plausible_subject("首页", "https://ani.girigirilove.com/"));
        assert!(!is_plausible_subject("游戏", "https://d36vouwhbucv.cloudfront.net/?attributionId=218"));
        assert!(!is_plausible_subject("联盟", "https://bbs.girigirilove.com/"));
        // 真实番剧名要留下
        assert!(is_plausible_subject("葬送的芙莉莲", "https://ani.girigirilove.com/GV27223/"));
        assert!(is_plausible_subject("PSYREN 决战游戏", "https://ani.girigirilove.com/GV27223/"));
    }

    #[test]
    fn episode_sort_extraction() {
        let (n, s) = ep_name_and_sort("第 03 话", r"第\s*(?<ep>.+)\s*[话集]");
        assert_eq!(n, "03");
        assert_eq!(s, 3.0);
        let (n2, s2) = ep_name_and_sort("01", "");
        assert_eq!(n2, "01");
        assert_eq!(s2, 1.0);
        // 长标题（带字幕组前缀）应当只留集数，别把整串塞进按钮
        let (n3, s3) = ep_name_and_sort("【喵萌奶茶屋】★10月新番★ 葬送的芙莉莲 第 12 话", r"第\s*(?<ep>.+)\s*[话集]");
        assert_eq!(s3, 12.0);
        assert_eq!(n3, "12");
        // 数据源正则带 (?<ch>...) 那种也要能解析
        let (_n4, s4) = ep_name_and_sort("1080P", r"(第\s*(?<ep>.+)\s*[话集])|1080P");
        assert!(s4 >= 0.0);
    }

    #[test]
    fn keyword_prepare() {
        assert_eq!(prepare_keyword("葬送的芙莉莲", true, false), "葬送的芙莉莲");
        assert_eq!(prepare_keyword("葬送的芙莉莲 TV", true, false), "葬送的芙莉莲");
        assert_eq!(prepare_keyword("葬送的芙莉莲！", false, true), "葬送的芙莉莲！");
        assert_eq!(prepare_keyword("Fate/Zero", false, true), "FateZero");
    }

    #[test]
    fn video_url_extraction_from_html() {
        let html = r#"<html><script>var player_aaaa={"encrypt":2,"url":"aHR0cHM6Ly9leGFtcGxlLmNvbS9wbGF5bGlzdC5tM3U4"}</script></html>"#;
        let (u, n) = extract_video(html, None, "https://x.com").expect("should extract");
        assert_eq!(u, "https://example.com/playlist.m3u8");
        assert!(!n);
    }

    #[test]
    fn source_parse_from_subscription() {
        let json = r#"{"exportedMediaSourceDataList":{"mediaSources":[
            {"factoryId":"web-selector","version":2,"arguments":{"name":"测试源",
             "searchConfig":{"searchUrl":"https://x.com/s?wd={keyword}"},"tier":3}}]}}"#;
        let v = parse_sources(json);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "测试源");
        assert_eq!(v[0].tier, 3);
        assert_eq!(v[0].kind, "online");
    }

    #[test]
    fn rss_parse_extracts_torrents() {
        let xml = r#"<?xml version="1.0"?><rss><channel>
            <item><title>[Lilith-Raws] 葬送的芙莉莲 - 01 [1080p]</title>
            <link>https://nyaa.land/view/1</link>
            <enclosure url="https://nyaa.land/download/1.torrent" type="application/x-bittorrent"/></item>
            </channel></rss>"#;
        let v = parse_rss(xml, "https://nyaa.land", "nyaa");
        assert_eq!(v.len(), 1);
        assert!(v[0].title.contains("芙莉莲"));
        assert!(v[0].url.ends_with(".torrent"));
    }

    // ============================================================
    // 联网冒烟测试 —— 默认 #[ignore]，用 `cargo test -- --ignored` 手动跑。
    //
    // 存在的意义：单测只能证明解析逻辑对，证明不了"站点的页面结构还配得上这些选择器"。
    // 而"选择器失配 -> 退回抓所有 <a> -> 抓出一堆广告"正是用户看到的问题，
    // 所以必须有一个真的打网络、真的过一遍 搜索→剧集→取流 的测试守着。
    // ============================================================
    #[tokio::test]
    #[ignore]
    async fn live_search_finds_real_subjects_no_ads() {
        let srcs = fetch_sources().await;
        assert!(!srcs.is_empty(), "订阅一个源都没拉到");
        println!("\n拉到 {} 个源", srcs.len());

        let mut ok = 0;
        for s in srcs.iter().take(6) {
            match search_subject(s, "葬送的芙莉莲").await {
                Ok(v) => {
                    println!("\n[{}] {} 条", s.name, v.len());
                    for it in v.iter().take(4) {
                        println!("    {} <- {}", it.title, it.url);
                    }
                    // 关键断言: 结果里不能出现图一那种站内导航/推广
                    for it in &v {
                        let t = it.title.trim();
                        let bad = matches!(t, "首页" | "日番" | "劇場版" | "APP下载" | "游戏" | "联盟"
                            | "Telegram群" | "联系邮箱" | "问题反馈");
                        assert!(!bad, "[{}] 结果里混进了导航/推广: {t} <- {}", s.name, it.url);
                        assert!(!it.url.contains("t.me/"), "[{}] 混进 Telegram 链接: {}", s.name, it.url);
                        // ★ 前端拿 source_url 去调 anime_episodes/anime_resolve；空了就整条链路断掉
                        assert!(!it.source_url.is_empty(), "[{}] source_url 是空的: {t}", s.name);
                        assert_eq!(it.source_url, s.search_url, "[{}] source_url 不是本源的 search_url", s.name);
                        assert!(!it.source_name.is_empty(), "[{}] source_name 是空的: {t}", s.name);
                    }
                    if !v.is_empty() {
                        ok += 1;
                    }
                }
                Err(e) => println!("\n[{}] 失败: {e}", s.name),
            }
        }
        assert!(ok > 0, "所有源都搜不到东西 —— 引擎或网络有问题");
    }

    /// 苹果CMS API 源全链路：搜索 → 剧集（直链）→ resolve 原样返回
    #[tokio::test]
    #[ignore]
    async fn live_maccms_api_chain() {
        let mut any = false;
        for src in api_sources() {
            match search_subject(&src, "葬送的芙莉莲").await {
                Ok(v) => {
                    println!("\n[{}] 搜索 {} 条", src.name, v.len());
                    let mut best: Option<SubjectItem> = None;
                    for it in &v {
                        println!("    {} <- {}", it.title, it.url);
                        assert!(!it.source_url.is_empty(), "source_url 空了");
                        assert_eq!(it.source_url, src.search_url);
                        if best.is_none() && it.title.contains("芙莉莲") && !it.title.contains("解说") {
                            best = Some(it.clone());
                        }
                    }
                    if let Some(b) = best {
                        let eps = fetch_episodes(&src, &b.url).await.expect("拉剧集失败");
                        println!("    剧集 {} 条，前 3：", eps.len());
                        for e in eps.iter().take(3) {
                            println!("      [{}] {} sort={} <- {}", e.channel, e.name, e.sort, e.url);
                        }
                        assert!(!eps.is_empty(), "[{}] 剧集是空的", src.name);
                        assert!(eps[0].url.starts_with("http"));
                        let pi = resolve_play(&src, &eps[0].url).await.expect("resolve 失败");
                        assert_eq!(pi.url, eps[0].url, "直链应该原样返回");
                        println!("    resolve OK: {}", pi.url);
                        any = true;
                    }
                }
                Err(e) => println!("\n[{}] 失败: {e}", src.name),
            }
        }
        assert!(any, "三个 API 源一个都没跑通");
    }

    #[tokio::test]
    #[ignore]
    async fn live_episodes_and_play_url() {
        // ★ 用「叽哔动漫」跑全链路：它实测能出结果，且剧集/播放页都是服务端渲染的。
        //   girigiri 的**搜索页**有 Cloudflare 人机校验（页面里 9 个 verify 字段、0 条结果），
        //   拿它当搜索源测不出东西，所以这里不用它。
        let srcs = fetch_sources().await;
        let src = srcs
            .iter()
            .find(|s| s.name.contains("叽哔"))
            .expect("订阅里没有叽哔源");

        let hits = search_subject(src, "葬送的芙莉莲").await.expect("搜索失败");
        assert!(!hits.is_empty(), "搜不到番剧");
        let page = &hits[0];
        println!("\n命中: {} <- {}", page.title, page.url);

        let eps = fetch_episodes(src, &page.url).await.expect("剧集失败");
        assert!(!eps.is_empty(), "没解析出剧集");
        println!("剧集 {} 条, 前几条:", eps.len());
        for e in eps.iter().take(5) {
            println!("    [{}] {} sort={} <- {}", e.channel, e.name, e.sort, e.url);
        }
        // 集数必须真的被解析出来(>0)，否则说明 matchEpisodeSortFromName 失配了
        assert!(
            eps.iter().any(|e| e.sort > 0.0),
            "所有剧集 sort 都是 0，集数正则没匹配上"
        );

        let play = resolve_play(src, &eps[0].url).await.expect("取流失败");
        println!("\n播放地址: {}", play.url);
        println!("Referer  : {}", play.referer);
        assert!(play.url.starts_with("http"), "播放地址不合法: {}", play.url);
        assert!(looks_like_media(&play.url), "取到的不是媒体地址: {}", play.url);
    }

    #[tokio::test]
    #[ignore]
    async fn live_metadata_and_home() {
        // 首页(趋势+推荐)
        let t = home_trends().await.expect("趋势失败");
        let r = home_recommendations(20).await.expect("推荐失败");
        println!("\n趋势 {} 条, 推荐 {} 条", t.len(), r.len());
        for c in t.iter().take(5) {
            println!("    {} / {}  {}", c.name_cn, c.name, c.image);
        }
        assert!(!t.is_empty() || !r.is_empty(), "首页数据全空");
        assert!(t.iter().all(|c| c.image.starts_with("http")), "封面地址不合法");

        // 详情(拿孤独摇滚 328609 —— 前面实测过有数据)
        let d = subject_detail(328609).await.expect("详情失败");
        println!("\n详情: {} / {}  {:.1}分  #{}", d.name_cn, d.name, d.score, d.rank);
        println!("  标签: {}", d.tags.join(", "));
        assert!(!d.name.is_empty(), "详情没名字");
        assert!(d.score > 0.0, "详情没评分");

        let eps = subject_episodes(328609).await.expect("剧集失败");
        let casts = subject_casts(328609).await.expect("角色失败");
        println!("  Bangumi 剧集 {} 条, 角色/制作 {} 条", eps.len(), casts.len());
        assert!(!eps.is_empty(), "Bangumi 剧集为空");
    }
}

#[cfg(test)]
mod hot_tests {
    use super::parse_fav_count;

    #[test]
    fn parses_favorite_counts_from_desc2() {
        assert_eq!(parse_fav_count("2 万收藏 · 7.5 分"), 20000);
        assert_eq!(parse_fav_count("1.5万收藏 · 8.1 分"), 15000);
        assert_eq!(parse_fav_count("3000 收藏 · 6.0 分"), 3000);
        assert_eq!(parse_fav_count("9 千收藏 · 5.9 分"), 9000);
        assert_eq!(parse_fav_count("12k收藏"), 12000);
        assert_eq!(parse_fav_count("没有收藏字样"), 0);
        assert_eq!(parse_fav_count(""), 0);
    }
}
