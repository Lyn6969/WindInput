//! Shadow 规则应用：对候选列表执行用户置顶/删除的重排。
//!
//! 与 Go 版本 `wind_input/internal/coordinator` 的 shadow 应用对齐。纯函数——规则数据
//! （删除词表 + 置顶项）由调用方从 store 取出后以基础类型传入，本模块不依赖 store，便于单测。

use crate::candidate::Candidate;

/// 对 `candidates` 应用 shadow 规则：
///   1. 删除 `deleted` 中出现的词（按 text 匹配）；
///   2. 按 `position` 升序把 `pinned`（词, 目标位置）重新就位——升序保证后插入项
///      考虑前面已就位的项，位置越界时钳到末尾。
///
/// `pinned` 用 `(String, usize)` 元组而非 store 的 ShadowPin，使本 crate 不依赖 wind-store。
pub fn apply_shadow(
    candidates: &mut Vec<Candidate>,
    deleted: &[String],
    pinned: &[(String, usize)],
) {
    if !deleted.is_empty() {
        candidates.retain(|c| !deleted.iter().any(|d| d == &c.text));
    }
    // 按 position 升序应用，使后续插入考虑前面已就位的项。
    let mut pins: Vec<&(String, usize)> = pinned.iter().collect();
    pins.sort_by_key(|p| p.1);
    for (word, position) in pins {
        if let Some(cur) = candidates.iter().position(|c| &c.text == word) {
            let cand = candidates.remove(cur);
            let at = (*position).min(candidates.len());
            candidates.insert(at, cand);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cands(texts: &[&str]) -> Vec<Candidate> {
        texts
            .iter()
            .map(|t| Candidate {
                text: (*t).to_string(),
                ..Default::default()
            })
            .collect()
    }

    fn texts(c: &[Candidate]) -> Vec<String> {
        c.iter().map(|c| c.text.clone()).collect()
    }

    #[test]
    fn deletes_matching_words() {
        let mut c = cands(&["甲", "乙", "丙"]);
        apply_shadow(&mut c, &["乙".to_string()], &[]);
        assert_eq!(texts(&c), ["甲", "丙"]);
    }

    #[test]
    fn pins_to_position() {
        let mut c = cands(&["甲", "乙", "丙", "丁"]);
        // 把"丙"置顶到 0。
        apply_shadow(&mut c, &[], &[("丙".to_string(), 0)]);
        assert_eq!(texts(&c), ["丙", "甲", "乙", "丁"]);
    }

    #[test]
    fn pins_applied_in_position_order() {
        let mut c = cands(&["甲", "乙", "丙", "丁"]);
        // 多个置顶：position 升序应用（丁→0，甲→1）。
        apply_shadow(&mut c, &[], &[("甲".to_string(), 1), ("丁".to_string(), 0)]);
        // 丁 先就位到 0 → [丁,甲,乙,丙]；甲 再到 1 → [丁,甲,乙,丙]。
        assert_eq!(texts(&c), ["丁", "甲", "乙", "丙"]);
    }

    #[test]
    fn pin_position_clamped_to_end() {
        let mut c = cands(&["甲", "乙"]);
        // position 越界 → 钳到末尾。
        apply_shadow(&mut c, &[], &[("甲".to_string(), 99)]);
        assert_eq!(texts(&c), ["乙", "甲"]);
    }

    #[test]
    fn delete_then_pin_combined() {
        let mut c = cands(&["甲", "乙", "丙"]);
        apply_shadow(&mut c, &["甲".to_string()], &[("丙".to_string(), 0)]);
        assert_eq!(texts(&c), ["丙", "乙"]);
    }

    #[test]
    fn missing_pin_word_is_ignored() {
        let mut c = cands(&["甲", "乙"]);
        apply_shadow(&mut c, &[], &[("不存在".to_string(), 0)]);
        assert_eq!(texts(&c), ["甲", "乙"]);
    }
}
