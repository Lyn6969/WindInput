//! 单字反查表与五笔词组取码。
//!
//! 两个用途：修复 extra 里被错填的 code 列，以及给自定义词表反查编码。

use crate::config::Config;
use crate::entry::Entry;
use crate::weight::Unigram;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

pub type CharCodes = HashMap<char, String>;

/// 从 jidian 单字条目建 汉字 → 首选编码 的反查表。
///
/// 每字取权重最高的编码（极点词库里 weight=30 的是首选全码）。
pub fn build_char_code_map(entries: &[Entry]) -> CharCodes {
    let mut best: HashMap<char, (String, i64)> = HashMap::new();
    for e in entries {
        let mut it = e.text.chars();
        let (Some(c), None) = (it.next(), it.next()) else {
            continue; // 只收单字
        };
        match best.get(&c) {
            Some((_, w)) if *w >= e.weight => {}
            _ => {
                best.insert(c, (e.code.clone(), e.weight));
            }
        }
    }
    best.into_iter().map(|(c, (code, _))| (c, code)).collect()
}

/// 按五笔 86 词组取码规则合成编码：
///
/// - 2 字：字1前 2 码 + 字2前 2 码
/// - 3 字：字1首码 + 字2首码 + 字3前 2 码
/// - 4 字及以上：字1/2/3 首码 + **末字**首码
///
/// 任一取码所需的字不在反查表中即返回 `None`——宁可丢弃也不产出错码。
pub fn encode_phrase(text: &str, char_codes: &CharCodes) -> Option<String> {
    let runes: Vec<char> = text.chars().collect();
    if runes.is_empty() {
        return None;
    }
    let get = |r: char| char_codes.get(&r);
    // 取前 n 码，不足补 'l'（五笔末笔识别码占位）
    let prefix = |code: &str, n: usize| -> String {
        let mut s: String = code.chars().take(n).collect();
        while s.chars().count() < n {
            s.push('l');
        }
        s
    };

    match runes.len() {
        1 => get(runes[0]).cloned(),
        2 => {
            let (c1, c2) = (get(runes[0])?, get(runes[1])?);
            Some(format!("{}{}", prefix(c1, 2), prefix(c2, 2)))
        }
        3 => {
            let (c1, c2, c3) = (get(runes[0])?, get(runes[1])?, get(runes[2])?);
            Some(format!(
                "{}{}{}",
                prefix(c1, 1),
                prefix(c2, 1),
                prefix(c3, 2)
            ))
        }
        n => {
            let (c1, c2, c3, cl) = (
                get(runes[0])?,
                get(runes[1])?,
                get(runes[2])?,
                get(runes[n - 1])?,
            );
            Some(format!(
                "{}{}{}{}",
                prefix(c1, 1),
                prefix(c2, 1),
                prefix(c3, 1),
                prefix(cl, 1)
            ))
        }
    }
}

/// 加载自定义词表：每行一词，可选 `<TAB>频率`（当前仅作说明性字段，不参与计算），
/// `#` 开头为注释。编码由 [`encode_phrase`] 反查，权重优先取 unigram。
pub fn load_custom_words(
    path: &Path,
    char_codes: &CharCodes,
    unigram: &Unigram,
    log_median: f64,
    cfg: &Config,
    log: &mut dyn FnMut(String),
) -> anyhow::Result<Vec<Entry>> {
    let f = std::fs::File::open(path)?;
    let mut entries = Vec::new();
    let mut skipped = 0usize;

    for line in BufReader::new(f).lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let word = line.split('\t').next().unwrap_or("").trim();
        if word.is_empty() {
            continue;
        }
        let Some(code) = encode_phrase(word, char_codes) else {
            log(format!("        [跳过] 无法编码: {word}"));
            skipped += 1;
            continue;
        };

        // 未命中 unigram 时给 target_median，使自定义词落在中位档而非沉底
        let mut weight = cfg.target_median;
        if log_median > 0.0
            && let Some(&freq) = unigram.get(word)
        {
            let w = cfg.target_median as f64 * ((freq as f64) + 1.0).log10() / log_median;
            weight = (w.round() as i64).clamp(cfg.weight_min, cfg.weight_max);
        }
        entries.push(Entry::new(word.to_string(), code, weight, 0));
    }
    if skipped > 0 {
        log(format!("        跳过 {skipped} 条（无法反查编码）"));
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes() -> CharCodes {
        // 取码规则验证用的最小表
        let mut m = CharCodes::new();
        m.insert('人', "w".into()); // 一级简码，仅 1 码
        m.insert('工', "aaa".into());
        m.insert('智', "tdkj".into());
        m.insert('能', "cexx".into());
        m.insert('中', "khk".into());
        m
    }

    #[test]
    fn two_char_takes_first_two_of_each() {
        let c = codes();
        assert_eq!(encode_phrase("智能", &c), Some("tdce".into()));
    }

    #[test]
    fn three_char_is_one_one_two() {
        let c = codes();
        assert_eq!(encode_phrase("工智能", &c), Some("atce".into()));
    }

    #[test]
    fn four_plus_uses_last_char_not_fourth() {
        let c = codes();
        // 5 字词：前三字首码 + **末字**首码（不是第 4 字）
        assert_eq!(encode_phrase("人工智能中", &c), Some("watk".into()));
    }

    #[test]
    fn short_code_is_padded_with_l() {
        let c = codes();
        // 「人」只有 1 码 w，2 字规则要 2 码 → 补 l
        assert_eq!(encode_phrase("人人", &c), Some("wlwl".into()));
    }

    #[test]
    fn missing_char_yields_none_rather_than_wrong_code() {
        let c = codes();
        assert_eq!(
            encode_phrase("智鑫", &c),
            None,
            "缺字须整体失败，不得产出错码"
        );
    }

    #[test]
    fn single_char_returns_its_own_code() {
        assert_eq!(encode_phrase("中", &codes()), Some("khk".into()));
        assert_eq!(encode_phrase("", &codes()), None);
    }

    #[test]
    fn char_map_prefers_highest_weight_code() {
        let entries = vec![
            Entry::new("中".into(), "khk".into(), 10, 0),
            Entry::new("中".into(), "k".into(), 30, 1), // 权重更高 → 首选
            Entry::new("中文".into(), "khyy".into(), 30, 2), // 词组不入表
        ];
        let m = build_char_code_map(&entries);
        assert_eq!(m.get(&'中'), Some(&"k".to_string()));
        assert_eq!(m.len(), 1, "只收单字");
    }
}
