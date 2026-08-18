//! 工具栏可见性闸门（纯逻辑，不触 Win32，可单元测试）。
//!
//! 只回答一个问题：`UpdateToolbar` / `HideToolbar` 到达时，**何时**真正显示或隐藏。
//! 工具栏状态本身由调用方持有（见 `manager.rs` 的 `toolbar_pending_state`）。
//!
//! ## 两侧都要迟滞，且两侧的理由不同
//!
//! - **隐藏侧 50ms**：吸收应用间切换的 `FocusLost → FocusGained` 串（Alt+Tab）。
//! - **显示侧 120ms**：吸收宿主在 DocMgr 层的焦点 churn。
//!
//! 早期只有隐藏侧有迟滞，于是 show→hide 方向的抖动原样穿透：50ms 只是把消失推迟，
//! 呈现出来就是「闪一下」。实测 QQ 密码框每约 180ms 一轮「A(可编辑) 获焦 →
//! B(READONLY) 获焦」，服务端据此发出的 focus_gained/focus_lost 对间隔仅约 17ms，
//! 工具栏因此以约 5Hz 持续闪烁（2026-08-03）。DocMgr 级本就是噪声层（Excel 把同一
//! DocMgr 置空再设回、VSCode 一次切换伴随 5 次事件），不该让它的每次翻转直接驱动 UI。
//!
//! ⚠ **显示迟滞只作用于「不可见 → 可见」这个转变**。`UpdateToolbar` 同时承担「更新
//! 中英/标点/全半角」与「让工具栏出现」两件事，整条延迟会让已显示时按 Shift 切中英
//! 也慢一档，手感明显退化。故 `on_update` 要求调用方传入当前可见性。

use std::time::{Duration, Instant};

/// 隐藏迟滞：`HideToolbar` 后延后这么久才真正隐藏，期间收到 `UpdateToolbar` 即取消。
pub const HIDE_DEBOUNCE: Duration = Duration::from_millis(50);
/// 显示迟滞：工具栏从不可见变为可见前的静默期，期间收到 `HideToolbar` 即撤销。
///
/// 取值依据：需明显大于 DocMgr churn 的 gained→lost 间隔（实测约 17ms），又要小到
/// 用户察觉不到工具栏「晚出现」。工具栏是常驻 UI 而非跟随光标的候选窗，120ms 无感。
pub const SHOW_DEBOUNCE: Duration = Duration::from_millis(120);

/// `UpdateToolbar` 到达后调用方需执行的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    /// 立即渲染（工具栏已可见，内容更新不受迟滞影响）。
    RenderNow,
    /// 已排入显示迟滞窗口；调用方只需暂存状态，等 `tick_at` 返回 `Show`。
    Deferred,
}

/// `HideToolbar` 到达后调用方需执行的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HideAction {
    /// 撤销了待显示：工具栏本就不可见，无需隐藏；调用方须丢弃暂存状态。
    CancelledPending,
    /// 已排入隐藏迟滞窗口，等 `tick_at` 返回 `Hide`。
    Scheduled,
}

/// `tick_at` 推进后调用方需执行的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateTick {
    None,
    /// 显示迟滞到期：用暂存状态渲染（`Toolbar::update` 内部会 show）。
    Show,
    /// 隐藏迟滞到期：隐藏窗口。
    Hide,
}

/// 工具栏显隐迟滞闸门。
///
/// 不变式：`show_at` 与 `hide_at` **不会同时为 `Some`**——待显示意味着当前不可见
/// （无可隐藏之物），待隐藏意味着当前可见（`on_update` 会先清掉待隐藏）。
pub struct ToolbarGate {
    show_at: Option<Instant>,
    hide_at: Option<Instant>,
}

impl ToolbarGate {
    pub fn new() -> Self {
        Self {
            show_at: None,
            hide_at: None,
        }
    }

