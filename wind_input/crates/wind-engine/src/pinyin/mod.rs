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
use lattice::LatticeBuilder;
use lm::UnigramLookup;
use scorer::AbbrevMatcher;
use std::sync::Arc;
use syllable::SyllableTrie;
use viterbi::{ViterbiDecoder, WordNode};
use wind_candidate::{Candidate, CandidateSource};
use wind_dict::cached::CachedDict;

/// 整句候选权重基准（高于拼音词频上限 ~19260817，确保整句置顶且不被截断）
const SENTENCE_WEIGHT_BASE: i32 = 30_000_000;

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
        }
    }

    /// 总条目数
    pub fn entry_count(&self) -> usize {
        self.dict.len()
    }

    /// 计算 preedit 显示与音节信息
    fn compute_composition(&self, input: &str) -> (String, Vec<String>, String) {
        let dag = Dag::build(input, &self.trie);
        let syllables = dag.maximum_match();
        let consumed: usize = syllables.iter().map(|s| s.len()).sum();
        let partial = if consumed < input.len() {
            input[consumed..].to_string()
        } else {
            String::new()
        };

        let mut preedit = syllables.join(" ");
        if !partial.is_empty() {
            if !preedit.is_empty() {
                preedit.push(' ');
            }
            preedit.push_str(&partial);
        }
        if preedit.is_empty() {
            preedit = input.to_string();
        }
        (preedit, syllables, partial)
    }
}

impl Engine for PinyinEngine {
    fn convert(&self, input: &str, max_candidates: usize) -> anyhow::Result<ConvertResult> {
        if input.is_empty() {
            return Ok(ConvertResult::default());
        }

        let dict = &self.dict;
        let trie = &self.trie;
        let mut candidates: Vec<Candidate> = Vec::new();

        let push_unique =
            |cands: &mut Vec<Candidate>, text: String, code: String, weight: i32, order: i32| {
                if text.is_empty() || cands.iter().any(|c| c.text == text) {
                    return;
                }
                cands.push(Candidate {
                    text,
                    code,
                    weight,
                    natural_order: order,
                    source: CandidateSource::Pinyin,
                    ..Default::default()
                });
            };

        // 1. 精确查找（完整匹配）
        for (text, weight, order) in dict.search(input) {
            push_unique(&mut candidates, text, input.to_string(), weight, order);
        }

        let dag = Dag::build(input, trie);
        let syllables = dag.maximum_match();

        // 2. Viterbi 长句解码（>=2 音节）
        if syllables.len() >= 2 {
            let lattice_nodes = self.lattice_builder.build(
                input,
                trie,
                dict,
                Some(&self.fuzzy_config),
                self.unigram.as_deref(),
            );
            let input_len = input.len();
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
                                code: input.to_string(),
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

        // 3. DAG 子短语查找
        if syllables.len() >= 2 {
            for start in 0..syllables.len() {
                for end in (start + 1)..=syllables.len().min(start + 6) {
                    let code: String = syllables[start..end].join("");
                    if code == input {
                        continue;
                    }
                    for (text, weight, order) in dict.search(&code) {
                        push_unique(&mut candidates, text, code.clone(), weight, order);
                    }
                }
            }
        }

        // 4. 前缀查找
        for (code, text, weight, order) in dict.search_prefix(input, 30) {
            push_unique(&mut candidates, text, code, weight, order);
        }

        // 5. 缩写/简拼匹配
        if AbbrevMatcher::is_abbreviation(input, trie) {
            for abbrev in AbbrevMatcher::find_candidates(input, trie, dict, 10) {
                push_unique(
                    &mut candidates,
                    abbrev.text,
                    abbrev.code,
                    abbrev.weight,
                    999999,
                );
            }
        }

        // 引擎内部排序（按权重降序，自然顺序升序）
        candidates.sort_by(|a, b| {
            b.weight
                .cmp(&a.weight)
                .then(a.natural_order.cmp(&b.natural_order))
        });
        candidates.truncate(max_candidates);

        // 分段上屏所需：标注每个候选实际消费的输入字节数。
        // code 为 input 的前缀（如 "ni" ⊂ "nihao"）→ 只消费该前缀，选中后保留剩余拼音续转；
        // 否则（整句/前缀补全/非前缀子串）消费整串。0 表示未知（由调用方按整串处理）。
        for c in candidates.iter_mut() {
            c.consumed_length = if !c.code.is_empty() && input.starts_with(&c.code) {
                c.code.len()
            } else {
                input.len()
            };
        }

        let (preedit_display, completed_syllables, partial_syllable) =
            self.compute_composition(input);
        let has_partial = !partial_syllable.is_empty();
        let is_empty = candidates.is_empty();

        Ok(ConvertResult {
            candidates,
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
}
