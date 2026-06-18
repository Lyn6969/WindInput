//! 求值上下文
//!
//! 对照 Go `wind_input/internal/cmdbar/context.go`。`EvalContext` 把宿主运行时状态
//! （编码、上屏历史、剪贴板、前台应用、时间、服务束）暴露给求值器函数。
//! [`MemoryContext`] 是测试友好的内存实现；宿主侧另有适配器。

use crate::services::Services;
use chrono::{DateTime, Local};
use std::sync::Mutex;

/// 求值上下文接口。各取值方法对越界/缺失返回空串（与 Go 同）。
pub trait EvalContext {
    /// 当前输入编码（composition 快照）。
    fn input(&self) -> String;
    /// 倒数第 n 次上屏文本（1-based，1 为最近）；越界返回空。
    fn last(&self, n: i64) -> String;
    /// 剪贴板：n==0 或 1 为当前，n>1 为历史第 n 条（1-based）。
    fn clip(&self, n: i64) -> String;
    /// 前台应用中选中的文本；无选区返回空。
    fn sel(&self) -> String;
    /// 前台进程名（basename）。
    fn app(&self) -> String;
    /// 前台窗口标题。
    fn title(&self) -> String;
    /// 环境变量值；不存在返回空。
    fn env(&self, name: &str) -> String;
    /// 当前时间（测试可注入固定时钟）。
    fn now(&self) -> DateTime<Local>;
    /// 副作用服务束；纯求值场景可为 None，动作函数须自行防御。
    fn services(&self) -> Option<&Services>;
}

/// 固定容量的上屏历史环形缓冲。index 1 为最近一次 push。
pub struct History {
    inner: Mutex<HistoryInner>,
}

struct HistoryInner {
    buf: Vec<String>,
    cap: usize,
    head: usize, // 下一写位置
    full: bool,
}

impl History {
    /// 构造容量为 `capacity`（下钳到 1）的历史。
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        History {
            inner: Mutex::new(HistoryInner {
                buf: vec![String::new(); cap],
                cap,
                head: 0,
                full: false,
            }),
        }
    }

    /// 记录最近一条上屏。
    pub fn push(&self, s: impl Into<String>) {
        let mut h = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let head = h.head;
        h.buf[head] = s.into();
        h.head = (head + 1) % h.cap;
        if h.head == 0 {
            h.full = true;
        }
    }

    /// 取倒数第 n 条（1-based）；越界返回空。
    pub fn get(&self, n: i64) -> String {
        let h = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if n < 1 {
            return String::new();
        }
        let n = n as usize;
        let size = if h.full { h.cap } else { h.head };
        if n > size {
            return String::new();
        }
        let idx = (h.head + h.cap - n) % h.cap;
        h.buf[idx].clone()
    }

    /// 当前条目数。
    pub fn len(&self) -> usize {
        let h = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if h.full {
            h.cap
        } else {
            h.head
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 内存实现的 [`EvalContext`]，主要供测试与纯求值（无宿主）使用。
#[derive(Default)]
pub struct MemoryContext {
    pub input: String,
    pub history: Option<History>,
    /// 当前剪贴板（非空时优先）。
    pub clip: String,
    /// 剪贴板历史，index 0 为最新。
    pub clip_stack: Vec<String>,
    pub sel: String,
    pub app: String,
    pub title: String,
    pub env: std::collections::HashMap<String, String>,
    /// 固定时钟（None 时用 `Local::now()`）。
    pub clock: Option<DateTime<Local>>,
    pub services: Option<Services>,
}

impl MemoryContext {
    /// 带 16 条历史缓冲的空上下文。
    pub fn new() -> Self {
        MemoryContext {
            history: Some(History::new(16)),
            ..Default::default()
        }
    }

    pub fn with_input(mut self, input: impl Into<String>) -> Self {
        self.input = input.into();
        self
    }

    pub fn with_services(mut self, s: Services) -> Self {
        self.services = Some(s);
        self
    }
}

impl EvalContext for MemoryContext {
    fn input(&self) -> String {
        self.input.clone()
    }
    fn last(&self, n: i64) -> String {
        match &self.history {
            Some(h) => h.get(n),
            None => String::new(),
        }
    }
    fn clip(&self, n: i64) -> String {
        if n <= 1 {
            if !self.clip.is_empty() || self.clip_stack.is_empty() {
                return self.clip.clone();
            }
            return self.clip_stack[0].clone();
        }
        let idx = (n - 1) as usize;
        self.clip_stack.get(idx).cloned().unwrap_or_default()
    }
    fn sel(&self) -> String {
        self.sel.clone()
    }
    fn app(&self) -> String {
        self.app.clone()
    }
    fn title(&self) -> String {
        self.title.clone()
    }
    fn env(&self, name: &str) -> String {
        self.env.get(name).cloned().unwrap_or_default()
    }
    fn now(&self) -> DateTime<Local> {
        self.clock.unwrap_or_else(Local::now)
    }
    fn services(&self) -> Option<&Services> {
        self.services.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_ring() {
        let h = History::new(3);
        assert_eq!(h.get(1), "");
        h.push("a");
        h.push("b");
        h.push("c");
        assert_eq!(h.get(1), "c");
        assert_eq!(h.get(3), "a");
        assert_eq!(h.get(4), "");
        h.push("d"); // 覆盖 a
        assert_eq!(h.get(1), "d");
        assert_eq!(h.get(3), "b");
    }

    #[test]
    fn clip_current_and_history() {
        let mut ctx = MemoryContext::new();
        ctx.clip = "now".into();
        ctx.clip_stack = vec!["h1".into(), "h2".into()];
        assert_eq!(ctx.clip(0), "now");
        assert_eq!(ctx.clip(1), "now");
        assert_eq!(ctx.clip(2), "h2");
        assert_eq!(ctx.clip(9), "");
    }
}
