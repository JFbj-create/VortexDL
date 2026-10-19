//! 应用自更新：启动后查 GitHub Release，发现比当前新的版本就下载安装包、拉起安装程序。
//!
//! ★ 仓库是**唯一的版本来源**：每次发版都要在 GitHub 上建对应 tag 的 Release，并上传
//!     · `VortexDL-Setup-v<版本>.exe`    —— 安装包（自动更新下载的就是它）
//!     · `VortexDL-Portable-v<版本>.zip` —— 便携版（解压即用）
//!   这里靠 **tag 名**判版本、靠 **资产名里带 "Setup" 且以 .exe 结尾**找安装包，
//!   所以这两个附件名不能乱改。
//! ★ 判定"是不是安装版"看 exe 同级有没有 `uninstall.exe`：
//!     安装版 → 安装程序以 `/S` **静默**安装（NSIS 的 .onInit 会先 taskkill 掉我们），
//!              装完由 .onInstSuccess 重新拉起；
//!     便携版 → 用普通（非静默）方式打开安装程序，让用户自己选目录，不偷偷装到 Program Files。
//!
//! # ★★ 为什么要走镜像
//!
//! 实测（2026-10-12，国内家宽）：直连 `github.com/.../releases/download/...` 只有
//! **86 KB/s 而且会在 0.5~1.5MB 处卡死**，200MB 的安装包根本下不完。
//! 加了加速镜像后 `ghfast.top` 能跑到 **1.9 MB/s**。所以下载按下面的顺序试，
//! 任何一个源"卡住"就换下一个 —— 这对国内用户是能不能更新的区别，不是优化。
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const VX_AUX_D: &str = "0000000000000000"; // 公开版占位（豪华版校验已移除）

const API_LATEST: &str = "https://api.github.com/repos/jfbj-create/VortexDL/releases/latest";

/// ★★ 2026-10-18 修「自动检查不到更新」：
///   本机 hosts 里 `api.github.com` 被指到 127.0.0.1（不是我们写的，是别的软件下发的），
///   reqwest 不会走 Windows 系统代理，直连必然失败（Python/curl 走系统代理所以看着正常）。
///   → 直连失败时依次试 GitHub 加速镜像的 API 转发（实测 gh-proxy.com 可用）。
const API_MIRRORS: &[&str] = &[
    "https://gh-proxy.com/https://api.github.com/repos/jfbj-create/VortexDL/releases/latest",
    "https://ghfast.top/https://api.github.com/repos/jfbj-create/VortexDL/releases/latest",
    "https://ghproxy.net/https://api.github.com/repos/jfbj-create/VortexDL/releases/latest",
];
const UA: &str = "VortexDL-Updater/1.0 (+https://github.com/jfbj-create/VortexDL)";

/// 下载候选源。**第一个是直连**，后面是加速镜像（前缀拼在原始 URL 前面）。
/// 顺序按 2026-10-12 实测速率排的：直连 86KB/s、ghfast 1.9MB/s、gh-proxy 275KB/s。
/// 空串 = 直连。
const MIRRORS: &[&str] = &[
    "",
    "https://ghfast.top/",
    "https://gh-proxy.com/",
    "https://ghproxy.net/",
];

/// 单个源最多给多少秒"看不到进展"就换下一个
const STALL_SECS: u64 = 20;
/// 换源门槛：STALL_SECS 内至少要下到这么多字节，否则认为这个源没用
const STALL_MIN_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppUpdateResult {
    pub has_update: bool,
    pub latest_version: String,
    pub current_version: String,
    pub download_url: String,
    #[serde(default)] pub release_notes: String,
    #[serde(default)] pub file_size: u64,
    #[serde(default)] pub created_at: String,
    #[serde(default)] pub asset_name: String,
    #[serde(default)] pub html_url: String,
    /// 非空表示"查不到 / 查失败"，前端据此决定是静默跳过还是提示
    #[serde(default)] pub error: String,
}

