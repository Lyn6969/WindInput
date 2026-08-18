//! 通用防抖器（trailing debounce）
//!
//! 用于鼠标悬停高亮 / tooltip / 状态提示等：短时间内反复触发只在"稳定"后生效一次，
//! 避免抖动（如打字换候选时静止鼠标下方候选变化引起的高亮/提示闪烁）。
//!
//! UI 线程的消息循环在 [`Debouncer::deadline`] 到期时醒来并调用 `poll()`。分辨率 =
//! 唤醒精度（毫秒级）。早先循环是固定 ~8ms 轮询，`poll` 不必自报到期时刻；改为事件驱动
//! 后，不上报就等于永远不被唤醒。

use std::time::{Duration, Instant};

/// 尾沿防抖：每次 `trigger` 重置截止时间，`poll` 在静止 `delay` 后吐出最后一次的值。
pub struct Debouncer<T> {
    pending: Option<T>,
    fire_at: Option<Instant>,
    delay: Duration,
}

impl<T: Clone> Debouncer<T> {
    pub fn new(delay_ms: u64) -> Self {
        Self {
            pending: None,
            fire_at: None,
            delay: Duration::from_millis(delay_ms),
        }
    }

    /// 触发：记录待定值并把截止时间重置为 now + delay。
    pub fn trigger(&mut self, value: T) {
        self.pending = Some(value);
        self.fire_at = Some(Instant::now() + self.delay);
    }

    /// 取消待定（如窗口隐藏 / 重新开始）。
    pub fn cancel(&mut self) {
        self.pending = None;
        self.fire_at = None;
    }

    /// 轮询：若已到期则取出并清空待定值返回 Some，否则 None。
    pub fn poll(&mut self) -> Option<T> {
        if let Some(at) = self.fire_at
            && Instant::now() >= at
        {
            self.fire_at = None;
            return self.pending.take();
        }
        None
    }

    /// 下一次 [`Self::poll`] 可能吐出值的时刻；`None` = 无待定，无需为此安排唤醒。
    ///
    /// 消息循环据此决定睡多久。没有它，`poll` 就只能靠「反正每轮都会被调一次」生效——
    /// 那正是本类型原先隐含依赖、而事件驱动下不再成立的前提。
    pub fn deadline(&self) -> Option<Instant> {
        self.fire_at
    }
}
