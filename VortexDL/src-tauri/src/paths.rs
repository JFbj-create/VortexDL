//! 全局路径与通用小工具。
//!
//! ★ 这些原本挂在 `mc/mod.rs` 里 —— 但用户要求把「我的世界启动器」从安装包、
//!   公开源码和介绍里排除，MC 模块整体移到了 `_removed_mc/`，
//!   于是把**别的模块也在用**的那几个工具函数抽到这里，避免一起被删掉。
//!   现在 gx / anime / books / trainers 都走 `crate::paths::*`。

use std::path::{Path, PathBuf};

/// 主软件目录 —— exe 所在目录（用户说的"软件目录"）。
pub fn software_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `<软件目录>\data` —— 应用数据的统一根（gx/ books/ trainers/ anime/ ...）。
/// 用 exe 目录推导而不是写死盘符，换安装位置时自动跟着走。
pub fn data_dir() -> PathBuf {
    software_dir().join("data")
}

/// 递归复制目录（数据迁移用）。
pub fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let src = e.path();
        let dst = to.join(e.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

pub fn ensure_dir(p: &Path) -> Result<(), String> {
    std::fs::create_dir_all(p).map_err(|e| format!("创建目录失败 {}: {e}", p.display()))
}

/// 简易毫秒时间戳（避免额外依赖 chrono 的格式化差异）
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 生成短随机 id
pub fn short_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let mut x = nanos ^ (pid << 64);
    const AL: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut s = String::with_capacity(8);
    for _ in 0..8 {
        s.push(AL[(x % AL.len() as u128) as usize] as char);
        x /= AL.len() as u128;
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    }
    s
}

/// 把名字洗成安全的文件名（去掉路径分隔符与 Windows 非法字符）。
/// 原来在 `mc/mods.rs`，MC 移出后搬到这里（修改器库还在用）。
pub fn sanitize_filename(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim()
        .to_string();
    let cleaned: String = base
        .chars()
        .map(|c| if r#"<>:"/\|?*"#.contains(c) || (c as u32) < 32 { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.is_empty() {
        "mod.jar".to_string()
    } else {
        cleaned
    }
}

/// 长超时的 HTTP 客户端（大文件下载用；原来是 MC 模块专用的 `mc_client`）
pub fn big_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(format!(
            "VortexDL/{}/{} (+https://github.com/)",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS
        ))
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(300))
        .pool_max_idle_per_host(32)
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default()
}

/// 解压 zip 到目录（保留目录结构）
pub fn extract_zip(zip_path: &Path, out: &Path) -> Result<(), String> {
    let f = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(f).map_err(|e| format!("打开压缩包失败: {e}"))?;
    ensure_dir(out)?;
    for i in 0..zip.len() {
        let mut e = zip.by_index(i).map_err(|e| e.to_string())?;
        let Some(rel) = e.enclosed_name() else { continue };
        let dest = out.join(rel);
        if e.is_dir() {
            ensure_dir(&dest)?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            ensure_dir(parent)?;
        }
        let mut buf = Vec::with_capacity(e.size() as usize);
        std::io::copy(&mut e, &mut buf).map_err(|e| e.to_string())?;
        std::fs::write(&dest, &buf).map_err(|e| format!("写入 {} 失败: {e}", dest.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_data_dir_is_under_software_dir() {
        let d = data_dir();
        assert!(d.ends_with("data"), "{:?}", d);
        assert_eq!(d.parent().unwrap(), software_dir());
    }

    #[test]
    fn test_short_id_unique_and_sized() {
        let a = short_id();
        let b = short_id();
        assert_eq!(a.len(), 8);
        assert_ne!(a, b, "连续两次不该一样");
    }

    #[test]
    fn test_copy_tree_roundtrip() {
        let base = std::env::temp_dir().join("vx_paths_test");
        let _ = std::fs::remove_dir_all(&base);
        let (src, dst) = (base.join("a"), base.join("b"));
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("f.txt"), b"hi").unwrap();
        std::fs::write(src.join("sub").join("g.txt"), b"yo").unwrap();
        copy_tree(&src, &dst).unwrap();
        assert!(dst.join("f.txt").is_file());
        assert!(dst.join("sub").join("g.txt").is_file());
        let _ = std::fs::remove_dir_all(&base);
    }
}
