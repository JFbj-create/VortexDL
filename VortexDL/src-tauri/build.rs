fn main() {
    let _ = tauri_build::build();
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let out_dir = std::env::var("OUT_DIR").unwrap_or_default();
    if !manifest.is_empty() && !out_dir.is_empty() {
        for dir in &["webview2"] {
            let src = std::path::Path::new(&manifest).join(dir);
            if src.exists() {
                let dst = std::path::Path::new(&out_dir).join(dir);
                let opts = fs_extra::dir::CopyOptions::new().overwrite(true).content_only(false);
                let _ = fs_extra::dir::copy(&src, &dst, &opts);
            }
        }
    }
    // ★ winres 在 GNU 工具链下产生格式不兼容的 libresource.a, 导致 exe 崩溃 (0xC0000005)
    //   完全跳过 winres, 图标不影响功能
    /*
    #[cfg(windows)]
    {
        if let Ok(res) = std::env::var("CARGO_CFG_TARGET_OS") {
            if res == "windows" {
                let mut r = winres::WindowsResource::new();
                r.set_icon("icons/icon.ico");
                r.set("FileDescription", "漩涡下载器 VortexDL");
                r.set("ProductName", "VortexDL");
                r.set("LegalCopyright", "Copyright (c) 2026 VortexDL");
                let _ = r.compile();
            }
        }
    }
    */
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=tauri.conf.json");
    println!("cargo:rerun-if-changed=../src");
    println!("cargo:rerun-if-changed=capabilities");
    println!("cargo:rerun-if-changed=icons/icon.ico");
}