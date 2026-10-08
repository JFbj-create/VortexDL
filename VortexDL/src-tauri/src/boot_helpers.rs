// Auto-generated: contains print_frontend_fingerprint helper, kept as module,
// but commands are now compiled directly in bin crate to ensure #[macro_export]
// macros (__cmd__X, __tauri_command_name_X) live in the same crate as generate_handler!
use std::path::Path;

pub fn print_frontend_fingerprint() {
    let front_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let candidate = [
        front_dir.join("app.js"),
        front_dir.join("src").join("app.js"),
    ];
    let mut app_js_path = None;
    for c in &candidate { if c.exists() { app_js_path = Some(c.clone()); break; } }
    if app_js_path.is_none() {
        let builtin = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("src").join("app.js");
        if builtin.exists() { app_js_path = Some(builtin); }
    }
    let sha = match &app_js_path {
        Some(p) => match std::fs::read(p) {
            Ok(v) => {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(&v);
                let d = h.finalize();
                d.iter().map(|b| format!("{:02x}", b)).collect::<String>()
            }
            Err(_) => String::from("NA_read_err"),
        },
        None => String::from("NA_missing"),
    };
    let line = format!(
        "[BOOT] 前端资源版本: front_v20260805_tr_local_v2 | app.js sha256: {sha}",
    );
    let log_dir = front_dir.join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_file = log_dir.join("vortex.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_file) {
        use std::io::Write;
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        let _ = writeln!(f, "[{ts}] {line}");
    }
    eprintln!("{line}");
}