//! 辅助码过滤会话：聚合「候选快照 + 辅助码缓冲 + 重筛」的辅助码专用筛选状态。
//!
//! 会话是辅助码模式的**状态机种子**：进入时由调用方注入原始候选快照（引擎 convert
//! 结果的单槽 memo），此后每次按键（push / pop 缓冲）都从**快照**重筛，而不是在已
//! 筛选的列表上再筛——否则退格还原时 kept 相对顺序会被上一步筛选打乱，候选栏无法
//! 还原到「上一层筛选」。
//!
//! 会话是纯逻辑、不接触文件系统、不依赖协调器状态，可在任意主机编译测试。
//! **不含显示态**：组合区（preedit）拼接、光标定位是协调器的职责（与 `State.preedit`/
//! caret 机制绑在一起），不在这里——避免把 coordinator 的显示约定拖进纯筛选 crate。
//!
//! **被滤候选直接丢弃**：`apply` 只返回命中者（候选窗只显示匹配词，如 `om` 配
//! 「时间」「实践」时实践消失），还原不靠残留标记——快照在手，退出/退格都从快照恢复。

use wind_candidate::{Candidate, CandidateStore};

use crate::filter::{AuxCodeFilterOptions, aux_code_matches};
use crate::table::AuxCodeTable;

/// 辅助码会话：进入时建立、退出/上屏时销毁，随 `ModeKind::AuxCode` 独占存在。
///
/// 承载筛选所需的全部种子——进入时引擎产出的原始候选快照（退格/退出据此还原原样
/// 顺序）与辅助码缓冲。外界只经 `State.aux` 持有它；缓冲 push/pop、从快照重筛等
/// 语义全在本类型内。
pub struct AuxCodeSession {
    /// 辅助码输入缓冲（字形辅助码串，如 "mz"）。
    buffer: String,
    /// 候选快照容器：持有原始候选快照，每次筛选都从它重筛（而非在已筛选的列表上再筛）。
    /// 使用 [`CandidateStore`] 统一管理快照的存储与访问。
    store: CandidateStore,
}

impl AuxCodeSession {
    /// 建立会话：缓冲置空，`original_candidates` 即进入时的原始候选快照。
    pub fn new(original_candidates: Vec<Candidate>) -> Self {
        let mut store = CandidateStore::new();
        store.set_candidates(original_candidates);
        Self {
            buffer: String::new(),
            store,
        }
    }

    /// 当前辅助码缓冲。
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    /// 辅助码缓冲是否为空。
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// 追加一个辅助码字符。
    pub fn push_char(&mut self, ch: char) {
        self.buffer.push(ch);
    }

    /// 弹出一个辅助码字符（无则 `None`）。
    pub fn pop_char(&mut self) -> Option<char> {
        self.buffer.pop()
    }

    /// 按当前缓冲对**原始候选快照**重筛，返回**通过筛选的候选**（保持快照相对顺序）。
    ///
    /// 被滤候选**不进返回列表**——辅助码是「字形二次筛选」，候选窗只显示匹配词
    /// （如 `om` 配「时间」「实践」时，实践被滤掉、不再可见）。还原不需要把被滤候选
    /// 留在列表里兜底：快照在手，退出用 [`Self::restore_original`]、退格用空缓冲
    /// passthrough（见 [`aux_code_matches`]），都能从快照恢复。空缓冲 / 空表同样由
    /// [`aux_code_matches`] 内部 passthrough（原样放行全部候选）。
    pub fn apply(
        &mut self,
        table: &AuxCodeTable,
        options: &AuxCodeFilterOptions,
    ) -> Vec<Candidate> {
        self.store
            .set_filter(|c| aux_code_matches(c, table, &self.buffer, options));
        self.store.displayed().to_vec()
    }

    /// 退出还原：清除筛选视图，返回原始候选快照。
    pub fn restore_original(&mut self) -> Vec<Candidate> {
        self.store.clear_filter();
        self.store.displayed().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::AuxCodeTable;
    use wind_candidate::CandidateSource;

    /// 构造最简候选（只填过滤用到的 text/source，其余用默认值）
    fn cand(text: &str) -> Candidate {
        Candidate {
            text: text.into(),
            source: CandidateSource::Pinyin,
            ..Default::default()
        }
    }

    fn sample_table() -> AuxCodeTable {
        AuxCodeTable::from_rows(vec![
            ('李', "mz"), // 木+子
            ('樱', "my"), // 木+婴
            ('林', "mm"), // 木+木
            ('河', "sk"), // 氵+可
            ('花', "ch"), // 艹+化
            ('草', "cz"), // 艹+早
        ])
    }

    fn session() -> AuxCodeSession {
        AuxCodeSession::new(vec![cand("李"), cand("樱"), cand("河"), cand("花")])
    }

    /// 缓冲 push/pop / is_empty / buffer 访问器
    #[test]
    fn buffer_ops() {
        let mut s = AuxCodeSession::new(Vec::new());
        assert!(s.is_empty());
        s.push_char('m');
        s.push_char('z');
        assert_eq!(s.buffer(), "mz");
        assert!(!s.is_empty());
        assert_eq!(s.pop_char(), Some('z'));
        assert_eq!(s.pop_char(), Some('m'));
        assert_eq!(s.pop_char(), None);
        assert!(s.is_empty());
    }

    /// apply：只返回通过筛选的候选（命中子序列），被滤候选**不进列表**
    #[test]
    fn apply_keeps_only_matching() {
        let t = sample_table();
        let mut s = session();
        s.push_char('m');
        let list = s.apply(&t, &AuxCodeFilterOptions::default());
        let texts: Vec<_> = list.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["李", "樱"],
            "m：命中子序列；河/花 被滤，不再出现在列表里"
        );
    }

    /// apply 每次从快照重筛（不叠加）：退格还原语义，而不是在已筛选列表上再筛
    #[test]
    fn apply_always_reesifts_from_snapshot() {
        let t = sample_table();
        let mut s = session();
        s.push_char('m');
        let first = s.apply(&t, &AuxCodeFilterOptions::default());
        assert_eq!(first.len(), 2);
        // 缓冲没变时反复重筛结果一致；缓冲清空 = 从快照原样放行
        s.pop_char();
        let cleared = s.apply(&t, &AuxCodeFilterOptions::default());
        assert_eq!(cleared.len(), 4, "空缓冲 passthrough：原样放行全部候选");
    }

    /// restore_original：还原快照（退出语义）
    #[test]
    fn restore_original_unmarks_snapshot() {
        let t = sample_table();
        let mut s = session();
        s.push_char('m');
        let applied = s.apply(&t, &AuxCodeFilterOptions::default());
        assert_eq!(applied.len(), 2, "apply 只返回命中者");
        let restored = s.restore_original();
        assert_eq!(
            restored.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            vec!["李", "樱", "河", "花"],
            "还原 = 快照原样顺序"
        );
    }
}