/// 宽松解析版本号：`v1.2.3` / `1.2` / `1.2.3-beta` 都能出 (major, minor, patch)
fn parse_ver(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let core = s.split(['-', '+']).next().unwrap_or("");
    let mut it = core.split('.');
    let a = it.next()?.trim().parse::<u64>().ok()?;
    let b = it.next().unwrap_or("0").trim().parse::<u64>().unwrap_or(0);
    let c = it.next().unwrap_or("0").trim().parse::<u64>().unwrap_or(0);
    Some((a, b, c))
}

/// 查 API 用的客户端：小 JSON，总超时短一点没问题
fn api_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(UA)
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// ★★ 下载安装包用的客户端：**绝对不能设总超时**。
///    第一版照抄了 API 客户端那 20 秒总超时，结果 200MB 的包下到 0.5MB 就被判超时，
///    报了一句 `error decoding response body`（还是 reqwest 的原文）。
///    这里改成：连接 15 秒超时 + **单次读取** 30 秒超时（没有新字节才判死）。
fn dl_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(UA)
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// 把 reqwest 的英文错误翻成人话
fn friendly_dl_err(e: &reqwest::Error) -> String {
    let s = e.to_string();
    if e.is_timeout() {
        "下载超时（网络太慢或链接被限速）".to_string()
    } else if e.is_connect() {
        "连不上下载服务器".to_string()
    } else if s.contains("decoding") || s.contains("body") {
        "下载中途断了".to_string()
    } else {
        format!("下载出错: {}", s.chars().take(120).collect::<String>())
    }
}

/// 查最新 Release 并和当前版本比对。**任何网络错误都不算"有更新"**，
/// 只在 `error` 里留一句人话，前端静默跳过即可。
pub async fn check_app_update() -> AppUpdateResult {
    let cur = env!("CARGO_PKG_VERSION").to_string();
    let mut out = AppUpdateResult {
        current_version: cur.clone(),
        latest_version: cur.clone(),
        ..Default::default()
    };

    // 直连 → 镜像逐个试；全部失败才报错（错误里带上最后一次的原因）
    let mut resp = None;
    let mut last_err = String::new();
    for url in std::iter::once(API_LATEST).chain(API_MIRRORS.iter().copied()) {
        match api_client()
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => { resp = Some(r); break; }
            Ok(r) => { last_err = format!("更新服务器返回 HTTP {}", r.status().as_u16()); }
            Err(e) => { last_err = format!("{}", e); }
        }
    }
    let resp = match resp {
        Some(r) => r,
        None => {
            out.error = format!("连不上更新服务器（直连与镜像都失败）: {}", last_err);
            return out;
        }
    };
    if !resp.status().is_success() {
        out.error = format!("更新服务器返回 HTTP {}", resp.status().as_u16());
        return out;
    }
    let v: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            out.error = format!("更新信息解析失败: {}", e);
            return out;
        }
    };

    let tag = v.get("tag_name").and_then(|x| x.as_str()).unwrap_or("");
    out.html_url = v.get("html_url").and_then(|x| x.as_str()).unwrap_or("").to_string();
    out.release_notes = v.get("body").and_then(|x| x.as_str()).unwrap_or("").to_string();
    out.created_at = v.get("published_at").and_then(|x| x.as_str()).unwrap_or("").to_string();
    out.latest_version = tag.trim_start_matches(['v', 'V']).to_string();
    if out.latest_version.is_empty() {
        out.error = "最新 Release 没有 tag 名".into();
        return out;
    }

    // 找安装包：资产名里带 "setup" 且 .exe 结尾；找不到就退而求其次取第一个 .exe
    let assets = v.get("assets").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let pick = |want_setup: bool| -> Option<(String, String, u64)> {
        for a in &assets {
            let name = a.get("name").and_then(|x| x.as_str()).unwrap_or("");
            let lower = name.to_ascii_lowercase();
            if !lower.ends_with(".exe") { continue; }
            if want_setup && !lower.contains("setup") { continue; }
            let url = a.get("browser_download_url").and_then(|x| x.as_str()).unwrap_or("");
            if url.is_empty() { continue; }
            return Some((
                name.to_string(),
                url.to_string(),
                a.get("size").and_then(|x| x.as_u64()).unwrap_or(0),
            ));
        }
        None
    };
    if let Some((name, url, size)) = pick(true).or_else(|| pick(false)) {
        out.asset_name = name;
        out.download_url = url;
        out.file_size = size;
    }

    let newer = match (parse_ver(&out.latest_version), parse_ver(&cur)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    };
    if newer && out.download_url.is_empty() {
        out.error = format!("v{} 的 Release 里没有 .exe 安装包", out.latest_version);
        return out;
    }
    out.has_update = newer;
    out
}

