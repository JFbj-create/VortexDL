//! 浏览器相关：打开外部链接 + **用真浏览器取页面**。

pub fn open_external(url: &str) -> Result<(), String> {
    use std::process::Command;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        Command::new("cmd")
            .args(["/C", "start", "", url])
            .creation_flags(0x08000000u32)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(not(windows))]
    {
        let _ = url;
    }
    Ok(())
}

// ============================================================
// 用**真浏览器**取页面 —— 给那些"下载链接要点了按钮才出来"的站点用。
//
// ## 为什么需要它（2026-10-07 实测结论）
//
// `galgamex.net` 这类站不能纯 HTTP 抓：
//
// | 层 | 实测结果 |
// |---|---|
// | 列表页 `/games` | 服务端渲染，curl 能拿到 80 个 `/game/<id>` 链接 |
// | 详情页 | 服务端渲染、内嵌 `gameId`，但**没有**任何 `.zip/.rar/.7z` 链接 |
// | 下载链接 | 在「资源下载」标签页里，**点击后**才由 JS 加载 |
// | 候选 `/api/...` 接口 | 全部 404（站点用 Next.js server action，路径是哈希 id） |
// | 无头 Edge `--dump-dom` | JS 确实跑了，但**仍然没有**下载链接 —— 必须真的触发那次点击 |
//
// 所以这里启动浏览器、**模拟点击**、再取渲染后的 DOM。
//
// ## 不额外下载 Chromium
//
// 启动的是**系统已装的 Edge/Chrome**（Win10/11 自带 Edge），
// 不用 chromiumoxide 自带的 BrowserFetcher 去下几百 MB。
// ============================================================

use chromiumoxide::browser::{Browser, BrowserConfig};
use futures::StreamExt;
use std::path::PathBuf;
use std::time::Duration;

/// 系统上可用的浏览器可执行文件（Edge 优先 —— Windows 自带）
pub fn find_browser() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = vec![
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe".into(),
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe".into(),
        r"C:\Program Files\Google\Chrome\Application\chrome.exe".into(),
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe".into(),
    ];
    // 环境变量兜底（自定义安装位置）
    for k in ["PROGRAMFILES(X86)", "PROGRAMFILES", "LOCALAPPDATA"] {
        if let Ok(base) = std::env::var(k) {
            cands.push(PathBuf::from(&base).join(r"Microsoft\Edge\Application\msedge.exe"));
            cands.push(PathBuf::from(&base).join(r"Google\Chrome\Application\chrome.exe"));
        }
    }
    cands.into_iter().find(|p| p.exists())
}

#[derive(Debug, Clone, Default)]
pub struct PageResult {
    /// 渲染后的完整 DOM
    pub html: String,
    /// 从 DOM 里抓到的所有 http(s) 链接（已去重）
    pub urls: Vec<String>,
    /// 按文字点击是否命中
    pub clicked: bool,
    /// 页面自己发出的 fetch/XHR（含响应体）
    ///
    /// ★ 为什么需要它：不少站的下载地址**只存在于 JS 内存里**，DOM 里一个字都没有。
    ///   实测 galgamex：资源面板里「RAR 7.8 GB」是个**空 `<button>`**（无 href、
    ///   无 data-url），真链接是点了之后由 `/api/game/resource/<id>/download` 返回的，
    ///   所以从 HTML 里抓链接永远抓不到（这也是我第一版 0 直链的真因）。
    pub net: Vec<NetEntry>,
}

/// 页面发出的一次网络请求 + 响应体
#[derive(Debug, Clone, Default)]
pub struct NetEntry {
    pub kind: String,
    pub url: String,
    pub body: Option<String>,
    /// 请求方法 + 关键请求头 + 请求体（诊断 server action 用）
    pub req: Option<String>,
}

