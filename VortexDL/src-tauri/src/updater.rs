use serde::{Deserialize, Serialize};

pub const VX_AUX_D: &str = "0000000000000000"; // 公开版占位（豪华版校验已移除）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppUpdateResult {
    pub has_update: bool,
    pub latest_version: String,
    pub current_version: String,
    pub download_url: String,
    #[serde(default)] pub release_notes: String,
}
pub async fn check_app_update() -> AppUpdateResult {
    let cur = env!("CARGO_PKG_VERSION").to_string();
    AppUpdateResult { has_update: false, latest_version: cur.clone(), current_version: cur, download_url: String::new(), release_notes: String::new() }
}
pub async fn download_and_install(_r: AppUpdateResult) -> Result<String, String> {
    Ok("no update".into())
}