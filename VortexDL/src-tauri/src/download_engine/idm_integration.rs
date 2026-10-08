pub fn is_idm_available() -> bool { false }
pub fn idm_download(_u: &str, _p: &str) -> Result<(), String> { Err("not available".into()) }