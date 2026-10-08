//! Kazumi 规则源。
//!
//! 数据来自 [Predidit/Kazumi](https://github.com/Predidit/Kazumi) 与
//! [Predidit/KazumiRules](https://github.com/Predidit/KazumiRules)（两者都是 **MIT**，
//! 规则文件原样内置在 `src-tauri/kazumi_rules/`，`LICENSE` 一并保留）。
//!
//! 规则文件 = JSON + XPath，字段见 [`KazumiRule`]。这里做两件事：
//!
//! 1. **内置** 16 条活跃规则（`index.json` 里列出的那些）。
//!    ★ 为什么必须内置而不是运行时拉：这台机器的网络**连不上 github.com /
//!      raw.githubusercontent.com**（只有 codeload 和几个镜像通），运行时去拉规则
//!      会直接失败。内置 = 离线可用。
//! 2. 把规则翻译成 anime 引擎的 [`crate::anime::Source`]（`factory = "kazumi"`），
//!    由 anime 引擎调用 [`search`] / [`chapters`]。
//!
//! ★ 86 条规则里 70 条带 `deprecated: true`，所以只内置 index.json 里那 16 条活跃的。

use serde::{Deserialize, Serialize};

use crate::anime::{absolutize, base_origin, ep_name_and_sort, get_text_retry, html_unescape, Source};
use crate::anime::{EpisodeItem, SubjectItem};
use crate::xpath_lite;

/// 一条 Kazumi 规则（字段名与规则文件一一对应）
///
/// ★ `rename_all = "camelCase"` 覆盖不了所有键：规则里写的是 **`searchURL` / `baseURL`**
///   （URL 三个字母全大写），camelCase 会推成 `searchUrl` / `baseUrl`，字段静默变成空串
///   —— 表现是"16 条规则全都提示没配 searchURL"。这两个必须显式 rename。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct KazumiRule {
    pub api: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub version: String,
    pub muli_sources: bool,
    pub use_webview: bool,
    pub use_native_player: bool,
    pub use_post: bool,
    pub use_legacy_parser: bool,
    pub ad_blocker: bool,
    pub user_agent: String,
    #[serde(rename = "baseURL")]
    pub base_url: String,
    pub referer: String,
    pub deprecated: bool,
    pub search_mode: String,
    pub chapter_mode: String,
    // 搜索页
    #[serde(rename = "searchURL")]
    pub search_url: String,
    pub search_list: String,
    pub search_name: String,
    pub search_result: String,
    // 剧集页
    pub chapter_roads: String,
    pub chapter_result: String,
    // JSON API 模式
    pub search_api_config: serde_json::Value,
    pub chapter_api_config: serde_json::Value,
    pub anti_crawler_config: serde_json::Value,
}

impl KazumiRule {
    /// 规则里 `@keyword` 是关键词占位符（不是 `{keyword}`）
    fn search_request_url(&self, keyword: &str) -> String {
        self.search_url
            .replace("@keyword", &urlencoding::encode(keyword))
    }
}

/// 内置的 16 条活跃规则（`include_str!` 进二进制，离线可用）
pub fn builtin_rules() -> Vec<KazumiRule> {
    const RAW: &[&str] = &[
        include_str!("../kazumi_rules/7sefun.json"),
        include_str!("../kazumi_rules/AGE.json"),
        include_str!("../kazumi_rules/DM84.json"),
        include_str!("../kazumi_rules/MXdm.json"),
        include_str!("../kazumi_rules/aafun.json"),
        include_str!("../kazumi_rules/akianime.json"),
        include_str!("../kazumi_rules/baimao.json"),
        include_str!("../kazumi_rules/dalvdm.json"),
        include_str!("../kazumi_rules/ezdmw.json"),
        include_str!("../kazumi_rules/giriGiriLove.json"),
        include_str!("../kazumi_rules/mgnacg.json"),
        include_str!("../kazumi_rules/moonci.json"),
        include_str!("../kazumi_rules/mutefun.json"),
        include_str!("../kazumi_rules/sorani.json"),
        include_str!("../kazumi_rules/xfdmneo.json"),
        include_str!("../kazumi_rules/xfdmnext.json"),
    ];
    RAW.iter()
        .filter_map(|s| serde_json::from_str::<KazumiRule>(s).ok())
        .filter(|r| !r.deprecated && !r.name.is_empty())
        .collect()
}

