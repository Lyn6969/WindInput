//! Viterbi 解码
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/viterbi.go` 对齐。
//! 使用动态规划找到最优词序列。

/// Viterbi 解码结果
#[derive(Debug, Clone)]
pub struct ViterbiResult {
    pub words: Vec<String>,
    pub log_prob: f64,
    /// 最优路径**实际采用**的音节边界（全输入空间的起始位 bitmask）。
    ///
    /// 多路径切分下同一串输入可有多种切法，整句是按其中哪一条拼出来的，只有解码器
    /// 知道。此前整句候选一律标 `maximum_match` 的切分——单路径时那恰好就是真相，
    /// 多路径时便成了谎报（`xianjiaotongdaxue` 实走 `xi|an|…` 却标成 `xian|…`）。
    /// 该字段供整句候选回填 `Candidate::boundary`，双拼校验与用户造词都依赖它。
    ///
    /// 0 = 无可用信息（解码失败 / 输入超 64 字节，超出 bitmask 表达范围）。
    pub boundary: u64,
}

/// 词节点（用于构建 lattice）
#[derive(Debug, Clone)]
pub struct WordNode {
    pub start: usize,
    pub end: usize,
    pub word: String,
    /// 本节点所采用切分的音节起始位 bitmask，相对 `start`（见 `LatticeNode::syl_mask`）
    pub syl_mask: u64,
    pub log_prob: f64,
}

/// DP 状态
#[derive(Clone)]
struct DpEntry {
    log_prob: f64,
    prev_pos: usize,
    word: String,
    syl_mask: u64,
}

/// Viterbi 解码器
///
/// 节点权重（含单字惩罚/虚词加成/实体词加成）在 lattice 构建阶段由 `score_node`
/// 计算并写入 `WordNode.log_prob`，解码器只做最优路径 DP。
pub struct ViterbiDecoder {}

impl Default for ViterbiDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ViterbiDecoder {
    pub fn new() -> Self {
        Self {}
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
                boundary: 0,
            };
        }

        // dp[i] = 到达位置 i 的最优路径
        let mut dp: Vec<DpEntry> = (0..=input_len)
            .map(|_| DpEntry {
                log_prob: f64::NEG_INFINITY,
                prev_pos: 0,
                word: String::new(),
                syl_mask: 0,
            })
            .collect();
        dp[0].log_prob = 0.0;

        // 前向 DP
        // nodes[end_pos] = 所有在字节位置 end_pos 结束的词（与 LatticeBuilder::build 的
        // 存储约定一致：node 存入 nodes[char_end]）。此前误读 nodes[end_pos-1] 导致
        // 整段 Viterbi 长句解码恒为空（差一 bug）。
        for end_pos in 1..=input_len {
            if end_pos >= nodes.len() {
                continue;
            }
            for node in &nodes[end_pos] {
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
                        syl_mask: node.syl_mask,
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

        // 回溯的同时把各节点的音节 mask 平移到全输入空间累加，得到整句的真实边界。
        // 输入超 64 字节时 bitmask 表达不下，一律给 0（= 无信息，下游降级放行）。
        let mut boundary = 0u64;
        let expressible = input_len <= 64;
        while pos > 0 {
            let entry = &dp[pos];
            if entry.word.is_empty() {
                break;
            }
            words.push(entry.word.clone());
            if expressible {
                boundary |= entry.syl_mask << entry.prev_pos;
            }
            pos = entry.prev_pos;
        }

        words.reverse();

        ViterbiResult {
            words,
            log_prob: dp[input_len].log_prob,
            boundary: if expressible { boundary } else { 0 },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 锁定 nodes[end_pos] 的索引约定（C1 差一回归）。
    /// 输入 "nihao"（5 字节），节点在 char_end=5 结束。若 decode 误读
    /// nodes[end_pos-1]，则永远取不到结束于 5 的节点，words 为空。
    #[test]
    fn test_decode_index_convention() {
        let input_len = 5usize; // "nihao"
        let mut nodes: Vec<Vec<WordNode>> = vec![Vec::new(); input_len + 1];
        // 单个双字词 "你好" 覆盖 [0,5]，结束于位置 5
        nodes[5].push(WordNode {
            start: 0,
            end: 5,
            word: "你好".to_string(),
            syl_mask: 0b101, // ni|hao
            log_prob: 10.0,
        });
        let decoder = ViterbiDecoder::new();
        let result = decoder.decode(&nodes, input_len);
        assert_eq!(result.words, vec!["你好".to_string()], "应解码出 你好");
        assert!(result.log_prob.is_finite());
        assert_eq!(result.boundary, 0b101, "整句边界应回填节点自身的切分");
    }

    /// 两段路径：ni(0..2) + hao(2..5)，验证多节点拼接。
    #[test]
    fn test_decode_two_segments() {
        let input_len = 5usize;
        let mut nodes: Vec<Vec<WordNode>> = vec![Vec::new(); input_len + 1];
        nodes[2].push(WordNode {
            start: 0,
            end: 2,
            word: "你".to_string(),
            syl_mask: 0b1,
            log_prob: 3.0,
        });
        nodes[5].push(WordNode {
            start: 2,
            end: 5,
            word: "好".to_string(),
            syl_mask: 0b1,
            log_prob: 3.0,
        });
        let decoder = ViterbiDecoder::new();
        let result = decoder.decode(&nodes, input_len);
        assert_eq!(result.words, vec!["你".to_string(), "好".to_string()]);
        // 两个单音节节点分别起于 0 与 2 → {0,2}
        assert_eq!(result.boundary, 0b101);
    }
}
