// ============================================================
// 内核注入 (OpenSteamTool) 模块
// 功能: 向 Steam 目录注入/移除 OpenSteamTool 内核 DLL, 管理 Steam 进程, 检查更新
// ============================================================

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// 内核注入的 3 个 DLL 文件名
const KERNEL_DLLS: [&str; 3] = ["dwmapi.dll", "xinput1_4.dll", "OpenSteamTool.dll"];
/// 本地 OpenSteamTool 版本 (与打包的 zip 版本一致)
const LOCAL_KERNEL_VERSION: &str = "1.4.8";
/// OpenSteamTool GitHub Release API
const GITHUB_RELEASE_API: &str = "https://api.github.com/repos/OpenSteam001/OpenSteamTool/releases/latest";

/// 内核状态返回结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelStatus {
    #[serde(default)] pub error: Option<String>,
    pub injected: bool,
    #[serde(default)] pub injected_dlls: Vec<String>,
    #[serde(default)] pub steam_path: String,
}

/// 内核更新检查返回结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelUpdateInfo {
    pub current_version: String,
    pub has_update: bool,
    #[serde(default)] pub latest_version: String,
    #[serde(default)] pub download_url: String,
}

/// 定位 OpenSteamTool 压缩包 (已安装/开发模式)
fn find_kernel_archive() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
    // 已安装: resources/archives/OpenSteamTool-*.zip
    if let Some(d) = &exe_dir {
        let archives = d.join("resources").join("archives");
        if let Some(p) = find_opensteamtool_zip(&archives) {
            return Some(p);
        }
    }
    // 开发模式: CARGO_MANIFEST_DIR/archives/OpenSteamTool-*.zip
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("archives");
    find_opensteamtool_zip(&dev)
}

/// 在指定目录中查找 OpenSteamTool-*.zip
fn find_opensteamtool_zip(dir: &Path) -> Option<PathBuf> {
    if !dir.exists() { return None; }
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name.starts_with("opensteamtool") && name.ends_with(".zip") {
            return Some(entry.path());
        }
    }
    None
}

/// 检测 Steam 安装路径 (注册表 + 常见路径)
pub(crate) fn find_steam_path() -> Option<PathBuf> {
    // 1. 注册表 HKCU\Software\Valve\Steam\SteamPath
    if let Ok(out) = Command::new("reg")
        .args(["query", r"HKCU\Software\Valve\Steam", "/v", "SteamPath"])
        .output()
    {
        let s = String::from_utf8_lossy(&out.stdout);
        for line in s.lines() {
            if line.contains("SteamPath") {
                if let Some(idx) = line.find("REG_SZ") {
                    let path = line[idx + 6..].trim();
                    if !path.is_empty() {
                        let p = PathBuf::from(path);
                        if p.join("steam.exe").exists() || p.exists() {
                            return Some(p);
                        }
                    }
                }
            }
        }
    }
    // 2. 注册表 HKLM\SOFTWARE\WOW6432Node\Valve\Steam\InstallPath
    if let Ok(out) = Command::new("reg")
        .args(["query", r"HKLM\SOFTWARE\WOW6432Node\Valve\Steam", "/v", "InstallPath"])
        .output()
    {
        let s = String::from_utf8_lossy(&out.stdout);
        for line in s.lines() {
            if line.contains("InstallPath") {
                if let Some(idx) = line.find("REG_SZ") {
                    let path = line[idx + 6..].trim();
                    if !path.is_empty() {
                        let p = PathBuf::from(path);
                        if p.exists() { return Some(p); }
                    }
                }
            }
        }
    }
    // 3. 常见安装路径
    for candidate in [
        r"C:\Program Files (x86)\Steam",
        r"C:\Program Files\Steam",
        r"D:\Steam",
        r"E:\Steam",
    ] {
        let p = PathBuf::from(candidate);
        if p.join("steam.exe").exists() {
            return Some(p);
        }
    }
    None
}

/// 查询内核注入状态
#[tauri::command]
pub fn kernel_status() -> Result<KernelStatus, String> {
    let steam_path = match find_steam_path() {
        Some(p) => p,
        None => return Ok(KernelStatus {
            error: Some("未找到 Steam 安装目录".into()),
            injected: false,
            injected_dlls: vec![],
            steam_path: String::new(),
        }),
    };

    let mut injected_dlls = Vec::new();
    for dll in &KERNEL_DLLS {
        if steam_path.join(dll).exists() {
            injected_dlls.push(dll.to_string());
        }
    }
    let injected = injected_dlls.len() == KERNEL_DLLS.len();

    Ok(KernelStatus {
        error: None,
        injected,
        injected_dlls,
        steam_path: steam_path.to_string_lossy().to_string(),
    })
}

