// Tauri 命令桥接层 - 暴露给前端的 IPC 接口
use crate::cache::Cache;
use crate::config::Config;
use crate::download_engine::downloader_manager::{Aria2Supervisor, SharedAria2Supervisor};
use crate::downloader::{DownloadTask, DownloadTasksMap};
use crate::game_translator::{GameTranslator, TranslateConfig, TranslateEngine, TranslateReport, TranslateConfigWrap, TranslateEngineWrap, TranslateReportWrap};
use crate::search_engine::{GameCard, GameDetail, SearchEngine, DownloadLink};
use crate::snapshot::Snapshot;
use crate::translator::Translator;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
use serde_json::{json, Value};

#[cfg(windows)] use std::os::windows::process::CommandExt;

pub struct AppState {
    pub cache: Arc<Cache>,
    pub snapshot: Arc<Snapshot>,
    pub search: Arc<SearchEngine>,
    pub translator: Arc<Translator>,
    pub game_translator: Arc<GameTranslator>,
    pub download_tasks: DownloadTasksMap,
    pub aria2: SharedAria2Supervisor,
}
impl AppState {
    pub fn new() -> Self {
        let cache = Arc::new(Cache::new());
        let snapshot = Arc::new(Snapshot::new());
        let search = Arc::new(SearchEngine::new(cache.clone(), snapshot.clone()));
        let translator = Arc::new(Translator::new());
        let game_translator = Arc::new(GameTranslator::new());
        let download_tasks = crate::downloader::create_tasks_map();
        let aria2: SharedAria2Supervisor = Arc::new(std::sync::Mutex::new(Aria2Supervisor::new()));
        AppState { cache, snapshot, search, translator, game_translator, download_tasks, aria2 }
    }
}

// ---------- append_frontend_log (写 frontend_events.log) ----------
fn frontend_log_path() -> std::path::PathBuf {
    let base = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("logs").join("frontend_events.log")
}

#[tauri::command]
pub fn append_frontend_log(kind: String, detail: String) -> Result<(), String> {
    use std::io::Write;
    let p = frontend_log_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p).map_err(|e| e.to_string())?;
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
    writeln!(f, "[{ts}] [{kind}] {detail}").map_err(|e| e.to_string())
}

// ===================== 配置 =====================
#[tauri::command]
pub fn get_config() -> Result<Config, String> { Ok(Config::load()) }

#[tauri::command]
pub fn save_config(config: Config) -> Result<(), String> { config.save() }

/// 前端首屏渲染完成后调用, 显示主窗口。
///
/// ★ 消除启动白屏 (2026-10-03): 窗口配置为 `visible: false` 启动,
///   避免用户看到一个**纯白空窗口**等 WebView2 解析 654KB 前端资源
///   (实测白屏约 1.5 秒, 用户感知就是"刚打开卡死一下")。
///   前端在 init 完成、首屏 DOM 渲染好之后调用本命令把窗口显示出来 ——
///   用户看到的第一帧就是完整界面。
///
/// 兜底: 前端异常时窗口会一直不显示, 所以 main.rs 里另有一个
/// 3 秒超时强制显示的保护。
#[tauri::command]
pub fn show_main_window(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
        crate::app_logger::log_window("SHOW", "前端就绪, 显示主窗口");
    }
    Ok(())
}

// ===================== 授权 / 认证 =====================
/// 查询当前授权状态 (前端启动时与激活后都会调)
#[tauri::command]
pub fn license_status() -> crate::licensing::LicenseInfo {
    let info = crate::licensing::info();
    // ★ 记一条日志: 前端是否真的调到了后端、当前是什么档位, 一眼可查
    crate::app_logger::log_config(
        "LICENSE_STATUS",
        &format!(
            "activated={} tier={} key_id={} speed={}% all_themes={}",
            info.activated, info.tier, info.key_id, info.speed_percent,
            info.all_themes
        ),
    );
    info
}

/// 激活密钥。付费密钥为一次性, 用过的会被拒。
#[tauri::command]
pub fn license_activate(key: String) -> Result<crate::licensing::LicenseInfo, String> {
    let r = crate::licensing::activate(&key);
    crate::app_logger::log_config(
        "LICENSE_ACTIVATE",
        &match &r {
            Ok(i) => format!("ok tier={} key_id={}", i.tier, i.key_id),
            Err(e) => format!("failed err={}", e),
        },
    );
    r
}

// ===================== 浏览 / 搜索 =====================
#[tauri::command]
pub async fn browse(source: String, page: u32, state: State<'_, AppState>) -> Result<Vec<GameCard>, String> {
    let results = state.search.browse(&source, page).await;
    crate::app_logger::log_search("browse", &format!("source={} page={} results={}", source, page, results.len()));
    Ok(results)
}

#[tauri::command]
pub async fn browse_by_category(source: String, category: String, page: u32, state: State<'_, AppState>) -> Result<Vec<GameCard>, String> {
    let results = state.search.browse_by_category(&source, &category, page).await;
    crate::app_logger::log_search("browse_category", &format!("source={} category={} page={} results={}", source, category, page, results.len()));
    Ok(results)
}

#[tauri::command]
pub async fn get_adult_tags(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let _ = &state;
    Ok(state.search.adult_tags())
}

#[tauri::command]
pub async fn browse_adult(tag: Option<String>, page: u32, state: State<'_, AppState>) -> Result<Vec<GameCard>, String> {
    let results = state.search.browse_adult(tag.as_deref(), page).await;
    let count = results.as_ref().map(|v| v.len()).unwrap_or(0);
    crate::app_logger::log_search("browse_adult", &format!("tag={:?} page={} results={} ok={}", tag, page, count, results.is_ok()));
    results
}

#[tauri::command]
pub async fn search_adult(keyword: String, tag: Option<String>, state: State<'_, AppState>) -> Result<Vec<GameCard>, String> {
    let _ = tag;
    let results = state.search.search_adult(&keyword).await;
    let count = results.as_ref().map(|v| v.len()).unwrap_or(0);
    crate::app_logger::log_search("search_adult", &format!("keyword={} results={} ok={}", keyword, count, results.is_ok()));
    results
}

// 成人游戏独立页: 本地缓存筛选 (分类 + 关键词 + 分页), 毫秒级响应
// ★ 性能修复 (2026-09-30): 改为 async → 在 tokio 工作线程执行.
//   非 async 命令会在 Tauri 主线程 (UI 线程) 上运行, 而本命令要遍历/克隆全部缓存卡片,
//   在 ~2 万张卡片规模下会把 UI 线程卡住 → "点击 成人游戏 卡一会".
#[tauri::command]
pub async fn adult_browse_local(category: String, keyword: String, page: u32, state: State<'_, AppState>) -> Result<Vec<GameCard>, String> {
    Ok(state.search.adult_local_browse(&category, &keyword, page))
}

// 成人游戏独立页: 分类标签列表 (从成人游戏 tags 动态统计)
// ★ 性能修复 (2026-09-30): 同上, 遍历全部卡片统计标签, 移出主线程.
#[tauri::command]
pub async fn adult_categories(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    Ok(state.search.adult_categories())
}

/// 成人库总数（前端显示"共 N 款"，用户报"数量明显不对"时一眼能看出来）
#[tauri::command]
pub async fn adult_total(category: String, keyword: String, state: State<'_, AppState>) -> Result<usize, String> {
    Ok(state.search.adult_total(&category, &keyword))
}

#[tauri::command]
pub async fn search(keyword: String, source: String, category: String, state: State<'_, AppState>) -> Result<Vec<GameCard>, String> {
    let results = state.search.search(&keyword, &source, &category).await;
    crate::app_logger::log_search("search", &format!("keyword={} source={} category={} results={}", keyword, source, category, results.len()));
    Ok(results)
}

#[tauri::command]
pub async fn fetch_detail(source: String, detail_url: String, state: State<'_, AppState>) -> Result<GameDetail, String> {
    Ok(state.search.fetch_detail(&source, &detail_url).await)
}

#[tauri::command]
pub async fn fetch_downloads(source: String, detail_url: String, state: State<'_, AppState>) -> Result<Vec<DownloadLink>, String> {
    Ok(state.search.fetch_downloads(&source, &detail_url).await)
}

// ===================== 翻译 =====================
#[tauri::command]
pub async fn translate(text: String, state: State<'_, AppState>) -> Result<String, String> {
    state.translator.translate(text).await
}

#[tauri::command]
pub async fn translate_game_info(texts: Vec<String>, state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let _ = &state;
    let _ = &texts;
    Ok(texts.clone())
}

#[tauri::command]
pub async fn set_translate_lang(lang: String, state: State<'_, AppState>) -> Result<(), String> {
    state.translator.set_lang(lang); Ok(())
}

#[tauri::command]
pub async fn get_translate_lang(state: State<'_, AppState>) -> Result<String, String> {
    Ok(state.translator.get_lang())
}

#[tauri::command]
pub async fn clear_translate_cache(state: State<'_, AppState>) -> Result<(), String> {
    state.translator.clear_cache(); Ok(())
}

#[tauri::command]
pub async fn translate_force(text: String, state: State<'_, AppState>) -> Result<String, String> {
    state.translator.translate(text).await
}

#[tauri::command]
pub async fn game_tr_set_engine(engine: TranslateEngineWrap, state: State<'_, AppState>) -> Result<(), String> {
    state.game_translator.set_engine(engine.0)
}

#[tauri::command]
pub async fn game_tr_get_engine(state: State<'_, AppState>) -> Result<TranslateEngineWrap, String> {
    Ok(TranslateEngineWrap(state.game_translator.get_engine()))
}

#[tauri::command]
pub async fn game_tr_set_config(
    target_lang: Option<String>,
    auto_backup: Option<bool>,
    engine: Option<TranslateEngineWrap>,
    cfg: Option<TranslateConfigWrap>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    // 前端可能传 { targetLang } 或 { autoBackup } 或 { cfg }
    // 统一合并到 GameTranslator 的语言设置上
    if let Some(tl) = target_lang {
        state.game_translator.set_lang(tl);
    }
    if let Some(c) = cfg {
        let _ = state.game_translator.set_config(c.0);
    }
    let _ = auto_backup;
    let _ = engine;
    Ok(())
}

#[tauri::command]
pub async fn game_tr_get_config(state: State<'_, AppState>) -> Result<TranslateConfigWrap, String> {
    Ok(TranslateConfigWrap(state.game_translator.get_config()))
}

#[tauri::command]
pub async fn game_tr_translate(text: String, source: Option<String>, state: State<'_, AppState>) -> Result<String, String> {
    let _ = source;
    state.game_translator.translate(text).await
}

#[tauri::command]
pub async fn game_tr_translate_batch(texts: Vec<String>, source: Option<String>, state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let _ = source;
    state.game_translator.translate_batch(texts).await
}

#[tauri::command]
pub async fn game_tr_translate_dir(dir: String, source: Option<String>, state: State<'_, AppState>) -> Result<TranslateReportWrap, String> {
    let r = state.game_translator.translate_dir(dir, source).await?;
    Ok(TranslateReportWrap(r))
}

#[tauri::command]
pub async fn game_tr_clear_cache(state: State<'_, AppState>) -> Result<(), String> {
    state.game_translator.clear_cache(); Ok(())
}

/// 保存游戏中文翻译名到索引, 用于翻译名搜索
/// ★ 性能修复 (2026-09-30): 改为 async, 让 O(n) 全量卡片遍历离开 UI 主线程执行, 避免点击卡顿
#[tauri::command]
pub async fn save_translated_name(appid: String, name_cn: String, state: State<'_, AppState>) -> Result<bool, String> {
    Ok(state.search.update_translated_name(&appid, &name_cn))
}

/// 批量保存游戏中文翻译名 (前端卡片自动翻译完成后一次性回写)
/// ★ 性能修复 (2026-09-30): 替代「每张卡片各调一次」的旧方式, 单次 IPC 完成整批写入
#[tauri::command]
pub async fn save_translated_names(pairs: Vec<(String, String)>, state: State<'_, AppState>) -> Result<usize, String> {
    Ok(state.search.batch_set_translated_names(&pairs))
}