/// 2026-10-07 实测**全链路**（搜索 → 剧集 → 解析出播放地址）都通的规则。
///
/// ★ 只列了 2 条，这是实测结果不是保守：
///   - 7sefun / aafun / baimao / giriGiriLove / mgnacg / mutefun / xfdmneo：
///     服务端返回的搜索页里根本没有结果（要 JS 渲染，正是 Kazumi 里 `useWebview` 的含义）
///   - ezdmw：能搜到，但剧集选择器失配
///   - akianime：能搜到、有 84 集，但播放地址是**自定义加密**的 `Doki-<hex>`，
///     不是 macCMS 标准编码 → 解析不出来
///   - DM84 / xfdmnext：剧集能拿到，播放页解析不出地址
///   - dalvdm / sorani：这两个站在当前网络下连不上
///
/// ★ 只是**排序**用（排前面优先被搜到），不做排除 —— 站点会变，哪天修好了
///   把名字挪进来就行。
pub const VERIFIED: &[&str] = &["moonci", "MXdm"];

/// 规则 → 引擎用的 Source（规则本体塞进 `cfg`，`search_url` 只作展示/标识）
pub fn rule_sources() -> Vec<Source> {
    let mut list: Vec<Source> = builtin_rules()
        .into_iter()
        .filter_map(|r| {
            let cfg = serde_json::to_value(&r).ok()?;
            let verified = VERIFIED.contains(&r.name.as_str());
            Some(Source {
                name: r.name.clone(),
                description: format!(
                    "Kazumi 规则 v{}{}",
                    r.version,
                    if verified { "" } else { "（可能需要浏览器渲染）" }
                ),
                icon: String::new(),
                factory: "kazumi".into(),
                search_url: r.search_url.clone(),
                kind: "online".into(),
                tier: if verified { 5 } else { 50 },
                cfg,
            })
        })
        .collect();
    // 实测可用的排前面
    list.sort_by_key(|s| s.tier);
    list
}

/// 从 cfg 里取回规则
pub fn rule_of(src: &Source) -> Result<KazumiRule, String> {
    serde_json::from_value(src.cfg.clone())
        .map_err(|e| format!("规则解析失败({}): {e}", src.name))
}

/// 搜索：GET searchURL → XPath 选条目 → 每条的 (名称, 详情链接)
pub async fn search(rule: &KazumiRule, keyword: &str) -> Result<Vec<SubjectItem>, String> {
    if rule.search_mode == "api" {
        return search_api(rule, keyword).await;
    }
    let url = rule.search_request_url(keyword);
    if url.trim().is_empty() {
        return Err(format!("{} 没配 searchURL", rule.name));
    }
    let refr = referer_of(rule);
    let (body, final_url) = get_text_retry(&url, Some(&refr))
        .await
        .map_err(|e| format!("连接 {} 失败: {e}", rule.name))?;
    let base = base_origin(&final_url);
    let doc = scraper::Html::parse_document(&body);
    let root = doc.root_element();

    let items = xpath_lite::eval(root, &rule.search_list);
    let mut out = Vec::new();
    for it in items {
        // 名称与链接都**相对条目**求值（Kazumi 的规则就是这么写的）
        let name = xpath_lite::eval(it, &rule.search_name)
            .first()
            .map(|e| html_unescape(&xpath_lite::text_of(e)))
            .unwrap_or_default();
        let href = xpath_lite::eval(it, &rule.search_result)
            .first()
            .map(|e| xpath_lite::attr_of(e, "href"))
            .unwrap_or_default();
        if name.is_empty() || href.is_empty() {
            continue;
        }
        let detail = absolutize(&base, &href);
        if detail.is_empty() {
            continue;
        }
        out.push(SubjectItem {
            title: name,
            url: detail,
            image: String::new(),
            source: String::new(),
            source_url: String::new(),
            source_name: String::new(),
        });
    }
    Ok(out)
}

