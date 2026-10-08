use std::collections::HashMap;
use std::sync::Mutex;
pub struct Cache { inner: Mutex<HashMap<String, Vec<u8>>> }
impl Cache {
    pub fn new() -> Self { Self { inner: Mutex::new(HashMap::new()) } }
    pub fn get(&self, k: &str) -> Option<Vec<u8>> {
        let result = self.inner.lock().unwrap().get(k).cloned();
        // ★ 日志增强 (2026-09-15): 记录缓存命中/未命中, 方便排查重复请求问题
        match &result {
            Some(v) => crate::app_logger::log_cache("HIT", &format!("key={} bytes={}", k, v.len())),
            None => crate::app_logger::log_cache("MISS", &format!("key={}", k)),
        }
        result
    }
    pub fn put(&self, k: String, v: Vec<u8>) {
        let bytes = v.len();
        self.inner.lock().unwrap().insert(k.clone(), v);
        crate::app_logger::log_cache("PUT", &format!("key={} bytes={}", k, bytes));
    }
    pub fn clear(&self) -> String {
        let n = self.inner.lock().map(|mut m| { let n = m.len(); m.clear(); n }).unwrap_or(0);
        crate::app_logger::log_cache("CLEAR", &format!("entries={}", n));
        format!("cleared {n} cache entries")
    }
}
impl Default for Cache { fn default() -> Self { Self::new() } }
