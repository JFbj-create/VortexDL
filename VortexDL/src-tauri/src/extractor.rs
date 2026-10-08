// 纯 Rust 解压引擎 - 支持 7z/zip (内置库) + rar (7z.exe 兜底)

pub const VX_AUX_C: &str = "0000000000000000"; // 公开版占位（豪华版校验已移除）
// 使用 sevenz-rust2 (纯 Rust 7z) + zip (纯 Rust zip) 替代外部 7z.exe
// 支持进度上报: 百分比 + 已解压字节/总字节 + 当前文件名
use std::path::{Path, PathBuf};
use std::io::{Read, Write, BufRead, BufReader};
use std::process::{Command, Stdio};

pub struct ExtractResult {
    pub success: bool,
    pub output_dir: PathBuf,
    pub error: String,
}

/// 判定压缩包格式
fn archive_format(path: &Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).map(|s| s.to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "7z" => "7z",
        "zip" => "zip",
        "rar" => "rar",
        "tar" | "gz" | "bz2" => "7zexe", // tar/gz/bz2 走 7z.exe
        _ => "7zexe", // 未知格式走 7z.exe 兜底
    }
}

/// 定位 7z.exe (用于 rar/tar 等纯 Rust 不支持的格式, 以及 .7z 多线程加速)
/// ★ 修复 (issue 6): 打包后资源实际位于 resources_main/7z/7z.exe (见 tauri.conf.json bundle.resources),
///   旧实现只查 resources/7z/7z.exe → 运行时找不到 7z.exe → rar 解压直接失败
pub fn find_7z() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    // 1) 运行目录 (打包后 exe 同级)
    if let Some(d) = std::env::current_exe().ok().and_then(|p| p.parent().map(|x| x.to_path_buf())) {
        candidates.push(d.join("resources_main").join("7z").join("7z.exe"));
        candidates.push(d.join("resources").join("7z").join("7z.exe"));
        candidates.push(d.join("7z").join("7z.exe"));
    }
    // 2) 开发目录 (cargo run)
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    candidates.push(manifest.join("resources_main").join("7z").join("7z.exe"));
    candidates.push(manifest.join("resources").join("7z").join("7z.exe"));
    // 3) 仓库根 (src-tauri 的上一级) 的 resources/7z
    if let Some(parent) = manifest.parent() {
        candidates.push(parent.join("resources").join("7z").join("7z.exe"));
    }
    candidates.into_iter().find(|p| p.exists())
}

/// 安全化文件名 (防止路径穿越攻击) + 长路径支持
fn safe_extract_path(dest: &Path, entry_name: &str) -> PathBuf {
    let p = Path::new(entry_name);
    let components: Vec<_> = p.components().filter(|c| {
        !matches!(c, std::path::Component::RootDir | std::path::Component::ParentDir)
    }).collect();
    let mut full = dest.to_path_buf();
    for c in components {
        full.push(c.as_os_str());
    }
    // ★ Windows 长路径支持: 路径超过 260 字符时加 \\?\ 前缀
    //   日文游戏名+深层目录常超过 260 限制 → File::create 报 "文件不存在" 失败
    #[cfg(windows)]
    {
        let s = full.to_string_lossy();
        if s.len() > 247 && !s.starts_with("\\\\?\\") {
            let long_path = format!("\\\\?\\{}", s);
            return PathBuf::from(long_path);
        }
    }
    full
}

/// 带重试的文件创建 (杀软锁定/权限冲突时自动重试)
fn create_file_with_retry(path: &Path, max_retries: u32) -> std::io::Result<std::fs::File> {
    let mut last_err = None;
    for attempt in 0..max_retries {
        match std::fs::File::create(path) {
            Ok(f) => return Ok(f),
            Err(e) => {
                // 杀软锁定/权限冲突 → 重试
                if e.kind() == std::io::ErrorKind::PermissionDenied
                    || e.raw_os_error().map(|c| c == 5 || c == 32 || c == 33).unwrap_or(false) {
                    last_err = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(300 * (attempt + 1) as u64));
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "未知错误")))
}

