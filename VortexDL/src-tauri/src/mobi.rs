//! MOBI / PalmDOC 正文抽取。
//!
//! 为什么需要：苦瓜书盘上一大半中文书是 **mobi**（例如《窄门》），而阅读器原来只认
//! txt/epub —— 用户报「读不了 mobi 格式」。这里不引第三方 crate，自己按格式解。
//!
//! MOBI 本质是 Palm Database(PDB)，结构：
//!   · 偏移 60..68 是类型/创建者（正常是 "BOOKMOBI"）
//!   · 偏移 76 是记录数（u16 BE）；78 起是记录位置表，每条 8 字节（前 4 字节 = 文件偏移）
//!   · **记录 0 = 头**：PalmDOC 头(16 字节) + MOBI 头
//!       compression   u16 BE @0    1=不压缩 / 2=PalmDOC LZ77 / 17480=HUFF-CDC
//!       text_length   u32 BE @4    正文总字节数（解压后）
//!       record_count  u16 BE @8    正文记录条数
//!       record_size   u16 BE @10   每条正文解压后的最大字节数（通常 4096）
//!     MOBI 头从 @16 开始：magic "MOBI"；text_encoding u32 BE @28（65001=UTF-8 / 1252=cp1252）
//!   · **记录 1..=record_count** 是正文，按 record_size 切块；末尾还有 index/FLIS/FCIS/EOF 等记录
//!
//! PalmDOC LZ77 解压（**每条记录独立做**，字典不跨记录）：
//!   b == 0x00            → 输出 0x00，前进 1
//!   0x01 <= b <= 0x08    → 后随 b 个字节原样输出，前进 1+b
//!   0x09 <= b <= 0x7f    → 输出 b，前进 1
//!   0x80 <= b <= 0xbf    → 两字节：dist = ((b<<8 | next) >> 3) & 0x7FF，len = (b & 7) + 3
//!   b >= 0xC0            → 输出 空格 + (b ^ 0x80)，前进 1
//!
//! ★ 踩坑记录：正文记录尾部可能挂 "extra data"（AZW3/KF8 常见）。逐条按标志位算长度
//!   很容易算错，标准做法是**全部拼完再按 text_length 截断**，中间记录的脏尾在实际
//!   文件里基本不出现。

/// 解出来的是原始字节（编码可能是 UTF-8 也可能是 GBK），交给调用方按 HTML 解码。
pub fn mobi_text_bytes(raw: &[u8]) -> Result<Vec<u8>, String> {
    if raw.len() < 90 {
        return Err("文件太小，不像 mobi".into());
    }
    let magic = &raw[60..68];
    if magic != b"BOOKMOBI" {
        // 有些文件创建者是 "TEXtREAd"（纯 PalmDOC 文本），也认
        if magic != b"TEXtREAd" {
            return Err(format!(
                "不是 mobi/palmdoc（偏移 60 的类型是 {:?}）",
                String::from_utf8_lossy(magic)
            ));
        }
    }
    let nrec = be_u16(raw, 76) as usize;
    if nrec == 0 || raw.len() < 78 + nrec * 8 {
        return Err(format!("记录数异常：{nrec}"));
    }
    // 记录位置表
    let off_at = |i: usize| -> Option<usize> {
        let p = 78 + i * 8;
        if p + 4 > raw.len() {
            return None;
        }
        Some(be_u32(raw, p) as usize)
    };
    let rec = |i: usize| -> Option<&[u8]> {
        let a = off_at(i)?;
        let b = if i + 1 < nrec { off_at(i + 1)? } else { raw.len() };
        if a >= b || b > raw.len() {
            return None;
        }
        Some(&raw[a..b])
    };

    let hdr = rec(0).ok_or("读不到记录 0（文件头）")?;
    if hdr.len() < 24 {
        return Err("记录 0 太短".into());
    }
    let compression = be_u16(hdr, 0);
    let text_length = be_u32(hdr, 4) as usize;
    let text_records = be_u16(hdr, 8) as usize;
    if text_records == 0 {
        return Err("正文记录数为 0".into());
    }
    if compression == 17480 {
        return Err("这本 mobi 是 HUFF/CDIC 压缩（多见于亚马逊原版），暂不支持 —— 请点「下载」存到本地看".into());
    }
    if compression != 1 && compression != 2 {
        return Err(format!("不认识的 mobi 压缩方式：{compression}"));
    }

    let mut out: Vec<u8> = Vec::with_capacity(text_length.max(4096));
    for i in 1..=text_records {
        let Some(r) = rec(i) else { break };
        if compression == 1 {
            out.extend_from_slice(r);
        } else {
            palm_doc_decompress(r, &mut out);
        }
        // 已够长就不必再往后解（后面多半是索引记录）
        if text_length > 0 && out.len() >= text_length {
            break;
        }
    }
    if out.is_empty() {
        return Err("解压后正文是空的".into());
    }
    // ★ 末尾可能有 extra data，按声明的正文长度截断
    if text_length > 0 && text_length < out.len() {
        out.truncate(text_length);
    }
    Ok(out)
}

