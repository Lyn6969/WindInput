//! 码表引擎实现
//!
//! 与 Go 版本 `wind_input/internal/engine/codetable/` 对齐。
//!
//! 候选生成：精确匹配 + 前缀匹配。运行时词频 boost 由上层应用。

use crate::engine::{ConvertResult, Engine, EngineType, ExtendedEngine};
use wind_candidate::{Candidate, CandidateSource};
use wind_dict::cached::CachedDict;

/// 码表引擎
pub struct CodeTableEngine {
    max_code_length: usize,
    dict: CachedDict,
}

impl CodeTableEngine {
    pub fn new(max_code_length: usize, dict: CachedDict) -> Self {
        Self {
            max_code_length,
            dict,
        }
    }

    /// 总条目数
    pub fn entry_count(&self) -> usize {
        self.dict.len()
    }
}

impl Engine for CodeTableEngine {
    fn convert(&self, input: &str, max_candidates: usize) -> anyhow::Result<ConvertResult> {
        if input.is_empty() {
            return Ok(ConvertResult::default());
        }

        let mut candidates: Vec<Candidate> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // 精确匹配优先（完整编码）
        for (text, weight, order) in self.dict.search(input) {
            if seen.insert(text.clone()) {
                candidates.push(Candidate {
                    text,
                    code: input.to_string(),
                    weight,
                    natural_order: order,
                    source: CandidateSource::CodeTable,
                    ..Default::default()
                });
            }
        }

        // 前缀匹配补充
        for (code, text, weight, order) in self.dict.search_prefix(input, max_candidates.max(50)) {
            if seen.insert(text.clone()) {
                candidates.push(Candidate {
                    text,
                    code,
                    weight,
                    natural_order: order,
                    source: CandidateSource::CodeTable,
                    ..Default::default()
                });
            }
        }

        candidates.sort_by(|a, b| {
            b.weight
                .cmp(&a.weight)
                .then(a.natural_order.cmp(&b.natural_order))
        });
        candidates.truncate(max_candidates);

        let is_empty = candidates.is_empty();
        Ok(ConvertResult {
            candidates,
            preedit_display: input.to_string(),
            is_empty,
            ..Default::default()
        })
    }

    fn reset(&self) {}

    fn engine_type(&self) -> EngineType {
        EngineType::CodeTable
    }
}

impl ExtendedEngine for CodeTableEngine {
    fn max_code_length(&self) -> usize {
        self.max_code_length
    }

    fn should_auto_commit(&self, _input: &str, _candidates: &[Candidate]) -> Option<String> {
        None
    }

    fn handle_empty_code(&self, _input: &str) -> (bool, bool, String) {
        (true, false, String::new())
    }

    fn handle_top_code(&self, _input: &str) -> Option<(String, String)> {
        None
    }
}
