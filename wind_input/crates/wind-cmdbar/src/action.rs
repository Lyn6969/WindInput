//! 命令栏动作
//!
//! 对照 Go `wind_input/internal/cmdbar/action.go`。把 `$CC` 的动作区分为
//! 「文本上屏」(Text) 与「纯副作用」(Effect)。
//!
//! 优化：Go 用闭包 `Run func() (string, error)` 延迟求值；Rust 改为 [`ResolvedAction`]
//! 持有动作 [`Expr`]，在 [`ResolvedAction::run`] 调用时按**当前** ctx 重新求值
//! （同样实现 `type(last())` 每次触发重取 last() 的延迟语义，且不引入闭包生命周期）。

use crate::ast::Expr;
use crate::context::EvalContext;
use crate::error::Result;
use crate::registry::Registry;

/// 由 [`crate::eval`] 特例拦截、**不经 registry 查找**的动作名。
///
/// 单一真相源：eval 的拦截分支与 [`crate::capability`] 的能力分级都读这里。
/// 各写各的名单会让新增的拦截名在分级侧变成「registry 查不到」，进而被从危规则
/// 判成高危——正常短语一旦被泛滥的警示淹没，警示本身就失效了。
pub const EVAL_INTERCEPTED: &[&str] = &["type"];

/// 名字是否由 eval 特例拦截。见 [`EVAL_INTERCEPTED`]。
pub fn is_eval_intercepted(name: &str) -> bool {
    EVAL_INTERCEPTED.contains(&name)
}

/// 动作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// 纯副作用，[`ResolvedAction::run`] 返回空串。
    Effect,
    /// 文本上屏，[`ResolvedAction::run`] 返回待插入文本。
    Text,
}

/// eval 产出给宿主的统一执行单元。`expr` 在 [`run`](Self::run) 时延迟求值。
///
/// - `Text`：`expr` 是 `type(arg)` 里的 `arg`，run 返回其求值文本（宿主走 InsertText 上屏）。
/// - `Effect`：`expr` 是动作调用本身，run 触发副作用并返回空串。
#[derive(Debug, Clone)]
pub struct ResolvedAction {
    pub kind: ActionKind,
    pub expr: Expr,
}

impl ResolvedAction {
    pub fn text(expr: Expr) -> Self {
        Self {
            kind: ActionKind::Text,
            expr,
        }
    }

    pub fn effect(expr: Expr) -> Self {
        Self {
            kind: ActionKind::Effect,
            expr,
        }
    }

    /// 按当前 `ctx` 执行：Text 返回上屏文本，Effect 触发副作用后返回空串。
    pub fn run(&self, ctx: &dyn EvalContext, reg: &Registry) -> Result<String> {
        let s = crate::eval::eval_expr(&self.expr, ctx, reg)?;
        match self.kind {
            ActionKind::Text => Ok(s),
            ActionKind::Effect => Ok(String::new()),
        }
    }
}
