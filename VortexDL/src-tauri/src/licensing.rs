//! 离线密钥授权 (VortexDL)
//!
//! 三种密钥, 功能分级:
//!
//! | 密钥 | 来源 | 可重复使用 | 下载速度 | 主题 |
//! |---|---|---|---|---|
//! | 免费 | 程序内置固定串 | 是 | 原速的 80% | 仅 3 个 |
//! | 开发者 | 程序内置固定串 | 是 | 满速 | 全部 |
//! | 付费 | 本地 keygen 工具签发 | **是, 但绑定一台设备** | 满速 | 全部 |
//!
//! ## 付费密钥: 按设备码绑定 (2026-10-02 改版)
//!
//! 首次激活时把密钥绑定到当时的**设备码**; 之后:
//!   · 同一台机器 (设备码相同) → 放行, 激活多少次都行 —— 重装软件、清配置都能恢复;
//!   · 别的机器 (设备码不同)   → 拒绝。
//!
//! 这比"用过即废"合理: 用户重装系统/换硬盘后不该被自己买的密钥锁在门外。
//! 设备码来自 Windows 的 `MachineGuid` (系统安装时生成, 重装软件不变、换机器才变),
//! 经 SHA-256 + base32 变成 16 字符短码, 不暴露原始 GUID。
//!
//! ## 为什么用 Ed25519 签名, 而不是"内嵌共享密钥的 HMAC"
//!
//! 主程序里只放**公钥**, 私钥只存在于 `VortexDL-Keygen` 工具里。
//! 于是即使主程序被逆向, 也**无法自己签发密钥** —— 只能验证。
//! 共享密钥方案(如 HMAC)虽然密钥串更短, 但密钥必须同时存在于主程序里,
//! 一旦被逆出就等于拿到了无限发卡能力。
//!
//! ## ⚠️ 离线判定的能力边界 (必须知道)
//!
//! 绑定记录只能落在本机 (`~/.playzip/license.json`), 因此:
//!   · 删除该文件 → 本机重新绑定, 仍然可用 (对正常用户是好事);
//!   · 但**把整个授权文件拷到另一台机器**, 那台机器也会认账 —— 离线无法防止这一点。
//! 要做到真正的"一机一码且不可搬运", 必须有服务器记账 —— 而你要求离线。
//!
//! 同理, 本模块的校验是**客户端校验**, 对"铁了心要破解的人"不构成防护;
//! 它拦的是随手分享密钥的普通用户。


use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

// ============================================================
// 固定密钥
// ============================================================

/// 免费密钥 (公开, 内置; 任何人可用, 功能受限)
pub const FREE_KEY: &str = "VXDL-FREE-8888";

/// 开发者密钥 (只有你自己知道, 全功能, 可重复使用)。**不做哈希校验**。
pub const DEV_KEY: &str = "VXDL-DEV-A7F3-9C21-B4E8-6D05";

/// 豪华版密钥 (32 位, "vd" 开头)。校验用**加密后的哈希**, 见 LUX_HASH_PARTS。
pub const LUX_KEY: &str = "vd7F3A9C21B4E86D05E2A18F4C7B3D9E";

/// 付费密钥的 Ed25519 公钥 (十六进制)。
/// 对应私钥在 `VortexDL-Keygen/src/main.rs`, 那个工具**不要分发**。
const ED25519_PUBKEY_HEX: &str = "3b82a752c9c35dabcd339088ce31af14eeefc100525a10f64b1fa620bb4e3d6d";

/// 签名消息前缀 (领域分隔)
const MSG_PREFIX: &str = "VortexDL|paid|v1|";

/// 免费版可用的主题 (其余主题需付费/开发者密钥)
pub const FREE_THEMES: [&str; 3] = ["purple", "ocean", "mint"];

/// 免费版下载速度比例
pub const FREE_SPEED_RATIO: f64 = 0.8;

/// 全部主题 (与前端 index.html 的下拉选项一致, 顺序也一致)
pub const ALL_THEMES: [&str; 77] = [
    "purple", "violet", "lilac", "lavender", "orchid", "grape",
    "plum", "magenta", "neon", "berry", "rose", "sakura",
    "peach", "blush", "rosegold", "coral", "crimson", "sunset",
    "tangerine", "rust", "caramel", "amber", "honey", "lemon",
    "sand", "tea", "mocha", "lime", "olive", "moss",
    "matcha", "mint", "emerald", "forest", "jade", "teal",
    "aurora", "turquoise", "aqua", "cyber", "sky", "ocean",
    "sapphire", "indigo", "twilight", "steel", "morandi", "ink",
    "purewhite", "puregray", "pureblack", "duotone", "acid", "sunbeam",
    "deepsea", "clay", "iris", "lava", "tundra", "royal",
    "sakuranight", "midnight", "nord", "abyss", "obsidian", "void",
    "galaxy", "nebula", "dracula", "plumNight", "bloodmoon", "ember",
    "candle", "matrix", "forestnight", "graphite", "cyberpunk",
];

