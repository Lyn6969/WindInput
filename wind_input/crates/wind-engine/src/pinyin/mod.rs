//! 拼音输入引擎
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/` 对齐。
//!
//! 候选生成流程（对齐 Go convertCore）：
//! 1. 精确查找（完整音节 join 无空格）
//! 2. Viterbi 长句解码（>=2 音节）
//! 3. DAG 子短语查找
//! 4. 前缀查找
//! 5. 缩写/简拼匹配
//!
//! 注意：运行时词频 boost 由上层（协调器）应用，本引擎只产出基础权重候选。

pub mod dag;
pub mod fuzzy;
pub mod generate;
pub mod lattice;
pub mod lm;
pub mod parser;
pub mod scorer;
pub mod shuangpin;
pub mod syllable;
pub mod viterbi;

use crate::engine::{ConvertResult, Engine, EngineType};
use dag::Dag;
use fuzzy::FuzzyConfig;
use generate::CharPinyinIndex;
use lattice::LatticeBuilder;
use lm::UnigramLookup;
use scorer::AbbrevMatcher;
use shuangpin::ShuangpinConverter;
use std::sync::{Arc, OnceLock};
use syllable::SyllableTrie;
use viterbi::{ViterbiDecoder, WordNode};
use wind_candidate::{Candidate, CandidateSource};
use wind_dict::DictManager;
use wind_dict::cached::CachedDict;

/// 整句候选权重基准（高于拼音词频上限 ~19260817，确保整句置顶且不被截断）
const SENTENCE_WEIGHT_BASE: i32 = 30_000_000;

/// 裸声母（无完整音节，如 "m"）单字提权：使单字候选（吗/么）排在多字前缀补全词
/// （没有/目前）之前——对齐主流输入法「首字优先」。取 1e7：高于常规词频（单字基础权重上限
/// ~2e6），稳压多字词；又低于整句底线 PINYIN_SENTENCE_FLOOR(2e7)，不会被 freq_rerank 误当整句
/// 锚定。提权改的是 weight，故能穿过协调器按权重的重排（否则引擎内单字优先会被 build_candidates
/// 重排冲掉）。仅裸声母（syllables 为空）时应用——完整音节输入的单字已靠 is_prefix 层级就位。
const BARE_INITIAL_SINGLE_CHAR_BOOST: i32 = 10_000_000;

/// 拼音引擎配置
#[derive(Debug, Clone)]
pub struct Config {
    pub show_code_hint: bool,
    pub use_smart_compose: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            show_code_hint: false,
            use_smart_compose: true,
        }
    }
}

/// 拼音引擎
pub struct PinyinEngine {
    /// 引擎配置（show_code_hint / use_smart_compose 等）
    config: Config,
    dict: CachedDict,
    trie: SyllableTrie,
    viterbi: ViterbiDecoder,
    lattice_builder: LatticeBuilder,
    fuzzy_config: FuzzyConfig,
    /// Unigram 语言模型（长句 Viterbi 打分；缺失时回退词典权重）
    unigram: Option<Arc<dyn UnigramLookup>>,
    /// 用户/临时造词层（L 造词显现）：仅含 StoreUserLayer/StoreTempLayer，无系统层。
    /// 拼音候选除主词典外，按相同的码并入这些层的用户造词（None=无持久化，如纯测试）。
    store_layers: Option<Arc<DictManager>>,
    /// 造词反推用的单字读音索引（懒构建：首次 generate_word_pinyin 时从词典派生）。
    char_pinyin_idx: OnceLock<CharPinyinIndex>,
    /// 双拼转换器（None 表示全拼模式，输入原样传递）。
    shuangpin: Option<ShuangpinConverter>,
}

impl PinyinEngine {
    pub fn new(config: Config, dict: CachedDict) -> Self {
        Self::with_unigram(config, dict, None)
    }

    pub fn with_unigram(
        config: Config,
        dict: CachedDict,
        unigram: Option<Arc<dyn UnigramLookup>>,
    ) -> Self {
        Self {
            config,
            dict,
            trie: SyllableTrie::new(),
            viterbi: ViterbiDecoder::new(),
            lattice_builder: LatticeBuilder::new(),
            fuzzy_config: FuzzyConfig::default(),
            unigram,
            store_layers: None,
            char_pinyin_idx: OnceLock::new(),
            shuangpin: None,
        }
    }

    /// 注入用户/临时造词层（L 造词显现）。链式 builder：构造后由 EngineManager 按 schema 挂上。
    pub fn with_store_layers(mut self, layers: Arc<DictManager>) -> Self {
        self.store_layers = Some(layers);
        self
    }

    /// 注入模糊音配置（取代 with_unigram 中的 FuzzyConfig::default()）。
    pub fn with_fuzzy(mut self, fuzzy: FuzzyConfig) -> Self {
        self.fuzzy_config = fuzzy;
        self
    }

    /// 注入双拼转换器（链式 builder）。注入后 convert/compute_composition 均先把输入转为全拼。
    pub fn with_shuangpin(mut self, conv: ShuangpinConverter) -> Self {
        self.shuangpin = Some(conv);
        self
    }

    /// 仅测试用：读取 fuzzy_config.zh_z 以验证 with_fuzzy 注入是否生效。
    #[cfg(test)]
    pub fn fuzzy_zh_z(&self) -> bool {
        self.fuzzy_config.zh_z
    }

    /// 总条目数
    pub fn entry_count(&self) -> usize {
        self.dict.len()
    }

    /// 从起始位置贪心切出连续完整音节（每步取最长匹配），返回 (音节序列, 结束字节位置)。
    /// 对齐 Go `ContiguousCompletedFromStart`：遇到无完整音节即停（残缺尾部不计入）。
    fn contiguous_completed_from_start(&self, prefix: &str) -> (Vec<String>, usize) {
        let mut syllables = Vec::new();
        let mut pos = 0;
        while pos < prefix.len() {
            // match_at 返回该位置所有完整音节，最长优先；取最长贪心推进。
            let matches = self.trie.match_at(prefix, pos);
            let Some(syl) = matches.into_iter().next() else {
                break;
            };
            pos += syl.len();
            syllables.push(syl);
        }
        (syllables, pos)
    }

    /// 计算 preedit 显示与音节信息。
    /// `full_pinyin` 必须已是全拼串（调用方负责转换），本方法不再内部做双拼→全拼转换。
    ///
    /// 含手动分隔符 `'` 时：按 `'` 分段各自组合，段间以 `'` 重新连接——保留全部手动边界
    /// （含开头 / 结尾 / 连续 `''`），使末尾 `'` 立即可见。段内仍走自动分词。
    fn compute_composition(&self, full_pinyin: &str) -> (String, Vec<String>, String) {
        if !full_pinyin.contains('\'') {
            return self.compose_segment(full_pinyin);
        }
        let mut all_syllables: Vec<String> = Vec::new();
        let mut last_partial = String::new();
        let mut rendered: Vec<String> = Vec::new();
        for seg in full_pinyin.split('\'') {
            if seg.is_empty() {
                rendered.push(String::new());
                continue;
            }
            let (seg_pre, seg_syls, seg_partial) = self.compose_segment(seg);
            rendered.push(seg_pre);
            all_syllables.extend(seg_syls);
            last_partial = seg_partial;
        }
        let preedit = rendered.join("'");
        (preedit, all_syllables, last_partial)
    }

    /// 对单个「无分隔符」片段做自动分词并组合 preedit（原 compute_composition 逻辑）。
    fn compose_segment(&self, full_pinyin: &str) -> (String, Vec<String>, String) {
        let input = full_pinyin;
        let dag = Dag::build(input, &self.trie);
        let syllables = dag.maximum_match();
        let consumed: usize = syllables.iter().map(|s| s.len()).sum();
        let partial = if consumed < input.len() {
            input[consumed..].to_string()
        } else {
            String::new()
        };

        let mut preedit = syllables.join("'");
        if !partial.is_empty() {
            if !preedit.is_empty() {
                preedit.push('\'');
            }
            preedit.push_str(&partial);
        }
        if preedit.is_empty() {
            preedit = input.to_string();
        }
        (preedit, syllables, partial)
    }

    /// 尊重手动分隔符 `'` 的音节分段：按 `'` 切段、各段独立 DAG 最大匹配，
    /// 拼接为纯音节序列（不含 `'`）。`'` 为硬边界，任何音节不得跨越。
    /// 段内未成音节的残码（partial）不计入（仅用于 completed 音节序列）。
    fn segment_with_separators(&self, input: &str) -> Vec<String> {
        let mut syllables = Vec::new();
        for seg in input.split('\'') {
            if seg.is_empty() {
                continue;
            }
            syllables.extend(Dag::build(seg, &self.trie).maximum_match());
        }
        syllables
    }

    /// 由全拼码现算简拼（各音节声母拼接），供用户/临时造词层动态简拼匹配。
    /// 系统词库规模大，离线预建 AbbrevSection 索引（性能考量）；用户词库规模小，
    /// 现场切分+取声母足够快，无需为其单独建索引/维护写入时的双写一致性。
    /// 切分未完全覆盖 code（残码/非法拼音）时返回 None，不参与简拼匹配。
    fn abbrev_of_code(&self, code: &str) -> Option<String> {
        let syllables = self.segment_with_separators(code);
        let consumed: usize = syllables.iter().map(|s| s.len()).sum();
        if syllables.is_empty() || consumed != code.len() {
            return None;
        }
        Some(syllables.iter().filter_map(|s| s.chars().next()).collect())
    }

    /// 带模糊拼音扩展的词库查找（对齐 Go lookupWithFuzzy）。
    /// `code` 为待查询的全拼码（整串或前缀子码）；`syllables` 为该码对应的音节切分，
    /// 用于生成模糊变体。返回与 `dict.search` 相同的 `(text, weight, order)`。
    ///
    /// fuzzy 全 false 时 fuzzy_variants 返回空 → 天然退化为纯 `dict.search`（无需 enabled 判断）。
    /// 返回 `(text, weight, order, is_fuzzy)`：原 code 精确命中 is_fuzzy=false；
    /// 模糊变体命中 is_fuzzy=true（供排序时整体降到精确候选之后）。
    fn lookup_with_fuzzy(&self, code: &str, syllables: &[String]) -> Vec<(String, i32, i32, bool)> {
        let mut results: Vec<(String, i32, i32, bool)> = self
            .dict
            .search(code)
            .into_iter()
            .map(|(t, w, o)| (t, w, o, false))
            .collect();
        let mut seen: std::collections::HashSet<String> =
            results.iter().map(|(t, _, _, _)| t.clone()).collect();

        if syllables.len() <= 1 {
            // 单音节：对该音节（无切分时退化为整码）生成变体逐个查询。
            let syllable: &str = if syllables.len() == 1 {
                &syllables[0]
            } else {
                code
            };
            for variant in fuzzy::FuzzyMatcher::fuzzy_variants(syllable, &self.fuzzy_config) {
                for (text, weight, order) in self.dict.search(&variant) {
                    if seen.insert(text.clone()) {
                        results.push((text, weight, order, true));
                    }
                }
            }
        } else {
            // 多音节：笛卡尔积展开各音节变体，拼成完整 altCode 查询。
            for alt_code in self.expand_code(syllables) {
                if alt_code == code {
                    continue;
                }
                for (text, weight, order) in self.dict.search(&alt_code) {
                    if seen.insert(text.clone()) {
                        results.push((text, weight, order, true));
                    }
                }
            }
        }

        results
    }

    /// 对多音节做模糊变体笛卡尔积展开（对齐 Go FuzzyConfig.ExpandCode）。
    /// 每个音节取 `[原音节] + fuzzy_variants(音节)`，做笛卡尔积拼接成完整 code。
    /// 组合数超过上限（64）时跳过扩展返回空，避免组合爆炸。
    fn expand_code(&self, syllables: &[String]) -> Vec<String> {
        let per_syllable: Vec<Vec<String>> = syllables
            .iter()
            .map(|s| {
                let mut opts = vec![s.clone()];
                opts.extend(fuzzy::FuzzyMatcher::fuzzy_variants(s, &self.fuzzy_config));
                opts
            })
            .collect();

        // 预估组合数，超限直接放弃扩展，避免组合爆炸。
        let mut combo_count: usize = 1;
        for opts in &per_syllable {
            combo_count = combo_count.saturating_mul(opts.len());
            if combo_count > 64 {
                return Vec::new();
            }
        }

        let mut codes: Vec<String> = vec![String::new()];
        for opts in &per_syllable {
            let mut next: Vec<String> = Vec::with_capacity(codes.len() * opts.len());
            for prefix in &codes {
                for opt in opts {
                    next.push(format!("{prefix}{opt}"));
                }
            }
            codes = next;
        }
        codes
    }
}

