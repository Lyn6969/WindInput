//! 工具栏自动隐藏状态机（纯逻辑，不触 Win32，可单元测试）。
//!
//! 生命周期：显示（on_shown 重置计时）→ 超时 → 1 秒线性淡出 → 隐藏。
//! 光标在工具栏内或拖动中顺延计时；淡出中光标移入取消淡出恢复不透明。
//! 未启用/无活动计时时 `is_active()` 为 false，调用方走快速路径（不取时间）。

use std::time::{Duration, Instant};

/// 淡出时长（固定 1 秒，不做配置）。
const FADE_DURATION: Duration = Duration::from_millis(1000);

/// tick 推进后调用方需执行的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoHideAction {
    /// 无事可做。
    None,
    /// 淡出中：以该 alpha 重提交窗口（255→0 线性）。
    Fade(u8),
    /// 淡出被取消（光标移入）：恢复不透明（alpha=255），计时已重置。
    Restore,
    /// 淡出完成：隐藏窗口。
    Hide,
}

pub struct AutoHide {
    enabled: bool,
    delay: Duration,
    /// 到期时刻；None = 未启用或已隐藏。
    deadline: Option<Instant>,
    /// 淡出起点；Some = 淡出动画进行中。
    fade_start: Option<Instant>,
}

impl AutoHide {
    pub fn new() -> Self {
        Self {
            enabled: false,
            delay: Duration::from_secs(5),
            deadline: None,
            fade_start: None,
        }
    }

    /// 配置变更（SetToolbarAutoHide）。返回 true = 淡出被中断，调用方需恢复不透明。
    /// delay_ms 下限 1000（防误设 0 即隐；协调器侧另有秒级钳制，双保险）。
    pub fn configure(&mut self, enabled: bool, delay_ms: u64) -> bool {
        self.enabled = enabled;
        self.delay = Duration::from_millis(delay_ms.max(1000));
        if !enabled {
            let was_fading = self.fade_start.is_some();
            self.deadline = None;
            self.fade_start = None;
            return was_fading;
        }
        false
    }

    /// 每次显示/重绘（render 单点）后调用：重置计时。未启用时 no-op（保持快速路径）。
    pub fn on_shown(&mut self, now: Instant) {
        if self.enabled {
            self.deadline = Some(now + self.delay);
            self.fade_start = None;
        }
    }

    /// 隐藏（含失活防抖与自动隐藏自身）时调用：清空计时与淡出。
    pub fn on_hidden(&mut self) {
        self.deadline = None;
        self.fade_start = None;
    }

    /// 是否有活动计时/淡出。false 时调用方跳过 tick 推进（不取 Instant::now()）。
    pub fn is_active(&self) -> bool {
        self.deadline.is_some() || self.fade_start.is_some()
    }

    /// 推进状态机。cursor_inside=光标在工具栏窗口内；dragging=拖动中。
    pub fn tick_at(&mut self, now: Instant, cursor_inside: bool, dragging: bool) -> AutoHideAction {
        if !self.is_active() {
            return AutoHideAction::None;
        }
        if cursor_inside || dragging {
            // 悬停/拖动：顺延计时；淡出中则取消恢复。
            self.deadline = Some(now + self.delay);
            if self.fade_start.take().is_some() {
                return AutoHideAction::Restore;
            }
            return AutoHideAction::None;
        }
        if let Some(start) = self.fade_start {
            let elapsed = now.duration_since(start);
            if elapsed >= FADE_DURATION {
                self.deadline = None;
                self.fade_start = None;
                return AutoHideAction::Hide;
            }
            let t = elapsed.as_secs_f32() / FADE_DURATION.as_secs_f32();
            return AutoHideAction::Fade((255.0 * (1.0 - t)) as u8);
        }
        match self.deadline {
            Some(d) if now >= d => {
                self.fade_start = Some(now);
                AutoHideAction::Fade(255)
            }
            _ => AutoHideAction::None,
        }
    }
}

impl Default for AutoHide {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: Duration = Duration::from_secs(1);

    /// 启用并显示于 t0，返回 (状态机, t0)。
    fn armed() -> (AutoHide, Instant) {
        let mut ah = AutoHide::new();
        ah.configure(true, 5000);
        let t0 = Instant::now();
        ah.on_shown(t0);
        (ah, t0)
    }

    #[test]
    fn disabled_by_default_stays_inactive() {
        let mut ah = AutoHide::new();
        ah.on_shown(Instant::now()); // 未启用：不设 deadline
        assert!(!ah.is_active());
    }

    #[test]
    fn shown_arms_deadline_when_enabled() {
        let (ah, _) = armed();
        assert!(ah.is_active());
    }

