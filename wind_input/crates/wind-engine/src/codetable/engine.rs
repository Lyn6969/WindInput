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

/// 码表上屏策略配置（schema 的 [engine.codetable] 相关开关）。
#[derive(Clone, Copy, Debug, Default)]
pub struct CommitOptions {
    /// 全码自动上屏（含 legacy auto_commit_unique 回退，调用方解析）
    pub auto_commit_at_full: bool,
    /// 自动上屏最短码长（0 跟随 max_code_length）
    pub auto_commit_min_len: usize,
    /// 满码无候选时清空缓冲
    pub clear_on_empty_max: bool,
    /// 超过满码长时取前 N 码顶字上屏
    pub top_code_commit: bool,
    /// 显示编码提示：码表方案下,给前缀候选标注「剩余编码」(候选全码去掉已输入前缀)。
    pub show_code_hint: bool,
}

/// 码表引擎
pub struct CodeTableEngine {
    max_code_length: usize,
    opts: CommitOptions,
    dm: Arc<DictManager>,
}

impl CodeTableEngine {
    pub fn new(max_code_length: usize, mut opts: CommitOptions, dm: Arc<DictManager>) -> Self {
        // min_len 为 0 时跟随 max_code_length（对齐 Go codetable.go:135）。
        if opts.auto_commit_min_len == 0 {
            opts.auto_commit_min_len = max_code_length;
        }
        Self {
            max_code_length,
            opts,
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

    /// `input` 是否存在精确（code==input）匹配。
    fn has_full_input_match(&self, input: &str) -> bool {
        !self.dm.search(input, 1).is_empty()
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
    /// 热插拔扩展词库：翻 composite 中 `codetable-extra-<id>` 层的 enabled 标志。
    fn set_dict_enabled(&self, dict_id: &str, enabled: bool) -> bool {
        self.dm
            .set_layer_enabled(&format!("codetable-extra-{dict_id}"), enabled)
    }

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

        // 编码提示(码表自身):前缀候选标注「剩余编码」=候选全码去掉已输入前缀(对齐 Go codetable.go)。
        // 精确候选(code==input)剩余为空 → 不标注。已有 comment 的候选不覆盖。
        if self.opts.show_code_hint {
            let input_len = input.chars().count();
            for c in candidates.iter_mut() {
                if c.comment.is_empty() && c.code.chars().count() > input_len {
                    c.comment = c.code.chars().skip(input_len).collect();
                }
            }
        }

        let is_empty = candidates.is_empty();
        let (should_commit, commit_text) = match self.should_auto_commit(input, &candidates) {
            Some(text) => (true, text),
            None => (false, String::new()),
        };
        // 满码空码清空：无候选 + 码长达满码 + 无更长后继（避免吞掉长码精确匹配）。
        let should_clear = is_empty
            && self.opts.clear_on_empty_max
            && input.chars().count() >= self.max_code_length
            && !self.has_longer_code(input);
        Ok(ConvertResult {
            candidates,
            preedit_display: input.to_string(),
            is_empty,
            should_commit,
            commit_text,
            should_clear,
            ..Default::default()
        })
    }

    fn reset(&self) {}

    fn engine_type(&self) -> EngineType {
        EngineType::CodeTable
    }

    fn max_code_length(&self) -> usize {
        self.max_code_length
    }

    fn has_full_input_match(&self, input: &str) -> bool {
        CodeTableEngine::has_full_input_match(self, input)
    }

    fn has_longer_code(&self, input: &str) -> bool {
        CodeTableEngine::has_longer_code(self, input)
    }

    /// 顶码上屏（对齐 Go HandleTopCode）：超过满码长 + 整串无精确匹配 + 无更长后继时，
    /// 取前 max_code_length 码的首选上屏，返回 (上屏文本, 剩余编码)。
    fn handle_top_code(&self, input: &str) -> Option<(String, String)> {
        if !self.opts.top_code_commit {
            return None;
        }
        if input.chars().count() <= self.max_code_length {
            return None;
        }
        // 整串若仍是精确匹配或有更长后继，说明不是「溢出顶字」，交回正常流程。
        if self.has_full_input_match(input) || self.has_longer_code(input) {
            return None;
        }
        let prefix: String = input.chars().take(self.max_code_length).collect();
        let remainder: String = input.chars().skip(self.max_code_length).collect();
        let r = self.convert(&prefix, 1).ok()?;
        let top = r.candidates.first()?;
        Some((top.text.clone(), remainder))
    }
}

impl ExtendedEngine for CodeTableEngine {
    fn max_code_length(&self) -> usize {
        self.max_code_length
    }

    fn should_auto_commit(&self, input: &str, candidates: &[Candidate]) -> Option<String> {
        decide_auto_commit(
            self.opts.auto_commit_at_full,
            self.opts.auto_commit_min_len,
            input,
            candidates,
            self.has_longer_code(input),
        )
    }

    fn handle_empty_code(&self, _input: &str) -> (bool, bool, String) {
        (true, false, String::new())
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
        engine_opts(
            entries,
            CommitOptions {
                auto_commit_at_full: at_full,
                auto_commit_min_len: min_len,
                ..Default::default()
            },
        )
    }

    fn engine_opts(entries: &[(&str, &str, i32)], opts: CommitOptions) -> CodeTableEngine {
        let mut d = CodetableDict::empty();
        for (i, (code, text, w)) in entries.iter().enumerate() {
            d.merge_single(code.to_string(), text.to_string(), *w, i as i32);
        }
        let dm = DictManager::new();
        dm.register_layer(Box::new(SystemDictLayer::new(
            CachedDict::Memory(d),
            "codetable-system",
        )));
        CodeTableEngine::new(4, opts, Arc::new(dm))
    }

    #[test]
    fn clear_on_empty_at_full_len() {
        // 满码(4) 无候选 + clear_on_empty_max → should_clear
        let e = engine_opts(
            &[("aaaa", "工", 100)],
            CommitOptions {
                clear_on_empty_max: true,
                ..Default::default()
            },
        );
        let r = e.convert("zzzz", 50).unwrap();
        assert!(r.is_empty && r.should_clear, "满码空码应请求清空");
        // 未满码的空码不清空
        let r2 = e.convert("zz", 50).unwrap();
        assert!(r2.is_empty && !r2.should_clear, "未满码空码不应清空");
    }

    #[test]
    fn top_code_commits_overflow_prefix() {
        // max=4，"aaaa"=工 唯一全码；输入 "aaaab"（>4，整串无匹配/无更长）→ 顶前4码"工"，余 "b"
        let e = engine_opts(
            &[("aaaa", "工", 100)],
            CommitOptions {
                top_code_commit: true,
                ..Default::default()
            },
        );
        let top = e.handle_top_code("aaaab");
        assert_eq!(top, Some(("工".to_string(), "b".to_string())));
        // 关闭开关 → None
        let e2 = engine_opts(&[("aaaa", "工", 100)], CommitOptions::default());
        assert_eq!(e2.handle_top_code("aaaab"), None);
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