/// 候选码 `code` 是否恰好落在前 k 个音节的边界上（`syllables[..k].join("") == code`）。
/// 命中返回 `Some(k)`（k>=1）；不落任何边界（如前缀补全的超长码）返回 `None`。
/// 供手动分隔符边界过滤：判断候选字数是否与所跨音节数一致。
fn syllable_span(syllables: &[String], code: &str) -> Option<usize> {
    if code.is_empty() {
        return None;
    }
    let mut acc = String::new();
    for (i, s) in syllables.iter().enumerate() {
        acc.push_str(s);
        if acc.len() > code.len() {
            break;
        }
        if acc == code {
            return Some(i + 1);
        }
    }
    None
}

/// 把「剥除分隔符 `'` 后的 query 空间」消费字节数回映射到「含 `'` 的原始输入空间」字节数。
/// 引擎按剥除 `'` 的 query 计算 consumed_length，而协调器按含 `'` 的原始缓冲切片提交，
/// 二者失配会致分隔符后残留尾字符（xi'an 选「西安」残 "n"、两步流残 "'an"）。此函数补偿
/// `'` 偏移，使 consumed_length 语义统一为「原始输入空间」（与双拼 map_consumed_length 同域）。
///
/// 规则：消费边界紧跟分隔符时，`'` 归入已消费侧（两步流残留 "an" 而非 "'an"）；连续 `''` 一并
/// 吸收。无 `'` 输入时恒等（零回归）。`'` 与拼音键均为 ASCII，按字节处理安全。
fn map_consumed_over_separators(input: &str, fp_consumed: usize) -> usize {
    if fp_consumed == 0 {
        return 0;
    }
    let bytes = input.as_bytes();
    let mut non_sep = 0usize; // 已跨过的非分隔符字节数（query 空间计数）
    let mut i = 0usize;
    while i < bytes.len() && non_sep < fp_consumed {
        if bytes[i] != b'\'' {
            non_sep += 1;
        }
        i += 1;
    }
    // 消费边界紧跟的分隔符并入已消费侧（连续 `''` 一并吸收），使已消费段带走其后的手动边界。
    while i < bytes.len() && bytes[i] == b'\'' {
        i += 1;
    }
    i
}

/// Fix A：用双拼原始按键重建 preedit（按音节边界以空格分隔）。
/// 依次取每个已完成音节在原始输入中的字节区间 `raw[sp_start..sp_end]`；
/// 若有 partial，把最后一个完成音节之后的剩余原始字节作为 partial 段追加。
/// 分隔符与全拼自动分词一致用 `'`（更省空间、观感更好）。
/// 双拼键均为 ASCII，字节切片安全。
fn build_raw_preedit(raw_input: &str, sp: &shuangpin::SpConvertResult) -> String {
    if raw_input.is_empty() {
        return String::new();
    }
    let mut segments: Vec<&str> = Vec::new();
    let mut last_end = 0usize;
    for s in &sp.syllables {
        segments.push(&raw_input[s.sp_start..s.sp_end]);
        last_end = s.sp_end;
    }
    if sp.has_partial && last_end < raw_input.len() {
        segments.push(&raw_input[last_end..]);
    }
    if segments.is_empty() {
        // 无 syllables 且无 partial：原样返回（如无匹配键对等边界）。
        return raw_input.to_string();
    }
    segments.join("'")
}

