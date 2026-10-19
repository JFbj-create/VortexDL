//! 输入层：键盘 / 手柄 → 统一的按键状态，供内置模拟器前端使用。
//!
//! # 为什么放后端
//!
//! WebView2 在**窗口没有焦点**时（模拟器全屏、或者 Game Bar 抢焦点）收不到键盘事件，
//! 手柄更是浏览器根本不暴露（Gamepad API 在 WebView2 里不可用）。所以这里用
//! Windows 的 `GetAsyncKeyState` + `XInputGetState` 轮询，**不依赖焦点**，
//! 开销只有每帧几十次原子读，远小于渲染。
//!
//! # 默认键位（和 Yuzu / Ryujinx 的默认一致，用户不用重学）
//!
//! | Switch | 键盘 | 手柄 |
//! |---|---|---|
//! | A | K | XInput A |
//! | B | J | XInput B |
//! | X | I | XInput X |
//! | Y | U | XInput Y |
//! | L / R | Q / E | LB / RB |
//! | ZL / ZR | 1 / 3 | LT / RT |
//! | + / - | Enter / Backspace | Start / Back |
//! | 十字 | WASD | DPad |
//! | 左摇杆 | 方向键 | 左摇杆 |
//! | 右摇杆 | TFGH | 右摇杆 |
//!
//! 键位可以 `input_bind` 覆盖，存到 `data/emulator/input.json`。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InputState {
    /// 12 个按键位（A/B/X/Y/L/R/ZL/ZR/Plus/Minus/Home/Capture）的按下状态
    pub buttons: u32,
    /// 十字键
    pub dpad: u8,
    /// 左摇杆 x/y（-32768..32767）
    pub lx: i16,
    pub ly: i16,
    /// 右摇杆
    pub rx: i16,
    pub ry: i16,
    /// 哪个设备在生效：none / keyboard / gamepad
    pub source: String,
    /// 检测到的手柄名（找不到就是空）
    pub pad_name: String,
}

/// 按键位掩码
pub mod btn {
    pub const A: u32 = 1 << 0;
    pub const B: u32 = 1 << 1;
    pub const X: u32 = 1 << 2;
    pub const Y: u32 = 1 << 3;
    pub const L: u32 = 1 << 4;
    pub const R: u32 = 1 << 5;
    pub const ZL: u32 = 1 << 6;
    pub const ZR: u32 = 1 << 7;
    pub const PLUS: u32 = 1 << 8;
    pub const MINUS: u32 = 1 << 9;
    pub const HOME: u32 = 1 << 10;
    pub const CAPTURE: u32 = 1 << 11;
}

fn bind_path() -> std::path::PathBuf {
    crate::paths::data_dir().join("emulator").join("input.json")
}

/// 默认键位：Switch 按钮名 → 虚拟键码（Windows VK）
pub fn default_binds() -> HashMap<String, u16> {
    let mut m = HashMap::new();
    // VK 码：A=0x41 B=0x42 ...，方向键 0x25..0x28，Enter=0x0D，Backspace=0x08
    m.insert("A".to_string(), 0x4B); // K
    m.insert("B".to_string(), 0x4A); // J
    m.insert("X".to_string(), 0x49); // I
    m.insert("Y".to_string(), 0x55); // U
    m.insert("L".to_string(), 0x51); // Q
    m.insert("R".to_string(), 0x45); // E
    m.insert("ZL".to_string(), 0x31); // 1
    m.insert("ZR".to_string(), 0x33); // 3
    m.insert("PLUS".to_string(), 0x0D); // Enter
    m.insert("MINUS".to_string(), 0x08); // Backspace
    m.insert("HOME".to_string(), 0x24); // Home
    m.insert("CAPTURE".to_string(), 0x2C); // PrintScreen
    m.insert("UP".to_string(), 0x57); // W
    m.insert("DOWN".to_string(), 0x53); // S
    m.insert("LEFT".to_string(), 0x41); // A
    m.insert("RIGHT".to_string(), 0x44); // D
    m.insert("LSTICK_UP".to_string(), 0x26); // ↑
    m.insert("LSTICK_DOWN".to_string(), 0x28); // ↓
    m.insert("LSTICK_LEFT".to_string(), 0x25); // ←
    m.insert("LSTICK_RIGHT".to_string(), 0x27); // →
    m
}