/// 后台全量翻译缓存中所有未翻译的游戏名 (byrut + koyso)
/// 返回成功翻译的数量, 不阻塞 UI (前端在启动后异步调用)
#[tauri::command]
pub async fn translate_all_game_names(state: State<'_, AppState>) -> Result<usize, String> {
    let untranslated = state.search.get_untranslated_games();
    if untranslated.is_empty() { return Ok(0); }
    let mut total = 0usize;
    // 分批翻译, 每批 40 个, 避免单次请求过大
    const BATCH: usize = 40;
    for chunk in untranslated.chunks(BATCH) {
        let texts: Vec<String> = chunk.iter().map(|(_, n)| n.clone()).collect();
        match state.game_translator.translate_batch(texts).await {
            Ok(translated) => {
                let pairs: Vec<(String, String)> = chunk.iter().zip(translated.iter())
                    .filter(|((_appid, orig), tr)| !tr.is_empty() && tr.as_str() != orig.as_str())
                    .map(|((appid, _), tr)| (appid.clone(), tr.clone()))
                    .collect();
                total += state.search.batch_update_translated_names(&pairs);
            }
            Err(_) => { /* 单批失败跳过, 继续下一批 */ }
        }
        // 每批之间短暂让步, 避免阻塞事件循环
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    if total > 0 {
        state.search.save_index_to_disk();
    }
    Ok(total)
}

// ===================== 下载 =====================
// 前端契约: invoke('start_download', { taskId, url, filePath, headers })
// (Tauri 自动把 camelCase 映射为 snake_case 参数名)
#[tauri::command]
pub async fn start_download(
    task_id: String,
    url: String,
    file_path: String,
    headers: Option<Vec<(String, String)>>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    crate::app_logger::log_command("start_download", &format!("task_id={} url={} file_path={}", task_id, url, file_path));
    // ★ 授权闸门 (后端强制): 未激活不允许下载。
    //   前端也有拦截, 但前端可被绕过, 所以这里必须再判一次。
    if !crate::licensing::is_activated() {
        crate::app_logger::log_command("start_download", "拒绝: 未激活授权");
        return Err("未激活授权, 请先在「设置 → 认证」中输入密钥".to_string());
    }
    if task_id.is_empty() {
        return Err("task_id 不能为空".to_string());
    }

    // byrut 下载页返回 .torrent 种子: 先下载种子到临时文件, 再用本地种子启动 BT
    let (effective_url, effective_file_path) = if crate::downloader::is_byrut_torrent_url(&url) {
        let client = crate::search_engine::build_client();
        // ★ 修复: 之前单次请求无重试, byrut 站点偶发连接失败会直接导致 BT 任务失败
        //   ("卡在连接然后下载失败"). 改为最多 3 次重试 + 递增退避, 并区分 HTTP 错误/内容错误/网络错误.
        let mut last_err = String::new();
        let mut fetched: Option<Vec<u8>> = None;
        for attempt in 1..=3u32 {
            let mut req = client.get(&url);
            for (k, v) in headers.iter().flatten() {
                req = req.header(k, v);
            }
            // ★ byrut 站点下载种子必须带 cookie (age_verified=true; site_auth=1)
            //   缺这两个 cookie 时服务器返回 HTML 错误页而非 .torrent 二进制.
            req = req.header("Cookie", "age_verified=true; site_auth=1");
            crate::app_logger::log_network(
                "TORRENT_FETCH",
                &format!("url={} task_id={} attempt={}", url, task_id, attempt),
            );
            match req.send().await {
                Ok(resp) => {
                    if !resp.status().is_success() {
                        last_err = format!("HTTP {}", resp.status());
                        crate::app_logger::log_network(
                            "TORRENT_HTTP_ERR",
                            &format!("url={} status={} attempt={}", url, resp.status(), attempt),
                        );
                    } else {
                        match resp.bytes().await {
                            Ok(b) => {
                                if !b.is_empty() && b[0] == b'd' {
                                    fetched = Some(b.to_vec());
                                    break;
                                }
                                let preview =
                                    String::from_utf8_lossy(&b[..b.len().min(200)]).to_string();
                                last_err = format!("服务器返回的不是有效的 .torrent 文件 (开头: {}...)", preview);
                                crate::app_logger::log_network(
                                    "TORRENT_BAD_BODY",
                                    &format!("url={} attempt={}", url, attempt),
                                );
                            }
                            Err(e) => {
                                last_err = format!("读取种子失败: {}", e);
                                crate::app_logger::log_network(
                                    "TORRENT_READ_FAIL",
                                    &format!("url={} error={} attempt={}", url, e, attempt),
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    last_err = format!("下载种子失败: {}", e);
                    crate::app_logger::log_network(
                        "TORRENT_FAIL",
                        &format!("url={} error={} attempt={}", url, e, attempt),
                    );
                }
            }
            if attempt < 3 {
                tokio::time::sleep(std::time::Duration::from_millis(600 * attempt as u64)).await;
            }
        }
        let bytes = match fetched {
            Some(b) => b,
            None => {
                crate::app_logger::log_network(
                    "TORRENT_GIVEUP",
                    &format!("url={} task_id={} last_err={}", url, task_id, last_err),
                );
                return Err(format!("下载种子失败(已重试 3 次): {}", last_err));
            }
        };
        // BT 输出目录: 用 file_path 本身作为目录名 (去掉压缩包扩展名), 每个游戏独立目录
        //   file_path 形如 D:\game\GameName[.zip] → 输出目录 D:\game\GameName
        let bt_dir = {
            let mut p = file_path.clone();
            for ext in &[".torrent", ".zip", ".rar", ".7z", ".tar", ".gz", ".bz2"] {
                if p.to_lowercase().ends_with(ext) {
                    p.truncate(p.len() - ext.len());
                    break;
                }
            }
            p
        };
        // 种子存到 BT 输出目录下
        let torrent_path = format!("{}.torrent", bt_dir);
        if let Some(parent) = Path::new(&torrent_path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&torrent_path, &bytes).map_err(|e| format!("保存种子失败: {}", e))?;
        (torrent_path, bt_dir)
    } else {
        // 非 byrut 的 BT (magnet/.torrent): file_path 直接作为输出目录
        (url.clone(), file_path.clone())
    };

    // BT 任务 ID 统一 bt-xxx 前缀 (前端据此渲染 BT 进度条与跳过自动解压)
    let real_id = if crate::downloader::is_bt_url(&effective_url) {
        format!("bt-{}", task_id.trim_start_matches("dl_"))
    } else {
        task_id.clone()
    };
    let task = Arc::new(DownloadTask::new(
        real_id.clone(),
        effective_url,
        effective_file_path,
        headers.unwrap_or_default(),
    ));
    {
        let mut m = state.download_tasks.lock().await;
        m.insert(real_id.clone(), task.clone());
    }
    crate::downloader::spawn_download(app, task).await;
    Ok(real_id)
}

// ============================================================
// BT 种子预解析 (下载前弹窗: 列出目录内容 + 识别压缩包)
// ============================================================
#[derive(serde::Serialize)]
pub struct TorrentPreviewFile {
    pub name: String,
    pub size: u64,
    pub ext: String,
    pub is_archive: bool,
}

#[derive(serde::Serialize)]
pub struct TorrentPreview {
    pub ok: bool,
    pub name: String,
    pub total_size: u64,
    pub file_count: usize,
    pub files: Vec<TorrentPreviewFile>,
    pub has_archive: bool,
    pub source_kind: String,
    pub error: Option<String>,
}

/// 压缩包扩展名识别 (下载前弹窗据此显示"自动解压/建快捷方式"选项)
fn detect_archive_ext(name: &str) -> Option<String> {
    let lower = name.to_lowercase();
    for ext in [".zip", ".rar", ".7z", ".tar", ".gz", ".bz2", ".xz"] {
        if lower.ends_with(ext) {
            return Some(ext.trim_start_matches('.').to_string());
        }
    }
    None
}

/// 下载前解析 BT 种子: 支持 magnet / 本地 .torrent / 远程 .torrent / byrut 种子页.
/// 返回文件目录列表与压缩包识别结果, 供前端弹窗在下载前展示。
#[tauri::command]
pub async fn parse_torrent_preview(
    url: String,
    headers: Option<Vec<(String, String)>>,
) -> Result<TorrentPreview, String> {
    let url_trim = url.trim().to_string();
    let lower = url_trim.to_lowercase();

    // magnet: 连接前无法获取文件列表, 只返回显示名占位
    if lower.starts_with("magnet:") {
        let meta = swiftfetch::TorrentMeta::from_magnet(&url_trim)
            .map_err(|e| format!("解析磁力链接失败: {}", e))?;
        return Ok(TorrentPreview {
            ok: true,
            name: meta.display_name.clone(),
            total_size: meta.total_size,
            file_count: 0,
            files: Vec::new(),
            has_archive: false,
            source_kind: "magnet".to_string(),
            error: None,
        });
    }

    // 读取 .torrent 字节: 本地文件直接读, 远程 URL 走 HTTP (byrut 需带 cookie)
    let bytes: Vec<u8> = if lower.ends_with(".torrent") && Path::new(&url_trim).exists() {
        std::fs::read(&url_trim).map_err(|e| format!("读取本地种子失败: {}", e))?
    } else {
        let client = crate::search_engine::build_client();
        let mut req = client.get(&url_trim);
        for (k, v) in headers.iter().flatten() {
            req = req.header(k, v);
        }
        if crate::downloader::is_byrut_torrent_url(&url_trim) {
            req = req.header("Cookie", "age_verified=true; site_auth=1");
        }
        let resp = req.send().await.map_err(|e| format!("下载种子失败: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("下载种子失败: HTTP {}", resp.status()));
        }
        let b = resp.bytes().await.map_err(|e| format!("读取种子失败: {}", e))?;
        if b.is_empty() || b[0] != b'd' {
            return Err("服务器返回的不是有效的 .torrent 文件".to_string());
        }
        b.to_vec()
    };

    let meta = swiftfetch::TorrentMeta::from_torrent_bytes(&bytes)
        .map_err(|e| format!("解析种子失败: {}", e))?;

    let mut files = Vec::with_capacity(meta.files.len());
    let mut has_archive = false;
    for f in &meta.files {
        let ext = detect_archive_ext(&f.name);
        let is_archive = ext.is_some();
        if is_archive {
            has_archive = true;
        }
        files.push(TorrentPreviewFile {
            name: f.name.clone(),
            size: f.size,
            ext: ext.unwrap_or_default(),
            is_archive,
        });
    }

    Ok(TorrentPreview {
        ok: true,
        name: meta.display_name.clone(),
        total_size: meta.total_size,
        file_count: files.len(),
        files,
        has_archive,
        source_kind: "torrent".to_string(),
        error: None,
    })
}

#[tauri::command]
pub async fn cancel_download(task_id: String, state: State<'_, AppState>) -> Result<(), String> {
    crate::app_logger::log_command("cancel_download", &format!("task_id={}", task_id));
    crate::app_logger::log_pause("CANCEL", &format!("task_id={}", task_id));
    let task = {
        let m = state.download_tasks.lock().await;
        m.get(&task_id).cloned()
    };
    if let Some(t) = task {
        // ★ 新版: 优先通过 PauseController.cancel() 广播取消信号
        // (HTTP: 广播 EngineState::Canceled + cancel_flag=true + Notify;
        //  BT: cancel_flag=true + stop_notify 唤醒所有模块)
        let pc_opt = t.pause_controller.lock().await.clone();
        if let Some(pc) = pc_opt {
            pc.cancel();
        } else {
            // 引擎尚未初始化 (probe 阶段), 直接设 cancel_flag
            t.cancel();
        }
        *t.state.lock().await = "canceled".into();
        crate::app_logger::log_pause("CANCELED", &format!("task_id={} state→canceled", task_id));
        // 200ms 内 worker / BT 模块会退出, run_*_download 会发 download-finished(canceled)
    }
    Ok(())
}

#[tauri::command]
pub async fn pause_download(task_id: String, state: State<'_, AppState>) -> Result<(), String> {
    crate::app_logger::log_command("pause_download", &format!("task_id={}", task_id));
    crate::app_logger::log_pause("PAUSE", &format!("task_id={}", task_id));
    let task = {
        let m = state.download_tasks.lock().await;
        m.get(&task_id).cloned()
    };
    if let Some(t) = task {
        // ★ 新版 (HTTP): 通过 PauseController.pause() 广播 EngineState::Paused,
        //   workers 主动 select! 等待 resume_notify, 不再 abort future,
        //   彻底解决 pause 后进度条在 running/paused 之间抽搐的 bug.
        // ★ BT 模式: 仍需 abort handle (旧引擎不支持热暂停), 但也走 stop_notify 通知.
        let pc_opt = t.pause_controller.lock().await.clone();
        let is_http = t.engine == "speed";
        if let Some(pc) = pc_opt {
            pc.pause();
        }
        if !is_http {
            // BT 旧引擎: 模块从 stop_notify 退出后, 还需 abort handle 防止残留
            if let Some(h) = t.handle.lock().await.take() {
                h.abort();
            }
        }
        *t.state.lock().await = "paused".into();
        crate::app_logger::log_pause("PAUSED", &format!("task_id={} engine={} state→paused", task_id, t.engine));
    }
    Ok(())
}

#[tauri::command]
pub async fn resume_download(task_id: String, app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    crate::app_logger::log_command("resume_download", &format!("task_id={}", task_id));
    crate::app_logger::log_pause("RESUME", &format!("task_id={}", task_id));
    let task = {
        let m = state.download_tasks.lock().await;
        m.get(&task_id).cloned()
    };
    if let Some(t) = task {
        let cur = t.state.lock().await.clone();
        if cur != "paused" {
            crate::app_logger::log_pause("RESUME_SKIP", &format!("task_id={} 当前状态={} (非 paused, 跳过)", task_id, cur));
            return Ok(()); // 仅暂停态可恢复
        }
        // ★ 新版 (HTTP): 通过 PauseController.resume() 广播 EngineState::Running + Notify,
        //   workers 从 select! 唤醒继续下载, 不重新 spawn (断点续传 in-place).
        // ★ BT 模式: 旧引擎不支持热恢复, 需 spawn 新任务.
        let is_http = t.engine == "speed";
        let pc_opt = t.pause_controller.lock().await.clone();
        if is_http {
            if let Some(pc) = pc_opt {
                // ★ 重新应用当前授权档位 (2026-10-03)。
                //   用户可能在暂停期间换了密钥 (付费↔免费), 而引擎是下载启动时
                //   一次性建好的、resume 复用旧引擎 —— 不在这里重设的话切档不生效。
                //   实测过: 暂停后切到免费档再继续, 速度毫无变化, 就是这个原因。
                let ratio = crate::licensing::speed_ratio();
                if let Some(cap) = t.speed_cap.lock().await.clone() {
                    cap.set_ratio(ratio);
                    crate::app_logger::log_pause(
                        "RESUME_LICENSE",
                        &format!("task_id={} 限速比例更新为 {:.0}%", task_id, ratio * 100.0),
                    );
                }
                pc.resume();
                // workers 已被唤醒, 不需要重新 spawn
                t.finished_notified.store(false, std::sync::atomic::Ordering::Relaxed);
                *t.state.lock().await = "running".into();
                crate::app_logger::log_pause("RESUMED", &format!("task_id={} engine=http (in-place 唤醒)", task_id));
                return Ok(());
            }
            // PauseController 已释放 (引擎完成或失败), 走 spawn 路径
            crate::app_logger::log_pause("RESUME_SPAWN", &format!("task_id={} engine=http PauseController 已释放, 重新 spawn", task_id));
        }
        // BT 或 HTTP 无 PauseController: 走 spawn_download 重新启动
        t.cancel_flag.store(false, std::sync::atomic::Ordering::Relaxed);
        *t.state.lock().await = "running".into();
        t.finished_notified.store(false, std::sync::atomic::Ordering::Relaxed);
        crate::downloader::spawn_download(app, t).await;
        crate::app_logger::log_pause("RESUMED", &format!("task_id={} engine={} (重新 spawn)", task_id, if is_http { "http" } else { "bt" }));
    }
    Ok(())
}

#[tauri::command]
pub async fn retry_download(task_id: String, app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    // 前端停滞检测自动恢复: 中止当前 future, 重置状态, 重新 spawn_download
    let task = {
        let m = state.download_tasks.lock().await;
        m.get(&task_id).cloned()
    };
    if let Some(t) = task {
        // 中止当前下载 future
        if let Some(h) = t.handle.lock().await.take() {
            h.abort();
        }
        t.cancel_flag.store(false, std::sync::atomic::Ordering::Relaxed);
        *t.state.lock().await = "running".into();
        t.finished_notified.store(false, std::sync::atomic::Ordering::Relaxed);
        crate::downloader::spawn_download(app, t).await;
    }
    Ok(())
}

#[tauri::command]
pub async fn get_downloads_status(state: State<'_, AppState>) -> Result<Vec<Value>, String> {
    let m = state.download_tasks.lock().await;
    let mut out = Vec::with_capacity(m.len());
    for (_, t) in m.iter() {
        out.push(t.status().await);
    }
    Ok(out)
}

#[tauri::command]
pub async fn get_speed_engine_status(state: State<'_, AppState>) -> Result<Vec<Value>, String> {
    // 前端轮询主数据源: 先快速 clone Arc 引用释放 map 锁, 再逐个 build status
    // (旧实现持锁期间逐个 .status().await 阻塞 start_download/pause 等命令 → 卡死)
    let tasks: Vec<Arc<DownloadTask>> = {
        let m = state.download_tasks.lock().await;
        m.values().cloned().collect()
    };
    let mut out = Vec::with_capacity(tasks.len());
    for t in &tasks {
        out.push(t.status().await);
    }
    Ok(out)
}

#[tauri::command]
pub fn aria2_status() -> Result<bool, String> {
    Ok(true)
}
#[tauri::command]
pub fn aria2_start() -> Result<(), String> { Ok(()) }
#[tauri::command]
pub fn aria2_stop() -> Result<(), String> { Ok(()) }
#[tauri::command]
pub fn aria2_get_active_downloads() -> Result<Vec<Value>, String> { Ok(vec![]) }

// ===================== 快照 / 缓存 =====================
#[tauri::command]
pub fn clear_cache(state: State<'_, AppState>) -> Result<String, String> { Ok(state.cache.clear()) }

// ===================== 缓存清理 (设置页 2026-10-01) =====================

/// 临时缓存目录: BT 下载前解析的种子/元数据等临时产物
pub fn temp_cache_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("vortexdl_temp_cache");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 运行日志目录 (exe 同级 logs/)
fn logs_dir_path() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    exe_dir.join("logs")
}

/// 递归计算目录/文件占用 (字节)
fn path_size(path: &Path) -> u64 {
    if path.is_file() {
        return path.metadata().map(|m| m.len()).unwrap_or(0);
    }
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(path) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() { total += path_size(&p); }
            else if let Ok(m) = e.metadata() { total += m.len(); }
        }
    }
    total
}

/// 系统临时目录中属于本程序的临时项 (vortexdl_* / opensteamtool_extract_*)
fn scan_system_temp() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("vortexdl_") || name.starts_with("opensteamtool_extract_") {
                out.push(e.path());
            }
        }
    }
    out
}

/// 读取临时缓存占用 (供设置页展示)
#[tauri::command]
pub fn get_cache_usage() -> Result<Value, String> {
    let mut temp_bytes = path_size(&temp_cache_dir());
    for p in scan_system_temp() { temp_bytes += path_size(&p); }
    let logs_bytes = path_size(&logs_dir_path());
    Ok(json!({ "temp_bytes": temp_bytes, "logs_bytes": logs_bytes }))
}

/// 清理临时缓存: BT 解析临时产物 + 运行日志 + 本程序系统临时文件
#[tauri::command]
pub fn clear_temp_cache() -> Result<Value, String> {
    let mut freed = 0u64;
    let mut items = 0u32;

    // 1. BT 解析临时目录 (整目录删除后重建)
    let bt_dir = temp_cache_dir();
    freed += path_size(&bt_dir);
    if std::fs::remove_dir_all(&bt_dir).is_ok() { items += 1; }
    let _ = std::fs::create_dir_all(&bt_dir);

    // 2. 运行日志 — 截断内容而非删除文件 (日志句柄仍持有, 删除会导致句柄失效)
    let logs = logs_dir_path();
    freed += path_size(&logs);
    if let Ok(rd) = std::fs::read_dir(&logs) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() && std::fs::OpenOptions::new().write(true).truncate(true).open(&p).is_ok() {
                items += 1;
            }
        }
    }

    // 3. 系统临时目录中的本程序临时文件
    for p in scan_system_temp() {
        let sz = path_size(&p);
        let ok = if p.is_dir() { std::fs::remove_dir_all(&p).is_ok() } else { std::fs::remove_file(&p).is_ok() };
        if ok { freed += sz; items += 1; }
    }

    crate::app_logger::log_cache("TEMP_CLEAR", &format!("freed={} items={}", freed, items));
    Ok(json!({ "freed_bytes": freed, "items": items }))
}

