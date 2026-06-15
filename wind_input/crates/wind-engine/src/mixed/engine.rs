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
}

impl MixedEngine {
    pub fn new(
        primary: Box<dyn Engine>,
        secondary: Option<Box<dyn Engine>>,
        min_pinyin_length: usize,
        codetable_weight_boost: i32,
    ) -> Self {
        Self {
            primary,
            secondary,
            min_pinyin_length,
            codetable_weight_boost,
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
        if input_len >= self.min_pinyin_length {
            if let Some(sec) = &self.secondary {
                if let Ok(py) = sec.convert(input, max_candidates) {
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

        let is_empty = merged.is_empty();
        Ok(ConvertResult {
            candidates: merged,
            // 混输组合区显示原始输入码（五笔为主，简明）
            preedit_display: input.to_string(),
            is_empty,
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
}