/// JSON API 模式的搜索（sorani / xfdmnext 这种）
async fn search_api(rule: &KazumiRule, keyword: &str) -> Result<Vec<SubjectItem>, String> {
    let cfg = &rule.search_api_config;
    let req = cfg
        .get("request")
        .ok_or_else(|| format!("{} 的 searchApiConfig 没有 request", rule.name))?;
    let url = req.get("url").and_then(|x| x.as_str()).unwrap_or("");
    if url.is_empty() {
        return Err(format!("{} 的 API 搜索没配 url", rule.name));
    }
    let c = crate::anime::client();
    let mut r = if req.get("method").and_then(|x| x.as_str()) == Some("POST") {
        let body = json_with_placeholder(req.get("body"), keyword);
        let mut b = c.post(url);
        if let Some(h) = req.get("headers").and_then(|x| x.as_object()) {
            for (k, v) in h {
                if let Some(vs) = v.as_str() {
                    b = b.header(k, vs);
                }
            }
        }
        b.json(&body)
    } else {
        let mut b = c.get(url);
        if let Some(q) = req.get("query").and_then(|x| x.as_object()) {
            let pairs: Vec<(String, String)> = q
                .iter()
                .map(|(k, v)| {
                    let vs = match v {
                        serde_json::Value::String(s) => s.replace("@keyword", keyword),
                        other => other.to_string(),
                    };
                    (k.clone(), vs)
                })
                .collect();
            b = b.query(&pairs);
        }
        b
    };
    r = r.header("User-Agent", ua_of(rule));
    let resp = r.send().await.map_err(|e| format!("{} API 请求失败: {e}", rule.name))?;
    let txt = resp.text().await.map_err(|e| e.to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(&txt).map_err(|e| format!("{} API 返回的不是 JSON: {e}", rule.name))?;

    let list = json_path(cfg.get("listPath").and_then(|x| x.as_str()).unwrap_or("$[*]"), &v);
    let name_path = cfg.get("namePath").and_then(|x| x.as_str()).unwrap_or("$.title");
    let src_path = cfg.get("sourcePath").and_then(|x| x.as_str()).unwrap_or("$.id");
    let mut out = Vec::new();
    for it in list {
        let title = json_first_str(name_path, &it);
        let id = json_first_str(src_path, &it);
        if title.is_empty() || id.is_empty() {
            continue;
        }
        out.push(SubjectItem {
            title,
            // 详情地址用 `@source` 占位，chapters() 会拿它去请求 chapterApiConfig
            url: format!("api:{}", id),
            image: String::new(),
            source: String::new(),
            source_url: String::new(),
            source_name: String::new(),
        });
    }
    Ok(out)
}

/// 剧集：打开详情页 → XPath 选「线路」→ 每条线路里选剧集 `<a>`
pub async fn chapters(rule: &KazumiRule, page_url: &str) -> Result<Vec<EpisodeItem>, String> {
    if rule.chapter_mode == "api" || page_url.starts_with("api:") {
        return chapters_api(rule, page_url).await;
    }
    let refr = referer_of(rule);
    let (body, final_url) = get_text_retry(page_url, Some(&refr))
        .await
        .map_err(|e| format!("打开 {} 详情页失败: {e}", rule.name))?;
    let base = base_origin(&final_url);
    let doc = scraper::Html::parse_document(&body);
    let root = doc.root_element();

    let roads = xpath_lite::eval(root, &rule.chapter_roads);
    let mut out: Vec<EpisodeItem> = Vec::new();
    // 一条线路都没有时，退回"整个页面按 chapterResult 选"（有些规则只有单线路）
    let sources: Vec<scraper::ElementRef> = if roads.is_empty() { vec![root] } else { roads };
    for (ri, road) in sources.iter().enumerate() {
        let ch = format!("线路{}", ri + 1);
        for a in xpath_lite::eval(*road, &rule.chapter_result) {
            let href = xpath_lite::attr_of(&a, "href");
            if href.is_empty() {
                continue;
            }
            let url = absolutize(&base, &href);
            if url.is_empty() {
                continue;
            }
            let raw = {
                let t = xpath_lite::text_of(&a);
                if t.is_empty() {
                    html_unescape(&xpath_lite::attr_of(&a, "title"))
                } else {
                    html_unescape(&t)
                }
            };
            if raw.is_empty() || raw.chars().count() > 60 {
                continue;
            }
            let (name, sort) = ep_name_and_sort(&raw, r"第\s*(?<ep>\d+)");
            out.push(EpisodeItem {
                channel: ch.clone(),
                name,
                sort,
                url,
            });
        }
    }
    // 去重 + 按 (线路, 集数) 排序
    let mut seen = std::collections::HashSet::new();
    out.retain(|e| !e.url.is_empty() && seen.insert(e.url.clone()));
    out.sort_by(|a, b| {
        a.channel
            .cmp(&b.channel)
            .then(a.sort.partial_cmp(&b.sort).unwrap_or(std::cmp::Ordering::Equal))
    });
    Ok(out)
}

/// API 模式的剧集（chapterApiConfig）
async fn chapters_api(rule: &KazumiRule, page_url: &str) -> Result<Vec<EpisodeItem>, String> {
    let cfg = &rule.chapter_api_config;
    let id = page_url.trim_start_matches("api:");
    let req = cfg
        .get("request")
        .ok_or_else(|| format!("{} 的 chapterApiConfig 没有 request", rule.name))?;
    let url = req
        .get("url")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .replace("@source", id);
    if url.is_empty() {
        return Err(format!("{} 的 API 剧集没配 url", rule.name));
    }
    let c = crate::anime::client();
    let mut r = if req.get("method").and_then(|x| x.as_str()) == Some("POST") {
        let body = json_with_placeholder(req.get("body"), id);
        let mut b = c.post(&url);
        if let Some(h) = req.get("headers").and_then(|x| x.as_object()) {
            for (k, v) in h {
                if let Some(vs) = v.as_str() {
                    b = b.header(k, vs);
                }
            }
        }
        b.json(&body)
    } else {
        c.get(&url)
    };
    r = r.header("User-Agent", ua_of(rule));
    let resp = r.send().await.map_err(|e| format!("{} API 请求失败: {e}", rule.name))?;
    let txt = resp.text().await.map_err(|e| e.to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(&txt).map_err(|e| format!("{} API 返回的不是 JSON: {e}", rule.name))?;

    let name_path = cfg.get("episodeNamePath").and_then(|x| x.as_str()).unwrap_or("$.name");
    let order_path = cfg.get("episodeUrlPath").and_then(|x| x.as_str()).unwrap_or("$.order");
    // 播放页 URL 模板：占位符是 `@source` / `@episodeUrl`（**不是** `@episode`！
    // 之前按 `@episode` 替换会把 `@episodeUrl` 弄成 `3252Url`）
    let page_tpl = cfg
        .get("episodePage")
        .and_then(|x| x.get("url"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    // 附加 query（有些规则在这里挂参数）
    let extra_q: Vec<(String, String)> = cfg
        .get("episodePage")
        .and_then(|x| x.get("query"))
        .and_then(|x| x.as_object())
        .map(|o| {
            o.iter()
                .map(|(k, val)| {
                    (
                        k.clone(),
                        match val {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    // ★ 嵌套结构：`roadsPath` 先选线路，`episodesPath` 再**相对每条线路**选剧集。
    //   （xfdmnext 是 `$.sources[*]` → `$.episodes[*]`；sorani 是 `$.data` → `$.episodes[*]`）
    //   之前把 episodesPath 直接套在根响应上，所以永远是 0 条。
    let roads_path = cfg.get("roadsPath").and_then(|x| x.as_str()).unwrap_or("");
    let road_name_path = cfg.get("roadNamePath").and_then(|x| x.as_str()).unwrap_or("");
    let episodes_path = cfg
        .get("episodesPath")
        .and_then(|x| x.as_str())
        .unwrap_or("$.episodes[*]");

    let roads: Vec<serde_json::Value> = if roads_path.trim().is_empty() {
        vec![v.clone()]
    } else {
        json_path(roads_path, &v)
    };

    let mut out = Vec::new();
    for (ri, road) in roads.iter().enumerate() {
        let ch = if road_name_path.trim().is_empty() {
            String::new()
        } else {
            let n = json_first_str(road_name_path, road);
            if n.is_empty() { format!("线路{}", ri + 1) } else { n }
        };
        let eps = json_path(episodes_path, road);
        for (i, e) in eps.iter().enumerate() {
            let raw = json_first_str(name_path, e);
            let ord = {
                let s = json_first_str(order_path, e);
                if s.is_empty() { (i + 1).to_string() } else { s }
            };
            let mut url = page_tpl
                .replace("@episodeUrl", &ord)
                .replace("@source", id)
                .replace("@episode", &ord);
            if url.is_empty() {
                continue;
            }
            if !extra_q.is_empty() {
                let qs: Vec<String> = extra_q
                    .iter()
                    .map(|(k, val)| {
                        format!(
                            "{}={}",
                            k,
                            urlencoding::encode(&val.replace("@source", id).replace("@episodeUrl", &ord))
                        )
                    })
                    .collect();
                url.push(if url.contains('?') { '&' } else { '?' });
                url.push_str(&qs.join("&"));
            }
            let (name, sort) = ep_name_and_sort(&raw, r"第\s*(?<ep>\d+)");
            out.push(EpisodeItem {
                channel: ch.clone(),
                name,
                sort,
                url,
            });
        }
    }
    Ok(out)
}

// ============================================================
// 小工具
// ============================================================

fn referer_of(rule: &KazumiRule) -> String {
    if !rule.referer.trim().is_empty() {
        rule.referer.clone()
    } else {
        rule.base_url.clone()
    }
}

fn ua_of(rule: &KazumiRule) -> String {
    if rule.user_agent.trim().is_empty() {
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36".to_string()
    } else {
        rule.user_agent.clone()
    }
}

/// 把 body 模板里的 `@keyword` / `@source` 换掉（只处理字符串值）
fn json_with_placeholder(v: Option<&serde_json::Value>, val: &str) -> serde_json::Value {
    fn walk(v: &serde_json::Value, val: &str) -> serde_json::Value {
        match v {
            serde_json::Value::String(s) => serde_json::Value::String(
                s.replace("@keyword", val).replace("@source", val),
            ),
            serde_json::Value::Object(o) => serde_json::Value::Object(
                o.iter().map(|(k, x)| (k.clone(), walk(x, val))).collect(),
            ),
            serde_json::Value::Array(a) => {
                serde_json::Value::Array(a.iter().map(|x| walk(x, val)).collect())
            }
            other => other.clone(),
        }
    }
    match v {
        Some(x) => walk(x, val),
        None => serde_json::json!({}),
    }
}

/// 取 JSONPath 结果的第一个值并转成字符串（字符串原样，数字等用 JSON 表示）
///
/// ★ 不能写成 `.and_then(|x| x.as_str())` —— 那样返回的 `&str` 指向被 move 进闭包的
///   临时 `Value`，编译器直接报 E0515。
fn json_first_str(path: &str, v: &serde_json::Value) -> String {
    json_path(path, v)
        .into_iter()
        .next()
        .map(|x| match x {
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        })
        .unwrap_or_default()
}

/// 极简 JSONPath：只支持规则里用到的那几种
///
/// ```text
/// $[*]           数组每个元素
/// $.data.records[*]
/// $.title
/// $.episodes[*]
/// ```
/// 不支持过滤、递归、切片。
pub fn json_path(path: &str, v: &serde_json::Value) -> Vec<serde_json::Value> {
    let p = path.trim().trim_start_matches('$');
    let mut cur = vec![v.clone()];
    for seg in p.split('.').filter(|s| !s.is_empty()) {
        let wildcard = seg.ends_with("[*]");
        let key = seg.trim_end_matches("[*]");
        let mut next = Vec::new();
        for node in &cur {
            if wildcard {
                // `records[*]` 或裸 `[*]`
                let target = if key.is_empty() { Some(node) } else { node.get(key) };
                if let Some(serde_json::Value::Array(a)) = target {
                    next.extend(a.iter().cloned());
                }
            } else {
                // 段名本身可能是 `arr[*]` 之外的普通字段
                if let Some(x) = node.get(key) {
                    next.push(x.clone());
                }
            }
        }
        cur = next;
    }
    cur
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_rules_load_and_are_active() {
        let rs = builtin_rules();
        assert!(rs.len() >= 14, "内置规则太少: {}", rs.len());
        for r in &rs {
            assert!(!r.deprecated, "{} 是 deprecated，不该内置", r.name);
            assert!(!r.name.is_empty());
        }
        // 必须覆盖到我们关心的几个
        let names: Vec<&str> = rs.iter().map(|r| r.name.as_str()).collect();
        for want in ["AGE", "akianime", "giriGiriLove", "xfdmnext", "7sefun"] {
            assert!(names.contains(&want), "缺少规则 {want}");
        }
    }

    #[test]
    fn rule_sources_carry_the_rule_in_cfg() {
        let srcs = rule_sources();
        assert!(!srcs.is_empty());
        for s in &srcs {
            assert_eq!(s.factory, "kazumi");
            let r = rule_of(s).expect("cfg 应该能还原成规则");
            assert_eq!(r.name, s.name);
        }
    }

    #[test]
    fn keyword_placeholder_is_at_keyword() {
        let r = KazumiRule {
            name: "t".into(),
            search_url: "https://x/s?wd=@keyword".into(),
            ..Default::default()
        };
        let u = r.search_request_url("葬送的芙莉莲");
        assert!(u.starts_with("https://x/s?wd="), "{u}");
        assert!(!u.contains("@keyword"));
        assert!(u.contains("%E8%91%AC"), "关键词要 URL 编码: {u}");
    }

    #[test]
    fn json_path_handles_the_rule_shapes() {
        let v = serde_json::json!({
            "data": { "records": [ {"title":"A","id":1}, {"title":"B","id":2} ] }
        });
        let got = json_path("$.data.records[*]", &v);
        assert_eq!(got.len(), 2);
        assert_eq!(json_path("$.title", &got[0])[0].as_str().unwrap(), "A");
        assert_eq!(json_path("$.id", &got[1])[0].as_i64().unwrap(), 2);

        let arr = serde_json::json!([{"title":"X"}]);
        assert_eq!(json_path("$[*]", &arr).len(), 1);
        assert_eq!(json_path("$.nope", &v).len(), 0);
    }

    #[test]
    fn placeholder_substitution_is_recursive() {
        let body = serde_json::json!({"search_term":"@keyword","nested":{"p_id":"@source"},"n":1});
        let out = json_with_placeholder(Some(&body), "abc");
        assert_eq!(out["search_term"], "abc");
        assert_eq!(out["nested"]["p_id"], "abc");
        assert_eq!(out["n"], 1);
    }

    /// 拿真实站点跑一遍全部内置规则：搜索 → 取剧集。
    ///
    /// 单测只能证明 XPath 引擎对，证明不了"这些站点的页面结构还配得上规则里的 XPath"。
    #[tokio::test]
    #[ignore]
    async fn live_all_kazumi_rules() {
        let rules = builtin_rules();
        let mut ok = 0;
        let mut search_ok = 0;
        for r in &rules {
            match search(r, "葬送的芙莉莲").await {
                Ok(v) if !v.is_empty() => {
                    search_ok += 1;
                    let first = &v[0];
                    println!(
                        "[{}] 搜索 {} 条
    首条: {} <- {}",
                        r.name, v.len(), first.title, first.url
                    );
                    match chapters(r, &first.url).await {
                        Ok(eps) if !eps.is_empty() => {
                            println!(
                                "    剧集 {} 条
    首集: [{}] {} sort={} <- {}",
                                eps.len(), eps[0].channel, eps[0].name, eps[0].sort, eps[0].url
                            );
                            // ★ 光有剧集不算通 —— 还得能从播放页解析出真地址，
                            //   否则前端一点就是"没解析出播放地址"（akianime 就是这种：
                            //   它的 url 字段是自定义加密的 `Doki-<hex>`，不是 macCMS 标准编码）。
                            let rule = r;
                            match crate::anime::resolve_play(
                                &Source {
                                    name: rule.name.clone(),
                                    factory: "kazumi".into(),
                                    search_url: rule.search_url.clone(),
                                    ..Default::default()
                                },
                                &eps[0].url,
                            )
                            .await
                            {
                                Ok(pi) if !pi.url.is_empty() => {
                                    println!("    可播放: {}", &pi.url[..pi.url.len().min(110)]);
                                    ok += 1;
                                }
                                Ok(_) => println!("    播放地址为空"),
                                Err(e) => println!("    解析播放地址失败: {e}"),
                            }
                        }
                        Ok(_) => println!("    剧集 0 条（选择器可能失配）"),
                        Err(e) => println!("    剧集失败: {e}"),
                    }
                }
                Ok(_) => println!("[{}] 搜索 0 条", r.name),
                Err(e) => println!("[{}] 搜索失败: {e}", r.name),
            }
        }
        println!("
搜索有结果: {search_ok}/{}   全链路可用: {ok}/{}", rules.len(), rules.len());
        // ★ 门槛是实测值：纯 HTTP 下大多数 Kazumi 规则需要 webview 才能用，
        //   2 条是可接受的下限；掉到 1 条说明规则或引擎坏了。
        assert!(ok >= 2, "全链路可用的 Kazumi 规则太少: {ok}");
    }
}
