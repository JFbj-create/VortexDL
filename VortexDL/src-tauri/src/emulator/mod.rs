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

/// 模拟器装在哪。
///
/// ★★ 2026-10-20 按用户要求改成**软件本地**：`<软件目录>\emulator`，
///   和安装包一起装（安装包里已经带了一份 `resources_main\emulator`）。
///   理由：模拟器是软件的一部分，不该混在游戏目录里；重装/迁移游戏目录时
///   也不会把模拟器（连同密钥、配置、着色器缓存）一起搬走。
///
///   兼容老用户：如果软件目录下没有、但老的 `D:\game\switch\emulator` 有，
///   就继续用老位置（不强制搬迁，避免用户已经下好的着色器缓存白费）。
pub fn emu_dir() -> PathBuf {
    if let Some(base) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        return base.join("emulator");
    }
    switch_dir().join("emulator")
}

/// 老的模拟器位置（D:\game\switch\emulator）。只用于**一次性迁移**。
pub fn legacy_emu_dir() -> PathBuf {
    switch_dir().join("emulator")
}

/// 把老位置的模拟器搬到软件目录（用户要求「放到软件本地和安装包一起装」）。
///
/// 迁移的是**整个目录**（含 user/ 配置、密钥、着色器缓存），不做删除：
/// 复制成功后才把老目录改名成 `emulator.migrated`，用户可以自己删。
pub fn migrate_legacy() -> Result<bool, String> {
    let new = emu_dir();
    if new.join("citron.exe").exists() {
        return Ok(false);
    }
    let old = legacy_emu_dir();
    if !old.join("citron.exe").exists() {
        return Ok(false);
    }
    copy_tree(&old, &new)?;
    if !new.join("citron.exe").exists() {
        return Err("迁移后新位置没有 citron.exe".into());
    }
    // ★★ 迁移后必须核对文件数与总字节：实测 `same_size` 判等会漏掉
    //   zstd.dll / zlib1.dll 这类文件（目标已存在同名 0 字节占位或多份旧副本时），
    //   结果模拟器起不来却一句提示都没有（报 "error while loading shared libraries"）。
    let (on, ob) = dir_fingerprint(&old);
    let (nn, nb) = dir_fingerprint(&new);
    if nn < on || nb < ob {
        // 缺东西 → 补齐（逐文件按大小重拷一遍）
        copy_tree(&old, &new)?;
        let (nn2, nb2) = dir_fingerprint(&new);
        if nn2 < on || nb2 < ob {
            return Err(format!(
                "迁移不完整：源 {} 个文件/{} 字节，目标 {} 个文件/{} 字节",
                on, ob, nn2, nb2
            ));
        }
    }
    // 老目录改名，不直接删（里面可能有用户的存档/密钥）
    //
    // ★ 不能用 with_extension("migrated")：`.../switch/emulator` 没有扩展名时
    //   它会得到 `.../switch/emulator.migrated`，但如果末段本身带点（如 emulator-x64）
    //   就会被替换掉一部分。直接用加后缀的字符串更稳。
    let bak = PathBuf::from(format!("{}.migrated", old.display()));
    if !bak.exists() {
        let _ = std::fs::rename(&old, &bak);
    }
    crate::app_logger::log_command(
        "emulator",
        &format!(
            "模拟器已迁移到软件目录: {} -> {}（旧目录改名 {})",
            old.display(),
            new.display(),
            bak.display()
        ),
    );
    Ok(true)
}

/// 随安装包分发的模拟器（Citron 精简发行版）在软件目录下的位置。
///
/// 安装包把它释放到 `<软件目录>\resources_main\emulator\`，首次运行整目录复制到
/// `D:\game\switch\emulator` —— 用户装完软件就能直接点游戏启动，不用自己去下模拟器。
fn bundled_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(base) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        // ★ 不要加 `base.join("emulator")`：那是**安装目标**（emu_dir），
        //   它一旦出现在「源」列表里，ensure_bundled 就会把目录往自己身上复制。
        for cand in [
            base.join("resources_main").join("emulator"),
            base.join("resources").join("emulator"),
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
    // 先把老位置（D:\game\switch\emulator）搬过来
    let _ = migrate_legacy();
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

/// 统计目录里的文件数和总字节数（给迁移做完整性校验）
pub fn dir_fingerprint(dir: &Path) -> (usize, u64) {
    let mut n = 0usize;
    let mut bytes = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = match std::fs::read_dir(&d) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(md) = e.metadata() {
                n += 1;
                bytes += md.len();
            }
        }
    }
    (n, bytes)
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

