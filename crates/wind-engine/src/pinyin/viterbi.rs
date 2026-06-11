//! Viterbi 解码
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/viterbi.go` 对齐。
//! 使用动态规划找到最优词序列。

/// Viterbi 解码结果
#[derive(Debug, Clone)]
pub struct ViterbiResult {
    pub words: Vec<String>,
    pub log_prob: f64,
}

/// 词节点（用于构建 lattice）
#[derive(Debug, Clone)]
pub struct WordNode {
    pub start: usize,
    pub end: usize,
    pub word: String,
    pub log_prob: f64,
}

/// DP 状态
#[derive(Clone)]
struct DpEntry {
    log_prob: f64,
    prev_pos: usize,
    word: String,
}

/// Viterbi 解码器
pub struct ViterbiDecoder {
    /// 单字惩罚（负值 = 惩罚）
    single_char_penalty: f64,
    /// 功能词奖励
    function_word_bonus: f64,
}

impl ViterbiDecoder {
    pub fn new() -> Self {
        Self {
            single_char_penalty: -3.0,
            function_word_bonus: 2.0,
        }
    }

    /// Viterbi 解码：找到最优词序列
    ///
    /// 输入：
    /// - `nodes`: 按 endPos 索引的词节点列表（nodes[endPos] = 所有在 endPos 结束的词）
    /// - `input_len`: 输入字符串长度
    ///
    /// 输出：最优词序列
    pub fn decode(&self, nodes: &[Vec<WordNode>], input_len: usize) -> ViterbiResult {
        if input_len == 0 {
            return ViterbiResult {
                words: Vec::new(),
                log_prob: 0.0,
            };
        }

        // dp[i] = 到达位置 i 的最优路径
        let mut dp: Vec<DpEntry> = (0..=input_len)
            .map(|_| DpEntry {
                log_prob: f64::NEG_INFINITY,
                prev_pos: 0,
                word: String::new(),
            })
            .collect();
        dp[0].log_prob = 0.0;

        // 前向 DP
        for end_pos in 1..=input_len {
            if end_pos > nodes.len() {
                continue;
            }
            for node in &nodes[end_pos - 1] {
                let start_pos = node.start;
                if dp[start_pos].log_prob == f64::NEG_INFINITY {
                    continue;
                }

                let total_prob = dp[start_pos].log_prob + node.log_prob;
                if total_prob > dp[end_pos].log_prob {
                    dp[end_pos] = DpEntry {
                        log_prob: total_prob,
                        prev_pos: start_pos,
                        word: node.word.clone(),
                    };
                }
            }
        }

        // 回溯
        let mut words = Vec::new();
        let mut pos = input_len;

        // 从最远可达位置回溯
        while pos > 0 && dp[pos].log_prob == f64::NEG_INFINITY {
            pos -= 1;
        }

        while pos > 0 {
            let entry = &dp[pos];
            if entry.word.is_empty() {
                break;
            }
            words.push(entry.word.clone());
            pos = entry.prev_pos;
        }

        words.reverse();

        ViterbiResult {
            words,
            log_prob: dp[input_len].log_prob,
        }
    }

    /// 计算词的对数概率（考虑长度惩罚）
    pub fn word_log_prob(&self, word: &str, dict_weight: i32) -> f64 {
        let char_count = word.chars().count();
        let base_prob = (dict_weight as f64 + 1.0).ln();

        if char_count == 1 {
            // 单字：检查是否为功能词
            if is_function_word(word) {
                base_prob + self.function_word_bonus
            } else {
                base_prob + self.single_char_penalty
            }
        } else {
            // 多字词：奖励
            base_prob + 3.0 * (char_count as f64).sqrt()
        }
    }
}

/// 是否为功能词（代词、助词、介词等）
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