/// 带重试的目录创建
fn create_dir_with_retry(path: &Path) -> std::io::Result<()> {
    for attempt in 0..3 {
        match std::fs::create_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if attempt < 2 {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    continue;
                }
                return Err(e);
            }
        }
    }
    std::fs::create_dir_all(path)
}

/// 解压 7z 文件 (纯 Rust sevenz-rust2)
fn extract_7z<F>(
    archive: &Path,
    dest: &Path,
    password: Option<&str>,
    mut progress_cb: F,
) -> ExtractResult
where
    F: FnMut(f64, u64, u64, &str),
{
    // ★ 修复文件权限问题 (2026-09-13): 重试 5 次打开, 避免杀软锁定导致失败
    //   注意: Password 不实现 Copy, 每次重试需要重新创建
    let pw_str = password.filter(|p| !p.is_empty()).unwrap_or("");
    let mut reader = {
        let mut last_err = String::new();
        let mut r = None;
        for _ in 0..5 {
            let pw = sevenz_rust2::Password::new(pw_str);
            match sevenz_rust2::ArchiveReader::open(archive, pw) {
                Ok(ar) => { r = Some(ar); break; }
                Err(e) => {
                    last_err = format!("{}", e);
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
        match r {
            Some(ar) => ar,
            None => {
                let msg = last_err;
                return ExtractResult {
                    success: false,
                    output_dir: dest.to_path_buf(),
                    error: if msg.to_lowercase().contains("password") || msg.to_lowercase().contains("encrypt") {
                        format!("需要密码或密码错误: {}", msg)
                    } else {
                        format!("打开 7z 失败 (重试5次): {}", msg)
                    },
                };
            }
        }
    };

    // 计算总大小 (所有文件条目的 size 之和)
    let archive_ref = reader.archive();
    let mut total_size: u64 = 0;
    let mut file_count: usize = 0;
    for entry in &archive_ref.files {
        if !entry.is_directory && entry.has_stream {
            total_size += entry.size;
            file_count += 1;
        }
    }

    progress_cb(0.0, 0, total_size, &format!("准备解压 {} 个文件...", file_count));

    let mut extracted: u64 = 0;
    let mut last_percent = 0.0f64;
    let mut current_file = String::new();

    let result = reader.for_each_entries(|entry, reader| {
        let entry_path = entry.name.replace('\\', "/");
        current_file = entry_path.clone();
        let full = safe_extract_path(dest, &entry_path);
        let full_clone = full.clone();
        let entry_size = entry.size;

        if entry.is_directory || !entry.has_stream {
            // 创建目录
            let _ = std::fs::create_dir_all(&full);
            // 上报进度
            let pct = if total_size > 0 { (extracted as f64 / total_size as f64) * 100.0 } else { 0.0 };
            if (pct - last_percent).abs() >= 0.3 {
                last_percent = pct;
                progress_cb(pct, extracted, total_size, &current_file);
            }
            return Ok(true);
        }

        // 创建父目录
        if let Some(parent) = full.parent() {
            let _ = create_dir_with_retry(parent);
        }

        // 写文件 (带重试, 杀软锁定时自动等待)
        let mut file = match create_file_with_retry(&full_clone, 5) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[extract_7z] 创建文件失败 (重试5次) {} : {}", full_clone.display(), e);
                return Err(sevenz_rust2::Error::from(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("创建文件失败: {}", e)
                )));
            }
        };

        // 逐块读取写入 (64KB buffer, 旧值 4KB 慢且 IO 次数多)
        let mut buf = [0u8; 65536];
        let mut written: u64 = 0;
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    eprintln!("[extract_7z] 读取失败 {}: {}", full_clone.display(), e);
                    // ★ 增强 (2026-09-15): 返回带文件名的错误, 方便回退逻辑匹配和定位问题文件
                    //   保留原始错误信息 (如 "dist overflow"), 仅附加文件名, 不破坏回退条件匹配
                    return Err(sevenz_rust2::Error::from(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("{}: {}", full_clone.display(), e),
                    )));
                }
            };
            if let Err(e) = file.write_all(&buf[..n]) {
                eprintln!("[extract_7z] 写入失败 {}: {}", full_clone.display(), e);
                return Err(sevenz_rust2::Error::from(e));
            }
            written += n as u64;
            extracted += n as u64;

            // 上报进度 (每 1MB 或文件完成时)
            if written % (1024 * 1024) < 65536 || written >= entry_size {
                let pct = if total_size > 0 {
                    (extracted as f64 / total_size as f64) * 100.0
                } else { 0.0 };
                if (pct - last_percent).abs() >= 0.3 || written >= entry_size {
                    last_percent = pct;
                    progress_cb(pct, extracted, total_size, &current_file);
                }
            }
        }

        // 设置文件时间 (如果可用)
        if entry.has_last_modified_date {
            use std::time::SystemTime;
            // NtTime is u64 (Windows file time, 100ns intervals since 1601-01-01)
            let _ = file.set_modified(SystemTime::from(std::time::UNIX_EPOCH + std::time::Duration::from_secs(0)));
        }

        Ok(true)
    });

    match result {
        Ok(()) => {
            progress_cb(100.0, total_size, total_size, "解压完成");
            ExtractResult { success: true, output_dir: dest.to_path_buf(), error: String::new() }
        }
        Err(e) => {
            let msg = format!("{}", e);
            progress_cb(100.0, total_size, total_size, "解压失败");
            ExtractResult {
                success: false,
                output_dir: dest.to_path_buf(),
                error: if msg.to_lowercase().contains("password") || msg.to_lowercase().contains("encrypt") {
                    format!("需要密码或密码错误: {}", msg)
                } else {
                    msg
                },
            }
        }
    }
}

