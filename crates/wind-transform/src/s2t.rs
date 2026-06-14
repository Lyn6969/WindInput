//! 简繁转换 (S2T)
//!
//! 与 Go 版本 `wind_input/internal/transform/s2t/` 对齐。
//! 读取 OpenCC `.octrie` 二进制词典，按转换链做最长前缀匹配替换。
//!
//! .octrie 格式：Header(16B: Magic "WIOC", Version u32, Count u32, MaxKeyB u16, Reserved u16)
//! + Entries(Count×12B: KeyOff u32, KeyLen u16, ValOff u32, ValLen u16，按 key 升序)
//! + StringTable(UTF-8 字节池)。

use std::path::Path;

const MAGIC: &[u8; 4] = b"WIOC";
const HEADER_SIZE: usize = 16;
const ENTRY_SIZE: usize = 12;

struct Entry {
    key_off: u32,
    key_len: u16,
    val_off: u32,
    val_len: u16,
}

/// 单个 OpenCC 词典：紧凑字节池 + 有序 entry 数组，支持二分查找与最长前缀匹配。
pub struct Dict {
    entries: Vec<Entry>,
    strings: Vec<u8>,
    max_key_len: usize,
}

impl Dict {
    /// 从字节切片解析 .octrie。
    pub fn parse(data: &[u8]) -> Option<Dict> {
        if data.len() < HEADER_SIZE || &data[0..4] != MAGIC {
            return None;
        }
        let version = u32::from_le_bytes(data[4..8].try_into().ok()?);
        if version != 1 {
            return None;
        }
        let count = u32::from_le_bytes(data[8..12].try_into().ok()?) as usize;
        let max_key = u16::from_le_bytes(data[12..14].try_into().ok()?) as usize;

        let entries_end = HEADER_SIZE + count * ENTRY_SIZE;
        if entries_end > data.len() {
            return None;
        }
        let mut entries = Vec::with_capacity(count);
        let mut off = HEADER_SIZE;
        for _ in 0..count {
            entries.push(Entry {
                key_off: u32::from_le_bytes(data[off..off + 4].try_into().ok()?),
                key_len: u16::from_le_bytes(data[off + 4..off + 6].try_into().ok()?),
                val_off: u32::from_le_bytes(data[off + 6..off + 10].try_into().ok()?),
                val_len: u16::from_le_bytes(data[off + 10..off + 12].try_into().ok()?),
            });
            off += ENTRY_SIZE;
        }
        let strings = data[entries_end..].to_vec();
        Some(Dict {
            entries,
            strings,
            max_key_len: max_key,
        })
    }

    /// 从文件加载。
    pub fn load(path: &Path) -> Option<Dict> {
        let data = std::fs::read(path).ok()?;
        Self::parse(&data)
    }

    fn key_of(&self, i: usize) -> &[u8] {
        let e = &self.entries[i];
        &self.strings[e.key_off as usize..e.key_off as usize + e.key_len as usize]
    }

    fn val_of(&self, i: usize) -> &[u8] {
        let e = &self.entries[i];
        &self.strings[e.val_off as usize..e.val_off as usize + e.val_len as usize]
    }

    fn lookup(&self, key: &[u8]) -> Option<&[u8]> {
        match self
            .entries
            .binary_search_by(|e| self.strings[e.key_off as usize..e.key_off as usize + e.key_len as usize].cmp(key))
        {
            Ok(i) => Some(self.val_of(i)),
            Err(_) => None,
        }
    }

    /// 在 input 起点找最长 key 命中，返回 (匹配字节数, value)。
    fn longest_prefix(&self, input: &[u8]) -> Option<(usize, &[u8])> {
        if input.is_empty() || self.max_key_len == 0 {
            return None;
        }
        let max_l = self.max_key_len.min(input.len());
        for l in (1..=max_l).rev() {
            if let Some(val) = self.lookup(&input[..l]) {
                return Some((l, val));
            }
        }
        None
    }
}

/// 转换器：串行多步，每步一组词典（OpenCC group 语义：组内取最长匹配）。
pub struct Converter {
    steps: Vec<Vec<Dict>>,
}

