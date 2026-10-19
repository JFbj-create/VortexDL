//! 模拟器集成：启动 / 进程跟踪 / 输入层 / 顶部功能条。
//!
//! # 边界（必须说清楚）
//!
//! 这里**不包含任何 Switch 模拟器核心**（ARM64 翻译 + GPU 模拟 + 系统服务）。
//! 那是一个需要多年、数十万行代码的工程，而且是法律雷区（需要 Nintendo 的
//! 加密密钥）。本模块做的是「把用户已安装的模拟器用起来」：
//!
//! ```text
//!   点游戏卡片
//!        │
//!        ▼
//!   launch()            ← 起进程（Ryujinx / Yuzu / Sudachi / Suyu / Citron / Eden）
//!        │
//!        ▼
//!   overlay 窗口          ← 顶部功能条：最大化 / 全屏 / 设置 / 关闭
//!        │
//!        ▼
//!   input::poll()        ← 键盘 + 手柄统一成 Switch 键位（每帧读，不依赖焦点）
//! ```
//!
//! 输入层的价值：WebView2 没焦点时收不到键盘事件，Gamepad API 在 WebView2 里
//! 也不可用。所以键盘/手柄一律走 `GetAsyncKeyState` + `XInputGetState`，
//! 每帧几十次内存读，开销可以忽略。

pub mod input;
pub mod overlay;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub use input::{InputState, btn};

/// 支持的模拟器（按优先级）
const EMU_NAMES: &[(&str, &str)] = &[
    ("ryujinx.exe", "Ryujinx"),
    ("ryujinx.ava.exe", "Ryujinx (Ava)"),
    ("yuzu.exe", "Yuzu"),
    ("sudachi.exe", "Sudachi"),
    ("suyu.exe", "Suyu"),
    ("citron.exe", "Citron"),
    ("eden.exe", "Eden"),
];

pub fn switch_dir() -> PathBuf {
    PathBuf::from(r"D:\game\switch")
}

pub fn emu_dir() -> PathBuf {
    switch_dir().join("emulator")
}

/// 随安装包分发的模拟器（Citron 精简发行版）在软件目录下的位置。
///
/// 安装包把它释放到 `<软件目录>\resources_main\emulator\`，首次运行整目录复制到
/// `D:\game\switch\emulator` —— 用户装完软件就能直接点游戏启动，不用自己去下模拟器。
fn bundled_dir() -> Option<PathBuf> {
    let base = std::env::current_exe().ok()?.parent().map(|d| d.to_path_buf())?;
    for cand in [
        base.join("resources_main").join("emulator"),
        base.join("resources").join("emulator"),
        base.join("emulator"),
    ] {
        if cand.join("citron.exe").exists() {
            return Some(cand);
        }
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources_main")
        .join("emulator");
    if dev.join("citron.exe").exists() {
        return Some(dev);
    }
    None
}

/// 把内置模拟器装到 `D:\game\switch\emulator`（幂等：已有 citron.exe 直接跳过）。
///
/// 目标目录里可能还放着用户自己的 Ryujinx/Yuzu，所以只按文件名逐个补，
/// 不删任何东西。
pub fn ensure_bundled() -> Result<(bool, PathBuf), String> {
    let dest = emu_dir();
    if dest.join("citron.exe").exists() {
        return Ok((false, dest));
    }
    let src = bundled_dir().ok_or_else(|| "安装包里没有内置模拟器".to_string())?;
    copy_tree(&src, &dest)?;
    crate::app_logger::log_command(
        "emulator",
        &format!("内置模拟器已安装: {} -> {}", src.display(), dest.display()),
    );
    Ok((true, dest))
}

fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("创建目录失败 {}: {}", dst.display(), e))?;
    let rd = std::fs::read_dir(src).map_err(|e| format!("读取目录失败 {}: {}", src.display(), e))?;
    for e in rd.flatten() {
        let from = e.path();
        let to = dst.join(e.file_name());
        if from.is_dir() {
            copy_tree(&from, &to)?;
        } else if !same_size(&from, &to) {
            std::fs::copy(&from, &to).map_err(|e| format!("复制 {} 失败: {}", from.display(), e))?;
        }
    }
    Ok(())
}

