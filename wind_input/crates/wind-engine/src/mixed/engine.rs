//! 混合引擎实现（码表主 + 拼音次，分层加权合并）
//!
//! 与 Go 版本 `wind_input/internal/engine/mixed/mixed.go` 对齐（核心分层）。
//!
//! 加权策略（双向夹击）：
//! - 码表：精确匹配(code==input) +CodetableWeightBoost(默认 1e7)；短语 +1M；前缀补全 +500K
//! - 拼音：weight ÷ PinyinTierScale(100) 归一化到低档（0~100K），与码表/短语严格隔离
//! - 合并后按权重排序、按文本去重；输入短于 min_pinyin_length 时仅码表
//!
//! 后置：英文候选、简拼长度惩罚（HasFullSyllable）、convertMixedOverflow 精细档。

use crate::engine::{ConvertResult, Engine, EngineType};
use wind_candidate::Candidate;

/// 短语候选提权（高于拼音、低于码表词）
const PHRASE_WEIGHT_BOOST: i32 = 1_000_000;
/// 码表前缀补全（拆分组合）提权
const PARTIAL_MATCH_BOOST: i32 = 500_000;
/// 拼音候选归一化系数（÷ 后落入低档）
const PINYIN_TIER_SCALE: i32 = 100;

/// 混合引擎
pub struct MixedEngine {
    /// 主引擎（码表，如五笔）
    primary: Box<dyn Engine>,
    /// 次引擎（拼音）
    secondary: Option<Box<dyn Engine>>,
    /// 拼音生效的最小输入长度
    min_pinyin_length: usize,
    /// 码表精确匹配提权
    codetable_weight_boost: i32,
    /// 全码自动上屏时，若存在拼音候选则否决（保护拼音用户，对齐 Go AutoCommitBlockOnPinyin）
    auto_commit_block_on_pinyin: bool,
}

impl MixedEngine {
    pub fn new(
        primary: Box<dyn Engine>,
        secondary: Option<Box<dyn Engine>>,
        min_pinyin_length: usize,
        codetable_weight_boost: i32,
        auto_commit_block_on_pinyin: bool,
    ) -> Self {
        Self {
            primary,
            secondary,
            min_pinyin_length,
            codetable_weight_boost,
            auto_commit_block_on_pinyin,
        }
    }
}

impl Engine for MixedEngine {
    fn convert(&self, input: &str, max_candidates: usize) -> anyhow::Result<ConvertResult> {
        if input.is_empty() {
            return Ok(ConvertResult::default());
        }
        let input_len = input.chars().count();

        // 1. 码表候选 + 加权
        let ct = self.primary.convert(input, max_candidates)?;
        // 主码表的全码自动上屏意向（下方按拼音守护 + 合并存活性复核后才放行）。
        let ct_should_commit = ct.should_commit;
        let ct_commit_text = ct.commit_text.clone();
        let ct_should_clear = ct.should_clear;
        let mut codetable: Vec<Candidate> = ct.candidates;
        for c in &mut codetable {
            if c.is_phrase {
                c.weight = c.weight.saturating_add(PHRASE_WEIGHT_BOOST);
            } else if c.code == input {
                c.weight = c.weight.saturating_add(self.codetable_weight_boost);
            } else {
                c.weight = c.weight.saturating_add(PARTIAL_MATCH_BOOST);
            }
        }

        // 2. 拼音候选（输入达到最小长度）+ 归一化降档
        let mut pinyin: Vec<Candidate> = Vec::new();
        // 多音节拼音的组合区分隔显示（如 "ni hao"）：仅当拼音解析出 ≥2 完成音节时采用，
        // 否则保持原始码（单音节如 "cang" 无需分隔，纯五笔码更不应被拆）。
        let mut pinyin_preedit: Option<String> = None;
        if input_len >= self.min_pinyin_length {
            if let Some(sec) = &self.secondary {
                if let Ok(py) = sec.convert(input, max_candidates) {
                    if py.completed_syllables.len() >= 2 && !py.preedit_display.is_empty() {
                        pinyin_preedit = Some(py.preedit_display.clone());
                    }
                    pinyin = py.candidates;
                    for c in &mut pinyin {
                        c.weight /= PINYIN_TIER_SCALE;
                        if c.weight < 0 {
                            c.weight = 0;
                        }
                    }
                }
            }
        }

        // 3. 合并（码表在前，拼音在后）→ 按权重稳定排序 → 按文本去重
        let has_pinyin = !pinyin.is_empty();
        let mut merged = codetable;
        merged.extend(pinyin);
        merged.sort_by(|a, b| {
            b.weight
                .cmp(&a.weight)
                .then(a.natural_order.cmp(&b.natural_order))
        });
        let mut seen = std::collections::HashSet::new();
        merged.retain(|c| seen.insert(c.text.clone()));
        merged.truncate(max_candidates);

        // 全码自动上屏重评（对齐 Go recheckAutoCommit）：取主码表意向，
        // 但若开启拼音守护且存在拼音候选则否决（输入可能是拼音，留给用户选）；
        // 并复核上屏目标在合并结果中仍存活。
        let (should_commit, commit_text) = if ct_should_commit
            && !ct_commit_text.is_empty()
            && !(self.auto_commit_block_on_pinyin && has_pinyin)
            && merged.iter().any(|c| c.text == ct_commit_text)
        {
            (true, ct_commit_text)
        } else {
            (false, String::new())
        };

        // 空码清空：仅当主码表请求清空且无拼音候选（合法拼音序列留给拼音，不清空）。
        let should_clear = ct_should_clear && !has_pinyin;

        let is_empty = merged.is_empty();
        Ok(ConvertResult {
            candidates: merged,
            // 组合区：多音节拼音用音节分隔（ni hao），否则原始码（五笔为主，简明）。
            preedit_display: pinyin_preedit.unwrap_or_else(|| input.to_string()),
            is_empty,
            should_commit,
            commit_text,
            should_clear,
            ..Default::default()
        })
    }

