//! Shadow 规则应用：对候选列表执行用户置顶/删除的重排。
//!
//! 与 Go 版本 `wind_input/internal/coordinator` 的 shadow 应用对齐。纯函数——规则数据
//! （删除词表 + 置顶项）由调用方从 store 取出后以基础类型传入，本模块不依赖 store，便于单测。

use crate::candidate::Candidate;

/// 一条置顶/移动规则（store `ShadowPin` 的无依赖镜像，使本 crate 不依赖 wind-store）。
///
/// **匹配优先级**（对齐 Go `dict.ApplyShadowPins`）：`cand_id` 非空 → **严格**按
/// [`Candidate::id`] 匹配，候选 id 为空时视为不命中，**绝不回退到 `word`**；
/// `cand_id` 为空 → 按 `word` 匹配候选文本（存量规则与静态候选的既有行为）。
///
/// ⚠ 「id 非空则不回退 word」是本结构的核心契约，不是保守写法。动态候选（`date` 等
/// 求值型短语）的 `word` 记的是**写入规则那天的求值结果**（如 `2026-07-29`），次日
/// 必然过期；一旦回退，那条过期文本会去匹配当天候选里**碰巧同文**的另一条候选，
/// 把「规则失效」升级成「规则误伤」。
#[derive(Debug, Clone)]
pub struct ShadowPinRule {
    pub word: String,
    pub cand_id: Option<String>,
    pub position: usize,
}

impl ShadowPinRule {
    /// 按 word 匹配的规则（静态候选 / 存量规则）。
    pub fn by_word(word: impl Into<String>, position: usize) -> Self {
        Self {
            word: word.into(),
            cand_id: None,
            position,
        }
    }

    /// 本规则是否命中该候选（匹配优先级见结构体文档）。
    fn matches(&self, c: &Candidate) -> bool {
        match self.cand_id.as_deref() {
            Some(id) if !id.is_empty() => !c.id.is_empty() && c.id == id,
            _ => c.text == self.word,
        }
    }
}