/// 解压 zip 文件 (纯 Rust zip 库)
fn extract_zip<F>(
    archive: &Path,
    dest: &Path,
    password: Option<&str>,
    mut progress_cb: F,
) -> ExtractResult
where
    F: FnMut(f64, u64, u64, &str),
{
    // ★ 修复文件权限问题 (2026-09-13): 下载刚完成时杀软可能正在扫描文件, 导致 File::open 失败.
    //   重试 5 次, 每次 500ms, 等杀软释放锁后再打开.
    let file = {
        let mut last_err = String::new();
        let mut f = None;
        for _ in 0..5 {
            match std::fs::File::open(archive) {
                Ok(fh) => { f = Some(fh); break; }
                Err(e) => {
                    last_err = format!("{}", e);
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
        match f {
            Some(fh) => fh,
            None => return ExtractResult { success: false, output_dir: dest.to_path_buf(), error: format!("打开 zip 失败 (重试5次后仍被占用): {}", last_err) },
        }
    };
    let mut zip = match zip::ZipArchive::new(file) {
        Ok(z) => z,
        Err(e) => return ExtractResult { success: false, output_dir: dest.to_path_buf(), error: format!("读取 zip 失败: {}", e) },
    };

    let total_size: u64 = (0..zip.len()).map(|i| {
        zip.by_index_raw(i).ok().map(|f| f.size()).unwrap_or(0)
    }).sum();

    let file_count = zip.len();
    progress_cb(0.0, 0, total_size, &format!("准备解压 {} 个文件...", file_count));

    let mut extracted: u64 = 0;
    let mut last_percent = 0.0f64;
    let mut failed_count: u32 = 0;

    for i in 0..zip.len() {
        let mut zf = match if let Some(pw) = password {
            if pw.is_empty() { zip.by_index(i) } else { zip.by_index_decrypt(i, pw.as_bytes()) }
        } else {
            zip.by_index(i)
        } {
            Ok(f) => f,
            Err(e) => {
                let msg = format!("{}", e);
                // zip 密码错误检测
                if msg.to_lowercase().contains("password") || msg.to_lowercase().contains("invalid") {
                    return ExtractResult { success: false, output_dir: dest.to_path_buf(), error: format!("需要密码或密码错误: {}", msg) };
                }
                // 跳过无法读取的文件
                continue;
            }
        };

        let entry_name = zf.name().to_string();
        let full = safe_extract_path(dest, &entry_name);

        if zf.is_dir() {
            let _ = std::fs::create_dir_all(&full);
            continue;
        }

        // 创建父目录
        if let Some(parent) = full.parent() {
            let _ = create_dir_with_retry(parent);
        }

        // 写文件 (带重试, 杀软锁定时自动等待)
        let mut out = match create_file_with_retry(&full, 5) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[extract_zip] 创建文件失败 (重试5次) {} : {}", full.display(), e);
                failed_count += 1;
                continue;
            }
        };

        // 逐块读取写入 (64KB buffer, 旧值 4KB 慢且 IO 次数多)
        let mut buf = [0u8; 65536];
        loop {
            let n = match zf.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    eprintln!("[extract_zip] 读取失败 {}: {}", full.display(), e);
                    failed_count += 1;
                    break;
                }
            };
            if let Err(e) = out.write_all(&buf[..n]) {
                eprintln!("[extract_zip] 写入失败 {}: {}", full.display(), e);
                failed_count += 1;
                break;
            }
            extracted += n as u64;

            let pct = if total_size > 0 { (extracted as f64 / total_size as f64) * 100.0 } else { 0.0 };
            if (pct - last_percent).abs() >= 0.3 {
                last_percent = pct;
                let short_name = entry_name.rsplit('/').next().unwrap_or(&entry_name);
                progress_cb(pct, extracted, total_size, short_name);
            }
        }
    }

    progress_cb(100.0, total_size, total_size, "解压完成");
    if failed_count > 0 {
        ExtractResult {
            success: true,
            output_dir: dest.to_path_buf(),
            error: format!("{} 个文件解压失败 (可能是杀软拦截或路径过长)", failed_count),
        }
    } else {
        ExtractResult { success: true, output_dir: dest.to_path_buf(), error: String::new() }
    }
}

