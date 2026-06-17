//! 按键管线 / 模式决策（对齐 Go `pipeline_decider.go` 的*意图*，按 Rust 现状裁剪）。
//!
//! 设计依据：`docs/redesign/key-pipeline.md`。本模块是 S1「模式收编为单点决策」的落点。
//!
//! ## 与 Go 决策器的差异（刻意为之）
//!
//! Go 的 `decider` 持有一组 `Processor` trait 对象，并用 `applyEngineDiff(Capability)`
//! 在切换 processor 时挂卸**共享引擎**的词典层。Rust 现状不需要这套机制：
//!
//! - **Capability 不移植**：Rust 各模式按 schema id 独立查询引擎
//!   （`EngineManager::convert_with(schema_id, ...)`），不存在被多模式改写的共享引擎，
//!   因此没有「引擎副作用」需要单点统一。强行引入 Capability 只是死抽象。
//! - **CommitStrategy 推迟到 S4**：全码/空码上屏逻辑在 Rust coordinator 里尚未实现
//!   （`codetable::engine::should_auto_commit` 为空实现），现在没有调用点消费策略归属。
//!   按「避免过早抽象」原则，等 S4 实现全码上屏时再与真实调用点一起设计。
//!
//! ## 本阶段交付：单一活跃模式 + 单点分派
//!
//! 原先三个散装 bool（`temp_pinyin_mode/quick_input_mode/temp_english_mode`）+ 三处串行
//! `if` 分派 + 分散的激活/复位入口，收敛为单一 [`ModeKind`] 字段 `State.active`：
//!
//! - 结构上保证「同一时刻至多一个独占模式」（三 bool 无法表达这一不变式）。
//! - 键事件分派变为对 `state.active` 的单次 `match`（唯一入口）。
//! - S3 新增 URL / 特殊模式时，只需加 `ModeKind` 变体 + 一条 `match` 臂，而非再加 bool + 分支。
//!
//! 决策链顺序仍由 `Coordinator::handle_key_event` 承载（对齐 key-pipeline.md §2.1）；
//! 各模式的具体按键处理仍是 `Coordinator` 上与其紧耦合的 `handle_*_key` 方法，
//! 故采用 enum 分派而非 trait 对象（避免把 ~600 行逻辑套进 `Ctx` 间接层，更「boring」）。

/// 当前激活的独占输入模式。`None` 表示普通码表/拼音输入（default processor）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModeKind {
    /// 临时拼音：码表方案下经触发键临时切到拼音反查。
    TempPinyin,
    /// 快捷输入：分号触发的日期/计算器。
    QuickInput,
    /// 临时英文：Shift+字母触发的临时英文输入。
    TempEnglish,
    /// 网址模式：普通输入累积到某前缀（如 "www."/"http"）时夺取，原样累积 ASCII。
    Url,
    // S3 将追加：Special(u8)
}