/// PalmDOC LZ77 解压一条记录，结果追加到 out（字典就是 out 本身，但**只在本次调用内**——
/// 所以调用方必须每条记录传一个"从该记录起点开始"的 out；实测各记录之间不共享字典）。
fn palm_doc_decompress(src: &[u8], out: &mut Vec<u8>) {
    let start = out.len();
    let mut i = 0usize;
    while i < src.len() {
        let b = src[i];
        match b {
            0x00 => {
                out.push(0);
                i += 1;
            }
            0x01..=0x08 => {
                let n = b as usize;
                let end = (i + 1 + n).min(src.len());
                out.extend_from_slice(&src[i + 1..end]);
                i = end;
            }
            0x09..=0x7f => {
                out.push(b);
                i += 1;
            }
            0x80..=0xbf => {
                if i + 1 >= src.len() {
                    break;
                }
                let pair = ((b as usize) << 8) | src[i + 1] as usize;
                let dist = (pair >> 3) & 0x07ff;
                let len = (pair & 0x07) + 3;
                i += 2;
                if dist == 0 {
                    continue;
                }
                // 从 out 里往回拷 len 个字节（逐字节拷，因为可能重叠）
                let mut p = out.len() - dist.min(out.len() - start);
                for _ in 0..len {
                    if p >= out.len() {
                        break;
                    }
                    let c = out[p];
                    out.push(c);
                    p += 1;
                }
            }
            _ => {
                // 0xc0..=0xff：空格 + (b & 0x7f)
                out.push(b' ');
                out.push(b & 0x7f);
                i += 1;
            }
        }
    }
}

fn be_u16(b: &[u8], off: usize) -> u16 {
    ((b[off] as u16) << 8) | b[off + 1] as u16
}
fn be_u32(b: &[u8], off: usize) -> u32 {
    ((b[off] as u32) << 24) | ((b[off + 1] as u32) << 16) | ((b[off + 2] as u32) << 8) | b[off + 3] as u32
}

// ============================================================================
// 测试
// ============================================================================
#[cfg(test)]
mod tests {
    use super::*;

    /// 手搓一个最小 MOBI：头 + 1 条不压缩正文记录
    fn fake_mobi(text: &[u8], compression: u16) -> Vec<u8> {
        let nrec = 2usize;
        let mut hdr = vec![0u8; 16 + 200];
        hdr[0..2].copy_from_slice(&compression.to_be_bytes());
        hdr[4..8].copy_from_slice(&(text.len() as u32).to_be_bytes());
        hdr[8..10].copy_from_slice(&1u16.to_be_bytes()); // 1 条正文
        hdr[10..12].copy_from_slice(&4096u16.to_be_bytes());
        hdr[16..20].copy_from_slice(b"MOBI");
        hdr[28..32].copy_from_slice(&65001u32.to_be_bytes());

        let mut out = vec![0u8; 78 + nrec * 8];
        out[60..68].copy_from_slice(b"BOOKMOBI");
        out[76..78].copy_from_slice(&(nrec as u16).to_be_bytes());
        let o0 = 78 + nrec * 8;
        let o1 = o0 + hdr.len();
        out[78..82].copy_from_slice(&(o0 as u32).to_be_bytes());
        out[86..90].copy_from_slice(&(o1 as u32).to_be_bytes());
        out.extend_from_slice(&hdr);
        out.extend_from_slice(text);
        out
    }

