//! 语言模型（Unigram / Bigram）
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/lm.go` 对齐。

/// Unigram 查找接口
pub trait UnigramLookup: Send + Sync {
    fn log_prob(&self, word: &str) -> f64;
    fn contains(&self, word: &str) -> bool;
    fn char_based_score(&self, word: &str) -> f64;
    fn boost_user_freq(&self, word: &str, delta: i32);
}
