//! 通用候选词筛选容器：封装「原始候选 + 可选筛选视图」。
//!
//! - 原始数据始终保留在 [`CandidateStore::original`] 中，筛选构建独立的显示列表
//! - 清除筛选即复制原始数据恢复全量显示
//! - [`CandidateStore::filtered_out`] 按需计算被滤集（用于翻页放宽），不存储副本
//!
//! 使用场景：
//! - 辅助码：`set_candidates`（快照）+ `set_filter`（重筛）+ `clear_filter`（还原）

use crate::Candidate;

/// 候选词筛选容器。
///
/// 持有原始候选列表（拥有所有权）和一个可选的筛选视图。筛选不修改原始数据，
/// 只构建一组新的候选列表（通过筛选的候选的克隆）。
///
/// # 示例
///
/// ```ignore
/// let mut store = CandidateStore::new();
/// store.set_candidates(vec![cand_a, cand_b, cand_c]);
/// store.set_filter(|c| c.is_common);         // 只保留常用词
/// assert_eq!(store.displayed().len(), 1);     // 只有 cand_a
/// store.clear_filter();
/// assert_eq!(store.displayed().len(), 3);     // 恢复全量
/// ```
pub struct CandidateStore {
    /// 原始候选（拥有所有权，筛选期间不修改）。
    original: Vec<Candidate>,
    /// 当前显示的候选（筛选后的子集或原始的克隆）。
    displayed: Vec<Candidate>,
    /// 是否处于筛选状态（精确追踪，不依赖长度启发）。
    filtered: bool,
}

impl Default for CandidateStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CandidateStore {
    /// 创建空容器。
    pub fn new() -> Self {
        Self {
            original: Vec::new(),
            displayed: Vec::new(),
            filtered: false,
        }
    }

    /// 替换原始候选列表，同时重置显示为全量。
    ///
    /// 语义：新数据到来（引擎 convert 结果），旧快照失效。
    pub fn set_candidates(&mut self, candidates: Vec<Candidate>) {
        self.original = candidates;
        self.displayed = self.original.clone();
        self.filtered = false;
    }

    /// 应用筛选谓词，构建筛选视图。
    ///
    /// 从 `original` 中选出所有使 `predicate` 返回 `true` 的候选，按原序组成显示列表。
    /// 重复调用会覆盖前一次的筛选（等效于先 `clear_filter` 再 `set_filter`）。
    pub fn set_filter(&mut self, mut predicate: impl FnMut(&Candidate) -> bool) {
        self.displayed = self
            .original
            .iter()
            .filter(|c| predicate(c))
            .cloned()
            .collect();
        self.filtered = true;
    }

    /// 清除筛选视图，恢复全量显示。
    pub fn clear_filter(&mut self) {
        self.displayed = self.original.clone();
        self.filtered = false;
    }

    /// 当前显示的候选（筛选后的子集或全量）。
    pub fn displayed(&self) -> &[Candidate] {
        &self.displayed
    }

    /// 原始全量候选（不受筛选影响）。
    pub fn original(&self) -> &[Candidate] {
        &self.original
    }

    /// 当前显示的候选数量。
    pub fn len(&self) -> usize {
        self.displayed.len()
    }

    /// 当前是否为空。
    pub fn is_empty(&self) -> bool {
        self.displayed.is_empty()
    }

    /// 是否处于筛选状态（调用过 `set_filter` 且未 `clear_filter`）。
    pub fn is_filtered(&self) -> bool {
        self.filtered
    }

    /// 被筛掉的候选迭代器（保持原序）。
    ///
    /// 用于翻页放宽：把被滤候选追加到末尾并标记 `is_scope_filtered`。
    /// 内部构建 `HashSet` 用于 O(1) 查找，候选列表通常 < 300 条，开销可忽略。
    pub fn filtered_out(&self) -> FilteredOut<'_> {
        let displayed_set: std::collections::HashSet<&str> =
            self.displayed.iter().map(|c| c.text.as_str()).collect();
        FilteredOut {
            original: &self.original,
            displayed_set,
            index: 0,
        }
    }

    /// 消费容器，返回原始候选列表。
    pub fn into_original(self) -> Vec<Candidate> {
        self.original
    }
}

/// 被筛候选的惰性迭代器。
///
/// 由 [`CandidateStore::filtered_out`] 返回，按原序遍历所有不在显示列表中的候选。
pub struct FilteredOut<'a> {
    original: &'a [Candidate],
    displayed_set: std::collections::HashSet<&'a str>,
    index: usize,
}