/// 这是不是一份真的 prod.keys？
///
/// Switch 的密钥文件是纯文本，每行形如 `master_key_00 = 00112233...`。
/// 只做**格式校验**，不校验内容对不对（内容对不对要等模拟器自己去验）。
fn looks_like_prod_keys(text: &str) -> bool {
    let mut has_master = false;
    let mut pairs = 0;
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') || l.starts_with(';') {
            continue;
        }
        let (k, v) = match l.split_once('=') {
            Some(x) => x,
            None => continue,
        };
        let k = k.trim();
        let v = v.trim();
        if k.is_empty() || v.len() < 16 {
            continue;
        }
        if !v.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        pairs += 1;
        if k.eq_ignore_ascii_case("master_key_00") {
            has_master = true;
        }
    }
    has_master && pairs >= 5
}

/// 在某个目录里找可能的密钥文件（给"一键导入"做候选）。
///
/// 只看两层：用户多半是把 keys 解压在下载目录或桌面。
pub fn find_key_files(dir: &str) -> Vec<String> {
    let mut out = Vec::new();
    let root = Path::new(dir);
    if !root.is_dir() {
        return out;
    }
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((d, depth)) = stack.pop() {
        let rd = match std::fs::read_dir(&d) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if depth < 2 {
                    stack.push((p, depth + 1));
                }
                continue;
            }
            let n = p.file_name().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
            if n == "prod.keys" || n == "prod.keys.txt" {
                out.push(p.to_string_lossy().to_string());
            } else if n.ends_with(".keys") {
                // 别的 .keys 也可能是（有些教程把它改名），一并列出来让用户自己认
                if let Ok(t) = std::fs::read_to_string(&p) {
                    if looks_like_prod_keys(&t) {
                        out.push(p.to_string_lossy().to_string());
                    }
                }
            }
        }
    }
    out
}

/// 导入用户自己的 prod.keys（用户明确知道这是什么、从哪儿来的）。
///
/// ★ 只搬用户**已经拥有的**文件：这里既不生成也不下载密钥（那属于任天堂版权
///   内容）。做法是把选定文件复制成 `user/keys/prod.keys`，并校验格式 ——
///   选错文件时直接说清楚，而不是让模拟器过一会儿用英文报错。
pub fn import_keys(src: &str) -> Result<String, String> {
    let from = Path::new(src);
    if !from.is_file() {
        return Err(format!("找不到这个文件: {}", src));
    }
    let raw = std::fs::read_to_string(from)
        .map_err(|_| "这个文件不是文本格式（密钥文件应该是纯文本）".to_string())?;
    if !looks_like_prod_keys(&raw) {
        return Err(
            "这个文件不像是 prod.keys。\n\n\
             正确的内容长这样（每行一条，必须包含 master_key_00）：\n\
             master_key_00 = 00000000000000000000000000000000\n\
             ..."
                .to_string(),
        );
    }
    let dst_dir = keys_dir();
    std::fs::create_dir_all(&dst_dir).map_err(|e| format!("创建密钥目录失败: {}", e))?;
    let dst = dst_dir.join("prod.keys");
    std::fs::copy(from, &dst).map_err(|e| format!("复制密钥失败: {}", e))?;
    // title.keys 有就一起带上（部分游戏需要，没有也能跑）
    if let Some(parent) = from.parent() {
        let t = parent.join("title.keys");
        if t.is_file() {
            let _ = std::fs::copy(&t, dst_dir.join("title.keys"));
        }
    }
    crate::app_logger::log_command(
        "emulator",
        &format!("已导入 prod.keys: {} -> {}", from.display(), dst.display()),
    );
    Ok(dst.to_string_lossy().to_string())
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
    // 关掉启动时的「有新版本」弹框（它会在游戏画面前盖一层英文框）
    ("UI", "check_for_updates_on_start", "false"),
    ("WebService", "enable_auto_update_check", "false"),
    ("UI", "UIGameList\\game_list_grid_view", "true"),
    ("UI", "UIGameList\\game_icon_size", "64"),
    // ★★ 打开 Citron 自带的逐帧基准日志：它每帧往 stdout 打一行
    //   `[UltraLowBenchmark] system_fps=… game_fps=… frametime_ms=…`。
    //   这是**唯一不需要注入就能拿到真实帧率**的来源（我们启动时把 stdout
    //   重定向到 data/fps/emu_stdout.log，帧率记录器从中解析）。
    ("Debugging", "ultralow_benchmark_logging", "true"),
];

