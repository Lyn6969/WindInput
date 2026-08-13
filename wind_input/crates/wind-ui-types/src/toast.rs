//! Toast 通知的位置与类型（渲染配色等实现细节留在渲染端）。

/// Toast 屏幕位置（相对光标所在显示器工作区）。API 可按需扩展更多位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastPosition {
    Center,
    TopCenter,
    BottomCenter,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl ToastPosition {
    /// 从配置字符串解析（未知→BottomCenter）。
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "center" => Self::Center,
            "top_center" | "top" => Self::TopCenter,
            "top_left" => Self::TopLeft,
            "top_right" => Self::TopRight,
            "bottom_left" => Self::BottomLeft,
            "bottom_right" => Self::BottomRight,
            _ => Self::BottomCenter,
        }
    }
}

/// Toast 类型（决定强调色）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

impl ToastKind {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "success" => Self::Success,
            "error" => Self::Error,
            _ => Self::Info,
        }
    }
}
