//! Kazumi 规则里用到的那一小撮 XPath 求值器。
//!
//! Kazumi 的规则文件用 XPath 选元素（`//div[2]/div[1]/a` 这种）。我们原来只有 CSS
//! 选择器（`scraper::Selector`）。把 KazumiRules 全部 86 条规则里出现过的表达式
//! dump 出来统计过，语法只用到：
//!
//! ```text
//! //       后代
//! /        子
//! .//      相对当前节点的后代
//! 标签名 / *
//! [n]              位置
//! [@a='v']         属性相等
//! [contains(@a,'v')]  属性包含
//! ```
//!
//! 没有 `text()`、没有 `@attr` 终结符、没有 `|` 联合、没有其它函数。
//! 所以这里只实现这个子集 —— 拉一个完整的 XPath 引擎（sxd-xpath 要 XML、
//! libxml 要 C 库）不划算。
//!
//! ★ 语义细节：`[n]` 是**位置谓词**，XPath 里 `//div[1]` 表示"是它父节点下第 1 个
//!   div"，而不是"整个文档里第 1 个 div"。所以位置谓词必须**按父节点分组**再取第 n 个，
//!   直接对结果列表取下标是错的（很多站点的卡片嵌套深度不同，一错就全选歪）。
//!
//! ★ `//` 在开头时按"当前上下文的全部后代"处理（XPath 严格语义是从文档根算起，
//!   但 Kazumi 的规则是**相对条目节点**写的，比如 searchName 是相对每条搜索结果）。
//!   两者在"上下文=文档根"时结果一致，所以这样处理是安全的。

use scraper::ElementRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Child,
    Descendant,
}

#[derive(Debug, Clone, PartialEq)]
enum Pred {
    /// `[n]` —— 按父节点分组后的位置（从 1 开始）
    Position(usize),
    /// `[@a='v']`
    AttrEq(String, String),
    /// `[contains(@a,'v')]`
    AttrContains(String, String),
}

#[derive(Debug, Clone, PartialEq)]
struct Step {
    axis: Axis,
    /// `None` = `*`
    name: Option<String>,
    preds: Vec<Pred>,
}

/// 把 XPath 表达式解析成步骤列表。解析不了就返回 None（调用方当成"选择器失效"）。
fn parse(expr: &str) -> Option<Vec<Step>> {
    let e = expr.trim();
    if e.is_empty() {
        return None;
    }
    let b: Vec<char> = e.chars().collect();
    let mut i = 0usize;
    let mut steps: Vec<Step> = Vec::new();

    // 开头的 `.` 只是"当前节点"，跳过（`.//a` / `./a`）
    if b.first() == Some(&'.') {
        i += 1;
    }
    // 第一个步骤的轴
    let mut axis = Axis::Child;
    if i + 1 < b.len() && b[i] == '/' && b[i + 1] == '/' {
        axis = Axis::Descendant;
        i += 2;
    } else if i < b.len() && b[i] == '/' {
        axis = Axis::Child;
        i += 1;
    } else {
        // 裸表达式（没有前导斜杠）当作相对当前节点的子/后代选择
        axis = Axis::Child;
    }

    loop {
        // 读节点名
        let start = i;
        while i < b.len() && b[i] != '/' && b[i] != '[' {
            i += 1;
        }
        let raw: String = b[start..i].iter().collect();
        let raw = raw.trim().to_string();
        let name = if raw.is_empty() || raw == "*" {
            None
        } else {
            Some(raw.to_lowercase())
        };

        // 读谓词
        let mut preds = Vec::new();
        while i < b.len() && b[i] == '[' {
            // 找配对的 ]
            let mut depth = 0i32;
            let pstart = i + 1;
            let mut j = i;
            while j < b.len() {
                if b[j] == '[' {
                    depth += 1;
                } else if b[j] == ']' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                j += 1;
            }
            if j >= b.len() {
                return None; // 括号不配对
            }
            let inner: String = b[pstart..j].iter().collect();
            preds.push(parse_pred(&inner)?);
            i = j + 1;
        }

        steps.push(Step { axis, name, preds });

        // 下一个步骤
        if i >= b.len() {
            break;
        }
        if b[i] != '/' {
            return None;
        }
        if i + 1 < b.len() && b[i + 1] == '/' {
            axis = Axis::Descendant;
            i += 2;
        } else {
            axis = Axis::Child;
            i += 1;
        }
        if i >= b.len() {
            return None; // 以 / 结尾
        }
    }

    if steps.is_empty() {
        None
    } else {
        Some(steps)
    }
}

