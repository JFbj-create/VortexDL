// 纯 Rust 解压引擎 - 支持 7z/zip (内置库) + rar (7z.exe 兜底)

pub const VX_AUX_C: &str = "0000000000000000"; // 公开版占位（豪华版校验已移除）
// 使用 sevenz-rust2 (纯 Rust 7z) + zip (纯 Rust zip) 替代外部 7z.exe
// 支持进度上报: 百分比 + 已解压字节/总字节 + 当前文件名
use std::path::{Path, PathBuf};
use std::io::{Read, Write, BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

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
/// zip 是否**缺少结尾目录记录 (EOCD)** —— 也就是"没下完"。
///
/// zip 的中央目录在文件**末尾**，签名是 `PK\x05\x06`；下载中断时这部分根本没写出来。
/// 只扫最后 64KB（EOCD 后面最多再跟 64KB 注释），不做全文件扫描。
/// 读不到文件（不存在/被占用）时返回 false，让后续逻辑照常报它自己的错。
fn zip_missing_eocd(archive: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    const SCAN: u64 = 64 * 1024;
    let Ok(mut f) = std::fs::File::open(archive) else { return false };
    let Ok(len) = f.metadata().map(|m| m.len()) else { return false };
    if len < 22 {
        // 比最小 EOCD 还短，肯定不完整
        return true;
    }
    let start = len.saturating_sub(SCAN);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return false;
    }
    !buf.windows(4).any(|w| w == b"PK\x05\x06")
}

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
    // ★ 2026-10-11: 先判断"这个 zip 到底下完了没有"。
    //   zip 的目录区 (EOCD, 签名 PK\x05\x06) 在**文件末尾** —— 下载没完成时末尾
    //   根本没写出来，zip 库只会报一句 `invalid Zip archive: Could not find EOCD`，
    //   用户看到的是「解压失败」却不知道为什么。
    //   实测来源：一个 BT 任务只下了 1.4% (10.5MB/770MB) 就被误判成 completed，
    //   随后自动解压这个残缺文件。这里给一句人话，用户就知道要重新下载。
    if zip_missing_eocd(archive) {
        return ExtractResult {
            success: false,
            output_dir: dest.to_path_buf(),
            error: "压缩包不完整（下载没有完成），请重新下载后再解压".to_string(),
        };
    }
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

// ============================================================
// 归档清单 + 并行解压 (2026-10-12)
//
// ★ 为什么要做: 7z.exe 解压 RAR/7z **只有一个线程**在解码, 实测单进程约 470 MB/s
//   (NVMe 上)。同一台机器开 3 个进程各解各的 → 合计 1411 MB/s, 正好 3 倍线性,
//   说明瓶颈是单线程解码, 磁盘和 CPU 都还有大量余量。
//   而游戏压缩包(RAR5)绝大多数 **不是 solid** —— 每个文件独立压缩,
//   所以把文件列表按体积均分给 N 个 7z.exe 进程就能近乎线性加速。
//
// ★ 只在安全的前提下并行, 任何一条不满足就老实走串行:
//   1) 归档级 `Solid = +`, 或**任何一个条目**带 `Solid = +` → 不并行
//      (solid 块里单独取一个文件要从块头开始解码, 并行反而更慢/出错)
//   2) 文件数 < 2×路数 → 不值得
//   3) 归档 < 400 MB → 并行开销盖过收益
//   4) 路径里含 `* ? [ ]` → 7z 的 `-i@清单` 把每行当**通配符**处理, 会匹配错
//      (实测 `-i@` 只解指定文件是对的, 但通配符字符会误伤) → 不并行
// ============================================================

/// 7z.exe 的 `-o` 参数: 目录超过 247 字符时补 `\\?\` 前缀
/// (日文/中文游戏名 + 深层目录很容易破 260 限制, 不补的话 7z 直接报错)
fn long_path_arg(dir: &Path) -> String {
    let s = dir.to_string_lossy();
    if s.len() > 247 && !s.starts_with("\\\\?\\") {
        format!("\\\\?\\{}", s)
    } else {
        s.to_string()
    }
}

/// 归档里的一条记录
#[derive(Clone)]
struct ArcEntry {
    path: String,
    size: u64,
    is_dir: bool,
}

/// `7z l -slt` 解析结果
struct ArcListing {
    total_size: u64,
    solid: bool,
    entries: Vec<ArcEntry>,
}

