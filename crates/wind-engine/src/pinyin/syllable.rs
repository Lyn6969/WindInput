//! 音节 Trie（~400 个合法拼音音节）
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/syllable_trie.go` 对齐。

/// 音节 Trie
pub struct SyllableTrie {
    // TODO: trie 结构
}

impl SyllableTrie {
    pub fn new() -> Self {
        Self {}
    }

    /// 在指定位置匹配所有可能的音节（最长优先）
    pub fn match_at(&self, _input: &str, _pos: usize) -> Vec<String> {
        Vec::new()
    }

    /// 检查是否为合法音节
    pub fn contains(&self, _syllable: &str) -> bool {
        false
    }

    /// 检查是否为某音节的前缀
    pub fn has_prefix(&self, _prefix: &str) -> bool {
        false
    }
}
