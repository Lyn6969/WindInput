//! 格子构建（Lattice）
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/lattice.go` 对齐。

/// 格子节点
#[derive(Debug, Clone)]
pub struct LatticeNode {
    pub start: usize,
    pub end: usize,
    pub word: String,
    pub syllables: Vec<String>,
    pub log_prob: f64,
}