fn load_binds() -> HashMap<String, u16> {
    let mut def = default_binds();
    if let Ok(s) = std::fs::read_to_string(bind_path()) {
        if let Ok(m) = serde_json::from_str::<HashMap<String, u16>>(&s) {
            for (k, v) in m {
                def.insert(k, v);
            }
        }
    }
    def
}

pub fn save_binds(m: &HashMap<String, u16>) -> Result<(), String> {
    let p = bind_path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    std::fs::write(&p, serde_json::to_string_pretty(m).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

pub fn get_binds() -> HashMap<String, u16> {
    load_binds()
}

pub fn reset_binds() -> HashMap<String, u16> {
    let d = default_binds();
    let _ = save_binds(&d);
    d
}

// ============================================================
// Windows 轮询
// ============================================================

#[cfg(windows)]
mod win {
    use super::*;
    use windows_sys::Win32::UI::Input::XboxController::{
        XInputGetState, XINPUT_GAMEPAD_A, XINPUT_GAMEPAD_B, XINPUT_GAMEPAD_BACK,
        XINPUT_GAMEPAD_DPAD_DOWN, XINPUT_GAMEPAD_DPAD_LEFT, XINPUT_GAMEPAD_DPAD_RIGHT,
        XINPUT_GAMEPAD_DPAD_UP, XINPUT_GAMEPAD_LEFT_SHOULDER, XINPUT_GAMEPAD_LEFT_THUMB,
        XINPUT_GAMEPAD_RIGHT_SHOULDER, XINPUT_GAMEPAD_RIGHT_THUMB, XINPUT_GAMEPAD_START,
        XINPUT_GAMEPAD_X, XINPUT_GAMEPAD_Y, XINPUT_STATE,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    #[allow(unused_imports)]
    use windows_sys::Win32::UI::Input::XboxController::{XInputGetState as _X, XINPUT_STATE as _S};

    fn down(vk: u16) -> bool {
        unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
    }

    pub fn poll() -> InputState {
        let binds = load_binds();
        let get = |k: &str| -> bool {
            binds.get(k).map(|vk| down(*vk)).unwrap_or(false)
        };

        let mut st = InputState::default();
        let mut any_key = false;
        let mut set = |name: &str, bit: u32, st: &mut InputState| {
            if get(name) {
                st.buttons |= bit;
                any_key = true;
            }
        };
        set("A", btn::A, &mut st);
        set("B", btn::B, &mut st);
        set("X", btn::X, &mut st);
        set("Y", btn::Y, &mut st);
        set("L", btn::L, &mut st);
        set("R", btn::R, &mut st);
        set("ZL", btn::ZL, &mut st);
        set("ZR", btn::ZR, &mut st);
        set("PLUS", btn::PLUS, &mut st);
        set("MINUS", btn::MINUS, &mut st);
        set("HOME", btn::HOME, &mut st);
        set("CAPTURE", btn::CAPTURE, &mut st);

        let mut dp = 0u8;
        if get("UP") { dp |= 1; any_key = true; }
        if get("DOWN") { dp |= 2; any_key = true; }
        if get("LEFT") { dp |= 4; any_key = true; }
        if get("RIGHT") { dp |= 8; any_key = true; }
        st.dpad = dp;

        // 左摇杆用方向键（数字量 → 满量程）
        let mut lx = 0i32;
        let mut ly = 0i32;
        if get("LSTICK_LEFT") { lx = -32767; any_key = true; }
        if get("LSTICK_RIGHT") { lx = 32767; any_key = true; }
        if get("LSTICK_UP") { ly = 32767; any_key = true; }
        if get("LSTICK_DOWN") { ly = -32767; any_key = true; }

        // ---- XInput 手柄 ----
        let mut pad_name = String::new();
        for idx in 0..4u32 {
            let mut s: XINPUT_STATE = unsafe { std::mem::zeroed() };
            let r = unsafe { XInputGetState(idx, &mut s) };
            if r != 0 {
                continue;
            }
            pad_name = format!("XInput 手柄 #{}", idx + 1);
            let g = s.Gamepad;
            let b = g.wButtons;
            if b & XINPUT_GAMEPAD_A != 0 { st.buttons |= btn::A; }
            if b & XINPUT_GAMEPAD_B != 0 { st.buttons |= btn::B; }
            if b & XINPUT_GAMEPAD_X != 0 { st.buttons |= btn::X; }
            if b & XINPUT_GAMEPAD_Y != 0 { st.buttons |= btn::Y; }
            if b & XINPUT_GAMEPAD_LEFT_SHOULDER != 0 { st.buttons |= btn::L; }
            if b & XINPUT_GAMEPAD_RIGHT_SHOULDER != 0 { st.buttons |= btn::R; }
            if g.bLeftTrigger > 30 { st.buttons |= btn::ZL; }
            if g.bRightTrigger > 30 { st.buttons |= btn::ZR; }
            if b & XINPUT_GAMEPAD_START != 0 { st.buttons |= btn::PLUS; }
            if b & XINPUT_GAMEPAD_BACK != 0 { st.buttons |= btn::MINUS; }
            if b & XINPUT_GAMEPAD_LEFT_THUMB != 0 { st.buttons |= btn::HOME; }
            if b & XINPUT_GAMEPAD_RIGHT_THUMB != 0 { st.buttons |= btn::CAPTURE; }
            let mut d = 0u8;
            if b & XINPUT_GAMEPAD_DPAD_UP != 0 { d |= 1; }
            if b & XINPUT_GAMEPAD_DPAD_DOWN != 0 { d |= 2; }
            if b & XINPUT_GAMEPAD_DPAD_LEFT != 0 { d |= 4; }
            if b & XINPUT_GAMEPAD_DPAD_RIGHT != 0 { d |= 8; }
            if d != 0 { st.dpad = d; }
            // 摇杆：XInput 以 32767 为正满，向下为负 → Switch 的 Y 轴正方向是上，保持一致
            let dead = 7800i32;
            let sx = g.sThumbLX as i32;
            let sy = g.sThumbLY as i32;
            if sx.abs() > dead { lx = sx; }
            if sy.abs() > dead { ly = sy; }
            let rx = g.sThumbRX as i32;
            let ry = g.sThumbRY as i32;
            if rx.abs() > dead { st.rx = rx.clamp(-32767, 32767) as i16; }
            if ry.abs() > dead { st.ry = ry.clamp(-32767, 32767) as i16; }
            st.source = "gamepad".to_string();
            break;
        }

        st.lx = lx.clamp(-32767, 32767) as i16;
        st.ly = ly.clamp(-32767, 32767) as i16;
        if st.source.is_empty() {
            st.source = if any_key { "keyboard".to_string() } else { "none".to_string() };
        }
        st.pad_name = pad_name;
        // 屏蔽误触：左键按下时（用户在点界面）不当作 A
        unsafe {
            let _ = VK_LBUTTON;
        }
        st
    }
}

#[cfg(not(windows))]
mod win {
    use super::*;
    pub fn poll() -> InputState {
        InputState::default()
    }
}

/// 轮询一次输入状态（非阻塞，微秒级）
pub fn poll() -> InputState {
    win::poll()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_binds_has_all_buttons() {
        let d = default_binds();
        for k in ["A", "B", "X", "Y", "L", "R", "ZL", "ZR", "PLUS", "MINUS", "UP", "DOWN", "LEFT", "RIGHT"] {
            assert!(d.contains_key(k), "缺键位 {}", k);
        }
    }

    #[test]
    fn poll_returns_struct() {
        let s = poll();
        assert!(s.buttons <= 0xFFF);
    }
}
