//! 命令栏动作
//!
//! 与 Go 版本 `wind_input/internal/cmdbar/action.go` 对齐。

/// 动作类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Text,
    Effect,
}

/// 已解析的动作
#[derive(Debug, Clone)]
pub struct ResolvedAction {
    pub kind: ActionKind,
    pub text: String,
}