/// 注入网络记录钩子：包装 `fetch` / `XMLHttpRequest`，把 URL 和响应体记到
/// `window.__vxNet`。必须在目标请求发生**之前**注入。
pub(crate) const NET_HOOK_JS: &str = r#"(function () {
    if (window.__vxHooked) return true;
    window.__vxHooked = true;
    window.__vxNet = [];
    function rec(kind, url, body, req) {
        try {
            window.__vxNet.push({
                kind: kind,
                url: String(url == null ? '' : url),
                body: body == null ? null : String(body).slice(0, 400000),
                req: req == null ? null : String(req).slice(0, 20000)
            });
        } catch (e) {}
    }
    var of = window.fetch;
    if (of) {
        window.fetch = function (input, init) {
            var url = (typeof input === 'string') ? input : ((input && input.url) || '');
            var info = '';
            try {
                var m = (init && init.method) || (input && input.method) || 'GET';
                info = m;
                if (init && init.headers) {
                    try {
                        var hs = init.headers;
                        if (typeof hs.forEach === 'function' && !Array.isArray(hs)) {
                            hs.forEach(function (v, k) { info += ' | ' + k + ': ' + v; });
                        } else if (Array.isArray(hs)) {
                            hs.forEach(function (kv) { info += ' | ' + kv[0] + ': ' + kv[1]; });
                        } else {
                            for (var k in hs) { info += ' | ' + k + ': ' + hs[k]; }
                        }
                    } catch (e) {}
                }
                if (init && init.body != null) info += ' || BODY: ' + String(init.body).slice(0, 4000);
            } catch (e) {}
            var p = of.apply(this, arguments);
            try {
                p.then(function (resp) {
                    try {
                        resp.clone().text().then(function (t) { rec('fetch', url, t, info); })
                                          .catch(function () { rec('fetch', url, null, info); });
                    } catch (e) { rec('fetch', url, null, info); }
                });
            } catch (e) {}
            return p;
        };
    }
    var oo = XMLHttpRequest.prototype.open;
    var os = XMLHttpRequest.prototype.send;
    XMLHttpRequest.prototype.open = function (m, u) { this.__vxUrl = u; return oo.apply(this, arguments); };
    XMLHttpRequest.prototype.send = function () {
        var xhr = this;
        try {
            xhr.addEventListener('load', function () {
                var b = null;
                try {
                    if (xhr.responseType === '' || xhr.responseType === 'text') b = xhr.responseText;
                } catch (e) {}
                rec('xhr', xhr.__vxUrl, b);
            });
        } catch (e) {}
        return os.apply(this, arguments);
    };
    return true;
})()"#;

/// 读出钩子记录的请求
const NET_READ_JS: &str = r#"(function () {
    var out = [];
    var src = window.__vxNet || [];
    for (var i = 0; i < src.length; i++) {
        out.push({ kind: src[i].kind, url: src[i].url, body: src[i].body, req: src[i].req });
    }
    return out;
})()"#;