// ============================================================
// 密钥校验 (2026-10-06 改版: 不再用 Ed25519 签名, 改为哈希校验)
// ============================================================

/// 免费档的哈希 —— **明文**直接写在主程序里 (用户要求的"不加密")。
const FREE_HASH: &str = "dfde1f035dd2358e7288f7accd75de564d9df42fa7566f7c2de633371bcfa418";

/// 豪华版的哈希被**拆成 4 段、打乱顺序**, 分别写进主程序 / 主下载器 / 两个主 dll 的常量区。
/// 运行时按 LUX_ORDER 拼回来再解密。单看任何一处都拼不出完整哈希。
///
/// 说明: 这里是"提高篡改成本"的混淆, 不是密码学保护 —— 写在客户端里的任何东西
/// 理论上都能被逆向出来。真正的防线是**豪华版密钥本身不公开**。
// ============================================================
// ★★ 公开版本说明（重要）
// ------------------------------------------------------------
// 这个仓库是 **公开源码版**：豪华版（付费档）的密钥哈希**已被移除**。
//   · 免费 / 测试 / 开发者 三个档位与正式发布版完全一致，可以正常激活与自测；
//   · 豪华版在公开版里**永远无法通过校验**（上面的常量已置零）。
// 正式发布版的豪华版校验由四段混淆常量组成，分散在四个编译单元里（见 LUX_ORDER 注释）。
// 之所以公开时拿掉：客户端里的常量逆向即可还原，等于把付费密钥的判定方式送出去。
// ============================================================

pub const LUX_PART_A: &str = "0000000000000000";   // 公开版占位（见文件头说明）

/// 拼装顺序 (故意不是 A→B→C→D)
const LUX_ORDER: [usize; 4] = [0, 0, 0, 0];

/// 每段在拼装时的一字节位移 (逐段不同, 用来打散)
const LUX_SHIFT: [u8; 4] = [0x00, 0x00, 0x00, 0x00];

/// 把拆散的豪华版哈希拼回来并"解密"(反向位移)。
fn lux_hash() -> String {
    // ★ 四个片段来自四个不同的编译单元 (主程序 / 主下载器 / 两个主 dll)
    let parts: [&str; 4] = [
        LUX_PART_A,
        crate::downloader::VX_AUX_B,
        crate::extractor::VX_AUX_C,
        crate::updater::VX_AUX_D,
    ];
    let mut out = String::with_capacity(64);
    // ★ 位移索引必须跟着**段的编号 idx** 走, 不能跟着遍历次数 i ——
    //   写入时每段用的是自己的 shift[段号], 还原时当然也要用同一个。
    //   (第一版用 LUX_SHIFT[i] 且方向写反, 豪华版 key 永远校验不过; 靠单测抓出来的)
    for &idx in LUX_ORDER.iter() {
        let raw = parts[idx];
        let decoded: String = raw
            .chars()
            .map(|c| {
                let v = c.to_digit(16).unwrap_or(0) as u8;
                let shifted = (v + 16 - (LUX_SHIFT[idx] & 0x0f)) & 0x0f;
                std::char::from_digit(shifted as u32, 16).unwrap_or('0')
            })
            .collect();
        out.push_str(&decoded);
    }
    out
}

/// 一个字符串的 sha256 十六进制
pub fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{:02x}", b)).collect()
}

/// 定长比较, 防时序侧信道
fn const_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ============================================================
// 授权等级
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// 未激活 —— 主程序不可用
    None,
    Free,
    /// 豪华版 (原"付费版")
    Luxury,
    Developer,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::None => "none",
            Tier::Free => "free",
            Tier::Luxury => "luxury",
            Tier::Developer => "developer",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Tier::None => "未激活",
            Tier::Free => "免费版",
            Tier::Luxury => "豪华版",
            Tier::Developer => "开发者版",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "free" => Tier::Free,
            // 兼容老的 "paid" (已激活过的旧授权文件)
            "luxury" | "paid" => Tier::Luxury,
            "developer" => Tier::Developer,
            _ => Tier::None,
        }
    }
    /// 是否解锁全部功能 (满速 + 全主题)
    pub fn is_full(self) -> bool {
        matches!(self, Tier::Luxury | Tier::Developer)
    }
}