/// 跑一次 `7z l -slt` 把清单读全。
/// 旧实现为了拿总大小本来就要跑这条命令, 所以这里没有额外开销。
fn list_archive(seven_zip: &Path, archive: &Path, password: Option<&str>) -> Option<ArcListing> {
    let mut cmd = Command::new(seven_zip);
    // ★ `-sccUTF-8`: 7z 默认按**控制台代码页**输出 (中文系统 = GBK)。归档里有中文/日文
    //   文件名时, 那些 `Path = ` 行就是 GBK 字节; 而 Rust 侧 `lines()` 遇到非 UTF-8
    //   会整行报错, `flatten()` 直接把它丢掉 —— 表现为"清单里一条文件都没有",
    //   total_size 归零, 并行静默不生效 (不丢数据, 但白丢一次加速机会)。
    cmd.arg("l").arg("-slt").arg("-sccUTF-8");
    if let Some(pw) = password {
        if !pw.is_empty() {
            cmd.arg(format!("-p{}", pw));
        }
    }
    cmd.arg(archive).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000u32);
    }
    let mut child = cmd.spawn().ok()?;
    let stdout = child.stdout.take()?;

    // 再兜一层: 按字节收行 + from_utf8_lossy, 无论 7z 用什么编码输出都不会解析崩/断行
    let mut reader = BufReader::new(stdout);
    let mut raw: Vec<u8> = Vec::with_capacity(256);
    let mut chunk = [0u8; 8192];
    let mut lines: Vec<String> = Vec::new();
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        for &b in &chunk[..n] {
            if b == b'\n' {
                lines.push(String::from_utf8_lossy(&raw).to_string());
                raw.clear();
            } else {
                raw.push(b);
            }
        }
    }
    if !raw.is_empty() {
        lines.push(String::from_utf8_lossy(&raw).to_string());
    }
    let listing = parse_listing_lines(lines.into_iter());
    let _ = child.wait();
    Some(listing)
}

/// 解析 `7z l -slt` 的输出行。
/// 抽成纯函数是为了能单测 —— 真实归档塞不进单测, 而这里几个边界
/// (归档级的 Path/Solid 不算条目、目录用 Folder 还是 Attributes 标、最后一条要收尾)
/// 恰恰是最容易写错的地方。
fn parse_listing_lines<I: Iterator<Item = String>>(lines: I) -> ArcListing {
    let mut total: u64 = 0;
    let mut solid = false;
    let mut entries: Vec<ArcEntry> = Vec::new();
    let mut in_body = false;

    // 当前条目 (累积到下一个 "Path = " 时收尾)
    let mut c_path: Option<String> = None;
    let mut c_size: u64 = 0;
    let mut c_dir = false;
    let mut c_solid = false;

    for line in lines {
        let line = line.trim_end_matches('\r');
        if line == "----------" {
            // ★ 进正文前要把"当前条目"丢掉 —— 分隔线之前的 `Path = ` 是**归档自己**的路径
            //   (7z l -slt 第一行就是它), 不丢的话它会变成一个 0 字节的假条目混进文件列表,
            //   再被塞进 `-i@` 清单里去匹配一个归档内部根本不存在的路径。
            in_body = true;
            c_path = None;
            c_size = 0;
            c_dir = false;
            c_solid = false;
            continue;
        }
        if let Some(v) = line.strip_prefix("Path = ") {
            if in_body {
                if let Some(p) = c_path.take() {
                    total += c_size;
                    if c_solid {
                        solid = true;
                    }
                    entries.push(ArcEntry { path: p, size: c_size, is_dir: c_dir });
                }
            }
            c_path = Some(v.to_string());
            c_size = 0;
            c_dir = false;
            c_solid = false;
            continue;
        }
        if let Some(v) = line.strip_prefix("Size = ") {
            if let Ok(n) = v.trim().parse::<u64>() {
                c_size = n;
            }
            continue;
        }
        if let Some(v) = line.strip_prefix("Folder = ") {
            if v.trim() == "+" {
                c_dir = true;
            }
            continue;
        }
        if let Some(v) = line.strip_prefix("Solid = ") {
            let is_solid = v.trim() == "+";
            if in_body {
                if is_solid {
                    c_solid = true;
                }
            } else if is_solid {
                // 归档级 Solid = +
                solid = true;
            }
            continue;
        }
        // 非 RAR 的归档用 Attributes 标目录 (Attributes = D....)
        if let Some(v) = line.strip_prefix("Attributes = ") {
            if v.trim_start().starts_with('D') {
                c_dir = true;
            }
            continue;
        }
    }
    if in_body {
        if let Some(p) = c_path.take() {
            total += c_size;
            if c_solid {
                solid = true;
            }
            entries.push(ArcEntry { path: p, size: c_size, is_dir: c_dir });
        }
    }
    ArcListing { total_size: total, solid, entries }
}

