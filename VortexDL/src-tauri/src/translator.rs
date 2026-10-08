use parking_lot::Mutex;

pub struct Translator { lang: Mutex<String> }

impl Translator {
    pub fn new() -> Self { Self { lang: Mutex::new("zh-CN".into()) } }
    pub async fn translate(&self, text: String) -> Result<String, String> {
        let lang = self.lang.lock().clone();
        Ok(crate::game_translator::translate_texts(&[text], &lang).await.pop().unwrap_or_default())
    }
    pub fn set_lang(&self, l: String) { *self.lang.lock() = l; }
    pub fn get_lang(&self) -> String { self.lang.lock().clone() }
    pub fn clear_cache(&self) { crate::game_translator::clear_all(); }
}
