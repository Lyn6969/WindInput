//! View 结构体定义
//!
//! 与 Go 版本 `wind_input/internal/ui/viewbox.go` 中的 View struct 对齐。

/// 布局方向
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutKind {
    Row,
    Column,
    Stack,
}

/// View 节点
#[derive(Debug, Clone)]
pub struct View {
    pub name: String,
    pub layout: LayoutKind,
    pub margin: [f64; 4],
    pub padding: [f64; 4],
    pub children: Vec<View>,
    // TODO: 完整字段
}
