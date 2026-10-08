use std::collections::HashMap;
use std::sync::Mutex;
use crate::search_engine::GameCard;

pub struct Snapshot {
    inner: Mutex<HashMap<String, String>>,
    adult_cards: Mutex<Vec<GameCard>>,
    adult_tags: Mutex<Vec<String>>,
}

impl Snapshot {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            adult_cards: Mutex::new(Vec::new()),
            adult_tags: Mutex::new(vec!["角色扮演".into(), "动作游戏".into(), "模拟经营".into()]),
        }
    }

    pub fn categories(&self, _src: &str) -> Vec<String> {
        vec![
            "全部类型".into(), "动作游戏".into(), "角色扮演".into(), "格斗游戏".into(),
            "射击游戏".into(), "策略游戏".into(), "模拟游戏".into(), "模拟经营".into(),
            "战争游戏".into(), "体育运动".into(), "棋牌游戏".into(), "独立游戏".into(),
            "解谜游戏".into(), "桌面游戏".into(), "竞速游戏".into(),
            "平台跳跃".into(), "生存游戏".into(), "开放世界".into(), "动漫".into(),
        ]
    }

    pub fn browse_by_category(&self, source: &str, category: &str, page: u32, _page_size: u32) -> Vec<GameCard> {
        let _ = (source, page);
        self.adult_cards.lock().map(|c| c.clone()).unwrap_or_default()
            .into_iter()
            .filter(|g| category.is_empty() || category == "全部类型" || g.category.contains(category))
            .collect()
    }

    pub fn browse_adult(&self, category: Option<&str>, page: u32) -> Vec<GameCard> {
        let _ = page;
        let cards = self.adult_cards.lock().map(|c| c.clone()).unwrap_or_default();
        match category {
            Some(c) if !c.is_empty() => cards.into_iter().filter(|g| g.category.contains(c)).collect(),
            _ => cards,
        }
    }

    pub fn search_adult(&self, query: &str, page: u32) -> Vec<GameCard> {
        let _ = page;
        let q = query.trim().to_lowercase();
        if q.is_empty() { return self.adult_cards.lock().map(|c| c.clone()).unwrap_or_default(); }
        self.adult_cards.lock().map(|c| c.clone()).unwrap_or_default()
            .into_iter()
            .filter(|g| g.name.to_lowercase().contains(&q))
            .collect()
    }

    pub fn get_adult_tags(&self) -> Vec<String> {
        self.adult_tags.lock().map(|t| t.clone()).unwrap_or_default()
    }

    pub fn clear(&self) -> String {
        let n = self.inner.lock().map(|mut m| { let n = m.len(); m.clear(); n }).unwrap_or(0);
        format!("cleared {n} snapshot entries")
    }

    pub fn reset_adult_tags(&self) -> String { "reset done".into() }
}

impl Default for Snapshot {
    fn default() -> Self { Self::new() }
}