/// 解压 rar/tar/gz (使用外部 7z.exe)
fn extract_7zexe<F>(
    archive: &Path,
    dest: &Path,
    password: Option<&str>,
    mut progress_cb: F,
) -> ExtractResult
where
    F: FnMut(f64, u64, u64, &str),
{
    let out_dir = if dest.as_os_str().is_empty() {
        archive.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."))
    } else {
        dest.to_path_buf()
    };

    let Some(seven_zip) = find_7z() else {
        return ExtractResult { success: false, output_dir: out_dir, error: "未找到 7z.exe (resources\\7z 目录缺失, 仅影响 rar 格式)".into() };
    };

    if !archive.exists() {
        return ExtractResult { success: false, output_dir: out_dir, error: format!("压缩包不存在: {}", archive.display()) };
    }

    let _ = create_dir_with_retry(&out_dir);

    // ★ 长路径支持: 7z.exe 的 -o 参数也用 \\?\ 前缀
    let out_dir_arg = {
        let s = out_dir.to_string_lossy();
        if s.len() > 247 && !s.starts_with("\\\\?\\") {
            format!("\\\\?\\{}", s)
        } else {
            s.to_string()
        }
    };

    // ★ 修复 (issue 6): 7z l -slt 枚举归档内所有文件大小可能耗时数秒, 期间必须上报"扫描中",
    //   否则 UI 会长时间停在 0% 无任何反馈 (用户反馈: "停在那里好久")
    progress_cb(0.0, 0, 0, "扫描压缩包...");

    // 获取总大小
    let mut cmd = Command::new(&seven_zip);
    cmd.arg("l").arg("-slt");
    if let Some(pw) = password { if !pw.is_empty() { cmd.arg(format!("-p{}", pw)); } }
    cmd.arg(archive).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)] { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000u32); }
    let total_size = match cmd.spawn() {
        Ok(child) => {
            let stdout = child.stdout;
            if let Some(o) = stdout {
                BufReader::new(o).lines().flatten()
                    .filter_map(|l| l.strip_prefix("Size = ").and_then(|s| s.trim().parse::<u64>().ok()))
                    .sum()
            } else { 0 }
        }
        Err(_) => 0,
    };

    progress_cb(0.0, 0, total_size, "准备解压...");

    let mut cmd = Command::new(&seven_zip);
    cmd.arg("x").arg(archive)
        .arg(format!("-o{}", out_dir_arg))
        .arg("-y").arg("-bsp1").arg("-bb1");
    if let Some(pw) = password {
        if !pw.is_empty() { cmd.arg(format!("-p{}", pw)); }
        else { cmd.arg("-p"); }
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)] { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000u32); }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ExtractResult { success: false, output_dir: out_dir, error: format!("启动 7z 失败: {}", e) },
    };

    // ★ 修复 (issue 6): 7z 的 -bsp1 进度用 '\r' 原地刷新 (不换行), 必须按 '\r' 与 '\n' 同时分割.
    //   旧实现用 BufReader::lines() (只认 '\n') → 解压过程中读不到任何中间进度,
    //   直到进程结束才一次性刷出 → 表现为"卡很久然后直接提示解压完成, 没有实时进度条".
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut last_percent = 0.0f64;
    let mut bytes_extracted: u64 = 0;
    let mut current_file = String::new();
    let mut raw: Vec<u8> = Vec::with_capacity(256);
    let mut chunk = [0u8; 4096];
    loop {
        let n = match reader.read(&mut chunk) { Ok(0) => break, Ok(n) => n, Err(_) => break };
        for &b in &chunk[..n] {
            if b == b'\r' || b == b'\n' {
                if raw.is_empty() { continue; }
                let line = String::from_utf8_lossy(&raw).to_string();
                raw.clear();
                let trimmed = line.trim_start();
                if let Some(pct_str) = trimmed.split('%').next() {
                    if let Ok(pct) = pct_str.trim().parse::<f64>() {
                        last_percent = pct.clamp(0.0, 100.0);
                        if total_size > 0 {
                            bytes_extracted = ((last_percent / 100.0) * total_size as f64) as u64;
                        }
                        if let Some(idx) = trimmed.find(" - ") {
                            let fname = trimmed[idx + 3..].trim().to_string();
                            if !fname.is_empty() { current_file = fname; }
                        }
                        progress_cb(last_percent, bytes_extracted, total_size, &current_file);
                    }
                }
            } else {
                raw.push(b);
            }
        }
    }
    if !raw.is_empty() {
        let line = String::from_utf8_lossy(&raw).to_string();
        let trimmed = line.trim_start();
        if let Some(pct_str) = trimmed.split('%').next() {
            if let Ok(pct) = pct_str.trim().parse::<f64>() {
                last_percent = pct.clamp(0.0, 100.0);
                if total_size > 0 {
                    bytes_extracted = ((last_percent / 100.0) * total_size as f64) as u64;
                }
                progress_cb(last_percent, bytes_extracted, total_size, &current_file);
            }
        }
    }

    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => { progress_cb(100.0, total_size, total_size, "完成"); return ExtractResult { success: false, output_dir: out_dir, error: format!("等待 7z 进程失败: {}", e) }; }
    };
    progress_cb(100.0, total_size, total_size, "完成");

    if status.success() {
        ExtractResult { success: true, output_dir: out_dir, error: String::new() }
    } else {
        let stderr = child.stderr.take();
        let err_msg = stderr.map(|e| BufReader::new(e).lines().flatten().collect::<Vec<_>>().join("; ")).unwrap_or_default();
        let msg = if err_msg.is_empty() { "解压失败 (7z 返回非零退出码)".to_string() } else { err_msg };
        ExtractResult { success: false, output_dir: out_dir, error: msg }
    }
}

