//! 格子构建（Lattice）+ 多切分评分
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/lattice.go` 对齐。
//! 构建词图并支持多路径评分，用于 Viterbi 解码。

use crate::pinyin::dag::Dag;
use crate::pinyin::syllable::SyllableTrie;
use crate::pinyin::fuzzy::{FuzzyConfig, FuzzyMatcher};
use wind_dict::cached::CachedDict;

/// 格子节点
#[derive(Debug, Clone)]
pub struct LatticeNode {
    pub start: usize,
    pub end: usize,
    pub word: String,
    pub syllables: Vec<String>,
    pub log_prob: f64,
}

/// 格子构建器
pub struct LatticeBuilder {
    /// 最大词长（音节数）
    max_word_len: usize,
    /// 单字惩罚
    single_char_penalty: f64,
    /// 功能词奖励
    function_word_bonus: f64,
}

impl LatticeBuilder {
    pub fn new() -> Self {
        Self {
            max_word_len: 6,
            single_char_penalty: -3.0,
            function_word_bonus: 2.0,
        }
    }

    /// 构建格子
    ///
    /// 对每个起始位置，尝试 1~max_word_len 个连续音节组合，
    /// 在词典中查找匹配的词，构建 LatticeNode。
    pub fn build(
        &self,
        input: &str,
        trie: &SyllableTrie,
        dict: &CachedDict,
        fuzzy_config: Option<&FuzzyConfig>,
    ) -> Vec<Vec<LatticeNode>> {
        let dag = Dag::build(input, trie);
        let syllables = dag.maximum_match();
        let input_len = input.len();

        // nodes[end_pos] = 所有在 end_pos 结束的节点
        let mut nodes: Vec<Vec<LatticeNode>> = vec![Vec::new(); input_len + 1];

        for start in 0..syllables.len() {
            for end in (start + 1)..=syllables.len().min(start + self.max_word_len) {
                let code: String = syllables[start..end].join("");
                let char_start: usize = syllables[..start].iter().map(|s| s.len()).sum();
                let char_end: usize = syllables[..end].iter().map(|s| s.len()).sum();

                if char_end > input_len {
                    continue;
                }

                // 查找词典
                let results = dict.search(&code);
                for (text, weight, _order) in &results {
                    let log_prob = self.word_log_prob(text, *weight);
                    nodes[char_end].push(LatticeNode {
                        start: char_start,
                        end: char_end,
                        word: text.clone(),
                        syllables: syllables[start..end].to_vec(),
                        log_prob,
                    });
                }

                // 模糊拼音变体
                if let Some(fuzzy) = fuzzy_config {
                    let variants = FuzzyMatcher::fuzzy_variants(&code, fuzzy);
                    for variant in variants {
                        let variant_results = dict.search(&variant);
                        for (text, weight, _order) in &variant_results {
                            // 去重
                            if !nodes[char_end].iter().any(|n| n.word == *text && n.start == char_start) {
                                let log_prob = self.word_log_prob(text, *weight) - 0.5; // 模糊匹配轻微惩罚
                                nodes[char_end].push(LatticeNode {
                                    start: char_start,
                                    end: char_end,
                                    word: text.clone(),
                                    syllables: syllables[start..end].to_vec(),
                                    log_prob,
                                });
                            }
                        }
                    }
                }
            }
        }

        nodes
    }

    /// 计算词的对数概率
    fn word_log_prob(&self, word: &str, dict_weight: i32) -> f64 {
        let char_count = word.chars().count();
        let base_prob = (dict_weight as f64 + 1.0).ln();

        if char_count == 1 {
            if is_function_word(word) {
                base_prob + self.function_word_bonus
            } else {
                base_prob + self.single_char_penalty
            }
        } else {
            base_prob + 3.0 * (char_count as f64).sqrt()
        }
    }
}

/// 是否为功能词
fn is_function_word(word: &str) -> bool {
    matches!(
        word,
        "的" | "了" | "在" | "是" | "我" | "你" | "他" | "她" | "它"
            | "们" | "这" | "那" | "有" | "不" | "人" | "大" | "一"
            | "和" | "就" | "都" | "而" | "及" | "与" | "或" | "但"
            | "把" | "被" | "让" | "给" | "从" | "向" | "对" | "以"
            | "也" | "还" | "又" | "再" | "很" | "太" | "最" | "更"
            | "没" | "无" | "非" | "未" | "别" | "莫" | "勿" | "休"
    )
}
