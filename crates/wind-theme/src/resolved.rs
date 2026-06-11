//! 已解析的 V3 主题
//!
//! 与 Go 版本 `wind_input/pkg/theme/resolved.go` 对齐。

use crate::views::ResolvedViews;

/// 已解析的 V3 主题
#[derive(Debug, Clone, Default)]
pub struct ResolvedV3 {
    pub views: ResolvedViews,
    pub is_dark: bool,
}
