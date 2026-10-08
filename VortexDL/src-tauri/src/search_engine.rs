use std::sync::Arc;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use crate::game_translator::translate_texts;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GameCard {
    #[serde(default)] pub name: String,
    #[serde(default)] pub name_original: String,
    #[serde(default)] pub name_cn: String,
    #[serde(default)] pub appid: String,
    #[serde(default)] pub source: String,
    #[serde(default)] pub detail_url: String,
    #[serde(default)] pub header_image: String,
    #[serde(default)] pub category: String,
    #[serde(default)] pub update_time: i64,
    #[serde(default)] pub extra: serde_json::Value,
    // 合并去重后的标签 (by 来源: 俄语类型映射; ko 来源: 分类; 成人游戏 = "成人游戏")
    #[serde(default)] pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DownloadLink {
    pub name: String,
    pub url: String,
    #[serde(default)] pub size: String,
    #[serde(default)] pub engine: String,
    #[serde(default)] pub label: String,
    #[serde(default)] pub filename: String,
    #[serde(default)] pub version: String,
    #[serde(rename = "type", default)] pub link_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GameDetail {
    pub title: String,
    #[serde(default)] pub description: String,
    #[serde(default)] pub cover: String,
    #[serde(default)] pub downloads: Vec<DownloadLink>,
    #[serde(default)] pub extra: serde_json::Value,
    #[serde(default)] pub header_image_large: String,
    #[serde(default)] pub size: String,
    #[serde(default)] pub version: String,
}

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const BYRUT_BASE: &str = "https://byrutgame.org";
const PLAYZIP_BASE: &str = "https://playzip.com";
const PLAYZIP_API: &str = "https://playzip.com/api/getGamesDownloadUrl";
const PLAYZIP_SECRET: &str = "f6i6@m29r3fwi^yqd";
const R18_MAX_PAGES: u32 = 18; // playzip R18 分类真实页数 (分页指示器 <a>1/18</a>)
// byrut 成人游戏: /for-adults/ 分类 (俄语 "Для взрослых"), 与主列表结构一致
const BYRUT_R18_BASE: &str = "https://byrutgame.org/for-adults/";
const BYRUT_R18_MAX_PAGES: u32 = 278; // byrut for-adults 总页数 (每页约 24 个)

// playzip (koyso) 15 个分类: slug → 中文标签 (r18 = 成人游戏)
// 主列表不带分类信息, 后台爬分类页为所有 ko 游戏打上分类标签
fn map_koyso_category(slug: &str) -> Option<&'static str> {
    let m: &[(&str, &str)] = &[
        ("action", "动作游戏"), ("adventure", "冒险游戏"), ("card", "卡牌游戏"),
        ("casual", "休闲游戏"), ("fighting", "格斗游戏"), ("horror", "恐怖游戏"),
        ("indie", "独立游戏"), ("lan", "联机游戏"), ("r18", "成人游戏"),
        ("rpg", "角色扮演"), ("rts", "即时战略"), ("shooting", "射击游戏"),
        ("simulation", "模拟游戏"), ("sports_racing", "体育竞速"), ("strategy", "策略游戏"),
    ];
    m.iter().find(|(k, _)| *k == slug).map(|(_, zh)| *zh)
}

pub fn build_client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    let ins = |m: &mut reqwest::header::HeaderMap, k: &str, v: &str| {
        if let (Ok(name), Ok(val)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) {
            m.insert(name, val);
        }
    };
    ins(&mut headers, "Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8");
    ins(&mut headers, "Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8");
    ins(&mut headers, "Cache-Control", "no-cache");
    ins(&mut headers, "Pragma", "no-cache");
    ins(&mut headers, "Sec-Fetch-Dest", "document");
    ins(&mut headers, "Sec-Fetch-Mode", "navigate");
    ins(&mut headers, "Sec-Fetch-Site", "none");
    ins(&mut headers, "Upgrade-Insecure-Requests", "1");
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .default_headers(headers)
        .gzip(true)
        .brotli(true)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(8)
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default()
}

fn now_secs() -> i64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) }
fn now_millis() -> u128 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0) }

// 伪随机数 (无需 rand crate)
fn pseudo_rand(n: u32) -> u32 {
    if n == 0 { return 0; }
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    nanos.wrapping_mul(1664525).wrapping_add(1013904223) % n
}

fn sha256_hex(input: &str) -> String {
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    h.finalize().iter().map(|b| format!("{:02x}", b)).collect()
}

fn html_unescape(s: &str) -> String {
    s.replace("&#43;", "+")
     .replace("&amp;", "&")
     .replace("&lt;", "<")
     .replace("&gt;", ">")
     .replace("&quot;", "\"")
     .replace("&#039;", "'")
     .replace("&#034;", "\"")
     .replace("&#39;", "'")
     .replace("&nbsp;", " ")
}

/// HTML 转义 (纯文本放入 <p> 标签前)
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
     .replace('<', "&lt;")
     .replace('>', "&gt;")
}

// byrut 俄文分类关键词 → 中文分类映射
fn map_byrut_category(ru: &str) -> String {
    let lower = ru.to_lowercase();
    let mapping: &[(&str, &str)] = &[
        ("шутер", "射击游戏"),
        ("от первого лица", "射击游戏"),
        ("экшен", "动作游戏"),
        ("экшены", "动作游戏"),
        ("боевик", "动作游戏"),
        ("файтинг", "格斗游戏"),
        ("ролев", "角色扮演"),
        ("rpg", "角色扮演"),
        ("стратег", "策略游戏"),
        ("симуля", "模拟游戏"),
        ("симулятор", "模拟游戏"),
        ("выживан", "生存游戏"),
        ("открытый мир", "开放世界"),
        ("гонк", "竞速游戏"),
        ("спорт", "体育运动"),
        ("хоррор", "动作游戏"),
        ("ужас", "动作游戏"),
        ("платформ", "平台跳跃"),
        ("головолом", "解谜游戏"),
        ("пазл", "解谜游戏"),
        ("инди", "独立游戏"),
        ("крафт", "模拟经营"),
        ("аниме", "动漫"),
        ("военн", "战争游戏"),
        ("войн", "战争游戏"),
        ("карточн", "桌面游戏"),
        ("настольн", "桌面游戏"),
        ("приключен", "动作游戏"),
    ];
    for (kw, zh) in mapping {
        if lower.contains(kw) { return zh.to_string(); }
    }
    "动作游戏".to_string()
}

/// byrut 俄语类型 → 中文标签 (完整映射, 用于给所有 by 游戏打标签)
/// 与 map_byrut_category 不同: 这里做完整词条匹配而非关键词包含
fn map_byrut_tag(ru: &str) -> Option<&'static str> {
    let t = ru.trim();
    let m: &[(&str, &str)] = &[
        ("2D-Платформер", "2D平台"), ("3D-Платформер", "3D平台"), ("90-е", "90年代"),
        ("Beat 'em up", "清版动作"), ("Idle-игра", "放置游戏"), ("Point and click", "点击冒险"),
        ("RPG", "角色扮演"), ("Shoot 'em up", "弹幕射击"), ("Tower Defense", "塔防"),
        ("Автосимулятор", "驾驶模拟"), ("Аниме", "动漫"), ("Антиутопия", "反乌托邦"),
        ("Аркады", "街机"), ("Атмосферная", "氛围感"), ("Бездорожье", "越野"),
        ("Боевые гонки", "战斗竞速"), ("Боевые искусства", "武术格斗"), ("Бой", "战斗"),
        ("Вампиры", "吸血鬼"), ("Вестерн", "西部"), ("Вид сбоку", "横版视角"),
        ("Вид сверху", "俯视角"), ("Визуальная новелла", "视觉小说"),
        ("Военные действия", "军事行动"), ("Военные конфликты", "军事冲突"),
        ("Вождение", "驾驶"), ("Война", "战争游戏"), ("Выживание", "生存游戏"),
        ("Глобальные стратегии", "全球战略"), ("Глубокий сюжет", "深度剧情"),
        ("Головоломка-платформер", "平台解谜"), ("Головоломка", "解谜游戏"),
        ("Гонки", "竞速游戏"), ("Горное дело", "采矿"), ("Градостроение", "城市建设"),
        ("Демоны", "恶魔"), ("Детектив", "侦探"), ("Динозавры", "恐龙"),
        ("Для всей семьи", "全家同乐"), ("Драконы", "巨龙"), ("Зомби", "僵尸"),
        ("Зрелищные сражения", "华丽战斗"), ("Игрок против ИИ", "人机对战"),
        ("Игры в 2D", "2D游戏"), ("Изометрия", "等距视角"), ("Иммерсивный симулятор", "沉浸模拟"),
        ("Инди", "独立游戏"), ("Исследования", "探索"), ("Историческая", "历史题材"),
        ("Казуальная", "休闲游戏"), ("Капитализм", "资本主义"), ("Карточная игра", "卡牌游戏"),
        ("Кастомизация оружия", "武器定制"), ("Кастомизация персонажа", "角色定制"),
        ("Киберпанк", "赛博朋克"), ("Кинематографичная", "电影化叙事"), ("Кликер", "点击游戏"),
        ("Коллектатон", "收集要素"), ("Космос", "太空"), ("Котики", "猫咪"),
        ("Крафтинг", "合成制作"), ("Криминал", "犯罪"), ("Кровь", "血腥"), ("Кулинария", "烹饪"),
        ("Логика", "逻辑推理"), ("Лут", "装备拾取"), ("Лутер-шутер", "刷宝射击"),
        ("Магия", "魔法"), ("Менеджмент инвентаря", "背包管理"), ("Менеджмент", "经营管理"),
        ("Метроидвания", "银河恶魔城"), ("Милая", "治愈可爱"), ("Мифология", "神话"),
        ("Морской бой", "海战"), ("Музыка", "音乐"), ("Мультипликация", "动画风格"),
        ("Мультфильмы", "动画改编"), ("Нагота", "裸露内容"), ("Насилие", "暴力"),
        ("Настольная игра", "桌面游戏"), ("Научная фантастика", "科幻"), ("Несколько концовок", "多结局"),
        ("Ниндзя", "忍者"), ("Одна жизнь", "单命模式"), ("От первого лица", "第一人称"),
        ("От третьего лица", "第三人称"), ("Открытый мир", "开放世界"), ("Отличный саундтрек", "优质配乐"),
        ("Охота", "狩猎"), ("Паркур", "跑酷"), ("Партийная RPG", "组队RPG"),
        ("Перемещение по сетке", "网格移动"), ("Песочница", "沙盒"), ("Пиксельная графика", "像素画面"),
        ("Пираты", "海盗"), ("Платформеры", "平台跳跃"), ("По комиксу", "漫画改编"),
        ("Повествовательная", "叙事"), ("Подводный мир", "海底世界"), ("Подземелья", "地下城"),
        ("Поиск предметов", "物品查找"), ("Полёты", "飞行"), ("Постапокалипсис", "末日废土"),
        ("Построение колоды", "卡组构筑"), ("Похожа на Dark Souls", "类魂"),
        ("Пошаговая тактика", "回合战术"), ("Пошаговые сражения", "回合战斗"),
        ("Пошаговые стратегии", "回合策略"), ("Пошаговая", "回合制"),
        ("Приключенческий экшен", "动作冒险"), ("Приключения", "冒险游戏"),
        ("Природа", "自然"), ("Протагонистка", "女主角"), ("Процедурная генерация", "程序生成"),
        ("Психологический хоррор", "心理恐怖"), ("Разделение экрана", "分屏合作"),
        ("Разрушения", "破坏"), ("Расслабляющая", "放松休闲"), ("Реализм", "写实"),
        ("Реиграбельность", "重玩价值"), ("Ретро", "复古"), ("Решения с последствиями", "抉择后果"),
        ("Рисованная графика", "手绘画面"), ("Ритм-игра", "节奏游戏"), ("Роботы", "机器人"),
        ("Рогалик", "Roguelike"), ("Ролевой экшен", "动作RPG"), ("Ролевые стратегии", "角色策略"),
        ("Романтика", "浪漫恋爱"), ("Рыбалка", "钓鱼"), ("Сверхъестественное", "超自然"),
        ("Симулятор жизни", "生活模拟"), ("Симулятор колонии", "殖民地模拟"),
        ("Симулятор фермы", "农场模拟"), ("Симулятор ходьбы", "步行模拟"), ("Симуляторы", "模拟游戏"),
        ("Сложная", "高难度"), ("Слэшер", "砍杀"), ("Смешная", "搞笑"), ("Спортивные", "体育运动"),
        ("Сражения на мечах", "剑术对决"), ("Средневековье", "中世纪"), ("Стелс", "潜行"),
        ("Стилизация", "风格化"), ("Стратегии в реальном времени", "即时战略"), ("Стратегии", "策略游戏"),
        ("Строительство базы", "基地建设"), ("Строительство", "建造"), ("Супергерои", "超级英雄"),
        ("Сюрреалистичная", "超现实"), ("Тайм-менеджмент", "时间管理"), ("Тайна", "悬疑"),
        ("Тактика в реальном времени", "即时战术"), ("Тактическая RPG", "战术RPG"), ("Тактика", "战术"),
        ("Тёмное фэнтези", "黑暗奇幻"), ("Традиционный рогалик", "传统Roguelike"),
        ("Транспорт", "交通运输"), ("Управление ресурсами", "资源管理"),
        ("Упрощённый рогалик", "轻量Roguelike"), ("Файтинги", "格斗游戏"), ("Физика", "物理引擎"),
        ("Флот", "舰队"), ("Фэнтези", "奇幻"), ("Хоррор на выживание", "生存恐怖"),
        ("Хорроры", "恐怖游戏"), ("Шутер от первого лица", "第一人称射击"),
        ("Шутер от третьего лица", "第三人称射击"), ("Шутер с видом сверху", "俯视角射击"),
        ("Шутеры", "射击游戏"), ("Экономика", "经济"), ("Экшены", "动作游戏"),
        ("Эмоциональная", "情感向"), ("Юмор", "幽默"), ("Японская RPG", "日式RPG"),
    ];
    for (k, zh) in m {
        if t.eq_ignore_ascii_case(k) || t == *k { return Some(zh); }
    }
    None
}

/// 检测字符串是否包含中文字符 (CJK 统一表意文字)
fn contains_cjk(s: &str) -> bool {
    s.chars().any(|c| matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}'))
}