/// 低于这个体积不值得并行 (进程启动开销盖过收益)
const PAR_MIN_BYTES: u64 = 400 * 1024 * 1024;
/// 并行路数上限 (实测 NVMe 上 3 路已线性, 4 路是稳妥默认)
const PAR_MAX_WORKERS: usize = 4;
/// 每路命令行里文件名的字符预算 (Windows CreateProcess 上限约 32767, 留足余量)
const PAR_ARGV_BUDGET: usize = 20_000;

/// LPT (Longest Processing Time first) 调度: 按体积从大到小, 每个丢给当前负载最轻的一路。
/// 游戏压缩包里往往一两个几十 GB 的大文件占大头, 贪心均分能让**最长的那一路**尽量短
/// —— 并行的墙钟时间就是由最长那一路决定的。
/// 返回 (每路的条目下标, 每路的字节数)。
fn partition_lpt(sizes: &[u64], workers: usize) -> (Vec<Vec<usize>>, Vec<u64>) {
    let workers = workers.max(1);
    let mut order: Vec<usize> = (0..sizes.len()).collect();
    order.sort_by(|&a, &b| sizes[b].cmp(&sizes[a]));
    let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); workers];
    let mut loads: Vec<u64> = vec![0; workers];
    for &i in &order {
        let k = (0..workers).min_by_key(|&k| loads[k]).unwrap();
        buckets[k].push(i);
        loads[k] += sizes[i];
    }
    (buckets, loads)
}

/// 把一路的文件按**命令行长度**切成若干批 (Windows CreateProcess 上限约 32767 字符)。
/// 返回 (每批的路径, 每批的字节数)。单个路径再长也会单独成批, 不会死循环。
fn batch_by_argv_budget(paths: &[String], sizes: &[u64], budget: usize) -> Vec<(Vec<String>, u64)> {
    let mut out: Vec<(Vec<String>, u64)> = Vec::new();
    let mut cur_paths: Vec<String> = Vec::new();
    let mut cur_bytes: u64 = 0;
    let mut cur_chars = 0usize;
    for (p, &sz) in paths.iter().zip(sizes.iter()) {
        let c = p.chars().count() + 3; // 两个引号 + 一个空格
        if !cur_paths.is_empty() && cur_chars + c > budget {
            out.push((std::mem::take(&mut cur_paths), cur_bytes));
            cur_bytes = 0;
            cur_chars = 0;
        }
        cur_chars += c;
        cur_bytes += sz;
        cur_paths.push(p.clone());
    }
    if !cur_paths.is_empty() {
        out.push((cur_paths, cur_bytes));
    }
    out
}