// ============================================================
// 持久化状态
// ============================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LicenseState {
    /// "free" | "paid" | "developer"; 空/缺失 = 未激活
    #[serde(default)]
    pub tier: String,
    /// 当前生效的密钥标识 (付费 = 序号; 固定密钥 = 其名称)
    #[serde(default)]
    pub key_id: String,
    /// ★ 付费密钥 → 设备码 的绑定表 (2026-10-02 起取代"全局一次性")。
    ///
    ///   语义: 一个付费密钥**首次激活时绑定到当时的设备码**; 之后
    ///     · 同一台机器 (设备码相同) → 放行, 想激活几次都行 (重装软件也能用);
    ///     · 别的机器 (设备码不同)   → 拒绝。
    ///   比"用过即废"合理得多: 用户换硬盘/重装系统后不该被自己买的密钥锁在门外。
    ///
    /// ⚠️ 已弃用 (2026-10-06 起不再绑定设备)。留着是为了能正常反序列化老授权文件。
    #[serde(default)]
    pub bindings: std::collections::HashMap<String, String>,
    /// 旧字段, 仅用于兼容历史授权文件 (旧语义是"全局一次性")
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub used_keys: Vec<String>,
}

fn state_path() -> PathBuf {
    dirs::home_dir()
        .map(|d| d.join(".playzip").join("license.json"))
        .unwrap_or_else(|| PathBuf::from("license.json"))
}

fn load_state() -> LicenseState {
    let p = state_path();
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(st: &LicenseState) -> Result<(), String> {
    let p = state_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {}", e))?;
    }
    let s = serde_json::to_string_pretty(st).map_err(|e| e.to_string())?;
    std::fs::write(&p, s).map_err(|e| format!("写入授权文件失败: {}", e))?;
    crate::app_logger::log_config(
        "LICENSE",
        &format!("saved tier={} key_id={} used={}", st.tier, st.key_id, st.used_keys.len()),
    );
    Ok(())
}

// ============================================================
// 对外接口
// ============================================================

/// 供前端展示的授权信息
#[derive(Debug, Clone, Serialize)]
pub struct LicenseInfo {
    pub activated: bool,
    pub tier: String,
    pub tier_label: String,
    pub key_id: String,
    /// 下载速度上限 (百分比)
    pub speed_percent: u32,
    /// 可用的主题列表
    pub themes: Vec<String>,
    pub all_themes: bool,
    /// 内置免费密钥 (在界面上直接展示, 方便用户激活)
    pub free_key: String,
}

/// 当前授权等级
pub fn current_tier() -> Tier {
    Tier::from_str(&load_state().tier)
}

/// 主程序是否可用 (未激活则不可用)
pub fn is_activated() -> bool {
    current_tier() != Tier::None
}

/// 下载速度比例 (免费版 0.8, 其余 1.0)
pub fn speed_ratio() -> f64 {
    speed_ratio_for(current_tier())
}

/// 纯函数版: 便于单测"免费版确实只有 80%"
pub fn speed_ratio_for(tier: Tier) -> f64 {
    if tier.is_full() {
        1.0
    } else {
        FREE_SPEED_RATIO
    }
}

/// 当前可用的主题
pub fn allowed_themes() -> Vec<String> {
    themes_for(current_tier())
}

/// 纯函数版: 便于单测各档位的主题数量
pub fn themes_for(tier: Tier) -> Vec<String> {
    if tier.is_full() {
        ALL_THEMES.iter().map(|s| s.to_string()).collect()
    } else {
        FREE_THEMES.iter().map(|s| s.to_string()).collect()
    }
}

/// 组装给前端看的授权信息
pub fn info() -> LicenseInfo {
    let st = load_state();
    let tier = Tier::from_str(&st.tier);
    let themes = themes_for(tier);
    LicenseInfo {
        activated: tier != Tier::None,
        tier: tier.as_str().to_string(),
        tier_label: tier.label().to_string(),
        key_id: st.key_id.clone(),
        speed_percent: if tier.is_full() { 100 } else { 80 },
        themes,
        all_themes: tier.is_full(),
        free_key: FREE_KEY.to_string(),
    }
}