impl Engine for PinyinEngine {
    fn convert(&self, input: &str, max_candidates: usize) -> anyhow::Result<ConvertResult> {
        if input.is_empty() {
            return Ok(ConvertResult::default());
        }

        // 双拼方案不支持手动音节分隔符 `'`：若含 `'` 先整体剥除，保持双拼转换/preedit 原语义。
        // 手动分隔符仅在全拼路径（shuangpin=None）生效。
        let sp_stripped: String;
        let input: &str = if self.shuangpin.is_some() && input.contains('\'') {
            sp_stripped = input.chars().filter(|&c| c != '\'').collect();
            &sp_stripped
        } else {
            input
        };

        // Fix A：在任何 shadow 之前保存用户实际输入的原始字符（双拼键序列或全拼）。
        // 仅用于重建 preedit_display（显示原始按键），不影响候选/消费语义。
        let raw_input = input;

        // 双拼激活时保留 SpConvertResult，以便后续用 map_consumed_length 回算消费键数。
        let sp_result: Option<shuangpin::SpConvertResult> =
            self.shuangpin.as_ref().map(|conv| conv.convert(input));
        let full_owned: String = match &sp_result {
            Some(r) if !r.full_pinyin.is_empty() => r.full_pinyin.clone(),
            Some(_) => input.to_string(),
            None => input.to_string(),
        };
        let input = full_owned.as_str();

        // 手动音节分隔符 `'` 支持（全拼路径）：
        // - `has_sep`：输入含手动分隔符，走边界感知分词 + 剥除查询。
        // - `query`：剥除 `'` 后的纯拼音串（词典查询用）；音节边界信息来自带 `'` 的分段。
        let has_sep = input.contains('\'');
        let query_owned: String = if has_sep {
            input.chars().filter(|&c| c != '\'').collect()
        } else {
            String::new()
        };
        let query: &str = if has_sep { query_owned.as_str() } else { input };

        // 纯分隔符输入（如 `'` / `''`）：无可查询拼音，仅回显分隔符，不产候选。
        if has_sep && query.is_empty() {
            let (preedit, _, _) = self.compute_composition(input);
            return Ok(ConvertResult {
                preedit_pinyin: preedit.clone(),
                preedit_display: preedit,
                is_empty: true,
                ..Default::default()
            });
        }

        let dict = &self.dict;
        let trie = &self.trie;
        let mut candidates: Vec<Candidate> = Vec::new();

        let push_unique = |cands: &mut Vec<Candidate>,
                           text: String,
                           code: String,
                           weight: i32,
                           order: i32,
                           is_fuzzy: bool,
                           is_prefix: bool| {
            if text.is_empty() || cands.iter().any(|c| c.text == text) {
                return;
            }
            // 子短语候选：code 是输入的真前缀（比输入短，如 baoan 的「报」(bao)）。
            // Viterbi 整句走 insert(0) 不经此闭包，故无需 weight 启发式即可排除整句。
            // 注：以剥除分隔符后的 query 为基准（无分隔符时 query==input，行为不变）。
            let is_partial = !is_prefix && code.len() < query.len() && query.starts_with(&code);
            cands.push(Candidate {
                text,
                code,
                weight,
                natural_order: order,
                source: CandidateSource::Pinyin,
                is_fuzzy,
                is_prefix,
                is_partial,
                ..Default::default()
            });
        };

        // DAG 分词提前到 step1 之前：lookup_with_fuzzy 需要音节列表生成模糊变体。
        // 含手动分隔符时按 `'` 分段独立分词（`'` 为硬边界，音节不得跨越），否则整串 DAG。
        let syllables = if has_sep {
            self.segment_with_separators(input)
        } else {
            Dag::build(input, trie).maximum_match()
        };

        // 完成音节覆盖的连续前缀（从起点算）。尾部不成音节的残码（如「nihaom」的「m」）
        // 不参与精确匹配/整句解码——否则 lattice 到不了残码末端、Viterbi 失败、整句退化成单字，
        // 且精确层会把「nihao」当模糊变体误标 is_fuzzy 沉底被截断（bug①）。
        let completed_len: usize = syllables.iter().map(|s| s.len()).sum();
        // 含分隔符时用音节直接拼接（避免 `'` 字节位错位）；无分隔符时 query==input，等价原切片。
        let completed_owned: String;
        let completed: &str = if has_sep {
            completed_owned = syllables.join("");
            &completed_owned
        } else {
            &query[..completed_len]
        };

        // 1. 精确查找（完整匹配，含模糊扩展，对齐 Go lookupWithFuzzy）。
        //    以 completed（完成音节前缀）而非 query（可能含尾部残码）为查询码与存储 code：
        //    残码存在时（nihaom）query 非合法音节序列，search(query) 为空，而 lookup_with_fuzzy
        //    的 expand_code「全原音节」组合会命中 completed 的精确匹配——但因 `alt_code == code`
        //    守卫按 query 比较而失配，被误标 is_fuzzy=true 沉底、遭 truncate 截断（bug①）。
        //    传 completed 后守卫正确跳过全原组合（精确匹配 is_fuzzy=false）；code 存 completed 使
        //    残码输入的 consumed_length 只覆盖完成音节（nihao 消费 5 留 m 续输）。
        for (text, weight, order, is_fuzzy) in self.lookup_with_fuzzy(completed, &syllables) {
            push_unique(
                &mut candidates,
                text,
                completed.to_string(),
                weight,
                order,
                is_fuzzy,
                false,
            );
        }

        // 2. Viterbi 长句解码（>=2 音节，仅在完成音节前缀上跑；use_smart_compose=false 时跳过）
        if self.config.use_smart_compose && syllables.len() >= 2 {
            let lattice_nodes = self.lattice_builder.build(
                completed,
                trie,
                dict,
                Some(&self.fuzzy_config),
                self.unigram.as_deref(),
            );
            let input_len = completed.len();
            let mut lattice: Vec<Vec<WordNode>> = vec![Vec::new(); input_len + 1];
            for (end_pos, nodes_at_end) in lattice_nodes.iter().enumerate() {
                if end_pos > input_len {
                    continue;
                }
                for node in nodes_at_end {
                    lattice[end_pos].push(WordNode {
                        start: node.start,
                        end: node.end,
                        word: node.word.clone(),
                        log_prob: node.log_prob,
                    });
                }
            }
            let result = self.viterbi.decode(&lattice, input_len);
            // 仅接受有限概率的完整路径：解码失败时 log_prob 为 NEG_INFINITY，
            // 不能把这种空/错误路径强插到首选位置。
            if !result.words.is_empty() && result.log_prob.is_finite() {
                let sentence: String = result.words.join("");
                if !sentence.is_empty() {
                    // 整句优先：给予高权重置顶（log_prob 为负，原 .max(1) 会被截断淘汰）。
                    // clamp + saturating_add 防止超长低频句的 log_prob 溢出 i32 导致沉底/panic。
                    let log_offset = (result.log_prob * 1000.0)
                        .clamp(-(SENTENCE_WEIGHT_BASE as f64), 0.0)
                        as i32;
                    let weight = SENTENCE_WEIGHT_BASE.saturating_add(log_offset);
                    if let Some(existing) = candidates.iter_mut().find(|c| c.text == sentence) {
                        // 整句与已有候选（如精确匹配 你好）同文：提升其权重置顶，
                        // 同时抹去 is_partial（step1 标了 true，但整句是完整解读并非子短语），
                        // 否则残码场景下 is_partial=true 会在排序时被 is_partial=false 的前缀补全
                        // （如「你好吗」）压下去——后者经 trailing_partial 优化也是 false。
                        existing.weight = existing.weight.max(weight);
                        existing.is_partial = false;
                    } else {
                        candidates.insert(
                            0,
                            Candidate {
                                text: sentence,
                                // 码为完成音节前缀（不含残码），使 consumed_length=completed_len，
                                // 整句上屏后残码留缓冲续输（你好m → 选你好留 m）。
                                code: completed.to_string(),
                                weight,
                                natural_order: 0,
                                source: CandidateSource::Pinyin,
                                ..Default::default()
                            },
                        );
                    }
                }
            }
        }

        // 3. DAG 前缀子短语查找（仅 start==0）：给出输入的各级前缀词，供从左到右分段上屏
        //    （「nihao」→「你」「你好」）。不取中段/后段子串（如「hao」→「好」）——非前缀词
        //    应在前缀提交后、剩余拼音重转时才出现，否则会污染整串候选并破坏分段语义。
        if syllables.len() >= 2 {
            for end in 1..syllables.len().min(6) {
                let code: String = syllables[..end].join("");
                if code == query {
                    continue;
                }
                // 子词组 code 是输入的真前缀（比输入*短*，如 nihao 的「你」(ni)），是合法的
                // 分段上屏候选，与精确同层按权重排（不可降权——否则罕见全长词「拟好」会压过
                // 常用子词组「你」）。只有 code 比输入*长*的补全词(step4)才算前缀补全降权。
                for (text, weight, order, is_fuzzy) in
                    self.lookup_with_fuzzy(&code, &syllables[..end])
                {
                    push_unique(
                        &mut candidates,
                        text,
                        code.clone(),
                        weight,
                        order,
                        is_fuzzy,
                        false,
                    );
                }
            }
        }

        // 4. 前缀查找（补全词，code 比输入长，如 si→思考）→ 前缀层级，降到精确之后。
        //
        // 尾部残码存在时（如 meiy 的 "y" 未成音节，completed="mei" ⊂ query="meiy"）：**不标
        // is_prefix**。若标 is_prefix=true，协调器 build_candidates 重排 is_prefix asc 会把
        // 前缀补全候选压到全部精确匹配（is_prefix=false）之后，数百条单字「没/每/美/…」
        // 会淹掉「没有」（用户翻 15+ 页才见，与 "不处理" 无异）。不标后 is_prefix=false，
        // 同时 code("meiyou") 长于 query("meiy") → is_partial=false（由 push_unique 自动计算），
        // is_partial asc 让他们浮到 is_partial=true 的精确子串（没/每）之前。
        // 无残码时（meiyou）保持 is_prefix=true，前缀补全沉在精确匹配之后（正常行为）。
        let trailing_partial = completed != query;
        for (code, text, weight, order) in dict.search_prefix(query, 30) {
            push_unique(
                &mut candidates,
                text,
                code,
                weight,
                order,
                false,
                !trailing_partial, // 有残码时不标 is_prefix，让候选上浮
            );
        }

        // 5. 简拼匹配（声母缩写，如 nh→你好）：查 wdat 预存的独立 AbbrevSection。
        //    仅当输入像简拼时才查（is_abbreviation：每字母均为某音节首字母、且非完整音节序列），
        //    避免对全拼输入做无谓查找。natural_order=999999 让简拼候选默认排在全拼之后。
        if AbbrevMatcher::is_abbreviation(query, trie) {
            for (text, weight, _order) in dict.search_abbrev(query, 10) {
                push_unique(
                    &mut candidates,
                    text,
                    query.to_string(),
                    weight,
                    999999,
                    false,
                    true,
                );
            }
        }

        // 6. 用户/临时造词层（L：让拼音造的词显现）。查询与主词典相同的码——整串精确 +
        //    各前缀子码（你好 coded「nihao」当输入「nihaoma」时部分消费）+ 前缀补全——
        //    并入候选（dedup by text，已在系统词典出现的不重复加）。weight 由 store 记录给出，
        //    随后统一按 weight 排序；词频上浮交协调器 apply_freq_rerank（衰减软置前，frequency.md §4）。
        if let Some(store_dm) = &self.store_layers {
            let limit = max_candidates.max(50);
            let mut store_cands: Vec<Candidate> = store_dm.search(query, limit);
            if syllables.len() >= 2 {
                for end in 1..syllables.len().min(6) {
                    let code: String = syllables[..end].join("");
                    if code == query {
                        continue;
                    }
                    store_cands.extend(store_dm.search(&code, limit));
                }
            }
            store_cands.extend(store_dm.search_prefix(query, limit));
            for mut c in store_cands {
                if c.text.is_empty() || candidates.iter().any(|x| x.text == c.text) {
                    continue;
                }
                c.source = CandidateSource::Pinyin;
                // 与 push_unique 一致：store 层的前缀子码命中也是子短语，降到完整匹配之后。
                c.is_partial =
                    !c.is_prefix && c.code.len() < query.len() && query.starts_with(&c.code);
                candidates.push(c);
            }

            // 简拼匹配（用户/临时造词层）：用户词写入时只存全拼码，不像系统词库那样离线
            // 预建 AbbrevSection——规模小，现算即可（枚举该 schema 下全部用户/临时词，
            // 现场切分各词全拼码取声母比对，见 abbrev_of_code）。natural_order 对齐
            // step5 系统简拼候选，同样排在全拼之后。
            if AbbrevMatcher::is_abbreviation(query, trie) {
                for mut c in store_dm.search_prefix("", 0) {
                    if c.text.is_empty() || candidates.iter().any(|x| x.text == c.text) {
                        continue;
                    }
                    if self.abbrev_of_code(&c.code).as_deref() != Some(query) {
                        continue;
                    }
                    c.source = CandidateSource::Pinyin;
                    c.code = query.to_string();
                    c.is_prefix = false;
                    c.is_partial = false;
                    c.is_fuzzy = true;
                    c.natural_order = 999999;
                    candidates.push(c);
                }
            }
        }

        // 手动分隔符边界过滤：用户以 `'` 强制音节边界后，凡「码恰好落在某音节边界、但字数
        // 与所跨音节数不符」的候选被剔除——如 xi'an 强制 [xi,an]，则单字「先」(code=xian,
        // 跨 2 音节却仅 1 字) 不该出现；「西」(code=xi,1 字 1 音节)、整句「西安」(2 字 2 音节) 保留。
        // 码不落在任何边界的候选（如前缀补全）不受影响。
        if has_sep {
            let syls = &syllables;
            candidates.retain(|c| match syllable_span(syls, &c.code) {
                Some(k) if k >= 1 => c.text.chars().count() == k,
                _ => true,
            });
        }

        // 裸声母（无完整音节，如 "m"/"zh"）单字优先：候选全为前缀补全词（is_prefix=true），
        // 纯按词频排会让高频多字词（没有/目前）压过单字（吗/么）——不合直觉。给单字提权使其
        // 排在多字词之前（对齐主流输入法首字优先，见 BARE_INITIAL_SINGLE_CHAR_BOOST）。
        // 仅此情形——完整音节输入的单字已靠精确层级(is_prefix=false)就位，无需提权。
        if syllables.is_empty() {
            for c in candidates.iter_mut() {
                if c.text.chars().count() == 1 {
                    c.weight = c.weight.saturating_add(BARE_INITIAL_SINGLE_CHAR_BOOST);
                }
            }
        }

        // 引擎内部排序（层级对齐 Go：完整匹配 >> 子短语 >> 前缀补全 >> 模糊）：
        // ① 非模糊优先于模糊（is_fuzzy=false 在前）；② 完整匹配/子短语(is_prefix=false)优先于
        // 前缀补全(is_prefix=true)；③ 完整匹配(is_partial=false)优先于子短语(is_partial=true)
        // ——对齐 Go coverage 分层，避免高频单字「报/宝」插进完整词「保安」「报案」之间；
        // ④ 同层内按权重降序、自然序升序。
        // 使输入 si 时：精确单字「四/死」> 前缀补全「思考/似乎」> 模糊命中「是」；
        // 输入 baoan 时：完整词「保安」「报案」> 子短语单字「报/宝」。
        candidates.sort_by(|a, b| {
            a.is_fuzzy
                .cmp(&b.is_fuzzy)
                .then(a.is_prefix.cmp(&b.is_prefix))
                .then(a.is_partial.cmp(&b.is_partial))
                .then(b.weight.cmp(&a.weight))
                .then(a.natural_order.cmp(&b.natural_order))
        });
        candidates.truncate(max_candidates);

        // 分段上屏所需：标注每个候选实际消费的输入字节数。
        // code 为 input（全拼）的前缀（如 "ni" ⊂ "nihao"）→ 只消费该前缀，选中后保留剩余拼音续转；
        // 否则（整句/前缀补全/非前缀子串）消费整串。0 表示未知（由调用方按整串处理）。
        // 双拼激活时：全拼字节数需通过 map_consumed_length 回算为双拼原始键数，
        // 否则变长音节（2键→3字节，如 zh/ch/sh）会错误消费/越界双拼键缓冲区。
        for c in candidates.iter_mut() {
            // 以剥除分隔符后的 query 为基准计算消费长度（无分隔符时 query==input）。
            let fp_consumed = if !c.code.is_empty() && query.starts_with(&c.code) {
                c.code.len()
            } else {
                query.len()
            };
            c.consumed_length = match &sp_result {
                Some(r) => r.map_consumed_length(fp_consumed),
                // 全拼含手动分隔符：query 是剥除 `'` 的串，需回映射到含 `'` 的原始输入空间，
                // 否则协调器按含 `'` 缓冲切片时会残留尾字符（xi'an 选「西安」残 "n"）。
                None if has_sep => map_consumed_over_separators(input, fp_consumed),
                None => fp_consumed,
            };
        }

        let (mut preedit_display, completed_syllables, partial_syllable) =
            self.compute_composition(input);

        // Fix A：双拼激活时，preedit 改为显示用户实际输入的原始按键（按双拼音节边界以 `'` 分隔），
        // 而非转换后的全拼。仅覆盖 preedit_display；候选/completed_syllables/partial_syllable/
        // consumed_length 仍保持全拼语义不变。
        if let Some(r) = &sp_result {
            preedit_display = build_raw_preedit(raw_input, r);
        }

        let has_partial = !partial_syllable.is_empty();
        let is_empty = candidates.is_empty();

        Ok(ConvertResult {
            candidates,
            // 拼音恒为拆分形态（供混输高亮跟随：高亮拼音候选时取此串）。
            preedit_pinyin: preedit_display.clone(),
            preedit_display,
            completed_syllables,
            partial_syllable,
            has_partial,
            should_commit: false,
            commit_text: String::new(),
            is_empty,
            should_clear: false,
        })
    }