/// 在 QSettings 的 INI 文本里改一条值，其余内容原样保留。
///
/// ★ 不用 `WritePrivateProfileStringW`：它按 ANSI 处理没有 BOM 的文件，
///   而 Qt 6 的 INI 是 UTF-8 —— 里面的中文游戏目录会被写成乱码。
/// ★★ 值以 `#` / `;` 开头（比如主题色 `#eb3333`）时必须加双引号，
///    否则 Qt 会当成注释，读回来是空值 —— 实测会静默丢主题色。
fn ini_value(value: &str) -> String {
    let need_quote = value.starts_with('#')
        || value.starts_with(';')
        || value.starts_with('"')
        || value.trim() != value
        || value.contains(',');
    if need_quote && !(value.starts_with('"') && value.ends_with('"')) {
        format!("\"{}\"", value)
    } else {
        value.to_string()
    }
}

fn upsert_ini(text: &str, section: &str, key: &str, value: &str) -> String {
    let value = &ini_value(value);
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

/// 把 `game_path` 同目录下的**数据文件夹**接到模拟器 SD 卡根（`user/sdmc`）。
///
/// # 为什么需要这一步（实测结论，别再猜）
///
/// OutRun2006-NX 这类「移植游戏」的 NRO 是**自己去找数据根**的：它先看 romfs，
/// 再看 `sdmc`（Switch 的 SD 卡根）。实测日志里 `home=` / `(started from: .)`
/// 证明它只认这两个位置 —— 把 Windows 工作目录设成 ROM 所在目录**没用**。
///
/// 所以正确做法是：把 ROM 旁边那些数据目录（`Scripts/ Stage/ CARS/ ...`）
/// 以**目录联接（junction）**的方式挂到 `user/sdmc/` 下 ——
/// junction 不复制文件、不占空间，Windows 上看就是普通目录。
///
/// 只挂「看起来像游戏数据」的目录，并且**一个游戏一张记分牌**：
/// 换游戏时先把上一次挂的拆掉，避免不同游戏的数据互相污染 SD 卡根。
#[cfg(windows)]
pub fn link_data_into_sdmc(game_path: &str) -> Result<Vec<String>, String> {
    let rom = Path::new(game_path);
    let rom_dir = rom
        .parent()
        .ok_or_else(|| "游戏文件没有上级目录".to_string())?;
    let sdmc = emu_dir().join("user").join("sdmc");
    std::fs::create_dir_all(&sdmc).map_err(|e| format!("创建 sdmc 目录失败: {}", e))?;

    // 先拆掉上次挂的（只拆我们记过名字的，绝不碰真实目录）
    let log = sdmc.join(".vx_linked.json");
    if let Ok(txt) = std::fs::read_to_string(&log) {
        if let Ok(old) = serde_json::from_str::<Vec<String>>(&txt) {
            for name in old {
                let p = sdmc.join(&name);
                if p.is_file() {
                    // 硬链接文件：删链接本身不会删源文件
                    let _ = std::fs::remove_file(&p);
                } else if is_junction(&p) {
                    let _ = unjunction(&p);
                }
            }
        }
    }

    // 这些是「数据目录」的典型名字；命中就把整个目录挂上去
    const DATA_DIRS: &[&str] = &[
        "Scripts", "Stage", "CARS", "Cars", "Sprite", "Sprani", "OCP", "OBJ",
        "Chr", "Driver", "Common", "Sound", "Media", "Text", "Anims", "AS",
        "BK", "Ghosts", "Motion", "OSO", "RVT", "config_text", "SaveGame", "mv",
    ];
    let mut linked = Vec::new();
    let rd = match std::fs::read_dir(rom_dir) {
        Ok(r) => r,
        Err(_) => return Ok(linked),
    };
    for e in rd.flatten() {
        let p = e.path();
        let name = match p.file_name().and_then(|x| x.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        // ★ 单个文件也要挂：移植游戏的 NRO 会去找**原版主程序 exe**
        //   （实测 OutRunSwitch 报 `no OR2006C2C.EXE in .`，数据目录都对了也起不来）。
        //   方案：把 ROM 旁边那几个 .exe / .dll / .ini 一起硬链接进 sdmc 根。
        //   用**硬链接**（同盘不占空间，跨盘会自动退化成复制），因为 junction 只能对目录。
        if p.is_file() {
            let low = name.to_lowercase();
            let want = low.ends_with(".exe")
                || low.ends_with(".dll")
                || low == "outrun2006.ini"
                || low.ends_with(".dat");
            if !want {
                continue;
            }
            let dst = sdmc.join(&name);
            if dst.exists() {
                continue;
            }
            if link_or_copy_file(&p, &dst) {
                linked.push(name);
            }
            continue;
        }

        if !DATA_DIRS.iter().any(|d| d.eq_ignore_ascii_case(&name)) {
            continue;
        }
        let dst = sdmc.join(&name);
        if dst.exists() {
            // 已经有同名的（真目录、沿用旧位置复制过来的、或别处的联接）：
            // 不覆盖，但要记进清单，换游戏时才知道哪些是我们挂的。
            //
            // ★ 只有「确实指向本次游戏目录」的才算我们的：用 canonicalize 比对，
            //   避免把用户自己的真实目录误删。
            let same = std::fs::canonicalize(&dst).ok() == std::fs::canonicalize(&p).ok();
            if same {
                linked.push(name);
            }
            continue;
        }
        if junction(&p, &dst) {
            linked.push(name);
        }
    }
    let _ = std::fs::write(
        &log,
        serde_json::to_string(&linked).unwrap_or_else(|_| "[]".into()),
    );
    if !linked.is_empty() {
        crate::app_logger::log_command(
            "emulator",
            &format!("数据目录已挂到 sdmc: {:?} (来自 {})", linked, rom_dir.display()),
        );
    }
    Ok(linked)
}

/// 建目录联接（junction）。
///
/// ★★ 不能用 `std::os::windows::fs::symlink_dir`：那是**符号链接**，
///   需要开发者模式或管理员权限，普通用户会静默失败（实测返回 Err 但很容易被忽略）。
///   目录联接（junction）用 `mklink /J`，**普通用户就能建**，不占额外空间。
#[cfg(windows)]
fn junction(target: &Path, link: &Path) -> bool {
    use std::os::windows::process::CommandExt;
    let mut c = std::process::Command::new("cmd");
    c.args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    match c.output() {
        Ok(o) => o.status.success() && link.exists(),
        Err(_) => false,
    }
}

/// 把 ROM 旁边的辅助文件（原版 exe / dll / ini）放进 sdmc 根。
///
/// 优先硬链接（同盘不占空间）；**跨盘时 mklink /H 会直接失败**（实测 D: → F:
/// 就是这个情况），所以退化成复制。这些文件通常只有几 MB，复制代价可以接受。
#[cfg(windows)]
fn link_or_copy_file(target: &Path, link: &Path) -> bool {
    use std::os::windows::process::CommandExt;
    let mut c = std::process::Command::new("cmd");
    c.args(["/C", "mklink", "/H"]).arg(link).arg(target)
        .creation_flags(0x0800_0000);
    if let Ok(o) = c.output() {
        if o.status.success() && link.exists() {
            return true;
        }
    }
    std::fs::copy(target, link).is_ok()
}

#[cfg(windows)]
fn unjunction(link: &Path) -> bool {
    use std::os::windows::process::CommandExt;
    let mut c = std::process::Command::new("cmd");
    c.args(["/C", "rmdir"]).arg(link).creation_flags(0x0800_0000);
    matches!(c.output(), Ok(o) if o.status.success())
}

/// 判断是不是 junction（避免误删用户真实目录）
#[cfg(windows)]
fn is_junction(p: &Path) -> bool {
    use std::os::windows::process::CommandExt;
    let mut c = std::process::Command::new("cmd");
    c.args(["/C", "dir", "/AL"]).arg(p.parent().unwrap_or(p));
    c.creation_flags(0x0800_0000);
    match c.output() {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout).to_lowercase();
            let name = p.file_name().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
            s.contains("<junction>") && s.contains(&name)
        }
        Err(_) => false,
    }
}

