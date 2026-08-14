//! **wind-host：核心 ↔ 宿主的接缝（纯数据 + trait）。**
//!
//! # 这道缝为什么存在
//!
//! 主输入路的既有契约是**为厚宿主设计的**：TSF 与 IMKit 都在协调器之前替它过滤按键
//! （`OnTestKeyDown`）、拥有组合区、上报光标坐标、执行编辑动作。协调器的
//! `handle_key_event` 因此隐含三条前置条件——「你只会送我该处理的键」「组合区归你画」
//! 「坐标你会给我」。两个桌面宿主碰巧都满足，这些假设从未被检验。
//!
//! Android 的 `InputMethodService` 是**薄宿主**：只给原始按键和一个 `InputConnection`，
//! 其余全由输入法自己决定。于是每条隐含假设都要在 FFI 层补一块，而补出来的每一块都是
//! 对核心内部逻辑的**再实现**——实测一个会话里同形 bug 出现三次（空缓冲功能键失效、
//! 英文模式字母失效，以及必然还会有的下一个），根因全是「宿主手写的判据与核心漂移」。
//!
//! 本 crate 把那些判据与语义**收进核心**，宿主只依赖这里定义的类型：
//!
//! | 类型 | 取代宿主侧的什么 |
//! |------|------------------|
//! | [`KeyProbe`] + `should_handle_key` | 手写的「这个键该不该送进去」 |
//! | [`EditOp`] / [`KeyOutcome`] | 对 `KeyAction` 各变体的猜测式映射 |
//! | [`InputSnapshot`] | 从渲染命令里反推输入状态 |
//! | [`Readiness`] | 猜哪条路径会触发惰性构建、然后手动预热 |
//!
//! # 设计原则：宿主自治，厚宿主可忽略
//!
//! 这里**不做能力协商**（`HostProfile { has_pre_filter, has_caret, … }` 让核心分支适配）。
//! 那会在核心里长出组合爆炸的分支，且每个组合都难测。方向是反过来的：API 默认按
//! **最薄的宿主**设计，厚宿主忽略自己已经有的部分即可。TSF 有 `OnTestKeyDown`，它可以
//! 不调 `should_handle_key`；但这个函数必须先存在、且是唯一真相源。

#![forbid(unsafe_code)]

mod edit;
mod key;
mod ready;
mod snapshot;

pub use edit::{EditOp, KeyOutcome, TimingHint};
pub use key::{KeyProbe, Modifiers};
pub use ready::Readiness;
pub use snapshot::{InputSnapshot, ModeFlags};

/// 薄宿主的统一入口：一个实现对应一个输入会话。
///
/// 协调器实现它；Android FFI 只经此 trait 调用，不碰协调器内部。判据是：
/// **移动端新增需求时，改动应落在这个 trait 及其实现上，而不是散进核心各处。**
pub trait HostSession: Send + Sync {
    /// 该键是否交给输入法处理。
    ///
    /// 返回 `false` 时宿主必须执行默认行为（把键还给应用），**不要**再调
    /// [`Self::process_key`]。判错的代价是统一的：核心对不该收的键返回「已消费但无输出」，
    /// 宿主当成消费后既不上屏也不执行默认行为——键就这么静默消失。
    fn should_handle_key(&self, probe: &KeyProbe) -> bool;

    /// 处理按键，产出宿主无关的编辑指令流。
    fn process_key(&self, probe: &KeyProbe) -> KeyOutcome;

    /// 当前输入态快照（编码区 / 候选 / 分页 / 模式）。
    ///
    /// 与推送通道**同源**：推送是「变了通知你」，这里是「现在是什么」。自绘宿主在
    /// 重建视图（如横竖屏切换）时需要后者，靠缓存上一帧推送是不可靠的。
    fn snapshot(&self) -> InputSnapshot;

    /// 就绪状态。词库/索引的构建是惰性的，首次查询可能同步阻塞数秒
    /// （真机实测 2.8s）；宿主据此决定是等待、还是先显示未就绪提示。
    fn readiness(&self) -> Readiness;

    /// 触发后台准备（幂等，非阻塞）。返回 `false` 表示已在进行或已就绪。
    fn prepare(&self) -> bool;
}