/// 打开页面，可选地**按可见文字点击一个按钮**，等一会儿再取渲染结果。
///
/// - `click_text`：例如 `"资源下载"`。会在 button/a/[role=button]/span 里找第一个
///   文本包含该字样的元素并 `.click()`。找不到返回 `clicked=false`（不报错，
///   有些页面本来就不需要点击）。
/// - `wait_ms`：导航后的等待，以及点击后的等待。
/// 一个**可复用**的浏览器会话。
///
/// ★ 为什么要有它：启动一次 Edge 要 1~2 秒，抓一页就 close 掉的话，
///   抓 6 个详情页要反复启动 7 次浏览器（实测 109 秒，其中大半是启动开销）。
///   会话把 browser 和 handler 长期持有，`fetch()` 只是开个新标签页。
pub struct BrowserSession {
    pub(crate) browser: Browser,
    handler_task: tokio::task::JoinHandle<()>,
    /// handler 里最后一条 CDP 错误（用于把"oneshot canceled"这种含糊报错说清楚）
    last_err: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl BrowserSession {
    pub async fn launch() -> Result<Self, String> {
        let exe = find_browser()
            .ok_or_else(|| "没找到 Edge/Chrome（这个功能需要系统装了浏览器）".to_string())?;

        // ★ 必须显式设视口：chromiumoxide 默认用 800x600 的模拟视口
        //   （`Viewport::default()`），页面按小窗布局，标签栏这类元素会掉到
        //   折叠线以下 —— 量出来的坐标在视口外，`page.click` 自然点空
        //   （实测：标签栏在 y=637，视口只有 600 高，点击毫无反应）。
        let config = BrowserConfig::builder()
            .chrome_executable(&exe)
            .no_sandbox()
            .new_headless_mode()
            .viewport(chromiumoxide::handler::viewport::Viewport {
                width: 1440,
                height: 1000,
                device_scale_factor: None,
                emulating_mobile: false,
                is_landscape: false,
                has_touch: false,
            })
            .arg("--disable-gpu")
            .arg("--disable-dev-shm-usage")
            .arg("--lang=zh-CN")
            .arg("--window-size=1440,1000")
            .build()
            .map_err(|e| format!("浏览器配置失败: {e}"))?;

        let (browser, mut handler) = Browser::launch(config)
            .await
            .map_err(|e| format!("启动浏览器失败: {e}"))?;

        // 必须有人持续 poll handler，否则 CDP 消息不会推进（chromiumoxide 的固定用法）。
        // 出错时记下来而不是静默 break —— 否则上层只会看到 "oneshot canceled"。
        let last_err: std::sync::Arc<std::sync::Mutex<Option<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let le = last_err.clone();
        let handler_task = tokio::spawn(async move {
            while let Some(ev) = handler.next().await {
                if let Err(e) = ev {
                    if let Ok(mut g) = le.lock() {
                        *g = Some(e.to_string());
                    }
                    break;
                }
            }
        });

        Ok(Self { browser, handler_task, last_err })
    }

    /// 打开一页（新标签页），可选按文字点击，取渲染后的 DOM。用完自动关掉标签页。
    ///
    /// ★ 签名写成 `impl Future + Send + 'a` 而不是 `async fn`：这是为了绕开
    ///   "implementation of `Send` is not general enough" 那个坑。
    ///   用 `async fn` 时编译器只证明了 `&'0 BrowserSession: Send`（某个具体生命周期），
    ///   证明不了 `for<'a> &'a BrowserSession: Send`，于是 Tauri 命令的 Send 检查失败。
    ///   显式写出来，编译器就会按 `for<'a>` 去校验。
    pub fn fetch<'a>(
        &'a self,
        url: &'a str,
        click_text: Option<&'a str>,
        wait_ms: u64,
    ) -> impl std::future::Future<Output = Result<PageResult, String>> + Send + 'a {
        async move {
            let page = self.browser.new_page(url).await.map_err(|e| {
                format!("打开页面失败: {e} (CDP: {})", self.cdp_detail())
            })?;

            let out = Self::drive(&page, url, click_text, wait_ms).await;
            let _ = page.close().await; // 别把标签页堆在后台
            out
        }
    }

    /// 一页的实际操作：等导航 → 可选点击 → 等渲染 → 取 DOM。
    fn drive<'a>(
        page: &'a chromiumoxide::Page,
        url: &'a str,
        click_text: Option<&'a str>,
        wait_ms: u64,
    ) -> impl std::future::Future<Output = Result<PageResult, String>> + Send + 'a {
        async move {
            // 导航失败不致命（有些站会一直挂着连接），继续等
            let _ = page.wait_for_navigation().await;
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;

            // ★ 必须在点击**之前**注入：要抓的请求正是点击触发的
            let _ = page.evaluate(NET_HOOK_JS).await;

            let mut clicked = false;
            if let Some(text) = click_text {
                clicked = Self::click_by_text(page, text).await;
                // 点击后等资源列表渲染出来
                tokio::time::sleep(Duration::from_millis(wait_ms)).await;
            }

            let html = page
                .content()
                .await
                .map_err(|e| format!("取页面内容失败: {e}"))?;
            let urls = extract_urls_with_base(&html, url);
            let net = Self::read_net(page).await;
            Ok(PageResult { html, urls, clicked, net })
        }
    }

    /// 读回钩子记录的网络请求
    pub(crate) async fn read_net(page: &chromiumoxide::Page) -> Vec<NetEntry> {
        let v = match page.evaluate(NET_READ_JS).await {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let arr = match v.value().and_then(|x| x.as_array()) {
            Some(a) => a.clone(),
            None => return Vec::new(),
        };
        arr.iter()
            .map(|e| NetEntry {
                kind: e.get("kind").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                url: e.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                body: e.get("body").and_then(|x| x.as_str()).map(|s| s.to_string()),
                req: e.get("req").and_then(|x| x.as_str()).map(|s| s.to_string()),
            })
            .collect()
    }

    /// 按可见文字点击。
    ///
    /// ★ 为什么不用 `el.click()`：这类站用 Radix UI（shadcn），
    ///   它的 **Tabs.Trigger 监听的是 `mousedown` 而不是 `click`**
    ///   （`el.click()` 只派发 click，不含 mousedown/mouseup），
    ///   于是"资源下载"标签页点不动 —— 实测 clicked=true 但弹窗始终 `data-state="closed"`。
    ///   改用 CDP 的 `Input.dispatchMouseEvent` 派发**真实鼠标事件**（可信事件，
    ///   mousedown/mouseup/click 都齐），Radix、React 合成事件、原生监听器通吃。
    async fn click_by_text(page: &chromiumoxide::Page, text: &str) -> bool {
        let want = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
        // 1) 找到元素、滚到视口中间、打标记
        //
        // ★ 按"可交互程度"分组依次找，**不能一把 querySelectorAll 全查**：
        //   `querySelectorAll('button, a, span, div')` 是按文档顺序返回的，
        //   外层容器 div（如标签栏 `-mx-4 mt-6 overflow-x-auto`）排在里面的 button 之前，
        //   它的 textContent 恰好也含"资源下载"、长度也在阈值内，于是点到容器中心
        //   = 两个标签之间的空白，什么都不会发生
        //   （实测 12 个游戏全部 clicked=true，但弹窗从未打开，data-state 一直是 closed）。
        let find_js = format!(
            r#"(function() {{
                var want = {want};
                var groups = ['button', 'a', '[role=tab]', '[role=button]',
                              'input[type=button]', 'input[type=submit]', 'label', 'span', 'div'];
                for (var g = 0; g < groups.length; g++) {{
                    var list = document.querySelectorAll(groups[g]);
                    for (var i = 0; i < list.length; i++) {{
                        var el = list[i];
                        var t = (el.textContent || '').trim();
                        if (t.indexOf(want) < 0 || t.length >= 80) continue;
                        // 容器不算：里面还包着别的可交互元素，点它的中心等于点空白
                        if ((el.tagName === 'DIV' || el.tagName === 'SPAN')
                            && el.querySelector('button, a, [role=tab], [role=button], input')) continue;
                        try {{
                            el.scrollIntoView({{ block: 'center', inline: 'center' }});
                            document.querySelectorAll('[data-vx-target]').forEach(function (o) {{
                                o.removeAttribute('data-vx-target');
                            }});
                            el.setAttribute('data-vx-target', '1');
                            return true;
                        }} catch (e) {{}}
                    }}
                }}
                return false;
            }})()"#
        );
        match page.evaluate(find_js).await {
            Ok(v) if v.value().and_then(|x| x.as_bool()).unwrap_or(false) => {}
            _ => return false,
        }

        // 滚动是异步的，等一帧再量坐标
        tokio::time::sleep(Duration::from_millis(250)).await;

        // 2) 量中心点
        let rect_js = r#"(function() {
            var el = document.querySelector('[data-vx-target="1"]');
            if (!el) return null;
            var r = el.getBoundingClientRect();
            if (r.width <= 0 || r.height <= 0) return null;
            return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
        })()"#;
        let coords = match page.evaluate(rect_js).await {
            Ok(v) => v.value().cloned(),
            Err(_) => None,
        };
        let (x, y) = match coords {
            Some(c) => (
                c.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0),
                c.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0),
            ),
            None => return false,
        };

        // 3) 真实鼠标点击
        page.click(chromiumoxide::layout::Point::new(x, y))
            .await
            .is_ok()
    }

    fn cdp_detail(&self) -> String {
        self.last_err
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| "无 CDP 错误".into())
    }

    pub async fn close(mut self) {
        let _ = self.browser.close().await;
        self.handler_task.abort();
    }
}