/// 清理软件缓存: 翻译缓存 + 通用内存缓存 + 快照
#[tauri::command]
pub fn clear_app_cache(state: State<'_, AppState>) -> Result<Value, String> {
    state.translator.clear_cache();          // 翻译缓存 (game_translator 全局 store)
    let cache_msg = state.cache.clear();     // 通用内存缓存 (搜索结果等)
    let snap_msg = state.snapshot.clear();   // 浏览快照
    crate::app_logger::log_cache("APP_CLEAR", &format!("{}; {}", cache_msg, snap_msg));
    Ok(json!({ "ok": true, "detail": format!("{}; {}", cache_msg, snap_msg) }))
}

/// 统计各源的游戏数量。
///
/// ★ 改成 `async fn` (2026-10-03): 它原本是**同步命令**, 而 Tauri 的同步命令跑在
///   主线程上 → 直接冻结 UI。而 `total_counts()` 要遍历整个缓存
///   (1670 页 byrut + 96 页 koyso 的全部卡片) 建 HashSet 去重, 启动时缓存刚灌满,
///   这一下就是几十万张卡片的遍历 —— 用户感受到的就是"刚打开软件卡死一下"。
///   前端 `hotRefreshIndexOnStart` 还会连调它两次, 叠加更明显。
///   `async fn` 由 Tauri 调度到线程池, 不再占用主线程。
///
/// 注意: 前端拿到的 JSON 结构未变, 契约不受影响。
#[tauri::command]
pub async fn snapshot_stats(state: State<'_, AppState>) -> Result<Value, String> {
    // 前端契约: [byCount, koCount, gxCount, total, byMem, koMem]
    // 数量 = max(内存 unique appid 数, 分页导航估算总数) — 预热完成后即为真实总量
    let (by, ko) = state.search.total_counts();
    // ★ 原来硬编码 0；现在 GX 已并入成人页，计数要真实
    let gx = crate::gx::total_cards();
    let total = by + ko + gx;
    Ok(json!([by, ko, gx, total, by, ko]))
}

#[tauri::command]
pub fn snapshot_categories(source: String, state: State<'_, AppState>) -> Result<Vec<String>, String> {
    Ok(state.snapshot.categories(&source))
}

#[tauri::command]
pub async fn refresh_snapshot(state: State<'_, AppState>) -> Result<String, String> {
    // 热刷新统计: snapshot_stats 读的是实时数据, 无需清缓存重建
    // (旧实现调用 search.refresh() 会清空索引并删除 game_index.json,
    //  导致每次启动全量重爬 + 翻译/标签全部丢失, 已移除)
    let (by, ko) = state.search.total_counts();
    Ok(format!("by={by},ko={ko}"))
}

#[tauri::command]
pub async fn preload_byko(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    // 阶段1 (同步): 本地索引已恢复则数量即时就绪; 首次启动抓首页+导航解析总量
    state.search.preload_byko().await?;
    let _ = app.emit("preload-ready", ());
    // 阶段2 (后台):
    //   - 本地索引完整 → 只做增量检查 (爬最新 1-2 页, 新游戏合并进缓存)
    //   - 本地无/不全 → 全量预热所有资源页 (byrut ~1650 页 + koyso)
    // payload: {by: 新增数, ko: 新增数} — 前端发现新增时提示 "发现 N 款新游戏"
    let engine = state.search.clone();
    // ★ 修复 (2026-10-02): 首屏预热 —— 立即把第 1 页灌进内存缓存。
    //
    //   用户反馈"普通游戏加载太慢"。原因: 下面阶段2 的 incremental_update()
    //   会无条件 fetch_byrut_force(1) 重抓第 1 页 (force 绕过缓存), 而用户点击
    //   首屏又触发 browse(1) 再抓一次 —— 两次网络 + 抢同一把缓存锁, 首屏要等数秒。
    //   这里先并行预热, 用户的 browse(1) 就能直接读内存 (磁盘索引已有第 1 页时零网络)。
    {
        let engine_warm = engine.clone();
        tokio::spawn(async move {
            engine_warm.warm_first_page().await;
        });
    }
    // ★ 修复 (2026-10-02): 标签补爬改为**与预热并行**独立启动。
    //
    //   旧实现把 retag_all() 串行排在阶段2 之后, 而阶段2 在已有本地索引时要跑
    //   增量更新 (byrut ~1671 页 + koyso 95 页), 耗时可长达十几分钟。结果是:
    //   成人页 (按 "成人游戏" 标签筛选本地缓存) 在整个预热跑完之前**一直是空的**,
    //   用户看到的就是「该分类暂无资源」——而爬取其实只是还没轮到。
    //
    //   现在标签爬取自己一个任务, 不受预热进度影响; 两者写的是同一份共享缓存,
    //   save_index_to_disk 每次都是完整快照, 不会丢卡片。
    {
        let engine_retag = engine.clone();
        tokio::spawn(async move {
            engine_retag.retag_all().await;
        });
    }
    tokio::spawn(async move {
        // 防重入: 前端 STEP14.5 与 STEP18 都会调用 preload_byko,
        // 第二次调用直接跳过, 避免与正在进行的全量预热并发竞争
        if !engine.try_begin_preload_spawn() { return; }
        let (by_new, ko_new) = if engine.has_disk_index_pub() {
            engine.incremental_update().await
        } else {
            engine.preload_all_pages().await;
            (0usize, 0usize)
        };
        let _ = app.emit("preload-all-done", serde_json::json!({ "by": by_new, "ko": ko_new }));
    });
    Ok(())
}

#[tauri::command]
pub fn clear_snapshot(state: State<'_, AppState>) -> Result<String, String> { Ok(state.snapshot.clear()) }

#[tauri::command]
pub fn reset_adult_tags(state: State<'_, AppState>) -> Result<String, String> { Ok(state.snapshot.reset_adult_tags()) }

/// 推荐游戏。
///
/// ★ 改成 `async fn` (2026-10-03): 与 `snapshot_stats` 同理 —— 同步命令跑在主线程,
///   而 `recommend` / `recommend_by_tags` 要从全量缓存里筛卡片 (几十万张),
///   首页渲染时会调用, 主线程一卡就是肉眼可见的停顿。`async fn` 交给线程池。
#[tauri::command]
pub async fn recommend(
    appid: Option<String>,
    category: Option<String>,
    limit: Option<u32>,
    count: Option<u32>,
    state: State<'_, AppState>,
) -> Result<Vec<GameCard>, String> {
    let n = limit.or(count).unwrap_or(10) as usize;
    // ★ 修改 (2026-09-13): 基于 appid 查找当前游戏标签, 推荐同标签游戏
    //   无 appid 或找不到游戏时, 回退到全量推荐 (ko 优先)
    if let Some(id) = appid {
        let recs = state.search.recommend_by_tags(&id, n);
        if !recs.is_empty() {
            return Ok(recs);
        }
    }
    Ok(state.search.recommend(n))
}

// ===================== 剪贴板 =====================
#[tauri::command]
pub fn copy_to_clipboard(text: String) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // 使用 clip.exe (Windows 内置) 将文本写入剪贴板
        let mut cmd = std::process::Command::new("cmd");
        cmd.creation_flags(0x08000000u32); // CREATE_NO_WINDOW
        cmd.args(["/C", "echo", &text, "|", "clip"]);
        // echo 会追加换行; 改用 PowerShell Set-Clipboard 更精确
        let mut ps = std::process::Command::new("powershell");
        ps.creation_flags(0x08000000u32);
        ps.args(["-NoProfile", "-Command", &format!("Set-Clipboard -Value '{}'", text.replace('\'', "''"))]);
        ps.output().map_err(|e| e.to_string())?;
        return Ok(());
    }
    #[cfg(not(windows))]
    {
        let _ = text;
        Err("clipboard not supported on this platform".into())
    }
}

// ===================== 文件系统 =====================
#[tauri::command]
pub fn open_folder(path: String) -> Result<(), String> {
    let mut cmd = std::process::Command::new("explorer");
    #[cfg(windows)] { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000u32); }
    cmd.arg(path).spawn().map_err(|e| e.to_string())?; Ok(())
}

