//! **候选的拉取式读取与绝对下标选词**：薄宿主（移动端）的候选接口。
//!
//! # 为什么推送帧不够用
//!
//! 协调器每次按键推送的 `UpdateCandidates` 是**当页**候选（`selected`/`hover` 的注释
//! 明写"页内下标"）。这套形状源自桌面：候选窗只画一页，数字键 1-9 选当页，`-`/`=` 翻页。
//!
//! 移动端的候选栏是一条可滚动的长列表，用户想点第几个点第几个。沿用推送帧的后果是
//! 三个看起来无关的症状，其实同一个根：
//! - 候选栏做了滚动，但一帧只有 6~9 个词，**滑动等于空转**；
//! - 「展开全部候选」的面板里也只有那 6~9 个，右上角却写着 `1/36`；
//! - 点选靠**合成数字键**（页内 1-9），于是永远选不到第 10 个及以后。
//!
//! 分页本身要保留：它决定空格上屏的目标与数字键语义。本模块只是把「读多少」和
//! 「选哪个」从视图分页里解放出来。
//!
//! # 性能：为什么是窗口而不是全量
//!
//! 全量候选可以很长（前缀匹配下拼音常有几百上千条）。每次按键把它整个跨 JNI 送一遍
//! 有三处代价，且**都落在主线程**：
//! 1. UniFFI 逐条序列化字符串；
//! 2. 宿主侧候选栏要为每条 `measureText` 才能算出命中区域；
//! 3. 读取期间持 state 锁，与引擎查询争用。
//!
//! 所以接口是 `(offset, limit)` 窗口，并在核心侧再压一道
//! [`MAX_WINDOW`] 硬上限——宿主传多大都不会真把全量搬过去。宿主按需续取：
//! 起手只要够铺满一两屏，用户滚到尾部再要下一段。

use wind_host::KeyOutcome;

use crate::coordinator::Coordinator;
use crate::edit_ops;

/// 单次拉取的候选条数硬上限。
///
/// 这是**核心侧的护栏**而不是建议值：宿主传 `u32::MAX` 也只会拿到这么多。
/// 取 200 的依据是横向候选栏一屏约 8 条、竖排展开面板一屏约 12 条，200 条足够
/// 连续滚动若干屏而不触发续取，同时序列化开销仍在一帧预算内。
pub const MAX_WINDOW: usize = 200;

/// 候选全量的一个窗口。
#[derive(Debug, Clone, Default)]
pub struct CandidateWindow {
    /// 窗口内的候选文本
    pub items: Vec<String>,
    /// 窗口起点（原样回送，供宿主校验自己拿到的是哪一段）
    pub offset: usize,
    /// **全量**候选总数（宿主据此判断要不要续取，以及画滚动条）
    pub total: usize,
    /// 当前高亮候选的**绝对**下标（空格上屏的目标）。
    ///
    /// 推送帧里的 `selected` 是页内下标，滚动列表用不了——那是"第几页的第几个"，
    /// 而这里要的是"整条列表里的第几个"。
    pub selected: usize,
}

impl Coordinator {
    /// 读取候选全量的一个窗口。**纯读**，不改任何状态。
    ///
    /// `limit` 会被压到 [`MAX_WINDOW`]；`offset` 越界时返回空 `items`（但 `total`
    /// 仍然有效，宿主据此知道自己越界了而不是"候选没了"）。
    pub fn candidate_window(&self, offset: usize, limit: usize) -> CandidateWindow {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let total = state.candidates.len();
        let selected = self.highlighted_global_index(&state);
        let end = offset.saturating_add(limit.min(MAX_WINDOW)).min(total);
        let items = if offset >= total {
            Vec::new()
        } else {
            state.candidates[offset..end]
                .iter()
                .map(|c| c.text.clone())
                .collect()
        };
        CandidateWindow {
            items,
            offset,
            total,
            selected,
        }
    }

    /// **按绝对下标选词**，返回编辑指令流。
    ///
    /// 与 [`Coordinator::should_handle_key`] + `edit_ops::to_outcome` 是同一种形状：
    /// 宿主拿到 ops 就执行，不需要 push 通道。此前移动端点选候选靠合成数字键走按键路，
    /// 正是因为桌面的鼠标点选把上屏结果发进 push 管道，而 headless 那头没有消费端。
    ///
    /// 越界、`$CC` 命令候选、overlay 模式（临拼/临英/快捷输入）这些**不经主输入路**的
    /// 情形返回 `passthrough`（`consumed=false`、无 ops）——它们的副作用已在内部完成
    /// （命令已异步执行、overlay 已整串提交并复位），宿主不该再补一次上屏。
    pub fn select_candidate(&self, index: usize) -> KeyOutcome {
        match self.select_candidate_at(index) {
            Some(action) => edit_ops::to_outcome(action),
            None => KeyOutcome::passthrough(),
        }
    }
}
