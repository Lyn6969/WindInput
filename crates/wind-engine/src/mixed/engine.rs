//! 混合引擎实现
//!
//! 与 Go 版本 `wind_input/internal/engine/mixed/` 对齐。

use crate::engine::{ConvertResult, Engine, EngineType};

/// 混合引擎
pub struct MixedEngine {
    // TODO: primary (codetable) + secondary (pinyin) engines
}

impl MixedEngine {
    pub fn new() -> Self {
        Self {}
    }
}

impl Engine for MixedEngine {
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
        EngineType::Mixed
    }
}
