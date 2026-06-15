//! wind-cmdbar: 命令栏系统（表达式解析、求值、内置函数）
//!
//! 与 Go 版本 `wind_input/internal/cmdbar/` 对齐。

pub mod action;
pub mod ast;
pub mod context;
pub mod eval;
pub mod funcs;
pub mod parser;
pub mod registry;
pub mod services;

pub use action::ResolvedAction;
pub use context::EvalContext;
pub use registry::Registry;