/// 解压 OpenSteamTool zip 到临时目录, 返回解压后的目录
fn extract_opensteamtool(zip_path: &Path) -> Result<PathBuf, String> {
    let file = std::fs::File::open(zip_path).map_err(|e| format!("打开压缩包失败: {e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("读取压缩包失败: {e}"))?;

    let out_dir = std::env::temp_dir().join(format!("opensteamtool_extract_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("创建临时目录失败: {e}"))?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("读取条目失败: {e}"))?;
        let name = entry.name().to_string();
        // 跳过目录条目
        if name.ends_with('/') { continue; }
        // 安全化路径
        let safe_name = name.replace('\\', "/");
        let components: Vec<&str> = safe_name.split('/').filter(|c| !c.is_empty() && *c != "..").collect();
        let rel = components.join("/");
        if rel.is_empty() { continue; }
        let out_path = out_dir.join(&rel);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut out_file = std::fs::File::create(&out_path).map_err(|e| format!("创建文件失败: {e}"))?;
        std::io::copy(&mut entry, &mut out_file).map_err(|e| format!("解压文件失败: {e}"))?;
    }
    Ok(out_dir)
}

/// 在解压目录中查找指定 DLL (递归搜索)
fn find_dll_in_dir(dir: &Path, dll_name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_dll_in_dir(&path, dll_name) {
                return Some(found);
            }
        } else if path.file_name().map(|n| n.eq_ignore_ascii_case(dll_name)).unwrap_or(false) {
            return Some(path);
        }
    }
    None
}

/// 内核注入: 复制 3 个 DLL 到 Steam 根目录 (原文件备份为 .vortex_backup)
#[tauri::command]
pub fn kernel_inject() -> Result<String, String> {
    let steam_path = find_steam_path().ok_or("未找到 Steam 安装目录")?;
    let archive = find_kernel_archive().ok_or("未找到 OpenSteamTool 压缩包 (resources/archives 目录缺失)")?;

    // 1. 解压
    let extract_dir = extract_opensteamtool(&archive)?;

    // 2. 查找并复制 3 个 DLL
    let mut copied = Vec::new();
    for dll in &KERNEL_DLLS {
        let src = find_dll_in_dir(&extract_dir, dll)
            .ok_or_else(|| format!("压缩包中未找到 {dll}"))?;
        let dst = steam_path.join(dll);
        // 备份原文件 (如果存在且不是我们之前注入的)
        if dst.exists() {
            let backup = dst.with_extension("vortex_backup");
            if !backup.exists() {
                std::fs::copy(&dst, &backup).map_err(|e| format!("备份 {dll} 失败: {e}"))?;
            }
        }
        std::fs::copy(&src, &dst).map_err(|e| format!("复制 {dll} 失败: {e}"))?;
        copied.push(dll.to_string());
    }

    // 3. 创建 config\lua 脚本目录
    let lua_dir = steam_path.join("config").join("lua");
    std::fs::create_dir_all(&lua_dir).map_err(|e| format!("创建 lua 目录失败: {e}"))?;

    // 清理临时目录
    let _ = std::fs::remove_dir_all(&extract_dir);

    Ok(format!(
        "内核注入成功!\n\n已复制 {} 到 Steam 目录: {}\n\nSteam 路径: {}\n\n请重启 Steam 使内核生效。",
        copied.join(", "), copied.len(), steam_path.display()
    ))
}

/// 内核移除: 删除 3 个 DLL, 恢复备份
#[tauri::command]
pub fn kernel_remove() -> Result<String, String> {
    let steam_path = find_steam_path().ok_or("未找到 Steam 安装目录")?;

    let mut removed = Vec::new();
    for dll in &KERNEL_DLLS {
        let dst = steam_path.join(dll);
        if dst.exists() {
            // 恢复备份
            let backup = dst.with_extension("vortex_backup");
            if backup.exists() {
                std::fs::copy(&backup, &dst).map_err(|e| format!("恢复 {dll} 备份失败: {e}"))?;
                let _ = std::fs::remove_file(&backup);
            } else {
                std::fs::remove_file(&dst).map_err(|e| format!("删除 {dll} 失败: {e}"))?;
            }
            removed.push(dll.to_string());
        }
    }

    if removed.is_empty() {
        return Ok("Steam 目录中未发现 OpenSteamTool 内核 DLL, 无需移除。".into());
    }

    Ok(format!(
        "内核移除成功!\n\n已恢复/删除: {}\n\nSteam 路径: {}\n\n请重启 Steam 恢复原样。",
        removed.join(", "), steam_path.display()
    ))
}

/// 关闭 Steam 进程
#[tauri::command]
pub fn stop_steam() -> Result<String, String> {
    let _ = Command::new("taskkill")
        .args(["/F", "/IM", "steam.exe", "/T"])
        .output();
    // 等待进程退出
    std::thread::sleep(std::time::Duration::from_millis(1500));
    Ok("Steam 进程已关闭。".into())
}

/// 重启 Steam
#[tauri::command]
pub fn restart_steam() -> Result<String, String> {
    // 1. 关闭
    let _ = Command::new("taskkill")
        .args(["/F", "/IM", "steam.exe", "/T"])
        .output();
    std::thread::sleep(std::time::Duration::from_secs(2));

    // 2. 重新启动
    let steam_path = find_steam_path().ok_or("未找到 Steam 安装目录")?;
    let steam_exe = steam_path.join("steam.exe");
    Command::new(&steam_exe)
        .spawn()
        .map_err(|e| format!("启动 Steam 失败: {e}"))?;

    Ok(format!("Steam 已重启。\n\n路径: {}", steam_path.display()))
}

/// 检查 OpenSteamTool 更新 (force=true 忽略1小时冷却)
#[tauri::command]
pub async fn kernel_check_update(force: Option<bool>) -> Result<KernelUpdateInfo, String> {
    let force = force.unwrap_or(false);

    // 1 小时冷却 (非强制检查时)
    if !force {
        if let Some(elapsed) = last_update_check_time() {
            if elapsed.as_secs() < 3600 {
                return Ok(KernelUpdateInfo {
                    current_version: LOCAL_KERNEL_VERSION.into(),
                    has_update: false,
                    latest_version: String::new(),
                    download_url: String::new(),
                });
            }
        }
    }

    // 请求 GitHub API
    let client = reqwest::Client::builder()
        .user_agent("VortexDL")
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))?;

    let resp = client.get(GITHUB_RELEASE_API).send().await
        .map_err(|e| format!("请求 GitHub 失败: {e}"))?;
    let json: serde_json::Value = resp.json().await
        .map_err(|e| format!("解析 GitHub 响应失败: {e}"))?;

    let latest_version = json.get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim_start_matches('v')
        .to_string();

    let download_url = json.get("assets")
        .and_then(|a| a.as_array())
        .and_then(|arr| {
            // 名称转小写, 便于匹配
            let lower = |a: &serde_json::Value| -> String {
                a.get("name").and_then(|n| n.as_str()).unwrap_or("").to_lowercase()
            };
            // 优先取 Release 版 zip; 找不到则回退到任意 zip (防止 asset 命名变化导致空地址)
            arr.iter()
                .find(|a| { let l = lower(*a); l.contains("release") && l.ends_with(".zip") })
                .or_else(|| arr.iter().find(|a| lower(*a).ends_with(".zip")))
        })
        .and_then(|a| a.get("browser_download_url"))
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();

    // 比较版本
    let has_update = !latest_version.is_empty() && latest_version != LOCAL_KERNEL_VERSION;

    // 记录检查时间
    save_update_check_time();

    Ok(KernelUpdateInfo {
        current_version: LOCAL_KERNEL_VERSION.into(),
        has_update,
        latest_version,
        download_url,
    })
}