/// 抓**单页**（一次性会话，用完即关）。要抓多页请复用 [`BrowserSession`]。
pub async fn fetch_rendered(
    url: &str,
    click_text: Option<&str>,
    wait_ms: u64,
) -> Result<PageResult, String> {
    let s = BrowserSession::launch().await?;
    let r = s.fetch(url, click_text, wait_ms).await;
    s.close().await;
    r
}

/// 从 HTML 里抓所有链接并去重。
///
/// ★ 必须同时处理**相对链接**：galgamex 列表页的卡片是 `href="/game/caf5aa1d"`，
///   只抓绝对 URL 会一条都拿不到（实测踩过）。`base` 用来补全 origin。
pub fn extract_urls(html: &str) -> Vec<String> {
    extract_urls_with_base(html, "")
}

pub fn extract_urls_with_base(html: &str, base: &str) -> Vec<String> {
    // ★ JSON 转义必须还原：RSC / JSON 响应里的签名 URL 长这样
    //   `...zip?X-Amz-Algorithm=...\u0026X-Amz-Credential=...`，
    //   不还原的话正则会在 `\` 处截断，拿到一条残缺的 URL（query 全丢）。
    let unescaped = html
        .replace("&amp;", "&")
        .replace("\\/", "/")
        .replace("\\u002F", "/")
        .replace("\\u002f", "/")
        .replace("\\u0026", "&")
        .replace("\\u003d", "=")
        .replace("\\u003D", "=")
        .replace("\\u003f", "?")
        .replace("\\u003F", "?")
        .replace("\\u0025", "%");
    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // 绝对 URL
    if let Ok(re) = regex::Regex::new(r#"https?://[^\s"'<>\\\)\]\}]+"#) {
        for m in re.find_iter(&unescaped) {
            let u = m
                .as_str()
                .trim_end_matches(['.', ',', ';', ')', ']', '}'])
                .to_string();
            if seen.insert(u.clone()) {
                out.push(u);
            }
        }
    }

    // 相对链接 → 用 base 的 origin 补全
    let origin = origin_of(base);
    if !origin.is_empty() {
        if let Ok(re) = regex::Regex::new(r#"href="(/[^"']*)""#) {
            for cap in re.captures_iter(&unescaped) {
                let path = cap[1].trim();
                if path.starts_with("//") {
                    continue;
                }
                let u = format!("{origin}{path}");
                if seen.insert(u.clone()) {
                    out.push(u);
                }
            }
        }
    }
    out
}

