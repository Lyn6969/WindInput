//! Viterbi 解码
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/viterbi.go` 对齐。

/// Viterbi 解码结果
#[derive(Debug, Clone)]
pub struct ViterbiResult {
    pub words: Vec<String>,
    pub log_prob: f64,
}
