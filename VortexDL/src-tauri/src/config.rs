use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)] pub last_seen_version: String,
    #[serde(default)] pub download_dir: String,
    #[serde(default)] pub extract_dir: String,
    #[serde(default)] pub translate_lang: String,
    #[serde(default)] pub extra: serde_json::Value,
    /// 断点续传开关 (默认启用, 失败/取消后保存进度, 下次启动可恢复)
    #[serde(default = "default_true")] pub resume_enabled: bool,
    /// 后台静默下载: 关闭窗口时最小化到系统托盘, 下载继续在后台运行
    #[serde(default = "default_true")] pub background_download: bool,
}

fn default_true() -> bool { true }

impl Config {
    fn path() -> PathBuf {
        dirs::home_dir()
            .map(|d| d.join(".playzip").join("config.json"))
            .unwrap_or_else(|| PathBuf::from("config.json"))
    }
    pub fn load() -> Self {
        let p = Self::path();
        let cfg = if let Ok(s) = std::fs::read_to_string(&p) {
            serde_json::from_str(&s).unwrap_or_default()
        } else { Self::default() };
        // ★ 日志增强 (2026-09-15): 记录配置加载, 方便排查配置丢失/重置问题
        crate::app_logger::log_config("LOAD", &format!(
            "path={} download_dir={} extract_dir={} resume={} bg_download={} translate_lang={}",
            p.display(), cfg.download_dir, cfg.extract_dir, cfg.resume_enabled, cfg.background_download, cfg.translate_lang
        ));
        cfg
    }
    pub fn save(&self) -> Result<(), String> {
        let p = Self::path();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let s = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&p, s).map_err(|e| e.to_string())?;
        // ★ 日志增强 (2026-09-15): 记录配置保存, 方便排查配置异常变更
        crate::app_logger::log_config("SAVE", &format!(
            "path={} download_dir={} extract_dir={} resume={} bg_download={} translate_lang={}",
            p.display(), self.download_dir, self.extract_dir, self.resume_enabled, self.background_download, self.translate_lang
        ));
        Ok(())
    }
}