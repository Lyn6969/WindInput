//! View 定义（已解析的像素值版本）
//!
//! 与 Go 版本 `wind_input/pkg/theme/views.go` 对齐。

/// 已解析的 View 节点
#[derive(Debug, Clone, Default)]
pub struct RVNode {
    pub margin: [f64; 4],
    pub padding: [f64; 4],
    pub background_color: Option<u32>,
}

/// 已解析的 Views 集合
#[derive(Debug, Clone, Default)]
pub struct ResolvedViews {
    pub window: RVNode,
    pub candidate_list: RVNode,
    pub item: RVNode,
    pub text: RVNode,
}
