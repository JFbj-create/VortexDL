use std::collections::HashMap;
use parking_lot::Mutex;
pub struct BtEngine { inner: Mutex<HashMap<String, String>> }
impl BtEngine { pub fn new() -> Self { Self { inner: Mutex::new(HashMap::new()) } } }
impl Default for BtEngine { fn default() -> Self { Self::new() } }