/// 下载并替换 OpenSteamTool 新版本
#[tauri::command]
pub async fn kernel_download_update(latest_version: String, download_url: String) -> Result<String, String> {
    if download_url.is_empty() {
        return Err("下载地址为空".into());
    }

    let client = reqwest::Client::builder()
        .user_agent("VortexDL")
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))?;

    let resp = client.get(&download_url).send().await
        .map_err(|e| format!("下载失败: {e}"))?;
    let bytes = resp.bytes().await
        .map_err(|e| format!("读取下载内容失败: {e}"))?;

    // 保存到 archives 目录
    let archives_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("archives");
    std::fs::create_dir_all(&archives_dir).map_err(|e| format!("创建 archives 目录失败: {e}"))?;
    let new_zip = archives_dir.join(format!("OpenSteamTool-{}-Release.zip", latest_version));
    std::fs::write(&new_zip, &bytes).map_err(|e| format!("保存压缩包失败: {e}"))?;

    // 删除旧版本 zip
    if let Ok(entries) = std::fs::read_dir(&archives_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if name.starts_with("opensteamtool") && name.ends_with(".zip") {
                let p = entry.path();
                if p != new_zip {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
    }

    // 同步到已安装目录 (如果存在)
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        let installed_archives = exe_dir.join("resources").join("archives");
        if installed_archives.exists() {
            let _ = std::fs::copy(&new_zip, installed_archives.join(new_zip.file_name().unwrap()));
        }
    }

    Ok(format!("OpenSteamTool v{} 下载完成, 已替换本地旧版本。", latest_version))
}

// ---------- 更新检查时间缓存 (文件存储) ----------

fn update_check_time_file() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("vortexdl_kernel_update_check")
}

fn last_update_check_time() -> Option<std::time::Duration> {
    let s = std::fs::read_to_string(update_check_time_file()).ok()?;
    let secs: u64 = s.trim().parse().ok()?;
    Some(std::time::Duration::from_secs(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs()
            .saturating_sub(secs)
    ))
}

fn save_update_check_time() {
    if let Ok(secs) = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
    {
        let _ = std::fs::write(update_check_time_file(), secs.to_string());
    }
}