    fn reset(&self) {}

    fn engine_type(&self) -> EngineType {
        EngineType::Pinyin
    }

    /// 为词语生成全拼编码（多音字按词典权重消歧）。
    /// 单字读音索引按词典懒构建并缓存。含无读音字符时返回 `None`。
    fn generate_word_pinyin(&self, word: &str) -> Option<String> {
        let idx = self
            .char_pinyin_idx
            .get_or_init(|| CharPinyinIndex::build(&self.dict));
        generate::generate_word_pinyin(&self.dict, idx, word)
    }

    fn is_possible_pinyin_sequence(&self, prefix: &str) -> bool {
        // 条件1：整个前缀本身是某合法音节的前缀（如 zhon→zhong），长度 >=2 过滤单字母简拼。
        if prefix.len() >= 2 && self.trie.is_prefix(prefix) {
            return true;
        }
        // 条件2：从起始连续完整音节 + 合法尾部前缀。首音节须非单字母。
        let (completed, end_pos) = self.contiguous_completed_from_start(prefix);
        if completed.is_empty() || completed[0].len() < 2 {
            return false;
        }
        if end_pos >= prefix.len() {
            return true;
        }
        self.trie.is_prefix(&prefix[end_pos..])
    }

    fn is_whole_syllable_pinyin(&self, prefix: &str) -> bool {
        // 整体即单个完整音节（wang/shen 等填满码长的场景）。
        if self.trie.is_syllable(prefix) {
            return true;
        }
        // 多音节：连续完整音节恰好覆盖整个前缀，且首音节非单字母简拼。
        let (completed, end_pos) = self.contiguous_completed_from_start(prefix);
        if completed.is_empty() || completed[0].len() < 2 {
            return false;
        }
        end_pos == prefix.len()
    }