#[cfg(not(windows))]
pub fn link_data_into_sdmc(_game_path: &str) -> Result<Vec<String>, String> {
    Ok(Vec::new())
}

/// 把模拟器主窗口标题改成中文。模拟器自己会在启动时反复改标题，所以多试几次。
/// 移植游戏的「原版数据源」自检。
///
/// # 为什么需要（实测 OutRun2006-NX 的坑）
///
/// 资源站的「移植游戏」包里常常同时放着一个 `OR2006C2C.EXE` 之类的**原版 exe**，
/// NRO 会从里面提取着色器（NRO 里的报错阶段就叫 `exe-image`）。
/// 但有些包里那个 exe 是**加密/占位**的（实测：962 KB、`.text` 熵 8.0，
/// 真原版 6 MB 左右、熵只有 6.x）—— 数据目录全对也照样起不来，
/// 界面上只留一段看不清的黑屏报错。
///
/// 这里只做**只读检测**，不修改任何东西：发现可疑就在启动后给前端一条中文提示。
#[cfg(windows)]
fn suspicious_original_exe(game_path: &str) -> Option<String> {
    let dir = Path::new(game_path).parent()?;
    let rd = std::fs::read_dir(dir).ok()?;
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        let name = p.file_name()?.to_str()?.to_lowercase();
        if !name.ends_with(".exe") {
            continue;
        }
        // 只看明显像「原版游戏主程序」的
        if !(name.starts_with("or2006") || name.starts_with("outrun")) {
            continue;
        }
        let data = match std::fs::read(&p) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if data.len() < 32 || &data[..2] != b"MZ" {
            continue;
        }
        // 取 .text 段的熵：占位/加密镜像接近 8.0
        let entropy = pe_text_entropy(&data);
        if let Some(h) = entropy {
            if h > 7.6 {
                return Some(format!(
                    "《{}》这个包里带的 {}（{:.0} KB）看起来是加密/占位文件（代码段熵 {:.1}），\
                     不是真正的原版主程序。这类移植游戏需要从原版 exe 里提取着色器，\
                     所以它会停在黑屏并报 exe-image 错误 —— 换一个资源重新下载即可。",
                    Path::new(game_path)
                        .file_stem()
                        .and_then(|x| x.to_str())
                        .unwrap_or("该游戏"),
                    p.file_name().and_then(|x| x.to_str()).unwrap_or("exe"),
                    data.len() as f64 / 1024.0,
                    h
                ));
            }
        }
    }
    None
}