    #[test]
    fn no_action_before_deadline() {
        let (mut ah, t0) = armed();
        assert_eq!(ah.tick_at(t0 + 4 * SEC, false, false), AutoHideAction::None);
        assert!(ah.is_active());
    }

    #[test]
    fn cursor_inside_extends_deadline() {
        let (mut ah, t0) = armed();
        // 已过原 deadline，但光标在内 → 顺延而非淡出
        assert_eq!(ah.tick_at(t0 + 6 * SEC, true, false), AutoHideAction::None);
        // 顺延后 4 秒（未到新 deadline t0+11s）仍不淡出
        assert_eq!(
            ah.tick_at(t0 + 10 * SEC, false, false),
            AutoHideAction::None
        );
        // 新 deadline 到期 → 进入淡出
        assert_eq!(
            ah.tick_at(t0 + 11 * SEC, false, false),
            AutoHideAction::Fade(255)
        );
    }

    #[test]
    fn dragging_extends_deadline() {
        let (mut ah, t0) = armed();
        assert_eq!(ah.tick_at(t0 + 6 * SEC, false, true), AutoHideAction::None);
        assert!(ah.is_active());
    }

    #[test]
    fn fades_linearly_then_hides() {
        let (mut ah, t0) = armed();
        // t0+5s 到期 → 淡出起点，首帧全亮
        assert_eq!(
            ah.tick_at(t0 + 5 * SEC, false, false),
            AutoHideAction::Fade(255)
        );
        // 半程 ≈ 127
        match ah.tick_at(t0 + 5 * SEC + Duration::from_millis(500), false, false) {
            AutoHideAction::Fade(a) => assert!((120..=135).contains(&a), "alpha={a}"),
            other => panic!("expected Fade, got {other:?}"),
        }
        // 1 秒后 → 隐藏并清空
        assert_eq!(
            ah.tick_at(t0 + 6 * SEC, false, false),
            AutoHideAction::Hide
        );
        assert!(!ah.is_active());
    }

    #[test]
    fn cursor_cancels_fade_and_rearms() {
        let (mut ah, t0) = armed();
        assert_eq!(
            ah.tick_at(t0 + 5 * SEC, false, false),
            AutoHideAction::Fade(255)
        );
        // 淡出中移入 → Restore 且重新计时（新 deadline = t0+5.2+5 秒）
        assert_eq!(
            ah.tick_at(t0 + 5 * SEC + Duration::from_millis(200), true, false),
            AutoHideAction::Restore
        );
        assert_eq!(ah.tick_at(t0 + 9 * SEC, false, false), AutoHideAction::None);
        assert_eq!(
            ah.tick_at(t0 + 11 * SEC, false, false),
            AutoHideAction::Fade(255)
        );
    }

    #[test]
    fn hidden_clears_state() {
        let (mut ah, t0) = armed();
        ah.on_hidden();
        assert!(!ah.is_active());
        assert_eq!(ah.tick_at(t0 + 9 * SEC, false, false), AutoHideAction::None);
    }

    #[test]
    fn disable_mid_fade_requests_restore() {
        let (mut ah, t0) = armed();
        assert_eq!(
            ah.tick_at(t0 + 5 * SEC, false, false),
            AutoHideAction::Fade(255)
        );
        assert!(ah.configure(false, 5000)); // 淡出中关闭 → 需恢复不透明
        assert!(!ah.is_active());
    }

    #[test]
    fn delay_ms_clamped_to_min_1s() {
        let mut ah = AutoHide::new();
        ah.configure(true, 0);
        let t0 = Instant::now();
        ah.on_shown(t0);
        // 0ms 被钳制为 1s：t0+0.5s 不应到期
        assert_eq!(
            ah.tick_at(t0 + Duration::from_millis(500), false, false),
            AutoHideAction::None
        );
    }

    #[test]
    fn reconfigure_enabled_mid_fade_keeps_fading_until_shown() {
        let (mut ah, t0) = armed();
        assert_eq!(
            ah.tick_at(t0 + 5 * SEC, false, false),
            AutoHideAction::Fade(255)
        );
        // 淡出中以 enabled=true 改配置：configure 不打断淡出（返回 false）
        assert!(!ah.configure(true, 8000));
        assert!(ah.is_active());
        // 调用方（set_auto_hide）随后 on_shown：取消淡出、按新 delay 重新计时
        ah.on_shown(t0 + 5 * SEC + Duration::from_millis(300));
        assert_eq!(
            ah.tick_at(t0 + 12 * SEC, false, false),
            AutoHideAction::None
        );
        // 新 deadline = t0+5.3s+8s = t0+13.3s
        assert_eq!(
            ah.tick_at(t0 + 14 * SEC, false, false),
            AutoHideAction::Fade(255)
        );
    }
}