fn same_size(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.len() == y.len(),
        _ => false,
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmuInfo {
    pub found: bool,
    pub path: String,
    pub name: String,
    /// 是否正在运行游戏
    pub running: bool,
    pub game: String,
}

static RUNNING: Mutex<Option<(u32, String)>> = Mutex::new(None);

/// 扫描已安装的模拟器
pub fn scan() -> Option<(PathBuf, &'static str)> {
    let root = emu_dir();
    if !root.exists() {
        return None;
    }
    let mut stack = vec![(root, 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() {
                let n = p.file_name().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
                for (fname, label) in EMU_NAMES {
                    if n == *fname {
                        return Some((p, label));
                    }
                }
            } else if p.is_dir() && depth < 3 {
                stack.push((p, depth + 1));
            }
        }
    }
    None
}

pub fn info() -> EmuInfo {
    let run = RUNNING.lock().map(|g| g.clone()).unwrap_or(None);
    match scan() {
        Some((p, name)) => EmuInfo {
            found: true,
            path: p.to_string_lossy().to_string(),
            name: name.to_string(),
            running: run.is_some(),
            game: run.map(|(_, g)| g).unwrap_or_default(),
        },
        None => EmuInfo {
            found: false,
            path: String::new(),
            name: String::new(),
            running: run.is_some(),
            game: run.map(|(_, g)| g).unwrap_or_default(),
        },
    }
}

/// 启动游戏。
///
/// 各模拟器的命令行约定一致：`<emu.exe> <rom路径>`。找不到模拟器时给出可操作的提示。
pub fn launch(game_path: &str) -> Result<String, String> {
    if !Path::new(game_path).exists() {
        return Err(format!("游戏文件不存在: {}", game_path));
    }
    let (exe, name) = scan().ok_or_else(|| {
        "没有找到 Switch 模拟器。\n\n请把模拟器解压到：\nD:\\game\\switch\\emulator\n\n\
         （支持 Ryujinx / Yuzu / Sudachi / Suyu / Citron / Eden）"
            .to_string()
    })?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg(game_path);
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP: 不跟主程序共用控制台
        cmd.creation_flags(0x0000_0200);
    }
    let child = cmd.spawn().map_err(|e| format!("启动 {} 失败: {}", name, e))?;
    if let Ok(mut g) = RUNNING.lock() {
        *g = Some((child.id(), game_path.to_string()));
    }
    let title = Path::new(game_path)
        .file_stem()
        .and_then(|x| x.to_str())
        .unwrap_or("游戏")
        .to_string();
    crate::app_logger::log_command(
        "emulator",
        &format!("launch name={} exe={} game={}", name, exe.display(), game_path),
    );
    Ok(title)
}

pub fn stop() -> Result<(), String> {
    let pid = RUNNING.lock().ok().and_then(|g| g.as_ref().map(|(p, _)| *p));
    let pid = match pid {
        Some(p) => p,
        None => return Ok(()),
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut c = std::process::Command::new("taskkill");
        c.args(["/PID", &pid.to_string(), "/F"]);
        c.creation_flags(0x0800_0000);
        let _ = c.output();
    }
    if let Ok(mut g) = RUNNING.lock() {
        *g = None;
    }
    Ok(())
}

