//! 快捷输入格式表的**用户调整**（右键调序 / 停用 / 恢复默认）。
//!
//! 三个落点各司其职，缺一个就出问题：
//!
//! | 落点 | 内容 | 谁写 |
//! |---|---|---|
//! | `data/system.quick.toml` | 格式模板与出厂顺序 | 出厂 / 高级用户手写 |
//! | `userdata.redb` 的 `quick_format` 表 | 用户的调序与停用 | 本模块（右键） |
//! | [`Coordinator::quick_adjust`] | 上一行的运行时镜像 | 本模块（写库时同步） |
//!
//! **GUI 调整绝不回写 `system.quick.toml`**：那会抢走高级用户手写文件的所有权
//! （重写丢注释与排版），更糟的是让普通用户点两下右键就永久脱离出厂更新——
//! 整份覆盖的代价必须是知情选择，不能是右键的副作用。
//!
//! ## 与候选调整（shadow）的分界
//!
//! shadow 的键是 `(方案, 输入码)`；快捷输入的「输入码」是 `2026.6.19` 这种具体值，
//! 把格式调整存进去，用户调完次日换个日期就失效。故本模块另用一张按**类别**索引的表，
//! 且**不复用 `candidate_op_scope`**——那个判据回答的是「有没有词库落点」，
//! 混输确实没有，它返回 `None` 是对的。

use crate::coordinator::{Coordinator, State};
use wind_quick_input::{FormatAdjust, FormatKind};

/// 右键菜单能对一条格式做的事。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickFormatOp {
    /// 移到本类首位。
    MoveTop,
    /// 上移一位。
    MoveUp,
    /// 下移一位。
    MoveDown,
    /// 不再显示这种格式。
    Disable,
    /// 恢复本类的全部默认（顺序 + 显示）。
    ///
    /// 粒度是**整类**而非单条：被停用的格式不出现在候选里，右键点不到，
    /// 没有整类重置就再也开不回来了。单条恢复要等设置页。
    ResetKind,
}

impl QuickFormatOp {
    /// 复用候选菜单的动作枚举——语义一一对应，省掉一套跨 crate 的新枚举与菜单 id。
    ///
    /// 两处语义有偏移，菜单标签必须相应改写（在 `show_candidate_menu` 里）：
    /// - `Delete` 对候选是「从词库屏蔽这个词」，对格式是「不再显示这种写法」；
    /// - `Reset` 对候选是「恢复这一条」，对格式是「恢复**整类**」（停用后点不到单条）。
    pub fn from_candidate_op(op: wind_ui::manager::CandidateOp) -> Self {
        use wind_ui::manager::CandidateOp as C;
        match op {
            C::MoveTop => Self::MoveTop,
            C::MoveUp => Self::MoveUp,
            C::MoveDown => Self::MoveDown,
            C::Delete => Self::Disable,
            C::Reset => Self::ResetKind,
        }
    }
}

/// 右键作用域：这条候选属于哪一类格式、id 是什么、在本类里排第几。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickFormatScope {
    pub kind: FormatKind,
    pub format_id: String,
    /// 该条在**本类候选**中的下标（上移/下移的基准）。
    ///
    /// 不是页内下标，也不是全列表下标：候选列表可能混着 calc / date / number 三类
    /// （由 `mix_modes.members` 决定），而 `position` 是**组内**语义。拿全列表下标去写
    /// position，用户会看到「上移一位」跳过好几条。
    pub index_in_kind: usize,
}

/// 候选 id 的前缀。与短语的 `phrase:` 同域不同前缀——两者都放在 `Candidate::id` 里，
/// 靠前缀分辨归属。
const ID_PREFIX: &str = "quick:";

/// 生成快捷输入候选的稳定 id：`quick:{kind}:{格式 id}`。
///
/// 候选文本逐次输入都不同（`2026年6月19日` / `2026年6月20日`），右键要认的是**格式**，
/// 按文本认人必然失配——与短语 `date` 候选需要 `cand_id` 是同一个理由。
pub fn quick_cand_id(kind: FormatKind, format_id: &str) -> String {
    format!("{ID_PREFIX}{}:{format_id}", kind.as_str())
}