    /// `UpdateToolbar` 到达。`visible` = 工具栏当前是否可见。
    pub fn on_update(&mut self, now: Instant, visible: bool) -> UpdateAction {
        // 取消待定隐藏（切回本输入法 → 保持显示）。
        self.hide_at = None;
        if visible {
            // 已可见：内容立即生效，且不应有待显示项。
            self.show_at = None;
            return UpdateAction::RenderNow;
        }
        // 不可见 → 待显示。**从第一条 UpdateToolbar 起算**：若每条都重置计时，
        // 连发多条（启动、主题下发、状态同步）会把窗口一再推后，工具栏迟迟不出现。
        self.show_at.get_or_insert(now + SHOW_DEBOUNCE);
        UpdateAction::Deferred
    }

    /// `HideToolbar` 到达。
    pub fn on_hide(&mut self, now: Instant) -> HideAction {
        if self.show_at.take().is_some() {
            // 撤销待显示——消除 DocMgr churn 闪烁的关键一步。
            return HideAction::CancelledPending;
        }
        self.hide_at = Some(now + HIDE_DEBOUNCE);
        HideAction::Scheduled
    }

    /// 是否有待定的显示/隐藏。false 时调用方可跳过 `tick_at`（不取 `Instant::now()`）。
    pub fn is_active(&self) -> bool {
        self.show_at.is_some() || self.hide_at.is_some()
    }