/// 当前是不是"安装版"（exe 同级有 uninstall.exe）
fn is_installed() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("uninstall.exe")))
        .map(|p| p.exists())
        .unwrap_or(false)
}

/// 用 `runas` 动词拉起（会弹 UAC；如果系统设成"管理员静默提权"就不会弹）。
/// 返回 Err 表示用户取消或启动失败。
#[cfg(windows)]
fn shell_run(path: &Path, args: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;

    let wide = |s: &str| -> Vec<u16> {
        std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    };
    let verb = wide("runas");
    let file = wide(&path.to_string_lossy());
    let params = wide(args);
    // SW_SHOWNORMAL = 1
    let h = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            1,
        )
    };
    // ShellExecuteW 返回 <= 32 都是错误码（> 32 才算成功）
    let code = h as isize;
    if code <= 32 {
        return Err(match code {
            5 => "安装程序需要管理员权限，已被拒绝".to_string(),
            _ => format!("拉起安装程序失败（ShellExecuteW={}）", code),
        });
    }
    Ok(())
}

#[cfg(not(windows))]
fn shell_run(_path: &Path, _args: &str) -> Result<(), String> {
    Err("当前平台不支持自动安装".into())
}

/// 从一个源把安装包拉下来。
/// ★ 带"卡住就放弃"的看门狗：`STALL_SECS` 秒内下不到 `STALL_MIN_BYTES` 就返回 Err，
///   让上层换镜像 —— 直连 GitHub 的典型表现就是"连上了、有 200、然后不动了"。
async fn fetch_from(
    app: &tauri::AppHandle,
    url: &str,
    dst: &Path,
    total_hint: u64,
    show_progress: bool,
) -> Result<u64, String> {
    use tauri::Emitter;

    let mut resp = dl_client()
        .get(url)
        .send()
        .await
        .map_err(|e| friendly_dl_err(&e))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let total = resp.content_length().unwrap_or(total_hint);

    use std::io::Write;
    let mut file = std::fs::File::create(dst).map_err(|e| format!("写入失败: {}", e))?;
    let mut got: u64 = 0;
    let mut last_emit = std::time::Instant::now();
    let started = std::time::Instant::now();

    loop {
        let chunk = match resp.chunk().await {
            Ok(c) => c,
            Err(e) => return Err(friendly_dl_err(&e)),
        };
        let Some(chunk) = chunk else { break };
        file.write_all(&chunk).map_err(|e| format!("写入失败: {}", e))?;
        got += chunk.len() as u64;

        if show_progress && last_emit.elapsed() >= std::time::Duration::from_millis(250) {
            last_emit = std::time::Instant::now();
            let _ = app.emit(
                "app-update-progress",
                serde_json::json!({ "downloaded": got, "total": total }),
            );
        }
        // 看门狗：起步阶段太慢就换源
        if started.elapsed().as_secs() >= STALL_SECS && got < STALL_MIN_BYTES {
            return Err(format!("这个源太慢（{} 秒只下了 {} KB）", STALL_SECS, got / 1024));
        }
    }
    let _ = file.flush();
    Ok(got)
}