/// 粗算 PE 里第一个非空代码段（.text）的字节熵。
#[cfg(windows)]
fn pe_text_entropy(d: &[u8]) -> Option<f64> {
    if d.len() < 0x40 || &d[..2] != b"MZ" {
        return None;
    }
    let e = u32::from_le_bytes(d.get(0x3c..0x40)?.try_into().ok()?) as usize;
    if d.len() < e + 24 || &d[e..e + 4] != b"PE\0\0" {
        return None;
    }
    let nsec = u16::from_le_bytes(d.get(e + 6..e + 8)?.try_into().ok()?) as usize;
    let opt_size = u16::from_le_bytes(d.get(e + 20..e + 22)?.try_into().ok()?) as usize;
    let sec_off = e + 24 + opt_size;
    for i in 0..nsec.min(16) {
        let s = sec_off + i * 40;
        if d.len() < s + 40 {
            break;
        }
        let name = &d[s..s + 8];
        let rsize = u32::from_le_bytes(d.get(s + 16..s + 20)?.try_into().ok()?) as usize;
        let roff = u32::from_le_bytes(d.get(s + 20..s + 24)?.try_into().ok()?) as usize;
        if !name.starts_with(b".text") || rsize < 4096 || roff + rsize > d.len() {
            continue;
        }
        let body = &d[roff..roff + rsize];
        let mut cnt = [0u32; 256];
        for b in body {
            cnt[*b as usize] += 1;
        }
        let n = body.len() as f64;
        let mut h = 0.0;
        for c in cnt {
            if c > 0 {
                let p = c as f64 / n;
                h -= p * p.log2();
            }
        }
        return Some(h);
    }
    None
}

#[cfg(not(windows))]
fn suspicious_original_exe(_game_path: &str) -> Option<String> {
    None
}

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

