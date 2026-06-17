//! 码表引擎实现
//!
//! 与 Go 版本 `wind_input/internal/engine/codetable/` 对齐。
//!
//! 查询经 `DictManager`（CompositeDict）——系统词库 + （后续）用户/临时词层统一合并。
//! 候选生成：精确匹配 + 前缀匹配。运行时词频/shadow 不在此（见 frequency.md / dict.md）。

use crate::engine::{ConvertResult, Engine, EngineType, ExtendedEngine};
use std::collections::HashSet;
use std::sync::Arc;
use wind_candidate::{Candidate, CandidateSource, better};
use wind_dict::DictManager;

/// 码表引擎
pub struct CodeTableEngine {
    max_code_length: usize,
    /// 全码自动上屏开关（schema 的 auto_commit_at_full，含 legacy auto_commit_unique 回退）
    auto_commit_at_full: bool,
    /// 自动上屏最短码长（0 在构建时已回退为 max_code_length）
    auto_commit_min_len: usize,
    dm: Arc<DictManager>,
}

impl CodeTableEngine {
    pub fn new(
        max_code_length: usize,
        auto_commit_at_full: bool,
        auto_commit_min_len: usize,
        dm: Arc<DictManager>,
    ) -> Self {
        // min_len 为 0 时跟随 max_code_length（对齐 Go codetable.go:135）。
        let auto_commit_min_len = if auto_commit_min_len == 0 {
            max_code_length
        } else {
            auto_commit_min_len
        };
        Self {
            max_code_length,
            auto_commit_at_full,
            auto_commit_min_len,
            dm,
        }
    }

    /// 是否存在比 `input` 更长的后继编码（避免把长码精确匹配的前缀误当全码上屏）。
    fn has_longer_code(&self, input: &str) -> bool {
        let n = input.chars().count();
        self.dm
            .search_prefix(input, 64)
            .iter()
            .any(|c| c.code.chars().count() > n)
    }
}

/// 全码自动上屏纯判定（对齐 Go checkAutoCommit）：
/// 开关开 + 码长达 min_len + 恰一个精确匹配（code==input）+ 无更长后继 → 上屏该候选文本。
fn decide_auto_commit(
    at_full: bool,
    min_len: usize,
    input: &str,
    candidates: &[Candidate],
    has_longer: bool,
) -> Option<String> {
    if !at_full || input.chars().count() < min_len {
        return None;
    }
    let mut exact = candidates.iter().filter(|c| c.code == input);
    let first = exact.next()?;
    if exact.next().is_some() {
        return None; // 多个精确匹配，不自动上屏
    }
    if has_longer {
        return None;
    }
    Some(first.text.clone())
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
        let (should_commit, commit_text) = match self.should_auto_commit(input, &candidates) {
            Some(text) => (true, text),
            None => (false, String::new()),
        };
        Ok(ConvertResult {
            candidates,
            preedit_display: input.to_string(),
            is_empty,
            should_commit,
            commit_text,
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

    fn should_auto_commit(&self, input: &str, candidates: &[Candidate]) -> Option<String> {
        decide_auto_commit(
            self.auto_commit_at_full,
            self.auto_commit_min_len,
            input,
            candidates,
            self.has_longer_code(input),
        )
    }

    fn handle_empty_code(&self, _input: &str) -> (bool, bool, String) {
        (true, false, String::new())
    }

    fn handle_top_code(&self, _input: &str) -> Option<(String, String)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wind_candidate::Candidate;
    use wind_dict::SystemDictLayer;
    use wind_dict::cached::CachedDict;
    use wind_dict::codetable::CodetableDict;

    fn cand(code: &str, text: &str) -> Candidate {
        Candidate {
            code: code.to_string(),
            text: text.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn decide_basic_unique_full() {
        let cands = [cand("aaaa", "工")];
        assert_eq!(
            decide_auto_commit(true, 4, "aaaa", &cands, false),
            Some("工".to_string())
        );
    }

    #[test]
    fn decide_blocked_when_disabled_or_short() {
        let cands = [cand("aaaa", "工")];
        assert_eq!(decide_auto_commit(false, 4, "aaaa", &cands, false), None);
        // 码长不足 min_len
        assert_eq!(
            decide_auto_commit(true, 4, "aaa", &[cand("aaa", "x")], false),
            None
        );
    }

    #[test]
    fn decide_blocked_when_ambiguous_or_has_longer() {
        // 两个精确匹配 → 不上屏
        let two = [cand("aaaa", "工"), cand("aaaa", "戈")];
        assert_eq!(decide_auto_commit(true, 4, "aaaa", &two, false), None);
        // 有更长后继 → 不上屏
        let one = [cand("aa", "式")];
        assert_eq!(decide_auto_commit(true, 2, "aa", &one, true), None);
    }

    fn engine_with(
        entries: &[(&str, &str, i32)],
        at_full: bool,
        min_len: usize,
    ) -> CodeTableEngine {
        let mut d = CodetableDict::empty();
        for (i, (code, text, w)) in entries.iter().enumerate() {
            d.merge_single(code.to_string(), text.to_string(), *w, i as i32);
        }
        let dm = DictManager::new();
        dm.register_layer(Box::new(SystemDictLayer::new(
            CachedDict::Memory(d),
            "codetable-system",
        )));
        CodeTableEngine::new(4, at_full, min_len, Arc::new(dm))
    }

    #[test]
    fn convert_sets_should_commit_for_unique_full_code() {
        // "aaaa" 唯一精确、无更长后继 → should_commit
        let e = engine_with(&[("aaaa", "工", 100)], true, 4);
        let r = e.convert("aaaa", 50).unwrap();
        assert!(r.should_commit, "唯一全码应自动上屏");
        assert_eq!(r.commit_text, "工");
    }

    #[test]
    fn convert_no_commit_when_longer_code_exists() {
        // "aaa" 精确存在，但还有更长 "aaaa" → 不自动上屏
        let e = engine_with(&[("aaa", "甲", 100), ("aaaa", "工", 90)], true, 3);
        let r = e.convert("aaa", 50).unwrap();
        assert!(!r.should_commit, "存在更长后继编码时不应自动上屏");
    }

    #[test]
    fn convert_no_commit_when_disabled() {
        let e = engine_with(&[("aaaa", "工", 100)], false, 4);
        let r = e.convert("aaaa", 50).unwrap();
        assert!(!r.should_commit);
    }
}