/// 检查进程是否还活着（前端定时调，用来收起顶部功能条）
pub fn alive() -> bool {
    #[cfg(windows)]
    {
        let pid = RUNNING.lock().ok().and_then(|g| g.as_ref().map(|(p, _)| *p));
        let pid = match pid {
            Some(p) => p,
            None => return false,
        };
        use std::os::windows::process::CommandExt;
        let mut c = std::process::Command::new("tasklist");
        c.args(["/FI", &format!("PID eq {}", pid), "/NH"]);
        c.creation_flags(0x0800_0000);
        let out = c.output();
        match out {
            Ok(o) => {
                let s = String::from_utf8_lossy(&o.stdout);
                let alive = s.contains(&pid.to_string());
                if !alive {
                    if let Ok(mut g) = RUNNING.lock() {
                        *g = None;
                    }
                }
                alive
            }
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

// ============================================================
// 前端配置（画面设置等都放这里，不碰模拟器自己的配置文件）
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmuSettings {
    /// 启动后自动全屏顶部条
    pub show_overlay: bool,
    /// 顶部条自动隐藏（鼠标移上去才出现）
    pub auto_hide: bool,
    /// 目标帧率上限（仅记录，供前端显示）
    pub fps_limit: u32,
    /// 分辨率缩放（仅记录）
    pub scale: f32,
    /// 手柄震动
    pub rumble: bool,
    /// 虚拟按键提示显示在画面上
    pub on_screen_hint: bool,
}

impl Default for EmuSettings {
    fn default() -> Self {
        Self {
            show_overlay: true,
            auto_hide: true,
            fps_limit: 60,
            scale: 1.0,
            rumble: true,
            on_screen_hint: true,
        }
    }
}

fn settings_path() -> PathBuf {
    crate::paths::data_dir().join("emulator").join("settings.json")
}

pub fn load_settings() -> EmuSettings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_settings(s: &EmuSettings) -> Result<(), String> {
    let p = settings_path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    std::fs::write(&p, serde_json::to_string_pretty(s).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}


// ============================================================
// 模拟器窗口操作（最大化 / 全屏）
// ============================================================

/// 按 PID 找到模拟器主窗口并切换最大化
pub fn toggle_maximize() -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, ShowWindow, SW_MAXIMIZE};
        let pid = RUNNING
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|(p, _)| *p))
            .ok_or_else(|| "游戏没在运行".to_string())?;
        let mut found: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut ctx = (pid, &mut found as *mut *mut std::ffi::c_void);
        unsafe {
            EnumWindows(Some(enum_proc), &mut ctx as *mut _ as isize);
            if !found.is_null() {
                ShowWindow(found, SW_MAXIMIZE);
            }
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err("仅支持 Windows".into())
    }
}

#[cfg(windows)]
unsafe extern "system" fn enum_proc(
    hwnd: *mut std::ffi::c_void,
    lparam: isize,
) -> i32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible,
    };
    let ctx = &mut *(lparam as *mut (u32, *mut *mut std::ffi::c_void));
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid == ctx.0 && IsWindowVisible(hwnd) != 0 && GetWindowTextLengthW(hwnd) > 0 {
        *ctx.1 = hwnd;
        return 0;
    }
    1
}

/// 切换全屏（最大化 + 隐藏标题栏的等价做法：用 F11 语义交给模拟器；这里只做最大化）
pub fn toggle_fullscreen() -> Result<(), String> {
    toggle_maximize()
}


/// 把下载目录注册到模拟器的「游戏目录」（只写我们自己的清单，不动模拟器配置）
///
/// ★ 为什么不去改 Ryujinx/Yuzu 的配置文件：它们的格式在每个版本都变，
///   写坏一次用户的模拟器就打不开了。这里维护自己的清单，
///   模拟器里让用户把 D:\game\switch 加一次即可（或在模拟器页看到完整列表）。
pub fn register_game(dir: &str) -> Result<(), String> {
    let p = crate::paths::data_dir().join("emulator").join("games.json");
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let mut list: Vec<String> = std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let d = dir.trim().to_string();
    if !d.is_empty() && !list.contains(&d) {
        list.push(d);
        std::fs::write(&p, serde_json::to_string_pretty(&list).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_roundtrip() {
        let s = EmuSettings::default();
        let j = serde_json::to_string(&s).unwrap();
        let back: EmuSettings = serde_json::from_str(&j).unwrap();
        assert_eq!(back.fps_limit, 60);
        assert!(back.show_overlay);
    }

    #[test]
    fn info_does_not_panic_without_emulator() {
        let i = info();
        assert!(i.path.is_empty() || i.found);
    }
}