fn parse_pred(s: &str) -> Option<Pred> {
    let t = s.trim();
    // [n]
    if let Ok(n) = t.parse::<usize>() {
        return Some(Pred::Position(n.max(1)));
    }
    // [contains(@a,'v')]
    if let Some(rest) = t.strip_prefix("contains(") {
        let rest = rest.strip_suffix(')')?;
        let (a, v) = split_args(rest)?;
        let a = a.trim().strip_prefix('@')?.to_string();
        return Some(Pred::AttrContains(a.to_lowercase(), v));
    }
    // [@a='v']  /  [@a="v"]
    let rest = t.strip_prefix('@')?;
    let (a, v) = rest.split_once('=')?;
    let a = a.trim().to_lowercase();
    let v = v.trim();
    let v = v.strip_prefix('\'').and_then(|x| x.strip_suffix('\''))
        .or_else(|| v.strip_prefix('"').and_then(|x| x.strip_suffix('"')))
        .unwrap_or(v);
    Some(Pred::AttrEq(a, v.to_string()))
}

/// 拆 `@class,'a b'` 这种参数（逗号不在引号里才算分隔）
fn split_args(s: &str) -> Option<(String, String)> {
    let mut quote: Option<char> = None;
    for (idx, c) in s.char_indices() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                } else if c == ',' {
                    let a = s[..idx].trim().to_string();
                    let mut b = s[idx + 1..].trim().to_string();
                    b = b.strip_prefix('\'').and_then(|x| x.strip_suffix('\''))
                        .or_else(|| b.strip_prefix('"').and_then(|x| x.strip_suffix('"')))
                        .unwrap_or(&b)
                        .to_string();
                    return Some((a, b));
                }
            }
        }
    }
    None
}

/// 在 `ctx` 上求值，返回匹配到的元素（文档序）。
pub fn eval<'a>(ctx: ElementRef<'a>, expr: &str) -> Vec<ElementRef<'a>> {
    let Some(steps) = parse(expr) else {
        return Vec::new();
    };
    let mut cur: Vec<ElementRef<'a>> = vec![ctx];

    for step in &steps {
        let mut cand: Vec<ElementRef<'a>> = Vec::new();
        for node in &cur {
            match step.axis {
                Axis::Child => cand.extend(node.child_elements()),
                Axis::Descendant => cand.extend(node.descendent_elements()),
            }
        }
        // 名称过滤
        if let Some(n) = &step.name {
            cand.retain(|el| el.value().name().eq_ignore_ascii_case(n));
        }
        // 谓词：属性类先过（逐节点），位置类最后（要按父分组）
        let mut pos: Vec<usize> = Vec::new();
        for (pi, p) in step.preds.iter().enumerate() {
            match p {
                Pred::AttrEq(a, v) => cand.retain(|el| {
                    el.value().attr(a).map(|x| x == v).unwrap_or(false)
                }),
                Pred::AttrContains(a, v) => cand.retain(|el| {
                    el.value().attr(a).map(|x| x.contains(v.as_str())).unwrap_or(false)
                }),
                Pred::Position(_) => pos.push(pi),
            }
        }
        for pi in pos {
            let Pred::Position(n) = step.preds[pi] else { continue };
            // ★ 按父节点分组，组内取第 n 个
            let mut out: Vec<ElementRef<'a>> = Vec::new();
            let mut idx = 0usize;
            while idx < cand.len() {
                let parent = cand[idx].parent().and_then(ElementRef::wrap);
                let mut group: Vec<ElementRef<'a>> = vec![cand[idx]];
                let mut k = idx + 1;
                while k < cand.len() {
                    let p2 = cand[k].parent().and_then(ElementRef::wrap);
                    if same_node(parent, p2) {
                        group.push(cand[k]);
                        k += 1;
                    } else {
                        break;
                    }
                }
                if let Some(el) = group.get(n - 1) {
                    out.push(*el);
                }
                idx = k;
            }
            cand = out;
        }
        cur = cand;
        if cur.is_empty() {
            break;
        }
    }
    cur
}

/// 两个 Option<ElementRef> 是不是同一个节点
fn same_node(a: Option<ElementRef<'_>>, b: Option<ElementRef<'_>>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x.id() == y.id(),
        (None, None) => true,
        _ => false,
    }
}

/// 取元素文本（折叠空白）
pub fn text_of(el: &ElementRef<'_>) -> String {
    collapse_ws(&el.text().collect::<Vec<_>>().join(" "))
}

/// 取属性
pub fn attr_of(el: &ElementRef<'_>, name: &str) -> String {
    el.value().attr(name).unwrap_or("").to_string()
}