/// ★ 删除一个文件或目录 (2026-10-03)。
///
/// 场景: 解压失败时 `delete_archive` 不会执行 (只在成功时删源包), 于是那个坏掉的
/// 压缩包就留在磁盘上 —— 用户既用不了也删不掉 (软件里根本没有删除入口)。
///
/// 带重试: 下载引擎/解压子进程刚退出时, 句柄可能还没完全释放, 直接删会报
/// "文件正在被另一个进程使用"。退避重试几次即可。
#[tauri::command]
pub fn delete_path(path: String) -> Result<(), String> {
    let p = Path::new(&path);
    if !p.exists() {
        return Ok(()); // 已经不在了, 视为成功
    }
    let mut last_err = String::new();
    for attempt in 0..5u32 {
        let r = if p.is_dir() {
            std::fs::remove_dir_all(p)
        } else {
            // 先清只读属性 (从压缩包里解出来的文件常带只读位)
            if let Ok(md) = std::fs::metadata(p) {
                let mut perm = md.permissions();
                if perm.readonly() {
                    perm.set_readonly(false);
                    let _ = std::fs::set_permissions(p, perm);
                }
            }
            std::fs::remove_file(p)
        };
        match r {
            Ok(()) => {
                crate::app_logger::log_command("delete_path", &format!("ok path={}", path));
                return Ok(());
            }
            Err(e) => {
                last_err = e.to_string();
                std::thread::sleep(std::time::Duration::from_millis(200 * (attempt as u64 + 1)));
            }
        }
    }
    crate::app_logger::log_command("delete_path", &format!("failed path={} err={}", path, last_err));
    Err(format!("删除失败: {} (文件可能仍被占用)", last_err))
}

#[tauri::command]
pub fn open_file(path: String) -> Result<(), String> {
    let p = Path::new(&path);
    let mut cmd = std::process::Command::new("explorer");
    #[cfg(windows)] { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000u32); }

    // 1) 路径是目录 → 直接打开该目录 (浏览内容), 而不是 /select 选中目录本身
    if p.is_dir() {
        cmd.arg(&path).spawn().map_err(|e| e.to_string())?;
        return Ok(());
    }

    // 2) 路径是存在的文件 → explorer /select,"path" 在父目录里选中该文件
    if p.exists() {
        let select_arg = format!("/select,\"{}\"", p.to_string_lossy());
        cmd.arg(&select_arg).spawn().map_err(|e| e.to_string())?;
        return Ok(());
    }

    // 3) 路径不存在 → 尝试去掉压缩包扩展名后作为目录打开 (byrut BT 下载目录 = 去扩展名的 file_path)
    //    例: 前端传来 D:\game\GameName.zip, 但 BT 实际下载目录是 D:\game\GameName (去掉了 .zip)
    {
        const EXTS: &[&str] = &[".torrent", ".zip", ".rar", ".7z", ".tar", ".gz", ".bz2"];
        let lower = path.to_lowercase();
        for ext in EXTS {
            if lower.ends_with(ext) {
                let stripped = path[..path.len() - ext.len()].to_string();
                let stripped_p = Path::new(&stripped);
                if stripped_p.is_dir() {
                    cmd.arg(&stripped).spawn().map_err(|e| e.to_string())?;
                    return Ok(());
                }
                break;
            }
        }
    }

    // 4) 路径不存在 (可能解压后已删除原压缩包) → 打开父目录, 没有父目录就回退到桌面
    let parent = p.parent().filter(|par| par.is_dir());
    if let Some(par) = parent {
        cmd.arg(par).spawn().map_err(|e| e.to_string())?;
        return Ok(());
    }

    // 兜底: 打开 "此电脑" (用户至少能看到 Explorer, 不会无反应)
    cmd.spawn().map_err(|e| e.to_string())?;
    Ok(())
}

// 仅检测本机程序是否存在 (用于 DLSS5 页面展示安装状态), 不启动进程
#[tauri::command]
pub fn program_exists(path: String) -> bool {
    let expanded = expand_env_vars(&path);
    Path::new(&expanded).exists()
}

/// 展开 %VAR% 形式的环境变量 (Windows 环境变量名大小写不敏感)
fn expand_env_vars(path: &str) -> String {
    let mut out = path.to_string();
    for var in ["LOCALAPPDATA", "APPDATA", "PROGRAMFILES", "PROGRAMFILES(X86)", "PROGRAMDATA", "USERPROFILE", "TEMP"] {
        if let Ok(val) = std::env::var(var) {
            out = out.replace(&format!("%{}%", var), &val);
        }
    }
    out
}

// ===================== 启动本地程序 =====================
// 资源导航里"启动软件"用: 直接拉起本机已安装的程序 (如 DLSS 5 Swapper)
// path 支持 %LOCALAPPDATA% 等环境变量; 不存在则返回 Err, 前端回退到下载页
#[tauri::command]
pub fn launch_program(path: String) -> Result<(), String> {
    let expanded = expand_env_vars(&path);
    let p = Path::new(&expanded);
    if !p.exists() {
        return Err(format!("程序未安装: {}", expanded));
    }
    let mut cmd = std::process::Command::new(&expanded);
    #[cfg(windows)] { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000u32); }
    if let Some(dir) = p.parent() { cmd.current_dir(dir); }
    cmd.spawn().map_err(|e| e.to_string())?;
    Ok(())
}

/// 桌面快捷方式让用户自己选主程序时，列出来的一个候选。
#[derive(serde::Serialize, Clone)]
pub struct ExeCandidate {
    pub path: String,
    /// 文件名（界面主标题）
    pub name: String,
    /// 相对解压目录的路径（界面副标题，用来区分同名 exe）
    pub rel: String,
    pub size: u64,
    pub has_icon: bool,
    pub score: i64,
    /// 自动推断会挑中的那一个（界面上标成「推荐」并默认选中）
    pub auto: bool,
    /// 名字（含相对路径）和提示名对得上 —— 只有这种才算"有把握"，见 `auto` 的赋值
    #[serde(skip)]
    pub name_hit: bool,
}

/// 递归找 exe，并按「像不像游戏主程序」打分排序。
///
/// 返回 `(候选按分降序, 是否共享根目录, 被共享根规则淘汰的条数)`。
/// ★ 抽出来是为了让 `create_desktop_shortcut`（自动挑）和
///   `list_exe_candidates`（弹窗让用户自己挑）用**同一套打分**，
///   否则「自动挑的」和「用户看到的推荐项」会对不上。
/// 给解压目录里的 exe 打分排序。
///
/// `drop_shared_root`：共享根目录（比如平铺解压的 `D:\game`，下面躺着几十个游戏）
/// 下名字对不上的 exe 是否直接淘汰。
///   · 自动创建快捷方式 → `true`。宁可报错也**不能指到别的游戏**。
///   · 弹窗让用户自己挑   → `false`。用户是主动选的、旁边还开着资源管理器，
///     把列表清空反而让人没得选。
fn rank_exe_candidates(
    target_dir: &Path,
    hint_name: &str,
    drop_shared_root: bool,
) -> (Vec<ExeCandidate>, bool, usize) {
    let dir_name = target_dir.file_name().and_then(|n| n.to_str()).unwrap_or("").to_lowercase();
    let game_name = hint_name.to_lowercase();
    let want_norm = norm_name(hint_name);

    let mut all_exes: Vec<(PathBuf, i64)> = Vec::new();
    collect_exes_recursive(target_dir, &mut all_exes, 0, 5);
    if all_exes.is_empty() {
        return (Vec::new(), false, 0);
    }

    // ★ 判断这是不是「共享根目录」（比如 D: 盘 game 目录下面躺着几十个游戏）。
    //   是的话就**不能**按体积/图标挑 —— 那会挑到别的游戏去。
    let mut top_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (p, _) in &all_exes {
        if let Ok(rel) = p.strip_prefix(target_dir) {
            let mut it = rel.components();
            if let Some(first) = it.next() {
                if it.next().is_some() {
                    top_dirs.insert(first.as_os_str().to_string_lossy().to_lowercase());
                }
            }
        }
    }
    let shared_root = top_dirs.len() >= 2;

    let mut out: Vec<ExeCandidate> = Vec::new();
    let mut dropped = 0usize;
    for (exe_path, size) in &all_exes {
        let name = exe_path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_lowercase();
        let mut score = *size;

        // 排除安装/卸载/配置程序
        if name.contains("setup") || name.contains("installer")
            || name.contains("uninstall") || name.contains("unins")
            || name.contains("配置") || name.contains("设置")
            || name.contains("config") || name.contains("update")
            || name.contains("patch") || name.contains("crack") {
            score -= 10_000_000_000;
        }

        // 排除引擎/工具类 exe（UE/Unity 的编辑器与辅助程序体积也很大）
        if name.contains("engine") || name.contains("unrealeditor")
            || name.contains("ue4editor") || name.contains("ue5editor")
            || name.contains("editor") || name.contains("shader")
            || name.contains("compiler") || name.contains("material")
            || name.contains("texture") || name.contains("mesh")
            || name.contains("crashreporter") || name.contains("symbol")
            || name.contains("tool") || name.contains("utility")
            || name.contains("crash") || name.contains("handler")
            || name.contains("report") || name.contains("proxy")
            || name.contains("helper") || name.contains("unitycrash")
            || name.contains("redist") || name.contains("prereq")
            || name.contains("vc_redist") || name.contains("dxsetup")
            || name.contains("validator") || name.contains("benchmark")
            || name.contains("eula") || name.contains("register") {
            score -= 8_000_000_000;
        }

        // 有内嵌图标的 exe 优先（游戏主程序通常带图标）
        let icon = exe_has_icon(exe_path);
        if icon {
            score += 30_000_000_000;
        } else {
            score -= 20_000_000_000;
        }

        // UE 游戏主程序通常以 -Win64-Shipping.exe 结尾
        if name.ends_with("-win64-shipping.exe") || name.ends_with("-win64-shipping") {
            score += 80_000_000_000;
        }
        // 与目录名同名 → 最高优先
        if !dir_name.is_empty() && name.starts_with(&dir_name) {
            score += 100_000_000_000;
        }

        let depth = exe_path
            .strip_prefix(target_dir)
            .map(|rel| rel.components().count() as i64)
            .unwrap_or(0);

        let mut name_hit = false;
        if !game_name.is_empty() && name.contains(&game_name) {
            score += 50_000_000_000;
            name_hit = true;
        }

        // ★ 关键：用**相对路径**（含中间子目录名）和快捷方式名做归一化比较。
        //   共享根目录下这是唯一可靠的信号 —— 体积和图标都靠不住。
        let rel_norm = exe_path
            .strip_prefix(target_dir)
            .map(|r| norm_name(&r.to_string_lossy()))
            .unwrap_or_default();
        let stem_norm = norm_name(exe_path.file_stem().and_then(|s| s.to_str()).unwrap_or(""));
        if want_norm.len() >= 3 {
            if rel_norm.contains(&want_norm) {
                score += 300_000_000_000;
                name_hit = true;
            } else if stem_norm.len() >= 3
                && (stem_norm.contains(&want_norm) || want_norm.contains(&stem_norm))
            {
                score += 150_000_000_000;
                name_hit = true;
            }
        }

        // ★ 共享根目录：名字对不上、又不在根目录这一层的，直接淘汰。
        //   宁可不建快捷方式，也不能建一个指向别的游戏的。
        if shared_root && drop_shared_root && !name_hit && depth > 1 {
            dropped += 1;
            continue;
        }

        if *size < 1_000_000 {
            score -= 5_000_000_000;
        }
        score -= depth * 1_000_000_000;

        out.push(ExeCandidate {
            path: exe_path.to_string_lossy().to_string(),
            name: exe_path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string(),
            rel: exe_path
                .strip_prefix(target_dir)
                .map(|r| r.to_string_lossy().to_string())
                .unwrap_or_default(),
            size: (*size).max(0) as u64,
            has_icon: icon,
            score,
            auto: false,
            name_hit,
        });
    }
    out.sort_by(|a, b| b.score.cmp(&a.score));
    let single = out.len() == 1;
    if let Some(first) = out.first_mut() {
        // ★ 「推荐」只在**有把握**时给：
        //   · 名字对得上（含相对路径命中）→ 明确就是这个游戏；
        //   · 不是共享根目录 → 单游戏目录，按分数挑出来的就是它；
        //   · 只有一个候选 → 没得选。
        //   共享目录 + 名字对不上时**不给**推荐 —— 否则会推荐另一个游戏的 exe
        //   （实测 D:\game 下提示名对不上时推荐了 `Hachishaku-Win64-Shipping.exe`），
        //   用户很容易一路点下去建出一个指错游戏的快捷方式。
        first.auto = first.name_hit || !shared_root || single;
    }
    (out, shared_root, dropped)
}

/// 列出解压目录里的候选主程序，给「让用户自己选」的弹窗用。
#[tauri::command]
pub fn list_exe_candidates(dir: String, hint_name: Option<String>) -> Result<Vec<ExeCandidate>, String> {
    let p = PathBuf::from(&dir);
    if !p.is_dir() {
        return Err(format!("{} 不是一个目录", dir));
    }
    let hint = hint_name.unwrap_or_default();
    // 用户在挑 → 不淘汰共享根目录里的候选（列表可能很长，截断到 80 条）
    let (mut cands, _shared, _dropped) = rank_exe_candidates(&p, &hint, false);
    cands.truncate(80);
    Ok(cands)
}

