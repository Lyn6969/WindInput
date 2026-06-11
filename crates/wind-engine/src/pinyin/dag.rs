//! DAG 构建与最大匹配
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/dag.go` 对齐。

/// DAG 节点
#[derive(Debug, Clone)]
pub struct DagNode {
    pub start: usize,
    pub end: usize,
    pub syllables: Vec<String>,
}

/// 有向无环图
pub struct Dag {
    nodes: Vec<Vec<DagNode>>,
    input: String,
}

impl Dag {
    /// 构建 DAG
    pub fn build(input: &str, syllable_trie: &crate::pinyin::syllable::SyllableTrie) -> Self {
        // TODO
        Self {
            nodes: Vec::new(),
            input: input.to_string(),
        }
    }

    /// DP 最大匹配（非贪心）
    pub fn maximum_match(&self) -> Vec<String> {
        // TODO
        Vec::new()
    }

    /// 枚举所有路径
    pub fn all_paths(&self, _max_paths: usize) -> Vec<Vec<String>> {
        // TODO
        Vec::new()
    }
}
