use parking_lot::Mutex;
use std::collections::HashMap;
pub type SharedAria2Supervisor = std::sync::Arc<std::sync::Mutex<Aria2Supervisor>>;
pub struct Aria2Supervisor { running: bool, inner: Mutex<HashMap<String, String>> }
impl Aria2Supervisor {
    pub fn new() -> Self { Self { running: false, inner: Mutex::new(HashMap::new()) } }
    pub fn is_running(&self) -> bool { self.running }
    pub fn start(&mut self) -> Result<(), String> { self.running = true; Ok(()) }
    pub fn stop(&mut self) -> Result<(), String> { self.running = false; Ok(()) }
    pub fn active(&self) -> Vec<serde_json::Value> { vec![] }
}
impl Default for Aria2Supervisor { fn default() -> Self { Self::new() } }