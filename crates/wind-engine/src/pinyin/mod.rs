//! 拼音输入引擎
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/` 对齐。

pub mod dag;
pub mod fuzzy;
pub mod lattice;
pub mod lm;
pub mod parser;
pub mod scorer;
pub mod shuangpin;
pub mod syllable;
pub mod viterbi;

use crate::engine::{ConvertResult, Engine, EngineType};
use wind_candidate::Candidate;

/// 拼音引擎配置
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub show_code_hint: bool,
    pub filter_mode: String,
    pub use_smart_compose: bool,
    pub candidate_order: String,
}

/// 拼音引擎
pub struct PinyinEngine {
    config: Config,
    // TODO: dict, syllable_trie, unigram, bigram, etc.
}

impl PinyinEngine {
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

impl Engine for PinyinEngine {
    fn convert(&self, input: &str, max_candidates: usize) -> anyhow::Result<ConvertResult> {
        // TODO: 实现完整的拼音转换流程
        Ok(ConvertResult {
            candidates: Vec::new(),
            preedit_display: input.to_string(),
            completed_syllables: Vec::new(),
            partial_syllable: String::new(),
            has_partial: false,
        })
    }

    fn reset(&self) {
        // TODO
    }

    fn engine_type(&self) -> EngineType {
        EngineType::Pinyin
    }
}