/// 从 byrut 详情页 URL 提取英文名 (slug)
/// 例: https://byrutgame.org/42635-onimusha-way-of-the-sword.html → "Onimusha Way Of The Sword"
fn slug_to_english_name(detail_url: &str, appid: &str) -> String {
    // 取最后一段路径 (文件名)
    let fname = detail_url.rsplit('/').next().unwrap_or("");
    // 去掉 .html 后缀
    let stem = fname.strip_suffix(".html").unwrap_or(fname);
    // 去掉前导 "{appid}-" 前缀
    let slug = if !appid.is_empty() {
        let prefix = format!("{}-", appid);
        stem.strip_prefix(&prefix).unwrap_or(stem).to_string()
    } else {
        // 没有 appid 时, 去掉第一个数字段前缀 (如 "42635-onimusha-...")
        let re = regex::Regex::new(r"^\d+-").unwrap();
        re.replace(stem, "").to_string()
    };
    if slug.is_empty() { return String::new(); }
    // 连字符转空格 + 每个单词首字母大写
    slug.split('-')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 三段推送的配额切片：全局每 100 条里该段占 `pct` 条，返回这一页该取的
/// `(段内起始偏移, 条数)`。分段在页面边界上不能重叠也不能漏 —— 见单测。
fn adult_section_slice(start: usize, end: usize, pct: usize) -> (usize, usize) {
    let from = start * pct / 100;
    (from, end * pct / 100 - from)
}

/// ★ 生成成人列表的第 `page_index` 页（0 基）。
///
/// 用户要的是**每一页**从上到下分三段：KO 40% / GX 55% / BY 5%
/// （40 条一页 = 16 / 22 / 2 条），不是"每 100 条混一次"。
///
/// 难点在池子耗尽时不能缩水、翻页又不能重复/漏条。做法：
/// 维护三个**顺序游标**，从第 0 页开始逐页重放（页数不多，40 条/页 × 几百页也就一万多次循环）：
///   1. 每页先按配额各取（16/22/2），取不满的记下缺口；
///   2. 缺口按 KO → GX → BY 的顺序，从**还有货**的池继续取（顺延补齐）；
///   3. 游标只前进不回头 → 天然无重复、无遗漏，页满到三个池全空为止。
///
/// 旧实现按"每页各自算偏移"切片：KO 池一耗尽（装机版实测只有 150 条），
/// 那一页最上面 40% 就整段空掉 —— 40 条的页缩成 24 条，
/// 用户看到的就是「成人游戏数量明显不对」。
fn adult_plan_page(
    pools: [&[GameCard]; 3],
    page_index: usize,
    pcts: [usize; 3],
    page_size: usize,
) -> Vec<GameCard> {
    // 每页各段的配额；余数补给前几段，保证配额之和 == page_size
    let mut quota = [0usize; 3];
    let mut assigned = 0usize;
    for i in 0..3 {
        quota[i] = page_size * pcts[i] / 100;
        assigned += quota[i];
    }
    let mut rem = page_size.saturating_sub(assigned);
    for i in 0..3 {
        if rem == 0 {
            break;
        }
        quota[i] += 1;
        rem -= 1;
    }

    let mut cursor = [0usize; 3];
    let mut page: Vec<GameCard> = Vec::with_capacity(page_size);
    for p in 0..=page_index {
        let mut taken = [0usize; 3];
        for i in 0..3 {
            taken[i] = quota[i].min(pools[i].len().saturating_sub(cursor[i]));
        }
        let mut short = page_size.saturating_sub(taken.iter().sum::<usize>());
        for i in 0..3 {
            if short == 0 {
                break;
            }
            let avail = pools[i].len().saturating_sub(cursor[i] + taken[i]);
            let t = short.min(avail);
            taken[i] += t;
            short -= t;
        }
        if p == page_index {
            for i in 0..3 {
                page.extend(pools[i][cursor[i]..cursor[i] + taken[i]].iter().cloned());
            }
        }
        for i in 0..3 {
            cursor[i] += taken[i];
        }
        if taken.iter().sum::<usize>() == 0 {
            break;   // 三个池都空了
        }
    }
    page
}

/// ★ parse_byrut_list: 增加 page 参数 (2026-09-13)
///   用途: 构造固定 update_time (替代 now()), 保证排序稳定 + ko 优先于 by
fn parse_byrut_list(html: &str, source: &str, page: u32) -> Vec<GameCard> {
    let mut cards = Vec::new();
    // ★ class 后面可能还跟着别的类名：成人区是 `<article class="short_item is-adult">`。
    //   早先写死 `class="short_item">`，导致 /for-adults/ 整页解析为 0 条 —— BY 池恒为空。
    let article_re = regex::Regex::new(r#"(?s)<article class="short_item[^"]*">.*?</article>"#).unwrap();
    let appid_re = regex::Regex::new(r#"data-appid="(\d+)""#).unwrap();
    // 标题: 标准布局 game-preview__title / 变体布局 h2.short_title > a (for-adults 区大量使用)
    let title_re = regex::Regex::new(r#"game-preview__title">([^<]+)<"#).unwrap();
    let title2_re = regex::Regex::new(r#"<h2 class="short_title">\s*<a[^>]*>([^<]+)</a>"#).unwrap();
    let img_alt_re = regex::Regex::new(r#"<img[^>]*alt="([^"]+)""#).unwrap();
    let genre_re = regex::Regex::new(r#"game-preview__genres">([^<]+)<"#).unwrap();
    let href_re = regex::Regex::new(r#"<a href="([^"]+)"[^>]*>\s*<img"#).unwrap();
    let img_re = regex::Regex::new(r#"<img[^>]*src="([^"]+)""#).unwrap();
    let img_datasrc_re = regex::Regex::new(r#"<img[^>]*data-src="([^"]+)""#).unwrap();
    // 变体布局无 data-appid, 从 URL 提取文章 ID (/57183-skimmed.html → 57183)
    let url_id_re = regex::Regex::new(r#"/(\d+)-[^/"']*?\.html"#).unwrap();

    // 按 appid/detail_url 去重 (页面轮播区与列表区会重复展示同一游戏)
    let mut seen = std::collections::HashSet::new();
    for (i, art) in article_re.find_iter(html).enumerate() {
        let block = art.as_str();
        // 标题解析: 标准 → 变体 h2 → img alt 兜底 (反转义 HTML 实体)
        let name = title_re.captures(block).map(|c| html_unescape(c[1].trim()))
            .filter(|s| !s.is_empty())
            .or_else(|| title2_re.captures(block).map(|c| html_unescape(c[1].trim())))
            .filter(|s| !s.is_empty())
            .or_else(|| img_alt_re.captures(block).map(|c| html_unescape(c[1].trim())))
            .filter(|s| !s.is_empty())
            .unwrap_or_default();
        if name.is_empty() { continue; }
        // appid: 优先 data-appid, 变体布局从详情 URL 提取
        let appid = appid_re.captures(block).map(|c| c[1].to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| href_re.captures(block)
                .and_then(|c| url_id_re.captures(&c[1]).map(|m| m[1].to_string())))
            .unwrap_or_default();
        let raw_genre = genre_re.captures(block).map(|c| c[1].trim().to_string()).unwrap_or_default();
        let category = map_byrut_category(&raw_genre);
        // 标签: 原始类型串按 "·" 分割, 逐条映射为中文并去重
        let mut tags: Vec<String> = Vec::new();
        for g in html_unescape(&raw_genre).split('·') {
            let g = g.trim();
            if g.is_empty() { continue; }
            if let Some(zh) = map_byrut_tag(g) {
                if !tags.iter().any(|t| t == zh) { tags.push(zh.to_string()); }
            }
        }
        let detail_url = href_re.captures(block).map(|c| c[1].to_string()).unwrap_or_default();
        // 封面图: src 为懒加载占位符 (data:image/svg...) 时回退 data-src
        let header_image = match img_re.captures(block).map(|c| c[1].to_string()) {
            Some(s) if !s.is_empty() && !s.starts_with("data:") => s,
            _ => img_datasrc_re.captures(block).map(|c| c[1].to_string()).unwrap_or_default(),
        };
        // 英文名: 从 URL slug 提取, 支持用户用英文名搜索
        let name_original = slug_to_english_name(&detail_url, &appid);
        let key = if !appid.is_empty() { format!("a:{}", appid) } else { format!("u:{}", detail_url) };
        if !seen.insert(key) { continue; }
        cards.push(GameCard {
            name,
            name_original,
            appid,
            source: source.to_string(),
            detail_url,
            header_image,
            category,
            // ★ Bug 修复 (2026-09-13): update_time 改为固定基准时间 (替代 now()),
            //   保证排序稳定; byrut 基准 1_000_000_000 < koyso 基准 2_000_000_000 → ko 优先
            //   同来源内按 page*10000+i 递减, 等效按网站更新顺序排列
            update_time: 1_000_000_000i64 - (page as i64) * 10_000 - i as i64,
            extra: serde_json::Value::Null,
            tags,
            ..Default::default()
        });
    }
    cards
}

// WordPress 分页导航: 提取最大页码 (如 /page/1650/)
fn extract_max_page_wp(html: &str) -> u32 {
    let re = regex::Regex::new(r#"page/(\d+)/?"#).unwrap();
    re.captures_iter(html)
        .filter_map(|c| c[1].parse::<u32>().ok())
        .max()
        .unwrap_or(0)
}

// playzip 分页导航: 提取最大页码
// 两种来源:
//   1. 链接 ?page=N (页面只显示前后 3 页, 严重低估总量)
//   2. 分页指示器 <a>10/92</a> (真实总页数) ← 必须解析, 否则 2760 个游戏只缓存 90 个
fn extract_max_page_pz(html: &str) -> u32 {
    let mut max = 0u32;
    let re = regex::Regex::new(r#"[?&]page=(\d+)"#).unwrap();
    for c in re.captures_iter(html) {
        if let Ok(n) = c[1].parse::<u32>() { if n > max { max = n; } }
    }
    let ind_re = regex::Regex::new(r#"<a>\d+/(\d+)</a>"#).unwrap();
    for c in ind_re.captures_iter(html) {
        if let Ok(n) = c[1].parse::<u32>() { if n > max { max = n; } }
    }
    max
}

// playzip (koyso) 游戏列表解析: <a class="game_item" href="/game/N"> ... data-src="img" alt="title"
fn parse_playzip_list(html: &str, source: &str, page: u32) -> Vec<GameCard> {
    let item_re = regex::Regex::new(r#"(?s)<a class="game_item"[^>]*href="/game/(\d+)">(.*?)</a>"#).unwrap();
    let img_re = regex::Regex::new(r#"data-src="([^"]+)""#).unwrap();
    let alt_re = regex::Regex::new(r#"alt="([^"]*)""#).unwrap();
    let mut cards = Vec::new();
    for (i, cap) in item_re.captures_iter(html).enumerate() {
        let id = cap[1].to_string();
        let block = &cap[2];
        let name = html_unescape(&alt_re.captures(block).map(|c| c[1].trim().to_string()).unwrap_or_default());
        if name.is_empty() { continue; }
        let header_image = img_re.captures(block).map(|c| c[1].to_string()).unwrap_or_default();
        // ★ Bug 修复 (2026-09-13): ko 基准 2_000_000_000 > by 基准 1_000_000_000
        //   → browse("all") 合并排序时 ko 排在 by 前面 (用户要求: 优先推 ko 资源)
        //   同来源内按 page*10000+i 递减, 等效按网站更新顺序排列
        let update_time = 2_000_000_000i64 - (page as i64) * 10_000 - i as i64;
        cards.push(GameCard {
            name,
            appid: id.clone(),
            source: source.to_string(),
            detail_url: format!("{PLAYZIP_BASE}/game/{id}"),
            header_image,
            update_time,
            ..Default::default()
        });
    }
    cards
}

/// 提取指定 class 的 div 块的内部 HTML (处理嵌套 div, 通过深度计数找到匹配的闭合标签)
/// 全程使用字节操作, 避免 UTF-8 多字节字符边界 panic
fn extract_div_block(html: &str, class_name: &str) -> String {
    let needle = format!("class=\"{}\"", class_name);
    let start_tag = match html.find(&needle) {
        Some(pos) => {
            let before = &html[..pos];
            match before.rfind("<div") {
                Some(dpos) => dpos,
                None => return String::new(),
            }
        }
        None => return String::new(),
    };
    let bytes = html.as_bytes();
    // 从 <div 开始找 '>' (开始标签结束)
    let mut tag_end = start_tag;
    while tag_end < bytes.len() && bytes[tag_end] != b'>' {
        tag_end += 1;
    }
    if tag_end >= bytes.len() { return String::new(); }
    tag_end += 1; // 跳过 '>'
    // 深度计数扫描, 找到匹配的闭合 </div>
    let mut depth = 1i32;
    let mut i = tag_end;
    while i < bytes.len() && depth > 0 {
        if bytes[i] == b'<' {
            if i + 4 < bytes.len() && &bytes[i..i+4] == b"<div" {
                depth += 1;
                i += 4;
            } else if i + 6 <= bytes.len() && &bytes[i..i+6] == b"</div>" {
                depth -= 1;
                if depth == 0 {
                    let s = std::str::from_utf8(&bytes[tag_end..i]).unwrap_or("");
                    return s.trim().to_string();
                }
                i += 6;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    let s = std::str::from_utf8(&bytes[tag_end..]).unwrap_or("");
    s.trim().to_string()
}

/// 从 HTML 片段中提取所有图片 URL (img src / img data-src / video poster)
fn extract_image_urls(html: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let img_re = regex::Regex::new(r#"<img[^>]*\ssrc="([^"]+)""#).unwrap();
    for c in img_re.captures_iter(html) {
        let u = c[1].to_string();
        if !u.is_empty() && !u.starts_with("data:") && !urls.contains(&u) { urls.push(u); }
    }
    // 兜底: data-src (懒加载)
    let data_src_re = regex::Regex::new(r#"<img[^>]*\sdata-src="([^"]+)""#).unwrap();
    for c in data_src_re.captures_iter(html) {
        let u = c[1].to_string();
        if !u.is_empty() && !u.starts_with("data:") && !urls.contains(&u) { urls.push(u); }
    }
    // video poster (作为预览图)
    let poster_re = regex::Regex::new(r#"<video[^>]*\sposter="([^"]+)""#).unwrap();
    for c in poster_re.captures_iter(html) {
        let u = c[1].to_string();
        if !u.is_empty() && !u.starts_with("data:") && !urls.contains(&u) { urls.push(u); }
    }
    urls
}

// playzip 详情页解析
fn parse_playzip_detail(html: &str) -> GameDetail {
    let title_re = regex::Regex::new(r#"(?s)<h1 class="content_title">\s*(.*?)\s*</h1>"#).unwrap();
    let cover_re = regex::Regex::new(r#"(?s)<div class="capsule_div">\s*<img src="([^"]+)""#).unwrap();
    let size_re = regex::Regex::new(r#"(?s)<li>\s*<span>游戏大小</span>\s*<span[^>]*>([^<]+)</span>"#).unwrap();
    let ver_re = regex::Regex::new(r#"(?s)<li>\s*<span>游戏版本</span>\s*<span[^>]*>([^<]+)</span>"#).unwrap();

    let mut title = title_re.captures(html).map(|c| c[1].trim().to_string()).unwrap_or_default();
    // 去掉标题尾部 " 下载"
    if let Some(stripped) = title.strip_suffix("下载") { title = stripped.trim().to_string(); }
    let cover = cover_re.captures(html).map(|c| c[1].to_string()).unwrap_or_default();
    // 用深度感知提取 content_body, 避免嵌套 div 时非贪婪正则截断
    let desc = extract_div_block(html, "content_body");
    let size = size_re.captures(html).map(|c| c[1].trim().to_string()).unwrap_or_default();
    let version = ver_re.captures(html).map(|c| c[1].trim().to_string()).unwrap_or_default();

    // 提取简介中的截图/图片 URL, 存入 extra.images 供前端画廊展示
    let images = extract_image_urls(&desc);
    let extra = if images.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!({ "images": images })
    };

    GameDetail {
        title,
        description: desc,
        cover: cover.clone(),
        header_image_large: cover,
        size,
        version,
        downloads: vec![],
        extra,
    }
}

struct AdultState {
    pages: HashMap<u32, Vec<GameCard>>,
    byrut_pages: HashMap<u32, Vec<GameCard>>,
    keyword_cache: HashMap<String, (Vec<GameCard>, u128)>,
    next_page: u32,
    byrut_next_page: u32,
    last_call_ms: u128,
}

// ============ 游戏索引磁盘持久化 ============
// exe 同目录 game_index.json: 启动瞬间从磁盘恢复全部游戏卡缓存, 数量立即显示
// 后续预热只做增量: 爬前 1-2 页对比 appid, 新游戏合并进缓存, 不再全量重爬
// version: 1 = 无标签旧格式; 2 = 带标签 (GameCard.tags); 3 = 成人/分类标签爬取完成标记
//   (adult_tagged / ko_tagged: 启动后按标记决定是否需要后台补爬标签, 避免每次全量重爬)
#[derive(Serialize, Deserialize, Default)]
struct DiskIndex {
    #[serde(default)] version: u32,
    #[serde(default)] byrut_max_page: u32,
    #[serde(default)] koyso_max_page: u32,
    #[serde(default)] adult_tagged: bool,
    #[serde(default)] ko_tagged: bool,
    // byrut 成人爬取断点: 下次启动从该页续爬 (0 = 未开始/已完成, 从第 1 页开始)
    // 站点偶发 TLS 重置会中断整轮爬取, 无断点时每次重启都从第 1 页重爬,
    // 若中断点稳定靠前则后半部分永远爬不到; 断点保证确定性收敛
    #[serde(default)] adult_next_page: u32,
    #[serde(default)] byrut: HashMap<u32, Vec<GameCard>>,
    #[serde(default)] koyso: HashMap<u32, Vec<GameCard>>,
}

// v3: 成人/分类标签爬取; v4: 标签爬取增加限流容错 (v3 索引的 for-adults 标签
//     因限流提前中断仍被标记完成, 升版本强制用修复后的逻辑重打标签)
const INDEX_VERSION: u32 = 4;
// 未缓存游戏 (如仅出现在 for-adults / r18 分类页) 存放的合成页码, 正常浏览不会到达, 但搜索/统计会包含
const SYNTHETIC_PAGE: u32 = 9000;

/// 增量合并: 最新一页列表 vs 缓存 — 新 appid 插到第1页头部;
/// 第1页超过 cap 时溢出到第2页头部 (不删除任何游戏, 总数只增不减)。
/// 返回 (新游戏数, 是否有任何变化)。
fn merge_fresh_into_cache(
    cache: &mut HashMap<u32, Vec<GameCard>>,
    fresh: &[GameCard],
    cap: usize,
) -> (usize, bool) {
    let existing: std::collections::HashSet<String> =
        cache.values().flat_map(|v| v.iter().map(|g| g.appid.clone())).collect();
    let mut new_count = 0usize;
    let mut changed = false;
    {
        let entry = cache.entry(1).or_default();
        for g in fresh.iter().rev() {
            if !existing.contains(&g.appid) {
                entry.insert(0, g.clone()); // 新游戏插到最前 (浏览时最先看到)
                new_count += 1;
                changed = true;
            }
        }
    }
    let overflow = match cache.get_mut(&1) {
        Some(e) if e.len() > cap => e.split_off(cap),
        _ => Vec::new(),
    };
    if !overflow.is_empty() {
        changed = true;
        let p2 = cache.entry(2).or_default();
        let p2_ids: std::collections::HashSet<String> =
            p2.iter().map(|g| g.appid.clone()).collect();
        for g in overflow.into_iter().rev() {
            if !p2_ids.contains(&g.appid) {
                p2.insert(0, g); // 从第1页掉落的游戏比第2页原有内容新, 插头部
            }
        }
    }
    (new_count, changed)
}

fn index_file() -> PathBuf {
    std::env::current_exe().ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("game_index.json")
}

/// 页面插入缓存: 正常页码的真实数据优先于 SYNTHETIC_PAGE 合成数据
/// 插入前把该页已包含的 appid 从 SYNTHETIC_PAGE 移除, 避免同一游戏出现两份
/// ★ 真实页覆盖合成页时, 合成页积累的标签 (如 "成人游戏") 会并入真实页卡片,
///   防止 "标签爬取先完成、真实页后爬到" 的竞态导致标签丢失
fn insert_page_dedup(cache: &mut HashMap<u32, Vec<GameCard>>, page: u32, mut cards: Vec<GameCard>) {
    if page == SYNTHETIC_PAGE {
        // 合成页本身也去重 (同一游戏出现在多个分类页时只保留一份)
        if let Some(v) = cache.get_mut(&SYNTHETIC_PAGE) {
            let have: std::collections::HashSet<String> = v.iter().map(|g| g.appid.clone()).collect();
            for c in cards {
                if !have.contains(&c.appid) { v.push(c); }
            }
        } else {
            cache.insert(SYNTHETIC_PAGE, cards);
        }
        return;
    }
    // 从合成页合并标签到本页卡片 (真实页优先), 再从合成页移除
    if let Some(v) = cache.get_mut(&SYNTHETIC_PAGE) {
        for c in cards.iter_mut() {
            if let Some(syn) = v.iter().find(|g| g.appid == c.appid) {
                for t in syn.tags.iter() {
                    if !c.tags.contains(t) { c.tags.push(t.clone()); }
                }
            }
        }
        let ids: std::collections::HashSet<String> = cards.iter().map(|g| g.appid.clone()).collect();
        v.retain(|g| !ids.contains(&g.appid));
        if v.is_empty() { cache.remove(&SYNTHETIC_PAGE); }
    }
    cache.insert(page, cards);
}

/// 合并标签页爬到的卡片 (for-adults / 分类页):
/// 统计缓存中带 "成人游戏" 标签的卡片数 (诊断用)
fn count_adult_cards(cache: &HashMap<u32, Vec<GameCard>>) -> usize {
    cache
        .values()
        .flat_map(|v| v.iter())
        .filter(|g| g.tags.iter().any(|t| t == "成人游戏"))
        .count()
}

/// - 已在缓存 (按 appid): 标签取并集, 空字段补全 (category/封面/详情链接)
/// - 不在缓存: 加入 SYNTHETIC_PAGE (搜索/统计可见, 正常浏览不到)
/// 返回合并的标签总数
fn merge_tagged_into_cache(cache: &mut HashMap<u32, Vec<GameCard>>, tagged: Vec<GameCard>) -> usize {
    let mut merged = 0usize;
    for mut c in tagged {
        if c.appid.is_empty() { continue; }
        let mut found = false;
        for page in cache.values_mut() {
            for g in page.iter_mut() {
                if g.appid == c.appid {
                    // 标签并集 (保序: 已有在前, 新增在后)
                    for t in c.tags.iter() {
                        if !g.tags.contains(t) { g.tags.push(t.clone()); merged += 1; }
                    }
                    // 补全空字段
                    if g.category.is_empty() && !c.category.is_empty() { g.category = c.category.clone(); }
                    if g.header_image.is_empty() && !c.header_image.is_empty() { g.header_image = c.header_image.clone(); }
                    if g.detail_url.is_empty() && !c.detail_url.is_empty() { g.detail_url = c.detail_url.clone(); }
                    if g.name_original.is_empty() && !c.name_original.is_empty() { g.name_original = c.name_original.clone(); }
                    found = true;
                }
            }
        }
        if !found {
            // 未缓存游戏 → 合成页
            let entry = cache.entry(SYNTHETIC_PAGE).or_default();
            if !entry.iter().any(|g| g.appid == c.appid) {
                entry.push(c);
            }
        }
    }
    merged
}

fn load_disk_index() -> DiskIndex {
    std::fs::File::open(index_file()).ok()
        .and_then(|f| serde_json::from_reader(f).ok())
        .unwrap_or_default()
}

pub struct SearchEngine {
    pub cache: Arc<crate::cache::Cache>,
    pub snapshot: Arc<crate::snapshot::Snapshot>,
    // ★ 性能: 复用同一个 reqwest 客户端 (内含连接池 + TLS 会话),
    //   避免每次抓页都 build_client() 重新建池 → 复用 TCP/TLS 连接大幅降低翻页/首屏延迟
    http: reqwest::Client,
    byrut_cache: Mutex<HashMap<u32, Vec<GameCard>>>,
    koyso_cache: Mutex<HashMap<u32, Vec<GameCard>>>,
    adult: Mutex<AdultState>,
    // playzip 下载直链缓存: game_id -> (url, expire_ms)
    pz_dl_cache: Mutex<HashMap<String, (String, u128)>>,
    // 站点资源总数 (byrut_est, koyso_est): 从分页导航解析出的 最大页码 × 每页数量
    site_totals: Mutex<(u64, u64)>,
    // 站点最大页码 (byrut_max, koyso_max): 全量预热的翻页上界 (防止 est 推算偏大)
    site_max_pages: Mutex<(u32, u32)>,
    // 上次索引写盘时间 (节流: 浏览发现的页也要持久化, 但限制写盘频率)
    last_disk_save: Mutex<u128>,
    // 后台全量预热进行中标记 (防重复预热)
    preloading: std::sync::atomic::AtomicBool,
    // 索引写盘串行锁: retag / batch_translate / 前端翻译按钮可能并发调用
    // save_index_to_disk, 无锁时两个线程同时写同一个 .tmp 会交叉截断 → JSON 损坏
    // → 下次启动 load_disk_index 静默失败 → 全量重爬 + 标记丢失
    index_save_lock: Mutex<()>,
    // preload_byko 后台任务已启动标记 (前端 STEP14.5/STEP18 重复调用时只 spawn 一次,
    // 避免第二个 spawn 跳过预热直接 retag 与全量爬取并发竞争)
    preload_spawned: std::sync::atomic::AtomicBool,
    // 成人游戏标签爬取完成标记 (byrut for-adults 全量爬完)
    adult_tagged: std::sync::atomic::AtomicBool,
    // byrut 成人爬取断点 (adult_next_page): 中断后续爬起点, 见 DiskIndex 注释
    adult_next_page: std::sync::atomic::AtomicU32,
    // ko 分类标签爬取完成标记 (playzip 15 个分类全量爬完)
    ko_tagged: std::sync::atomic::AtomicBool,
    // 磁盘索引后台加载完成标记 (true = game_index.json 已加载到缓存)
    disk_index_loaded: std::sync::atomic::AtomicBool,
}

impl SearchEngine {
    pub fn new(cache: Arc<crate::cache::Cache>, snapshot: Arc<crate::snapshot::Snapshot>) -> Self {
        // ★ 启动优化 (2026-09-13): 不在 main 线程同步反序列化 16MB game_index.json,
        //   先创建空缓存的 SearchEngine, 让 Tauri 事件循环立即启动 → 窗口秒开.
        //   磁盘索引在后台线程加载, 加载完成后填充缓存 (通常 < 1s).
        //   前端 browse/recommend 会重试直到缓存就绪 (已有 categoryRetryCount 机制).
        Self {
            cache,
            snapshot,
            http: build_client(),
            byrut_cache: Mutex::new(HashMap::new()),
            koyso_cache: Mutex::new(HashMap::new()),
            adult: Mutex::new(AdultState {
                pages: HashMap::new(),
                byrut_pages: HashMap::new(),
                keyword_cache: HashMap::new(),
                next_page: 1,
                byrut_next_page: 1,
                last_call_ms: 0,
            }),
            pz_dl_cache: Mutex::new(HashMap::new()),
            site_totals: Mutex::new((0, 0)),
            site_max_pages: Mutex::new((0, 0)),
            last_disk_save: Mutex::new(0),
            preloading: std::sync::atomic::AtomicBool::new(false),
            index_save_lock: Mutex::new(()),
            preload_spawned: std::sync::atomic::AtomicBool::new(false),
            adult_tagged: std::sync::atomic::AtomicBool::new(false),
            adult_next_page: std::sync::atomic::AtomicU32::new(1),
            ko_tagged: std::sync::atomic::AtomicBool::new(false),
            disk_index_loaded: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 后台线程加载磁盘索引到缓存 (由 setup 回调 spawn, 不阻塞主线程)
    pub fn load_disk_index_background(&self) {
        if self.disk_index_loaded.load(std::sync::atomic::Ordering::SeqCst) {
            return; // 已加载
        }
        let disk = load_disk_index();
        let byrut_pages = disk.byrut;
        let koyso_pages = disk.koyso;
        let avg = |m: &HashMap<u32, Vec<GameCard>>| {
            let n: u64 = m.values().map(|v| v.len() as u64).sum();
            let p = m.len().max(1) as u64;
            n / p
        };
        let by_est = disk.byrut_max_page as u64 * avg(&byrut_pages).max(1);
        let ko_est = disk.koyso_max_page as u64 * avg(&koyso_pages).max(1);

        {
            *self.byrut_cache.lock() = byrut_pages;
            *self.koyso_cache.lock() = koyso_pages;
            *self.site_totals.lock() = (by_est, ko_est);
            *self.site_max_pages.lock() = (disk.byrut_max_page, disk.koyso_max_page);
        }
        self.adult_tagged.store(
            disk.version >= INDEX_VERSION && disk.adult_tagged,
            std::sync::atomic::Ordering::SeqCst,
        );
        self.adult_next_page.store(disk.adult_next_page, std::sync::atomic::Ordering::SeqCst);
        self.ko_tagged.store(
            disk.version >= INDEX_VERSION && disk.ko_tagged,
            std::sync::atomic::Ordering::SeqCst,
        );
        self.disk_index_loaded.store(true, std::sync::atomic::Ordering::SeqCst);
        // ★ 诊断 (2026-10-02): 打印从磁盘**读回来**的成人标签数, 与 save 时对比即可
        //   判断标签是在"合并/写盘"丢了, 还是在"加载/后续覆盖"丢了。
        {
            let by = self.byrut_cache.lock();
            let ko = self.koyso_cache.lock();
            eprintln!(
                "[tag_diag] index_loaded: adult_in_byrut={} adult_in_koyso={} byrut_pages={} ko_pages={} adult_tagged={}",
                count_adult_cards(&by),
                count_adult_cards(&ko),
                by.len(),
                ko.len(),
                self.adult_tagged.load(std::sync::atomic::Ordering::SeqCst)
            );
        }
        eprintln!("[search] 磁盘索引加载完成 (后台线程)");
    }

    /// 磁盘索引是否已加载完成
    pub fn is_disk_index_loaded(&self) -> bool {
        self.disk_index_loaded.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// ★ 首屏预热 (2026-10-02): 把第 1 页的数据**提前**放进内存缓存。
    ///
    /// 背景 (用户反馈"普通游戏加载太慢"): 启动时 `incremental_update()` 会无条件
    /// `fetch_byrut_force(1)` 重新抓第 1 页 (force 绕过缓存), 而用户此时点击首屏
    /// 又触发一次 `browse(1)`。两者都在走网络 + 抢同一把缓存锁, 首屏要等好几秒。
    ///
    /// 这里在索引加载完成后立刻并行把 byrut/koyso 第 1 页灌进缓存:
    ///   · 磁盘索引已有第 1 页 → 直接命中, 零网络 (绝大多数情况)
    ///   · 否则才走一次网络抓取, 且结果写回缓存供后续复用
    /// 这样用户点击首屏时 `browse(1)` 直接读内存, 不再等网络。
    pub async fn warm_first_page(&self) {
        // 已有缓存则无需预热 (磁盘索引恢复时通常已经填好)
        let have_by = self.byrut_cache.lock().contains_key(&1);
        let have_ko = self.koyso_cache.lock().contains_key(&1);
        if have_by && have_ko {
            return;
        }
        // 并行抓取缺失的一侧 (browse_* 内部命中缓存时不会走网络)
        tokio::join!(
            async {
                if !have_by {
                    let _ = self.browse_byrut(1).await;
                }
            },
            async {
                if !have_ko {
                    let _ = self.browse_koyso(1).await;
                }
            }
        );
    }

    // 索引缓存写盘 (原子替换: 先写 tmp 再 rename)
    // ★ 串行锁: 多线程 (retag / batch_translate / 翻译按钮) 并发保存时,
    //   同时写同一个 tmp 会交叉截断产生损坏 JSON, 下次启动静默回退空索引
    pub fn save_index_to_disk(&self) {
        let _guard = self.index_save_lock.lock();
        let idx = {
            let by = self.byrut_cache.lock();
            let ko = self.koyso_cache.lock();
            let (bmax, kmax) = *self.site_max_pages.lock();
            // ★ 诊断 (2026-10-02): 落盘是**完整快照** (by.clone()), 理论上不会丢标签。
            //   这里打印快照里带「成人游戏」的卡片数 —— 若此处非 0 而磁盘上读到 0,
            //   说明问题在"写盘之后被别的路径覆盖", 而不在合并环节。
            if self.adult_tagged.load(std::sync::atomic::Ordering::SeqCst) {
                eprintln!(
                    "[tag_diag] save_index: adult_in_byrut={} adult_in_koyso={} byrut_pages={} ko_pages={}",
                    count_adult_cards(&by),
                    count_adult_cards(&ko),
                    by.len(),
                    ko.len()
                );
            }
            DiskIndex {
                version: INDEX_VERSION,
                byrut_max_page: bmax,
                koyso_max_page: kmax,
                adult_tagged: self.adult_tagged.load(std::sync::atomic::Ordering::SeqCst),
                adult_next_page: self.adult_next_page.load(std::sync::atomic::Ordering::SeqCst),
                ko_tagged: self.ko_tagged.load(std::sync::atomic::Ordering::SeqCst),
                byrut: by.clone(),
                koyso: ko.clone(),
            }
        };
        let path = index_file();
        let tmp = path.with_extension("json.tmp");
        let write_result = std::fs::File::create(&tmp)
            .map_err(|e| e.to_string())
            .and_then(|mut f| {
                use std::io::Write;
                serde_json::to_writer(&mut f, &idx).map_err(|e| e.to_string())?;
                f.sync_all().map_err(|e| e.to_string())
            });
        match write_result {
            Ok(()) => {
                if let Err(e) = std::fs::rename(&tmp, &path) {
                    eprintln!("[index] 索引写盘 rename 失败: {e}");
                }
            }
            Err(e) => {
                eprintln!("[index] 索引写盘失败: {e}");
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }

    /// 磁盘索引是否已有可用缓存 (决定增量 vs 全量预热)
    fn has_disk_index(&self) -> bool {
        // 需要的页数 = (最大页码-1) 的 95% 向上取整 (首页 + 分页 2..=max)
        let need = |max: u32| (max.saturating_sub(1).saturating_mul(95) + 99) / 100;
        let by = self.byrut_cache.lock();
        let (bmax, kmax) = *self.site_max_pages.lock();
        let by_ok = bmax > 0 && by.len() as u32 >= need(bmax);
        drop(by);
        // koyso 页数少: 同样要求 95% 页覆盖, 缺页时走全量预热断点续爬补齐
        let ko_ok = if kmax <= 1 { true } else { self.koyso_cache.lock().len() as u32 >= need(kmax) };
        by_ok && ko_ok
    }
    pub fn has_disk_index_pub(&self) -> bool { self.has_disk_index() }

    /// preload_byko 后台任务是否已启动 (true 表示本次是重复调用, 直接跳过)
    pub fn try_begin_preload_spawn(&self) -> bool {
        !self.preload_spawned.swap(true, std::sync::atomic::Ordering::SeqCst)
    }

    /// 更新游戏的中文翻译名 (按 appid 匹配), 用于翻译名搜索
    /// 返回是否找到了匹配的游戏
    pub fn update_translated_name(&self, appid: &str, name_cn: &str) -> bool {
        if appid.is_empty() || name_cn.is_empty() { return false; }
        let mut found = false;
        // 遍历 byrut 缓存
        {
            let mut by = self.byrut_cache.lock();
            for page in by.values_mut() {
                for g in page.iter_mut() {
                    if g.appid == appid {
                        g.name_cn = name_cn.to_string();
                        found = true;
                    }
                }
            }
        }
        // 遍历 koyso 缓存
        if !found {
            let mut ko = self.koyso_cache.lock();
            for page in ko.values_mut() {
                for g in page.iter_mut() {
                    if g.appid == appid {
                        g.name_cn = name_cn.to_string();
                        found = true;
                    }
                }
            }
        }
        if found {
            // ★ 性能修复 (2026-09-30): 原实现每保存一个翻译名就全量序列化 ~18MB 索引并写盘;
            //   前端卡片自动翻译会对每张卡片各调用一次 → 数十次 18MB 写盘 + 全量卡片遍历,
            //   直接把 UI 线程阻塞数秒 (表现为"点击 游戏/成人游戏 卡一会")。
            //   改为节流写盘 (最短 30s 一次), 翻译名最终仍会持久化, 且不再阻塞交互。
            self.maybe_save_index_throttled();
        }
        found
    }

    /// 获取所有未翻译的游戏 (appid, name) 对, 用于后台批量翻译
    pub fn get_untranslated_games(&self) -> Vec<(String, String)> {
        let mut result = Vec::new();
        let by = self.byrut_cache.lock();
        for page in by.values() {
            for g in page {
                if g.name_cn.is_empty() && !g.name.is_empty() {
                    result.push((g.appid.clone(), g.name.clone()));
                }
            }
        }
        drop(by);
        let ko = self.koyso_cache.lock();
        for page in ko.values() {
            for g in page {
                if g.name_cn.is_empty() && !g.name.is_empty() {
                    result.push((g.appid.clone(), g.name.clone()));
                }
            }
        }
        result
    }

    /// 批量更新翻译名 (用于后台批量翻译), 不立即持久化 (由调用方控制)
    /// ★ 性能修复 (2026-09-30): 原实现是「每个 appid × 遍历全部卡片」= O(pairs × n),
    ///   一次 40 个翻译名要对 ~2 万张卡片重复扫描 40 遍. 改为先建 appid→name 索引再单遍扫描.
    pub fn batch_update_translated_names(&self, pairs: &[(String, String)]) -> usize {
        use std::collections::HashMap;
        let map: HashMap<&str, &str> = pairs
            .iter()
            .filter(|(_, n)| !n.is_empty())
            .map(|(a, n)| (a.as_str(), n.as_str()))
            .collect();
        if map.is_empty() { return 0; }
        let mut count = 0usize;
        let mut by = self.byrut_cache.lock();
        for page in by.values_mut() {
            for g in page.iter_mut() {
                if let Some(n) = map.get(g.appid.as_str()) {
                    if g.name_cn.is_empty() {
                        g.name_cn = (*n).to_string();
                        count += 1;
                    }
                }
            }
        }
        drop(by);
        let mut ko = self.koyso_cache.lock();
        for page in ko.values_mut() {
            for g in page.iter_mut() {
                if let Some(n) = map.get(g.appid.as_str()) {
                    if g.name_cn.is_empty() {
                        g.name_cn = (*n).to_string();
                        count += 1;
                    }
                }
            }
        }
        count
    }

    /// 批量写入翻译名 (覆盖已存在值), 用于前端卡片自动翻译完成后一次性回写.
    /// ★ 性能修复 (2026-09-30): 前端原实现是「每张卡片各调一次 save_translated_name」,
    ///   每次都在 UI 线程做一遍 O(n) 全量卡片遍历 (~2 万张) → 一屏 60 张卡片就是 60 次全量扫描,
    ///   直接把 UI 线程卡住数秒 (表现为"点击 游戏/成人游戏 卡一会")。
    ///   改为一次 IPC 传入整批 (appid, name) 对, 单遍扫描完成, 大幅降低卡顿。
    pub fn batch_set_translated_names(&self, pairs: &[(String, String)]) -> usize {
        use std::collections::HashMap;
        let map: HashMap<&str, &str> = pairs
            .iter()
            .filter(|(a, n)| !a.is_empty() && !n.is_empty())
            .map(|(a, n)| (a.as_str(), n.as_str()))
            .collect();
        if map.is_empty() { return 0; }
        let mut count = 0usize;
        {
            let mut by = self.byrut_cache.lock();
            for page in by.values_mut() {
                for g in page.iter_mut() {
                    if let Some(n) = map.get(g.appid.as_str()) {
                        if g.name_cn.as_str() != *n {
                            g.name_cn = (*n).to_string();
                        }
                        count += 1;
                    }
                }
            }
        }
        {
            let mut ko = self.koyso_cache.lock();
            for page in ko.values_mut() {
                for g in page.iter_mut() {
                    if let Some(n) = map.get(g.appid.as_str()) {
                        if g.name_cn.as_str() != *n {
                            g.name_cn = (*n).to_string();
                        }
                        count += 1;
                    }
                }
            }
        }
        if count > 0 {
            self.maybe_save_index_throttled();
        }
        count
    }

    // 节流写盘: 浏览/增量发现的页也持久化, 最短间隔 30s (避免频繁全量序列化)
    fn maybe_save_index_throttled(&self) {
        const MIN_INTERVAL_MS: u128 = 30_000;
        let mut last = self.last_disk_save.lock();
        let now = now_millis();
        if now.saturating_sub(*last) >= MIN_INTERVAL_MS {
            *last = now;
            drop(last);
            self.save_index_to_disk();
        }
    }

    // ============ 列表浏览 ============
    // 主列表浏览: 成人游戏已独立到专门的成人游戏页 (adult_browse_local),
    // 主列表出口统一过滤掉带 "成人游戏" 标签的卡片
    pub async fn browse(&self, source: &str, page: u32) -> Vec<GameCard> {
        let src = if source.is_empty() { "byrut" } else { source };
        let cards = match src {
            "koyso" | "playzip" => self.browse_koyso(page).await,
            "all" => {
                // 全部来源: koyso + byrut 同页合并, ko 优先排在前面
                // ★ 性能: 两源并发抓取 (原本串行 await 两次网络往返 → 首屏/翻页延迟翻倍)
                let (ko_cards, by_cards) = tokio::join!(
                    self.browse_koyso(page),
                    self.browse_byrut(page)
                );
                let mut seen: std::collections::HashSet<String> = ko_cards.iter().map(|g| g.appid.clone()).collect();
                let mut cards = ko_cards;
                for g in by_cards {
                    if seen.insert(g.appid.clone()) { cards.push(g); }
                }
                // ★ 修复 (2026-09-13): ko 先入列 + by 后入列 → ko 在前 by 在后
                //   不再用 update_time 排序 (旧索引 by 的 update_time 是真实时间戳 ~1.7B,
                //   大于 ko 的基准 1B → 按时间排序会错误地把 by 排到 ko 前面)
                //   现改为: 保留入列顺序 (ko 先 by 后), 各源内部已按页码顺序排列
                cards
            }
            _ => self.browse_byrut(page).await,
        };
        cards.into_iter().filter(|g| !g.tags.iter().any(|t| t == "成人游戏")).collect()
    }

    async fn browse_byrut(&self, page: u32) -> Vec<GameCard> {
        let p = page.max(1);
        // ★ 性能修复 (2026-10-02): 原来 `.lock().get(&p)` 之后**在持锁状态下 clone 整页**
        //   (一页 30~60 张卡, 每张含多个 String)。后台预热 1671 页 + 成人爬取 278 页
        //   会持续抢同一把锁, 首屏读缓存不得不排队 → "普通游戏加载慢"。
        //   改为**持锁只做一次浅拷贝, 锁外 clone**: 锁占用从"克隆整页"缩短到"复制指针"。
        let cached = {
            let guard = self.byrut_cache.lock();
            guard.get(&p).cloned()
        };
        if let Some(cached) = cached {
            return cached;
        }
        let cards = self.fetch_byrut_force(p).await;
        if !cards.is_empty() {
            insert_page_dedup(&mut self.byrut_cache.lock(), p, cards.clone());
            // 浏览发现的页也持久化 (节流), 下次启动无需重抓
            self.maybe_save_index_throttled();
        }
        cards
    }

    // 强制抓取 byrut 页 (不读缓存): 增量检查 + 普通浏览抓取共用
    async fn fetch_byrut_force(&self, p: u32) -> Vec<GameCard> {
        // 第1页: 新游戏专区(单页, 无分页); 第2页起: 主站分页 /page/N/
        let url = if p == 1 {
            format!("{BYRUT_BASE}/new-pcgames/")
        } else {
            format!("{BYRUT_BASE}/page/{}/", p)
        };
        let client = self.http.clone();
        match client.get(&url).send().await {
            Ok(resp) => match resp.text().await {
                Ok(html) => {
                    // 解析分页导航: 最大页码 × 每页数量 = 站点资源总数 (只取更大值)
                    let max_page = extract_max_page_wp(&html);
                    let n = parse_byrut_list(&html, "byrut", p);
                    if max_page > 0 {
                        {
                            let mut mp = self.site_max_pages.lock();
                            if max_page > mp.0 { mp.0 = max_page; }
                        }
                        if !n.is_empty() {
                            let est = max_page as u64 * n.len() as u64;
                            let mut t = self.site_totals.lock();
                            if est > t.0 { t.0 = est; }
                        }
                    }
                    n
                }
                Err(_) => Vec::new(),
            },
            Err(_) => Vec::new(),
        }
    }

    async fn browse_koyso(&self, page: u32) -> Vec<GameCard> {
        let p = page.max(1);
        // ★ 性能修复 (2026-10-02): 同 browse_byrut —— 持锁只做浅拷贝, 锁外 clone。
        let cached = {
            let guard = self.koyso_cache.lock();
            guard.get(&p).cloned()
        };
        if let Some(cached) = cached {
            return cached;
        }
        let url = if p == 1 {
            format!("{PLAYZIP_BASE}/")
        } else {
            format!("{PLAYZIP_BASE}/?page={}", p)
        };
        let cards = self.fetch_playzip_cards(&url, "koyso", p).await;
        if !cards.is_empty() {
            insert_page_dedup(&mut self.koyso_cache.lock(), p, cards.clone());
            self.maybe_save_index_throttled();
        }
        cards
    }

    async fn fetch_playzip_cards(&self, url: &str, source: &str, page: u32) -> Vec<GameCard> {
        let client = self.http.clone();
        match client.get(url)
            .header(reqwest::header::COOKIE, "age_verified=true; site_auth=1")
            .header(reqwest::header::REFERER, PLAYZIP_BASE.to_string())
            .send().await
        {
            Ok(resp) => match resp.text().await {
                Ok(html) => {
                    // 解析分页导航: 最大页码 × 每页数量 = 站点资源总数 (只取更大值)
                    let max_page = extract_max_page_pz(&html);
                    let n = parse_playzip_list(&html, source, page);
                    if max_page > 0 {
                        {
                            let mut mp = self.site_max_pages.lock();
                            if max_page > mp.1 { mp.1 = max_page; }
                        }
                        if !n.is_empty() {
                            let est = max_page as u64 * n.len() as u64;
                            let mut t = self.site_totals.lock();
                            if est > t.1 { t.1 = est; }
                        }
                    }
                    n
                }
                Err(_) => Vec::new(),
            },
            Err(_) => Vec::new(),
        }
    }

    pub async fn browse_by_category(&self, source: &str, category: &str, page: u32) -> Vec<GameCard> {
        let all = self.browse(source, page).await;
        if category.is_empty() || category == "全部类型" { return all; }
        // 分类筛选: category 字段或 tags 标签任一匹配 (成人游戏等标签也走这里)
        all.into_iter().filter(|g| {
            g.category.contains(category) || g.name.contains(category)
                || g.tags.iter().any(|t| t == category)
        }).collect()
    }

    // ============ 成人游戏 (playzip R18 + byrut R18 合并) ============
    // 随机起点 + 顺序翻页: 每次进入成人页随机刷新, 滚动加载依次翻页
    // 两个源各自维护随机起点与翻页游标, 合并结果按 source:appid 去重
    pub async fn browse_adult(&self, _category: Option<&str>, _page: u32) -> Result<Vec<GameCard>, String> {
        let now = now_millis();
        let (pz_page, by_page) = {
            let mut st = self.adult.lock();
            let new_session = now.saturating_sub(st.last_call_ms) > 30_000;
            if new_session {
                // 新会话: 双源各自随机起始页
                st.next_page = pseudo_rand(R18_MAX_PAGES) + 1;
                st.byrut_next_page = pseudo_rand(BYRUT_R18_MAX_PAGES) + 1;
            }
            st.last_call_ms = now;
            (st.next_page, st.byrut_next_page)
        };
        // 并发拉取两个 R18 源
        let (pz, by) = tokio::join!(
            self.fetch_adult_page(pz_page),
            self.fetch_adult_byrut_page(by_page),
        );
        // 翻页推进 (到尾后随机回卷)
        {
            let mut st = self.adult.lock();
            st.next_page = if pz_page >= R18_MAX_PAGES { pseudo_rand(R18_MAX_PAGES) + 1 } else { pz_page + 1 };
            st.byrut_next_page = if by_page >= BYRUT_R18_MAX_PAGES { pseudo_rand(BYRUT_R18_MAX_PAGES) + 1 } else { by_page + 1 };
        }
        // 合并去重 (source:appid 组合键, 两源 appid 数字可能撞车)
        let mut seen = std::collections::HashSet::new();
        let mut cards = Vec::new();
        for g in pz.into_iter().chain(by.into_iter()) {
            let key = format!("{}:{}", g.source, g.appid);
            if key.contains("::") { continue; }
            if seen.insert(key) { cards.push(g); }
        }
        Ok(cards)
    }

    async fn fetch_adult_page(&self, page: u32) -> Vec<GameCard> {
        if let Some(c) = self.adult.lock().pages.get(&page) {
            return c.clone();
        }
        let url = format!("{PLAYZIP_BASE}/category/r18?page={}", page.max(1));
        let cards = self.fetch_playzip_cards(&url, "adult", page).await;
        if !cards.is_empty() {
            self.adult.lock().pages.insert(page, cards.clone());
        }
        cards
    }

    // byrut R18: /tag-nagota/ 标签页, 与主列表相同的 short_item 结构
    async fn fetch_adult_byrut_page(&self, page: u32) -> Vec<GameCard> {
        let p = page.max(1);
        if let Some(c) = self.adult.lock().byrut_pages.get(&p) {
            return c.clone();
        }
        let url = if p == 1 {
            BYRUT_R18_BASE.to_string()
        } else {
            format!("{BYRUT_R18_BASE}page/{}/", p)
        };
        let client = self.http.clone();
        let cards = match client.get(&url).send().await {
            Ok(resp) => match resp.text().await {
                Ok(html) => parse_byrut_list(&html, "byrut", p),
                Err(_) => Vec::new(),
            },
            Err(_) => Vec::new(),
        };
        if !cards.is_empty() {
            self.adult.lock().byrut_pages.insert(p, cards.clone());
        }
        cards
    }

    pub async fn search_adult(&self, keyword: &str) -> Result<Vec<GameCard>, String> {
        let kw = keyword.trim();
        if kw.is_empty() {
            return self.browse_adult(None, 1).await;
        }
        // 60 秒关键词缓存
        let now = now_millis();
        {
            let st = self.adult.lock();
            if let Some((cards, ts)) = st.keyword_cache.get(kw) {
                if now.saturating_sub(*ts) < 60_000 { return Ok(cards.clone()); }
            }
        }
        let url = format!("{PLAYZIP_BASE}/category/r18?keywords={}", urlencoding::encode(kw));
        let cards = self.fetch_playzip_cards(&url, "adult", 1).await;
        self.adult.lock().keyword_cache.insert(kw.to_string(), (cards.clone(), now));
        Ok(cards)
    }

    pub fn adult_tags(&self) -> Vec<String> { self.snapshot.get_adult_tags() }

    // ============ 成人游戏独立页 (本地缓存筛选, 不走网络) ============
    // 前端成人页数据源: 从 BY/KO 内存缓存筛出带 "成人游戏" 标签的游戏
    // 支持子分类筛选 + 关键词搜索 + 分页 (毫秒级响应, 不受源站限流影响)
    /// 成人页候选池（KO / BY 两份，已按分类+关键词筛过）。
    fn adult_pool(&self, category: &str, keyword: &str) -> (Vec<GameCard>, Vec<GameCard>) {
        let kw = keyword.trim().to_lowercase();
        let want_cat = !category.is_empty() && category != "全部类型";
        let hit = |g: &GameCard| -> bool {
            g.tags.iter().any(|t| t == "成人游戏")
                && (!want_cat || g.tags.iter().any(|t| t == category) || g.category == category)
                && (kw.is_empty()
                    || g.name.to_lowercase().contains(&kw)
                    || g.name_original.to_lowercase().contains(&kw)
                    || g.name_cn.to_lowercase().contains(&kw))
        };
        let mut ko = Vec::new();
        let mut by = Vec::new();
        {
            let koc = self.koyso_cache.lock();
            for cards in koc.values() {
                for g in cards {
                    if hit(g) {
                        ko.push(g.clone());
                    }
                }
            }
        }
        {
            let byc = self.byrut_cache.lock();
            for cards in byc.values() {
                for g in cards {
                    if hit(g) {
                        by.push(g.clone());
                    }
                }
            }
        }
        // ★★ playzip 的 R18 分类页（adult.pages）也当成 KO 用（2026-10-08 修）。
        //
        //   以前 KO 段**只**从 koyso_cache 里筛带「成人游戏」标签的卡片，而那个标签
        //   只有在主列表爬到 r18 分类时才会打上。装到新机器上（game_index.json 是
        //   随安装包带的、没爬过 r18 分类）→ KO 池恒为 0 →
        //   成人页只剩 BY 那 5% 的两条（用户报「安装包里成人游戏没资源就只有两个」）。
        //   现在 preload_byko 会主动抓前几页 R18 分类，这里把它们并进 KO 段。
        {
            let ad = self.adult.lock();
            for cards in ad.pages.values() {
                for g in cards {
                    if want_cat && !g.tags.iter().any(|t| t == category) && g.category != category {
                        continue;
                    }
                    if !kw.is_empty()
                        && !g.name.to_lowercase().contains(&kw)
                        && !g.name_original.to_lowercase().contains(&kw)
                    {
                        continue;
                    }
                    let mut g2 = g.clone();
                    // 统一按 KO 归类（它就是 playzip/koyso 的 r18 分类）
                    g2.source = "koyso".into();
                    if !g2.tags.iter().any(|t| t == "成人游戏") {
                        g2.tags.insert(0, "成人游戏".into());
                    }
                    ko.push(g2);
                }
            }
        }
        // ★ byrut 的成人区单独存在 adult.byrut_pages（/for-adults/ 抓来的），
        //   那些卡片**没有** "成人游戏" 标签（解析器不打），所以单独判定、补上标签。
        {
            let ad = self.adult.lock();
            for cards in ad.byrut_pages.values() {
                for g in cards {
                    if want_cat && !g.tags.iter().any(|t| t == category) && g.category != category {
                        continue;
                    }
                    if !kw.is_empty()
                        && !g.name.to_lowercase().contains(&kw)
                        && !g.name_original.to_lowercase().contains(&kw)
                    {
                        continue;
                    }
                    let mut g2 = g.clone();
                    if !g2.tags.iter().any(|t| t == "成人游戏") {
                        g2.tags.insert(0, "成人游戏".into());
                    }
                    by.push(g2);
                }
            }
        }
        (ko, by)
    }

    /// 成人页数据：**三段推送** —— 最上面 KO 40%、中间 GX 55%、最下面 BY 5%（用户指定）。
    ///
    /// ★ 用户要求「成人游戏推送改成三部分：最上面推 ko 资源 40%，中间推送 gx 55%，
    ///   下面 by 5%」—— 所以**不再是加权轮转混排**，而是按段顺序铺开：
    ///   每一页里 KO 占前 40%、GX 占中间 55%、BY 占最后 5%（按每 100 条的固定配额切）。
    /// ★ 另外「r18 和全年龄都要、不要自己选」「同人和 galgame 也不要选，全部推送」
    ///   —— 这里不做年龄/类型过滤。
    pub fn adult_local_browse(&self, category: &str, keyword: &str, page: u32) -> Vec<GameCard> {
        const ADULT_PAGE_SIZE: usize = 40;
        // (来源, 每 100 条的配额) —— 顺序即页面从上到下的顺序
        const MIX: [(&str, usize); 3] = [("koyso", 40), ("galgamex", 55), ("byrut", 5)];

        let start = (page.saturating_sub(1)) as usize * ADULT_PAGE_SIZE;
        let end = start + ADULT_PAGE_SIZE;

        let mut gx = crate::gx::adult_cards(category, keyword);
        let (mut ko, mut by) = self.adult_pool(category, keyword);
        let (gx_len, ko_len, by_len) = (gx.len(), ko.len(), by.len());
        // 段内顺序要稳定（否则翻页会重复/漏）：按更新时间倒序，GX 再按 id 兜底
        ko.sort_unstable_by(|a, b| b.update_time.cmp(&a.update_time));
        by.sort_unstable_by(|a, b| b.update_time.cmp(&a.update_time));
        gx.sort_unstable_by(|a, b| b.update_time.cmp(&a.update_time).then(a.appid.cmp(&b.appid)));

        let mut merged: Vec<GameCard> = Vec::with_capacity(end);
        // ★ 池内先去重，再交给全局编号 —— 否则同一 appid 重复占位，
        //   切片拿到的 16 条里可能有 7 条重复，去重后这一页就只剩 33 条
        //   （实测日志 "合并 33 条：GX 22 / KO 9 / BY 2"）。
        let dedup = |v: &mut Vec<GameCard>| {
            let mut seen = std::collections::HashSet::new();
            v.retain(|g| seen.insert(g.appid.clone()));
        };
        dedup(&mut ko);
        dedup(&mut by);
        dedup(&mut gx);
        let pcts = [MIX[0].1, MIX[1].1, MIX[2].1];
        let merged: Vec<GameCard> =
            adult_plan_page([&ko, &gx, &by], (page.saturating_sub(1)) as usize, pcts, ADULT_PAGE_SIZE);
        let total = ko_len + gx_len + by_len;

        // 配比诊断：第一页记一次，方便确认配比真的生效、池子有多大
        if page == 1 {
            let n = |src: &str| merged.iter().filter(|g| g.source == src).count();
            crate::app_logger::log_window(
                "ADULT_MIX",
                &format!(
                    "cat={:?} kw={:?} 池 GX {} / KO {} / BY {} (总 {}) → 本页 {} 条：GX {} / KO {} / BY {}",
                    category, keyword,
                    gx_len, ko_len, by_len, total,
                    merged.len(), n("galgamex"), n("koyso"), n("byrut")
                ),
            );
        }
        merged
    }

    /// 成人库里**总共有多少款**（前端拿来显示"共 N 款"，
    /// 免得用户只能靠数卡片猜数量对不对）
    pub fn adult_total(&self, category: &str, keyword: &str) -> usize {
        let gx = crate::gx::adult_cards(category, keyword).len();
        let (ko, by) = self.adult_pool(category, keyword);
        gx + ko.len() + by.len()
    }

    // 成人游戏分类标签 (从成人游戏自身 tags 统计频次, 排除 "成人游戏" 本身)
    pub fn adult_categories(&self) -> Vec<String> {
        let mut counter: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        {
            let by = self.byrut_cache.lock();
            let ko = self.koyso_cache.lock();
            for cache in [&*by, &*ko] {
                for cards in cache.values() {
                    for g in cards {
                        if !g.tags.iter().any(|t| t == "成人游戏") { continue; }
                        for t in &g.tags {
                            if t == "成人游戏" { continue; }
                            *counter.entry(t.clone()).or_insert(0) += 1;
                        }
                    }
                }
            }
        }
        // 按出现次数降序
        let mut list: Vec<(String, u32)> = counter.into_iter().collect();
        list.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut out: Vec<String> = list.into_iter().take(24).map(|(t, _)| t).collect();

        // ★ 用户要求「把 gx 资源放到成人资源里面**包括标签**」—— GX 的标签也进这个下拉。
        //   放在 KO/BY 分类后面（它们才是主要筛选项），按站点给的热度排。
        let mut gx_tags: Vec<(String, u64)> = crate::gx::tag_names_with_count();
        gx_tags.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (name, _) in gx_tags {
            if !name.is_empty() && !out.iter().any(|x| x == &name) {
                out.push(name);
            }
        }
        out
    }

    // ============ 搜索 ============
    pub async fn search(&self, query: &str, source: &str, category: &str) -> Vec<GameCard> {
        let q = query.trim().to_lowercase();
        let search_all = source.is_empty() || source == "all";
        let mut all: Vec<GameCard> = Vec::new();

        // byrut 缓存 (内存搜索, 极快)
        if search_all || source == "byrut" {
            {
                let cache = self.byrut_cache.lock();
                let mut pages: Vec<u32> = cache.keys().copied().collect();
                pages.sort_unstable();
                for p in pages {
                    if let Some(cards) = cache.get(&p) {
                        all.extend(cards.clone());
                    }
                }
            }
            // 缓存为空时只爬第 1 页
            if all.is_empty() {
                let cards = self.browse_byrut(1).await;
                all.extend(cards);
            }
            // 有关键词时, 额外走 byrut 服务端搜索获取缓存里没有的游戏
            // (如搜 "合金装备5" 时缓存可能没有 MGS5, 但 byrut 搜索能找到)
            // ★ 修复搜索无响应: translate_texts 多引擎链路可能数秒阻塞, 导致 invoke('search') 长时间不返回,
            //   前端看起来 "搜索词没反应". 用 1.2s 超时兜底, 超时则用原中文查询直查 byrut
            //   (即使翻译未完成, 本地缓存搜索已在前一步完成并加入 all, 这里只是补抓在线结果)
            if !q.is_empty() {
                let search_kw = if contains_cjk(&q) {
                    let translated = tokio::time::timeout(
                        std::time::Duration::from_millis(1200),
                        translate_texts(&[q.clone()], "en")
                    ).await;
                    match translated {
                        Ok(r) => r.first().map(|s| s.trim().to_lowercase())
                            .filter(|s| !s.is_empty()).unwrap_or_else(|| q.clone()),
                        Err(_) => q.clone(),
                    }
                } else {
                    q.clone()
                };
                let url = format!("{BYRUT_BASE}/?s={}", urlencoding::encode(&search_kw));
                if let Ok(resp) = self.http.get(&url).send().await {
                    if let Ok(html) = resp.text().await {
                        let server = parse_byrut_list(&html, "byrut", 1);
                        let have: std::collections::HashSet<String> = all.iter().map(|g| g.appid.clone()).collect();
                        for c in server {
                            if !have.contains(&c.appid) { all.push(c); }
                        }
                    }
                }
            }
        }

        // koyso 缓存 (内存搜索): "all" 时必须包含, 否则只搜 koyso 来源
        if search_all || source == "koyso" || source == "playzip" {
            {
                let cache = self.koyso_cache.lock();
                let mut pages: Vec<u32> = cache.keys().copied().collect();
                pages.sort_unstable();
                for p in pages {
                    if let Some(cards) = cache.get(&p) {
                        all.extend(cards.clone());
                    }
                }
            }
            // 有关键词时, 额外走服务端搜索获取更全结果
            if !q.is_empty() {
                let url = format!("{PLAYZIP_BASE}/?keywords={}", urlencoding::encode(query.trim()));
                let server = self.fetch_playzip_cards(&url, "koyso", 1).await;
                // 去重 (按 appid)
                let have: std::collections::HashSet<String> = all.iter().map(|g| g.appid.clone()).collect();
                for c in server {
                    if !have.contains(&c.appid) { all.push(c); }
                }
            }
        }

        // 主列表搜索: 过滤成人游戏 (已独立到专门的成人游戏页)
        if category != "成人游戏" {
            all.retain(|g| !g.tags.iter().any(|t| t == "成人游戏"));
        }

        if q.is_empty() && (category.is_empty() || category == "全部类型") {
            return all;
        }

        // 搜索时翻译中文查询词到英文, 同时匹配原文和译文
        // 解决 name_cn 未翻译时, 用户用中文名搜不到游戏的问题 (如 "千恋万花" → "Senren Banka")
        // ★ 修复搜索无响应: 此处 translate_texts 同样用 1.2s 超时兜底, 避免阻塞搜索返回
        let mut queries: Vec<String> = vec![q.clone()];
        // 英文查询分词: "metal gear solid 5" → ["metal","gear","solid","5"]
        // 用于部分匹配 (搜 "合金装备5" 时即使没有 MGS5 也能匹配到 MGS3/MGS4 等同系列)
        let mut en_words: Vec<String> = Vec::new();
        if !q.is_empty() && contains_cjk(&q) {
            let translated = tokio::time::timeout(
                std::time::Duration::from_millis(1200),
                translate_texts(&[q.clone()], "en")
            ).await;
            if let Ok(r) = translated {
                if let Some(en) = r.first() {
                    let en_low = en.trim().to_lowercase();
                    if !en_low.is_empty() && en_low != q {
                        queries.push(en_low.clone());
                        for w in en_low.split(|c: char| c.is_whitespace() || c == ':' || c == '-' || c == '_' || c == ',' || c == '(' || c == ')') {
                            let w = w.trim();
                            if w.len() >= 2 || w.chars().all(|c| c.is_ascii_digit()) {
                                en_words.push(w.to_string());
                            }
                        }
                    }
                }
            }
        } else if !q.is_empty() && !contains_cjk(&q) {
            // 英文查询直接分词
            for w in q.to_lowercase().split(|c: char| c.is_whitespace() || c == ':' || c == '-' || c == '_' || c == ',' || c == '(' || c == ')') {
                let w = w.trim();
                if w.len() >= 2 || w.chars().all(|c| c.is_ascii_digit()) {
                    en_words.push(w.to_string());
                }
            }
        }

        // 相关性打分 + 排序: 让最优匹配排最前
        let cat = category;
        let mut scored: Vec<(i32, GameCard)> = all.into_iter().filter_map(|mut g| {
            // 分类筛选: category 字段或 tags 标签任一匹配 (成人游戏/ko 分类标签都走这里)
            let cat_ok = cat.is_empty() || cat == "全部类型"
                || g.category.contains(cat) || g.tags.iter().any(|t| t == cat);
            if !cat_ok { return None; }
            if q.is_empty() { return Some((0, g)); }
            let name = g.name.to_lowercase();
            // byrut 游戏若未缓存英文名, 从 URL slug 实时提取
            if g.name_original.is_empty() && g.source == "byrut" {
                g.name_original = slug_to_english_name(&g.detail_url, &g.appid);
            }
            let orig = g.name_original.to_lowercase();
            let cn = g.name_cn.to_lowercase();
            let mut best = 0i32;
            for k in &queries {
                let k = k.as_str();
                let mut s = 0i32;
                if name == k || orig == k || cn == k { s = s.max(1000); }
                else {
                    if name.starts_with(k) { s = s.max(500); }
                    if orig.starts_with(k) { s = s.max(450); }
                    if cn.starts_with(k) { s = s.max(600); }
                    if let Some(pos) = cn.find(k) { s = s.max(400 - (pos as i32).min(200)); }
                    if let Some(pos) = orig.find(k) { s = s.max(300 - (pos as i32).min(200)); }
                    if let Some(pos) = name.find(k) { s = s.max(200 - (pos as i32).min(200)); }
                }
                best = best.max(s);
            }
            // 标签匹配: 搜索词与任一标签相等/包含 (如搜 "成人游戏"/"视觉小说" 直接命中)
            for t in g.tags.iter() {
                let tl = t.to_lowercase();
                for k in &queries {
                    let k = k.as_str();
                    if tl == k { best = best.max(350); }
                    else if tl.starts_with(k) || tl.contains(k) { best = best.max(200); }
                }
            }
            // 英文分词匹配: 每匹配一个词 +60 分 (最多匹配 4 个词)
            // 让 "合金装备5" → "metal gear solid 5" 能匹配到 MGS3/MGS4 等同系列游戏
            if !en_words.is_empty() {
                let mut matched = 0i32;
                for w in &en_words {
                    let w = w.as_str();
                    if name.contains(w) || orig.contains(w) || cn.contains(w) {
                        matched += 1;
                    }
                }
                if matched > 0 {
                    let word_score = matched * 60;
                    best = best.max(word_score);
                }
            }
            if best == 0 { None } else {
                let len_penalty = (name.len().min(60) as i32) / 4;
                Some((best - len_penalty, g))
            }
        }).collect();

        // 按分数降序, 同分按 update_time 降序
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.update_time.cmp(&a.1.update_time)));
        scored.into_iter().map(|(_, g)| g).collect()
    }

    // ============ 详情 / 下载链接 ============
    pub async fn fetch_detail(&self, source: &str, detail_url: &str) -> GameDetail {
        let is_playzip = detail_url.contains("playzip.com")
            || detail_url.contains("koyso")
            || source == "koyso" || source == "playzip" || source == "adult";
        if is_playzip {
            self.fetch_detail_playzip(detail_url).await
        } else {
            self.fetch_detail_byrut(detail_url).await
        }
    }

    async fn fetch_detail_playzip(&self, detail_url: &str) -> GameDetail {
        let client = self.http.clone();
        // 重试 3 次: 源站偶发 5xx/超时, 单次失败就返回空会导致前端 "一直加载中"
        let mut html = String::new();
        for attempt in 0..3u32 {
            match client.get(detail_url)
                .header(reqwest::header::COOKIE, "age_verified=true; site_auth=1")
                .header(reqwest::header::REFERER, PLAYZIP_BASE.to_string())
                .send().await
            {
                Ok(r) if r.status().is_success() => {
                    if let Ok(body) = r.text().await {
                        if !body.is_empty() { html = body; break; }
                    }
                }
                Ok(r) if r.status().is_server_error() => { /* 5xx 重试 */ }
                Ok(_) => break, // 4xx 不可恢复
                Err(_) => { /* 网络错误重试 */ }
            }
            if attempt + 1 < 3 {
                tokio::time::sleep(std::time::Duration::from_millis(500 * (1 << attempt))).await;
            }
        }
        if html.is_empty() { return GameDetail::default(); }
        let mut detail = parse_playzip_detail(&html);
        // 简介为空时用 Steam 商店搜索兜底 (ko 很多游戏无介绍)
        if detail.description.is_empty() || detail.description.trim().len() < 30 {
            if let Some((desc, img)) = self.steam_description_fallback(&detail.title).await {
                if detail.description.trim().is_empty() {
                    detail.description = desc;
                }
                if detail.cover.is_empty() {
                    detail.cover = img.clone();
                    detail.header_image_large = img;
                }
            }
        }
        // 详情页直接附带直链下载 (失败由 fetch_downloads 兜底重试)
        if let Some(dl) = self.playzip_download_link(detail_url).await {
            detail.size = if detail.size.is_empty() { dl.size.clone() } else { detail.size };
            detail.downloads.push(dl);
        }
        detail
    }

    /// Steam 商店搜索兜底: 按游戏名搜 Steam 拿中文简介 + 头图
    /// 很多 ko 游戏详情页无介绍, Steam 上有 (r18 游戏不在 Steam 会搜不到, 返回 None)
    async fn steam_description_fallback(&self, title: &str) -> Option<(String, String)> {
        let t = title.trim();
        if t.is_empty() { return None; }
        let url = format!(
            "https://store.steampowered.com/api/storesearch/?term={}&l=schinese&cc=CN",
            urlencoding::encode(t)
        );
        let client = self.http.clone();
        let resp = client.get(&url).send().await.ok()?;
        if !resp.status().is_success() { return None; }
        let v: serde_json::Value = resp.json().await.ok()?;
        let item = v.get("items")?.as_array()?.first()?;
        let desc = item.get("short_description").and_then(|d| d.as_str()).unwrap_or("");
        if desc.is_empty() { return None; }
        let img = item.get("tiny_image").and_then(|i| i.as_str()).unwrap_or("").to_string();
        Some((format!("<p>{}</p>", html_escape(desc)), img))
    }

    async fn fetch_detail_byrut(&self, detail_url: &str) -> GameDetail {
        // 重试机制: 源站 (byrutgame.org) 偶发 TLS 握手重置或 Cloudflare 间歇性 5xx,
        // 单次失败就返回空会导致前端 "下载链接都没了". 加 3 次重试 + 递增延迟.
        let client = self.http.clone();
        let mut html = String::new();
        for attempt in 0..3u32 {
            match client.get(detail_url).send().await {
                Ok(r) => {
                    let status = r.status();
                    if status.is_success() {
                        let body = r.text().await.unwrap_or_default();
                        if !body.is_empty() {
                            html = body;
                            break;
                        }
                    } else {
                        // 5xx 才重试, 4xx 直接退出 (404 不会因为重试变成 200)
                        if !status.is_server_error() { break; }
                    }
                }
                Err(_) => { /* 网络错误: 连接重置/超时, 进入下次重试 */ }
            }
            if attempt + 1 < 3 {
                let delay_ms = 500u64 * (1 << attempt); // 500ms → 1000ms → 2000ms
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        }
        if html.is_empty() { return GameDetail::default(); }
        // 简介: game_desc 容器 (含 <p> 段落, 保留 HTML 前端直接渲染)
        // 用深度感知提取, 避免嵌套 div 时非贪婪正则截断
        let mut desc = extract_div_block(&html, "game_desc");
        // 剥掉简介尾部的折叠按钮等杂项标签残留
        if desc.contains("game_desc__toggle") {
            if let Some(pos) = desc.find("game_desc__toggle") {
                desc = desc[..pos].trim().to_string();
            }
        }
        // byrut 用 data-src 懒加载图片, 前端渲染时需要转成 src 才能显示
        let data_src_re = regex::Regex::new(r#"\sdata-src="([^"]+)""#).unwrap();
        desc = data_src_re.replace_all(&desc, " src=\"$1\"").to_string();
        // 移除懒加载占位 src (data:image/svg+xml)
        let placeholder_re = regex::Regex::new(r#"\ssrc="data:image/svg\+xml[^"]*""#).unwrap();
        desc = placeholder_re.replace_all(&desc, "").to_string();
        let title_re = regex::Regex::new(r#"<h1[^>]*>([^<]+)</h1>"#).unwrap();
        // 封面: og:image meta (大图), 兜底海报区 img
        let og_re = regex::Regex::new(r#"property="og:image"\s+content="([^"]+)""#).unwrap();
        let og_re2 = regex::Regex::new(r#"content="([^"]+)"\s+property="og:image""#).unwrap();
        let img_re = regex::Regex::new(r#"(?s)<div class="[^"]*itemtop[^"]*"[^>]*>.*?<img[^>]*src="(https?://[^"]+)"#).unwrap();
        let dl_re = regex::Regex::new(r#"href="(https?://[^"]*index\.php\?do=download[^"]*)""#).unwrap();
        let title = title_re.captures(&html).map(|c| c[1].trim().to_string()).unwrap_or_default();
        let cover = og_re.captures(&html)
            .or_else(|| og_re2.captures(&html))
            .map(|c| c[1].to_string())
            .or_else(|| img_re.captures(&html).map(|c| c[1].to_string()))
            .unwrap_or_default();
        let mut downloads = Vec::new();
        if let Some(m) = dl_re.captures(&html) {
            let url = m[1].to_string();
            downloads.push(DownloadLink {
                name: "BT 种子下载".into(),
                label: "BT 种子下载 (推荐)".into(),
                url,
                engine: "bt".into(),
                link_type: "torrent".into(),
                ..Default::default()
            });
        }
        // 提取简介中的截图 URL (data-src 已转 src)
        let images = extract_image_urls(&desc);
        let extra = if images.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({ "images": images })
        };
        GameDetail {
            title,
            description: desc,
            cover: cover.clone(),
            header_image_large: cover,
            downloads,
            extra,
            ..Default::default()
        }
    }

    pub async fn fetch_downloads(&self, source: &str, detail_url: &str) -> Vec<DownloadLink> {
        let is_playzip = detail_url.contains("playzip.com")
            || detail_url.contains("koyso")
            || source == "koyso" || source == "playzip" || source == "adult";
        if is_playzip {
            match self.playzip_download_link(detail_url).await {
                Some(dl) => vec![dl],
                None => vec![],
            }
        } else {
            // byrut: 从详情页提取 BT 种子链接
            let detail = self.fetch_detail_byrut(detail_url).await;
            detail.downloads
        }
    }

    // playzip 下载直链: POST /api/getGamesDownloadUrl
    // 参数: id, timestamp, secretKey=sha256(timestamp+id+页内密钥), canvasId
    async fn playzip_download_link(&self, detail_url: &str) -> Option<DownloadLink> {
        let id_re = regex::Regex::new(r#"/game/(\d+)"#).unwrap();
        let id = id_re.captures(detail_url).map(|c| c[1].to_string())?;
        let now = now_millis();
        // 缓存 5 分钟内的直链
        if let Some((url, exp)) = self.pz_dl_cache.lock().get(&id) {
            if now < *exp {
                return Some(self.make_playzip_dl(&id, url));
            }
        }
        let ts = now_secs().to_string();
        let hash = sha256_hex(&format!("{ts}{id}{PLAYZIP_SECRET}"));
        let canvas_id = pseudo_rand(u32::MAX).to_string();
        let client = self.http.clone();
        let mut resp = client.post(PLAYZIP_API)
            .header(reqwest::header::COOKIE, "site_auth=1; age_verified=true")
            .header(reqwest::header::REFERER, format!("{PLAYZIP_BASE}/download/{id}"))
            .header(reqwest::header::ORIGIN, PLAYZIP_BASE.to_string())
            .form(&[
                ("id", id.as_str()),
                ("timestamp", ts.as_str()),
                ("secretKey", hash.as_str()),
                ("canvasId", canvas_id.as_str()),
            ])
            .send()
            .await;
        // 失败时从下载页重新提取 secretKey 并重试一次
        if resp.as_ref().map(|r| !r.status().is_success()).unwrap_or(true) {
            if let Some(new_secret) = self.fetch_playzip_secret(&id).await {
                let ts2 = now_secs().to_string();
                let hash2 = sha256_hex(&format!("{ts2}{id}{new_secret}"));
                let canvas2 = pseudo_rand(u32::MAX).to_string();
                resp = client.post(PLAYZIP_API)
                    .header(reqwest::header::COOKIE, "site_auth=1; age_verified=true")
                    .header(reqwest::header::REFERER, format!("{PLAYZIP_BASE}/download/{id}"))
                    .header(reqwest::header::ORIGIN, PLAYZIP_BASE.to_string())
                    .form(&[
                        ("id", id.as_str()),
                        ("timestamp", ts2.as_str()),
                        ("secretKey", hash2.as_str()),
                        ("canvasId", canvas2.as_str()),
                    ])
                    .send()
                    .await;
            }
        }
        let resp = resp.ok()?;
        if !resp.status().is_success() { return None; }
        let txt = resp.text().await.ok()?;
        // 响应为 JSON 字符串: "https://cdn.../xxx.7z?verify=..."
        let url = txt.trim().trim_matches('"').to_string();
        if !url.starts_with("http") { return None; }
        // 缓存 5 分钟
        self.pz_dl_cache.lock().insert(id.clone(), (url.clone(), now + 300_000));
        Some(self.make_playzip_dl(&id, &url))
    }

    async fn fetch_playzip_secret(&self, id: &str) -> Option<String> {
        let client = self.http.clone();
        let html = client.get(format!("{PLAYZIP_BASE}/download/{id}"))
            .header(reqwest::header::COOKIE, "site_auth=1; age_verified=true")
            .header(reqwest::header::REFERER, PLAYZIP_BASE.to_string())
            .send().await
            .ok()?
            .text().await
            .ok()?;
        let re = regex::Regex::new(r#"secretKey="([^"]+)""#).unwrap();
        re.captures(&html).map(|c| c[1].to_string())
    }

    fn make_playzip_dl(&self, _id: &str, url: &str) -> DownloadLink {
        let filename = url.split('?').next().unwrap_or("")
            .rsplit('/').next().unwrap_or("").to_string();
        let decoded = urlencoding::decode(&filename).map(|s| s.to_string()).unwrap_or(filename.clone());
        DownloadLink {
            name: "高速直链".into(),
            label: "高速直链下载 (PlayZip CDN)".into(),
            url: url.to_string(),
            engine: "http".into(),
            filename: decoded,
            ..Default::default()
        }
    }

    // ============ 快照 / 缓存管理 ============
    // 真实游戏数量统计: 各内存缓存中 unique appid 数 (byrut, playzip)
    pub fn unique_counts(&self) -> (u64, u64) {
        let count = |m: &HashMap<u32, Vec<GameCard>>| {
            let set: std::collections::HashSet<&str> =
                m.values().flat_map(|v| v.iter().map(|g| g.appid.as_str())).collect();
            set.len() as u64
        };
        (count(&self.byrut_cache.lock()), count(&self.koyso_cache.lock()))
    }

    pub async fn preload_byko(&self) -> Result<(), String> {
        // ★ 成人页要按 GX/KO/BY 配比推送，但 byrut 的成人区（/for-adults/）
        //   从来没进过主缓存（tag_diag 里 adult_in_byrut=0），BY 就永远占不到那 5%。
        //   这里顺手把前几页拉进成人缓存（每页约 24 条，3 页足够铺满十几页的 5%）。
        for pg in 1..=3u32 {
            let _ = self.fetch_adult_byrut_page(pg).await;
        }
        // ★★ 同理，playzip 的 R18 分类页也要主动抓（2026-10-08）。
        //   装在干净机器上时 game_index.json 是随包带的、没爬过 r18 分类，
        //   KO 段就会恒为 0 → 成人页只剩 BY 的两条。
        //   ⚠️ 必须放在下面 `has_disk_index()` 的**提前返回之前**，否则有索引的机器永远不抓。
        //   抓 5 页（每页约 30 条 ≈ 150 张），够铺十几页的 KO 段（每页 40% 即 16 条）。
        for pg in 1..=5u32 {
            let _ = self.fetch_adult_page(pg).await;
        }
        // 有本地索引缓存: 数量已就绪 (SearchEngine::new 时从磁盘恢复), 只做增量检查
        // 无缓存 (首次): 阶段1 抓首页 + 分页导航解析出总量
        if self.has_disk_index() {
            return Ok(());
        }
        let _ = self.browse_byrut(1).await;
        let _ = self.browse_koyso(1).await;
        {
            let t = *self.site_totals.lock();
            if t.0 == 0 {
                // new-pcgames 无导航: 请求主站分页页解析最大页码 (browse_byrut 内部自动更新 totals/max_pages)
                let _ = self.browse_byrut(2).await;
            }
        }
        Ok(())
    }

    /// 增量检查: 爬最新 1-2 页, 对比缓存中已有的 appid, 新游戏合并进缓存头部并写盘
    /// (本地缓存基础上检查网站新增, 不重爬全站)
    /// 返回 (byrut 新增数, koyso 新增数)
    pub async fn incremental_update(&self) -> (usize, usize) {
        // byrut: 第1页 = new-pcgames 最新游戏
        let fresh_by = self.fetch_byrut_force(1).await;
        // koyso: 第1页
        let fresh_ko = {
            let url = format!("{PLAYZIP_BASE}/");
            self.fetch_playzip_cards(&url, "koyso", 1).await
        };
        if fresh_by.is_empty() && fresh_ko.is_empty() {
            return (0, 0); // 网络失败: 保留本地缓存不动
        }
        let (mut by_new, mut ko_new) = (0usize, 0usize);
        let mut changed = false;
        {
            let mut by = self.byrut_cache.lock();
            let (n, c) = merge_fresh_into_cache(&mut by, &fresh_by, 100);
            by_new = n;
            changed |= c;
        }
        {
            let mut ko = self.koyso_cache.lock();
            let (n, c) = merge_fresh_into_cache(&mut ko, &fresh_ko, 100);
            ko_new = n;
            changed |= c;
        }
        if changed {
            self.save_index_to_disk();
        }
        // 增量补标签: 爬 for-adults / r18 最新页, 新增的成人游戏打上标签
        // (全量标签爬取只在标记未置位时进行, 这里兜底每日新增)
        self.incremental_adult_tags().await;
        (by_new, ko_new)
    }

    /// 增量补成人标签: for-adults 第 1 页 (by 新增) + r18 第 1 页 (ko 新增)
    async fn incremental_adult_tags(&self) {
        // byrut for-adults 第 1 页
        if let Some(html) = self.fetch_html_retry(BYRUT_R18_BASE, None).await {
            let tagged: Vec<GameCard> = parse_byrut_list(&html, "byrut", 1).into_iter().map(|mut g| {
                if !g.tags.iter().any(|t| t == "成人游戏") { g.tags.push("成人游戏".to_string()); }
                g
            }).collect();
            if !tagged.is_empty() {
                merge_tagged_into_cache(&mut self.byrut_cache.lock(), tagged);
            }
        }
        // playzip r18 第 1 页
        let r18_url = format!("{PLAYZIP_BASE}/category/r18?page=1");
        if let Some(html) = self.fetch_html_retry(&r18_url, Some("age_verified=true; site_auth=1")).await {
            let tagged: Vec<GameCard> = parse_playzip_list(&html, "koyso", 1).into_iter().map(|mut g| {
                if !g.tags.iter().any(|t| t == "成人游戏") { g.tags.push("成人游戏".to_string()); }
                if g.category.is_empty() { g.category = "成人游戏".to_string(); }
                g
            }).collect();
            if !tagged.is_empty() {
                merge_tagged_into_cache(&mut self.koyso_cache.lock(), tagged);
            }
        }
    }

    /// 后台全量预热: 并发爬取 byrut + koyso 全部分页到内存缓存
    /// 本地索引已完整时跳过全量 (增量检查由 incremental_update 负责)
    pub async fn preload_all_pages(&self) {
        if self.preloading.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return; // 已在预热中, 防重复
        }
        use futures::stream::{self, StreamExt};
        use std::sync::atomic::{AtomicU32, Ordering};
        static BY_DONE: AtomicU32 = AtomicU32::new(0);

        // 用解析到的真实最大页码作为翻页上界 (byrut ~1650, koyso ~3)
        let (by_max, ko_max) = *self.site_max_pages.lock();

        // 本地缓存已完整 → 跳过全量爬取
        if self.has_disk_index() {
            self.preloading.store(false, Ordering::SeqCst);
            return;
        }

        // byrut 并发 4 路全量爬取 (~1650 页, 后台数分钟)
        if by_max > 1 {
            // 已缓存的页跳过 (断点续爬)
            let have: std::collections::HashSet<u32> = self.byrut_cache.lock().keys().cloned().collect();
            let pages: Vec<u32> = (2..=by_max).filter(|p| !have.contains(p)).collect();
            let total_pages = pages.len();
            stream::iter(pages)
                .for_each_concurrent(4, |p| async move {
                    let _ = self.browse_byrut(p).await;
                    let done = BY_DONE.fetch_add(1, Ordering::Relaxed) + 1;
                    if done % 200 == 0 || done as usize == total_pages {
                        // 预热进度日志 + 定期写盘 (防中断丢失)
                        let (by, ko) = self.unique_counts();
                        let _ = std::fs::write(
                            std::env::temp_dir().join("vortexdl_preload.log"),
                            format!("byrut {done}/{total_pages} unique_by={by} unique_ko={ko}\n"),
                        );
                        self.save_index_to_disk();
                    }
                })
                .await;
        }

        // koyso: 页数少, 顺序爬取
        if ko_max > 1 {
            for p in 2..=ko_max {
                let _ = self.browse_koyso(p).await;
            }
        }
        // 最终写盘
        self.save_index_to_disk();
        self.preloading.store(false, Ordering::SeqCst);
    }

    // ============ 标签补爬: 成人游戏 + ko 分类 ============
    // 后台爬 byrut /for-adults/ 与 playzip 各分类页, 为全部游戏打上标签:
    // - for-adults 页的游戏 → "成人游戏" 标签 (俄语 "Для взрослых")
    // - ko 分类页的游戏 → 对应中文分类标签 (r18 → "成人游戏")
    // 已缓存游戏合并标签; 未缓存游戏入 SYNTHETIC_PAGE 保证可搜索
    // 完成后置位 adult_tagged / ko_tagged 并写盘, 下次启动跳过

    /// 是否还需要补爬标签 (任一源未完成)
    pub fn needs_tagging(&self) -> bool {
        !self.adult_tagged.load(std::sync::atomic::Ordering::SeqCst)
            || !self.ko_tagged.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 后台补爬标签入口 (幂等): 分别爬 byrut for-adults 与 ko 分类
    pub async fn retag_all(&self) {
        if !self.adult_tagged.load(std::sync::atomic::Ordering::SeqCst) {
            self.tag_byrut_adults().await;
        }
        if !self.ko_tagged.load(std::sync::atomic::Ordering::SeqCst) {
            self.tag_koyso_categories().await;
        }
    }

    /// 带重试的 GET (标签爬取用): 网络抖动/5xx 重试 2 次, 全失败返回 None (区别于真空页)
    async fn fetch_html_retry(&self, url: &str, cookie: Option<&str>) -> Option<String> {
        let client = self.http.clone();
        for attempt in 0..3u32 {
            let mut req = client.get(url);
            if let Some(c) = cookie {
                req = req.header(reqwest::header::COOKIE, c)
                    .header(reqwest::header::REFERER, PLAYZIP_BASE.to_string());
            }
            match req.send().await {
                Ok(r) if r.status().is_success() => {
                    if let Ok(html) = r.text().await {
                        if !html.is_empty() { return Some(html); }
                    }
                }
                Ok(r) if r.status().is_server_error() => { /* 5xx 重试 */ }
                Ok(_) => return None, // 4xx 等不可恢复
                Err(_) => { /* 网络错误重试 */ }
            }
            if attempt + 1 < 3 {
                tokio::time::sleep(std::time::Duration::from_millis(600 * (attempt as u64 + 1))).await;
            }
        }
        None
    }

    /// byrut for-adults 全量爬取: 每个游戏打 "成人游戏" 标签, 合并进主索引
    /// 限流容错: 空解析页 (200 但无卡片, 紧跟主列表全量爬取后常见) 同页延迟重试;
    /// 连续 3 页真空才认定尾页; 存在被限流跳过的页则不置完成标记 (下次启动续爬)
    /// 断点续爬: adult_next_page 记录进度, 网络中断后下次启动从断点继续 (不重爬前半部分)
    async fn tag_byrut_adults(&self) {
        let mut aborted = false;
        let mut abort_page: Option<u32> = None; // 网络硬失败中断的页 (下次续爬点)
        let mut skipped = false;      // 有页被限流跳过 → 不置完成标记
        let mut first_hole: Option<u32> = None; // 最早的限流洞 (成功页之前的空页段起点)
        let mut empty_run: Vec<u32> = Vec::new(); // 当前连续空页段 (成功页之前的是孤立洞)
        let start = self.adult_next_page.load(std::sync::atomic::Ordering::SeqCst).max(1);
        for p in start..=BYRUT_R18_MAX_PAGES {
            let url = if p == 1 {
                BYRUT_R18_BASE.to_string()
            } else {
                format!("{BYRUT_R18_BASE}page/{}/", p)
            };
            // 网络失败 (None) 重试后仍拿不到 → 中断本轮, 下次启动从断点续爬 (不置完成标记)
            let Some(html) = self.fetch_html_retry(&url, None).await else {
                aborted = true;
                abort_page = Some(p);
                break;
            };
            let mut cards = parse_byrut_list(&html, "byrut", p);
            // 空解析可能是限流页: 同页延迟重试 2 次
            if cards.is_empty() && p > 1 {
                let mut retry_failed = false;
                for attempt in 1..=2u32 {
                    tokio::time::sleep(std::time::Duration::from_millis(900 * attempt as u64)).await;
                    let Some(html2) = self.fetch_html_retry(&url, None).await else { retry_failed = true; break; };
                    cards = parse_byrut_list(&html2, "byrut", p);
                    if !cards.is_empty() { break; }
                }
                if retry_failed {
                    aborted = true;
                    abort_page = Some(p);
                    break;
                }
            }
            if cards.is_empty() {
                if p > 1 {
                    empty_run.push(p);
                    if empty_run.len() >= 3 { break; } // 连续 3 页真空 → 真实尾页
                }
                continue;
            }
            // 成功页: 之前的连续空页是被限流跳过的孤立洞
            if !empty_run.is_empty() {
                skipped = true;
                if first_hole.is_none() { first_hole = empty_run.first().copied(); }
                empty_run.clear();
            }
            // 全部打上 "成人游戏" 标签 (与俄语类型标签合并去重)
            let tagged: Vec<GameCard> = cards.into_iter().map(|mut g| {
                if !g.tags.iter().any(|t| t == "成人游戏") { g.tags.push("成人游戏".to_string()); }
                g
            }).collect();
            let tagged_n = tagged.len();
            let merged_n = merge_tagged_into_cache(&mut self.byrut_cache.lock(), tagged);
            // ★ 诊断 (2026-10-02): 成人页爬取完成但索引里「成人游戏」标签为 0 的矛盾,
            //   需要看 merge 到底有没有把标签并进去。merged_n = 新增的标签条数,
            //   为 0 说明 appid 一个都没匹配上 (卡片被丢进合成页或直接跳过)。
            if p == 1 || p % 40 == 0 || merged_n == 0 {
                let by = self.byrut_cache.lock();
                eprintln!(
                    "[tag_diag] for-adults p={} cards={} merged_new_tags={} adult_in_cache={} byrut_pages={} synthetic={}",
                    p, tagged_n, merged_n,
                    count_adult_cards(&by),
                    by.len(),
                    by.get(&SYNTHETIC_PAGE).map(|v| v.len()).unwrap_or(0)
                );
            }
            // 每 20 页写盘一次 (防中断丢失), 同时推进断点
            if p % 20 == 0 {
                self.adult_next_page.store(p + 1, std::sync::atomic::Ordering::SeqCst);
                self.save_index_to_disk();
            }
            // 温和限速: 避免触发 byrut 限流
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        if !aborted && !skipped {
            self.adult_tagged.store(true, std::sync::atomic::Ordering::SeqCst);
            self.adult_next_page.store(0, std::sync::atomic::Ordering::SeqCst); // 完成: 清除断点
        } else {
            // 中断: 断点 = 最早的限流洞 (若有) 与中断页中较小者
            let resume = [first_hole, abort_page].into_iter().flatten().min();
            if let Some(r) = resume {
                self.adult_next_page.store(r, std::sync::atomic::Ordering::SeqCst);
            }
        }
        self.save_index_to_disk();
        let (by, ko) = self.unique_counts();
        eprintln!("[tag] byrut for-adults 标签爬取{}完成, unique_by={by} unique_ko={ko}{}",
            if aborted || skipped { "中断" } else { "" },
            match self.adult_next_page.load(std::sync::atomic::Ordering::SeqCst) {
                0 => String::new(),
                n => format!(", 断点=第{}页 (下次启动续爬)", n),
            });
    }

    /// playzip 15 个分类全量爬取: 每个游戏打上分类标签 (r18 → 成人游戏), 合并进主索引
    async fn tag_koyso_categories(&self) {
        const KO_CATEGORIES: &[&str] = &[
            "action", "adventure", "card", "casual", "fighting", "horror", "indie",
            "lan", "r18", "rpg", "rts", "shooting", "simulation", "sports_racing", "strategy",
        ];
        const MAX_PAGES_PER_CAT: u32 = 40; // 安全上限 (分类页指示器显示的是主列表总数, 不可信)
        let mut aborted = false;
        let mut skipped = false; // 有限流跳页 → 不置完成标记, 下次启动续爬
        for (ci, slug) in KO_CATEGORIES.iter().enumerate() {
            let zh = match map_koyso_category(slug) { Some(z) => z, None => continue };
            let mut empty_run: Vec<u32> = Vec::new();
            for p in 1..=MAX_PAGES_PER_CAT {
                let url = format!("{PLAYZIP_BASE}/category/{}?page={}", slug, p);
                let Some(html) = self.fetch_html_retry(&url, Some("age_verified=true; site_auth=1")).await else {
                    aborted = true; // 网络中断: 不置完成标记, 下次启动续爬
                    break;
                };
                let mut cards = parse_playzip_list(&html, "koyso", p);
                // 空解析可能是限流页: 同页延迟重试 2 次
                if cards.is_empty() && p > 1 {
                    for attempt in 1..=2u32 {
                        tokio::time::sleep(std::time::Duration::from_millis(700 * attempt as u64)).await;
                        let Some(html2) = self.fetch_html_retry(&url, Some("age_verified=true; site_auth=1")).await else { aborted = true; break; };
                        cards = parse_playzip_list(&html2, "koyso", p);
                        if !cards.is_empty() { break; }
                    }
                    if aborted { break; }
                }
                if cards.is_empty() {
                    if p > 1 {
                        empty_run.push(p);
                        if empty_run.len() >= 3 { break; } // 连续 3 页真空 → 该分类到底
                    }
                    continue;
                }
                if !empty_run.is_empty() { skipped = true; empty_run.clear(); }
                let tagged: Vec<GameCard> = cards.into_iter().map(|mut g| {
                    // 分类标签 + category 字段 (首次遇到的分类作为主分类)
                    if !g.tags.iter().any(|t| t == zh) { g.tags.push(zh.to_string()); }
                    if g.category.is_empty() { g.category = zh.to_string(); }
                    g
                }).collect();
                merge_tagged_into_cache(&mut self.koyso_cache.lock(), tagged);
                // 温和限速
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            }
            if aborted { break; }
            // 每爬完一个分类写盘一次
            self.save_index_to_disk();
            eprintln!("[tag] ko 分类 {}/{} ({}) 完成", ci + 1, KO_CATEGORIES.len(), slug);
        }
        if !aborted && !skipped {
            self.ko_tagged.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.save_index_to_disk();
        let (by, ko) = self.unique_counts();
        eprintln!("[tag] ko 分类标签爬取{}完成, unique_by={by} unique_ko={ko}", if aborted || skipped { "中断" } else { "" });
    }

    /// 站点资源总数 (byrut, koyso): 全量爬取后的 unique 数与导航估算数取最大
    pub fn total_counts(&self) -> (u64, u64) {
        let count = |m: &HashMap<u32, Vec<GameCard>>| {
            let set: std::collections::HashSet<&str> =
                m.values().flat_map(|v| v.iter().map(|g| g.appid.as_str())).collect();
            set.len() as u64
        };
        let (by_uni, ko_uni) = (count(&self.byrut_cache.lock()), count(&self.koyso_cache.lock()));
        let (by_est, ko_est) = *self.site_totals.lock();
        (by_uni.max(by_est), ko_uni.max(ko_est))
    }

    pub fn recommend(&self, n: usize) -> Vec<GameCard> {
        // ★ 修改 (2026-09-13): 优先推送 ko (koyso) 资源, by (byrut) 资源全部放到 ko 下面
        //   旧版只取 byrut 首页, 既不够随机也不含 ko; 现在从全量缓存取, ko 先 by 后, 去重+排除成人
        self.recommend_filtered(n, false)
    }

    /// ★ 新增 (issue 9): 按"是否成人"过滤的全量推荐
    ///   adult=false → 仅普通游戏 (排除 "成人游戏" 标签)
    ///   adult=true  → 仅成人游戏 (只含 "成人游戏" 标签)
    ///   ko 资源优先排在 by 资源之前, 去重 (source:appid), 各组内按更新时间降序
    fn recommend_filtered(&self, n: usize, adult: bool) -> Vec<GameCard> {
        let is_adult = |g: &GameCard| g.tags.iter().any(|t| t == "成人游戏");
        let mut all: Vec<GameCard> = Vec::new();

        // 1) 先收集 ko (koyso) 资源 — 全部页
        {
            let ko = self.koyso_cache.lock();
            for cards in ko.values() {
                for g in cards {
                    if is_adult(g) != adult { continue; }
                    all.push(g.clone());
                }
            }
        }

        // 2) 再收集 by (byrut) 资源 — 全部页, 放到 ko 下面
        {
            let by = self.byrut_cache.lock();
            for cards in by.values() {
                for g in cards {
                    if is_adult(g) != adult { continue; }
                    all.push(g.clone());
                }
            }
        }

        // 3) 去重 (source:appid 组合键, ko/by 内跨页可能有重复)
        let mut seen = std::collections::HashSet::new();
        let mut deduped: Vec<GameCard> = Vec::with_capacity(all.len());
        for g in &all {
            let key = format!("{}:{}", g.source, g.appid);
            if seen.insert(key) {
                deduped.push(g.clone());
            }
        }
        let ko_actual = deduped.iter().filter(|g| g.source == "koyso").count();

        // 4) ko 部分按更新时间降序排, by 部分也按更新时间降序排, 但 ko 整体在 by 前面
        if ko_actual > 0 && ko_actual < deduped.len() {
            deduped[..ko_actual].sort_unstable_by(|a, b| b.update_time.cmp(&a.update_time));
            deduped[ko_actual..].sort_unstable_by(|a, b| b.update_time.cmp(&a.update_time));
        } else {
            deduped.sort_unstable_by(|a, b| b.update_time.cmp(&a.update_time));
        }

        deduped.into_iter().take(n).collect()
    }

    /// 基于同标签推荐: 查找与指定 appid 有相同标签的游戏, ko 优先
    /// ★ 修改 (issue 9): 严格同类型推荐 —— 成人游戏只推荐成人游戏, 普通游戏只推荐普通游戏
    pub fn recommend_by_tags(&self, appid: &str, n: usize) -> Vec<GameCard> {
        // 1) 查找当前游戏的标签
        let current_tags = {
            let by = self.byrut_cache.lock();
            let ko = self.koyso_cache.lock();
            let mut found: Option<Vec<String>> = None;
            'outer: for cards in by.values() {
                for g in cards {
                    if g.appid == appid { found = Some(g.tags.clone()); break 'outer; }
                }
            }
            if found.is_none() {
                'outer2: for cards in ko.values() {
                    for g in cards {
                        if g.appid == appid { found = Some(g.tags.clone()); break 'outer2; }
                    }
                }
            }
            match found { Some(t) => t, None => return Vec::new() }
        };
        if current_tags.is_empty() { return Vec::new(); }

        // ★ issue 9: 当前游戏是否为成人游戏 → 只推荐同类型
        let is_adult = current_tags.iter().any(|t| t == "成人游戏");

        // 2) 遍历全部缓存, 计算与当前游戏的标签交集数
        //    ko 先收集, by 后收集 → ko 优先排在前面
        let mut ko_recs: Vec<(usize, GameCard)> = Vec::new();
        let mut by_recs: Vec<(usize, GameCard)> = Vec::new();

        for (cache, target) in [
            (&self.koyso_cache, &mut ko_recs),
            (&self.byrut_cache, &mut by_recs),
        ] {
            let c = cache.lock();
            for cards in c.values() {
                for g in cards {
                    if g.appid == appid { continue; } // 排除自身
                    // ★ issue 9: 只推荐与当前游戏同类型 (成人↔成人, 普通↔普通)
                    let g_adult = g.tags.iter().any(|t| t == "成人游戏");
                    if g_adult != is_adult { continue; }
                    let common = g.tags.iter()
                        .filter(|t| current_tags.contains(t))
                        .count();
                    if common > 0 {
                        target.push((common, g.clone()));
                    }
                }
            }
        }

        // 3) 各组内按共同标签数降序, 同分按 update_time 降序
        ko_recs.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(b.1.update_time.cmp(&a.1.update_time)));
        by_recs.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(b.1.update_time.cmp(&a.1.update_time)));

        // 4) 去重 (ko 优先: 先遍历 ko 后遍历 by)
        let mut seen = std::collections::HashSet::new();
        let mut result: Vec<GameCard> = Vec::with_capacity(n);
        for (_, g) in ko_recs.iter().chain(by_recs.iter()) {
            if result.len() >= n { break; }
            let key = format!("{}:{}", g.source, g.appid);
            if seen.insert(key) {
                result.push(g.clone());
            }
        }

        // 5) 如果同标签游戏不足 n 个, 用全量推荐补齐 (ko 优先)
        // ★ issue 9: 补齐也必须同类型 (成人→成人, 普通→普通), 不能回退到普通推荐
        if result.len() < n {
            let mut more = self.recommend_filtered(n - result.len(), is_adult);
            let existing: std::collections::HashSet<String> = result.iter()
                .map(|g| format!("{}:{}", g.source, g.appid)).collect();
            for g in more.drain(..) {
                if result.len() >= n { break; }
                let key = format!("{}:{}", g.source, g.appid);
                if !existing.contains(&key) && !seen.contains(&key) {
                    result.push(g);
                }
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> SearchEngine {
        let cache = Arc::new(crate::cache::Cache::new());
        let snapshot = Arc::new(crate::snapshot::Snapshot::new());
        SearchEngine::new(cache, snapshot)
    }

    fn card(appid: &str) -> GameCard {
        GameCard { appid: appid.into(), name: format!("游戏{appid}"), ..Default::default() }
    }

    fn total_games(m: &HashMap<u32, Vec<GameCard>>) -> usize {
        let s: std::collections::HashSet<&str> =
            m.values().flat_map(|v| v.iter().map(|g| g.appid.as_str())).collect();
        s.len()
    }

    #[test]
    fn test_adult_local_browse_and_categories() {
        let e = engine();
        // 造数: 3 个成人游戏 + 2 个普通游戏, 分布在 BY/KO 两缓存
        let mut adult1 = card("a1");
        adult1.tags = vec!["成人游戏".into(), "视觉小说".into()];
        adult1.name = "Adult VN One".into();
        let mut adult2 = card("a2");
        adult2.tags = vec!["成人游戏".into(), "休闲游戏".into()];
        adult2.name = "Adult Casual Two".into();
        let mut adult3 = card("a3");
        adult3.tags = vec!["成人游戏".into(), "视觉小说".into()];
        adult3.name = "Adult VN Three".into();
        let mut normal1 = card("n1");
        normal1.tags = vec!["视觉小说".into()];
        normal1.name = "Normal VN".into();
        let mut normal2 = card("n2");
        normal2.tags = vec!["动作游戏".into()];
        normal2.name = "Normal Action".into();
        e.byrut_cache.lock().insert(1, vec![adult1, normal1]);
        e.koyso_cache.lock().insert(1, vec![adult2, adult3, normal2]);

        // 全量浏览: 只含成人游戏, 不含普通游戏
        let all = e.adult_local_browse("全部类型", "", 1);
        assert_eq!(all.len(), 3, "成人页应只含成人游戏");
        assert!(all.iter().all(|g| g.tags.iter().any(|t| t == "成人游戏")));

        // 分类筛选: 视觉小说 → 2 个
        let vn = e.adult_local_browse("视觉小说", "", 1);
        assert_eq!(vn.len(), 2);

        // 关键词搜索: "casual" → 1 个
        let kw = e.adult_local_browse("全部类型", "casual", 1);
        assert_eq!(kw.len(), 1);
        assert_eq!(kw[0].appid, "a2");

        // 分类标签统计: 视觉小说 2 + 休闲游戏 1, 按频次排序, 不含 "成人游戏" 本身
        let cats = e.adult_categories();
        assert_eq!(cats.first().map(|s| s.as_str()), Some("视觉小说"));
        assert!(cats.contains(&"休闲游戏".to_string()));
        assert!(!cats.contains(&"成人游戏".to_string()));
    }

    #[test]
    /// ★★ 回归测试 (2026-10-08)：playzip 的 R18 分类页（`adult.pages`）必须算进 KO 段。
    ///
    ///   以前 KO 段**只**从 koyso_cache 里筛带「成人游戏」标签的卡片，而那个标签
    ///   要爬到 r18 分类才会打上。装到干净机器上（game_index.json 是随安装包带的、
    ///   没爬过 r18 分类）→ KO 池恒为 0 → 成人页只剩 BY 那 5% 的两条
    ///   （用户报「安装包里成人游戏没资源就只有两个」）。
    #[test]
    fn test_adult_pool_includes_playzip_r18_pages_as_ko() {
        let e = engine();
        // 造一张"R18 分类页"卡片：没有「成人游戏」标签，source 也不是 koyso
        let mut c = card("r18-1");
        c.source = "adult".into();
        c.name = "测试R18游戏".into();
        c.tags = vec!["RPG".into()];
        e.adult.lock().pages.insert(1, vec![c]);

        let (ko, _by) = e.adult_pool("全部类型", "");
        assert!(
            ko.iter().any(|g| g.name == "测试R18游戏"),
            "R18 分类页的卡片没有进 KO 段（成人页会只剩 BY 的 5%）"
        );
        let got = ko.iter().find(|g| g.name == "测试R18游戏").unwrap();
        assert_eq!(got.source, "koyso", "应统一按 KO 归类");
        assert!(
            got.tags.iter().any(|t| t == "成人游戏"),
            "缺标签时应当补上「成人游戏」"
        );
    }

    #[test]
    fn test_adult_section_slice_ko40_gx55_by5() {
        // 每页 40 条：KO 16 / GX 22 / BY 2，且分段不能重叠、不能漏
        let page = |p: usize| {
            let start = (p - 1) * 40;
            let end = start + 40;
            let ko = adult_section_slice(start, end, 40);
            let gx = adult_section_slice(start, end, 55);
            let by = adult_section_slice(start, end, 5);
            (ko, gx, by)
        };
        // 第 1 页
        let (ko, gx, by) = page(1);
        assert_eq!(ko, (0, 16), "第 1 页 KO 段");
        assert_eq!(gx, (0, 22), "第 1 页 GX 段");
        assert_eq!(by, (0, 2), "第 1 页 BY 段");
        assert_eq!(ko.1 + gx.1 + by.1, 40, "三段加起来必须正好一页");
        // 第 2 页：段内偏移接着上一页，条数不变
        let (ko2, gx2, by2) = page(2);
        assert_eq!(ko2, (16, 16));
        assert_eq!(gx2, (22, 22));
        assert_eq!(by2, (2, 2));
        assert_eq!(ko2.1 + gx2.1 + by2.1, 40);
        // 连续 5 页：偏移必须严格接续，不重不漏
        let mut expect = (0usize, 0usize, 0usize);
        for p in 1..=5usize {
            let (ko, gx, by) = page(p);
            assert_eq!(ko.0, expect.0, "第 {p} 页 KO 偏移接续");
            assert_eq!(gx.0, expect.1, "第 {p} 页 GX 偏移接续");
            assert_eq!(by.0, expect.2, "第 {p} 页 BY 偏移接续");
            expect = (ko.0 + ko.1, gx.0 + gx.1, by.0 + by.1);
        }
        // 5 页共 200 条 → KO 80 / GX 110 / BY 10
        assert_eq!(expect, (80, 110, 10), "5 页累计配额 = 40%/55%/5%");
    }

    /// ★ 全局编号 + 池耗尽顺延：三个池大小悬殊时，页面不能缩水、翻页不能重复/漏条。
    ///
    /// 这是用户报「成人游戏数量明显不对」的回归测试：旧实现按每页各自切片，
    /// KO 池一旦耗尽（装机版实测只有 150 条，而 KO 配额 40%），
    /// 那一页最上面 40% 就整段空掉 —— 40 条的页变成 24 条。
    #[test]
    fn test_adult_plan_page_backfills_exhausted_pool() {
        let mk = |src: &str, n: usize| -> Vec<GameCard> {
            (0..n).map(|i| GameCard {
                appid: format!("{src}-{i}"),
                name: format!("{src}{i}"),
                source: src.into(),
                ..Default::default()
            }).collect()
        };
        // KO 只有 10 条（配额 40%），GX 100 条，BY 10 条 → 总共 120 条
        let ko = mk("koyso", 10);
        let gx = mk("galgamex", 100);
        let by = mk("byrut", 10);
        let pcts = [40, 55, 5];
        let pools = [&ko[..], &gx[..], &by[..]];

        // 一页一页取，拼起来必须正好是全部 120 条、无重复、无遗漏
        let mut all: Vec<String> = Vec::new();
        let mut page = 0usize;
        loop {
            let got = adult_plan_page(pools, page, pcts, 40);
            if got.is_empty() {
                break;
            }
            // 前三页必须是满的（池子加起来够 120 条）
            if page < 3 {
                assert_eq!(got.len(), 40, "第 {} 页缩水了（池耗尽后没顺延补齐）", page + 1);
            }
            all.extend(got.into_iter().map(|g| g.appid));
            page += 1;
            assert!(page < 50, "翻页死循环");
        }
        assert_eq!(all.len(), 120, "总数必须等于三个池之和");
        let uniq: std::collections::HashSet<&String> = all.iter().collect();
        assert_eq!(uniq.len(), 120, "翻页出现了重复条目");
        // KO 的 10 条必须全部出现过（不能因为池小就被跳过）
        for i in 0..10 {
            assert!(all.contains(&format!("koyso-{i}")), "KO 第 {i} 条丢了");
        }
    }

    /// 三个池都足够大时，配比必须精确等于 40/55/5（每页 16/22/2）。
    #[test]
    fn test_adult_plan_page_keeps_ratio_when_pools_are_big() {
        let mk = |src: &str, n: usize| -> Vec<GameCard> {
            (0..n).map(|i| GameCard {
                appid: format!("{src}-{i}"),
                name: format!("{src}{i}"),
                source: src.into(),
                ..Default::default()
            }).collect()
        };
        let ko = mk("koyso", 1000);
        let gx = mk("galgamex", 1000);
        let by = mk("byrut", 1000);
        let got = adult_plan_page([&ko[..], &gx[..], &by[..]], 0, [40, 55, 5], 40);
        assert_eq!(got.len(), 40);
        let n = |s: &str| got.iter().filter(|g| g.source == s).count();
        assert_eq!((n("koyso"), n("galgamex"), n("byrut")), (16, 22, 2), "配比必须是 40/55/5");
        // 段顺序：KO 全在最前、BY 全在最后
        let first_gx = got.iter().position(|g| g.source == "galgamex").unwrap();
        let last_ko = got.iter().rposition(|g| g.source == "koyso").unwrap();
        assert!(last_ko < first_gx, "KO 段必须在 GX 段前面");
    }

    #[test]
    fn test_parse_byrut_list_variant_layout() {
        // for-adults 区变体布局: 标题在 h2.short_title > a, 无 data-appid, 无 game-preview__title
        let html = r#"
        <article class="short_item">
            <div class="short_img">
                <a href="https://byrutgame.org/57183-skimmed.html"><img src="https://img.byrutgame.org/x.jpg" loading="lazy" alt="Skimmed" width="174" height="246"></a>
                <span class="short_size">360 МБ</span>
            </div>
            <h2 class="short_title">
                <a href="https://byrutgame.org/57183-skimmed.html">Skimmed</a>
            </h2>
            <div class="shor_subtitle"><div class="short_line"><span class="downloads i_downl">688</span></div></div>
        </article>
        <article class="short_item">
            <div class="short_img">
                <a href="https://byrutgame.org/57516-sword-saint.html"><img src="https://img.byrutgame.org/y.jpg" alt="Sword Saint"></a>
            </div>
            <h2 class="short_title">
                <a href="https://byrutgame.org/57516-sword-saint.html">Sword Saint</a>
            </h2>
        </article>
        <article class="short_item is-adult">
            <div class="short_img" data-appid="5117090">
                <a href="https://byrutgame.org/58352-take-me-in-totality.html"><img src="https://img.byrutgame.org/w.jpg" alt="Take Me in Totality"></a>
            </div>
            <h2 class="short_title">
                <a href="https://byrutgame.org/58352-take-me-in-totality.html">Take Me in Totality</a>
            </h2>
        </article>
        "#;
        let cards = parse_byrut_list(html, "byrut", 1);
        assert_eq!(cards.len(), 3, "变体布局应解析出全部文章（含 is-adult 附加类名）");
        let c0 = &cards[0];
        assert_eq!(c0.name, "Skimmed");
        assert_eq!(c0.appid, "57183", "无 data-appid 时应从 URL 提取文章 ID");
        assert_eq!(c0.detail_url, "https://byrutgame.org/57183-skimmed.html");
        assert_eq!(c0.header_image, "https://img.byrutgame.org/x.jpg");
        assert_eq!(c0.name_original, "Skimmed");
        let c1 = &cards[1];
        assert_eq!(c1.name, "Sword Saint");
        assert_eq!(c1.appid, "57516");
        let c2 = &cards[2];
        assert_eq!(c2.name, "Take Me in Totality", "is-adult 类名不得导致整页漏解析");
        assert_eq!(c2.appid, "5117090");
    }

    #[test]
    fn test_parse_byrut_list_standard_layout() {
        // 标准布局: game-preview__title + data-appid (回归保护)
        let html = r#"
        <article class="short_item">
            <div class="imgbox" data-appid="2842040">
                <template class="js-game-preview-data">
                    <div class="game-preview__title">Star Wars Outlaws</div>
                    <div class="game-preview__genres">Открытый мир · Экшены</div>
                </template>
                <a href="https://byrutgame.org/39974-star-wars-outlaws.html"><img src="https://img.byrutgame.org/z.jpg" alt="Star Wars Outlaws"></a>
            </div>
        </article>
        "#;
        let cards = parse_byrut_list(html, "byrut", 1);
        assert_eq!(cards.len(), 1);
        let c = &cards[0];
        assert_eq!(c.name, "Star Wars Outlaws");
        assert_eq!(c.appid, "2842040", "标准布局应优先 data-appid");
        assert!(c.category.len() > 0, "标准布局应有分类");
    }

    #[test]
    fn test_merge_fresh_new_games_prepend() {
        // 缓存: 第1页 3 个旧游戏, 第2页 2 个
        let mut cache = HashMap::new();
        cache.insert(1, vec![card("a"), card("b"), card("c")]);
        cache.insert(2, vec![card("d"), card("e")]);
        // 网站最新第1页: 2 个新游戏 + 1 个已有
        let fresh = vec![card("n1"), card("n2"), card("a")];
        let (new_count, changed) = merge_fresh_into_cache(&mut cache, &fresh, 100);
        assert!(changed);
        assert_eq!(new_count, 2);
        // 新游戏在前, 顺序 = fresh 顺序
        assert_eq!(cache[&1].iter().map(|g| g.appid.as_str()).collect::<Vec<_>>(), ["n1", "n2", "a", "b", "c"]);
        // 总数只增不减, 无重复
        assert_eq!(total_games(&cache), 5 + 2);
    }

    #[test]
    fn test_merge_fresh_page1_overflow_to_page2() {
        // 第1页 105 个 (超过 cap=100), 第2页 2 个
        let mut cache = HashMap::new();
        let p1: Vec<GameCard> = (0..105).map(|i| card(&format!("o{i}"))).collect();
        cache.insert(1, p1);
        cache.insert(2, vec![card("p1"), card("p2")]);
        // 网站第1页无新游戏 (全是已有的)
        let fresh: Vec<GameCard> = (0..10).map(|i| card(&format!("o{i}"))).collect();
        let (new_count, changed) = merge_fresh_into_cache(&mut cache, &fresh, 100);
        assert_eq!(new_count, 0);
        assert!(changed); // 溢出挪动也算变化
        // 第1页被限制在 100
        assert_eq!(cache[&1].len(), 100);
        // 第2页头部是溢出的 o100..o104 (新的在前), 原有 p1/p2 在后
        assert_eq!(
            cache[&2].iter().map(|g| g.appid.as_str()).collect::<Vec<_>>()[..5],
            ["o100", "o101", "o102", "o103", "o104"]
        );
        // 总数不变: 105 + 2 = 107
        assert_eq!(total_games(&cache), 107);
    }

    #[test]
    fn test_merge_fresh_no_change() {
        let mut cache = HashMap::new();
        cache.insert(1, vec![card("a"), card("b")]);
        let fresh = vec![card("a"), card("b")];
        let (new_count, changed) = merge_fresh_into_cache(&mut cache, &fresh, 100);
        assert!(!changed);
        assert_eq!(new_count, 0);
        assert_eq!(cache[&1].len(), 2);
    }

    #[test]
    fn test_disk_index_roundtrip() {
        let idx = DiskIndex {
            version: 3,
            byrut_max_page: 1650,
            koyso_max_page: 3,
            adult_tagged: true,
            ko_tagged: true,
            adult_next_page: 137,
            byrut: { let mut m = HashMap::new(); m.insert(1, vec![card("a")]); m },
            koyso: HashMap::new(),
        };
        let json = serde_json::to_string(&idx).unwrap();
        let back: DiskIndex = serde_json::from_str(&json).unwrap();
        assert_eq!(back.byrut_max_page, 1650);
        assert_eq!(back.byrut[&1][0].appid, "a");
        assert!(back.adult_tagged && back.ko_tagged);
        assert_eq!(back.adult_next_page, 137);
        // 字段缺失时使用默认值 (向后兼容)
        let back2: DiskIndex = serde_json::from_str("{}").unwrap();
        assert_eq!(back2.byrut_max_page, 0);
        assert!(back2.byrut.is_empty());
        assert!(!back2.adult_tagged && !back2.ko_tagged);
        assert_eq!(back2.adult_next_page, 0);
    }

    #[test]
    fn test_merge_tagged_into_cache() {
        // 已缓存游戏: 标签并集
        let mut cache = HashMap::new();
        let mut g = card("a");
        g.tags = vec!["动作游戏".into()];
        cache.insert(1, vec![g]);
        // for-adults 爬到同一游戏: 带新标签
        let mut c = card("a");
        c.tags = vec!["成人游戏".into(), "动作游戏".into()];
        let n = merge_tagged_into_cache(&mut cache, vec![c]);
        assert!(n >= 1);
        assert_eq!(cache[&1][0].tags.len(), 2);
        assert!(cache[&1][0].tags.contains(&"成人游戏".to_string()));
        // 未缓存游戏 → SYNTHETIC_PAGE
        let mut c2 = card("b");
        c2.tags = vec!["成人游戏".into()];
        merge_tagged_into_cache(&mut cache, vec![c2]);
        assert!(cache.contains_key(&SYNTHETIC_PAGE));
        assert!(cache[&SYNTHETIC_PAGE].iter().any(|g| g.appid == "b"));
        // 同一游戏再次出现 → 合成页去重
        let mut c3 = card("b");
        c3.tags = vec!["成人游戏".into()];
        merge_tagged_into_cache(&mut cache, vec![c3]);
        assert_eq!(cache[&SYNTHETIC_PAGE].iter().filter(|g| g.appid == "b").count(), 1);
    }

    #[test]
    fn test_insert_page_dedup() {
        // 合成页有游戏 b; 真实第 2 页也含 b → 插入后 b 从合成页移除
        let mut cache = HashMap::new();
        let mut b = card("b");
        b.tags = vec!["成人游戏".into()];
        cache.insert(SYNTHETIC_PAGE, vec![b]);
        let real = vec![card("a"), card("b")];
        insert_page_dedup(&mut cache, 2, real);
        assert!(cache.contains_key(&2));
        assert!(!cache.contains_key(&SYNTHETIC_PAGE), "真实页插入后合成页应清空");
        assert_eq!(cache[&2].len(), 2);
        // ★ 真实页卡片应继承合成页的标签 (防止标签爬取先于真实页爬取时标签丢失)
        let b_real = cache[&2].iter().find(|g| g.appid == "b").unwrap();
        assert!(b_real.tags.contains(&"成人游戏".to_string()), "真实页卡片应保留合成页标签");
    }

    #[test]
    fn test_parse_playzip_list() {
        let html = r#"<div class="games_content"><a class="game_item" href="/game/2728"><div class="skeleton"></div><div class="game_media"><img loading="lazy" data-src="https://cdn.example.com/a.jpg" alt="黑神话: 悟空"></div><div class="game_info"><span>黑神话: 悟空</span></div></a></div>"#;
        let cards = parse_playzip_list(html, "koyso", 1);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].appid, "2728");
        assert_eq!(cards[0].name, "黑神话: 悟空");
        assert_eq!(cards[0].detail_url, "https://playzip.com/game/2728");
        assert!(cards[0].header_image.contains("a.jpg"));
    }

    #[test]
    fn test_parse_playzip_detail() {
        let html = r#"<h1 class="content_title">修干嘛? 二手3C店的闇营业 下载</h1><div class="capsule_div"><img src="https://cdn.example.com/cap.jpg" alt="x"></div><div class="content_body"><p>游戏简介文本</p></div><div style="padding: 0 10px;"><time datetime="2026-07-08T18:22:00Z">07/08/2026</time></div><ul><li><span>游戏大小</span><span>1GB</span></li><li><span>游戏版本</span><span>v1.14</span></li></ul>"#;
        let d = parse_playzip_detail(html);
        assert_eq!(d.title, "修干嘛? 二手3C店的闇营业");
        assert!(d.cover.contains("cap.jpg"));
        assert!(d.description.contains("游戏简介文本"));
        assert_eq!(d.size, "1GB");
        assert_eq!(d.version, "v1.14");
    }

    #[tokio::test]
    async fn test_browse_adult_live() {
        let e = engine();
        let cards = e.browse_adult(None, 1).await.unwrap();
        assert!(!cards.is_empty(), "成人游戏列表为空");
        assert!(cards[0].detail_url.contains("playzip.com/game/"), "detail_url 错误: {}", cards[0].detail_url);
        assert!(!cards[0].name.is_empty());
        assert!(!cards[0].header_image.is_empty(), "封面图为空");
    }

    #[tokio::test]
    async fn test_search_adult_live() {
        let e = engine();
        let cards = e.search_adult("电车").await.unwrap();
        assert!(!cards.is_empty(), "成人搜索结果为空");
    }

    #[tokio::test]
    async fn test_browse_koyso_live() {
        let e = engine();
        let cards = e.browse_koyso(1).await;
        assert!(!cards.is_empty(), "koyso 列表为空");
        assert_eq!(cards[0].source, "koyso");
    }

    #[tokio::test]
    async fn test_preload_totals_live() {
        let e = engine();
        e.preload_byko().await.unwrap();
        let (by, ko) = *e.site_totals.lock();
        // byrut: 分页导航估算 (1650 页 × ~38/页 ≈ 6 万+); koyso: 3 页 × ~39
        assert!(by > 10000, "byrut 估算总数异常偏小: {}", by);
        assert!(ko > 0, "koyso 估算总数为 0");
        let (tby, tko) = e.total_counts();
        assert!(tby >= by && tko >= ko, "total_counts 应 >= 估算值: {tby}/{by}, {tko}/{ko}");
        // 数量立即反映真实总量 (无需等待全量预热)
        let unique = e.unique_counts();
        assert!(tby > unique.0, "预热未完成时 total 应大于 unique: {tby} vs {}", unique.0);
    }

    #[tokio::test]
    async fn test_fetch_detail_byrut_live() {
        let e = engine();
        // 真实 byrut 详情页: game_desc 简介容器 + og:image 封面
        let d = e.fetch_detail_byrut("https://byrutgame.org/47346-winion-virus.html").await;
        assert!(!d.title.is_empty(), "标题为空");
        assert!(d.description.len() > 200, "简介过短 ({}), game_desc 解析失败", d.description.len());
        assert!(d.cover.contains("img.byrutgame.org"), "封面解析失败: {}", d.cover);
    }

    #[tokio::test]
    async fn test_fetch_detail_playzip_live() {
        let e = engine();
        let d = e.fetch_detail("adult", "https://playzip.com/game/3148").await;
        assert!(!d.title.is_empty(), "标题为空");
        assert!(!d.description.is_empty(), "简介为空");
        assert!(!d.cover.is_empty(), "封面为空");
        assert!(!d.downloads.is_empty(), "下载链接为空");
        assert!(d.downloads[0].url.starts_with("http"), "下载链接无效: {}", d.downloads[0].url);
    }

    #[tokio::test]
    async fn test_fetch_downloads_byrut_live() {
        let e = engine();
        let dls = e.fetch_downloads("byrut", "https://byrutgame.org/16609-elden-ring.html").await;
        assert!(!dls.is_empty(), "byrut 下载链接为空");
        assert_eq!(dls[0].link_type, "torrent");
        assert!(dls[0].url.contains("do=download"));
    }

    #[tokio::test]
    async fn test_fetch_downloads_koyso_live() {
        let e = engine();
        let dls = e.fetch_downloads("koyso", "https://playzip.com/game/1129").await;
        assert!(!dls.is_empty(), "koyso 下载链接为空 (playzip API 可能改了校验)");
        assert!(dls[0].url.starts_with("http"), "下载链接无效: {:?}", dls.first().map(|d| &d.url));
    }

}