/// 对 `candidates` 应用 shadow 规则：
///   1. 删除 `deleted` 中出现的词（按 text 匹配）；
///   2. 把 `pinned` 各条按其匹配目标重新就位：
///      - 按 `position` 升序处理，保证后续插入考虑前面已就位的项；
///      - 同一 `position` 内按"最新在前"排列——`pinned` 契约为最新在前（index 0 最近），
///        排序键 `(position 升序, 原始索引降序)` 使较老项先 insert、最新项最后 insert 停在最前；
///      - 位置越界时钳到末尾。
///
/// `deleted` 仍是纯文本匹配：走 shadow 删除的只有系统码表/拼音候选（静态文本），
/// 短语删除走 `set_phrase_enabled`（按 `(code, phrase_template)` 定位，本就是稳定键，
/// 见 `delete_candidate_by_source`），故删除侧没有动态候选可失配。
pub fn apply_shadow(candidates: &mut Vec<Candidate>, deleted: &[String], pinned: &[ShadowPinRule]) {
    if !deleted.is_empty() {
        candidates.retain(|c| !deleted.iter().any(|d| d == &c.text));
    }
    // 按 position 升序应用；同一 position 内按"最新在前"（入参 index 小者更近）就位。
    // 契约：pinned 为最新在前（store apply_pin insert(0) + coordinator 原序传入）。
    // 排序键 (position 升序, 原始索引降序)：同 position 内让较老项先 insert，
    // 最新项最后 insert 停在最前，得到"后添加者靠前"。
    let mut idx: Vec<(usize, &ShadowPinRule)> = pinned.iter().enumerate().collect();
    idx.sort_by(|a, b| a.1.position.cmp(&b.1.position).then(b.0.cmp(&a.0)));
    for (_, rule) in idx {
        if let Some(cur) = candidates.iter().position(|c| rule.matches(c)) {
            let cand = candidates.remove(cur);
            let at = rule.position.min(candidates.len());
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

    /// (显示文本, 稳定 id) 构造——模拟求值型短语候选。
    fn cands_with_id(items: &[(&str, &str)]) -> Vec<Candidate> {
        items
            .iter()
            .map(|(t, id)| Candidate {
                text: (*t).to_string(),
                id: (*id).to_string(),
                ..Default::default()
            })
            .collect()
    }

    fn texts(c: &[Candidate]) -> Vec<String> {
        c.iter().map(|c| c.text.clone()).collect()
    }

    fn by_word(word: &str, position: usize) -> ShadowPinRule {
        ShadowPinRule::by_word(word, position)
    }

    fn by_id(word: &str, id: &str, position: usize) -> ShadowPinRule {
        ShadowPinRule {
            word: word.to_string(),
            cand_id: Some(id.to_string()),
            position,
        }
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
        apply_shadow(&mut c, &[], &[by_word("丙", 0)]);
        assert_eq!(texts(&c), ["丙", "甲", "乙", "丁"]);
    }

    #[test]
    fn pins_applied_in_position_order() {
        let mut c = cands(&["甲", "乙", "丙", "丁"]);
        // 多个置顶：position 升序应用（丁→0，甲→1）。
        apply_shadow(&mut c, &[], &[by_word("甲", 1), by_word("丁", 0)]);
        // 丁 先就位到 0 → [丁,甲,乙,丙]；甲 再到 1 → [丁,甲,乙,丙]。
        assert_eq!(texts(&c), ["丁", "甲", "乙", "丙"]);
    }

    #[test]
    fn pin_position_clamped_to_end() {
        let mut c = cands(&["甲", "乙"]);
        // position 越界 → 钳到末尾。
        apply_shadow(&mut c, &[], &[by_word("甲", 99)]);
        assert_eq!(texts(&c), ["乙", "甲"]);
    }

    #[test]
    fn delete_then_pin_combined() {
        let mut c = cands(&["甲", "乙", "丙"]);
        apply_shadow(&mut c, &["甲".to_string()], &[by_word("丙", 0)]);
        assert_eq!(texts(&c), ["丙", "乙"]);
    }

    #[test]
    fn missing_pin_word_is_ignored() {
        let mut c = cands(&["甲", "乙"]);
        apply_shadow(&mut c, &[], &[by_word("不存在", 0)]);
        assert_eq!(texts(&c), ["甲", "乙"]);
    }

    #[test]
    fn same_position_latest_first() {
        // pinned 为"最新在前"：乙(index0=最近) 与 甲(index1) 都 → pos0。
        let mut c = cands(&["甲", "乙", "丙", "丁"]);
        apply_shadow(&mut c, &[], &[by_word("乙", 0), by_word("甲", 0)]);
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
            &[by_word("丙", 0), by_word("乙", 0), by_word("甲", 0)],
        );
        // 期望最近的排最前：[丙, 乙, 甲, 丁]
        assert_eq!(texts(&c), ["丙", "乙", "甲", "丁"]);
    }

    /// 核心回归：规则的 `word` 是**写入当天**的求值文本，今天候选文本已全变。
    /// 按 id 匹配才命中——这正是 `date` 置顶「第二天被还原」的场景。
    #[test]
    fn pin_by_id_survives_text_change() {
        let mut c = cands_with_id(&[
            ("2026年7月30日", "phrase:date:$Y年$M月$D日"),
            ("2026-07-30", "phrase:date:$Y-$MM-$DD"),
            ("2026.07.30", "phrase:date:$Y.$MM.$DD"),
        ]);
        // 昨天写下的规则：word 是昨天的文本，今天一条都对不上。
        apply_shadow(
            &mut c,
            &[],
            &[by_id("2026-07-29", "phrase:date:$Y-$MM-$DD", 0)],
        );
        assert_eq!(
            texts(&c),
            ["2026-07-30", "2026年7月30日", "2026.07.30"],
            "按 cand_id 匹配应跨日生效"
        );
    }

    /// 反向闸门：`cand_id` 非空时**不得**回退到 word 匹配。
    /// 否则过期文本会误伤当天碰巧同文的另一条候选。
    #[test]
    fn pin_by_id_does_not_fall_back_to_word() {
        // 「2026-07-29」今天仍存在，但它是**另一条**短语（固定字面量）产出的。
        let mut c = cands_with_id(&[
            ("2026-07-30", "phrase:date:$Y-$MM-$DD"),
            ("2026-07-29", "phrase:date:literal-fixed"),
        ]);
        // 规则的 id 指向 $Y-$MM-$DD，word 是过期的 2026-07-29。
        apply_shadow(
            &mut c,
            &[],
            &[by_id("2026-07-29", "phrase:date:$Y-$MM-$DD", 1)],
        );
        // 命中的必须是 id 所指的那条（移到 1 位），而不是同文的字面量那条。
        assert_eq!(texts(&c), ["2026-07-29", "2026-07-30"]);
    }

    /// 规则带 id、候选却没填 id（如码表候选）→ 不命中，不回退 word。
    /// 对齐 Go `TestApplyShadowPins_IDFallsBackWhenAbsent`。
    #[test]
    fn pin_by_id_misses_when_candidate_has_no_id() {
        let mut c = cands(&["甲", "乙", "丙"]);
        apply_shadow(&mut c, &[], &[by_id("丙", "phrase:x:stale", 0)]);
        assert_eq!(texts(&c), ["甲", "乙", "丙"], "候选无 id → 规则不生效");
    }

    /// 空字符串 cand_id 等价于「无 id」，落回 word 匹配（防 store 侧写入空串）。
    #[test]
    fn empty_cand_id_falls_back_to_word() {
        let mut c = cands(&["甲", "乙"]);
        let rule = ShadowPinRule {
            word: "乙".into(),
            cand_id: Some(String::new()),
            position: 0,
        };
        apply_shadow(&mut c, &[], &[rule]);
        assert_eq!(texts(&c), ["乙", "甲"]);
    }
}
