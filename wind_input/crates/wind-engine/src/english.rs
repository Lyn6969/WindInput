//! 英文引擎（临时英文 / 融合英文候选用）
//!
//! 薄封装 [`CodeTableEngine`]：复用其词典加载与前缀匹配，但作为独立引擎类型，
//! 便于后续英文专属演化（词频归属、融合加权档、独立学习）。英文词库以 `type = "english"`
//! 声明（code 列小写化，大小写不敏感前缀匹配），构造时关闭码表的自动上屏 / 顶码 / 编码提示
//! （英文词变长，无「满码顶字」语义）。
//!
//! 大小写适配（输入 `HEL` → `HELLO`）由 coordinator 层的临时英文按输入形态后处理，
//! 融合模式（快捷 / 混输）不做适配，故本引擎只吐词库原文候选。

use crate::codetable::CodeTableEngine;
use crate::engine::{ConvertResult, Engine, EngineType};
use wind_candidate::CandidateSource;

/// 英文引擎：内部复用码表引擎的查询，候选统一标记为 [`CandidateSource::English`]。
pub struct EnglishEngine {
    inner: CodeTableEngine,
}

impl EnglishEngine {
    pub fn new(inner: CodeTableEngine) -> Self {
        Self { inner }
    }
}

impl Engine for EnglishEngine {
    fn convert(&self, input: &str, max_candidates: usize) -> anyhow::Result<ConvertResult> {
        let mut r = self.inner.convert(input, max_candidates)?;
        // 英文候选统一标记来源（词频归属 / 融合加权档区分用）。
        for c in &mut r.candidates {
            c.source = CandidateSource::English;
        }
        // 英文无「自动上屏」语义：即使内部误判也抹掉（构造已关，此为双保险）。
        r.should_commit = false;
        r.commit_text.clear();
        Ok(r)
    }

    fn reset(&self) {
        self.inner.reset();
    }

    fn engine_type(&self) -> EngineType {
        EngineType::English
    }

    fn set_dict_enabled(&self, dict_id: &str, enabled: bool) -> bool {
        self.inner.set_dict_enabled(dict_id, enabled)
    }

    fn input_chars(&self) -> Option<&wind_config::CodeCharSet> {
        Engine::input_chars(&self.inner)
    }

    fn max_code_length(&self) -> usize {
        Engine::max_code_length(&self.inner)
    }

    fn has_full_input_match(&self, input: &str) -> bool {
        self.inner.has_full_input_match(input)
    }

    fn has_longer_code(&self, input: &str) -> bool {
        self.inner.has_longer_code(input)
    }

    // handle_top_code：用 trait 默认 None —— 英文无顶码上屏语义。
}
