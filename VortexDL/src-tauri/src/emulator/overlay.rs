//! 模拟器顶部功能条：一个**无边框、置顶、透明**的小窗口，浮在模拟器窗口上方。
//!
//! # 为什么单独开窗口
//!
//! 模拟器是外部进程，占满屏幕；主窗口里画的任何东西都会被它盖住。
//! 所以要一个独立的 always-on-top 窗口，位置/宽度跟着模拟器窗口走。
//!
//! 条上只有几个必要动作：最大化 / 全屏 / 设置 / 关闭游戏。
//! 设置面板里能改帧率上限、分辨率缩放、手柄震动、键位映射。

use tauri::{AppHandle, Emitter, Manager};

pub const LABEL: &str = "emu-overlay";

/// 打开（或复用）顶部功能条
pub fn open(app: &AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(LABEL) {
        let _ = w.show();
        let _ = w.set_always_on_top(true);
        let _ = w.emit("emu-overlay-refresh", ());
        return Ok(());
    }
    let w = tauri::WebviewWindowBuilder::new(
        app,
        LABEL,
        tauri::WebviewUrl::App("emu-overlay.html".into()),
    )
    .title("VortexDL 模拟器功能条")
    .inner_size(720.0, 44.0)
    .resizable(false)
    .decorations(false)
    .transparent(true)
    .visible(false)
    .build()
    .map_err(|e| format!("创建功能条窗口失败: {}", e))?;
    let _ = w.set_always_on_top(true);
    let _ = w.show();
    Ok(())
}

pub fn close(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(LABEL) {
        let _ = w.close();
    }
}

/// 设置面板展开/收起时调整窗口高度
pub fn resize(app: &AppHandle, height: f64) {
    if let Some(w) = app.get_webview_window(LABEL) {
        let _ = w.set_size(tauri::LogicalSize::new(720.0, height));
    }
    dock_top(app);
}

/// 把功能条挪到屏幕中上方（模拟器多是无边框全屏，顶部居中不会挡住 HUD）
pub fn dock_top(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(LABEL) {
        if let Ok(Some(mon)) = w.current_monitor() {
            let size = mon.size();
            let wsize = w.outer_size().unwrap_or(tauri::PhysicalSize::new(720, 44));
            let x = (size.width as i32 - wsize.width as i32) / 2;
            let y = 0;
            let _ = w.set_position(tauri::PhysicalPosition::new(x.max(0), y));
        }
    }
}