/// ★ 2026-10-07 修「BT 下完立刻解压报 Cannot open the file as archive」：
///   BT 引擎把状态置 completed、前端立刻触发解压时，压缩包的**最后几个字节可能还没落盘**
///   （RAR 的结尾记录是最后写的），7z.exe 读到的是不完整归档 → 报 "Cannot open the file as archive"。
///   实测这种失败是**瞬间**返回的（日志里 [DIRECT] → [FAIL] 只隔 50ms），说明连打开都没成功，
///   而不是内容坏了 —— 事后同一个文件用同一个 7z.exe 能正常列出（已验证）。
///   `bt_output_readable` 那种"能否打开文件"的探测探不出"内容还没写完"，所以这里补一层退避重试。
///   内容真的损坏时每次都会失败，重试 3 次后照样如实返回失败。
fn extract_7zexe_retry<F>(
    archive: &Path,
    dest: &Path,
    password: Option<&str>,
    mut progress_cb: F,
) -> ExtractResult
where
    F: FnMut(f64, u64, u64, &str),
{
    let mut last = extract_7zexe(archive, dest, password, |p, b, t, f| progress_cb(p, b, t, f));
    for attempt in 1..=3u32 {
        if last.success {
            return last;
        }
        let e = last.error.to_lowercase();
        // 只对"打不开/被占用"这类**可能瞬时**的错误重试；
        // 密码错、CRC 错、格式不支持等确定性失败立刻返回，避免白等。
        let transient = e.contains("cannot open the file as archive")
            || e.contains("access is denied")
            || e.contains("being used by another process")
            || e.contains("拒绝访问");
        if !transient {
            return last;
        }
        eprintln!(
            "[extractor] 7z 打开归档失败(第 {} 次)，{}ms 后重试: {}",
            attempt,
            attempt * 1200,
            last.error
        );
        std::thread::sleep(std::time::Duration::from_millis(1200 * attempt as u64));
        last = extract_7zexe(archive, dest, password, |p, b, t, f| progress_cb(p, b, t, f));
    }
    last
}