/// 从候选 id 解析回 (类别, 格式 id)；不是快捷输入候选则 `None`。
pub fn parse_quick_cand_id(id: &str) -> Option<(FormatKind, String)> {
    let rest = id.strip_prefix(ID_PREFIX)?;
    let (kind, format_id) = rest.split_once(':')?;
    if format_id.is_empty() {
        return None;
    }
    Some((FormatKind::parse(kind)?, format_id.to_string()))
}

impl Coordinator {
    /// 从 store 装载用户调整到运行时镜像。启动时调用一次；写库后也走它保持一致。
    pub(crate) fn reload_quick_adjust(&self) {
        let Some(store) = self.store.as_ref() else {
            return; // headless：无 store = 无调整 = 出厂顺序
        };
        let rows = match store.list_quick_format() {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("快捷输入格式调整: 读取失败，本次按出厂顺序: {e}");
                return;
            }
        };
        let mut map = std::collections::HashMap::new();
        for (kind, rec) in rows {
            map.insert(
                kind,
                FormatAdjust {
                    moved: rec.moved.into_iter().map(|m| (m.id, m.position)).collect(),
                    disabled: rec.disabled,
                },
            );
        }
        if let Ok(mut w) = self.quick_adjust.write() {
            *w = map;
        }
    }

    /// 取整张调整表的快照（候选生成用）。
    ///
    /// 返回副本而不是持锁引用：候选生成期间会调用 cmdbar 求值等外部逻辑，
    /// 持读锁跨越那段是自找死锁。表极小（至多 4 个类别），clone 成本可忽略。
    pub(crate) fn quick_adjust_snapshot(&self) -> wind_quick_input::FormatAdjustMap {
        self.quick_adjust
            .read()
            .map(|m| m.clone())
            .unwrap_or_default()
    }

    /// 取某类的用户调整副本（无则空调整 = 出厂顺序）。
    pub(crate) fn quick_adjust_of(&self, kind: FormatKind) -> FormatAdjust {
        self.quick_adjust
            .read()
            .ok()
            .and_then(|m| m.get(kind.as_str()).cloned())
            .unwrap_or_default()
    }

    /// 当前高亮候选是否可做格式调整，返回它的类别与格式 id。
    ///
    /// 判据**独立于 `candidate_op_scope`**：后者问的是「有没有词库落点」（混输没有，
    /// 故对它返回 `None`），这里问的是「这条候选是不是某条格式渲染出来的」。
    /// 两个判据混用会让格式调整要么整个不可用、要么误落到主方案的词库上。
    pub(crate) fn quick_format_scope(
        &self,
        state: &State,
        page_local: usize,
    ) -> Option<QuickFormatScope> {
        let (start, end) = self.page_range(state);
        let idx = start + page_local;
        if idx >= end || idx >= state.candidates.len() {
            return None;
        }
        let (kind, format_id) = parse_quick_cand_id(&state.candidates[idx].id)?;
        // 组内下标：只数同类的快捷候选。列表里混着别的来源时，全列表下标会让
        // 「上移一位」跳过好几条。
        let index_in_kind = state
            .candidates
            .iter()
            .take(idx)
            .filter(|c| parse_quick_cand_id(&c.id).is_some_and(|(k, _)| k == kind))
            .count();
        Some(QuickFormatScope {
            kind,
            format_id,
            index_in_kind,
        })
    }

    /// 菜单动作分发：快捷输入的格式候选走格式调整，其余走词库 shadow。
    ///
    /// 两条路径共用同一组 [`CandidateOp`]（语义一一对应，见 [`QuickFormatOp::from_candidate_op`]），
    /// 只是落点不同。**判据必须与菜单构造侧同源**（都是 [`Coordinator::quick_format_scope`]）——
    /// 菜单给了入口而这里落到另一条路径，用户会看到「点了没反应」且日志干净。
    pub(crate) fn candidate_or_quick_format_op(
        &self,
        op: wind_ui::manager::CandidateOp,
        page_local: usize,
    ) {
        let scope = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            self.quick_format_scope(&state, page_local)
        };
        let Some(scope) = scope else {
            return self.candidate_op(op, page_local);
        };
        self.apply_quick_format_op(&scope, QuickFormatOp::from_candidate_op(op));
        // 立即重排：不刷新的话，用户得退出重进才看得到新顺序。
        // 走 mix 路径——快捷输入的候选在 `mix_buffer` 上，主路径的 `update_candidates`
        // 读 `input_buffer`（此处恒空），用错的后果不是「不刷新」而是候选窗被清空。
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.update_mix_candidates(&mut state);
        self.notify_ui_update(&state);
    }

    /// 执行一次格式调整：写库 → 回灌镜像。
    ///
    /// ⚠️ 两步都要做。只写库不回灌，用户会看到「调了没反应，重启才生效」。
    pub(crate) fn apply_quick_format_op(&self, scope: &QuickFormatScope, op: QuickFormatOp) {
        let current_index = scope.index_in_kind;
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let kind = scope.kind.as_str();
        let id = scope.format_id.as_str();
        let r = match op {
            QuickFormatOp::MoveTop => store.move_quick_format(kind, id, 0),
            // 首位再上移 = 原地不动（菜单侧应已灰显，这里兜住手滑与并发）
            QuickFormatOp::MoveUp => {
                store.move_quick_format(kind, id, current_index.saturating_sub(1))
            }
            // 下移不设上界：越界由渲染期 clamp 到末尾（条目数会因停用而变）
            QuickFormatOp::MoveDown => store.move_quick_format(kind, id, current_index + 1),
            QuickFormatOp::Disable => store.set_quick_format_enabled(kind, id, false),
            QuickFormatOp::ResetKind => store.reset_quick_format_kind(kind),
        };
        if let Err(e) = r {
            tracing::warn!("快捷输入格式调整失败 kind={kind} id={id} op={op:?}: {e}");
            return;
        }
        self.reload_quick_adjust();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cand_id_roundtrip() {
        let id = quick_cand_id(FormatKind::Date, "date.lunar");
        assert_eq!(id, "quick:date:date.lunar");
        let (kind, fid) = parse_quick_cand_id(&id).unwrap();
        assert_eq!(kind, FormatKind::Date);
        assert_eq!(fid, "date.lunar");
    }

    #[test]
    fn all_kinds_roundtrip() {
        for k in [
            FormatKind::Date,
            FormatKind::YearMonth,
            FormatKind::Number,
            FormatKind::Calc,
        ] {
            let (kind, _) = parse_quick_cand_id(&quick_cand_id(k, "x.y")).unwrap();
            assert_eq!(kind, k, "kind={} 未能往返", k.as_str());
        }
    }

    /// ★ 非快捷输入的候选 id 不得被误解析——短语候选也放在同一个 `Candidate::id` 字段里。
    #[test]
    fn foreign_ids_are_rejected() {
        assert!(parse_quick_cand_id("").is_none());
        assert!(parse_quick_cand_id("phrase:date:$Y年").is_none(), "短语 id");
        assert!(parse_quick_cand_id("quick:").is_none());
        assert!(parse_quick_cand_id("quick:date").is_none(), "缺格式 id");
        assert!(parse_quick_cand_id("quick:date:").is_none(), "空格式 id");
        assert!(parse_quick_cand_id("quick:weather:x").is_none(), "未知类别");
    }

    /// 格式 id 里含冒号时，只按第一个冒号切分，其余归 format_id。
    #[test]
    fn format_id_may_contain_colon() {
        let (_, fid) = parse_quick_cand_id("quick:date:a:b").unwrap();
        assert_eq!(fid, "a:b");
    }
}