// ===================== 创建桌面快捷方式 =====================
// 在桌面创建 .lnk 快捷方式, 指向解压目录中自动匹配到的最佳 exe
// 参数:
//   target_path: 解压输出目录 (或 .exe 路径)
//   shortcut_name: 快捷方式显示名称 (不含 .lnk 扩展名)
#[tauri::command]
pub fn create_desktop_shortcut(target_path: String, shortcut_name: String) -> Result<(), String> {
    #[cfg(windows)]
    {
        let desktop = dirs::desktop_dir()
            .or_else(|| std::env::var("USERPROFILE").ok().map(|p| PathBuf::from(p).join("Desktop")))
            .ok_or_else(|| "无法定位桌面路径".to_string())?;
        let safe_name: String = shortcut_name.chars().map(|c| {
            match c {
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
                _ => c,
            }
        }).collect();
        if safe_name.is_empty() {
            return Err("快捷方式名称不能为空".to_string());
        }
        let lnk_path = desktop.join(format!("{}.lnk", safe_name));

        let target_p = PathBuf::from(&target_path);

        // ★ 递归搜索解压目录里所有 exe, 自动匹配最符合的
        //   评分规则:
        //     + 目录名同名 → +100亿 (最高优先)
        //     + 名字包含游戏名 → +50亿
        //     - setup/installer/uninstall/配置/设置 → -100亿 (排除安装/卸载程序)
        //     - 体积小于 1MB → -50亿 (可能是辅助工具)
        //     + 体积越大分数越高 (主程序通常最大)
        // 主程序：目录 → 用同一套打分挑分最高的那个；文件 → 必须是 exe。
        let (final_target, working_dir) = if target_p.is_dir() {
            let (cands, shared_root, dropped) = rank_exe_candidates(&target_p, &safe_name, true);
            match cands.first() {
                Some(c) => {
                    let wd = Path::new(&c.path)
                        .parent()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default();
                    (c.path.clone(), wd)
                }
                // ★ 找不到就**明确报错**，不能静默 Ok(())。
                None => {
                    return Err(if shared_root {
                        format!(
                            "{} 看起来是个共享目录（下面有多个子目录含 exe，另有 {} 个 exe 因名字对不上被排除），但没有 exe 的名字和「{}」对得上；为避免指到别的游戏，没有创建快捷方式",
                            target_p.display(),
                            dropped,
                            safe_name
                        )
                    } else {
                        format!("在 {} 里没找到合适的游戏主程序，未创建快捷方式", target_p.display())
                    })
                }
            }
        } else {
            // target_path 是文件: 仅当是 exe 时才创建快捷方式
            let ext = target_p.extension().and_then(|e| e.to_str()).map(|s| s.to_lowercase()).unwrap_or_default();
            if ext != "exe" {
                return Ok(());
            }
            (target_path.clone(),
             target_p.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default())
        };

        // 用 PowerShell + **显式 IShellLinkW/IPersistFile** 创建 .lnk。
        //
        // ★★ 为什么不用 `New-Object -ComObject WScript.Shell`（2026-10-08 修）：
        //   那条路是 IDispatch 后期绑定，字符串按 **ANSI 代码页**编组，
        //   路径里只要有代码页表示不了的字符（实测游戏目录名 `らぶらぶ♥プリンセス`
        //   里的 ♥ U+2665）就会被写坏，`$s.Save()` 抛
        //   `System.ArgumentException: 值不在预期的范围内。`
        //   —— 用户看到的就是"解压完了但桌面没有快捷方式"（SC_FAIL）。
        //   实测对照：纯中文路径 OK、含 ♥ 路径 WScript.Shell 失败、同一路径改用
        //   下面的 IShellLink 接口 **成功**。强类型接口按 LPWStr(UTF-16) 编组，不受代码页影响。
        //
        // ★ 路径一律走**环境变量**传，不再拼进脚本字符串 —— 免得路径里的引号/反斜杠
        //   /全角字符和 PowerShell 的引号规则打架。
        const CS_DEF: &str = r#"
using System;
using System.Runtime.InteropServices;
using System.Text;
[ComImport, Guid("00021401-0000-0000-C000-000000000046")] class ShellLinkCoClass { }
[ComImport, Guid("000214F9-0000-0000-C000-000000000046"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IShellLinkW {
  void GetPath([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder p, int m, IntPtr d, int f);
  void GetIDList(out IntPtr p);
  void SetIDList(IntPtr p);
  void GetDescription([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder p, int m);
  void SetDescription([MarshalAs(UnmanagedType.LPWStr)] string s);
  void GetWorkingDirectory([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder p, int m);
  void SetWorkingDirectory([MarshalAs(UnmanagedType.LPWStr)] string s);
  void GetArguments([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder p, int m);
  void SetArguments([MarshalAs(UnmanagedType.LPWStr)] string s);
  void GetHotkey(out short k); void SetHotkey(short k);
  void GetShowCmd(out int c); void SetShowCmd(int c);
  void GetIconLocation([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder p, int m, out int i);
  void SetIconLocation([MarshalAs(UnmanagedType.LPWStr)] string s, int i);
  void SetRelativePath([MarshalAs(UnmanagedType.LPWStr)] string s, int r);
  void Resolve(IntPtr h, int f);
  void SetPath([MarshalAs(UnmanagedType.LPWStr)] string s);
}
[ComImport, Guid("0000010b-0000-0000-C000-000000000046"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IPersistFile {
  void GetClassID(out Guid g);
  [PreserveSig] int IsDirty();
  void Load([MarshalAs(UnmanagedType.LPWStr)] string f, int m);
  void Save([MarshalAs(UnmanagedType.LPWStr)] string f, bool r);
  void SaveCompleted([MarshalAs(UnmanagedType.LPWStr)] string f);
  void GetCurFile([MarshalAs(UnmanagedType.LPWStr)] out string f);
}
public static class VxLnk {
  public static void Make(string lnk, string target, string work, string icon) {
    var link = (IShellLinkW)new ShellLinkCoClass();
    link.SetPath(target);
    if (work.Length > 0) link.SetWorkingDirectory(work);
    if (icon.Length > 0) link.SetIconLocation(icon, 0);
    ((IPersistFile)link).Save(lnk, true);
  }
}
"#;
        let ps_script = format!(
            // ★ 先把 PowerShell 的输出编码设成 UTF-8：默认它按**控制台代码页**(中文系统 GBK)
            //   往 stderr 写错误，Rust 这边 from_utf8_lossy 出来就是乱码
            //   （实测 "ֵ����Ԥ�ڵķ�Χ�ڡ�"）。设了之后错误信息就是正常中文。
            "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; \
             $ErrorActionPreference='Stop'; \
             if (-not ('VxLnk' -as [type])) {{ Add-Type -TypeDefinition @'\n{}\n'@ -Language CSharp }}; \
             [VxLnk]::Make($env:VX_LNK, $env:VX_TARGET, $env:VX_WORKDIR, $env:VX_TARGET)",
            CS_DEF
        );
        let mut cmd = std::process::Command::new("powershell");
        cmd.arg("-NoProfile").arg("-NonInteractive").arg("-Command").arg(&ps_script);
        cmd.env("VX_LNK", &lnk_path)
            .env("VX_TARGET", &final_target)
            .env("VX_WORKDIR", &working_dir);
        #[cfg(windows)] { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000u32); }
        let output = cmd.output().map_err(|e| format!("创建快捷方式失败: {}", e))?;
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            return Err(format!("创建快捷方式失败: {}", err.trim()));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (target_path, shortcut_name);
        Err("桌面快捷方式仅支持 Windows 平台".to_string())
    }
}

/// 名字归一化：去掉空格/下划线/连字符/点，转小写。用于把「快捷方式名」和「exe 路径」做模糊比较。
/// 例：`Boundaries_of_Morality-0.600-pc` → `boundariesofmorality0600pc`
#[cfg(windows)]
fn norm_name(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// 递归收集目录内所有 .exe 文件 (带深度限制)
#[cfg(windows)]
fn collect_exes_recursive(dir: &std::path::Path, out: &mut Vec<(PathBuf, i64)>, depth: usize, max_depth: usize) {
    if depth > max_depth { return; }
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                collect_exes_recursive(&p, out, depth + 1, max_depth);
            } else if p.is_file() {
                let ext = p.extension().and_then(|e| e.to_str()).map(|s| s.to_lowercase()).unwrap_or_default();
                if ext == "exe" {
                    let size = entry.metadata().map(|m| m.len() as i64).unwrap_or(0);
                    out.push((p, size));
                }
            }
        }
    }
}

/// ★ 新增 (issue 4): 检测 exe 是否内嵌了图标资源 (PE 资源段 RT_GROUP_ICON=14 / RT_ICON=3)
///   纯 Rust 解析 PE, 不依赖任何外部库/FFI; 只读取文件头与资源段, 不会整文件加载
///   游戏主程序通常带图标, 而引擎工具/辅助程序 (Engine.exe / CrashHandler.exe 等) 往往没有图标
#[cfg(windows)]
fn exe_has_icon(path: &std::path::Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = match std::fs::File::open(path) { Ok(f) => f, Err(_) => return false };

    // 1) DOS 头 (e_lfanew @ 0x3C 指向 PE 头)
    let mut dos = [0u8; 64];
    if f.read_exact(&mut dos).is_err() || &dos[0..2] != b"MZ" { return false; }
    let pe_off = u32::from_le_bytes([dos[0x3C], dos[0x3D], dos[0x3E], dos[0x3F]]) as u64;

    // 2) PE 签名 + COFF 头
    if f.seek(SeekFrom::Start(pe_off)).is_err() { return false; }
    let mut sig = [0u8; 4];
    if f.read_exact(&mut sig).is_err() || &sig != b"PE\0\0" { return false; }
    let mut coff = [0u8; 20];
    if f.read_exact(&mut coff).is_err() { return false; }
    let num_sections = u16::from_le_bytes([coff[2], coff[3]]) as usize;
    let opt_size = u16::from_le_bytes([coff[16], coff[17]]) as usize;

    // 3) 可选头 → 取数据目录[2] = 资源目录 (RVA/Size)
    let opt_off = pe_off + 4 + 20;
    if f.seek(SeekFrom::Start(opt_off)).is_err() { return false; }
    let mut opt = vec![0u8; opt_size];
    if opt_size < 2 || f.read_exact(&mut opt).is_err() { return false; }
    let magic = u16::from_le_bytes([opt[0], opt[1]]);
    let dd_off = if magic == 0x10B { 96 } else if magic == 0x20B { 112 } else { return false; };
    let res_entry = dd_off + 2 * 8;
    if opt.len() < res_entry + 8 { return false; }
    let res_rva = u32::from_le_bytes([opt[res_entry], opt[res_entry+1], opt[res_entry+2], opt[res_entry+3]]) as u64;
    let res_size = u32::from_le_bytes([opt[res_entry+4], opt[res_entry+5], opt[res_entry+6], opt[res_entry+7]]) as u64;
    if res_rva == 0 || res_size == 0 { return false; }

    // 4) 段表 → 找到包含资源目录 RVA 的段, 换算文件偏移
    let sec_off = opt_off + opt_size as u64;
    let mut secs = vec![0u8; num_sections * 40];
    if f.seek(SeekFrom::Start(sec_off)).is_err() || f.read_exact(&mut secs).is_err() { return false; }
    let mut file_off = 0u64;
    for i in 0..num_sections {
        let s = &secs[i*40..i*40+40];
        let vsize = u32::from_le_bytes([s[8], s[9], s[10], s[11]]) as u64;
        let va = u32::from_le_bytes([s[12], s[13], s[14], s[15]]) as u64;
        let raw = u32::from_le_bytes([s[20], s[21], s[22], s[23]]) as u64;
        if res_rva >= va && res_rva < va + vsize {
            file_off = raw + (res_rva - va);
            break;
        }
    }
    if file_off == 0 { return false; }

    // 5) 读资源目录根 (最多 4MB), 第一层条目即资源类型 (RT_ICON=3 / RT_GROUP_ICON=14)
    let read_len = res_size.min(4 * 1024 * 1024) as usize;
    let mut buf = vec![0u8; read_len];
    if f.seek(SeekFrom::Start(file_off)).is_err() { return false; }
    let n = f.read(&mut buf).unwrap_or(0);
    if n < 16 { return false; }
    let named = u16::from_le_bytes([buf[12], buf[13]]) as usize;
    let id = u16::from_le_bytes([buf[14], buf[15]]) as usize;
    for i in 0..(named + id) {
        let e = 16 + i * 8;
        if n < e + 8 { break; }
        let name_id = u32::from_le_bytes([buf[e], buf[e+1], buf[e+2], buf[e+3]]);
        if name_id & 0x8000_0000 != 0 { continue; } // 具名条目, 跳过
        let type_id = name_id & 0xFFFF;
        if type_id == 3 || type_id == 14 { return true; }
    }
    false
}

#[tauri::command]
pub async fn extract_archive(
    app: AppHandle,
    task_id: Option<String>,
    archive_path: String,
    dest_dir: String,
    password: Option<String>,
    delete_archive: Option<bool>,
) -> Result<Value, String> {
    let tid = task_id.unwrap_or_default();
    let archive = PathBuf::from(&archive_path);
    let dest = PathBuf::from(&dest_dir);
    let app_clone = app.clone();

    // ★ 日志增强 (2026-09-15): 记录解压开始, 方便排查 dist overflow 等解压失败
    let ex_start_instant = std::time::Instant::now();
    let archive_size = std::fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
    eprintln!("[EX_START] task_id={} archive={} dest={} has_password={} archive_size={}",
        tid, archive.display(), dest.display(), password.is_some(), archive_size);
    // ★ tid 会被 spawn_blocking 的 move 闭包消费, 提前 clone 一份供 EX_FIN 日志使用
    let tid_for_log = tid.clone();

    // ★★★ 关键修复: 7z 解压是长时间 CPU/IO 密集操作, 必须用 spawn_blocking 放到阻塞线程池
    //   之前是同步 fn, 在 Tauri 主线程执行 → 冻结整个 UI (密码框不弹、进度条不动、应用无响应)
    let r = tokio::task::spawn_blocking(move || {
        crate::extractor::extract(
            &archive,
            &dest,
            password.as_deref(),
            move |percent, bytes_extracted, bytes_total, current_file| {
                let _ = app_clone.emit("extract_progress", json!({
                    "task_id": tid,
                    "percent": percent,
                    "bytes_extracted": bytes_extracted,
                    "bytes_total": bytes_total,
                    "current_file": current_file,
                }));
            },
        )
    }).await.map_err(|e| format!("解压线程异常: {}", e))?;

    // ★ 日志增强 (2026-09-15): 记录解压完成, 包含 success/error/output_dir/elapsed
    eprintln!("[EX_FIN] task_id={} success={} error={} output_dir={} elapsed_ms={}",
        tid_for_log, r.success, r.error, r.output_dir.display(), ex_start_instant.elapsed().as_millis());

    // 解压成功且用户要求删除原压缩包
    if r.success && delete_archive.unwrap_or(false) {
        let _ = std::fs::remove_file(&archive_path);
    }

    // ★ Bug 修复 (2026-09-15): 扁平化中文解压目录
    //   场景: 用户下载时用游戏中文名作为子目录 (如 D:\game\万词破 - 单词女友\),
    //   解压后压缩包内根目录是英文 (如 WordGirlgriend\), 实际游戏路径变成
    //   D:\game\万词破 - 单词女友\WordGirlgriend\ → 中文路径可能导致游戏运行 bug
    //   修复: 如果解压根目录名含中文且内部只有一个英文子目录, 把子目录提升到父级
    //   D:\game\万词破 - 单词女友\WordGirlgriend\ → D:\game\WordGirlgriend\
    let mut final_output_dir = r.output_dir.clone();
    if r.success {
        if let Some(flattened) = flatten_chinese_extract_dir(&r.output_dir) {
            eprintln!("[EX_FLATTEN] {} → {}", r.output_dir.display(), flattened.display());
            crate::app_logger::log_extract("FLATTEN", &format!(
                "from={} → to={}",
                r.output_dir.display(), flattened.display()
            ));
            final_output_dir = flattened;
        }
    }

    if r.success {
        Ok(json!({ "success": true, "output_dir": final_output_dir.to_string_lossy(), "error": "" }))
    } else {
        Ok(json!({ "success": false, "output_dir": final_output_dir.to_string_lossy(), "error": r.error }))
    }
}

/// ★ 扁平化中文解压目录: 如果解压根目录名含中文且只有一个英文子目录, 提升子目录
///   例: D:\game\万词破 - 单词女友\WordGirlgriend\ → D:\game\WordGirlgriend\
///   返回 Some(新路径) 如果执行了扁平化, None 如果不需要
#[cfg(windows)]
fn flatten_chinese_extract_dir(output_dir: &Path) -> Option<PathBuf> {
    // 1. 检查解压根目录名是否含中文
    let dir_name = output_dir.file_name()?.to_str()?;
    let has_chinese = dir_name.chars().any(|c| {
        let code = c as u32;
        // CJK 统一汉字范围 + 扩展A + 兼容汉字
        (0x4E00..=0x9FFF).contains(&code) || (0x3400..=0x4DBF).contains(&code) || (0xF900..=0xFAFF).contains(&code)
    });
    if !has_chinese { return None; }

    // 2. 读取解压目录内容, 检查是否只有一个子目录且无散落文件
    let entries: Vec<_> = std::fs::read_dir(output_dir).ok()?.flatten().collect();
    if entries.len() != 1 { return None; }
    let only_entry = &entries[0];
    if !only_entry.file_type().ok()?.is_dir() { return None; }

    // 3. 子目录名必须纯英文 (不含中文)
    let child_name = only_entry.file_name().to_string_lossy().to_string();
    let child_has_chinese = child_name.chars().any(|c| {
        let code = c as u32;
        (0x4E00..=0x9FFF).contains(&code) || (0x3400..=0x4DBF).contains(&code) || (0xF900..=0xFAFF).contains(&code)
    });
    if child_has_chinese { return None; }

    // 4. 计算扁平化后的目标路径: 父目录/子目录名
    let parent = output_dir.parent()?;
    let target = parent.join(&child_name);

    // 5. 如果目标路径已存在 (用户之前解压过同名游戏), 删除旧目录或跳过
    if target.exists() {
        // 尝试删除旧目录 (如果是空目录), 否则跳过扁平化
        if std::fs::remove_dir_all(&target).is_err() {
            eprintln!("[EX_FLATTEN] 目标已存在且非空, 跳过扁平化: {}", target.display());
            return None;
        }
    }

    // 6. 移动子目录到父目录下 (rename 跨目录可能失败, 用 copy+remove 兜底)
    if std::fs::rename(only_entry.path(), &target).is_err() {
        // rename 失败 (可能跨盘符), 用 copy + remove
        if let Err(e) = copy_dir_recursive(&only_entry.path(), &target) {
            eprintln!("[EX_FLATTEN] 复制子目录失败: {}", e);
            return None;
        }
        let _ = std::fs::remove_dir_all(only_entry.path());
    }

    // 7. 删除空的中文根目录
    let _ = std::fs::remove_dir(output_dir);

    Some(target)
}

#[cfg(not(windows))]
fn flatten_chinese_extract_dir(_output_dir: &Path) -> Option<PathBuf> { None }

/// 递归复制目录 (跨盘符 rename 失败时的兜底)
#[cfg(windows)]
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let p = entry.path();
        let name = entry.file_name();
        let target = dst.join(&name);
        if p.is_dir() {
            copy_dir_recursive(&p, &target)?;
        } else {
            std::fs::copy(&p, &target)?;
        }
    }
    Ok(())
}

/// 扫描指定目录(递归)下所有压缩包文件, 返回绝对路径数组
#[tauri::command]
pub fn list_archives(dir: String) -> Result<Value, String> {
    use walkdir::WalkDir;
    let base = PathBuf::from(&dir);
    if !base.exists() {
        return Ok(json!({ "archives": [] }));
    }
    let exts = ["zip", "rar", "7z", "tar", "gz", "bz2"];
    let mut archives: Vec<String> = Vec::new();
    for entry in WalkDir::new(&base).into_iter().flatten() {
        if entry.file_type().is_file() {
            if let Some(ext) = entry.path().extension().and_then(|e| e.to_str()) {
                if exts.contains(&ext.to_lowercase().as_str()) {
                    archives.push(entry.path().to_string_lossy().to_string());
                }
            }
        }
    }
    Ok(json!({ "archives": archives }))
}

/// ★ BT 下载磁盘进度检查: 扫描 BT 输出目录, 返回实际文件总大小
/// 用于前端当 BT 进度卡在 0 时, 检查磁盘上是否已有数据写入
#[tauri::command]
pub fn check_bt_disk_progress(dir: String) -> Result<Value, String> {
    use walkdir::WalkDir;
    let base = PathBuf::from(&dir);
    if !base.exists() {
        return Ok(json!({ "disk_bytes": 0, "file_count": 0, "exists": false }));
    }
    let mut total_bytes: u64 = 0;
    let mut file_count: u32 = 0;
    for entry in WalkDir::new(&base).into_iter().flatten() {
        if entry.file_type().is_file() {
            total_bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            file_count += 1;
        }
    }
    Ok(json!({ "disk_bytes": total_bytes, "file_count": file_count, "exists": true }))
}

#[tauri::command]
pub fn browse_path(_path: Option<String>) -> Result<String, String> {
    #[cfg(windows)]
    {
        // 调用 PowerShell 的 FolderBrowserDialog 弹出真实目录选择器
        // -NoProfile 加速启动, -NonInteractive 避免 PS 交互式提示
        // 输出 SelectedPath 到 stdout, 取第一行非空字符串作为结果
        let ps_script = r#"
Add-Type -AssemblyName System.Windows.Forms
$d = New-Object System.Windows.Forms.FolderBrowserDialog
$d.Description = '选择目录'
$d.ShowNewFolderButton = $true
$d.UseDescriptionForTitle = $true
$ok = $d.ShowDialog()
if ($ok -eq [System.Windows.Forms.DialogResult]::OK -and $d.SelectedPath) {
    Write-Output $d.SelectedPath
} else {
    Write-Output ''
}
"#;
        let mut cmd = std::process::Command::new("powershell");
        cmd.arg("-NoProfile").arg("-NonInteractive").arg("-Command").arg(ps_script);
        #[cfg(windows)] { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000u32); }
        let output = cmd.output().map_err(|e| format!("启动目录选择器失败: {}", e))?;
        let path = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string();
        if path.is_empty() {
            return Err("用户取消了选择".to_string());
        }
        Ok(path)
    }
    #[cfg(not(windows))]
    {
        let _ = _path;
        Err("当前平台不支持目录浏览".to_string())
    }
}

#[tauri::command]
pub fn get_game_translator_path() -> Result<String, String> {
    let base = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("resources").join("game_translator")))
        .unwrap_or_else(|| std::path::PathBuf::from("resources/game_translator"));
    if base.exists() { Ok(base.to_string_lossy().to_string()) }
    else { Ok(std::env::current_dir().map(|d| d.to_string_lossy().to_string()).unwrap_or_default()) }
}

// ===================== 应用信息 / 自更新 =====================
#[tauri::command]
pub fn get_app_info() -> Result<Value, String> {
    let version = env!("CARGO_PKG_VERSION").to_string();
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_string_lossy().to_string())).unwrap_or_default();
    let cache_dir = dirs::home_dir().map(|d| d.join(".playzip").to_string_lossy().to_string()).unwrap_or_default();
    Ok(json!({ "version": version, "exe_dir": exe_dir, "cache_dir": cache_dir }))
}

#[tauri::command]
pub fn mark_version_seen() -> Result<(), String> {
    let mut cfg = Config::load();
    cfg.last_seen_version = env!("CARGO_PKG_VERSION").to_string();
    cfg.save()
}

#[tauri::command]
pub async fn check_app_update() -> Result<crate::updater::AppUpdateResult, String> {
    Ok(crate::updater::check_app_update().await)
}

#[tauri::command]
pub async fn download_app_update(update: Option<serde_json::Value>) -> Result<String, String> {
    let _ = update; // 前端会传 update 对象, 后端自行 check_app_update 获取最新信息
    let r = crate::updater::check_app_update().await;
    if !r.has_update { return Ok("当前已是最新版本".to_string()); }
    crate::updater::download_and_install(r).await
}

// ===================== 系统监控 =====================
#[tauri::command]
pub async fn get_system_status() -> Result<Value, String> {
    crate::system_monitor::get_system_status().await
}

// ===================== 浏览器 =====================
#[tauri::command]
pub fn open_external_browser(url: String) -> Result<(), String> {
    crate::browser::open_external(&url)
}

// ===================== 运行库修复 =====================
/// 生成运行库修复的 PowerShell 脚本。
/// 由提权后的 powershell 执行, 依次: 静默安装 VC++ 运行库 → DirectX 修复 → 注册 DLL,
/// 最终把每一步结果以 JSON 写入 result_path。
fn build_repair_script(dir: &Path, result_path: &Path) -> String {
    let dir_s = dir.to_string_lossy().replace('\'', "''");
    let res_s = result_path.to_string_lossy().replace('\'', "''");
    format!(
        r#"$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'
$results = New-Object System.Collections.ArrayList

$dir = '{dir}'
if (Test-Path $dir) {{
  Get-ChildItem $dir -Filter *.exe | ForEach-Object {{
    $name = $_.Name
    $ok = $false; $msg = ''
    foreach ($combo in @(
        @('/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART','/SP-'),
        @('/SILENT','/SUPPRESSMSGBOXES','/NORESTART','/SP-'),
        @('/S','/NORESTART'),
        @('/install','/quiet','/norestart'),
        @('/quiet','/norestart'))) {{
      try {{
        $p = Start-Process -FilePath $_.FullName -ArgumentList $combo -Wait -PassThru -ErrorAction Stop
        if ($p.ExitCode -eq 0) {{ $ok = $true; $msg = '安装成功'; break }}
        else {{ $msg = "退出码 $($p.ExitCode)" }}
      }} catch {{ $msg = $_.Exception.Message }}
    }}
    [void]$results.Add(@{{ name = $name; success = $ok; message = $msg }})
  }}
}} else {{
  [void]$results.Add(@{{ name = '运行库目录'; success = $false; message = "目录不存在: $dir" }})
}}

$dx = 'F:\2种模块\系统修复工具\tools\dx\DirectX组件.exe'
if (Test-Path $dx) {{
  $ok = $false; $msg = ''
  foreach ($combo in @(@('/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART'), @('/S','/NORESTART'))) {{
    try {{
      $p = Start-Process -FilePath $dx -ArgumentList $combo -Wait -PassThru -ErrorAction Stop
      if ($p.ExitCode -eq 0) {{ $ok = $true; $msg = 'DirectX 组件修复完成'; break }}
      else {{ $msg = "退出码 $($p.ExitCode)" }}
    }} catch {{ $msg = $_.Exception.Message }}
  }}
  [void]$results.Add(@{{ name = 'DirectX修复'; success = $ok; message = $msg }})
}} else {{
  [void]$results.Add(@{{ name = 'DirectX修复'; success = $false; message = '未找到 DirectX 修复工具' }})
}}

$dlls = @('d3d9.dll','d3dx9_43.dll','xinput1_3.dll','msvcp140.dll','vcruntime140.dll')
$okc = 0
foreach ($d in $dlls) {{
  try {{
    $p = Start-Process -FilePath 'regsvr32.exe' -ArgumentList '/s', $d -Wait -PassThru -ErrorAction Stop
    if ($p.ExitCode -eq 0) {{ $okc++ }}
  }} catch {{}}
}}
[void]$results.Add(@{{ name = 'DLL注册'; success = $true; message = "已尝试注册 $okc 个常见系统 DLL" }})

$json = @{{ results = $results }} | ConvertTo-Json -Depth 5
[System.IO.File]::WriteAllText('{res}', $json, (New-Object System.Text.UTF8Encoding($false)))
"#,
        dir = dir_s,
        res = res_s
    )
}

/// 一键修复运行库: 安装 VC++ 运行库 + DirectX 修复 + 注册 DLL
/// 关键: 通过 UAC 提权执行 (部分运行库安装包要求管理员权限, 普通权限无法启动)
/// 采用「生成脚本 → 提权执行 → 读取结果」的方式, 只需一次 UAC, 且不阻塞 UI
#[tauri::command]
pub async fn repair_runtime(runtime_dir: Option<String>) -> Result<Value, String> {
    let dir = runtime_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Users\13961\Downloads\新建文件夹"));

    let tmp = std::env::temp_dir();
    let script_path = tmp.join("vortexdl_repair.ps1");
    let result_path = tmp.join("vortexdl_repair_result.json");
    let _ = std::fs::remove_file(&result_path);

    // 写入脚本 (带 UTF-8 BOM, 保证 PowerShell 正确读取中文路径)
    let script = build_repair_script(&dir, &result_path);
    let mut content = vec![0xEFu8, 0xBB, 0xBF];
    content.extend_from_slice(script.as_bytes());
    std::fs::write(&script_path, &content).map_err(|e| format!("写入修复脚本失败: {e}"))?;

    // 通过 UAC 提权运行脚本 (Start-Process -Verb RunAs 会弹出管理员授权)
    let script_arg = script_path.to_string_lossy().replace('\'', "''");
    let ps_cmd = format!(
        "Start-Process -FilePath 'powershell' -Verb RunAs -Wait -WindowStyle Hidden -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File','{}')",
        script_arg
    );

    let mut cmd = tokio::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &ps_cmd]);
    #[cfg(windows)] { cmd.creation_flags(0x08000000u32); }

    use tokio::time::{timeout, Duration};
    match timeout(Duration::from_secs(900), cmd.output()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(format!("启动提权修复失败: {e}")),
        Err(_) => return Err("运行库修复超时 (超过 15 分钟)".to_string()),
    }

    // 读取提权脚本写回的结果
    let text = std::fs::read_to_string(&result_path).map_err(|_| {
        "未获取到修复结果: 可能未通过管理员授权 (UAC) 或脚本执行失败".to_string()
    })?;
    let parsed: Value = serde_json::from_str(&text).map_err(|e| format!("解析修复结果失败: {e}"))?;
    let results = parsed.get("results").cloned().unwrap_or_else(|| json!([]));

    let all_ok = results
        .as_array()
        .map(|a| a.iter().all(|r| r.get("success").and_then(|v| v.as_bool()).unwrap_or(false)))
        .unwrap_or(false);

    Ok(json!({
        "success": all_ok,
        "results": results,
        "need_restart": true
    }))
}