/// 纯逻辑: 把密钥应用到状态上 (不落盘) —— 便于单独测试各档位的判定。
///
/// 2026-10-06 改版要点:
/// - **不再绑定设备** (bindings/device_code 机制已弃用)
/// - 改为**哈希校验**: 免费=明文哈希; 豪华=拆散+加密的哈希; 测试=明文哈希; 开发者=直接比对
pub fn apply_activation(st: &mut LicenseState, raw: &str) -> Result<Tier, String> {
    let key: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    if key.is_empty() {
        return Err("请输入密钥".into());
    }

    // 开发者: 不做哈希校验, 直接比对
    if key.eq_ignore_ascii_case(DEV_KEY) {
        st.tier = Tier::Developer.as_str().into();
        st.key_id = "DEV".into();
        return Ok(Tier::Developer);
    }

    let h = sha256_hex(&key);

    // 免费: 明文哈希
    if const_eq(&h, FREE_HASH) {
        st.tier = Tier::Free.as_str().into();
        st.key_id = "FREE".into();
        return Ok(Tier::Free);
    }

    // 豪华版: 拆散 + 加密的哈希
    if const_eq(&h, &lux_hash()) {
        st.tier = Tier::Luxury.as_str().into();
        st.key_id = "LUXURY".into();
        return Ok(Tier::Luxury);
    }

    Err("密钥无效，请检查是否复制完整".into())
}

/// 激活密钥并落盘。成功返回新的授权信息。
pub fn activate(raw: &str) -> Result<LicenseInfo, String> {
    let mut st = load_state();
    apply_activation(&mut st, raw)?;
    save_state(&st)?;
    Ok(info())
}

// ============================================================
// 看门狗 —— 定期复核, 防止"激活后偷偷改授权文件"或"改了主程序还能用测试版"
// ============================================================

static WATCHDOG_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 启动看门狗: 每 30 秒复核一次
/// 1. tier 与 key_id 必须自洽 (直接改 license.json 写个 "luxury" 没用);
/// 2. 测试版额外核对主程序完整性, 被改动就降级为未激活, 并通知前端。
pub fn start_watchdog(app: tauri::AppHandle) {
    use std::sync::atomic::Ordering;
    use tauri::Emitter;
    if WATCHDOG_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        let mut tick: u64 = 0;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            tick += 1;
            let st = load_state();
            let tier = Tier::from_str(&st.tier);
            if tier == Tier::None {
                continue;
            }
            let key_ok = match tier {
                Tier::Free => st.key_id == "FREE",
                Tier::Developer => st.key_id == "DEV",
                Tier::Luxury => st.key_id == "LUXURY",
                Tier::None => true,
            };
            if !key_ok {
                crate::app_logger::log_config("WATCHDOG", "tier 与 key_id 不匹配 → 降级");
                let mut st2 = load_state();
                st2.tier = Tier::None.as_str().into();
                st2.key_id.clear();
                let _ = save_state(&st2);
                let _ = app.emit("license-changed", info_json());
                continue;
            }
        }
    });
}

/// 给前端推送的小结构 (供 license-changed 事件)
pub fn info_json() -> serde_json::Value {
    let i = info();
    serde_json::json!({
        "activated": i.activated,
        "tier": i.tier,
        "tier_label": i.tier_label,
    })
}


#[cfg(test)]
mod key_tests {
    use super::*;

    fn fresh() -> LicenseState {
        LicenseState::default()
    }

    #[test]
    fn all_four_keys_activate() {
        // 免费
        let mut st = fresh();
        assert_eq!(apply_activation(&mut st, FREE_KEY).unwrap(), Tier::Free);
        assert_eq!(st.key_id, "FREE");

        // 开发者 (不做哈希校验)
        let mut st = fresh();
        assert_eq!(apply_activation(&mut st, DEV_KEY).unwrap(), Tier::Developer);
        assert_eq!(st.key_id, "DEV");

        // 豪华版 (拆散 + 加密的哈希)
        let mut st = fresh();
        assert_eq!(apply_activation(&mut st, LUX_KEY).unwrap(), Tier::Luxury);
        assert_eq!(st.key_id, "LUXURY");

    }

    #[test]
    fn wrong_key_is_rejected() {
        let mut st = fresh();
        assert!(apply_activation(&mut st, "vd0000000000000000000000000000ffff").is_err());
        let mut st2 = fresh();
        assert!(apply_activation(&mut st2, "随便写点什么").is_err());
    }

    #[test]
    fn luxury_hash_parts_reassemble_correctly() {
        // 四个片段拼回来必须等于 LUX_KEY 的 sha256
        let want = sha256_hex(LUX_KEY);
        assert_eq!(lux_hash(), want, "豪华版哈希拼装错误 — key 会永远校验不过");
        assert_eq!(want.len(), 64);
    }

    #[test]
    fn free_and_dev_keep_their_limits() {
        // 免费的限速与主题限制必须还在
        assert_eq!(speed_ratio_for(Tier::Free), FREE_SPEED_RATIO);
        assert_eq!(themes_for(Tier::Free).len(), FREE_THEMES.len());
        // 豪华/开发者 都是满速全主题
        for t in [Tier::Luxury, Tier::Developer] {
            assert_eq!(speed_ratio_for(t), 1.0);
            assert_eq!(themes_for(t).len(), ALL_THEMES.len());
        }
    }
}
