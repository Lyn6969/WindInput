//! 码表引擎实现
//!
//! 与 Go 版本 `wind_input/internal/engine/codetable/` 对齐。
//!
//! 查询经 `DictManager`（CompositeDict）——系统词库 + （后续）用户/临时词层统一合并。
//! 候选生成：精确匹配 + 前缀匹配。运行时词频/shadow 不在此（见 frequency.md / dict.md）。

use crate::engine::{ConvertResult, Engine, EngineType, ExtendedEngine};
use std::collections::HashSet;
use std::sync::Arc;
use wind_candidate::{better, Candidate, CandidateSource};
use wind_dict::DictManager;

/// 码表引擎
pub struct CodeTableEngine {
    max_code_length: usize,
    dm: Arc<DictManager>,
}

impl CodeTableEngine {
    pub fn new(max_code_length: usize, dm: Arc<DictManager>) -> Self {
        Self { max_code_length, dm }
    }
}

impl Engine for CodeTableEngine {
    fn convert(&self, input: &str, max_candidates: usize) -> anyhow::Result<ConvertResult> {
        if input.is_empty() {
            return Ok(ConvertResult::default());
        }

        let limit = max_candidates.max(50);
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        // 精确匹配优先（完整编码）
        for mut c in self.dm.search(input, limit) {
            if seen.insert(c.text.clone()) {
                c.source = CandidateSource::CodeTable;
                candidates.push(c);
            }
        }

        // 前缀匹配补充
        for mut c in self.dm.search_prefix(input, limit) {
            if seen.insert(c.text.clone()) {
                c.source = CandidateSource::CodeTable;
                candidates.push(c);
            }
        }

        candidates.sort_by(better);
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