impl Converter {
    /// 按变体从 opencc 目录加载转换链。无可用词典返回 None。
    pub fn load_variant(opencc_dir: &Path, variant: &str) -> Option<Converter> {
        let chain = chain_for(variant);
        let mut steps = Vec::new();
        for group_names in chain {
            let mut group = Vec::new();
            for name in group_names {
                let path = opencc_dir.join(format!("{}.octrie", name));
                if let Some(d) = Dict::load(&path) {
                    group.push(d);
                }
            }
            if !group.is_empty() {
                steps.push(group);
            }
        }
        if steps.is_empty() {
            None
        } else {
            Some(Converter { steps })
        }
    }

    /// 执行一次完整链路转换。
    pub fn convert(&self, s: &str) -> String {
        if s.is_empty() || self.steps.is_empty() {
            return s.to_string();
        }
        let mut cur = s.as_bytes().to_vec();
        for group in &self.steps {
            cur = apply_step(group, &cur);
        }
        String::from_utf8(cur).unwrap_or_else(|_| s.to_string())
    }
}

/// 变体 → 转换链（词典名分组）。
fn chain_for(variant: &str) -> Vec<Vec<&'static str>> {
    match variant.to_lowercase().as_str() {
        "s2tw" | "tw" | "taiwan" => {
            vec![vec!["STPhrases", "STCharacters"], vec!["TWVariants"]]
        }
        "s2twp" | "twp" => vec![
            vec!["STPhrases", "STCharacters"],
            vec!["TWPhrases"],
            vec!["TWVariants"],
        ],
        "s2hk" | "hk" | "hongkong" => {
            vec![vec!["STPhrases", "STCharacters"], vec!["HKVariants"]]
        }
        // s2t 标准
        _ => vec![vec!["STPhrases", "STCharacters"]],
    }
}

/// 用一组词典做最长前缀匹配替换扫描。
fn apply_step(group: &[Dict], input: &[u8]) -> Vec<u8> {
    if group.is_empty() || input.is_empty() {
        return input.to_vec();
    }
    let mut out = Vec::with_capacity(input.len() + 8);
    let mut i = 0;
    while i < input.len() {
        if let Some((n, val)) = group_longest_prefix(group, &input[i..]) {
            out.extend_from_slice(val);
            i += n;
        } else {
            let step = utf8_step(input[i]);
            let end = (i + step).min(input.len());
            out.extend_from_slice(&input[i..end]);
            i = end;
        }
    }
    out
}

/// 组内各词典取最长匹配，跨成员选最长。
fn group_longest_prefix<'a>(group: &'a [Dict], input: &[u8]) -> Option<(usize, &'a [u8])> {
    let mut best: Option<(usize, &'a [u8])> = None;
    for d in group {
        if let Some((n, val)) = d.longest_prefix(input) {
            if best.map_or(true, |(bl, _)| n > bl) {
                best = Some((n, val));
            }
        }
    }
    best
}

fn utf8_step(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b < 0xC0 {
        1 // 错误中间字节，按 1 跳过避免死循环
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn opencc_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build_debug/data/opencc")
    }

    #[test]
    fn test_s2t_standard_conversion() {
        let dir = opencc_dir();
        if !dir.join("STCharacters.octrie").exists() {
            eprintln!("跳过：缺少 opencc 数据");
            return;
        }
        let conv = Converter::load_variant(&dir, "s2t").expect("应加载 s2t 链");
        // 简体 → 繁体（字级）
        assert_eq!(conv.convert("汉字"), "漢字");
        assert_eq!(conv.convert("简体转换"), "簡體轉換");
        // 词级最长匹配（软件 → 軟件，标准 s2t 不转台湾习惯词）
        let r = conv.convert("计算机");
        assert!(r.chars().count() == 3, "长度应保持，实际: {}", r);
    }

    #[test]
    fn test_s2t_preserves_non_chinese() {
        let dir = opencc_dir();
        if !dir.join("STCharacters.octrie").exists() {
            return;
        }
        let conv = Converter::load_variant(&dir, "s2t").unwrap();
        assert_eq!(conv.convert("abc123"), "abc123");
        assert_eq!(conv.convert("hello 世界"), "hello 世界");
    }
}
