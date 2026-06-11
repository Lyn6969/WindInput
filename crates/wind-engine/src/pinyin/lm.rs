//! 语言模型（Unigram / Bigram）
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/lm.go` 对齐。
//! 基于词典权重的简化语言模型。

use std::collections::HashMap;
use std::sync::RwLock;

/// Unigram 查找接口
pub trait UnigramLookup: Send + Sync {
    fn log_prob(&self, word: &str) -> f64;
    fn contains(&self, word: &str) -> bool;
    fn char_based_score(&self, word: &str) -> f64;
    fn boost_user_freq(&self, word: &str, delta: i32);
}

/// 基于词典权重的 Unigram 模型
pub struct DictUnigramModel {
    /// word -> log_probability
    probs: RwLock<HashMap<String, f64>>,
    /// 用户选择频率 boost
    user_freq: RwLock<HashMap<String, i32>>,
    /// 默认 OOV 概率
    default_prob: f64,
}

impl DictUnigramModel {
    pub fn new() -> Self {
        Self {
            probs: RwLock::new(HashMap::new()),
            user_freq: RwLock::new(HashMap::new()),
            default_prob: -10.0,
        }
    }

    /// 从词典权重构建模型
    pub fn build_from_dict(entries: &[(String, i32)]) -> Self {
        let model = Self::new();
        let mut probs = model.probs.write().unwrap();

        // 找到最大权重用于归一化
        let max_weight = entries.iter().map(|(_, w)| *w).max().unwrap_or(1).max(1) as f64;

        for (word, weight) in entries {
            let normalized = (*weight as f64 / max_weight).max(0.001);
            let log_prob = normalized.ln();
            probs.insert(word.clone(), log_prob);
        }

        drop(probs);
        model
    }
}

impl UnigramLookup for DictUnigramModel {
    fn log_prob(&self, word: &str) -> f64 {
        let probs = self.probs.read().unwrap();
        let base = *probs.get(word).unwrap_or(&self.default_prob);

        // 加上用户频率 boost（最多 +5.0）
        let user_boost = {
            let freq = self.user_freq.read().unwrap();
            *freq.get(word).unwrap_or(&0) as f64
        };
        let boost = (user_boost * 0.5).min(5.0);

        base + boost
    }

    fn contains(&self, word: &str) -> bool {
        let probs = self.probs.read().unwrap();
        probs.contains_key(word)
    }

    fn char_based_score(&self, word: &str) -> f64 {
        // 对于 OOV 词，使用平均每字得分
        let probs = self.probs.read().unwrap();
        let char_count = word.chars().count().max(1);
        let total: f64 = word.chars().map(|c| {
            let s = c.to_string();
            *probs.get(&s).unwrap_or(&self.default_prob)
        }).sum();
        total / char_count as f64
    }

    fn boost_user_freq(&self, word: &str, delta: i32) {
        let mut freq = self.user_freq.write().unwrap();
        let entry = freq.entry(word.to_string()).or_insert(0);
        *entry = (*entry + delta).min(100); // 上限 100
    }
}

/// Bigram 模型（简化版：线性插值）
pub struct SimpleBigramModel {
    /// (prev_word, word) -> log_probability
    probs: RwLock<HashMap<(String, String), f64>>,
    /// unigram 回退
    unigram: Box<dyn UnigramLookup>,
    /// 插值权重
    lambda: f64,
}

impl SimpleBigramModel {
    pub fn new(unigram: Box<dyn UnigramLookup>) -> Self {
        Self {
            probs: RwLock::new(HashMap::new()),
            unigram,
            lambda: 0.7,
        }
    }

    /// 获取 bigram 对数概率
    pub fn log_prob(&self, prev: &str, word: &str) -> f64 {
        let probs = self.probs.read().unwrap();
        let key = (prev.to_string(), word.to_string());

        if let Some(&bigram_prob) = probs.get(&key) {
            // 线性插值：lambda * P_bigram + (1-lambda) * P_unigram
            let unigram_prob = self.unigram.log_prob(word);
            self.lambda * bigram_prob + (1.0 - self.lambda) * unigram_prob
        } else {
            // 未见过的 bigram：回退到 unigram，减 1.0 惩罚
            self.unigram.log_prob(word) - 1.0
        }
    }
}
