//! 求值上下文
//!
//! 与 Go 版本 `wind_input/internal/cmdbar/context.go` 对齐。

/// 求值上下文接口
pub trait EvalContext {
    fn input(&self) -> &str;
    fn last(&self, n: usize) -> &str;
    fn clipboard(&self) -> &str;
    fn selection(&self) -> &str;
    fn app_name(&self) -> &str;
    fn window_title(&self) -> &str;
    fn env(&self, name: &str) -> &str;
}