/// 关掉模拟器自己弹的**英文**提示框（密钥提示 / 有新版本）。
///
/// ★ 2026-10-20 实测：那两个框都只是**警告**，点掉之后游戏照常跑
///   （`.nro` 自制程序根本不需要 prod.keys）。所以不能因为「没有密钥」
///   就不让用户启动 —— 该做的是把这两个挡路的框自动关掉。
#[cfg(windows)]
unsafe extern "system" fn dismiss_proc(hwnd: *mut std::ffi::c_void, lparam: isize) -> i32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
        PostMessageW,
    };
    const WM_CLOSE: u32 = 0x0010;
    let ctx = &*(lparam as *const u32);
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid != *ctx || IsWindowVisible(hwnd) == 0 || GetWindowTextLengthW(hwnd) == 0 {
        return 1;
    }
    let mut buf = [0u16; 256];
    let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if n <= 0 {
        return 1;
    }
    let title = String::from_utf16_lossy(&buf[..n as usize]);
    const NOISE: &[&str] = &[
        "Derivation Components Missing",
        "Update Available",
        "Encryption keys are missing",
    ];
    if NOISE.iter().any(|t| title.contains(t)) {
        PostMessageW(hwnd, WM_CLOSE, 0, 0);
    }
    1
}

/// 启动后的一小段时间里反复关掉那些英文提示框（它们出现的时间不固定）
#[cfg(windows)]
fn dismiss_noise(pid: u32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::EnumWindows;
    std::thread::spawn(move || {
        for _ in 0..14 {
            std::thread::sleep(std::time::Duration::from_millis(900));
            let ctx: u32 = pid;
            unsafe {
                EnumWindows(Some(dismiss_proc), &ctx as *const u32 as isize);
            }
        }
    });
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmuInfo {
    pub found: bool,
    pub path: String,
    pub name: String,
    /// 是否正在运行游戏
    pub running: bool,
    pub game: String,
    /// 找到 prod.keys 没有（官方游戏需要）
    #[serde(default)]
    pub keys: bool,
    /// 启动时发现的问题（包本身有问题等）。前端调 emu_take_warning 取走。
    #[serde(default)]
    pub warning: String,
}

/// 弹出 Windows 的「打开文件」对话框，让用户选自己的 prod.keys。
///
/// 必须在**后台线程**里调：这个对话框是模态的，会跑自己的消息循环；
/// 直接在 Tauri 命令里调会卡住主线程。
#[cfg(windows)]
pub fn pick_key_file() -> Result<String, String> {
    use windows_sys::Win32::UI::Controls::Dialogs::{GetOpenFileNameW, OPENFILENAMEW};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;

    let mut buf = vec![0u16; 1024];
    let filter: Vec<u16> = "密钥文件 (*.keys;*.txt)\0*.keys;*.txt\0所有文件 (*.*)\0*.*\0\0"
        .encode_utf16()
        .collect();
    let title: Vec<u16> = "选择你自己的 prod.keys".encode_utf16().chain(Some(0)).collect();
    // 默认打开桌面/下载目录（用户多半放那儿）
    let init = std::env::var("USERPROFILE")
        .map(|p| PathBuf::from(p).join("Downloads"))
        .ok()
        .filter(|p| p.is_dir())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut init_buf: Vec<u16> = init.encode_utf16().collect();
    init_buf.resize(init.len() + 1, 0);

    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: unsafe { GetActiveWindow() },
        lpstrFilter: filter.as_ptr(),
        nFilterIndex: 1,
        lpstrFile: buf.as_mut_ptr(),
        nMaxFile: buf.len() as u32,
        lpstrTitle: title.as_ptr(),
        lpstrInitialDir: if init.is_empty() { std::ptr::null() } else { init_buf.as_ptr() },
        Flags: 0x0000_0800 | 0x0000_1000 | 0x0000_0004, // PATHMUSTEXIST|FILEMUSTEXIST|HIDEREADONLY
        ..unsafe { std::mem::zeroed() }
    };
    let ok = unsafe { GetOpenFileNameW(&mut ofn) };
    if ok == 0 {
        return Err("已取消".to_string());
    }
    let end = buf.iter().position(|c| *c == 0).unwrap_or(0);
    let s = String::from_utf16_lossy(&buf[..end]);
    if s.trim().is_empty() {
        return Err("已取消".to_string());
    }
    Ok(s)
}

