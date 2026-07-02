//! Shadow 规则应用：对候选列表执行用户置顶/删除的重排。
//!
//! 与 Go 版本 `wind_input/internal/coordinator` 的 shadow 应用对齐。纯函数——规则数据
//! （删除词表 + 置顶项）由调用方从 store 取出后以基础类型传入，本模块不依赖 store，便于单测。

use crate::candidate::Candidate;

/// 对 `candidates` 应用 shadow 规则：
///   1. 删除 `deleted` 中出现的词（按 text 匹配）；
///   2. 把 `pinned`（词, 目标位置）重新就位：
///      - 按 `position` 升序处理，保证后续插入考虑前面已就位的项；
///      - 同一 `position` 内按"最新在前"排列——`pinned` 契约为最新在前（index 0 最近），
///        排序键 `(position 升序, 原始索引降序)` 使较老项先 insert、最新项最后 insert 停在最前；
///      - 位置越界时钳到末尾。
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
    // 按 position 升序应用；同一 position 内按"最新在前"（入参 index 小者更近）就位。
    // 契约：pinned 为最新在前（store apply_pin insert(0) + coordinator 原序传入）。
    // 排序键 (position 升序, 原始索引降序)：同 position 内让较老项先 insert，
    // 最新项最后 insert 停在最前，得到"后添加者靠前"。
    let mut idx: Vec<(usize, &(String, usize))> = pinned.iter().enumerate().collect();
    idx.sort_by(|a, b| a.1.1.cmp(&b.1.1).then(b.0.cmp(&a.0)));
    for (_, (word, position)) in idx {
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

    #[test]
    fn same_position_latest_first() {
        // pinned 为"最新在前"：乙(index0=最近) 与 甲(index1) 都 → pos0。
        let mut c = cands(&["甲", "乙", "丙", "丁"]);
        apply_shadow(&mut c, &[], &[("乙".to_string(), 0), ("甲".to_string(), 0)]);
        // 后固定的乙应在最前：[乙, 甲, 丙, 丁]
        assert_eq!(texts(&c), ["乙", "甲", "丙", "丁"]);
    }

    #[test]
    fn three_same_position_latest_first() {
        // 最新在前：丙(最近) > 乙 > 甲(最早)，都 → pos0。
        let mut c = cands(&["甲", "乙", "丙", "丁"]);
        apply_shadow(
            &mut c,
            &[],
            &[
                ("丙".to_string(), 0),
                ("乙".to_string(), 0),
                ("甲".to_string(), 0),
            ],
        );
        // 期望最近的排最前：[丙, 乙, 甲, 丁]
        assert_eq!(texts(&c), ["丙", "乙", "甲", "丁"]);
    }
}
