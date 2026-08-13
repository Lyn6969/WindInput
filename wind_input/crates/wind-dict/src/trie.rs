//! 内存前缀 Trie
//!
//! 与 Go 版本 `wind_input/internal/dict/trie.go` 对齐。

use std::collections::HashMap;
use wind_candidate::Candidate;

/// Trie 节点
struct TrieNode {
    children: HashMap<u8, Box<TrieNode>>,
    candidates: Vec<Candidate>,
}

impl TrieNode {
    fn new() -> Self {
        Self {
            children: HashMap::new(),
            candidates: Vec::new(),
        }
    }
}

/// 前缀 Trie
pub struct Trie {
    root: TrieNode,
}

impl Default for Trie {
    fn default() -> Self {
        Self::new()
    }
}

impl Trie {
    pub fn new() -> Self {
        Self {
            root: TrieNode::new(),
        }
    }

    /// 插入词条
    pub fn insert(&mut self, code: &str, candidate: Candidate) {
        let mut node = &mut self.root;
        for &b in code.as_bytes() {
            node = node
                .children
                .entry(b)
                .or_insert_with(|| Box::new(TrieNode::new()));
        }
        node.candidates.push(candidate);
    }

    /// 精确查找
    pub fn search(&self, code: &str, limit: usize) -> Vec<Candidate> {
        let mut node = &self.root;
        for &b in code.as_bytes() {
            match node.children.get(&b) {
                Some(child) => node = child,
                None => return Vec::new(),
            }
        }
        let mut results = node.candidates.clone();
        results.sort_by(wind_candidate::better);
        results.truncate(limit);
        results
    }

    /// 前缀查找
    pub fn search_prefix(&self, prefix: &str, limit: usize) -> Vec<Candidate> {
        let mut node = &self.root;
        for &b in prefix.as_bytes() {
            match node.children.get(&b) {
                Some(child) => node = child,
                None => return Vec::new(),
            }
        }
        let mut results = Vec::new();
        Self::collect_all(node, &mut results);
        results.sort_by(wind_candidate::better);
        results.truncate(limit);
        results
    }

    fn collect_all(node: &TrieNode, results: &mut Vec<Candidate>) {
        results.extend(node.candidates.iter().cloned());
        for child in node.children.values() {
            Self::collect_all(child, results);
        }
    }
}
