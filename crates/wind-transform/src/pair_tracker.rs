//! 自动配对标点跟踪器
//!
//! 与 Go 版本 `wind_input/internal/transform/pair_tracker.go` 对齐。

/// 配对条目
#[derive(Debug, Clone)]
pub struct PairEntry {
    pub left: char,
    pub right: char,
}

/// 配对跟踪器
pub struct PairTracker {
    stack: Vec<PairEntry>,
}

impl PairTracker {
    pub fn new() -> Self {
        Self { stack: Vec::new() }
    }

    /// 压入配对
    pub fn push(&mut self, left: char, right: char) {
        self.stack.push(PairEntry { left, right });
    }

    /// 查看栈顶
    pub fn peek(&self) -> Option<&PairEntry> {
        self.stack.last()
    }

    /// 弹出栈顶
    pub fn pop(&mut self) -> Option<PairEntry> {
        self.stack.pop()
    }

    /// 清空
    pub fn clear(&mut self) {
        self.stack.clear();
    }
}