/// 重启电脑 (用于运行库修复完成后)
#[tauri::command]
pub fn restart_system() -> Result<(), String> {
    let mut cmd = std::process::Command::new("cmd");
    cmd.arg("/c").arg("shutdown").arg("/r").arg("/t").arg("5").arg("/c").arg("运行库修复完成, 5秒后重启电脑");
    #[cfg(windows)] { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000u32); }
    cmd.spawn().map_err(|e| format!("重启命令执行失败: {e}"))?;
    Ok(())
}


// ===================== 自绘标题栏 (2026-10-06) =====================
// 关掉系统装饰 (decorations:false) 后, 最小化/最大化/关闭 由前端按钮调用。
#[tauri::command]
pub fn vx_window_min(window: tauri::Window) -> Result<(), String> {
    window.minimize().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn vx_window_toggle_max(window: tauri::Window) -> Result<(), String> {
    match window.is_maximized() {
        Ok(true) => window.unmaximize().map_err(|e| e.to_string()),
        _ => window.maximize().map_err(|e| e.to_string()),
    }
}

#[tauri::command]
pub fn vx_window_close(window: tauri::Window) -> Result<(), String> {
    window.close().map_err(|e| e.to_string())
}

// ===================== 游戏站抓取 (galgamex) =====================
// ★ 用 chromiumoxide 起真浏览器：这个站的下载链接是点了「资源下载」标签后
//   才由 server action 拉回来的，HTML 里一个字都没有。
//   完整原理见 game_scrape.rs 顶部注释。
//
// 前端契约: invoke('game_scrape_site', { listUrl, maxGames })
#[tauri::command]
pub async fn game_scrape_site(
    list_url: Option<String>,
    max_games: Option<usize>,
) -> Result<Vec<crate::game_scrape::ScrapedResource>, String> {
    if !crate::licensing::is_activated() {
        return Err("未激活授权, 请先在「设置 → 认证」中输入密钥".to_string());
    }
    let url = list_url.unwrap_or_else(|| crate::game_scrape::DEFAULT_LIST_URL.to_string());
    let n = max_games.unwrap_or(6).clamp(1, 30);
    crate::app_logger::log_command("game_scrape_site", &format!("list_url={url} max_games={n}"));
    let r = crate::game_scrape::scrape_resources(&url, n, |m| {
        crate::app_logger::log_command("game_scrape_site", m);
    })
    .await;
    match &r {
        Ok(v) => crate::app_logger::log_command(
            "game_scrape_site",
            &format!("完成: {} 条可用资源", v.len()),
        ),
        Err(e) => crate::app_logger::log_command("game_scrape_site", &format!("失败: {e}")),
    }
    r
}

// ★ 用户要求: 「排除所有网盘链接随机挑选一个下载」
//   这里只返回**一条**直链(随机), 网盘全部丢弃, 并把丢弃数量报给前端。
#[tauri::command]
pub async fn game_scrape_pick_download(
    list_url: Option<String>,
    max_games: Option<usize>,
) -> Result<crate::game_scrape::PickedDownload, String> {
    if !crate::licensing::is_activated() {
        return Err("未激活授权, 请先在「设置 → 认证」中输入密钥".to_string());
    }
    let url = list_url.unwrap_or_else(|| crate::game_scrape::DEFAULT_LIST_URL.to_string());
    let n = max_games.unwrap_or(8).clamp(1, 30);
    crate::app_logger::log_command(
        "game_scrape_pick_download",
        &format!("list_url={url} max_games={n}"),
    );
    let rs = crate::game_scrape::scrape_resources(&url, n, |m| {
        crate::app_logger::log_command("game_scrape_pick_download", m);
    })
    .await?;
    let picked = crate::game_scrape::pick_random(&rs).ok_or_else(|| {
        format!(
            "扫了 {} 条资源, 全是网盘链接, 没有可直连下载的",
            rs.len()
        )
    })?;
    crate::app_logger::log_command(
        "game_scrape_pick_download",
        &format!(
            "选中 resource {} (候选 {} 条, 排除网盘 {} 条): {}",
            picked.resource_id, picked.candidates, picked.pan_skipped, picked.url
        ),
    );
    Ok(picked)
}

// ===================== galgamex 游戏库 (GX) =====================
// ★ 全流程纯 HTTP（协议见 src/gx.rs 顶部注释 + _gx_protocol.md）。
//   站点没有对这几个接口做反爬，数据只是藏在 Next.js server action 里。
//
// 前端契约: invoke('gx_sync') / invoke('gx_browse', { query: {...} }) ...

#[tauri::command]
pub async fn gx_sync(app: AppHandle) -> Result<serde_json::Value, String> {
    if !crate::licensing::is_activated() {
        return Err("未激活授权, 请先在「设置 → 认证」中输入密钥".to_string());
    }
    crate::app_logger::log_command("gx_sync", "开始同步 galgamex 游戏库");
    let log = move |m: &str| {
        crate::app_logger::log_command("gx_sync", m);
        let _ = app.emit("gx-sync-progress", serde_json::json!({ "message": m }));
    };
    let idx = crate::gx::sync(&log).await?;
    let (total, doujin, gal, ts) = crate::gx::index_info();
    Ok(json!({
        "total": total, "doujin": doujin, "galgame": gal,
        "tags": idx.tags.len(), "synced_at": ts,
    }))
}

#[tauri::command]
pub async fn gx_status() -> Result<serde_json::Value, String> {
    let (total, doujin, gal, ts) = crate::gx::index_info();
    Ok(json!({
        "total": total, "doujin": doujin, "galgame": gal,
        "tags": crate::gx::all_tags().len(), "synced_at": ts,
        "ready": total > 0,
    }))
}

#[tauri::command]
pub async fn gx_tags() -> Result<Vec<crate::gx::GxTag>, String> {
    Ok(crate::gx::all_tags())
}

#[tauri::command]
pub async fn gx_browse(query: crate::gx::GxQuery) -> Result<crate::gx::GxPage, String> {
    Ok(crate::gx::browse(&query))
}

#[tauri::command]
pub async fn gx_detail(slug: String) -> Result<crate::gx::GxDetail, String> {
    crate::gx::detail(&slug).await
}

#[tauri::command]
pub async fn gx_resources(slug: String, game_id: u64) -> Result<Vec<crate::gx::GxResource>, String> {
    crate::gx::resources(&slug, game_id).await
}

#[tauri::command]
pub async fn gx_pick_download(
    resource_id: u64,
    index: Option<usize>,
) -> Result<crate::gx::GxPicked, String> {
    crate::gx::pick_download(resource_id, index).await
}

/// GX 详情页右侧的「相关推荐」：同标签最多的其它游戏。
#[tauri::command]
pub async fn gx_related(
    slug: String,
    limit: Option<usize>,
) -> Result<Vec<crate::search_engine::GameCard>, String> {
    Ok(crate::gx::related(&slug, limit.unwrap_or(10).min(30)))
}

// ===================== 独立播放窗口（动漫） =====================
//
// ★ 用户要求「视频播放单独开一个窗口，不要在软件里面」。
//
// 设计：主窗口只负责"发起播放"，把整份播放会话（标题 / 剧集 / 候选源 / 相关推荐）
// 交给播放窗口；播放窗口自己调 `anime_resolve` / `anime_episodes` 解析并播放，
// 进度和"点了相关推荐"再通过事件回给主窗口。
//
// ★ 为什么用「暂存 + 自取」而不是直接 emit：
//   新建窗口的前端要几百毫秒才就绪，这期间 emit 的事件它收不到。
//   所以 payload 先放进全局，播放窗口启动后自己 `player_take_payload()` 取走。

/// 把窗口真正抬到前台。
///
/// ★ 只靠 `set_focus()` 实测**不可靠**：用户报「主软件点播放没反应、前台不变」，
///   就是播放窗口没能被抬起来。Windows 的前台锁 + WebView2 子窗口会让
///   SetForegroundWindow 静默失败，所以这里补上 ShowWindow(SW_RESTORE) 和
///   BringWindowToTop，并把结果写日志（否则失败是无声的）。
#[cfg(windows)]
fn force_foreground(w: &tauri::WebviewWindow, tag: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };
    let _ = w.unminimize();
    let _ = w.show();
    let _ = w.set_focus();
    if let Ok(h) = w.hwnd() {
        let hwnd: *mut core::ffi::c_void = h.0;
        unsafe {
            ShowWindow(hwnd, SW_RESTORE);
            BringWindowToTop(hwnd);
            let ok = SetForegroundWindow(hwnd);
            let fg = GetForegroundWindow();
            crate::app_logger::log_window(
                tag,
                &format!("raise ok={} isForeground={}", ok, fg == hwnd),
            );
        }
    }
}

