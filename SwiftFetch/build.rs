use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let profile = env::var("PROFILE").unwrap();
    let target_dir = PathBuf::from(&manifest_dir).join("target").join(&profile);

    let plugins_dir = PathBuf::from(&manifest_dir).join("plugins");
    let swiftfetch_cli_dir = target_dir.join("SwiftFetch_CLI");
    let cli_plugins_dir = swiftfetch_cli_dir.join("plugins");

    fs::create_dir_all(&cli_plugins_dir).ok();

    let hello_exe_src = target_dir.join("hello_plugin.exe");
    let hello_exe_dst = cli_plugins_dir.join("hello_plugin.exe");
    if hello_exe_src.exists() {
        let _ = fs::copy(&hello_exe_src, &hello_exe_dst);
    } else {
        let candidate = target_dir.join("deps").join("hello_plugin.exe");
        if candidate.exists() {
            let _ = fs::copy(&candidate, &hello_exe_dst);
        }
    }

    let swiftfetch_exe_src = target_dir.join("swiftfetch.exe");
    let swiftfetch_exe_dst = swiftfetch_cli_dir.join("SwiftFetch.exe");
    if swiftfetch_exe_src.exists() {
        fs::create_dir_all(&swiftfetch_cli_dir).ok();
        let _ = fs::copy(&swiftfetch_exe_src, &swiftfetch_exe_dst);
    }

    println!("cargo:rerun-if-changed=plugins/hello_plugin/src/main.rs");
    println!("cargo:rerun-if-changed=plugins/hello_plugin/Cargo.toml");
}
