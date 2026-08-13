//! 评分器 + 缩写匹配
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/ranker.go` 对齐。
//! 支持缩写匹配（如 "bzd" → "不知道"）。

use crate::pinyin::dag::Dag;
use crate::pinyin::syllable::SyllableTrie;
use wind_dict::cached::CachedDict;

/// 缩写匹配候选
#[derive(Debug, Clone)]
pub struct AbbrevCandidate {
    pub text: String,
    pub code: String,
    pub weight: i32,
    pub matched_syllables: Vec<String>,
}

/// 缩写匹配器
pub struct AbbrevMatcher;

impl AbbrevMatcher {
    /// 检查输入是否可能是缩写（每个字符都是单个字母且对应音节首字母）
    pub fn is_abbreviation(input: &str, trie: &SyllableTrie) -> bool {
        if input.len() < 2 {
            return false;
        }

        // 检查每个字符是否为某个音节的首字母
        for ch in input.chars() {
            if !ch.is_ascii_lowercase() {
                return false;
            }
            // 检查是否有以该字母开头的音节
            let prefix = ch.to_string();
            if !trie.is_prefix(&prefix) {
                return false;
            }
        }

        // 不应是一个完整的音节序列（否则走正常路径）
        let dag = Dag::build(input, trie);
        let syllables = dag.maximum_match();
        let matched_len: usize = syllables.iter().map(|s| s.len()).sum();
        matched_len < input.len()
    }

    /// 生成缩写候选
    ///
    /// 策略：取输入每个字母作为音节首字母，在词典中查找匹配的多字词。
    /// 例如：输入 "bzd"，查找 3 字词，其中第 1 个字的拼音以 b 开头，
    ///       第 2 个字的拼音以 z 开头，第 3 个字的拼音以 d 开头。
    pub fn find_candidates(
        input: &str,
        _trie: &SyllableTrie,
        dict: &CachedDict,
        limit: usize,
    ) -> Vec<AbbrevCandidate> {
        let letters: Vec<char> = input.chars().collect();
        let len = letters.len();

        if !(2..=6).contains(&len) {
            return Vec::new();
        }

        // 收集所有匹配的候选
        let mut candidates = Vec::new();

        // 使用前缀查找获取所有以第一个字母开头的词
        let prefix = letters[0].to_string();
        let entries = dict.search_prefix(&prefix, 2000);

        for (_code, text, weight, _order) in entries {
            let text_chars: Vec<char> = text.chars().collect();

            // 长度必须匹配
            if text_chars.len() != len {
                continue;
            }

            // 检查每个字的拼音首字母是否匹配
            let mut matched = true;
            let mut matched_syllables = Vec::new();

            for (i, ch) in text_chars.iter().enumerate() {
                // 查找这个字的拼音
                let char_str = ch.to_string();
                let char_entries = dict.search(&char_str);
                let mut found = false;

                for (code, _, _) in &char_entries {
                    if code.starts_with(letters[i]) {
                        matched_syllables.push(code.clone());
                        found = true;
                        break;
                    }
                }

                if !found {
                    matched = false;
                    break;
                }
            }

            if matched {
                candidates.push(AbbrevCandidate {
                    text,
                    code: input.to_string(),
                    weight,
                    matched_syllables,
                });
            }
        }

        // 按权重排序
        candidates.sort_by_key(|c| std::cmp::Reverse(c.weight));
        candidates.truncate(limit);
        candidates
    }
}
