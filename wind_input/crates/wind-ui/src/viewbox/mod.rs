//! Box-model 布局引擎
//!
//! 与 Go 版本 `wind_input/internal/ui/viewbox*.go` 对齐。
//!
//! ⚠️ 当前未启用:实际盒模型布局/绘制走 `view.rs`。本模块是更完整 box-model 引擎的
//! 占位骨架（仅 `types.rs` 有 View struct，其余子模块待实现）；勿误当作活跃渲染路径。

pub mod build;
pub mod image;
pub mod layout;
pub mod paint;
pub mod types;
