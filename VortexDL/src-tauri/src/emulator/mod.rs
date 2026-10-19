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
fn bundled_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(base) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        for cand in [
            base.join("resources_main").join("emulator"),
            base.join("resources").join("emulator"),
            base.join("emulator"),
        ] {
            if cand.join("citron.exe").exists() {
                out.push(cand);
            }
        }
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources_main")
        .join("emulator");
    if dev.join("citron.exe").exists() {
        out.push(dev);
    }
    out
}

fn bundled_dir() -> Option<PathBuf> {
    bundled_dirs().into_iter().next()
}

/// 把内置模拟器装到 `D:\game\switch\emulator`（幂等：已有 citron.exe 直接跳过）。
///
/// 目标目录里可能还放着用户自己的 Ryujinx/Yuzu，所以只按文件名逐个补，
/// 不删任何东西。
pub fn ensure_bundled() -> Result<(bool, PathBuf), String> {
    let dest = emu_dir();
    // 已经装过也要补一次「中文/主题」预设（老版本装的可能还没有 user 目录）
    let _ = seed_config();
    if dest.join("citron.exe").exists() {
        return Ok((false, dest));
    }
    let src = bundled_dir().ok_or_else(|| "安装包里没有内置模拟器".to_string())?;
    copy_tree(&src, &dest)?;
    let _ = seed_config();
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

// ============================================================
// 模拟器「魔改」：中文环境 / 主题色 / 中文窗口标题
// ============================================================
//
// # 能改到哪一步（说清楚，别吹）
//
// Citron 自己的菜单文字是**编译进 exe 的英文**，这个 nightly 构建没有附带
// 任何 Qt 翻译（exe 里既没有 `.qm` 魔数，也没有 `:/languages/` 资源路径），
// 所以外挂语言包做不到汉化，只能重编译上游（GPL-3.0 源码）。
// 能做的、而且是用户真正感知得到的：
//
//   1. 把 Switch 主机语言 / 区域设成简体中文 + 中国（**游戏内显示中文**）
//   2. 主题色改成和本软件一致的绯红，关掉控制台窗口、退出一律不弹确认
//   3. 模拟器运行数据放进它自己的 `user/` 便携目录（不污染 %APPDATA%\citron）
//   4. 启动后把窗口标题改成中文（`漩涡模拟器 · 游戏名`）
//
// 这几项都不改二进制，GPL 分发要求仍然满足（见同目录 README）。

/// Citron 的便携数据目录（exe 同级 `user/`）。有这个目录它就不去碰
/// `%APPDATA%\citron`，配置跟着软件走。
fn citron_ini() -> PathBuf {
    emu_dir().join("user").join("config").join("qt-config.ini")
}

/// 模拟器需要的 prod.keys 在哪儿（Switch 系统密钥，任天堂版权内容，软件不能自带）。
///
/// 找四个常见位置：我们的 `user/keys`、`user/keys` 上级、模拟器根目录、以及
/// 用户以前自己装过 Citron 时留在 `%APPDATA%\citron\keys` 的那份。
pub fn keys_path() -> Option<PathBuf> {
    let mut cands = vec![
        emu_dir().join("user").join("keys").join("prod.keys"),
        emu_dir().join("keys").join("prod.keys"),
    ];
    if let Ok(app) = std::env::var("APPDATA") {
        let base = PathBuf::from(app);
        cands.push(base.join("citron").join("keys").join("prod.keys"));
        cands.push(base.join("yuzu").join("keys").join("prod.keys"));
    }
    cands.into_iter().find(|p| p.exists())
}

/// 密钥放哪儿 —— 缺密钥时提示里用
pub fn keys_dir() -> PathBuf {
    emu_dir().join("user").join("keys")
}

/// 「魔改」预设。`language_index=15` = 简体中文（Switch 主机语言，决定游戏内
/// 文字），`region_index=4` = 中国（Citron 的 Region 枚举顺序是
/// Japan/Usa/Europe/Australia/China/Korea/Taiwan/Auto/Default）。
const PRESET: &[(&str, &str, &str)] = &[
    ("System", "language_index", "15"),
    ("System", "region_index", "4"),
    ("System", "use_docked_mode", "1"),
    ("Core", "use_multi_core", "true"),
    ("Renderer", "use_disk_shader_cache", "true"),
    ("Renderer", "use_asynchronous_gpu_emulation", "true"),
    ("UI", "theme", "colorful_dark"),
    ("UI", "accent_color", "#eb3333"),
    ("UI", "showConsole", "false"),
    ("UI", "confirmStop", "0"),
    ("UI", "pauseWhenInBackground", "false"),
    ("UI", "UIGameList\\game_list_grid_view", "true"),
    ("UI", "UIGameList\\game_icon_size", "64"),
];

/// 在 QSettings 的 INI 文本里改一条值，其余内容原样保留。
///
/// ★ 不用 `WritePrivateProfileStringW`：它按 ANSI 处理没有 BOM 的文件，
///   而 Qt 6 的 INI 是 UTF-8 —— 里面的中文游戏目录会被写成乱码。
fn upsert_ini(text: &str, section: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    let header = format!("[{}]", section);
    let mut sec: Option<(usize, usize)> = None;
    let mut cur: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        if t.starts_with('[') && t.ends_with(']') {
            if let Some(s) = cur {
                if sec.is_none() {
                    sec = Some((s, i));
                }
            }
            if t.eq_ignore_ascii_case(&header) {
                cur = Some(i);
            } else {
                cur = None;
            }
        }
    }
    if let Some(s) = cur {
        if sec.is_none() {
            sec = Some((s, lines.len()));
        }
    }

    let (sec_start, end) = match sec {
        Some(x) => x,
        None => {
            if !lines.is_empty() && !lines.last().map(|l| l.is_empty()).unwrap_or(true) {
                lines.push(String::new());
            }
            lines.push(header);
            lines.push(format!("{}={}", key, value));
            return lines.join("\r\n") + "\r\n";
        }
    };

    let vprefix = format!("{}=", key);
    let dprefix = format!("{}\\default=", key);
    let mut vline: Option<usize> = None;
    let mut dline: Option<usize> = None;
    for (i, line) in lines.iter().enumerate().take(end).skip(sec_start + 1) {
        if line.starts_with(&vprefix) {
            vline = Some(i);
        } else if line.starts_with(&dprefix) {
            dline = Some(i);
        }
    }
    if let Some(i) = vline {
        lines[i] = format!("{}={}", key, value);
    } else {
        let at = dline.map(|i| i + 1).unwrap_or(end);
        lines.insert(at, format!("{}={}", key, value));
    }
    if let Some(i) = dline {
        lines[i] = format!("{}\\default=false", key);
    }
    lines.join("\r\n") + "\r\n"
}

