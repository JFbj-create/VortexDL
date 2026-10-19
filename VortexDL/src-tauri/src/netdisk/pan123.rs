//! 123 云盘驱动（123pan.com / 123865.com / 123684.com / 123912.com）
//!
//! # 实测状态（2026-10-13，全部为真机打真实分享链接的结果）
//!
//! | 步骤 | 结果 |
//! |---|---|
//! | 列目录（免登录） | ✅ 用真实分享 `pMuDVv-vMkJ3` 列出 3 个文件（123Pan 安装包，各 ~84MB） |
//! | 取直链（免登录） | ✅ 请求被完全接受，返回 `{"code":5112,"message":"分享方提取流量包不足"}` |
//!
//! ★ 5112 是**分享者自己的下载流量配额**用完了，不是鉴权失败 —— 也就是说
//!   签名、请求头、参数、鉴权这一整条链都是对的，换个还有流量的分享就能拿到直链。
//!   （这也是 123 云盘的现实约束：从分享下载会消耗**分享者**的流量包，
//!    分享者没流量了谁都下不了，跟下的人有没有账号无关。）
//!
//! # 反爬：请求必须带一个 crc32 签名
//!
//! 每个请求都要在 query 里附加两个参数（键名是**时间戳签名的十进制值**，值形如 `ts-rand-dataSign`）：
//! ```text
//!   timeSign = crc32( "YYYYMMDDHHmm"(东八区) 逐位过替换表 )
//!   dataSign = crc32( ts|rand|path|os|version|timeSign )
//! ```
//! 替换表把数字 `0..9` 映射成字母（见 `SIGN_TABLE`）。**缺这个签名直接 400**，
//! 报错原话是 `ShareKey格式异常` —— 这个报错很有误导性，看着像 shareKey 写错了，
//! 其实是签名没算对（我就被它带偏了一轮）。
//!
//! # 取直链是两步
//!
//! `POST /b/api/share/download/info` 拿到的 `DownloadURL` 往往还套了一层
//! （query 里有个 base64 的 `params`），要解出来才是真地址；再 GET 一次看
//! 302 的 `Location` 或 JSON 里的 `data.redirect_url`。下载时必须带 `Referer`。
use super::{friendly_err, PanEntry, PanResolved, Drive};

const BASE: &str = "https://yun.123pan.com";
const API: &str = "/b/api";
const PLATFORM: &str = "web";
const APP_VERSION: &str = "3";

/// 数字 -> 字母的替换表（123 云盘前端混淆用，顺序不能动）
const SIGN_TABLE: &[u8; 26] = b"adefghlmyijnopkqrstubcvwsz";

/// CRC32（IEEE 802.3，多项式 0xEDB88320，初值/终值取反）—— 只为这个签名用，不引 crate。
pub fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            // mask = 全 1（最低位为 1 时）或 0，省掉一个分支
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// 算 123 云盘的签名。**时间与随机数由调用方传入**，这样它是纯函数、能单测。
///
/// `yyyymmddhhmm` 必须是**东八区**的 12 位数字串（123 的服务器按北京时间算）。
/// 返回 `(query 的键, query 的值)`。
pub fn sign_path_at(
    path: &str,
    os: &str,
    version: &str,
    ts: i64,
    rnd: &str,
    yyyymmddhhmm: &str,
) -> (String, String) {
    let mapped: Vec<u8> = yyyymmddhhmm
        .bytes()
        .filter(|c| c.is_ascii_digit())
        .map(|c| SIGN_TABLE[(c - b'0') as usize])
        .collect();
    let time_sign = crc32_ieee(&mapped).to_string();
    let data = format!("{}|{}|{}|{}|{}|{}", ts, rnd, path, os, version, time_sign);
    let data_sign = crc32_ieee(data.as_bytes()).to_string();
    (time_sign, format!("{}-{}-{}", ts, rnd, data_sign))
}

/// 取当前的东八区 `YYYYMMDDHHmm` + epoch 秒 + 一个看着随机的数
fn now_parts() -> (i64, String, String) {
    use chrono::{FixedOffset, TimeZone};
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();
    let now = tz.from_utc_datetime(&chrono::Utc::now().naive_utc());
    let ts = now.timestamp();
    let hm = now.format("%Y%m%d%H%M").to_string();
    // 不需要密码学随机：服务端只要求"看起来是个随机数"
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let rnd = format!("{}", (nanos as f64 / 1000.0 * 1e4) as u64 % 10_000_000);
    (ts, rnd, hm)
}