    fn reset(&self) {
        self.primary.reset();
        if let Some(s) = &self.secondary {
            s.reset();
        }
    }

    fn engine_type(&self) -> EngineType {
        EngineType::Mixed
    }

    /// 顶码委托主码表引擎（拼音守护的精细判定后续随混输顶码细化补充）。
    fn handle_top_code(&self, input: &str) -> Option<(String, String)> {
        self.primary.handle_top_code(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codetable::{CodeTableEngine, CommitOptions};
    use std::sync::Arc;
    use wind_dict::cached::CachedDict;
    use wind_dict::codetable::CodetableDict;
    use wind_dict::{DictManager, SystemDictLayer};

    /// 构建一个内存码表引擎（可选开启全码自动上屏）。
    fn ct_engine(entries: &[(&str, &str, i32)], at_full: bool) -> Box<dyn Engine> {
        let mut d = CodetableDict::empty();
        for (i, (code, text, w)) in entries.iter().enumerate() {
            d.merge_single(code.to_string(), text.to_string(), *w, i as i32);
        }
        let dm = DictManager::new();
        dm.register_layer(Box::new(SystemDictLayer::new(CachedDict::Memory(d), "sys")));
        let opts = CommitOptions {
            auto_commit_at_full: at_full,
            auto_commit_min_len: 4,
            ..Default::default()
        };
        Box::new(CodeTableEngine::new(4, opts, Arc::new(dm)))
    }

    #[test]
    fn mixed_propagates_auto_commit_without_pinyin() {
        // 主码表唯一全码自动上屏；无次引擎 → 无拼音候选 → 放行。
        let primary = ct_engine(&[("aaaa", "工", 100)], true);
        let e = MixedEngine::new(primary, None, 2, 10_000_000, true);
        let r = e.convert("aaaa", 50).unwrap();
        assert!(r.should_commit, "无拼音候选时应放行全码上屏");
        assert_eq!(r.commit_text, "工");
    }

    #[test]
    fn mixed_blocks_auto_commit_when_pinyin_present() {
        // 次引擎对同一输入也产出候选（模拟拼音命中）+ 守护开 → 否决上屏。
        let primary = ct_engine(&[("aaaa", "工", 100)], true);
        let secondary = ct_engine(&[("aaaa", "啊啊", 50)], false);
        let e = MixedEngine::new(primary, Some(secondary), 2, 10_000_000, true);
        let r = e.convert("aaaa", 50).unwrap();
        assert!(!r.should_commit, "有拼音候选且守护开时应否决全码上屏");
    }

    #[test]
    fn mixed_allows_auto_commit_when_guard_off() {
        // 守护关 → 即便有拼音候选也放行。
        let primary = ct_engine(&[("aaaa", "工", 100)], true);
        let secondary = ct_engine(&[("aaaa", "啊啊", 50)], false);
        let e = MixedEngine::new(primary, Some(secondary), 2, 10_000_000, false);
        let r = e.convert("aaaa", 50).unwrap();
        assert!(r.should_commit, "守护关时应放行");
        assert_eq!(r.commit_text, "工");
    }
}
