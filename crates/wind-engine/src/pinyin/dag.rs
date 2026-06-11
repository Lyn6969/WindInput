//! DAG 构建与最大匹配
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/dag.go` 对齐。
//! DP 最大匹配切分拼音音节。

use crate::pinyin::syllable::SyllableTrie;

/// DAG 节点
#[derive(Debug, Clone)]
pub struct DagNode {
    pub start: usize,
    pub end: usize,
    pub syllable: String,
}

/// 有向无环图
pub struct Dag {
    /// nodes[i] = 从位置 i 出发的所有边
    nodes: Vec<Vec<DagNode>>,
    input: String,
}

impl Dag {
    /// 构建 DAG：对每个位置匹配所有可能的音节
    pub fn build(input: &str, trie: &SyllableTrie) -> Self {
        let n = input.len();
        let mut nodes = vec![Vec::new(); n];

        for i in 0..n {
            let matches = trie.match_at(input, i);
            for syl in matches {
                let end = i + syl.len();
                nodes[i].push(DagNode {
                    start: i,
                    end,
                    syllable: syl,
                });
            }
        }

        Self {
            nodes,
            input: input.to_string(),
        }
    }

    /// DP 最大匹配（非贪心，覆盖最多字符）
    ///
    /// 为什么不用贪心： "henihejiele" 贪心选 "hen" 后 "i" 无法匹配。
    /// DP 选 "he"+"ni"+"he"+"jie"+"le" 覆盖全部。
    pub fn maximum_match(&self) -> Vec<String> {
        let n = self.input.len();
        if n == 0 {
            return Vec::new();
        }

        // dp[i] = 位置 i 之前最多覆盖的字符数，-1 表示不可达
        let mut dp = vec![-1i32; n + 1];
        dp[0] = 0;

        // prev[i] = 到达位置 i 的最优路径中，最后一个音节
        let mut prev_syl = vec![String::new(); n + 1];
        let mut prev_pos = vec![0usize; n + 1];

        for pos in 0..n {
            if dp[pos] < 0 {
                continue;
            }
            for node in &self.nodes[pos] {
                let end = node.end;
                let covered = dp[pos] + (end - pos) as i32;
                if covered > dp[end] {
                    dp[end] = covered;
                    prev_syl[end] = node.syllable.clone();
                    prev_pos[end] = pos;
                }
            }
        }

        // 从最远可达位置回溯
        let mut best_end = 0;
        for i in (0..=n).rev() {
            if dp[i] >= 0 {
                best_end = i;
                break;
            }
        }

        let mut result = Vec::new();
        let mut pos = best_end;
        while pos > 0 {
            let syl = prev_syl[pos].clone();
            if syl.is_empty() {
                break;
            }
            result.push(syl);
            pos = prev_pos[pos];
        }

        result.reverse();
        result
    }

    /// 获取未匹配的尾部（从最远可达位置到输入末尾）
    pub fn unmatched_tail(&self) -> &str {
        let n = self.input.len();
        if n == 0 {
            return "";
        }

        let mut dp = vec![-1i32; n + 1];
        dp[0] = 0;

        for pos in 0..n {
            if dp[pos] < 0 {
                continue;
            }
            for node in &self.nodes[pos] {
                let covered = dp[pos] + (node.end - pos) as i32;
                if covered > dp[node.end] {
                    dp[node.end] = covered;
                }
            }
        }

        // 找到最远可达位置
        let mut best = 0;
        for i in 0..=n {
            if dp[i] >= 0 {
                best = i;
            }
        }

        &self.input[best..]
    }

    /// 获取从指定位置开始的所有可能音节
    pub fn edges_from(&self, pos: usize) -> &[DagNode] {
        if pos < self.nodes.len() {
            &self.nodes[pos]
        } else {
            &[]
        }
    }

    /// 输入长度
    pub fn input_len(&self) -> usize {
        self.input.len()
    }

    /// 是否有从指定位置出发的边
    pub fn has_edges_from(&self, pos: usize) -> bool {
        pos < self.nodes.len() && !self.nodes[pos].is_empty()
    }
}