    fn has_non_initial_single_letter_syllable(&self, prefix: &str) -> bool {
        let (completed, _) = self.contiguous_completed_from_start(prefix);
        completed.iter().skip(1).any(|s| s.len() == 1)
    }

    fn completed_syllable_count(&self, prefix: &str) -> usize {
        self.contiguous_completed_from_start(prefix).0.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pinyin::shuangpin::{Layout, ShuangpinConverter};
    use wind_dict::codetable::CodetableDict;
    use wind_store::Store;

    fn empty_engine() -> PinyinEngine {
        PinyinEngine::new(
            Config::default(),
            CachedDict::Memory(CodetableDict::empty()),
        )
    }

    // ── 音节分析（顶码歧义裁决用；对齐 Go isPossiblePinyinSequence / isWholeSyllablePinyin /
    //    hasNonInitialSingleLetterSyllable）。trie 为封闭标准音节集，不依赖词典。──

    #[test]
    fn possible_pinyin_sequence_matches_go_cases() {
        let e = empty_engine();
        // 完整音节 / 音节前缀 / 完整音节+尾部前缀 → true
        assert!(e.is_possible_pinyin_sequence("wang")); // 单完整音节
        assert!(e.is_possible_pinyin_sequence("zhon")); // zhong 的前缀
        assert!(e.is_possible_pinyin_sequence("yans")); // yan + 尾部前缀 s
        assert!(e.is_possible_pinyin_sequence("naap")); // na + a + 前缀 p
        // 非拼音 / 首音节单字母 → false
        assert!(!e.is_possible_pinyin_sequence("rcqn")); // 无完整音节也非前缀
        assert!(!e.is_possible_pinyin_sequence("gggg")); // g 非合法音节/前缀
        assert!(!e.is_possible_pinyin_sequence("abcd")); // 首音节 a 为单字母
    }

    #[test]
    fn whole_syllable_pinyin_matches_go_cases() {
        let e = empty_engine();
        assert!(e.is_whole_syllable_pinyin("wang")); // 单完整音节
        assert!(e.is_whole_syllable_pinyin("aipu")); // ai+pu 恰好覆盖
        assert!(!e.is_whole_syllable_pinyin("zhon")); // 残缺前缀（非完整音节）
        assert!(!e.is_whole_syllable_pinyin("yans")); // yan + 残缺 s
        assert!(!e.is_whole_syllable_pinyin("abcd")); // 首音节单字母简拼
    }

    #[test]
    fn non_initial_single_letter_syllable_matches_go_cases() {
        let e = empty_engine();
        assert!(e.has_non_initial_single_letter_syllable("naap")); // na + a（第二音节单字母）
        assert!(!e.has_non_initial_single_letter_syllable("yans")); // yan + 残缺 s（残缺不计）
        assert!(!e.has_non_initial_single_letter_syllable("aipu")); // ai + pu 皆双字母
        assert!(!e.has_non_initial_single_letter_syllable("abcd")); // 首位单字母不算「非首位」
    }

    /// 构造含指定 (code, text) 词条的最小拼音引擎（每条 weight=100，order 递增）。
    fn engine_with_words(words: &[(&str, &str)]) -> PinyinEngine {
        let mut raw = CodetableDict::empty();
        for (i, (code, text)) in words.iter().enumerate() {
            raw.merge_single(code.to_string(), text.to_string(), 100, i as i32);
        }
        PinyinEngine::new(Config::default(), CachedDict::Memory(raw))
    }

    /// Task 8 Step 2：手动分隔符强制音节硬边界。
    /// 词典含 "xian"→"先" 与 "xi"/"an" 单字；带分隔符 xi'an 强制切分 [xi,an]，
    /// 跨界单音节词「先」(code=xian 却仅 1 字) 不得出现；preedit 保留手动 `'`。
    #[test]
    fn separator_forces_syllable_boundary() {
        let e = engine_with_words(&[("xian", "先"), ("xi", "西"), ("an", "安")]);
        let r = e.convert("xi'an", 50).unwrap();
        assert!(
            !r.candidates.iter().any(|c| c.text == "先"),
            "分隔符应阻止跨界音节 xian 匹配，实际: {:?}",
            r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
        assert!(
            r.preedit_display.contains('\''),
            "preedit 应保留手动分隔符，实际: {:?}",
            r.preedit_display
        );
        assert!(
            !r.candidates.is_empty(),
            "分隔符切分后仍应有候选（如「西」）"
        );
    }

    /// Task 8 Step 2：末尾分隔符必须立即显示，且不清空候选。
    #[test]
    fn trailing_separator_kept_in_preedit() {
        let e = engine_with_words(&[("ni", "你")]);
        let r = e.convert("ni'", 50).unwrap();
        assert!(
            r.preedit_display.ends_with('\''),
            "末尾分隔符必须立即显示，实际: {:?}",
            r.preedit_display
        );
        assert!(!r.candidates.is_empty(), "末尾分隔符不应清空候选");
    }

    /// Task 8 自审：五个边界（空段/开头 '/连续 ''/纯 '/末尾 '）均不 panic，
    /// 且手动分隔符在 preedit 中原样保留。
    #[test]
    fn separator_edge_cases_no_panic() {
        let e = engine_with_words(&[("ni", "你"), ("hao", "好"), ("xi", "西"), ("an", "安")]);

        // 开头 '：xi 段仍应产候选，preedit 以 ' 起头
        let r = e.convert("'xi", 20).unwrap();
        assert!(r.preedit_display.starts_with('\''), "开头分隔符应保留");
        assert!(r.candidates.iter().any(|c| c.text == "西"));

        // 连续 ''：等价单边界，preedit 保留双分隔符
        let r = e.convert("ni''hao", 20).unwrap();
        assert!(r.preedit_display.contains("''"), "连续分隔符应原样保留");
        assert!(r.candidates.iter().any(|c| c.text == "你"));

        // 纯 ' / 连续纯 '：无拼音可查，无候选、仅回显分隔符，不 panic
        for pure in ["'", "''", "'''"] {
            let r = e.convert(pure, 20).unwrap();
            assert!(r.candidates.is_empty(), "纯分隔符输入 {pure:?} 不应有候选");
            assert_eq!(r.preedit_display, pure, "纯分隔符应原样回显");
        }
    }

    fn tmp_store(name: &str) -> Arc<Store> {
        let p = std::env::temp_dir().join(format!("wind_pinyin_{name}.redb"));
        let _ = std::fs::remove_file(&p);
        Arc::new(Store::open(&p).unwrap())
    }

    /// L 造词显现：挂上用户/临时层后，拼音造的词应进入候选（即便主词典为空）。
    #[test]
    fn store_layer_words_appear_in_candidates() {
        let store = tmp_store("layer_show");
        store.add_user_word("pinyin", "nihao", "你好", 500).unwrap();
        store
            .learn_temp_word("pinyin", "lanshou", "蓝瘦", 800)
            .unwrap();
        let dm = DictManager::new();
        dm.register_layer(Box::new(wind_dict::StoreUserLayer::new(
            store.clone(),
            "pinyin",
        )));
        dm.register_layer(Box::new(wind_dict::StoreTempLayer::new(
            store.clone(),
            "pinyin",
        )));
        let engine = empty_engine().with_store_layers(Arc::new(dm));

        // 整串精确命中用户词
        let r = engine.convert("nihao", 20).unwrap();
        assert!(
            r.candidates.iter().any(|c| c.text == "你好"),
            "用户造词「你好」应出现在拼音候选"
        );
        // 临时词同样显现
        let r2 = engine.convert("lanshou", 20).unwrap();
        let shou = r2.candidates.iter().find(|c| c.text == "蓝瘦");
        assert!(shou.is_some(), "临时造词「蓝瘦」应出现在拼音候选");
        assert_eq!(
            shou.unwrap().source,
            CandidateSource::Pinyin,
            "来源应标为拼音"
        );
    }

    /// 无 store 层时行为不变（不 panic、空词典无候选）。
    #[test]
    fn no_store_layer_is_inert() {
        let engine = empty_engine();
        let r = engine.convert("nihao", 20).unwrap();
        assert!(r.candidates.is_empty(), "空词典无 store 层应无候选");
    }

    /// 部分消费：用户词码是输入的前缀（nihao ⊂ nihaoma）→ consumed_length 标为前缀长度，
    /// 选中后保留剩余拼音续转（分段上屏）。
    #[test]
    fn store_word_prefix_marks_partial_consumption() {
        let store = tmp_store("layer_partial");
        store.add_user_word("pinyin", "nihao", "你好", 500).unwrap();
        let dm = DictManager::new();
        dm.register_layer(Box::new(wind_dict::StoreUserLayer::new(
            store.clone(),
            "pinyin",
        )));
        let engine = empty_engine().with_store_layers(Arc::new(dm));
        let r = engine.convert("nihaoma", 20).unwrap();
        let c = r.candidates.iter().find(|c| c.text == "你好");
        assert!(c.is_some(), "前缀用户词应作为分段候选出现");
        assert_eq!(
            c.unwrap().consumed_length,
            "nihao".len(),
            "应只消费前缀 nihao"
        );
    }

    /// 用户造词的简拼应可命中（现算，非离线索引）：用户新增「菜鸟驿站」（全拼
    /// cainiaoyizhan），键入简拼 cnyz 应能查到；临时词同理。
    #[test]
    fn store_layer_words_match_abbreviation() {
        let store = tmp_store("layer_abbrev");
        store
            .add_user_word("pinyin", "cainiaoyizhan", "菜鸟驿站", 500)
            .unwrap();
        store
            .learn_temp_word("pinyin", "lanshoubing", "蓝瘦蘑菇", 800)
            .unwrap();
        let dm = DictManager::new();
        dm.register_layer(Box::new(wind_dict::StoreUserLayer::new(
            store.clone(),
            "pinyin",
        )));
        dm.register_layer(Box::new(wind_dict::StoreTempLayer::new(
            store.clone(),
            "pinyin",
        )));
        let engine = empty_engine().with_store_layers(Arc::new(dm));

        let r = engine.convert("cnyz", 20).unwrap();
        assert!(
            r.candidates.iter().any(|c| c.text == "菜鸟驿站"),
            "简拼 cnyz 应命中用户造词「菜鸟驿站」"
        );

        let r2 = engine.convert("lsb", 20).unwrap();
        assert!(
            r2.candidates.iter().any(|c| c.text == "蓝瘦蘑菇"),
            "简拼 lsb 应命中临时造词「蓝瘦蘑菇」"
        );

        // 全拼整串输入仍应正常命中（无回归）
        let r3 = engine.convert("cainiaoyizhan", 20).unwrap();
        assert!(r3.candidates.iter().any(|c| c.text == "菜鸟驿站"));
    }

    /// C1：query→原始输入空间的 consumed 回映射。无 `'` 恒等；边界紧跟 `'` 归入已消费侧；
    /// 连续 `''` 一并吸收；越过分隔符时正确计数；nih'ao 段内残码边界不 panic。
    #[test]
    fn map_consumed_over_separators_cases() {
        use super::map_consumed_over_separators as m;
        // 无分隔符：恒等（零回归红线）
        assert_eq!(m("nihao", 0), 0);
        assert_eq!(m("nihao", 2), 2);
        assert_eq!(m("nihao", 5), 5);
        // xi'an：西安 code="xian" 消费 query 4 → 含 ' 的原始空间 5（全消费）
        assert_eq!(m("xi'an", 4), 5);
        // xi'an：西 code="xi" 消费 query 2 → 边界紧跟 ' 归入已消费侧 → 3（残留 "an" 而非 "'an"）
        assert_eq!(m("xi'an", 2), 3);
        // 连续 '' 一并吸收：ni''hao 消费 "ni"(2) → 吃掉两个 ' → 4（残留 "hao"）
        assert_eq!(m("ni''hao", 2), 4);
        // 末尾 '：ni' 全消费 query 2 → 吸收尾部 ' → 3
        assert_eq!(m("ni'", 2), 3);
        // nih'ao：段内残码 h 不成音节；消费 "ni"(2) 时 h 非分隔符不吸收 → 2，残留 "h'ao"（不 panic）
        assert_eq!(m("nih'ao", 2), 2);
        // nih'ao 全 query 消费 5 → 覆盖到末尾 6
        assert_eq!(m("nih'ao", 5), 6);
    }

    /// Task 1.4 TDD：with_fuzzy builder 注入的配置应被引擎持有（探针验证）。
    #[test]
    fn engine_applies_fuzzy_config() {
        let dict = CachedDict::Memory(CodetableDict::empty());
        let mut fz = FuzzyConfig::default();
        fz.zh_z = true;
        let eng = PinyinEngine::new(Config::default(), dict).with_fuzzy(fz);
        assert!(eng.fuzzy_zh_z(), "with_fuzzy 注入的 zh_z=true 应被引擎持有");
    }

    /// Task 4.1 TDD Step 2：多音节双拼——consumed_length 必须回算为双拼键数，
    /// compute_composition 不能对已是全拼的串再做一次双拼转换。
    /// 输入小鹤双拼 "nihc"（ni+hc → 全拼 "nihao"），词典含「你好」。
    #[test]
    fn pinyin_engine_shuangpin_multisyllable_consumed_length() {
        // 构造含 "nihao"->"你好" 的最小词典
        let mut raw = CodetableDict::empty();
        raw.merge_single("nihao".to_string(), "你好".to_string(), 200, 0);
        raw.merge_single("ni".to_string(), "你".to_string(), 100, 1);
        let dict = CachedDict::Memory(raw);

        let schema_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../data/schemas/shuangpin");
        let layout = Layout::from_toml(&schema_dir.join("xiaohe.toml")).expect("加载小鹤布局失败");
        let conv = ShuangpinConverter::new(layout);

        let eng = PinyinEngine::new(Config::default(), dict).with_shuangpin(conv);
        // 小鹤双拼 "nihc" → 全拼 "nihao"
        let r = eng.convert("nihc", 10).unwrap();

        // 1. 候选含「你好」
        assert!(
            r.candidates.iter().any(|c| c.text == "你好"),
            "双拼输入 \"nihc\" 应包含候选「你好」，实际候选: {:?}",
            r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );

        // 2. 「你好」的 consumed_length 必须是双拼键数 4，而非全拼字节数 5
        let nihao = r.candidates.iter().find(|c| c.text == "你好").unwrap();
        assert_eq!(
            nihao.consumed_length, 4,
            "「你好」consumed_length 应为双拼键数 4（\"nihc\" 的长度），实际为 {}",
            nihao.consumed_length
        );
    }

    /// Fix A TDD：双拼 preedit 应显示用户实际输入的原始按键（按音节边界以空格分隔，
    /// 与全拼自动分词一致），而非转换后的全拼。输入小鹤 "nihc"（→全拼 nihao）应显示
    /// "ni hc"，候选仍含「你好」。
    #[test]
    fn shuangpin_preedit_shows_raw_keys() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("nihao".to_string(), "你好".to_string(), 200, 0);
        raw.merge_single("ni".to_string(), "你".to_string(), 100, 1);
        let dict = CachedDict::Memory(raw);

        let schema_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../data/schemas/shuangpin");
        let layout = Layout::from_toml(&schema_dir.join("xiaohe.toml")).expect("加载小鹤布局失败");
        let conv = ShuangpinConverter::new(layout);
        let eng = PinyinEngine::new(Config::default(), dict).with_shuangpin(conv);

        // 完整双音节：preedit 为原始键按音节 ' 分隔 "ni'hc"（而非全拼 "ni'hao"）
        let r = eng.convert("nihc", 10).unwrap();
        assert_eq!(
            r.preedit_display, "ni'hc",
            "双拼 preedit 应显示原始按键并按音节 ' 分隔，实际: {:?}",
            r.preedit_display
        );
        // 候选仍走全拼语义，含「你好」
        assert!(
            r.candidates.iter().any(|c| c.text == "你好"),
            "候选仍应含「你好」，实际: {:?}",
            r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );

        // 单音节：ni → "ni"
        let r2 = eng.convert("ni", 10).unwrap();
        assert_eq!(r2.preedit_display, "ni", "单音节 preedit 应为 \"ni\"");

        // 含 partial：nih（ni 完成 + h 未配对）→ "ni'h"
        let r3 = eng.convert("nih", 10).unwrap();
        assert_eq!(
            r3.preedit_display, "ni'h",
            "含 partial 的双拼 preedit 应为 \"ni'h\"，实际: {:?}",
            r3.preedit_display
        );
    }

    /// Fix B TDD：fuzzy 应接入精确/单音节查询。词典含 "shi"→"是"，
    /// fuzzy sh_s=true 时，输入单音节全拼 "si" 应能命中「是」（sh↔s 模糊）。
    #[test]
    fn fuzzy_lookup_single_syllable() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("shi".to_string(), "是".to_string(), 100, 0);
        let dict = CachedDict::Memory(raw);
        let mut fz = FuzzyConfig::default();
        fz.sh_s = true;
        let eng = PinyinEngine::new(Config::default(), dict).with_fuzzy(fz);

        let r = eng.convert("si", 10).unwrap();
        assert!(
            r.candidates.iter().any(|c| c.text == "是"),
            "fuzzy sh_s 开启时，单音节 \"si\" 应命中「是」，实际: {:?}",
            r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );

        // 反向：词典 "si"→"四"，输入 "shi" 应命中「四」
        let mut raw2 = CodetableDict::empty();
        raw2.merge_single("si".to_string(), "四".to_string(), 100, 0);
        let mut fz2 = FuzzyConfig::default();
        fz2.sh_s = true;
        let eng2 = PinyinEngine::new(Config::default(), CachedDict::Memory(raw2)).with_fuzzy(fz2);
        let r2 = eng2.convert("shi", 10).unwrap();
        assert!(
            r2.candidates.iter().any(|c| c.text == "四"),
            "fuzzy sh_s 开启时，\"shi\" 应命中「四」，实际: {:?}",
            r2.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    /// 优先级 TDD：原对应拼音（精确匹配）应优先于模糊命中——即便模糊词词频更高。
    /// 词典 "si"→"四"(weight 100) 与 "shi"→"是"(weight 9000，更高频)；fuzzy sh_s=true。
    /// 输入 "si"：「四」是精确命中、「是」是模糊命中，「四」必须排在「是」之前。
    #[test]
    fn fuzzy_exact_ranks_above_fuzzy() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("si".to_string(), "四".to_string(), 100, 0);
        raw.merge_single("shi".to_string(), "是".to_string(), 9000, 0);
        let dict = CachedDict::Memory(raw);
        let mut fz = FuzzyConfig::default();
        fz.sh_s = true;
        let eng = PinyinEngine::new(Config::default(), dict).with_fuzzy(fz);

        let r = eng.convert("si", 10).unwrap();
        let texts: Vec<&String> = r.candidates.iter().map(|c| &c.text).collect();
        let pos_si = texts.iter().position(|t| *t == "四");
        let pos_shi = texts.iter().position(|t| *t == "是");
        assert!(pos_si.is_some(), "精确候选「四」应存在，实际: {texts:?}");
        assert!(pos_shi.is_some(), "模糊候选「是」应存在，实际: {texts:?}");
        assert!(
            pos_si < pos_shi,
            "精确命中「四」应排在模糊命中「是」之前（即便「是」词频更高），实际: {texts:?}"
        );
        // 「四」是精确（非模糊），「是」是模糊命中
        assert!(!r.candidates[pos_si.unwrap()].is_fuzzy, "「四」应为非模糊");
        assert!(
            r.candidates[pos_shi.unwrap()].is_fuzzy,
            "「是」应为模糊命中"
        );
    }

    /// 层级 TDD：精确单字应优先于高频前缀补全词。词典 "si"→"四"(weight 100,单字精确) 与
    /// "sikao"→"思考"(weight 9000,补全词，code 比输入长)。输入 "si" 时「四」(精确,code==输入)
    /// 必须排在「思考」(前缀补全)之前——即便「思考」词频高得多。对齐 Go Exact>>Partial。
    #[test]
    fn exact_ranks_above_prefix_completion() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("si".to_string(), "四".to_string(), 100, 0);
        raw.merge_single("sikao".to_string(), "思考".to_string(), 9000, 0);
        let dict = CachedDict::Memory(raw);
        let eng = PinyinEngine::new(Config::default(), dict);

        let r = eng.convert("si", 10).unwrap();
        let texts: Vec<&String> = r.candidates.iter().map(|c| &c.text).collect();
        let pos_si = texts.iter().position(|t| *t == "四");
        let pos_kao = texts.iter().position(|t| *t == "思考");
        assert!(pos_si.is_some(), "精确「四」应存在，实际: {texts:?}");
        assert!(pos_kao.is_some(), "补全「思考」应存在，实际: {texts:?}");
        assert!(
            pos_si < pos_kao,
            "精确单字「四」应优先于高频前缀补全「思考」，实际: {texts:?}"
        );
        assert!(
            !r.candidates[pos_si.unwrap()].is_prefix,
            "「四」应为精确(非前缀)"
        );
        assert!(
            r.candidates[pos_kao.unwrap()].is_prefix,
            "「思考」应为前缀补全"
        );
    }