/// 首次释出内置模拟器时写一份中文预设；**已经有配置文件就不动**
/// （用户可能自己调过手柄映射/画质）。
pub fn seed_config() -> Result<bool, String> {
    if !emu_dir().join("citron.exe").exists() {
        return Ok(false);
    }
    let ini = citron_ini();
    if ini.exists() {
        return Ok(false);
    }
    apply_preset()?;
    Ok(true)
}

/// 把「魔改」预设写进 Citron 配置（其他已有设置全部保留）。
pub fn apply_preset() -> Result<String, String> {
    if !emu_dir().join("citron.exe").exists() {
        return Err("还没装内置模拟器（先点「重新安装模拟器」）".into());
    }
    let ini = citron_ini();
    if let Some(d) = ini.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("创建配置目录失败: {}", e))?;
    }
    let mut text = std::fs::read_to_string(&ini).unwrap_or_default();
    for (sec, k, v) in PRESET {
        text = upsert_ini(&text, sec, k, v);
    }
    std::fs::write(&ini, text).map_err(|e| format!("写配置失败: {}", e))?;
    // 顺手补一份中文使用说明（老版本装的模拟器目录里没有）
    let _ = ensure_guide();
    crate::app_logger::log_command("emulator", &format!("中文预设已写入: {}", ini.display()));
    Ok(ini.to_string_lossy().to_string())
}

/// 中文使用说明（菜单中英对照/密钥放哪/预设做了什么）。已有的不覆盖。
fn ensure_guide() -> Result<PathBuf, String> {
    let dst = emu_dir().join("使用说明.txt");
    if !dst.exists() {
        // ★ 软件目录下可能同时有旧版 resources_main（没有说明文件），
        //   所以要把所有候选目录都找一遍，不能只信第一个。
        for src in bundled_dirs() {
            let s = src.join("使用说明.txt");
            if s.exists() {
                std::fs::create_dir_all(emu_dir()).map_err(|e| e.to_string())?;
                std::fs::copy(&s, &dst).map_err(|e| format!("复制说明失败: {}", e))?;
                break;
            }
        }
    }
    Ok(dst)
}

/// 打开中文使用说明
pub fn open_guide() -> Result<(), String> {
    let p = ensure_guide()?;
    if !p.exists() {
        return Err("说明文件不存在（内置模拟器还没释出？）".into());
    }
    std::process::Command::new("explorer")
        .arg(&p)
        .spawn()
        .map_err(|e| format!("打开说明失败: {}", e))?;
    Ok(())
}

