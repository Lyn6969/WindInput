//! 通用候选词筛选容器：封装「原始候选 + 可选筛选视图」。
//!
//! - 原始数据始终保留在 `original` 字段中，筛选构建独立的显示列表
//! - 清除筛选即复制原始数据恢复全量显示
//!
//! 使用场景：
//! - 辅助码：`set_candidates`（快照）+ `set_filter`（重筛）+ `clear_filter`（还原）
//!
//! # ⛔ 曾有一个 `filtered_out()`（被滤集迭代器），已删除
//!
//! 它写来预备「翻页放宽」用，但那条路一直没接过来——而辅助码是**明确不做**翻页放宽的
//! （见 `aux_code_does_not_relax_scope_on_page_end`），所以既没有消费者也没有在途的
//! 消费者。留着的害处不是占地方，是它**按 `text` 建 HashSet 做差集**：同一段文本在
//! `original` 里出现两次而只有一条通过筛选时，两条都会被当成「已显示」，被滤的那条
//! 就此漏出结果。这个错在没有消费者时完全看不出来，等哪天有人把它接上翻页放宽，
//! 收到的是一个静默漏项的列表。
//!
//! 真要重做：按**下标**记录哪些进了 `displayed`（`set_filter` 时顺手存一份
//! `Vec<usize>`），别再按文本比对。同理删掉的还有 `is_filtered` / `original` /
//! `into_original`——都是没有消费者的推测式接口。

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
        }
    }

    /// 替换原始候选列表，同时重置显示为全量。
    ///
    /// 语义：新数据到来（引擎 convert 结果），旧快照失效。
    pub fn set_candidates(&mut self, candidates: Vec<Candidate>) {
        self.original = candidates;
        self.displayed = self.original.clone();
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
    }

    /// 清除筛选视图，恢复全量显示。
    pub fn clear_filter(&mut self) {
        self.displayed = self.original.clone();
    }

    /// 当前显示的候选（筛选后的子集或全量）。
    pub fn displayed(&self) -> &[Candidate] {
        &self.displayed
    }

    /// 当前显示的候选数量。
    pub fn len(&self) -> usize {
        self.displayed.len()
    }

    /// 当前是否为空。
    pub fn is_empty(&self) -> bool {
        self.displayed.is_empty()
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
    }

    #[test]
    fn set_candidates_shows_all() {
        let mut store = CandidateStore::new();
        store.set_candidates(vec![cand("a", true), cand("b", false)]);
        assert_eq!(store.len(), 2, "新数据到来即全量显示");
    }

    #[test]
    fn set_filter_builds_view() {
        let mut store = CandidateStore::new();
        store.set_candidates(vec![cand("王", true), cand("尪", false), cand("往", true)]);
        store.set_filter(|c| c.is_common);
        assert_eq!(store.len(), 2);
        let texts: Vec<_> = store.displayed().iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["王", "往"]);
    }

    /// 快照不受筛选影响：清除筛选能原样拿回全量，且顺序不变。
    /// （辅助码的退格还原与退出还原全靠这一条。）
    #[test]
    fn clear_filter_restores_full() {
        let mut store = CandidateStore::new();
        store.set_candidates(vec![cand("a", true), cand("b", false)]);
        store.set_filter(|c| c.is_common);
        assert_eq!(store.len(), 1);
        store.clear_filter();
        let texts: Vec<_> = store.displayed().iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["a", "b"], "原始快照与原序都要拿得回来");
    }

    /// 新数据到来即丢弃旧筛选。
    ///
    /// 判据取「新数据全都显示出来」而非查一个状态位：新装的 `c` 是 `is_common=false`，
    /// 旧筛选若还挂着就会把它滤掉、len 为 0。状态位断言反而测不出真实显示面。
    #[test]
    fn set_candidates_clears_filter() {
        let mut store = CandidateStore::new();
        store.set_candidates(vec![cand("a", true), cand("b", false)]);
        store.set_filter(|c| c.is_common);
        store.set_candidates(vec![cand("c", false)]);
        assert_eq!(store.len(), 1, "旧筛选必须随新数据一起丢弃");
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

    /// 反复筛选恒从**快照**重筛，不是在上一次的结果上再筛。
    ///
    /// 辅助码的退格还原全靠这条：`m` → `mz` → 退回 `m` 时，第二次的 `m` 必须能把
    /// 上一步被 `mz` 滤掉的候选拿回来。在已筛结果上叠筛的话它们再也回不来。
    #[test]
    fn filter_always_reruns_from_snapshot() {
        let mut store = CandidateStore::new();
        store.set_candidates(vec![cand("a", true), cand("b", false), cand("c", true)]);
        store.set_filter(|c| c.text == "a");
        assert_eq!(store.len(), 1);
        store.set_filter(|c| c.is_common);
        let texts: Vec<_> = store.displayed().iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["a", "c"], "被上一次筛掉的 c 必须能回来");
    }
}