    /// 完整匹配优先于子短语（对齐 Go coverage 分层）：输入完整音节 "nihao" 时，
    /// 全长精确词「拟好」(code==nihao) 即便权重远低于子短语「你」(code=ni)，也应排在「你」之前。
    /// 「你」只覆盖部分输入(ni)，是分段上屏候选(is_partial)，整体降到完整词之后。
    ///
    /// 注：此前 `subphrase_not_demoted_below_rare_exact` 断言相反（子词组不降权 → 你 > 拟好），
    /// 那是刻意偏离 Go 的旧设计，正是 baoan→报案 被高频单字埋没的根因。现改为对齐 Go：
    /// `score = exp(词频) + initialQuality + coverage`，完整词(cov=1,iq=4) 恒先于子短语单字(cov=.5,iq=2.5)。
    #[test]
    fn full_word_ranks_above_subphrase_singlechar() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("nihao".to_string(), "你好".to_string(), 200, 0);
        raw.merge_single("nihao".to_string(), "拟好".to_string(), 10, 1); // 罕见全长精确词
        raw.merge_single("ni".to_string(), "你".to_string(), 5000, 0); // 常用子短语
        let dict = CachedDict::Memory(raw);
        let eng = PinyinEngine::new(Config::default(), dict);

        let r = eng.convert("nihao", 10).unwrap();
        let texts: Vec<&String> = r.candidates.iter().map(|c| &c.text).collect();
        let pos_ni = texts.iter().position(|t| *t == "你");
        let pos_nihao_rare = texts.iter().position(|t| *t == "拟好");
        assert!(
            pos_ni.is_some() && pos_nihao_rare.is_some(),
            "候选缺失（子短语「你」仍应存在，供分段上屏）: {texts:?}"
        );
        assert!(
            pos_nihao_rare < pos_ni,
            "完整词「拟好」应优先于子短语单字「你」(对齐 Go coverage 分层)，实际: {texts:?}"
        );
        // 「你」是子短语(is_partial)，不是前缀补全(is_prefix)——分段上屏机制不受影响
        let ni = &r.candidates[pos_ni.unwrap()];
        assert!(!ni.is_prefix, "子短语「你」不应是前缀补全");
        assert!(ni.is_partial, "子短语「你」应标记 is_partial");
    }

    /// baoan 回归（用户报告场景）：输入 "baoan" 时，完整 bao'an 词「保安」「报案」必须
    /// 聚集在前，不被高频子短语单字「报」(bao) 插开。修复前「报」(高权重) 会塞进
    /// 「保安」「报案」之间，把「报案」挤到后面几页。
    #[test]
    fn baoan_full_words_group_above_subphrase() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("baoan".to_string(), "保安".to_string(), 3513, 0);
        raw.merge_single("baoan".to_string(), "报案".to_string(), 1374, 1);
        raw.merge_single("bao".to_string(), "报".to_string(), 9000, 0); // 高频单字，权重高于「报案」
        raw.merge_single("an".to_string(), "安".to_string(), 5000, 0);
        let dict = CachedDict::Memory(raw);
        let eng = PinyinEngine::new(Config::default(), dict);

        let r = eng.convert("baoan", 20).unwrap();
        let texts: Vec<&String> = r.candidates.iter().map(|c| &c.text).collect();
        let pos = |t: &str| texts.iter().position(|x| *x == t);
        let p_baoan = pos("保安").expect("「保安」应存在");
        let p_baoan2 = pos("报案").expect("「报案」应存在");
        let p_bao = pos("报").expect("子短语「报」应存在（供分段上屏）");
        assert!(
            p_baoan < p_bao && p_baoan2 < p_bao,
            "完整词「保安」({p_baoan})「报案」({p_baoan2}) 都应排在高频子短语单字「报」({p_bao}) 之前，实际: {texts:?}"
        );
    }

    /// Fix B TDD：fuzzy 应接入多音节整串查询（expand_code 笛卡尔积）。
    /// 词典只存 eng 形式 "shengri"→"生日"，用户输入 en 形式 "shenri"（DAG 切分 shen+ri），
    /// fuzzy en_eng=true 时应通过 expand_code 生成 "shengri" 反查命中「生日」。
    #[test]
    fn fuzzy_lookup_multi_syllable() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("shengri".to_string(), "生日".to_string(), 100, 0);
        let dict = CachedDict::Memory(raw);
        let mut fz = FuzzyConfig::default();
        fz.en_eng = true;
        let eng = PinyinEngine::new(Config::default(), dict).with_fuzzy(fz);

        let r = eng.convert("shenri", 10).unwrap();
        assert!(
            r.candidates.iter().any(|c| c.text == "生日"),
            "fuzzy en_eng 开启时，\"shenri\" 应模糊命中「生日」(shengri)，实际: {:?}",
            r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    /// Fix B TDD：fuzzy 全 false 时 lookup_with_fuzzy 退化为纯精确查找（不引入多余候选）。
    #[test]
    fn fuzzy_disabled_no_extra_candidates() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("shi".to_string(), "是".to_string(), 100, 0);
        let dict = CachedDict::Memory(raw);
        // 无 with_fuzzy → FuzzyConfig::default() 全 false
        let eng = PinyinEngine::new(Config::default(), dict);
        let r = eng.convert("si", 10).unwrap();
        assert!(
            !r.candidates.iter().any(|c| c.text == "是"),
            "fuzzy 关闭时 \"si\" 不应命中「是」，实际: {:?}",
            r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    /// Bug 复现：双拼下用户词（存储在 "pinyin" 共享 schema）应出现在候选中。
    /// 小鹤双拼 "dabologe" → 全拼 "daboluoge"；store 中有该用户词时应能命中。
    #[test]
    fn shuangpin_store_user_word_appears_in_candidates() {
        let store = tmp_store("sp_userdict");
        store
            .add_user_word("pinyin", "daboluoge", "大菠萝哥", 0)
            .unwrap();

        let dm = DictManager::new();
        dm.register_layer(Box::new(wind_dict::StoreUserLayer::new(
            store.clone(),
            "pinyin",
        )));

        let schema_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../data/schemas/shuangpin");
        let layout = Layout::from_toml(&schema_dir.join("xiaohe.toml")).expect("加载小鹤布局失败");
        let conv = ShuangpinConverter::new(layout);

        // 先确认转换正确：dabologe → daboluoge
        let sp_result = conv.convert("dabologe");
        assert_eq!(
            sp_result.full_pinyin(),
            "daboluoge",
            "小鹤双拼 dabologe 应转换为全拼 daboluoge，实际: {:?}",
            sp_result.full_pinyin()
        );

        let eng = empty_engine()
            .with_shuangpin(conv)
            .with_store_layers(Arc::new(dm));

        let r = eng.convert("dabologe", 20).unwrap();
        assert!(
            r.candidates.iter().any(|c| c.text == "大菠萝哥"),
            "双拼输入 \"dabologe\" 应命中用户词「大菠萝哥」，实际候选: {:?}",
            r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    // ── use_smart_compose 开关 ──────────────────────────────────────────────────

    /// 构造带单字词典的拼音引擎（供整句/Viterbi 相关测试使用）：
    /// 词典含 "ni"→"你"、"hao"→"好"，但无 "nihao"→"你好" 整词。
    /// 任何 "你好" 候选只能来自 Viterbi 整句路径。
    fn engine_for_sentence_tests(config: Config) -> PinyinEngine {
        let mut raw = CodetableDict::empty();
        raw.merge_single("ni".to_string(), "你".to_string(), 100, 0);
        raw.merge_single("hao".to_string(), "好".to_string(), 100, 1);
        PinyinEngine::new(config, CachedDict::Memory(raw))
    }

    /// 判断候选是否为 Viterbi 合成整句（权重达到 SENTENCE_WEIGHT_BASE 档）。
    fn is_viterbi_sentence(c: &Candidate) -> bool {
        c.weight >= super::SENTENCE_WEIGHT_BASE
    }

    /// TDD：use_smart_compose=false 时多音节输入不产生 Viterbi 合成整句候选。
    #[test]
    fn smart_compose_off_skips_viterbi_sentence() {
        let e = engine_for_sentence_tests(Config {
            use_smart_compose: false,
            ..Config::default()
        });
        let r = e.convert("nihao", 50).unwrap();
        assert!(
            !r.candidates.iter().any(|c| {
                c.text.chars().count() >= 2
                    && c.source == CandidateSource::Pinyin
                    && is_viterbi_sentence(c)
            }),
            "关闭智能组句后不应有 Viterbi 合成整句，实际候选: {:?}",
            r.candidates
                .iter()
                .map(|c| (&c.text, c.weight))
                .collect::<Vec<_>>()
        );
    }

    /// 回归：use_smart_compose=true（默认）时整句候选仍产生。
    #[test]
    fn smart_compose_on_produces_viterbi_sentence() {
        let e = engine_for_sentence_tests(Config::default()); // use_smart_compose 默认 true
        let r = e.convert("nihao", 50).unwrap();
        assert!(
            r.candidates.iter().any(|c| {
                c.text.chars().count() >= 2
                    && c.source == CandidateSource::Pinyin
                    && is_viterbi_sentence(c)
            }),
            "启用智能组句时应有 Viterbi 合成整句，实际候选: {:?}",
            r.candidates
                .iter()
                .map(|c| (&c.text, c.weight))
                .collect::<Vec<_>>()
        );
    }

    /// Task 4.1 TDD Step 1：双拼端到端——装配小鹤双拼 converter 后，
    /// 输入双拼键 "ni" 应返回含「你」的候选。
    #[test]
    fn pinyin_engine_shuangpin_input() {
        // 构造含 "ni"->"你" 的最小词典
        let mut raw = CodetableDict::empty();
        raw.merge_single("ni".to_string(), "你".to_string(), 100, 0);
        let dict = CachedDict::Memory(raw);

        // 小鹤双拼：ni → ni（声母 n + 韵母 i=i，即全拼 "ni"，保持不变）
        let schema_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../data/schemas/shuangpin");
        let layout = Layout::from_toml(&schema_dir.join("xiaohe.toml")).expect("加载小鹤布局失败");
        let conv = ShuangpinConverter::new(layout);

        let eng = PinyinEngine::new(Config::default(), dict).with_shuangpin(conv);
        let r = eng.convert("ni", 10).unwrap();
        assert!(
            r.candidates.iter().any(|c| c.text == "你"),
            "双拼输入 \"ni\" 经转换后应返回含「你」的候选，实际候选: {:?}",
            r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }
}