/// 主解压入口: 按格式分流到纯 Rust 库 或 7z.exe
pub fn extract<F>(
    archive: &Path,
    dest: &Path,
    password: Option<&str>,
    mut progress_cb: F,
) -> ExtractResult
where
    F: FnMut(f64, u64, u64, &str),
{
    let out_dir = if dest.as_os_str().is_empty() {
        archive.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."))
    } else {
        dest.to_path_buf()
    };

    if !archive.exists() {
        return ExtractResult { success: false, output_dir: out_dir, error: format!("压缩包不存在: {}", archive.display()) };
    }

    let _ = std::fs::create_dir_all(&out_dir);

    let fmt = archive_format(archive);
    eprintln!("[extractor] 解压 {} 格式={} dest={}", archive.display(), fmt, out_dir.display());
    // ★ 日志增强 (2026-09-15): 解压详细记录到 extract.log
    let archive_size = std::fs::metadata(archive).map(|m| m.len()).unwrap_or(0);
    crate::app_logger::log_extract("START", &format!(
        "archive={} format={} dest={} has_password={} size={}",
        archive.display(), fmt, out_dir.display(), password.is_some(), archive_size
    ));

    match fmt {
        "7z" => {
            let r = extract_7z(archive, &out_dir, password, |p, b, t, f| progress_cb(p, b, t, f));
            if r.success {
                crate::app_logger::log_extract("OK", &format!("format=7z engine=rust archive={}", archive.display()));
                return r;
            }
            let err_lower = r.error.to_lowercase();
            let needs_fallback = err_lower.contains("dist overflow")
                || err_lower.contains("overflow")
                || err_lower.contains("rc not finished")
                || err_lower.contains("lz has pending")
                || err_lower.contains("corrupt")
                || err_lower.contains("crc")
                || err_lower.contains("bad")
                || err_lower.contains("unexpected")
                || err_lower.contains("invalid");
            if needs_fallback {
                eprintln!("[extractor] 7z 纯 Rust 解压失败 ({}), 回退到 7z.exe", r.error);
                crate::app_logger::log_extract("FALLBACK", &format!(
                    "format=7z rust_error={} → 7z.exe archive={}", r.error, archive.display()
                ));
                progress_cb(0.0, 0, 0, "内置引擎不支持, 切换到 7z 解压...");
                let r2 = extract_7zexe_retry(archive, &out_dir, password, |p, b, t, f| progress_cb(p, b, t, f));
                crate::app_logger::log_extract(if r2.success { "OK" } else { "FAIL" },
                    &format!("format=7z engine=7zexe success={} archive={}", r2.success, archive.display()));
                return r2;
            }
            crate::app_logger::log_extract("FAIL", &format!("format=7z engine=rust error={} archive={}", r.error, archive.display()));
            r
        }
        "zip" => {
            // ★ Bug 修复 (2026-09-13): 纯 Rust zip crate 不支持 AES 加密 (传统 ZipCrypto 才支持),
            //   很多游戏站发布的 .zip 用 AES-256 加密 → 解压报 "unsupported encryption" 失败.
            //   兜底策略: 先用纯 Rust 解压 (快, 跨平台, 带进度), 如果返回加密/密码错误,
            //   自动回退到 7z.exe (支持 AES/ZipCrypto/多卷, 兼容性最强).
            let r = extract_zip(archive, &out_dir, password, |p, b, t, f| progress_cb(p, b, t, f));
            if r.success {
                crate::app_logger::log_extract("OK", &format!("format=zip engine=rust archive={}", archive.display()));
                return r;
            }
            let err_lower = r.error.to_lowercase();
            let needs_fallback = err_lower.contains("unsupported encryption")
                || err_lower.contains("password")
                || err_lower.contains("invalid")
                || err_lower.contains("encrypt")
                || err_lower.contains("corrupt")
                || err_lower.contains("crc")
                || err_lower.contains("bad")
                || err_lower.contains("unexpected")
                || err_lower.contains("dist overflow")
                || err_lower.contains("overflow")
                || err_lower.contains("rc not finished")
                || err_lower.contains("lz has pending");
            if needs_fallback {
                eprintln!("[extractor] zip 纯 Rust 解压失败 ({}), 回退到 7z.exe", r.error);
                crate::app_logger::log_extract("FALLBACK", &format!(
                    "format=zip rust_error={} → 7z.exe archive={}", r.error, archive.display()
                ));
                progress_cb(0.0, 0, 0, "内置引擎不支持, 切换到 7z 解压...");
                let r2 = extract_7zexe_retry(archive, &out_dir, password, |p, b, t, f| progress_cb(p, b, t, f));
                crate::app_logger::log_extract(if r2.success { "OK" } else { "FAIL" },
                    &format!("format=zip engine=7zexe success={} archive={}", r2.success, archive.display()));
                return r2;
            }
            crate::app_logger::log_extract("FAIL", &format!("format=zip engine=rust error={} archive={}", r.error, archive.display()));
            r
        }
        f => {
            crate::app_logger::log_extract("DIRECT", &format!("format={} → 7z.exe archive={}", f, archive.display()));
            let r = extract_7zexe_retry(archive, &out_dir, password, |p, b, t, f| progress_cb(p, b, t, f));
            crate::app_logger::log_extract(if r.success { "OK" } else { "FAIL" },
                &format!("format={} engine=7zexe success={} archive={}", f, r.success, archive.display()));
            r
        }
    }
}