#[cfg(windows)]
unsafe extern "system" fn retitle_proc(hwnd: *mut std::ffi::c_void, lparam: isize) -> i32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible, SetWindowTextW,
    };
    let ctx = &*(lparam as *const (u32, *const u16));
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid == ctx.0 && IsWindowVisible(hwnd) != 0 && GetWindowTextLengthW(hwnd) > 0 {
        SetWindowTextW(hwnd, ctx.1);
    }
    1
}

/// 把模拟器主窗口标题改成中文。模拟器自己会在启动时反复改标题，所以多试几次。
#[cfg(windows)]
fn retitle_windows(pid: u32, game: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::EnumWindows;
    let mut buf: Vec<u16> = format!("漩涡模拟器 · {}", game).encode_utf16().collect();
    buf.push(0);
    let ctx: (u32, *const u16) = (pid, buf.as_ptr());
    unsafe {
        EnumWindows(Some(retitle_proc), &ctx as *const _ as isize);
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
    /// 找到 prod.keys 没有（没有的话游戏起不来）
    #[serde(default)]
    pub keys: bool,
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
    let keys = keys_path().is_some();
    match scan() {
        Some((p, name)) => EmuInfo {
            found: true,
            path: p.to_string_lossy().to_string(),
            name: name.to_string(),
            running: run.is_some(),
            game: run.map(|(_, g)| g).unwrap_or_default(),
            keys,
        },
        None => EmuInfo {
            found: false,
            path: String::new(),
            name: String::new(),
            running: run.is_some(),
            game: run.map(|(_, g)| g).unwrap_or_default(),
            keys,
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
    let _ = seed_config();
    let (exe, name) = scan().ok_or_else(|| {
        "没有找到 Switch 模拟器。\n\n请把模拟器解压到：\nD:\\game\\switch\\emulator\n\n\
         （支持 Ryujinx / Yuzu / Sudachi / Suyu / Citron / Eden）"
            .to_string()
    })?;
    let mut cmd = std::process::Command::new(&exe);
    // ★ 内置 Citron 没有 prod.keys 时，游戏一定起不来；它自己只会弹一个英文
    //   「The keys are missing…」。这里提前拦下来，用中文说清楚放哪儿。
    if exe.file_name().and_then(|x| x.to_str()).map(|n| n.eq_ignore_ascii_case("citron.exe")) == Some(true)
        && keys_path().is_none()
    {
        let dir = keys_dir();
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::process::Command::new("explorer").arg(&dir).spawn();
        return Err(format!(
            "内置模拟器还缺 prod.keys（Switch 系统密钥，属任天堂版权内容，软件不能自带）。\n\n\
             请把你自己导出的 prod.keys 放进这个目录（已帮你打开）：\n{}\n\n\
             放好后重新点「启动游戏」。",
            dir.display()
        ));
    }
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
    let pid = child.id();
    if let Ok(mut g) = RUNNING.lock() {
        *g = Some((pid, game_path.to_string()));
    }
    let title = Path::new(game_path)
        .file_stem()
        .and_then(|x| x.to_str())
        .unwrap_or("游戏")
        .to_string();
    // 窗口标题中文化（模拟器自己启动时会反复写标题，所以多试几次）
    #[cfg(windows)]
    {
        let t = title.clone();
        std::thread::spawn(move || {
            for _ in 0..4 {
                std::thread::sleep(std::time::Duration::from_millis(1600));
                retitle_windows(pid, &t);
            }
        });
    }
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

    #[test]
    fn upsert_ini_edits_only_target_key() {
        let t = "[System]\r\nlanguage_index\\default=true\r\nlanguage_index=1\r\nregion_index=1\r\n[UI]\r\ntheme=dark\r\n";
        let o = upsert_ini(t, "System", "language_index", "15");
        assert!(o.contains("language_index\\default=false"), "{o}");
        assert!(o.contains("language_index=15"), "{o}");
        assert!(o.contains("region_index=1"), "{o}");
        assert!(o.contains("[UI]\r\ntheme=dark"), "{o}");
        assert_eq!(o.matches("language_index=").count(), 1, "{o}");
    }

    #[test]
    fn upsert_ini_appends_missing_section() {
        let o = upsert_ini("[System]\r\nx=1\r\n", "UI", "theme", "colorful_dark");
        assert!(o.contains("[UI]\r\ntheme=colorful_dark"), "{o}");
        assert!(o.contains("x=1"), "{o}");
    }

    #[test]
    fn preset_uses_simplified_chinese() {
        let lang = PRESET.iter().find(|(s, k, _)| *s == "System" && *k == "language_index").unwrap();
        assert_eq!(lang.2, "15");
        let region = PRESET.iter().find(|(s, k, _)| *s == "System" && *k == "region_index").unwrap();
        assert_eq!(region.2, "4");
    }
}