/// 取 `scheme://host`（用于补全相对链接）
fn origin_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| format!("{}://{}", u.scheme(), h)))
        .unwrap_or_default()
}

/// 网盘 / 需要提取码的分享链接 —— 下载器下不了，一律排除。
const PAN_HOSTS: &[&str] = &[
    "pan.baidu.com", "yun.baidu.com", "pan.quark.cn", "aliyundrive.com", "alipan.com",
    "123pan.com", "123684.com", "lanzou", "lanzo", "caiyun.139.com", "115.com",
    "115cdn.com", "weiyun.com", "cloud.189.cn", "drive.uc.cn", "fast.uc.cn",
    "mega.nz", "mega.io", "drive.google.com", "onedrive.live.com", "1drv.ms", "mediafire.com",
    "pixeldrain.com", "terabox", "pan.xunlei.com", "kuaipan", "ctfile.com", "545c.com",
    "cowtransfer", "wetransfer.com", "dropbox.com", "4shared.com", "send.now", "file.io",
    // ★ 实测在 galgamex 详情页的资源里出现过的
    "drive.proton.me", "proton.me", "kfpromax.com/down", "onedrive",
];

/// 是不是网盘/分享链接
pub fn is_pan_link(u: &str) -> bool {
    let l = u.to_lowercase();
    PAN_HOSTS.iter().any(|h| l.contains(h))
}

/// 静态资源后缀 —— 这些永远不会是"要下载的游戏包"，必须排除。
const STATIC_EXT: &[&str] = &[
    ".css", ".js", ".mjs", ".json", ".webmanifest", ".map", ".xml", ".txt",
    ".png", ".jpg", ".jpeg", ".webp", ".gif", ".svg", ".ico", ".avif",
    ".woff", ".woff2", ".ttf", ".eot", ".mp4", ".webm", ".mp3", ".html", ".htm",
];