impl<'a> Iterator for FilteredOut<'a> {
    type Item = &'a Candidate;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.index >= self.original.len() {
                return None;
            }
            let i = self.index;
            self.index += 1;
            if !self.displayed_set.contains(self.original[i].text.as_str()) {
                return Some(&self.original[i]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CandidateSource;

    fn cand(text: &str, is_common: bool) -> Candidate {
        Candidate {
            text: text.into(),
            source: CandidateSource::Pinyin,
            is_common,
            ..Default::default()
        }
    }

    #[test]
    fn new_is_empty() {
        let store = CandidateStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert!(!store.is_filtered());
    }

    #[test]
    fn set_candidates_populates_original() {
        let mut store = CandidateStore::new();
        store.set_candidates(vec![cand("a", true), cand("b", false)]);
        assert_eq!(store.len(), 2);
        assert_eq!(store.original().len(), 2);
        assert!(!store.is_filtered());
    }

    #[test]
    fn set_filter_builds_view() {
        let mut store = CandidateStore::new();
        store.set_candidates(vec![cand("王", true), cand("尪", false), cand("往", true)]);
        store.set_filter(|c| c.is_common);
        assert!(store.is_filtered());
        assert_eq!(store.len(), 2);
        let texts: Vec<_> = store.displayed().iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["王", "往"]);
    }

    #[test]
    fn clear_filter_restores_full() {
        let mut store = CandidateStore::new();
        store.set_candidates(vec![cand("a", true), cand("b", false)]);
        store.set_filter(|c| c.is_common);
        assert_eq!(store.len(), 1);
        store.clear_filter();
        assert!(!store.is_filtered());
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn set_candidates_clears_filter() {
        let mut store = CandidateStore::new();
        store.set_candidates(vec![cand("a", true), cand("b", false)]);
        store.set_filter(|c| c.is_common);
        assert!(store.is_filtered());
        store.set_candidates(vec![cand("c", false)]);
        assert!(!store.is_filtered());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn filtered_out_yields_removed() {
        let mut store = CandidateStore::new();
        store.set_candidates(vec![cand("王", true), cand("尪", false), cand("往", true)]);
        store.set_filter(|c| c.is_common);
        let filtered: Vec<_> = store.filtered_out().map(|c| c.text.as_str()).collect();
        assert_eq!(filtered, vec!["尪"]);
    }

    #[test]
    fn filtered_out_empty_when_no_filter() {
        let mut store = CandidateStore::new();
        store.set_candidates(vec![cand("a", true)]);
        assert!(store.filtered_out().next().is_none());
    }

    #[test]
    fn filtered_out_empty_when_all_pass() {
        let mut store = CandidateStore::new();
        store.set_candidates(vec![cand("a", true), cand("b", true)]);
        store.set_filter(|_| true);
        assert!(store.is_filtered(), "set_filter(|true) 仍标记为筛选态");
        assert!(store.filtered_out().next().is_none());
    }

    #[test]
    fn filtered_out_preserves_order() {
        let mut store = CandidateStore::new();
        store.set_candidates(vec![
            cand("a", true),
            cand("b", false),
            cand("c", true),
            cand("d", false),
            cand("e", true),
        ]);
        store.set_filter(|c| c.is_common);
        let filtered: Vec<_> = store.filtered_out().map(|c| c.text.as_str()).collect();
        assert_eq!(filtered, vec!["b", "d"]);
    }

    #[test]
    fn displayed_preserves_original_order() {
        let mut store = CandidateStore::new();
        store.set_candidates(vec![
            cand("e", false),
            cand("a", true),
            cand("d", false),
            cand("b", true),
            cand("c", false),
        ]);
        store.set_filter(|c| c.is_common);
        let displayed: Vec<_> = store.displayed().iter().map(|c| c.text.as_str()).collect();
        assert_eq!(displayed, vec!["a", "b"]);
    }

    #[test]
    fn into_original_consumes() {
        let mut store = CandidateStore::new();
        store.set_candidates(vec![cand("x", true)]);
        let original = store.into_original();
        assert_eq!(original.len(), 1);
    }

    #[test]
    fn reapply_filter_overwrites() {
        let mut store = CandidateStore::new();
        store.set_candidates(vec![cand("a", true), cand("b", false), cand("c", true)]);
        store.set_filter(|c| c.is_common);
        assert_eq!(store.len(), 2);
        // 重新筛选：只留「a」
        store.set_filter(|c| c.text == "a");
        assert_eq!(store.len(), 1);
        assert_eq!(store.displayed()[0].text, "a");
    }

    #[test]
    fn original_unaffected_by_filter() {
        let mut store = CandidateStore::new();
        store.set_candidates(vec![cand("a", true), cand("b", false)]);
        store.set_filter(|c| c.is_common);
        assert_eq!(store.original().len(), 2, "original 不受筛选影响");
    }
}