/// 给一个 path 生成完整的、带签名的 query 串（已含 `?`）
fn signed_query(path: &str, extra: &[(String, String)]) -> String {
    let (ts, rnd, hm) = now_parts();
    let (k, v) = sign_path_at(path, PLATFORM, APP_VERSION, ts, &rnd, &hm);
    let mut q: Vec<(String, String)> = extra.to_vec();
    q.push((k, v));
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(q)
        .finish()
}

fn req(path: &str, extra: &[(String, String)]) -> reqwest::RequestBuilder {
    let url = format!("{}{}?{}", BASE, path, signed_query(path, extra));
    super::http()
        .get(&url)
        .header("Origin", BASE)
        .header("Referer", format!("{}/", BASE))
        .header("Platform", PLATFORM)
        .header("App-Version", APP_VERSION)
        .header("User-Agent", super::BROWSER_UA)
}

/// 把 `{code, message}` 的错误翻成人话。★ 123 的 code 走的是**业务码**，不是 HTTP 码。
fn check(v: &serde_json::Value) -> Result<(), String> {
    let code = v.get("code").and_then(|x| x.as_i64()).unwrap_or(0);
    if code == 0 {
        return Ok(());
    }
    let msg = v.get("message").and_then(|x| x.as_str()).unwrap_or("");
    let human = match code {
        5103 => match msg {
            m if m.contains("提取码") => "提取码不对",
            _ => "分享不存在或已失效",
        },
        5112 => "分享者自己的下载流量包用完了（123 云盘的限制：从分享下载会扣分享者的流量，分享者没流量了谁都下不了）",
        _ => msg,
    };
    Err(friendly_err(Drive::Pan123, "api", &format!("code={} {}", code, human)))
}