/// 直链判定：路径是压缩包/镜像，或落在已知的对象存储域名上。
///
/// ★ 踩过的坑：第一版把 `galgamex.net` 也写进了 DIRECT_HOSTS，
///   结果整站链接（favicon、CSS、`/game-tag/2`…）全被当成"直链"，
///   一个游戏页刷出 59 条假直链。**站点域名绝不能进这个表**，
///   只放真正的分发主机（`game.galgamex.com` 这种子域）。
pub fn is_direct_archive(u: &str) -> bool {
    if is_pan_link(u) {
        return false;
    }
    let Ok(parsed) = url::Url::parse(u) else {
        return false;
    };
    let host = parsed.host_str().unwrap_or("").to_lowercase();
    let path = parsed.path().to_lowercase();

    if STATIC_EXT.iter().any(|e| path.ends_with(e)) {
        return false;
    }

    if path.ends_with(".zip")
        || path.ends_with(".rar")
        || path.ends_with(".7z")
        || path.ends_with(".tar.gz")
        || path.ends_with(".tar")
        || path.ends_with(".iso")
        || path.ends_with(".xz")
        || path.ends_with(".zst")
    {
        return true;
    }

    // 站点自建 / 对象存储的直链（签名 URL 的路径未必带扩展名）
    const DIRECT_HOSTS: &[&str] = &[
        "game.galgamex.com",
        "backblazeb2.com",
        "amazonaws.com",
        "cloudflarestorage.com",
        "r2.dev",
        "r2.cloudflarestorage.com",
        "digitaloceanspaces.com",
        "aliyuncs.com",
        "myqcloud.com",
        "wasabisys.com",
        "linodeobjects.com",
        "storage.googleapis.com",
        "b-cdn.net",
    ];
    DIRECT_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

/// 把一组链接里**可直连下载的压缩包**挑出来（排除网盘）
pub fn pick_direct_archives(urls: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for u in urls {
        if is_direct_archive(u) && seen.insert(u.clone()) {
            out.push(u.clone());
        }
    }
    out
}

/// 从网络记录（响应体）里把所有链接挖出来并去重。
///
/// 这是找"点了按钮才出现"的下载地址的主力：DOM 里没有，响应体里有。
pub fn urls_from_net(net: &[NetEntry]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for e in net {
        // 响应体
        if let Some(b) = &e.body {
            for u in extract_urls_with_base(b, "") {
                if seen.insert(u.clone()) {
                    out.push(u);
                }
            }
        }
        // 请求 URL 本身也可能是下载地址（比如直接 GET 那个 zip）
        if !e.url.is_empty() && seen.insert(e.url.clone()) {
            out.push(e.url.clone());
        }
    }
    out
}

/// 只要**直链压缩包**（从网络记录里挑，排除网盘）
pub fn pick_direct_from_net(net: &[NetEntry]) -> Vec<String> {
    pick_direct_archives(&urls_from_net(net))
}

/// 详情页链接判定：同站、路径恰好是 `/game/<slug>`、且不是图片/静态资源。
///
/// ★ 踩过的坑：用 `u.contains("/game/")` 过滤会把
///   `https://imgs.galgamex.win/game/6264/cover.webp` 也当成详情页，
///   结果 6 个"详情页"全是封面图，点进去当然没有下载链接（实测 109 秒白跑）。
pub fn is_detail_link(u: &str) -> bool {
    let Ok(parsed) = url::Url::parse(u) else {
        return false;
    };
    let host = parsed.host_str().unwrap_or("");
    if host.starts_with("imgs.")
        || host.starts_with("static.")
        || host.starts_with("cdn.")
        || host.starts_with("img.")
    {
        return false;
    }
    let segs: Vec<&str> = parsed.path().trim_matches('/').split('/').collect();
    // 恰好两段，且第一段是 game
    segs.len() == 2 && segs[0] == "game" && !segs[1].is_empty() && !segs[1].contains('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_extraction_handles_escapes() {
        let html = r#"<a href="https://a.com/x.zip?a=1&amp;b=2">x</a>
            <script>var u="https:\/\/b.com\/y.rar";</script>"#;
        let v = extract_urls(html);
        assert!(v.iter().any(|u| u == "https://a.com/x.zip?a=1&b=2"), "实体没还原: {v:?}");
        assert!(v.iter().any(|u| u == "https://b.com/y.rar"), "JS 转义没还原: {v:?}");
    }

    #[test]
    fn pan_links_are_excluded() {
        assert!(is_pan_link("https://pan.baidu.com/s/1xyz"));
        assert!(is_pan_link("https://pan.quark.cn/s/abc"));
        assert!(is_pan_link("https://www.aliyundrive.com/s/abc"));
        assert!(!is_pan_link("https://game.galgamex.com/7001-8000/a.zip"));
    }

    #[test]
    fn direct_archive_detection() {
        // 用户给过的那条真实链接形态
        assert!(is_direct_archive(
            "https://game.galgamex.com/7001-8000/Gamex-007580/%23A9830.zip?X-Amz-Signature=abc"
        ));
        assert!(is_direct_archive("https://x.s3.amazonaws.com/a.7z"));
        assert!(!is_direct_archive("https://pan.baidu.com/s/1abc.zip"), "网盘即使带 zip 也要排除");
        assert!(!is_direct_archive("https://example.com/page.html"));
        // ★ 实测踩过：站点自己的域名曾被写进 DIRECT_HOSTS，导致整站链接都算直链
        assert!(!is_direct_archive("https://www.galgamex.net/game/013baqnj"));
        assert!(!is_direct_archive("https://www.galgamex.net/game-tag/2"));
        assert!(!is_direct_archive("https://www.galgamex.net/_next/static/chunks/2ta9v4d_6zla0.css"));
        assert!(!is_direct_archive("https://rybbit.galgamex.com/api/script.js"));
        assert!(!is_direct_archive("https://www.galgamex.net/logo.svg"));
    }

    #[test]
    fn relative_links_are_absolutized() {
        let html = r#"<a href="/game/caf5aa1d">x</a><a href="https://o.com/a.zip">y</a>"#;
        let v = extract_urls_with_base(html, "https://www.galgamex.net/games");
        assert!(v.iter().any(|u| u == "https://www.galgamex.net/game/caf5aa1d"), "{v:?}");
        assert!(v.iter().any(|u| u == "https://o.com/a.zip"), "{v:?}");
    }

    #[test]
    fn browser_session_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BrowserSession>();
    }

    #[test]
    fn detail_link_filter_excludes_images() {
        assert!(is_detail_link("https://www.galgamex.net/game/caf5aa1d"));
        // ★ 实测踩过的坑：封面图也含 "/game/"
        assert!(!is_detail_link("https://imgs.galgamex.win/game/6264/cover.webp"));
        assert!(!is_detail_link("https://imgs.galgamex.win/game/6264/header-mini.webp"));
        assert!(!is_detail_link("https://www.galgamex.net/games"));
        assert!(!is_detail_link("https://www.galgamex.net/game/a.b"));
    }

    // ============================================================
    // 联网实测（默认 #[ignore]，用 `cargo test browser:: -- --ignored --nocapture` 跑）
    // 证明"必须点按钮才出链接"这一判断，并把真实直链抓出来。
    // ============================================================
    // ============================================================
    // 诊断用：截图 + 列出候选元素，看点击到底落在哪
    // ============================================================
    #[tokio::test]
    #[ignore]
    async fn live_debug_click() {
        let s = BrowserSession::launch().await.expect("启动浏览器失败");
        let url = "https://www.galgamex.net/game/013baqnj";
        let page = s.browser.new_page(url).await.expect("open");
        let _ = page.wait_for_navigation().await;
        tokio::time::sleep(Duration::from_millis(6000)).await;
        let _ = page.evaluate(NET_HOOK_JS).await;

        // 第一步：点「资源下载」标签
        println!("\n[1] 点资源下载 -> {}", BrowserSession::click_by_text(&page, "资源下载").await);
        tokio::time::sleep(Duration::from_millis(4000)).await;

        // 第二步：点「RAR」那条线路
        println!("[2] 点 RAR -> {}", BrowserSession::click_by_text(&page, "RAR").await);
        tokio::time::sleep(Duration::from_millis(6000)).await;

        let net = BrowserSession::read_net(&page).await;
        println!("\n=== 网络请求 {} 条 ===", net.len());
        for e in &net {
            println!("  [{}] {}", e.kind, e.url.chars().take(140).collect::<String>());
        }
        println!("\n=== 响应体里含 download/zip/rar 的 ===");
        for e in &net {
            let l = e.url.to_lowercase();
            if l.contains("download") || l.contains("resource") {
                if let Some(b) = &e.body {
                    println!("--- {}", e.url);
                    println!("{}", b.chars().take(1200).collect::<String>());
                }
            }
        }
        // 资源列表是从 server action 响应里来的：把 /game/<slug> 的响应体也 dump 出来
        for (i, e) in net.iter().enumerate() {
            if e.url.contains("/game/") && !e.url.contains("/api/") {
                if let Some(b) = &e.body {
                    let _ = std::fs::write(format!(r"F:\vdgame\_rsc{i}.txt"), b);
                    println!("\n=== server action 响应 {} ({} 字节) ===", e.url, b.len());
                    // 找 resources 附近
                    if let Some(p) = b.find("\"resources\"") {
                        println!("{}", b[p.saturating_sub(120)..(p + 900).min(b.len())].to_string());
                    } else if let Some(p) = b.find("resources") {
                        println!("{}", b[p.saturating_sub(120)..(p + 900).min(b.len())].to_string());
                    } else {
                        println!("(响应里没有 resources 关键字, 前 600 字)");
                        println!("{}", b.chars().take(600).collect::<String>());
                    }
                }
            }
        }
        // 有没有弹出登录框
        let html = page.content().await.unwrap_or_default();
        println!("\n登录对话框出现: {}", html.contains("登录") && html.contains("密码"));
        println!("页面里出现 .rar/.zip: {}", html.to_lowercase().matches(".rar").count());
        let _ = std::fs::write(r"F:\vdgame\_detail_debug.html", &html);
        if let Ok(b) = page.screenshot(chromiumoxide::page::ScreenshotParams::default()).await {
            let _ = std::fs::write(r"F:\vdgame\_shot_after.png", &b);
        }
        let _ = page.close().await;
        s.close().await;
    }

    #[tokio::test]
    #[ignore]
    async fn live_galgamex_scrape_direct_link() {
        let s = BrowserSession::launch().await.expect("启动浏览器失败");

        // 1) 列表页 → 详情链接
        let list = s
            .fetch("https://www.galgamex.net/games", None, 6000)
            .await
            .expect("列表页抓取失败");
        let _ = std::fs::write(r"F:\vdgame\_list.html", &list.html);

        let mut details: Vec<String> =
            list.urls.iter().filter(|u| is_detail_link(u)).cloned().collect();
        details.sort();
        details.dedup();
        println!(
            "\n列表页: {} 条链接, 详情页 {} 条",
            list.urls.len(),
            details.len()
        );
        for d in details.iter().take(5) {
            println!("   {d}");
        }
        assert!(!details.is_empty(), "列表页没抓到详情链接");

        // 2) 逐个详情页点「资源下载」，汇总所有直链（排除网盘）
        let mut direct: Vec<String> = Vec::new();
        let mut pan_all: Vec<String> = Vec::new();
        let mut with_pan = 0usize;
        let scanned = 12;

        for (i, d) in details.iter().take(scanned).enumerate() {
            let r = match s.fetch(d, Some("资源下载"), 4000).await {
                Ok(r) => r,
                Err(e) => {
                    println!("  [跳过] {d} -> {e}");
                    continue;
                }
            };
            let _ = std::fs::write(format!(r"F:\vdgame\_detail{i}.html"), &r.html);

            // 两条路一起走：DOM 里的 + 网络响应里的
            let mut d2 = pick_direct_archives(&r.urls);
            let from_net = pick_direct_from_net(&r.net);
            for u in from_net {
                if !d2.contains(&u) {
                    d2.push(u);
                }
            }
            let all = urls_from_net(&r.net);
            let pan: Vec<String> = r
                .urls
                .iter()
                .chain(all.iter())
                .filter(|u| is_pan_link(u))
                .cloned()
                .collect();
            if !pan.is_empty() {
                with_pan += 1;
            }
            println!(
                "[{}/{}] {}  clicked={}  DOM链接={}  网络请求={}  网盘={}  直链={}",
                i + 1,
                scanned,
                d.rsplit('/').next().unwrap_or(d),
                r.clicked,
                r.urls.len(),
                r.net.len(),
                pan.len(),
                d2.len()
            );
            for p in pan.iter().take(4) {
                println!("      [网盘-排除] {}", p.chars().take(100).collect::<String>());
            }
            for u in d2.iter().take(6) {
                println!("      [直链] {}", u.chars().take(150).collect::<String>());
            }
            // 网络请求里带 download/zip/rar 字样的，也打出来看
            for e in r.net.iter() {
                let l = e.url.to_lowercase();
                if l.contains("download") || l.contains("resource") {
                    println!(
                        "      [net] {} {}",
                        e.kind,
                        e.url.chars().take(120).collect::<String>()
                    );
                }
            }
            pan_all.extend(pan);
            direct.append(&mut d2);
            // 攒够 3 条候选就够随机挑了
            if direct.len() >= 3 {
                break;
            }
        }

        s.close().await;
        println!(
            "\n扫描完: 直链候选 {} 条, 网盘 {} 条 (来自 {with_pan} 个游戏)",
            direct.len(),
            pan_all.len()
        );
        assert!(
            !direct.is_empty(),
            "扫描 {scanned} 个游戏都没抓到直链（网盘链接共 {} 条）",
            pan_all.len()
        );

        // 3) 随机挑一条 —— 用户要求"排除所有网盘链接随机挑选一个下载"
        let pick = {
            use std::time::{SystemTime, UNIX_EPOCH};
            let seed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as usize)
                .unwrap_or(0);
            direct[seed % direct.len()].clone()
        };
        println!("\n★ 随机选中的下载链接:\n{pick}");
    }
}
