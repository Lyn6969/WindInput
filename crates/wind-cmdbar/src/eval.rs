//! AST 求值器
//!
//! 与 Go 版本 `wind_input/internal/cmdbar/eval/eval.go` 对齐。

use crate::action::ResolvedAction;
use crate::ast::Phrase;
use crate::context::EvalContext;
use crate::registry::Registry;

/// 求值结果
pub fn evaluate(
    phrase: &Phrase,
    ctx: &dyn EvalContext,
    reg: &Registry,
) -> anyhow::Result<(String, Vec<ResolvedAction>)> {
    // TODO
    Ok((String::new(), Vec::new()))
}
