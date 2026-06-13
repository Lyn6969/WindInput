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
    fn convert(&self, input: &str, _max_candidates: usize) -> anyhow::Result<ConvertResult> {
        Ok(ConvertResult {
            preedit_display: input.to_string(),
            ..Default::default()
        })
    }

    fn reset(&self) {}

    fn engine_type(&self) -> EngineType {
        EngineType::Mixed
    }
}