/// 下载安装包 -> 拉起安装程序 -> 退出自己。安装程序装完会重新启动应用。
pub async fn download_and_install(
    app: &tauri::AppHandle,
    r: AppUpdateResult,
) -> Result<String, String> {
    use tauri::Emitter;

    if r.download_url.is_empty() {
        return Err("没有可下载的安装包地址".into());
    }
    let name = if r.asset_name.is_empty() {
        format!("VortexDL-Setup-v{}.exe", r.latest_version)
    } else {
        r.asset_name.clone()
    };
    let dir = std::env::temp_dir().join("VortexDL-update");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {}", e))?;
    let dst: PathBuf = dir.join(&name);

    let mut got: u64 = 0;
    let mut errs: Vec<String> = Vec::new();
    for (i, m) in MIRRORS.iter().enumerate() {
        let url = format!("{}{}", m, r.download_url);
        let label = if m.is_empty() { "直连".to_string() } else { m.trim_end_matches('/').to_string() };
        let _ = app.emit(
            "app-update-source",
            serde_json::json!({ "index": i, "label": label, "total": MIRRORS.len() }),
        );
        match fetch_from(app, &url, &dst, r.file_size, true).await {
            Ok(n) if n >= 1024 * 1024 => {
                got = n;
                break;
            }
            Ok(n) => {
                errs.push(format!("{}: 只下到 {} KB", label, n / 1024));
            }
            Err(e) => {
                errs.push(format!("{}: {}", label, e));
            }
        }
        let _ = std::fs::remove_file(&dst);
    }

    if got == 0 {
        let _ = app.emit("app-update-progress", serde_json::json!({ "failed": true }));
        return Err(format!(
            "所有下载源都失败了 —— {}。可以到项目主页手动下载安装包。",
            errs.join("；")
        ));
    }

    // ★ 大小校验：CDN 报的 Content-Length 有时比真实内容小，截断的安装包会装坏
    //   （训练器模块踩过一次 os error 193，就是被截断的 exe）。
    if r.file_size > 0 && got != r.file_size {
        let _ = std::fs::remove_file(&dst);
        return Err(format!(
            "安装包不完整（{}/{} 字节），已删除，请重试",
            got, r.file_size
        ));
    }
    let _ = app.emit(
        "app-update-progress",
        serde_json::json!({ "downloaded": got, "total": got, "done": true }),
    );

    // 安装版静默升级；便携版走正常向导，让用户自己选目录
    let args = if is_installed() { "/S" } else { "" };
    shell_run(&dst, args)?;

    // 安装程序的 .onInit 会 taskkill 我们；这里主动退干净，避免文件占用
    let _ = app.emit(
        "app-update-launched",
        serde_json::json!({ "path": dst.to_string_lossy() }),
    );
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(1200));
        std::process::exit(0);
    });
    Ok(dst.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(parse_ver("v1.2.0").unwrap() > parse_ver("1.1.0").unwrap());
        assert!(parse_ver("1.2").unwrap() > parse_ver("1.1.9").unwrap());
        assert!(parse_ver("1.2.0-beta").unwrap() == parse_ver("1.2.0").unwrap());
        assert_eq!(parse_ver("2.0.0").unwrap(), (2, 0, 0));
        assert!(parse_ver("垃圾").is_none());
    }

    #[test]
    fn mirrors_start_with_direct() {
        // 第一个必须是直连（空串），否则国内用户永远先撞一个慢镜像
        assert_eq!(MIRRORS[0], "");
        assert!(MIRRORS.len() >= 2, "至少要有直连 + 一个镜像");
        for m in &MIRRORS[1..] {
            assert!(m.starts_with("https://"), "镜像必须是完整 URL 前缀: {}", m);
            assert!(m.ends_with('/'), "镜像前缀必须以 / 结尾才能直接拼: {}", m);
        }
    }

    #[test]
    fn mirror_join_is_valid_url() {
        let orig = "https://github.com/a/b/releases/download/v1/x.exe";
        for m in MIRRORS {
            let u = format!("{}{}", m, orig);
            assert!(u.starts_with("https://"));
            assert!(u.ends_with("x.exe"));
        }
    }
}