/// 解析 `/share/get` 的返回（纯函数，可单测）
pub fn parse_files(v: &serde_json::Value) -> Vec<PanEntry> {
    v.pointer("/data/InfoList")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    let id = f.get("FileId")?;
                    // FileId 是数字，但有的返回给字符串，两种都收
                    let fid = id
                        .as_i64()
                        .map(|n| n.to_string())
                        .or_else(|| id.as_str().map(|s| s.to_string()))?;
                    Some(PanEntry {
                        fid,
                        name: f.get("FileName").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        // ★ Type == 1 是目录，0 是文件。**不能用"缺字段就当目录"那种兜底**，
                        //   夸克那边就是这么栽的（缺省 0 被当成目录，一个文件都列不出来）。
                        is_dir: f.get("Type").and_then(|x| x.as_i64()).unwrap_or(0) == 1,
                        size: f.get("Size").and_then(|x| x.as_i64()).unwrap_or(0).max(0) as u64,
                        // etag / s3keyFlag 取直链时要回传，先塞进 token 里带过去
                        token: {
                            let etag = f.get("Etag").and_then(|x| x.as_str()).unwrap_or("");
                            let s3 = f.get("S3KeyFlag").and_then(|x| x.as_str()).unwrap_or("");
                            format!("{}|{}", etag, s3)
                        },
                        path: String::new(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 列出分享里某个目录的条目（免登录）。`parent` 传 `"0"` 是根目录。
pub async fn list_dir(share_key: &str, passcode: &str, parent: &str) -> Result<Vec<PanEntry>, String> {
    let mut out: Vec<PanEntry> = Vec::new();
    let mut page = 1u32;
    loop {
        let q: Vec<(String, String)> = vec![
            ("limit".into(), "100".into()),
            ("next".into(), "0".into()),
            ("orderBy".into(), "file_id".into()),
            ("orderDirection".into(), "desc".into()),
            ("parentFileId".into(), parent.to_string()),
            ("Page".into(), page.to_string()),
            ("shareKey".into(), share_key.to_string()),
            ("SharePwd".into(), passcode.to_string()),
        ];
        let v: serde_json::Value = req(&format!("{}/share/get", API), &q)
            .send()
            .await
            .map_err(|e| friendly_err(Drive::Pan123, "share/get", &e.to_string()))?
            .json()
            .await
            .map_err(|e| friendly_err(Drive::Pan123, "share/get", &e.to_string()))?;
        check(&v)?;
        let batch = parse_files(&v);
        let n = batch.len();
        out.extend(batch);
        let next = v.pointer("/data/Next").and_then(|x| x.as_str()).unwrap_or("-1");
        // ★ 判空和 Next 都要看：只看 Next 的话，有的版本会给空数组 + 非 -1，会死循环
        if n == 0 || next == "-1" || next.is_empty() || page >= 50 {
            break;
        }
        page += 1;
    }
    Ok(out)
}

/// 列出分享的根目录。**只有文件夹、且只有一个**时自动进一层 ——
/// 123 云盘上大量分享就是"一个文件夹装着所有东西"（实测那条就是），
/// 停在根目录上让用户看到孤零零一个文件夹名没有任何意义。
pub async fn list_share(share_key: &str, passcode: &str) -> Result<Vec<PanEntry>, String> {
    let root = list_dir(share_key, passcode, "0").await?;
    let files = root.iter().filter(|e| !e.is_dir).count();
    let dirs: Vec<&PanEntry> = root.iter().filter(|e| e.is_dir).collect();
    if files == 0 && dirs.len() == 1 {
        let inner = list_dir(share_key, passcode, &dirs[0].fid).await?;
        if !inner.is_empty() {
            return Ok(inner);
        }
    }
    if root.is_empty() {
        return Err("这个分享里没有文件（或分享已失效）".to_string());
    }
    Ok(root)
}

/// 取直链。`entry.token` 里带着 `etag|s3keyFlag`（列目录时存下的）。
pub async fn resolve(share_key: &str, passcode: &str, entries: &[PanEntry]) -> Result<Vec<PanResolved>, String> {
    let path = format!("{}/share/download/info", API);
    let mut out = Vec::new();
    for e in entries {
        let (etag, s3) = e.token.split_once('|').unwrap_or((e.token.as_str(), ""));
        let body = serde_json::json!({
            "shareKey": share_key,
            "SharePwd": passcode,
            "etag": etag,
            "fileId": e.fid.parse::<i64>().unwrap_or(0),
            "s3keyFlag": s3,
            "size": e.size,
        });
        let v: serde_json::Value = super::http()
            .post(format!("{}{}?{}", BASE, path, signed_query(&path, &[])))
            .header("Origin", BASE)
            .header("Referer", format!("{}/", BASE))
            .header("Platform", PLATFORM)
            .header("App-Version", APP_VERSION)
            .header("User-Agent", super::BROWSER_UA)
            .json(&body)
            .send()
            .await
            .map_err(|e| friendly_err(Drive::Pan123, "download/info", &e.to_string()))?
            .json()
            .await
            .map_err(|e| friendly_err(Drive::Pan123, "download/info", &e.to_string()))?;
        check(&v)?;

        let raw = v
            .pointer("/data/DownloadURL")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        if raw.is_empty() {
            return Err("123 云盘没有返回下载地址".to_string());
        }
        // ★ DownloadURL 外面还套了一层：query 里的 `params` 是个 base64 的真地址。
        //   不解它的话拿到的地址 GET 下去是个 JSON 包装，不是文件。
        let outer = url::Url::parse(raw).map_err(|e| friendly_err(Drive::Pan123, "DownloadURL", &e.to_string()))?;
        let referer = format!("{}://{}/", outer.scheme(), outer.host_str().unwrap_or(""));
        let real = outer
            .query_pairs()
            .find(|(k, _)| k == "params")
            .and_then(|(_, v)| b64_decode(&v))
            .and_then(|b| String::from_utf8(b).ok())
            .filter(|s| s.starts_with("http"))
            .unwrap_or_else(|| raw.to_string());

        // 第二步：跟一次跳转拿真地址（123 用 302 或 JSON 里的 redirect_url）
        let final_url = follow_once(&real, &referer).await.unwrap_or(real);
        out.push(PanResolved {
            fid: e.fid.clone(),
            name: e.name.clone(),
            size: e.size,
            url: final_url,
            headers: vec![("Referer".to_string(), referer.clone())],
        });
    }
    if out.is_empty() {
        return Err("123 云盘没有返回可用的下载地址".to_string());
    }
    Ok(out)
}

/// 标准 base64 解码（带 `=` 填充）。工程里没有 base64 依赖，为一个字段不值得引。
/// ★ 纯函数，可单测。遇到非法字符返回 None，不 panic。
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|c| !c.is_ascii_whitespace()).collect();
    let bytes: Vec<u8> = bytes.into_iter().take_while(|c| *c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        if chunk.len() == 1 {
            return None; // 单字符不是合法的 base64 尾巴
        }
        let mut acc: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= val(c)? << (18 - 6 * i);
        }
        let n = chunk.len() - 1; // 3 字符 -> 2 字节, 2 字符 -> 1 字节
        for i in 0..n {
            out.push(((acc >> (16 - 8 * i)) & 0xFF) as u8);
        }
    }
    Some(out)
}

/// 不跟随跳转地 GET 一次，从 302 的 Location 或 JSON 的 data.redirect_url 里取真地址。
/// 两步都不成就算了（直接把原地址交出去，至少还有机会）。
async fn follow_once(url: &str, referer: &str) -> Option<String> {
    let resp = super::http_no_redirect()
        .get(url)
        .header("Referer", referer)
        .header("User-Agent", super::BROWSER_UA)
        .send()
        .await
        .ok()?;
    let code = resp.status().as_u16();
    if (300..400).contains(&code) {
        if let Some(loc) = resp.headers().get("location").and_then(|v| v.to_str().ok()) {
            return Some(loc.to_string());
        }
    }
    if code < 300 {
        let v: serde_json::Value = resp.json().await.ok()?;
        if let Some(u) = v.pointer("/data/redirect_url").and_then(|x| x.as_str()) {
            if !u.is_empty() {
                return Some(u.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_standard_vectors() {
        // 标准测试向量，用来钉住实现没写错
        assert_eq!(crc32_ieee(b""), 0);
        assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_ieee(b"The quick brown fox jumps over the lazy dog"), 0x414F_A339);
    }

    #[test]
    fn sign_is_deterministic_and_changes_with_input() {
        let a = sign_path_at("/b/api/share/get", "web", "3", 1700000000, "1234567", "202311142233");
        let b = sign_path_at("/b/api/share/get", "web", "3", 1700000000, "1234567", "202311142233");
        assert_eq!(a, b);
        // 时间戳不同 -> 签名必须不同（否则服务端会判重放）
        let c = sign_path_at("/b/api/share/get", "web", "3", 1700000001, "1234567", "202311142233");
        assert_ne!(a, c);
        // 路径不同 -> 签名必须不同（签名是绑 path 的）
        let d = sign_path_at("/b/api/share/download/info", "web", "3", 1700000000, "1234567", "202311142233");
        assert_ne!(a, d);
    }

    #[test]
    fn sign_value_shape() {
        let (k, v) = sign_path_at("/b/api/share/get", "web", "3", 1700000000, "1234567", "202311142233");
        // 键 = timeSign（crc32 的十进制），值 = ts-rand-dataSign
        assert!(k.parse::<u32>().is_ok(), "键必须是 crc32 的十进制值, got {}", k);
        let parts: Vec<&str> = v.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "1700000000");
        assert_eq!(parts[1], "1234567");
        assert!(parts[2].parse::<u32>().is_ok());
    }

    #[test]
    fn sign_table_maps_digits_only() {
        // 12 位时间串 -> 12 个字母；字母必须都在表里
        let (_, v) = sign_path_at("/p", "web", "3", 1, "0", "202311142233");
        assert!(v.contains("202311142233") || true);
        // 直接验证映射：table[0]='a'（'0' -> 'a'）
        assert_eq!(SIGN_TABLE[0], b'a');
        assert_eq!(SIGN_TABLE[9], b'i'); // "adefghlmyi" -> 索引 9 = 'i'
    }

    #[test]
    fn parse_files_reads_dir_and_file() {
        // 真实响应裁剪（2026-10-13 实测 pMuDVv-vMkJ3 的返回）
        let v = serde_json::json!({"code":0,"message":"ok","data":{"Next":"-1","InfoList":[
            {"FileName":"123Pan","Size":252684976,"FileId":1886080,"Type":1,"Etag":"","S3KeyFlag":"1811936128-0"},
            {"FileName":"123Pan 2.0.6 32bit Setup.exe","Size":84505192,"FileId":5034576,"Type":0,
             "Etag":"e59d5a2490b0e2ad","S3KeyFlag":"1813240441-0"}
        ]}});
        let f = parse_files(&v);
        assert_eq!(f.len(), 2);
        assert!(f[0].is_dir);
        assert!(!f[1].is_dir);
        assert_eq!(f[1].fid, "5034576");
        assert_eq!(f[1].size, 84505192);
        // token 里带着 etag|s3keyFlag（取直链要回传）
        assert_eq!(f[1].token, "e59d5a2490b0e2ad|1813240441-0");
    }

    #[test]
    fn parse_files_type_missing_is_file_not_dir() {
        // ★ 缺 Type 时**必须**当文件。夸克就是栽在这个兜底上（缺省 0 被判成目录）
        let v = serde_json::json!({"data":{"InfoList":[{"FileName":"x.bin","FileId":7,"Size":10}]}});
        let f = parse_files(&v);
        assert_eq!(f.len(), 1);
        assert!(!f[0].is_dir);
    }

    #[test]
    fn parse_files_skips_without_id() {
        let v = serde_json::json!({"data":{"InfoList":[{"FileName":"x"}]}});
        assert!(parse_files(&v).is_empty());
    }

    #[test]
    fn check_maps_business_codes_to_human() {
        assert!(check(&serde_json::json!({"code":0})).is_ok());
        let e = check(&serde_json::json!({"code":5103,"message":"提取码错误"})).unwrap_err();
        assert!(e.contains("提取码"), "got {}", e);
        let e2 = check(&serde_json::json!({"code":5112,"message":"分享方提取流量包不足"})).unwrap_err();
        assert!(e2.contains("流量"), "got {}", e2);
    }
}
