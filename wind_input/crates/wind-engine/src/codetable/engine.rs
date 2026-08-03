//! 码表引擎实现
//!
//! 与 Go 版本 `wind_input/internal/engine/codetable/` 对齐。
//!
//! 查询经 `DictManager`（CompositeDict）——系统词库 + （后续）用户/临时词层统一合并。
//! 候选生成：精确匹配 + 前缀匹配。运行时词频/shadow 不在此（见 frequency.md / dict.md）。

use crate::engine::{ConvertResult, Engine, EngineType, ExtendedEngine};
use std::collections::HashMap;
use std::sync::Arc;
use wind_candidate::{Candidate, CandidateSource, better, by_natural, cmp_exact_first};
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
    /// 解析配置字符串：`"natural"` → Natural，`""`/`"weight"` → Weight。
    ///
    /// 其余取值同样回退 Weight，但**会告警**：此前静默吞掉拼写错误，配置者只会观察到
    /// 「改了没生效」而拿不到任何线索。注意本项**不接受 librime 的 `by_weight`/`original`
    /// 拼法**——那是 `.dict.yaml` 里 rime 的库内同码排序键，与本项（方案级全局排序维度）
    /// 语义不同，故列为非法值而非别名，避免两套词汇被误当等价。
    pub fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case("natural") {
            Self::Natural
        } else {
            if !s.is_empty() && !s.eq_ignore_ascii_case("weight") {
                tracing::warn!(
                    value = %s,
                    "[engine.codetable].base_sort 取值无法识别，已回退 \"weight\"；合法值仅 \"weight\" / \"natural\""
                );
            }
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
    ///
    /// 走 `DictManager::has_longer_code` 直接问各层有序索引，而非「`search_prefix(input, 64)`
    /// 再 `.any(code 更长)`」——后者为一个 bool 遍历整棵前缀子树（`ok` 拼字这类单前缀
    /// 8.8 万条的词库上单次 20ms 级），且其判据经权重截断与跨层「同 text 取最短码」两道
    /// 变形，长码候选权重偏低时会漏判成 false，反而让不该自动上屏的情形上了屏。
    fn has_longer_code(&self, input: &str) -> bool {
        self.dm.has_longer_code(input)
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

    /// 空码枚举：空前缀查询从根遍历整表（datformat::search_prefix），已按 weight 降序 +
    /// order 升序排好并截断。标 CodeTable 来源供协调器统一处理。
    /// 注：大表会在字典层 materialize 全部条目再截断，仅宜用于小符号表的「进入即浏览」。
    ///
    /// 遵循精确匹配模式：`single_code_input`（关前缀枚举）时最多补 **1 条**——与空码补全
    /// `single_code_complete`「取首位后续码」同语义；非精确模式才枚举首页（`limit` 条）。
    fn enumerate(&self, limit: usize) -> Vec<Candidate> {
        let n = if self.opts.single_code_input {
            1
        } else {
            limit
        };
        self.dm
            .search_prefix("", n)
            .into_iter()
            .map(|mut c| {
                c.source = CandidateSource::CodeTable;
                c
            })
            .collect()
    }

    fn convert(&self, input: &str, max_candidates: usize) -> anyhow::Result<ConvertResult> {
        if input.is_empty() {
            return Ok(ConvertResult::default());
        }

        let limit = max_candidates.max(50);
        let mut candidates: Vec<Candidate> = Vec::new();
        // text -> 已入列候选的下标。**不能退回 `HashSet`**：同文本重复命中时要把被丢弃那条
        // 的码位并进幸存者（`absorb_codes_from`），否则「检索范围」过滤按 (source, code) 分组
        // 时会丢掉「该码位下有常用字」这一事实，见 `Candidate::merged_codes`。
        let mut seen: HashMap<String, usize> = HashMap::new();

        // 精确匹配优先（完整编码）
        for mut c in self.dm.search(input, limit) {
            // ⚠️ source 必须**先于** `absorb_codes_from` 赋值：该方法跨来源直接 return，
            // 而 `dm` 返回的候选 source 还是 `None`，晚一步赋值会让归并静默失效。
            c.source = CandidateSource::CodeTable;
            if let Some(&idx) = seen.get(&c.text) {
                candidates[idx].absorb_codes_from(&c);
                continue;
            }
            seen.insert(c.text.clone(), candidates.len());
            // 精确层级随候选流动，供协调器重排时沿用（见 `cmp_exact_first`）。
            c.is_exact_code = c.code == input;
            candidates.push(c);
        }

        // 前缀匹配补充（精确匹配模式下跳过）
        let mut completion_hint: Option<Candidate> = None;
        if !self.opts.single_code_input {
            for mut c in self.dm.search_prefix(input, limit) {
                // source 须先于 absorb 赋值，理由同上面的精确循环。
                c.source = CandidateSource::CodeTable;
                if let Some(&idx) = seen.get(&c.text) {
                    // 简码字在此被吃掉：打 `siv` 时「档」已由精确循环以 code="siv" 入列，
                    // 这条 code="sivg" 的同字条目被丢弃 —— 但 sivg 码位确实被一个常用字占着，
                    // 该事实必须留给「检索范围」过滤，否则同码位的生僻字（桜）会当孤儿码放行。
                    candidates[idx].absorb_codes_from(&c);
                    continue;
                }
                seen.insert(c.text.clone(), candidates.len());
                // 前缀扫描也会命中输入自身（"usr".starts_with("usr")）。正常情况该条已被
                // 上面的精确循环占位去重，此处按 code 判定只为不依赖循环先后顺序。
                c.is_exact_code = c.code == input;
                candidates.push(c);
            }
        } else if self.opts.single_code_complete
            && candidates.is_empty()
            && input.chars().count() < self.max_code_length
        {
            // 空码补全：从更长编码取首个候选作提示（对齐 Go 仅取 1 个）。
            // limit=8：仅需第一个 code != input 者，取 8 条留少量余量以跳过与输入同码项，
            // 避免全量前缀扫描开销。
            //
            // 只备货、不入列：`candidates.is_empty()` 在这一层只代表「码表没货」，而补全该不该
            // 出的判据是「最终屏幕上一条都没有」——协调器随后还要叠短语。就地 push 会在短语
            // 已命中时多冒一条后续编码。交由协调器按最终列表定夺，见 `ConvertResult::completion_hint`。
            completion_hint = self
                .dm
                .search_prefix(input, 8)
                .into_iter()
                .find(|c| c.code != input)
                .map(|mut c| {
                    c.source = CandidateSource::CodeTable;
                    c
                });
        }

        // 排序：精确匹配（code==input）优先，其内按基础维度 weight（默认，better）或
        // natural（by_natural，纯出现序、忽略权重）。
        //
        // 精确优先必须是**常驻主键**而非仅截断时的临时分区：词组权重取自词频、单字权重取自
        // 字频，两套量纲不可比，纯按权重排会让简码字沉底——如「新的」(usrq, 47487) 与
        // 「新手」(usrt, 22229) 双双压过简码「新」(usr, 11777)，把它挤到第三位。
        //
        // 该层级同时落在 `Candidate::is_exact_code` 上随候选流动：协调器合并短语后会用
        // `candidate_display_order` 无条件重排全部候选，只在此处排好而不落字段，下游重排即
        // 按纯权重推翻本层结果（此前的实际行为）。两处共用 `cmp_exact_first` 这一个键。
        let base_cmp = self.opts.base_sort.cmp();
        candidates.sort_by(|a, b| cmp_exact_first(a, b).then_with(|| base_cmp(a, b)));
        // 精确匹配已居首，截断不会再把它挤出配额（此前需一次临时分区保护：单字母等短输入下
        // 前缀候选可达数百，纯按基础序截断会让低权重简码字丢失，此后协调器再排也找不回）。
        candidates.truncate(max_candidates);

        // 编码提示(码表自身):前缀候选标注「剩余编码」=候选全码去掉已输入前缀(对齐 Go codetable.go)。
        // 精确候选(code==input)剩余为空 → 不标注。已有 comment 的候选不覆盖。
        if self.opts.show_code_hint {
            let input_len = input.chars().count();
            // 补全备选一并标注：它已移出 `candidates`（见上方 completion_hint），若不接进本循环，
            // 协调器采纳后会缺「剩余编码」注释——而它恰恰是全场最需要该提示的候选（码更长）。
            for c in candidates.iter_mut().chain(completion_hint.iter_mut()) {
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
            completion_hint,
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
        // 码表首选文本；prefix 码表无字（短语专属码如 date/zzbd）时留空，由上层用显示首选
        // （短语/命令）兜底顶码。此处**只判定溢出该顶**（超满码长 + 无全码匹配 + 无更长后继），
        // 「顶什么」交上层——原 `first()?` 短路会让码表无字时顶码整个不触发（短语顶不了）。
        let top = self
            .convert(&prefix, 1)
            .ok()
            .and_then(|r| r.candidates.first().map(|c| c.text.clone()))
            .unwrap_or_default();
        Some((top, remainder))
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
    fn top_code_overflow_prefix_no_char_returns_empty_top() {
        // prefix 码表无字（短语专属码场景）：仍判定溢出该顶，返回 Some(("", 余码))——
        // 「顶什么」交上层用短语显示首选兜底。原 `first()?` 短路会让顶码整个不触发。
        let e = engine_opts(
            &[("aaaa", "工", 100)],
            CommitOptions {
                top_code_commit: true,
                ..Default::default()
            },
        );
        // "bbbb" 无字，"bbbbc"(>4，无匹配/无更长后继) → Some(("", "c"))
        assert_eq!(
            e.handle_top_code("bbbbc"),
            Some((String::new(), "c".to_string())),
            "prefix 码表无字应返回空 top + 余码，而非 None"
        );
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
        // 补全候选走 `completion_hint` 旁路而**不入** `candidates`：该不该补取决于最终屏幕上
        // 有没有候选，而引擎看不见协调器随后叠加的短语，无权就地拍板（见 ConvertResult 文档）。
        assert!(r.candidates.is_empty(), "补全候选不应入引擎候选列表");
        let hint = r.completion_hint.expect("应备好空码补全候选");
        assert_eq!(hint.text, "你", "空码补全取更长编码首选");
        assert_eq!(hint.comment, "c", "补全候选应标注剩余编码");
        assert!(!r.should_commit, "补全候选不应触发自动上屏");
    }

    #[test]
    fn single_code_complete_hint_absent_without_longer_code() {
        // 无 "ab" 精确项、也无更长后继 → 无货可备。
        let e = engine_opts(
            &[("xy", "甲", 100)],
            CommitOptions {
                single_code_input: true,
                single_code_complete: true,
                ..Default::default()
            },
        );
        let r = e.convert("ab", 50).unwrap();
        assert!(r.candidates.is_empty());
        assert!(r.completion_hint.is_none(), "无更长编码时不应备补全候选");
    }

    #[test]
    fn exact_match_suppresses_completion_hint() {
        // 有 "ab" 精确项 → 不是空码，不该备补全（否则协调器侧判空虽拦得住，但白查一次前缀）。
        let e = engine_opts(
            &[("ab", "甲", 100), ("abc", "你", 90)],
            CommitOptions {
                single_code_input: true,
                single_code_complete: true,
                ..Default::default()
            },
        );
        let r = e.convert("ab", 50).unwrap();
        assert_eq!(r.candidates.len(), 1);
        assert!(r.completion_hint.is_none(), "有精确候选时不备补全");
    }

    #[test]
    fn exact_match_outranks_higher_weight_prefix_words() {
        // 真实现场（古精86五笔-深海词库）：简码 usr→「新」(11777)，前缀词组 usrq→「新的」(47487)、
        // usrt→「新手」(22229)。词组权重取自词频、单字取自字频，两套量纲不可比——纯按权重排会把
        // 简码「新」挤到第三位。精确匹配须恒居首，其后的前缀候选内部仍按权重降序。
        let e = engine_opts(
            &[
                ("usr", "新", 11777),
                ("usrq", "新的", 47487),
                ("usrt", "新手", 22229),
                ("usrp", "亲近", 1861),
            ],
            CommitOptions::default(),
        );
        let r = e.convert("usr", 50).unwrap();
        let order: Vec<&str> = r.candidates.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            order,
            vec!["新", "新的", "新手", "亲近"],
            "精确匹配应居首、其余按权重降序"
        );
        // 该层级必须落到字段上随候选流动：协调器合并短语后会无条件重排，只在引擎内排好而
        // 不标记，下游会按纯权重把结果推翻（本 bug 的原始成因）。
        assert!(
            r.candidates[0].is_exact_code,
            "精确候选须标记 is_exact_code 供协调器重排沿用"
        );
        assert!(
            r.candidates[1..].iter().all(|c| !c.is_exact_code),
            "前缀补全候选不应被标记为精确匹配"
        );
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