pub fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut sp = false;
    for c in s.chars() {
        if c.is_whitespace() {
            sp = true;
        } else {
            if sp && !out.is_empty() {
                out.push(' ');
            }
            sp = false;
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use scraper::Html;

    fn root(html: &str) -> Html {
        Html::parse_document(html)
    }
    fn top<'a>(h: &'a Html) -> ElementRef<'a> {
        h.root_element()
    }

    #[test]
    fn parses_the_grammar_subset() {
        let s = parse("//div[2]/div[1]/a/strong").unwrap();
        assert_eq!(s.len(), 4);
        assert_eq!(s[0].axis, Axis::Descendant);
        assert_eq!(s[0].name.as_deref(), Some("div"));
        assert_eq!(s[0].preds, vec![Pred::Position(2)]);
        assert_eq!(s[3].name.as_deref(), Some("strong"));

        let s = parse(".//a[contains(@class,'module-play-list-link')]").unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].axis, Axis::Descendant);
        assert_eq!(s[0].preds, vec![Pred::AttrContains("class".into(), "module-play-list-link".into())]);

        let s = parse("//*[@id='main0']/div/ul").unwrap();
        assert_eq!(s[0].name, None, "* 应该是 None");
        assert_eq!(s[0].preds, vec![Pred::AttrEq("id".into(), "main0".into())]);

        let s = parse("//ul[@class='anthology-list-play size']").unwrap();
        assert_eq!(s[0].preds, vec![Pred::AttrEq("class".into(), "anthology-list-play size".into())]);

        assert!(parse("//div[").is_none());
        assert!(parse("").is_none());
    }

    /// ★ 位置谓词必须按父分组：这里有两个父节点，各自都该选出第 1 个 div
    #[test]
    fn positional_predicate_groups_by_parent() {
        let h = root(
            "<html><body>
               <section><div>a1</div><div>a2</div></section>
               <section><div>b1</div><div>b2</div></section>
             </body></html>",
        );
        let got = eval(top(&h), "//section/div[1]");
        let txt: Vec<String> = got.iter().map(|e| text_of(e)).collect();
        assert_eq!(txt, vec!["a1", "b1"], "每个 section 各取第 1 个 div");

        let got = eval(top(&h), "//section/div[2]");
        let txt: Vec<String> = got.iter().map(|e| text_of(e)).collect();
        assert_eq!(txt, vec!["a2", "b2"]);
    }

    #[test]
    fn attribute_predicates_and_wildcard() {
        let h = root(
            r#"<html><body>
                 <div id="main0"><div><ul><li><a href="/p/1">一</a></li></ul></div></div>
                 <div id="other"><ul><li><a href="/x">别</a></li></ul></div>
               </body></html>"#,
        );
        let got = eval(top(&h), "//*[@id='main0']/div/ul");
        assert_eq!(got.len(), 1);
        let links = eval(got[0], ".//a");
        assert_eq!(links.len(), 1);
        assert_eq!(attr_of(&links[0], "href"), "/p/1");

        let cls = root(
            r#"<html><body>
                 <a class="module-play-list-link active" href="/e/1">第一话</a>
                 <a class="module-play-list-link" href="/e/2">第二话</a>
                 <a class="other" href="/e/3">广告</a>
               </body></html>"#,
        );
        let got = eval(top(&cls), "//a[contains(@class,'module-play-list-link')]");
        assert_eq!(got.len(), 2, "第三个 class 不含关键词，应该被排除");
        assert_eq!(text_of(&got[0]), "第一话");
    }

    /// 相对上下文求值：searchName 是相对每条搜索结果写的
    #[test]
    fn evaluates_relative_to_context() {
        // 贴近真实规则：条目下第一个 div 是封面，第二个 div 里才是标题链接
        let h = root(
            r#"<html><body>
                 <div class="item"><div class="pic"></div><div><h4><a href="/d/1">葬送的芙莉莲</a></h4></div></div>
                 <div class="item"><div class="pic"></div><div><h4><a href="/d/2">别的番</a></h4></div></div>
               </body></html>"#,
        );
        let items = eval(top(&h), "//div[@class='item']");
        assert_eq!(items.len(), 2);
        // 规则里 searchName = //div[2]/h4/a（相对条目）
        let names: Vec<String> = items
            .iter()
            .map(|it| {
                let n = eval(*it, "//div[2]/h4/a");
                n.first().map(|e| text_of(e)).unwrap_or_default()
            })
            .collect();
        assert_eq!(names, vec!["葬送的芙莉莲", "别的番"]);
        // searchResult = //h4/a，取其 href
        let hrefs: Vec<String> = items
            .iter()
            .map(|it| {
                let n = eval(*it, "//h4/a");
                n.first().map(|e| attr_of(e, "href")).unwrap_or_default()
            })
            .collect();
        assert_eq!(hrefs, vec!["/d/1", "/d/2"]);
        // 封面那个 div 不该被误选（它里面没有 h4）
        assert!(eval(items[0], "//div[1]/h4/a").is_empty());
    }

    /// 深路径 + `//` 混用（真实规则里很多）
    #[test]
    fn deep_path_with_descendant_axis() {
        let h = root(
            r#"<html><body>
                 <div><div><div><div>
                   <div><div><div><div><div><div><div><div>
                     <div><ul><li><a href="/e/1">1</a></li><li><a href="/e/2">2</a></li></ul></div>
                   </div></div></div></div></div></div></div></div>
                 </div></div></div></div>
               </body></html>"#,
        );
        // 规则里出现过这种超深路径
        let got = eval(top(&h), "//div/div/div/div/div/div/div/div/div/ul//li");
        assert_eq!(got.len(), 2, "拿到 li 才对");
    }

    #[test]
    fn bad_expression_yields_empty_not_panic() {
        let h = root("<html><body><p>x</p></body></html>");
        assert!(eval(top(&h), "//div[").is_empty());
        assert!(eval(top(&h), "").is_empty());
        assert!(eval(top(&h), "///").is_empty());
    }
}