#[cfg(not(windows))]
fn force_foreground(w: &tauri::WebviewWindow, _tag: &str) {
    let _ = w.unminimize();
    let _ = w.show();
    let _ = w.set_focus();
}

static PLAYER_PAYLOAD: std::sync::Mutex<Option<serde_json::Value>> =
    std::sync::Mutex::new(None);

/// 上一次真正交给播放窗口的 payload 指纹。
///
/// ★ 用来做幂等：同一部番、同一集被重复发起时，只把窗口调到前台，
///   **不再发 player-reload** —— 否则播放会从头重来（实测踩过）。
static PLAYER_LAST_SENT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 打开（或复用）独立播放窗口。
///
/// `payload` 是一整份播放会话，字段见 src/player.js 的 applyPayload()。
#[tauri::command]
pub async fn open_player_window(app: AppHandle, payload: serde_json::Value) -> Result<(), String> {
    let fingerprint = payload.to_string();
    // 已经开着就复用。★ 只有 payload 真的变了才 reload，否则重复发起会把
    //   正在播的进度打断（从头重播）。
    if let Some(w) = app.get_webview_window("player") {
        let same = PLAYER_LAST_SENT
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .map(|prev| prev == fingerprint)
            .unwrap_or(false);
        // ★ 无论 payload 变没变都要抬到前台：用户点「开始观看」的预期就是
        //   播放窗口出现（之前这里只 show+set_focus，实测抬不起来）
        force_foreground(&w, "PLAYER_RAISE");
        if same {
            return Ok(());
        }
        {
            let mut g = PLAYER_PAYLOAD.lock().map_err(|e| e.to_string())?;
            *g = Some(payload);
        }
        if let Ok(mut g) = PLAYER_LAST_SENT.lock() {
            *g = Some(fingerprint);
        }
        let _ = w.emit("player-reload", ());
        return Ok(());
    }
    {
        let mut g = PLAYER_PAYLOAD.lock().map_err(|e| e.to_string())?;
        *g = Some(payload);
    }
    if let Ok(mut g) = PLAYER_LAST_SENT.lock() {
        *g = Some(fingerprint);
    }
    let w = tauri::WebviewWindowBuilder::new(
        &app,
        "player",
        tauri::WebviewUrl::App("player.html".into()),
    )
    .title("VortexDL 播放器 Player")
    .inner_size(1280.0, 800.0)
    .min_inner_size(760.0, 460.0)
    .resizable(true)
    .center()
    // ★ 用户要求：标题栏隐藏、窗口透明 —— 三个窗口按钮和底部的播放控件
    //   都由前端自己画（浮在画面上，鼠标移上去才显示）
    .decorations(false)
    .transparent(true)
    // ★ 自动播放策略必须和主窗口一致，否则一点播放就是黑屏
    //   （WebView2 默认要求用户手势，而解析播放地址是异步的，手势早就过期了）
    .additional_browser_args(
        "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --autoplay-policy=no-user-gesture-required",
    )
    .build()
    .map_err(|e| format!("创建播放窗口失败: {e}"))?;
    force_foreground(&w, "PLAYER_RAISE_NEW");
    Ok(())
}