    #[test]
    fn uncompressed_mobi() {
        let t = b"<html><body><p>hello mobi</p></body></html>";
        let m = fake_mobi(t, 1);
        assert_eq!(mobi_text_bytes(&m).unwrap(), t.to_vec());
    }

    #[test]
    fn palmdoc_literals_and_space_runs() {
        // ★ 2026-10-10 修正测试（实现是对的，测试写错了）：
        //   PalmDOC 里 `0x01..=0x08` 这条指令的**长度就是它自己** ——
        //   `0x03` 表示"后随 3 个字节原样输出"，**不是**先来一个 0x01 再来个 0x03。
        //   原测试写成 `[0x01, 0x03, 'a','b','c', …]`，等于"1 个字面字节 0x03"
        //   再"3 个字面 a b c"，所以解出来会多一个前导 0x03。
        //   这两个用例一直没跑过（_tbook.bat 只跑 #[ignore] 的），所以没暴露。
        let src = vec![0x03, b'a', b'b', b'c', 0x09, b'x', 0xc1];
        let mut out = Vec::new();
        palm_doc_decompress(&src, &mut out);
        assert_eq!(out, b"abc\tx A".to_vec());
    }

    #[test]
    fn palmdoc_back_reference() {
        // 先写 "abcd"（0x04 = 后随 4 个字面字节），再放一个两字节回溯：
        // dist=4, len=4 → 复制 "abcd"
        // pair = (dist<<3) | (len-3) = (4<<3)|1 = 0x21；高位字节 0x80 | (0x21>>8) = 0x80
        let src = vec![0x04, b'a', b'b', b'c', b'd', 0x80, 0x21];
        let mut out = Vec::new();
        palm_doc_decompress(&src, &mut out);
        assert_eq!(out, b"abcdabcd".to_vec());
    }

    /// 0xc0..0xff = "空格 + (b ^ 0x80)"。★ 注意它**总是先出一个空格**，
    /// 所以单个空格是 0x20（普通字面），不是 0xc0（0xc0 会解成 "空格 + @"）。
    #[test]
    fn palmdoc_space_runs() {
        let src = vec![0x02, b'h', b'i', 0xc1, 0xe1];   // "hi" + " A" + " a"
        let mut out = Vec::new();
        palm_doc_decompress(&src, &mut out);
        assert_eq!(out, b"hi A a".to_vec());
    }

    #[test]
    fn rejects_huff() {
        let t = b"x";
        let m = fake_mobi(t, 17480);
        let e = mobi_text_bytes(&m).unwrap_err();
        assert!(e.contains("HUFF"), "{e}");
    }

    #[test]
    fn rejects_non_mobi() {
        let mut b = vec![0u8; 200];
        b[60..68].copy_from_slice(b"GIF89a  ");
        assert!(mobi_text_bytes(&b).is_err());
    }

    #[test]
    fn truncates_to_text_length() {
        // 声明正文只有 3 字节，但记录里多塞了尾巴（模拟 extra data）
        let mut m = fake_mobi(b"abcXXXX", 1);
        let p = 4; // text_length 在记录 0 内的偏移
        let rec0 = be_u32(&m, 78) as usize;
        m[rec0 + p..rec0 + p + 4].copy_from_slice(&3u32.to_be_bytes());
        assert_eq!(mobi_text_bytes(&m).unwrap(), b"abc".to_vec());
    }
}