/// 非 solid 归档 → 按体积均分给 N 个 7z.exe 并行解压。
/// 返回 `None` = 这条归档不适用并行 (调用方走串行)。
fn extract_7zexe_parallel<F>(
    seven_zip: &Path,
    archive: &Path,
    dest: &Path,
    out_dir_arg: &str,
    password: Option<&str>,
    listing: &ArcListing,
    mut progress_cb: F,
) -> Option<ExtractResult>
where
    F: FnMut(f64, u64, u64, &str),
{
    if listing.solid || listing.total_size < PAR_MIN_BYTES {
        return None;
    }
    let files: Vec<&ArcEntry> = listing.entries.iter().filter(|e| !e.is_dir).collect();
    if files.is_empty() {
        return None;
    }
    // 通配符字符会让 `-i@` 匹配错 → 这种归档老实串行
    if files.iter().any(|e| {
        e.path.contains('*') || e.path.contains('?') || e.path.contains('[') || e.path.contains(']')
    }) {
        return None;
    }

    // ★ "几万个碎文件"的包并行**不提速**: 实测同一个 14127 文件的 zip,
    //   4 路 21.0s vs 单路 18.6s (反而慢 13%) —— 瓶颈是每个文件的创建/打开开销
    //   (还叠着杀软实时扫描), 这部分 4 个进程抢同一个目录并不能并行。
    //   所以文件多 + 平均体积小的时候老实串行。游戏包是"少而巨大", 不落这条。
    let avg = listing.total_size / files.len().max(1) as u64;
    if files.len() >= 2000 && avg < 512 * 1024 {
        return None;
    }

    let nproc = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let workers = nproc.min(PAR_MAX_WORKERS).min(files.len() / 2);
    if workers < 2 {
        return None;
    }

    // LPT 调度: 大文件优先, 每次丢给当前最空的那一路
    let sizes: Vec<u64> = files.iter().map(|f| f.size).collect();
    let (buckets, _loads) = partition_lpt(&sizes, workers);

    // ★ 文件名走**命令行参数**, 不用 `-i@清单文件`:
    //   实测 7z 的清单文件对非 ASCII 文件名会**静默匹配不到** —— 目录里带中文名的条目
    //   一个都没解出来, 而且退出码还是 0 (假装成功)。UTF-8 内容配 `-scsUTF-8`、
    //   UTF-16LE 配 `-scsUTF-16LE`、GBK 不配, 三种都试过, 都不行。
    //   而命令行参数在 Windows 上是 UTF-16 原样传的, 中文/日文名完全正确 (已实测)。
    //   代价是命令行有长度上限 (CreateProcess 约 32767 字符), 所以某一路分到的路径
    //   总长超预算就整体不并行 —— 游戏包是"少而巨大"的文件, 完全够用; 而"几万个碎文件"
    //   的包并行本来也不提速 (实测 4 路 18.6s vs 单路 18.8s, 瓶颈在每文件创建开销)。
    // 每一路的文件再按命令行长度切成若干**批**; 同一个 worker 内部把它的批次串行跑完。
    // 这样"几万个碎文件"的包也能并行 (只是每路跑好几个 7z 进程), 而不是因为
    // 一次塞不下就整体放弃并行 —— 之前就是这样把 2645 文件那个包白白退回串行的。
    let worker_batches: Vec<Vec<(Vec<String>, u64)>> = buckets
        .iter()
        .map(|b| {
            let paths: Vec<String> = b.iter().map(|&i| files[i].path.clone()).collect();
            let sizes: Vec<u64> = b.iter().map(|&i| files[i].size).collect();
            batch_by_argv_budget(&paths, &sizes, PAR_ARGV_BUDGET)
        })
        .collect();

    eprintln!(
        "[extractor] 并行解压 {} 路 (非 solid, {} 个文件, {:.1} GB)",
        workers,
        files.len(),
        listing.total_size as f64 / 1073741824.0
    );
    crate::app_logger::log_extract(
        "PARALLEL",
        &format!(
            "workers={} files={} total={} archive={}",
            workers,
            files.len(),
            listing.total_size,
            archive.display()
        ),
    );

    // 每路"已完成字节数" (直接累计字节, 比百分比好算 —— 一路要跑好几个批次)
    let prog: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(vec![0u64; workers]));
    let done: Arc<Mutex<Vec<Option<bool>>>> = Arc::new(Mutex::new(vec![None; workers]));
    let errs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![String::new(); workers]));

    let sz_owned = seven_zip.to_path_buf();
    let arc_owned = archive.to_path_buf();
    let dest_arg_owned = out_dir_arg.to_string();
    let pw_owned: Option<String> = password.map(|s| s.to_string());

    let mut handles = Vec::new();
    for k in 0..workers {
        let batches = worker_batches[k].clone();
        let sz2 = sz_owned.clone();
        let arc2 = arc_owned.clone();
        let dest_arg2 = dest_arg_owned.clone();
        let pw2 = pw_owned.clone();
        let prog2 = prog.clone();
        let done2 = done.clone();
        let errs2 = errs.clone();
        handles.push(std::thread::spawn(move || {
            let mut base: u64 = 0;
            for (paths, bytes) in batches.iter() {
                let mut cmd = Command::new(&sz2);
                cmd.arg("x")
                    .arg(&arc2)
                    .arg(format!("-o{}", dest_arg2))
                    .arg("-y")
                    .arg("-bsp1")
                    .args(paths);
                if let Some(pw) = pw2.as_ref() {
                    if !pw.is_empty() {
                        cmd.arg(format!("-p{}", pw));
                    } else {
                        cmd.arg("-p");
                    }
                }
                cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x08000000u32);
                }

                let mut child = match cmd.spawn() {
                    Ok(c) => c,
                    Err(e) => {
                        errs2.lock().unwrap()[k] = format!("启动 7z 失败: {}", e);
                        done2.lock().unwrap()[k] = Some(false);
                        return;
                    }
                };
                if let Some(o) = child.stdout.take() {
                    let mut reader = BufReader::new(o);
                    let mut raw: Vec<u8> = Vec::with_capacity(256);
                    let mut chunk = [0u8; 4096];
                    loop {
                        let n = match reader.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(_) => break,
                        };
                        for &b in &chunk[..n] {
                            if b == b'\r' || b == b'\n' {
                                if raw.is_empty() {
                                    continue;
                                }
                                let line = String::from_utf8_lossy(&raw).to_string();
                                raw.clear();
                                let t = line.trim_start();
                                if let Some(ps) = t.split('%').next() {
                                    if let Ok(pct) = ps.trim().parse::<f64>() {
                                        let pct = pct.clamp(0.0, 100.0);
                                        let done_now = base + (pct / 100.0 * *bytes as f64) as u64;
                                        prog2.lock().unwrap()[k] = done_now;
                                    }
                                }
                            } else {
                                raw.push(b);
                            }
                        }
                    }
                }
                // 先把 stderr 抽干再 wait, 避免管道写满把子进程卡死
                let err_txt = child
                    .stderr
                    .take()
                    .map(|e| BufReader::new(e).lines().flatten().collect::<Vec<_>>().join("; "))
                    .unwrap_or_default();
                let ok = child.wait().map(|s| s.success()).unwrap_or(false);
                if !ok {
                    if !err_txt.is_empty() {
                        errs2.lock().unwrap()[k] = err_txt;
                    }
                    done2.lock().unwrap()[k] = Some(false);
                    return;
                }
                base += *bytes;
                prog2.lock().unwrap()[k] = base;
            }
            done2.lock().unwrap()[k] = Some(true);
        }));
    }

    // 汇总进度: 每路的百分比 × 该路分配到的总字节
    let total_all = listing.total_size;
    let mut last_reported = 0.0f64;
    loop {
        if done.lock().unwrap().iter().all(|x| x.is_some()) {
            break;
        }
        let bytes: u64 = prog.lock().unwrap().iter().sum();
        let pct = if total_all > 0 {
            (bytes as f64 / total_all as f64 * 100.0).min(99.9)
        } else {
            0.0
        };
        // 进度没动就别重复刷 UI (省掉无意义的跨进程事件)
        if pct - last_reported >= 0.05 || pct >= 99.9 {
            last_reported = pct;
            progress_cb(
                pct,
                bytes.min(total_all),
                total_all,
                &format!("并行解压 ({} 路)", workers),
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
    for h in handles {
        let _ = h.join();
    }

    // 空目录不会被文件列表带出来 → 自己补建 (create_dir_all 幂等, 有文件在里面也不影响)
    for e in listing.entries.iter().filter(|e| e.is_dir) {
        let _ = std::fs::create_dir_all(safe_extract_path(dest, &e.path));
    }

    // ★ 哨兵校验: 7z 对"一个文件都没匹配上"是**退出码 0 + No files to process**(假装成功),
    //   所以不能只看退出码 —— 不然"并行什么都没解出来还报成功"就会静默丢数据。
    //   这里抽查每一路最大的那个文件在不在、大小对不对: O(路数) 次 stat, 几乎零成本。
    let mut all_ok = done.lock().unwrap().iter().all(|x| *x == Some(true));
    if all_ok {
        for b in buckets.iter() {
            if let Some(&i) = b.iter().max_by_key(|&&i| files[i].size) {
                let f = safe_extract_path(dest, &files[i].path);
                let got = std::fs::metadata(&f).map(|m| m.len()).ok();
                if got != Some(files[i].size) {
                    eprintln!(
                        "[extractor] 并行哨兵失败: {} 期望 {} 字节, 实际 {:?}",
                        f.display(),
                        files[i].size,
                        got
                    );
                    crate::app_logger::log_extract(
                        "PARALLEL_SENTINEL_FAIL",
                        &format!("path={} want={} got={:?}", f.display(), files[i].size, got),
                    );
                    all_ok = false;
                    break;
                }
            }
        }
    }
    if all_ok {
        progress_cb(100.0, total_all, total_all, "完成");
        Some(ExtractResult {
            success: true,
            output_dir: dest.to_path_buf(),
            error: String::new(),
        })
    } else {
        let msg = {
            let e = errs.lock().unwrap();
            e.iter().find(|s| !s.is_empty()).cloned().unwrap_or_default()
        };
        let msg = if msg.is_empty() {
            "解压失败 (并行 7z 返回非零退出码)".to_string()
        } else {
            msg
        };
        Some(ExtractResult {
            success: false,
            output_dir: dest.to_path_buf(),
            error: msg,
        })
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

    let out_dir_arg = long_path_arg(&out_dir);

    // ★ 修复 (issue 6): 7z l -slt 枚举归档内所有文件大小可能耗时数秒, 期间必须上报"扫描中",
    //   否则 UI 会长时间停在 0% 无任何反馈 (用户反馈: "停在那里好久")
    progress_cb(0.0, 0, 0, "扫描压缩包...");

    // 清单 (总大小 + 是否 solid + 每个文件大小) —— 并行解压也要用同一份
    let listing = list_archive(&seven_zip, archive, password);
    let total_size = listing.as_ref().map(|l| l.total_size).unwrap_or(0);

    // ★ 非 solid 归档 → 多路并行 (7z.exe 解码只有单线程, 实测 3 路 ≈ 3 倍)
    if let Some(l) = listing.as_ref() {
        if let Some(r) = extract_7zexe_parallel(
            &seven_zip,
            archive,
            &out_dir,
            &out_dir_arg,
            password,
            l,
            |p, b, t, f| progress_cb(p, b, t, f),
        ) {
            if r.success {
                return r;
            }
            // 并行失败(含哨兵校验不过) → 落回串行再试一次, 不把失败直接甩给用户
            eprintln!("[extractor] 并行解压失败 ({}), 回退串行", r.error);
            crate::app_logger::log_extract(
                "PARALLEL_FAIL",
                &format!("err={} → 串行 archive={}", r.error, archive.display()),
            );
        }
    }

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

    // ★ 7z / zip 也吃并行 (2026-10-12):
    //   这两个格式原本固定走纯 Rust 解码器 (extract_7z / extract_zip), 而纯 Rust 解码
    //   是**单线程**的。实测同一个 zip: 纯 Rust 19.3s vs 7z.exe 单进程 18.6s —— 基本同速,
    //   也就是说"纯 Rust 更快"并不成立, 换成 N 路 7z.exe 才是真的快。
    //   这里只对**大且非 solid** 的归档先试并行; 不适用/失败就落回原来的纯 Rust 优先逻辑,
    //   行为与改动前完全一致 (纯 Rust 的 AES/密码回退、进度语义都保留)。
    if (fmt == "7z" || fmt == "zip") && archive_size >= PAR_MIN_BYTES {
        if let Some(sz) = find_7z() {
            if let Some(l) = list_archive(&sz, archive, password) {
                if let Some(r) = extract_7zexe_parallel(
                    &sz,
                    archive,
                    &out_dir,
                    &long_path_arg(&out_dir),
                    password,
                    &l,
                    |p, b, t, f| progress_cb(p, b, t, f),
                ) {
                    if r.success {
                        crate::app_logger::log_extract(
                            "OK",
                            &format!("format={} engine=7zexe-parallel archive={}", fmt, archive.display()),
                        );
                        return r;
                    }
                    crate::app_logger::log_extract(
                        "PARALLEL_FAIL",
                        &format!(
                            "format={} err={} → 回落纯 Rust archive={}",
                            fmt, r.error, archive.display()
                        ),
                    );
                }
            }
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("vx_eocd_{}_{}", std::process::id(), name));
        p
    }

    /// 真 zip 的结尾一定有 EOCD（`PK\x05\x06` + 18 字节），不能误判成"没下完"。
    #[test]
    fn zip_missing_eocd_accepts_complete_zip() {
        let ok = tmp("ok.zip");
        let mut bytes = b"PK\x05\x06".to_vec();
        bytes.extend_from_slice(&[0u8; 18]);
        std::fs::write(&ok, &bytes).unwrap();
        assert!(!zip_missing_eocd(&ok), "空 zip 带 EOCD，应判为完整");
        let _ = std::fs::remove_file(&ok);
    }

    /// 下载中断的 zip：末尾没有 EOCD（而且长度够长，走的是真正的"扫末尾"分支）。
    #[test]
    fn zip_missing_eocd_detects_truncated_zip() {
        let bad = tmp("bad.zip");
        let mut bytes = b"PK\x03\x04\x14\x00\x00\x00\x08\x00".to_vec();
        bytes.extend_from_slice(&[0x41u8; 4096]);
        std::fs::write(&bad, &bytes).unwrap();
        assert!(zip_missing_eocd(&bad), "缺 EOCD 应判为不完整");
        let _ = std::fs::remove_file(&bad);
    }

    /// 比最小 EOCD(22 字节) 还短 → 不完整；文件不存在 → 不误报（交给后续逻辑报错）。
    #[test]
    fn zip_missing_eocd_short_and_missing_file() {
        let tiny = tmp("tiny.zip");
        std::fs::write(&tiny, b"PK").unwrap();
        assert!(zip_missing_eocd(&tiny), "太短应判为不完整");
        let _ = std::fs::remove_file(&tiny);

        let gone = tmp("definitely_absent.zip");
        let _ = std::fs::remove_file(&gone);
        assert!(!zip_missing_eocd(&gone), "文件不存在不应误报为不完整");
    }

    // ---------- 并行解压: 清单解析 ----------

    fn listing_of(text: &str) -> ArcListing {
        parse_listing_lines(text.lines().map(|s| s.to_string()))
    }

    /// 归档级的 Path / Solid 是**归档自己的属性**, 不是条目 —— 不能混进文件列表,
    /// 否则 total_size 会多算一个压缩包体积, 并行分配也会错。
    #[test]
    fn parse_listing_ignores_archive_level_fields() {
        let l = listing_of(
            "Path = D:\\game\\x\\a.rar\n\
             Type = Rar5\n\
             Physical Size = 74554055613\n\
             Solid = -\n\
             Blocks = 3\n\
             \n\
             ----------\n\
             Path = dir1\n\
             Folder = +\n\
             Size = 0\n\
             Path = dir1\\a.bin\n\
             Folder = -\n\
             Size = 1000\n\
             Solid = -\n",
        );
        assert_eq!(l.total_size, 1000, "只应统计正文里的文件, 不含归档自身");
        assert_eq!(l.entries.len(), 2, "目录 + 1 个文件");
        assert!(l.entries[0].is_dir);
        assert_eq!(l.entries[1].path, "dir1\\a.bin");
        assert_eq!(l.entries[1].size, 1000);
        assert!(!l.solid);
    }

    /// 非 RAR 的归档 (zip/7z) 用 `Attributes = D...` 标目录, 而不是 `Folder = +`。
    #[test]
    fn parse_listing_detects_dir_via_attributes() {
        let l = listing_of(
            "Path = a.zip\n\
             Type = zip\n\
             ----------\n\
             Path = sub\n\
             Attributes = D_ drwxrwxrwx\n\
             Size = 0\n\
             Path = sub\\f.txt\n\
             Attributes = A_ -rw-rw-rw-\n\
             Size = 42\n",
        );
        assert_eq!(l.total_size, 42);
        assert_eq!(l.entries.len(), 2);
        assert!(l.entries[0].is_dir, "Attributes 以 D 开头 → 目录");
        assert!(!l.entries[1].is_dir);
    }

    /// 归档级 `Solid = +` 或者**任何一个条目**带 `Solid = +` 都不能并行
    /// (solid 块里单独取一个文件要从块头开始解码, 并行只会更慢)。
    #[test]
    fn parse_listing_flags_solid_both_ways() {
        let archive_level = listing_of(
            "Path = a.7z\nSolid = +\n----------\nPath = f\nFolder = -\nSize = 10\n",
        );
        assert!(archive_level.solid, "归档级 Solid = + 要认出来");

        let entry_level = listing_of(
            "Path = a.rar\nSolid = -\n----------\nPath = f1\nFolder = -\nSize = 10\nSolid = -\nPath = f2\nFolder = -\nSize = 20\nSolid = +\n",
        );
        assert!(entry_level.solid, "任一 entry 带 Solid = + 也要认出来");
        assert_eq!(entry_level.total_size, 30);
    }

    /// 空归档 / 只有头部没有正文时, 不能 panic, 也不能把归档自身当条目。
    #[test]
    fn parse_listing_handles_empty_and_header_only() {
        let empty = listing_of("");
        assert_eq!(empty.total_size, 0);
        assert!(empty.entries.is_empty());

        let header_only = listing_of("Path = a.rar\nType = Rar5\nPhysical Size = 999\n");
        assert_eq!(header_only.total_size, 0);
        assert!(header_only.entries.is_empty(), "没有 ---------- 之前不该产出条目");
    }

    // ---------- 并行解压: LPT 分配 ----------

    fn max_load(buckets: &[Vec<usize>], sizes: &[u64]) -> u64 {
        buckets
            .iter()
            .map(|b| b.iter().map(|&i| sizes[i]).sum::<u64>())
            .max()
            .unwrap_or(0)
    }

    /// 所有下标都必须被分到**恰好一次** —— 漏了会丢文件, 重了会两个进程抢写同一个文件。
    #[test]
    fn partition_lpt_covers_every_index_exactly_once() {
        let sizes: Vec<u64> = vec![10, 9, 8, 7, 6, 5, 4, 3, 2, 1];
        let (buckets, loads) = partition_lpt(&sizes, 3);
        let mut seen: Vec<usize> = buckets.iter().flatten().copied().collect();
        seen.sort();
        assert_eq!(seen, (0..sizes.len()).collect::<Vec<_>>(), "每个文件恰好一次");
        assert_eq!(loads.iter().sum::<u64>(), 55);
        // 最优下界是 55/3 ≈ 18.3, LPT 给到 19 已经很接近
        assert_eq!(max_load(&buckets, &sizes), 19);
    }

    /// 游戏包的真实形状: 一两个几十 GB 的大文件占大头。
    /// 大文件必须落在**不同**的桶里, 否则并行等于没并。
    #[test]
    fn partition_lpt_spreads_the_big_files() {
        let sizes: Vec<u64> = vec![
            20_656_373_062,
            15_741_964_105,
            13_922_502_080,
            700_000_000,
            400_000_000,
            300_000_000,
            120_000_000,
            80_000_000,
            50_000_000,
            20_000_000,
            5_000_000,
            1_000_000,
        ];
        let (buckets, _) = partition_lpt(&sizes, 4);
        let bucket_of = |idx: usize| buckets.iter().position(|b| b.contains(&idx)).unwrap();
        let a = bucket_of(0);
        let b = bucket_of(1);
        let c = bucket_of(2);
        assert!(a != b && b != c && a != c, "三个大文件必须分散到不同路");
        // 最长那一路不该超过理论下界太多 (总量 50 GB / 4 路 = 12.5 GB,
        // 但单个文件就 20.6 GB, 所以下界就是 20.6 GB)
        let mx = max_load(&buckets, &sizes);
        assert!(mx >= 20_656_373_062 && mx < 21_500_000_000, "makespan 应贴着最大文件: {}", mx);
    }

    /// 命令行分批: 每批的路径总长不能超预算, 但**所有文件都必须被分到**且顺序不变。
    #[test]
    fn batch_by_argv_budget_splits_and_keeps_everything() {
        let paths: Vec<String> = (0..7).map(|i| format!("d\\f{}.bin", i)).collect(); // 每个 9~10 字符
        let sizes: Vec<u64> = (0..7).map(|i| (i as u64 + 1) * 100).collect();
        let batches = batch_by_argv_budget(&paths, &sizes, 30);

        let flat: Vec<String> = batches.iter().flat_map(|(p, _)| p.clone()).collect();
        assert_eq!(flat, paths, "分批不能丢文件、不能改顺序");

        let sum: u64 = batches.iter().map(|(_, b)| *b).sum();
        assert_eq!(sum, 100 + 200 + 300 + 400 + 500 + 600 + 700, "每批字节数要能对上");
        for (p, _) in batches.iter() {
            let chars: usize = p.iter().map(|s| s.chars().count() + 3).sum();
            assert!(chars <= 30, "单批超预算: {}", chars);
        }
    }

    /// 单个路径本身就超预算时, 它得**自己成一批** —— 不能空转死循环, 也不能被丢掉。
    #[test]
    fn batch_by_argv_budget_handles_oversized_single_path() {
        let paths = vec!["a".repeat(500), "b.bin".to_string()];
        let sizes = vec![11u64, 22];
        let batches = batch_by_argv_budget(&paths, &sizes, 20);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].0, vec!["a".repeat(500)]);
        assert_eq!(batches[0].1, 11);
        assert_eq!(batches[1].0, vec!["b.bin".to_string()]);
    }

    #[test]
    fn batch_by_argv_budget_empty() {
        assert!(batch_by_argv_budget(&[], &[], 100).is_empty());
    }

    #[test]
    fn partition_lpt_edge_cases() {
        let (b, _) = partition_lpt(&[], 4);
        assert_eq!(b.len(), 4, "空输入也要给够路数");
        assert!(b.iter().all(|x| x.is_empty()));

        let (b1, l1) = partition_lpt(&[7, 3], 1);
        assert_eq!(b1.len(), 1);
        assert_eq!(b1[0], vec![0, 1]);
        assert_eq!(l1[0], 10);

        // workers = 0 会被兜成 1, 不能 panic (min_by_key 在空区间上会炸)
        let (bz, _) = partition_lpt(&[5], 0);
        assert_eq!(bz.len(), 1);
        assert_eq!(bz[0], vec![0]);
    }
}
