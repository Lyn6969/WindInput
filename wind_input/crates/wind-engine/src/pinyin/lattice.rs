//! 格子构建（Lattice）+ 多切分评分
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/lattice.go` 对齐。
//! 构建词图并支持多路径评分，用于 Viterbi 解码。

use crate::pinyin::dag::Dag;
use crate::pinyin::fuzzy::{FuzzyConfig, FuzzyMatcher};
use crate::pinyin::lm::UnigramLookup;
use crate::pinyin::syllable::SyllableTrie;
use wind_dict::cached::CachedDict;

/// 虚词集合（单字时轻微惩罚，对齐 Go functionWords）
fn is_function_word(w: &str) -> bool {
    matches!(
        w,
        "了" | "的"
            | "地"
            | "得"
            | "着"
            | "过"
            | "我"
            | "你"
            | "他"
            | "她"
            | "它"
            | "们"
            | "这"
            | "那"
            | "和"
            | "与"
            | "在"
            | "把"
            | "被"
            | "让"
            | "从"
            | "到"
            | "对"
            | "向"
            | "跟"
            | "不"
            | "没"
            | "也"
            | "都"
            | "就"
            | "才"
            | "还"
            | "又"
            | "再"
            | "很"
            | "太"
            | "最"
            | "是"
            | "有"
            | "会"
            | "能"
            | "要"
            | "可"
            | "去"
            | "来"
            | "做"
            | "说"
            | "看"
            | "想"
    )
}

/// V+助词尾字（多字词以此结尾时降权，对齐 Go particleSuffixes）
fn is_particle_suffix(c: char) -> bool {
    matches!(c, '了' | '的' | '着' | '过' | '得' | '地')
}

/// 节点对数概率打分（对齐 Go lattice calcLogProb + 惩罚/加成）。
/// 无 unigram 时回退到归一化词典权重。
///
/// 对 crate 内可见：`PinyinEngine::convert` 用它给「覆盖全部输入的词典精确整词」
/// 算单节点等价分，使其与 Viterbi 整句在同一量纲比较（见 mod.rs step 1.5）。
pub(crate) fn score_node(word: &str, weight: i32, unigram: Option<&dyn UnigramLookup>) -> f64 {
    const SINGLE_CHAR_PENALTY: f64 = -3.0;
    const FUNCTION_WORD_BONUS: f64 = 2.0; // 虚词加成（Go 原名 functionWordPenalty，值为正）
    const VERB_PARTICLE_PENALTY: f64 = -1.0;
    const BASE_CONTENT_WORD_BONUS: f64 = 3.0;
    const CHAR_BASED_PENALTY: f64 = -2.0; // 多字 OOV 用字符平均估算时的惩罚（对齐 Go）
    const LOG_PROB_MIN: f64 = -15.0;
    const LOG_PROB_RANGE: f64 = 12.0;

    let chars: Vec<char> = word.chars().collect();
    let char_count = chars.len();

    let Some(ug) = unigram else {
        // 无 unigram：用词典权重归一化（与 Go calcLogProb 的 nil 分支一致）
        return weight as f64 / 100_000.0;
    };

    // 基础 logProb：单字或在 unigram 中的词直接取；多字 OOV 用字符平均 + 惩罚，
    // 避免高频字组合（如"接了"）虚高碾压有真实词频的词（如"和解"）。
    let mut log_prob = if char_count <= 1 || ug.contains(word) {
        ug.log_prob(word)
    } else {
        ug.char_based_score(word) + CHAR_BASED_PENALTY
    };

    if char_count == 1 {
        if is_function_word(word) {
            log_prob += FUNCTION_WORD_BONUS;
        } else {
            log_prob += SINGLE_CHAR_PENALTY;
        }
    } else if char_count > 1 {
        if chars
            .last()
            .map(|c| is_particle_suffix(*c))
            .unwrap_or(false)
        {
            log_prob += VERB_PARTICLE_PENALTY;
        } else if ug.contains(word) {
            let freq_factor = ((log_prob - LOG_PROB_MIN) / LOG_PROB_RANGE).clamp(0.0, 1.0);
            log_prob += BASE_CONTENT_WORD_BONUS * (char_count as f64).sqrt() * freq_factor;
        }
    }
    // Weight ≤ 0 = 字典标记的非标准读音映射（如 那→ne 方言读法 w=0）。其 unigram
    // 高频不应凌驾字典的显式判断——否则 Viterbi 会在 ne 音节选 那 而非 呢(w=262461)。
    // -10.0 足够压过典型虚词-实词间的 unigram 差距（~2-8），又留足余量使正确
    // 但低频的单字（w>0 正常条目）不被误伤。
    if weight <= 0 {
        log_prob -= 10.0;
    }
    log_prob
}

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
}

impl LatticeBuilder {
    pub fn new() -> Self {
        // 10 而非 6：6 会把「中华人民共和国」(7 音节) 挡在词图外，却放行它的语义碎片
        // 「中华人民共和」(freq=2，法律条文名切出来的残片)，于是 Viterbi 只能在
        // 「中华人民共和」+「过」之类的错误切分里挑最优。上限须覆盖常见长专名。
        Self { max_word_len: 10 }
    }

    /// 词图能容纳的最长词（音节数）。超过它的词典整词进不了 Viterbi，
    /// 需由 `PinyinEngine::convert` 的 step 1.5 单独兜底。
    pub fn max_word_len(&self) -> usize {
        self.max_word_len
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
        unigram: Option<&dyn UnigramLookup>,
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
                    let log_prob = score_node(text, *weight, unigram);
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
                            if !nodes[char_end]
                                .iter()
                                .any(|n| n.word == *text && n.start == char_start)
                            {
                                let log_prob = score_node(text, *weight, unigram) - 0.5; // 模糊匹配轻微惩罚
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
}