/// 播放窗口里点了"相关推荐" → 把**主窗口**调到前台，并让它打开那部番剧的详情。
///
/// ★ 必须在 Rust 侧做：用户报"前台有播放还切换不了" —— 主窗口在后台时
///   自己 `set_focus()` 抢不到焦点（Windows 的前台锁）。由本进程在收到
///   播放窗口的调用后去 unminimize+show+set_focus，才能真的切到前台。
/// 让主窗口切到某个番剧详情，但**不抢焦点**。
///
/// ★ 播放窗口里点「相关推荐」现在是**本窗口直接换片播放**（用户要求），
///   但主窗口不能继续显示旧的那一部 —— 否则用户在主窗口点「开始观看」会把
///   播放窗口切回上一部，很困惑。所以只同步内容，不打扰正在看的画面。
#[tauri::command]
pub fn main_sync_subject(app: AppHandle, subject_id: u64) -> Result<(), String> {
    app.emit_to(
        "main",
        "player-open-subject",
        serde_json::json!({ "id": subject_id, "noFocus": true }),
    )
    .map_err(|e| e.to_string())
}

/// 把主窗口提到前台。快捷方式确认弹窗要用：下载+解压往往要一两分钟，
/// 那时主窗口多半被别的程序盖住了，不叫一下用户根本看不到那个确认框。
#[tauri::command]
pub fn raise_main_window(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("main") {
        force_foreground(&w, "MAIN_RAISE_SC");
    }
    Ok(())
}

#[tauri::command]
pub async fn focus_main_and_open(app: AppHandle, subject_id: u64) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("main") {
        force_foreground(&w, "MAIN_RAISE");
    }
    // 只发给主窗口，别让播放窗口自己也收到
    app.emit_to(
        "main",
        "player-open-subject",
        serde_json::json!({ "id": subject_id }),
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 播放窗口启动时自取一份播放会话（取走即清空）
#[tauri::command]
pub fn player_take_payload() -> Option<serde_json::Value> {
    // ★ 用 clone 而不是 take：播放窗口被 WebView 重载（或刷新）后还要能自己
    //   恢复播放，take 掉就只剩「没有待播放的内容」了。
    PLAYER_PAYLOAD.lock().ok().and_then(|g| g.clone())
}

#[cfg(all(test, windows))]
mod shortcut_tests {
    /// 归一化：空格/下划线/连字符/点都去掉，方便模糊比较
    #[test]
    fn norm_name_strips_separators() {
        assert_eq!(super::norm_name("Boundaries_of_Morality-0.600-pc"), "boundariesofmorality0600pc");
        assert_eq!(super::norm_name("Granny Remake v3.6.4"), "grannyremakev364");
        assert_eq!(super::norm_name(""), "");
    }

    /// ★ 共享根目录 + 名字对不上 → 必须**报错**，绝不能指到别的游戏。
    ///   这是"快捷方式错了"那个 bug 的回归测试。
    #[test]
    #[ignore]
    fn shared_root_refuses_when_name_mismatches() {
        let r = super::create_desktop_shortcut(
            r"D:\game".to_string(),
            "__绝对不存在的游戏名_zzz__".to_string(),
        );
        match r {
            Err(e) => {
                println!("按预期拒绝: {e}");
                assert!(e.contains("共享目录") || e.contains("没找到"), "错误信息应说明原因: {e}");
            }
            Ok(()) => panic!("共享根目录下名字对不上却创建成功了 —— 会指到别的游戏！"),
        }
        // 桌面不该多出这个 lnk
        if let Some(d) = dirs::desktop_dir() {
            let p = d.join("__绝对不存在的游戏名_zzz__.lnk");
            assert!(!p.exists(), "不该在桌面留下 .lnk");
        }
    }

    /// 图标字段真的写进去了。
    ///
    /// ★ 踩过的坑：`$s.IconLocation = '{}',0'` 逗号写在引号**外**，PowerShell 把
    ///   `'path',0` 当 Object[]，赋值抛 SetValueInvocationException，Save() 照跑，
    ///   于是 .lnk 建出来了但 IconLocation 是空的（读回来只有 `,0`）。逗号必须在引号内。
    #[test]
    #[ignore]
    fn shortcut_icon_location_points_at_target() {
        let exe = r"C:\Windows\System32\notepad.exe";
        let name = "__vx_icon_test__";
        super::create_desktop_shortcut(exe.to_string(), name.to_string())
            .expect("创建测试快捷方式失败");

        let desktop = dirs::desktop_dir().expect("没有桌面目录");
        let lnk = desktop.join(format!("{name}.lnk"));
        assert!(lnk.exists(), ".lnk 没建出来");

        let ps = format!(
            "$ws = New-Object -ComObject WScript.Shell; \
             $s = $ws.CreateShortcut('{}'); \
             Write-Output $s.IconLocation",
            lnk.to_string_lossy().replace('\'', "''")
        );
        let mut cmd = std::process::Command::new("powershell");
        cmd.arg("-NoProfile").arg("-NonInteractive").arg("-Command").arg(&ps);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000u32);
        }
        let out = cmd.output().expect("powershell 跑不起来");
        let icon = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let _ = std::fs::remove_file(&lnk); // 别在用户桌面留垃圾
        println!("IconLocation 读回 = [{icon}]");

        assert!(
            icon.to_lowercase().contains("notepad.exe"),
            "IconLocation 是空的/没指到目标 exe: [{icon}]"
        );
        assert!(icon.ends_with(",0"), "IconLocation 缺索引: [{icon}]");
    }

    /// ★★ 回归测试 (2026-10-08)：目标路径里带**代码页表示不了的字符**时，
    ///   旧的 `New-Object -ComObject WScript.Shell`（IDispatch 后期绑定，按 ANSI 编组）
    ///   会把路径写坏，`Save()` 抛 `System.ArgumentException: 值不在预期的范围内。`
    ///   —— 用户看到的是"解压完了但桌面没有快捷方式"。
    ///   实测游戏目录 `らぶらぶ♥プリンセス`（♥ U+2665）必现。
    ///   现在改用强类型 IShellLinkW（LPWStr/UTF-16 编组）必须能建出来。
    #[test]
    fn shortcut_target_path_with_unencodable_char() {
        let tmp = std::env::temp_dir().join("vx_らぶらぶ♥プリンセス");
        if std::fs::create_dir_all(&tmp).is_err() {
            eprintln!("跳过：建不了临时目录");
            return;
        }
        let exe = tmp.join("测试启动.exe");
        if std::fs::copy(r"C:\Windows\System32\notepad.exe", &exe).is_err() {
            eprintln!("跳过：拷不到 notepad.exe");
            return;
        }
        let name = "__vx_unicode_test__";
        let r = super::create_desktop_shortcut(tmp.to_string_lossy().to_string(), name.to_string());
        let desktop = dirs::desktop_dir().expect("没有桌面目录");
        let lnk = desktop.join(format!("{name}.lnk"));
        let existed = lnk.exists();
        let _ = std::fs::remove_file(&lnk);
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(r.is_ok(), "含 ♥ 的路径建快捷方式失败: {:?}", r.err());
        assert!(existed, ".lnk 没建出来");
    }

    // ============================================================
    // 「让用户自己选程序」弹窗（list_exe_candidates）的排序测试
    // ============================================================

    /// 造一个像 GX 解压出来的目录：游戏主程序 + 一堆干扰 exe。
    fn make_fake_extract(tag: &str) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
        let root = std::env::temp_dir().join(format!("vx_pick_{tag}"));
        let game = root.join("夏空のペルセウス");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(game.join("redist")).ok()?;
        let notepad = r"C:\Windows\System32\notepad.exe";
        // 主程序：带图标、体积最大、名字和目录同名
        std::fs::copy(notepad, game.join("夏空のペルセウス.exe")).ok()?;
        // 干扰项：安装/卸载/运行库
        std::fs::copy(notepad, game.join("setup.exe")).ok()?;
        std::fs::copy(notepad, game.join("unins000.exe")).ok()?;
        std::fs::copy(notepad, game.join("redist").join("vcredist_x64.exe")).ok()?;
        Some((root, game))
    }

    /// 弹窗列表：第一名必须是游戏主程序，且只有它带 `auto`（界面上标「推荐」）。
    #[test]
    fn picker_ranks_game_exe_first() {
        let Some((root, game)) = make_fake_extract("rank") else {
            eprintln!("跳过：建不了测试目录");
            return;
        };
        let (cands, shared, _) = super::rank_exe_candidates(&game, "夏空のペルセウス", false);
        let names: Vec<String> = cands.iter().map(|c| c.name.clone()).collect();
        println!("候选顺序 = {names:?}  shared_root={shared}");
        assert!(!shared, "单个游戏目录不该被判成共享根目录");
        assert!(!cands.is_empty(), "一个候选都没列出来");
        assert_eq!(cands[0].name, "夏空のペルセウス.exe", "第一名不是主程序: {names:?}");
        assert!(cands[0].auto, "主程序必须带 auto 标记（界面标「推荐」）");
        assert_eq!(cands.iter().filter(|c| c.auto).count(), 1, "auto 只能有一个");
        // 主程序必须排在 setup / unins 前面
        let pos = |n: &str| names.iter().position(|x| x == n).unwrap_or(usize::MAX);
        assert!(pos("夏空のペルセウス.exe") < pos("setup.exe"), "setup 排在主程序前面了");
        assert!(pos("夏空のペルセウス.exe") < pos("unins000.exe"), "unins 排在主程序前面了");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 共享根目录（平铺解压的 D:\game）下，**自动**创建要淘汰名字对不上的 exe，
    /// 但**弹窗**必须把它们列出来（用户主动挑，清空列表等于没得选）。
    #[test]
    fn shared_root_auto_drops_but_picker_lists() {
        let Some((root, _)) = make_fake_extract("shared") else {
            eprintln!("跳过：建不了测试目录");
            return;
        };
        // 再塞一个别的游戏，制造"共享根目录"
        let other = root.join("另一个游戏");
        std::fs::create_dir_all(&other).expect("建不了子目录");
        std::fs::copy(r"C:\Windows\System32\notepad.exe", other.join("Other.exe"))
            .expect("拷不了 notepad");

        // ① 自动：名字对不上 → 必须一个都不给（create_desktop_shortcut 会因此报错）
        let (auto_cands, shared, dropped) =
            super::rank_exe_candidates(&root, "完全不相干的名字zzz", true);
        println!("自动: 候选={} 共享={shared} 淘汰={dropped}", auto_cands.len());
        assert!(shared, "应该被判成共享根目录");
        assert!(auto_cands.is_empty(), "共享目录下名字对不上却给了候选: {:?}",
            auto_cands.iter().map(|c| c.name.clone()).collect::<Vec<_>>());

        // ② 弹窗：同一目录必须列出 exe 供用户选
        let (pick_cands, _, _) = super::rank_exe_candidates(&root, "完全不相干的名字zzz", false);
        println!("弹窗: 候选={}", pick_cands.len());
        assert!(!pick_cands.is_empty(), "弹窗一个候选都没列出来（用户没得选）");
        // ★ 名字对不上时**不能**给「推荐」：那会推荐到别的游戏的 exe，
        //   用户一路点下去就建出指错游戏的快捷方式（实测 D:\game 上就是这个现象）
        assert!(
            !pick_cands.iter().any(|c| c.auto),
            "共享目录 + 名字对不上，却给了推荐项: {:?}",
            pick_cands.iter().filter(|c| c.auto).map(|c| c.name.clone()).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 名字命中时，共享根目录里**对的**那个游戏目录要留下，别的游戏要淘汰。
    ///
    /// 注意语义：判定用的是**相对路径**，所以 `夏空のペルセウス\` 这个目录下面的
    /// setup/unins/运行库 exe 都算"名字命中"（它们确实属于这个游戏），
    /// 会被留下当候选；真正要淘汰的是**别的游戏目录**里的 exe。
    #[test]
    fn shared_root_name_hit_wins() {
        let Some((root, _)) = make_fake_extract("hit") else {
            eprintln!("跳过：建不了测试目录");
            return;
        };
        let other = root.join("另一个游戏");
        std::fs::create_dir_all(&other).expect("建不了子目录");
        std::fs::copy(r"C:\Windows\System32\notepad.exe", other.join("Other.exe"))
            .expect("拷不了 notepad");

        let (cands, shared, dropped) = super::rank_exe_candidates(&root, "夏空のペルセウス", true);
        assert!(shared);
        let names: Vec<String> = cands.iter().map(|c| c.name.clone()).collect();
        println!("名字命中后 = {names:?} 淘汰={dropped}");
        assert_eq!(cands[0].name, "夏空のペルセウス.exe", "主程序没排第一: {names:?}");
        assert!(cands[0].auto, "主程序要带 auto 标记");
        assert!(
            !names.iter().any(|n| n == "Other.exe"),
            "别的游戏的 exe 混进来了: {names:?}"
        );
        assert!(dropped >= 1, "另一个游戏的 exe 应该被淘汰计数");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 诊断：拿**真实**的共享目录（平铺解压的 D:\game）看看弹窗列表长什么样。
    /// 用户点已完成任务上的「🔗 快捷方式」时就是这个列表。
    #[test]
    #[ignore]
    fn diag_real_dir_picker_list() {
        let dir = std::path::PathBuf::from(r"D:\game");
        if !dir.is_dir() {
            eprintln!("跳过：{} 不存在", dir.display());
            return;
        }
        let (cands, shared, dropped) = super::rank_exe_candidates(&dir, "夏空のペルセウス", false);
        println!("共享={shared} 淘汰={dropped} 候选={}", cands.len());
        for c in cands.iter().take(12) {
            println!("  auto={} icon={} {} | {}", c.auto, c.has_icon, c.name, c.rel);
        }
    }
}
