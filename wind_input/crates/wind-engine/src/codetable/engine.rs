//! 码表引擎实现
//!
//! 与 Go 版本 `wind_input/internal/engine/codetable/` 对齐。
//!
//! 查询经 `DictManager`（CompositeDict）——系统词库 + （后续）用户/临时词层统一合并。
//! 候选生成：精确匹配 + 前缀匹配。运行时词频/shadow 不在此（见 frequency.md / dict.md）。

use crate::engine::{ConvertResult, Engine, EngineType, ExtendedEngine};
use std::collections::HashSet;
use std::sync::Arc;
use wind_candidate::{Candidate, CandidateSource, better, by_natural};
use wind_dict::DictManager;

/// 基础排序（`[engine.codetable].base_sort`）：候选**主排序维度**。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BaseSort {
    /// 按词库权重降序（默认；等权回退 natural_order）。行为 = `candidate::better`。
    #[default]
    Weight,
    /// 纯按 natural_order（词库出现序，含 base_order 层偏移）升序，**忽略权重**。
    /// 行为 = `candidate::by_natural`。用于"设计者按文件顺序排、不用权重"的词库。
    Natural,
}

impl BaseSort {
    /// 解析配置字符串：`"natural"` → Natural，其余（含空/`"weight"`）→ Weight。
    pub fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case("natural") {
            Self::Natural
        } else {
            Self::Weight
        }
    }

    /// 该模式对应的候选比较器。
    fn cmp(self) -> fn(&Candidate, &Candidate) -> std::cmp::Ordering {
        match self {
            Self::Weight => better,
            Self::Natural => by_natural,
        }
    }
}

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
    /// 精确匹配模式（关闭前缀匹配，对齐 Go SingleCodeInput）。
    pub single_code_input: bool,
    /// 精确匹配空码补全：精确无候选且未满码时，从更长编码取首选（对齐 Go SingleCodeComplete）。
    pub single_code_complete: bool,
    /// 基础排序维度（weight 降序 / natural 出现序）。见 [`BaseSort`]。
    pub base_sort: BaseSort,
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

        // 前缀匹配补充（精确匹配模式下跳过）
        if !self.opts.single_code_input {
            for mut c in self.dm.search_prefix(input, limit) {
                if seen.insert(c.text.clone()) {
                    c.source = CandidateSource::CodeTable;
                    candidates.push(c);
                }
            }
        } else if self.opts.single_code_complete
            && candidates.is_empty()
            && input.chars().count() < self.max_code_length
        {
            // 空码补全：从更长编码取首个候选作提示（对齐 Go 仅取 1 个）。
            // limit=8：仅需第一个 code != input 者，取 8 条留少量余量以跳过与输入同码项，
            // 避免全量前缀扫描开销。
            if let Some(mut c) = self
                .dm
                .search_prefix(input, 8)
                .into_iter()
                .find(|c| c.code != input)
            {
                c.source = CandidateSource::CodeTable;
                candidates.push(c);
            }
        }

        // 基础排序维度：weight（默认，better）或 natural（by_natural，纯出现序、忽略权重）。
        let base_cmp = self.opts.base_sort.cmp();
        candidates.sort_by(base_cmp);
        // 截断保护精确匹配：单字母等短输入下前缀候选可达数百，若纯按基础序截断，靠后的精确
        // 全码（code==input，如五笔一/二级简码）会被前缀词组挤出配额而丢失（此后协调器
        // 再排也找不回）。仅在超额时做一次「精确优先」稳定分区截断——精确候选必留、其余按
        // base_cmp 序填满剩余配额——再恢复 base_cmp 显示序。不持久化 is_prefix：跨来源权重档位
        // （混输码表 ÷100 拼音等）不受影响，纯码表显示序也维持基础排序主导。
        if candidates.len() > max_candidates {
            candidates.sort_by(|a, b| {
                (a.code != input)
                    .cmp(&(b.code != input))
                    .then_with(|| base_cmp(a, b))
            });
            candidates.truncate(max_candidates);
            candidates.sort_by(base_cmp);
        }

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
        // has_longer 一次求值复用：自动上屏判定与满码空码清空共用同一「更长后继」前缀扫描，
        // 避免每次按键各查一次 search_prefix（此前经 should_auto_commit + should_clear 两次）。
        let has_longer = self.has_longer_code(input);
        let (should_commit, commit_text) = match decide_auto_commit(
            self.opts.auto_commit_at_full,
            self.opts.auto_commit_min_len,
            input,
            &candidates,
            has_longer,
        ) {
            Some(text) => (true, text),
            None => (false, String::new()),
        };
        // 满码空码清空：无候选 + 码长达满码 + 无更长后继（避免吞掉长码精确匹配）。
        let should_clear = is_empty
            && self.opts.clear_on_empty_max
            && input.chars().count() >= self.max_code_length
            && !has_longer;
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

    /// natural 模式（`base_sort = "natural"`）忽略权重：协调器据此对齐 `by_natural` 重排。
    fn base_sort_ignores_weight(&self) -> bool {
        matches!(self.opts.base_sort, BaseSort::Natural)
    }

    fn has_full_input_match(&self, input: &str) -> bool {
        CodeTableEngine::has_full_input_match(self, input)
    }

    fn has_longer_code(&self, input: &str) -> bool {
        CodeTableEngine::has_longer_code(self, input)
    }

    /// 顶码上屏（对齐 Go HandleTopCode）：超过满码长 + 整串无精确匹配 + 无更长后继时，
    /// 取前 max_code_length 码的首选上屏，返回 (上屏文本, 剩余编码)。
    fn recheck_auto_commit(&self, input: &str, candidates: &[Candidate]) -> Option<String> {
        decide_auto_commit(
            self.opts.auto_commit_at_full,
            self.opts.auto_commit_min_len,
            input,
            candidates,
            self.has_longer_code(input),
        )
    }

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

    #[test]
    fn recheck_auto_commit_unique_after_filter() {
        // 同码两个精确候选（"hhnu"→X 常用 / 愳 生僻）：引擎按未过滤候选判不唯一 → 不上屏。
        let e = engine_with(&[("hhnu", "X", 100), ("hhnu", "愳", 1)], true, 4);
        let r = e.convert("hhnu", 50).unwrap();
        assert!(!r.should_commit, "两个精确同码候选不自动上屏");
        // 模拟智能过滤后仅剩一个精确全码候选 → 复评放行。
        let filtered = [cand("hhnu", "X")];
        assert_eq!(
            e.recheck_auto_commit("hhnu", &filtered),
            Some("X".to_string()),
            "过滤后唯一精确全码应复评放行"
        );
        // 满码上屏开关关闭时复评不放行。
        let e_off = engine_with(&[("hhnu", "X", 100), ("hhnu", "愳", 1)], false, 4);
        assert_eq!(e_off.recheck_auto_commit("hhnu", &filtered), None);
    }

    #[test]
    fn single_code_input_disables_prefix() {
        // 词典：精确 "aa"→"式"，更长 "aab"→"想"。开启精确匹配后 "aa" 只应出 "式"。
        let e = engine_opts(
            &[("aa", "式", 100), ("aab", "想", 90)],
            CommitOptions {
                single_code_input: true,
                ..Default::default()
            },
        );
        let r = e.convert("aa", 50).unwrap();
        assert_eq!(r.candidates.len(), 1, "精确匹配模式不应含前缀候选");
        assert_eq!(r.candidates[0].text, "式");
    }

    #[test]
    fn single_code_complete_fills_from_longer_code() {
        // 无 "ab" 精确项；补全应从 "abc"→"你" 取首选，且仅一个。
        let e = engine_opts(
            &[("abc", "你", 100), ("abd", "他", 90)],
            CommitOptions {
                single_code_input: true,
                single_code_complete: true,
                show_code_hint: true,
                ..Default::default()
            },
        );
        let r = e.convert("ab", 50).unwrap();
        assert_eq!(r.candidates.len(), 1, "空码补全仅取首选");
        assert_eq!(r.candidates[0].text, "你");
        assert_eq!(r.candidates[0].comment, "c", "补全候选应标注剩余编码");
        assert!(!r.should_commit, "补全候选不应触发自动上屏");
    }

    #[test]
    fn truncate_protects_low_weight_exact_match() {
        // 精确全码 "aa"→式(权重 1) + 5 个高权重前缀词(code="aab".."aaf",权重 1000)。
        // max_candidates=3：纯按权重截断会把低权重精确「式」挤出配额丢失；分区保护须保留它。
        let e = engine_opts(
            &[
                ("aa", "式", 1),
                ("aab", "A", 1000),
                ("aac", "B", 1000),
                ("aad", "C", 1000),
                ("aae", "D", 1000),
                ("aaf", "E", 1000),
            ],
            CommitOptions::default(),
        );
        let r = e.convert("aa", 3).unwrap();
        assert_eq!(r.candidates.len(), 3, "应截断到 3 条");
        assert!(
            r.candidates.iter().any(|c| c.text == "式"),
            "低权重精确全码不应被高权重前缀词截断挤出，实际: {:?}",
            r.candidates
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn single_code_complete_off_yields_empty() {
        let e = engine_opts(
            &[("abc", "你", 100)],
            CommitOptions {
                single_code_input: true,
                ..Default::default()
            },
        );
        let r = e.convert("ab", 50).unwrap();
        assert!(r.is_empty, "补全关闭时无精确匹配应为空");
    }

    #[test]
    fn base_sort_natural_ignores_weight_uses_appearance_order() {
        // 同码 "aa" 两候选：低权重"低"先出现（order 0）、高权重"高"后出现（order 1）。
        let entries = &[("aa", "低", 1), ("aa", "高", 100)];
        // natural：忽略权重，按出现序 → 低、高。
        let e = engine_opts(
            entries,
            CommitOptions {
                base_sort: BaseSort::Natural,
                ..Default::default()
            },
        );
        let t: Vec<String> = e
            .convert("aa", 50)
            .unwrap()
            .candidates
            .into_iter()
            .map(|c| c.text)
            .collect();
        assert_eq!(t, vec!["低", "高"], "natural 应按出现序、忽略权重");
        // weight（默认）：高权重在前 → 高、低。
        let e2 = engine_opts(entries, CommitOptions::default());
        let t2: Vec<String> = e2
            .convert("aa", 50)
            .unwrap()
            .candidates
            .into_iter()
            .map(|c| c.text)
            .collect();
        assert_eq!(t2, vec!["高", "低"], "weight 应按权重降序");
    }

    #[test]
    fn base_sort_parse_maps_strings() {
        assert_eq!(BaseSort::parse("natural"), BaseSort::Natural);
        assert_eq!(BaseSort::parse("Natural"), BaseSort::Natural);
        assert_eq!(BaseSort::parse("weight"), BaseSort::Weight);
        assert_eq!(BaseSort::parse(""), BaseSort::Weight);
        assert_eq!(BaseSort::parse("xyz"), BaseSort::Weight);
    }

    /// 构造双层码表引擎（贴近真实多词库方案）：
    /// - 主库 `codetable-system`（base_order 0，**带权重**）：同码 "aa" 两条——"主低"(w10,出现序0)、
    ///   "主高"(w100,出现序1)，故权重序与出现序**相反**（用于区分 weight/natural）。
    /// - 扩展库 `codetable-extra-x`（base_order 1，**无权重**，default_weight=50）：同码 "aa" 一条 "扩"。
    fn engine_two_layers(opts: CommitOptions) -> CodeTableEngine {
        let mut main = CodetableDict::empty();
        main.merge_single("aa".into(), "主低".into(), 10, 0);
        main.merge_single("aa".into(), "主高".into(), 100, 1);
        let mut ext = CodetableDict::empty();
        ext.merge_single("aa".into(), "扩".into(), 0, 0);

        let dm = DictManager::new();
        dm.register_layer(Box::new(SystemDictLayer::new(
            CachedDict::Memory(main),
            "codetable-system",
        )));
        dm.register_layer(Box::new(
            SystemDictLayer::with_enabled(CachedDict::Memory(ext), "codetable-extra-x", true)
                .with_base_order(1)
                .with_default_weight(Some(50)),
        ));
        CodeTableEngine::new(4, opts, Arc::new(dm))
    }

    fn texts_of(e: &CodeTableEngine, input: &str) -> Vec<String> {
        e.convert(input, 50)
            .unwrap()
            .candidates
            .into_iter()
            .map(|c| c.text)
            .collect()
    }

    #[test]
    fn multi_layer_weight_mode_weight_primary_default_weight_places_ext() {
        // weight 模式（默认）：权重主导 → 主高(100) > 扩(50, 由 default_weight) > 主低(10)。
        // 证明：① 权重优先于 base_order（主低虽 base_order 0 却因低权重沉底）；
        //       ② default_weight 让无权重扩展库落在 50 档（介于 100 与 10 之间）。
        let e = engine_two_layers(CommitOptions::default());
        assert_eq!(
            texts_of(&e, "aa"),
            vec!["主高", "扩", "主低"],
            "weight 模式应权重主导 + default_weight 定档"
        );
    }

    #[test]
    fn multi_layer_natural_mode_base_order_tiers_dicts_ignores_weight() {
        // natural 模式：忽略权重，按 base_order 档位分组、组内按出现序。
        // → 主库(base_order 0)整组在前：主低(出现序0)、主高(出现序1)；扩展库(base_order 1)在后：扩。
        // 证明：① base_order 分档把整个扩展库排到主库之后（与条目权重无关）；
        //       ② 组内忽略权重按出现序（主低虽权重低却因出现序靠前而在主高之前）。
        let e = engine_two_layers(CommitOptions {
            base_sort: BaseSort::Natural,
            ..Default::default()
        });
        assert_eq!(
            texts_of(&e, "aa"),
            vec!["主低", "主高", "扩"],
            "natural 模式应按 base_order 分档 + 组内出现序、忽略权重"
        );
    }
}