#[cfg(not(windows))]
pub fn pick_key_file() -> Result<String, String> {
    Err("仅支持 Windows".into())
}

/// 一键导入：弹框选文件 → 校验 → 复制到 user/keys/prod.keys
pub(crate) fn pick_and_import_keys() -> Result<String, String> {
    let picked = pick_key_file()?;
    import_keys(&picked)
}

static RUNNING: Mutex<Option<(u32, String)>> = Mutex::new(None);
/// 启动时发现的问题（游戏包本身有问题等），前端读一次后不自动清空
static LAST_WARNING: Mutex<Option<String>> = Mutex::new(None);

pub fn take_warning() -> String {
    LAST_WARNING
        .lock()
        .ok()
        .and_then(|mut g| g.take())
        .unwrap_or_default()
}

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
            warning: String::new(),
        },
        None => EmuInfo {
            found: false,
            path: String::new(),
            name: String::new(),
            running: run.is_some(),
            game: run.map(|(_, g)| g).unwrap_or_default(),
            keys,
            warning: String::new(),
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
    // ★★ 2026-10-20 实测：移植游戏（如 OutRun2006-NX）**不认 Windows 工作目录**，
    //   它只按 Switch 的规矩去 romfs / sdmc 找数据。所以启动前要把 ROM 旁边的
    //   数据目录挂到模拟器 SD 卡根（junction，不占空间）。
    let linked = link_data_into_sdmc(game_path).unwrap_or_default();
    // 移植游戏的包如果缺可用的原版 exe，游戏会黑屏报 exe-image —— 提前查出来
    if let Some(w) = suspicious_original_exe(game_path) {
        crate::app_logger::log_command("emulator", &format!("包检查: {}", w));
        if let Ok(mut g) = LAST_WARNING.lock() {
            *g = Some(w);
        }
    }
    let mut cmd = std::process::Command::new(&exe);
    // ★★ 2026-10-20 实测推翻上一版：**.nro 自制程序不需要 prod.keys 也能跑**。
    //   上一版「没密钥就拒绝启动」把本来能玩的游戏挡在门外了 —— 已删。
    //   真正要做的是把 Citron 那两个挡路的英文弹窗（缺密钥警告 / 有新版本）
    //   自动关掉，见 dismiss_noise()。
    cmd.arg(game_path);
    // ★★ 2026-10-20 修：工作目录必须是**游戏文件自己所在的目录**，不是模拟器目录。
    //
    //   证据：OutRun2006-NX.nro 解出来在 `switch/outrun2/`，同目录里就是
    //   `Scripts/ Stage/ CARS/ Sprites/` 这些数据。之前把 current_dir 设成
    //   模拟器目录（exe.parent()），NRO 按相对路径找不到数据，于是报
    //   「Original OutRun 2006 data tree not found / path is not an OutRun 2006
    //   retail data root」。
    //
    //   Citron 自己的 DLL 不受影响：Windows 的 DLL 搜索顺序里 **exe 所在目录
    //   永远优先于当前目录**，所以 Qt6*.dll 这些照样能加载。
    if let Some(dir) = Path::new(game_path).parent() {
        cmd.current_dir(dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP: 不跟主程序共用控制台
        cmd.creation_flags(0x0000_0200);
    }
    // ★★ 把模拟器的 stdout 收下来。Citron 开了 ultralow_benchmark_logging 之后
    //   每帧打一行 `[UltraLowBenchmark] ... game_fps=…`，帧率记录器就靠这个
    //   拿到**真实帧率**（不需要注入游戏进程）。
    //   CREATE_NO_WINDOW 仍然保留，所以用户看不到黑框。
    let stdout_log = crate::paths::data_dir().join("fps").join("emu_stdout.log");
    if let Some(d) = stdout_log.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    // 每次启动覆盖，避免越滚越大
    if let Ok(f) = std::fs::File::create(&stdout_log) {
        cmd.stdout(std::process::Stdio::from(f));
    }
    cmd.stderr(std::process::Stdio::null());
    crate::fps_recorder::watch_emu_stdout(stdout_log.to_string_lossy().to_string());
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
    // 窗口标题中文化 + 关掉挡路的英文弹窗（模拟器启动时会反复写标题、弹框，
    // 所以两个都要多试几次）
    #[cfg(windows)]
    {
        dismiss_noise(pid);
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
        &format!(
            "launch name={} exe={} game={} data_linked={}",
            name,
            exe.display(),
            game_path,
            linked.len()
        ),
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
    /// 悬浮条不透明度（0.25~1，浮在游戏上，用户可以调淡）
    #[serde(default = "d_alpha")]
    pub overlay_alpha: f32,
}

fn d_alpha() -> f32 {
    0.9
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
            overlay_alpha: 0.9,
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

    /// ★ 以 # 开头的值不加引号会被 Qt 当注释（主题色会静默丢失）
    #[test]
    fn ini_quotes_hash_values() {
        assert_eq!(ini_value("#eb3333"), "\"#eb3333\"");
        assert_eq!(ini_value("true"), "true");
        assert_eq!(ini_value("15"), "15");
    }

    /// 密钥文件格式校验：必须是 key = hex，且含 master_key_00
    #[test]
    fn prod_keys_format_check() {
        let good = "master_key_00 = 00000000000000000000000000000000\n\
                    header_key = 1111111111111111111111111111111111111111111111111111111111111111\n\
                    title_kek_00 = 22222222222222222222222222222222\n\
                    sd_key = 33333333333333333333333333333333\n\
                    key_area_key_application_00 = 44444444444444444444444444444444\n";
        assert!(looks_like_prod_keys(good));
        // 没有 master_key_00
        assert!(!looks_like_prod_keys("header_key = 1111111111111111\nsd_key = 2222222222222222\n"));
        // 不是 key = value
        assert!(!looks_like_prod_keys("hello world\nthis is not keys\n"));
        // 键名对但值不是十六进制
        assert!(!looks_like_prod_keys(
            "master_key_00 = zzzzzzzzzzzzzzzz\na = zzzzzzzzzzzzzzzz\nb = zzzzzzzzzzzzzzzz\n\
             c = zzzzzzzzzzzzzzzz\nd = zzzzzzzzzzzzzzzz\n"
        ));
    }

    /// 别因为「没有密钥」就拒绝启动：.nro 自制程序不需要密钥。
    /// （这条是回归测试：上一版真的拦了，把本来能玩的游戏挡在门外。）
    #[test]
    fn launch_does_not_require_keys_source_guard() {
        let src = include_str!("mod.rs");
        let launch_start = src.find("pub fn launch(game_path: &str)").expect("launch 不见了");
        let launch_end = src[launch_start..].find("pub fn stop()").map(|x| launch_start + x)
            .unwrap_or(src.len());
        let body = &src[launch_start..launch_end];
        assert!(
            !body.contains("keys_path().is_none()"),
            "launch() 里又出现『缺密钥就不启动』的拦截了 —— .nro 不需要密钥，不能拦"
        );
    }

    /// ★ 回归测试：工作目录必须是 ROM 所在目录，不能是模拟器目录。
    ///
    /// OutRun2006-NX.nro 的数据（Scripts/ Stage/ CARS/）就在它自己旁边，
    /// 工作目录设错就报「Original OutRun 2006 data tree not found」。
    #[test]
    fn launch_uses_rom_dir_as_cwd() {
        let src = include_str!("mod.rs");
        let launch_start = src.find("pub fn launch(game_path: &str)").expect("launch 不见了");
        let launch_end = src[launch_start..].find("pub fn stop()").map(|x| launch_start + x)
            .unwrap_or(src.len());
        let body = &src[launch_start..launch_end];
        assert!(
            body.contains("Path::new(game_path).parent()"),
            "launch() 的 current_dir 必须取游戏文件所在目录"
        );
        // 反面断言要精确：`current_dir` 的实参必须是 `Path::new(game_path).parent()`，
        // 不能是 `exe.parent()`。
        //
        // ★ 别写「不许出现 cmd.current_dir(dir)」——变量名恰好也叫 dir，
        //   正确的写法会被误判（这条断言第一版就是这么写错的）。
        assert!(
            !body.contains("cmd.current_dir(exe.parent()")
                && !body.contains("cmd.current_dir(&dir)"),
            "launch() 又把工作目录设成模拟器目录了 —— NRO 会找不到自己的数据"
        );
    }
}
