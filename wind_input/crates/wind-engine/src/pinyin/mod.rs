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

/// 拼音引擎配置
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub show_code_hint: bool,
    pub use_smart_compose: bool,
}

/// 拼音引擎
pub struct PinyinEngine {
    /// 引擎配置（show_code_hint / filter_mode 等，后续阶段接入）
    #[allow(dead_code)]
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
    fn compute_composition(&self, full_pinyin: &str) -> (String, Vec<String>, String) {
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
            let is_partial = !is_prefix && code.len() < input.len() && input.starts_with(&code);
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
        let dag = Dag::build(input, trie);
        let syllables = dag.maximum_match();

        // 1. 精确查找（完整匹配，含模糊扩展，对齐 Go lookupWithFuzzy）。code==input → 精确层级。
        for (text, weight, order, is_fuzzy) in self.lookup_with_fuzzy(input, &syllables) {
            push_unique(
                &mut candidates,
                text,
                input.to_string(),
                weight,
                order,
                is_fuzzy,
                false,
            );
        }

        // 完成音节覆盖的连续前缀（从起点算）。尾部不成音节的残码（如「nihaom」的「m」）
        // 不参与整句解码——否则 lattice 到不了残码末端、Viterbi 失败、整句退化成单字（bug①）。
        let completed_len: usize = syllables.iter().map(|s| s.len()).sum();
        let completed: &str = &input[..completed_len];

        // 2. Viterbi 长句解码（>=2 音节，仅在完成音节前缀上跑）
        if syllables.len() >= 2 {
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
                        // 否则单字（如 你）会因词频更高反超整句词。
                        existing.weight = existing.weight.max(weight);
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
                if code == input {
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
        for (code, text, weight, order) in dict.search_prefix(input, 30) {
            push_unique(&mut candidates, text, code, weight, order, false, true);
        }

        // 5. 简拼匹配（声母缩写，如 nh→你好）：查 wdat 预存的独立 AbbrevSection。
        //    仅当输入像简拼时才查（is_abbreviation：每字母均为某音节首字母、且非完整音节序列），
        //    避免对全拼输入做无谓查找。natural_order=999999 让简拼候选默认排在全拼之后。
        if AbbrevMatcher::is_abbreviation(input, trie) {
            for (text, weight, _order) in dict.search_abbrev(input, 10) {
                push_unique(
                    &mut candidates,
                    text,
                    input.to_string(),
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
            let mut store_cands: Vec<Candidate> = store_dm.search(input, limit);
            if syllables.len() >= 2 {
                for end in 1..syllables.len().min(6) {
                    let code: String = syllables[..end].join("");
                    if code == input {
                        continue;
                    }
                    store_cands.extend(store_dm.search(&code, limit));
                }
            }
            store_cands.extend(store_dm.search_prefix(input, limit));
            for mut c in store_cands {
                if c.text.is_empty() || candidates.iter().any(|x| x.text == c.text) {
                    continue;
                }
                c.source = CandidateSource::Pinyin;
                // 与 push_unique 一致：store 层的前缀子码命中也是子短语，降到完整匹配之后。
                c.is_partial =
                    !c.is_prefix && c.code.len() < input.len() && input.starts_with(&c.code);
                candidates.push(c);
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
            let fp_consumed = if !c.code.is_empty() && input.starts_with(&c.code) {
                c.code.len()
            } else {
                input.len()
            };
            c.consumed_length = match &sp_result {
                Some(r) => r.map_consumed_length(fp_consumed),
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
            .learn_temp_word("pinyin", "lanshou", "蓝瘦", 800, 0)
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