    /// 下一次需要 [`Self::tick_at`] 的时刻；`None` = 闸门空闲，无需为它安排唤醒。
    ///
    /// 消息循环据此决定睡多久。两个迟滞窗口本来就只有几十到一百多毫秒，睡过头就是
    /// 「工具栏该显示时没显示」。
    pub fn deadline(&self) -> Option<Instant> {
        // 类型文档的不变式保证两者不同时为 `Some`。仍取较早者而非任选其一：这样即便将来
        // 不变式被放松，最坏结果也只是多醒一次，而不是漏掉一次迟滞到期。
        match (self.show_at, self.hide_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// 推进闸门。到期则清空对应计时并返回动作。
    pub fn tick_at(&mut self, now: Instant) -> GateTick {
        if let Some(d) = self.show_at
            && now >= d
        {
            self.show_at = None;
            return GateTick::Show;
        }
        if let Some(d) = self.hide_at
            && now >= d
        {
            self.hide_at = None;
            return GateTick::Hide;
        }
        GateTick::None
    }
}

impl Default for ToolbarGate {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn first_update_defers_show() {
        let mut g = ToolbarGate::new();
        let t0 = Instant::now();
        assert_eq!(g.on_update(t0, false), UpdateAction::Deferred);
        // 未到期不显示
        assert_eq!(g.tick_at(t0 + 119 * MS), GateTick::None);
        assert_eq!(g.tick_at(t0 + 120 * MS), GateTick::Show);
        assert!(!g.is_active());
    }

    #[test]
    fn update_when_visible_renders_immediately() {
        let mut g = ToolbarGate::new();
        let t0 = Instant::now();
        // 已可见：中英切换等内容更新不得被迟滞拖慢
        assert_eq!(g.on_update(t0, true), UpdateAction::RenderNow);
        assert!(!g.is_active());
    }

    /// 核心回归：QQ 密码框 DocMgr churn —— gained 后约 17ms 即 lost，工具栏不应出现。
    ///
    /// ⚠ **必须模拟窗口期内的 tick**：真实事件循环每约 8ms 推进一次（无命令时 sleep 8ms），
    /// 缺了这几次 tick，本用例在 `SHOW_DEBOUNCE = 0` 时**照样全绿**——锁住的就只是接口
    /// 形状而非「迟滞必须长于 churn 间隔」这个关键性质。
    #[test]
    fn docmgr_churn_never_shows_toolbar() {
        let mut g = ToolbarGate::new();
        let mut t = Instant::now();
        for _ in 0..10 {
            assert_eq!(g.on_update(t, false), UpdateAction::Deferred);
            // 事件循环在 gained→lost 的 17ms 间隔内会 tick 两次，此时不得放行显示
            for elapsed in [8, 16] {
                assert_eq!(
                    g.tick_at(t + elapsed * MS),
                    GateTick::None,
                    "churn 间隔内不得显示（SHOW_DEBOUNCE 必须显著大于 gained→lost 间隔）"
                );
            }
            // 17ms 后 focus_lost 抵达 → 撤销待显示，且不排隐藏（本就不可见）
            assert_eq!(g.on_hide(t + 17 * MS), HideAction::CancelledPending);
            assert!(!g.is_active(), "撤销后不应残留任何待定项");
            // 推进到远超两档迟滞：全程不得显示
            assert_eq!(g.tick_at(t + 300 * MS), GateTick::None);
            t += 180 * MS; // 下一轮 churn
        }
    }

    /// 对照组：正常点进输入框（无尾随 hide）→ 迟滞到期后正常显示。
    #[test]
    fn normal_focus_shows_after_debounce() {
        let mut g = ToolbarGate::new();
        let t0 = Instant::now();
        g.on_update(t0, false);
        assert_eq!(g.tick_at(t0 + 200 * MS), GateTick::Show);
    }

    /// 对照组：Alt+Tab 应用间切换（hide 紧跟 update）→ 隐藏被取消，保持显示。
    #[test]
    fn alt_tab_hide_then_update_stays_visible() {
        let mut g = ToolbarGate::new();
        let t0 = Instant::now();
        assert_eq!(g.on_hide(t0), HideAction::Scheduled);
        // 30ms 后新宿主 focus_gained 抵达（工具栏此刻仍可见）
        assert_eq!(g.on_update(t0 + 30 * MS, true), UpdateAction::RenderNow);
        assert!(!g.is_active(), "待定隐藏应已被取消");
        assert_eq!(g.tick_at(t0 + 500 * MS), GateTick::None);
    }

    /// 真正离开可编辑控件：隐藏照常在 50ms 后生效。
    #[test]
    fn hide_takes_effect_after_debounce() {
        let mut g = ToolbarGate::new();
        let t0 = Instant::now();
        assert_eq!(g.on_hide(t0), HideAction::Scheduled);
        assert_eq!(g.tick_at(t0 + 49 * MS), GateTick::None);
        assert_eq!(g.tick_at(t0 + 50 * MS), GateTick::Hide);
        assert!(!g.is_active());
    }

    /// 连发多条 UpdateToolbar 不得把显示窗口一再推后（否则工具栏迟迟不出现）。
    #[test]
    fn repeated_updates_do_not_postpone_show() {
        let mut g = ToolbarGate::new();
        let t0 = Instant::now();
        g.on_update(t0, false);
        g.on_update(t0 + 40 * MS, false);
        g.on_update(t0 + 80 * MS, false);
        // 仍以第一条为准：t0+120ms 到期
        assert_eq!(g.tick_at(t0 + 120 * MS), GateTick::Show);
    }

    /// 不变式：show_at 与 hide_at 不同时存在。
    #[test]
    fn show_and_hide_are_mutually_exclusive() {
        let mut g = ToolbarGate::new();
        let t0 = Instant::now();
        g.on_update(t0, false); // 待显示
        assert_eq!(g.on_hide(t0 + 10 * MS), HideAction::CancelledPending);
        assert!(!g.is_active());

        g.on_hide(t0 + 20 * MS); // 待隐藏
        assert_eq!(g.on_update(t0 + 30 * MS, true), UpdateAction::RenderNow);
        assert!(!g.is_active());
    }

    /// 待显示期间工具栏不可见 → 此时到达的 update 仍走 Deferred，不改期。
    #[test]
    fn update_while_pending_keeps_deadline_and_stays_deferred() {
        let mut g = ToolbarGate::new();
        let t0 = Instant::now();
        assert_eq!(g.on_update(t0, false), UpdateAction::Deferred);
        assert_eq!(g.on_update(t0 + 60 * MS, false), UpdateAction::Deferred);
        assert_eq!(g.tick_at(t0 + 119 * MS), GateTick::None);
        assert_eq!(g.tick_at(t0 + 120 * MS), GateTick::Show);
    }
}
