//! 码表引擎实现
//!
//! 与 Go 版本 `wind_input/internal/engine/codetable/` 对齐。

use crate::engine::{ConvertResult, Engine, EngineType, ExtendedEngine};
use wind_candidate::Candidate;

/// 码表引擎
pub struct CodeTableEngine {
    max_code_length: usize,
}

impl CodeTableEngine {
    pub fn new(max_code_length: usize) -> Self {
        Self { max_code_length }
    }
}

impl Engine for CodeTableEngine {
    fn convert(&self, input: &str, max_candidates: usize) -> anyhow::Result<ConvertResult> {
        Ok(ConvertResult {
            candidates: Vec::new(),
            preedit_display: input.to_string(),
            completed_syllables: Vec::new(),
            partial_syllable: String::new(),
            has_partial: false,
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
