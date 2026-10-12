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
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const VX_AUX_D: &str = "0000000000000000"; // 公开版占位（豪华版校验已移除）

const API_LATEST: &str = "https://api.github.com/repos/jfbj-create/VortexDL/releases/latest";
const UA: &str = "VortexDL-Updater/1.0 (+https://github.com/jfbj-create/VortexDL)";

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

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(UA)
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
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

    let resp = match client()
        .get(API_LATEST)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            out.error = format!("连不上更新服务器: {}", e);
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

/// 用 `runas` 动词拉起（会弹 UAC）。返回 Err 表示用户取消或启动失败。
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

    let mut resp = client()
        .get(&r.download_url)
        .send()
        .await
        .map_err(|e| format!("下载安装包失败: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("下载安装包失败: HTTP {}", resp.status().as_u16()));
    }
    let total = resp.content_length().unwrap_or(r.file_size);

    use std::io::Write;
    let mut file = std::fs::File::create(&dst).map_err(|e| format!("写入失败: {}", e))?;
    let mut got: u64 = 0;
    let mut last_emit = std::time::Instant::now();
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("下载中断: {}", e))? {
        file.write_all(&chunk).map_err(|e| format!("写入失败: {}", e))?;
        got += chunk.len() as u64;
        if last_emit.elapsed() >= std::time::Duration::from_millis(250) {
            last_emit = std::time::Instant::now();
            let _ = app.emit(
                "app-update-progress",
                serde_json::json!({ "downloaded": got, "total": total }),
            );
        }
    }
    let _ = file.flush();
    drop(file);

    // ★ 大小校验：CDN 报的 Content-Length 有时比真实内容小，截断的安装包会装坏
    //   （训练器模块踩过一次 os error 193，就是被截断的 exe）。
    if total > 0 && got != total {
        let _ = std::fs::remove_file(&dst);
        return Err(format!("安装包不完整（{}/{} 字节），已删除，请重试", got, total));
    }
    if got < 1024 * 1024 {
        let _ = std::fs::remove_file(&dst);
        return Err("安装包太小，多半是下到了错误页面".into());
    }
    let _ = app.emit(
        "app-update-progress",
        serde_json::json!({
            "downloaded": got, "total": if total > 0 { total } else { got }, "done": true
        }),
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
