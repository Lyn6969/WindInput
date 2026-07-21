//! 语言模型（Unigram / Bigram）
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/lm.go` 对齐。
//! 基于词典权重的简化语言模型。

use std::collections::HashMap;
use std::path::Path;
use std::sync::RwLock;
use wind_dict::unigram::UnigramReader;

/// Unigram 查找接口
pub trait UnigramLookup: Send + Sync {
    fn log_prob(&self, word: &str) -> f64;
    fn contains(&self, word: &str) -> bool;
    fn char_based_score(&self, word: &str) -> f64;
    fn boost_user_freq(&self, word: &str, delta: i32);
}

/// 解析 unigram.txt（`词\t频次`，`#` 注释）为 (词, 频次) 列表。
pub fn parse_unigram_freqs(path: &Path) -> anyhow::Result<Vec<(String, f64)>> {
    let content = std::fs::read_to_string(path)?;
    let mut freqs = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split('\t');
        if let (Some(word), Some(freq_s)) = (it.next(), it.next()) {
            if !word.is_empty() {
                if let Ok(freq) = freq_s.trim().parse::<f64>() {
                    if freq > 0.0 {
                        freqs.push((word.to_string(), freq));
                    }
                }
            }
        }
    }
    if freqs.is_empty() {
        anyhow::bail!("unigram txt empty: {}", path.display());
    }
    Ok(freqs)
}

/// mmap 版 Unigram 模型：词频数据走 mmap（几乎不占常驻内存），
/// 仅 user_freq（用户选词加成）在内存。优先选用此实现。
pub struct MmapUnigram {
    /// 经 [`wind_dict::reader_pool`] 按路径共享：pinyin / shuangpin / 混输子引擎都指向
    /// 同一个 `<cache>/pinyin/unigram.wdb`，此前各映射一份。
    reader: std::sync::Arc<UnigramReader>,
    user_freq: RwLock<HashMap<String, i32>>,
}

impl MmapUnigram {
    pub fn new(reader: std::sync::Arc<UnigramReader>) -> Self {
        Self {
            reader,
            user_freq: RwLock::new(HashMap::new()),
        }
    }

    pub fn size(&self) -> usize {
        self.reader.key_count() as usize
    }
}

impl UnigramLookup for MmapUnigram {
    fn log_prob(&self, word: &str) -> f64 {
        let base = match self.reader.lookup(word) {
            Some(lp) => lp as f64,
            None if word.chars().count() > 1 => self.char_based_score(word),
            None => self.reader.min_prob() as f64,
        };
        let freq = *self
            .user_freq
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(word)
            .unwrap_or(&0);
        if freq > 0 {
            base + ((freq as f64) * 0.5).min(5.0)
        } else {
            base
        }
    }

    fn contains(&self, word: &str) -> bool {
        self.reader.contains(word)
    }

    fn char_based_score(&self, word: &str) -> f64 {
        let chars: Vec<char> = word.chars().collect();
        if chars.is_empty() {
            return self.reader.min_prob() as f64;
        }
        let sum: f64 = chars.iter().map(|c| self.log_prob(&c.to_string())).sum();
        sum / chars.len() as f64
    }

    fn boost_user_freq(&self, word: &str, delta: i32) {
        let mut freq = self.user_freq.write().unwrap_or_else(|e| e.into_inner());
        let entry = freq.entry(word.to_string()).or_insert(0);
        *entry = (*entry + delta).min(100);
    }
}

/// 从文件加载的 Unigram 模型（对齐 Go `UnigramModel`）。
///
/// 文件格式：`词语\t频次`，`#` 开头为注释。
/// `log_prob(word) = ln(freq/total)`；OOV 单字回退 `min_prob = ln(0.5/total)`，
/// 多字 OOV 用字符平均（避免合法多字词被单字组合碾压）。
pub struct UnigramModel {
    log_probs: HashMap<String, f64>,
    user_freq: RwLock<HashMap<String, i32>>,
    min_prob: f64,
}

impl UnigramModel {
    /// 从 unigram.txt 加载
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut freqs: Vec<(String, f64)> = Vec::new();
        let mut total = 0.0f64;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut it = line.split('\t');
            match (it.next(), it.next()) {
                (Some(word), Some(freq_s)) if !word.is_empty() => {
                    if let Ok(freq) = freq_s.trim().parse::<f64>() {
                        if freq > 0.0 {
                            freqs.push((word.to_string(), freq));
                            total += freq;
                        }
                    }
                }
                _ => {}
            }
        }
        if total == 0.0 {
            anyhow::bail!("unigram model is empty: {}", path.display());
        }
        let mut log_probs = HashMap::with_capacity(freqs.len());
        for (w, f) in freqs {
            log_probs.insert(w, (f / total).ln());
        }
        Ok(Self {
            log_probs,
            user_freq: RwLock::new(HashMap::new()),
            min_prob: (0.5 / total).ln(),
        })
    }

    pub fn size(&self) -> usize {
        self.log_probs.len()
    }
}

impl UnigramLookup for UnigramModel {
    fn log_prob(&self, word: &str) -> f64 {
        let base = if let Some(p) = self.log_probs.get(word) {
            *p
        } else if word.chars().count() > 1 {
            self.char_based_score(word)
        } else {
            self.min_prob
        };
        let freq = *self
            .user_freq
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(word)
            .unwrap_or(&0);
        if freq > 0 {
            base + ((freq as f64) * 0.5).min(5.0)
        } else {
            base
        }
    }

    fn contains(&self, word: &str) -> bool {
        self.log_probs.contains_key(word)
    }

    fn char_based_score(&self, word: &str) -> f64 {
        let chars: Vec<char> = word.chars().collect();
        if chars.is_empty() {
            return self.min_prob;
        }
        // 逐字走 log_prob（含 user_freq boost），与 Go CharBasedScore→LogProb 链一致。
        // 单字不会再递归进本函数（log_prob 仅对多字 OOV 调用 char_based_score）。
        let sum: f64 = chars.iter().map(|c| self.log_prob(&c.to_string())).sum();
        sum / chars.len() as f64
    }

    fn boost_user_freq(&self, word: &str, delta: i32) {
        let mut freq = self.user_freq.write().unwrap_or_else(|e| e.into_inner());
        let entry = freq.entry(word.to_string()).or_insert(0);
        *entry = (*entry + delta).min(100);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unigram_load_and_logprob() {
        let tmp = std::env::temp_dir().join("wind_unigram_test.txt");
        std::fs::write(&tmp, "# comment\n的\t100\n中国\t40\n爱\t10\n").unwrap();
        let m = UnigramModel::load(&tmp).unwrap();
        assert_eq!(m.size(), 3);
        // 高频词 log_prob 更大（更接近 0）
        assert!(m.log_prob("的") > m.log_prob("中国"));
        assert!(m.log_prob("中国") > m.log_prob("爱"));
        assert!(m.contains("中国"));
        // OOV 单字回退 min_prob；多字 OOV 用字符平均
        assert!(m.log_prob("龘") <= m.log_prob("爱"));
        // 用户频率 boost
        let before = m.log_prob("爱");
        m.boost_user_freq("爱", 4);
        assert!(m.log_prob("爱") > before);
        let _ = std::fs::remove_file(&tmp);
    }
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
        let total: f64 = word
            .chars()
            .map(|c| {
                let s = c.to_string();
                *probs.get(&s).unwrap_or(&self.default_prob)
            })
            .sum();
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
