//! 工具栏自动隐藏状态机（纯逻辑，不触 Win32，可单元测试）。
//!
//! 生命周期：显示（on_shown 重置计时）→ 超时 → 1 秒线性淡出 → 隐藏。
//! 光标在工具栏内或拖动中顺延计时；淡出中光标移入取消淡出恢复不透明。
//! 未启用/无活动计时时 `is_active()` 为 false，调用方走快速路径（不取时间）。

use std::time::{Duration, Instant};

/// 淡出时长（固定 1 秒，不做配置）。
const FADE_DURATION: Duration = Duration::from_millis(1000);

/// 淡出期间的重绘间隔（约 60fps）。
///
/// 只在淡出这 1 秒内生效，是 [`AutoHide::next_deadline`] 唯一会要求高频唤醒的场景。
/// 取 60fps 而非消息循环从前的 ~125fps：alpha 只有 256 级，1 秒 60 帧的步进已在肉眼
/// 分辨力之下，再高只是白费唤醒。
const FADE_FRAME: Duration = Duration::from_millis(16);

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
    /// 上一次 `tick_at` 时光标是否占用工具栏（在栏内或拖动中）。
    ///
    /// 用于识别「刚离开」这一沿并据此重置计时。早先消息循环每 ~8ms 跑一次 tick，
    /// 占用期间**每轮**顺延 deadline，于是「离开时刻 + delay」是自然的结果；循环改为
    /// 事件驱动后 tick 只在唤醒时发生，最后一次顺延可能远早于真正的离开时刻，隐藏会
    /// 相应提前。显式记住这一沿，才能让隐藏时刻与轮询年代一致。
    was_engaged: bool,
}

impl AutoHide {
    pub fn new() -> Self {
        Self {
            enabled: false,
            delay: Duration::from_secs(5),
            deadline: None,
            fade_start: None,
            was_engaged: false,
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
            self.was_engaged = false;
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
        self.was_engaged = false;
    }

    /// 下一次需要 `tick_at` 的时刻；`None` = 无需为本状态机安排唤醒。
    ///
    /// 消息循环据此决定睡多久（与其它计时器取最早者）。**唯一会要求高频唤醒的是淡出**：
    /// 那 1 秒内每 [`FADE_FRAME`] 推进一级 alpha。其余情形要么返回那个 5 秒级的到期时刻，
    /// 要么返回 `None`。
    ///
    /// 光标占用工具栏期间照常返回到期时刻（而非 `None`）：到点醒来发现仍被占用就再顺延
    /// 一轮，每 delay 一次的唤醒可忽略。反过来若在此返回 `None`、只等 `WM_MOUSELEAVE`
    /// 来唤醒，工具栏就把「自动隐藏还能不能发生」全押在那条消息必达上——窗口在光标停留
    /// 期间被隐藏或重建都会让它不再到来，代价是自动隐藏永久失效。
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        if self.fade_start.is_some() {
            return Some(now + FADE_FRAME);
        }
        self.deadline
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
        let engaged = cursor_inside || dragging;
        // 「刚离开」这一沿：见 `was_engaged` 字段说明。必须在下面提前返回之前取，
        // 否则占用分支自己的 return 会把它吃掉。
        let just_left = self.was_engaged && !engaged;
        self.was_engaged = engaged;
        if engaged {
            // 悬停/拖动：顺延计时；淡出中则取消恢复。
            self.deadline = Some(now + self.delay);
            if self.fade_start.take().is_some() {
                return AutoHideAction::Restore;
            }
            return AutoHideAction::None;
        }
        if just_left {
            // 光标刚移出：计时从此刻起算。轮询年代由「占用期间每轮顺延」自然达成，
            // 事件驱动下必须显式做。此时 fade_start 必为 None（上一轮占用分支已清）。
            self.deadline = Some(now + self.delay);
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

    /// 光标移出后，计时应从**移出那一刻**起算满 delay。
    ///
    /// 这条锁的是消息循环从轮询改事件驱动时最容易丢的行为。轮询年代占用期间每 ~8ms
    /// 顺延一次，「离开时刻 + delay」是免费得来的；事件驱动下 tick 变稀疏，若不显式处理
    /// 移出沿，离开时读到的是**上一次唤醒时**设的 deadline——它多半已经过期，工具栏会在
    /// 光标刚移开的瞬间就开始淡出，而不是等满 5 秒。
    #[test]
    fn leaving_toolbar_restarts_timer_from_departure() {
        let (mut ah, t0) = armed();
        // 光标在栏内（稀疏 tick，模拟事件驱动：只在有消息/到期时才 tick）
        assert_eq!(ah.tick_at(t0 + SEC, true, false), AutoHideAction::None);
        // 6 秒后才轮到下一次 tick，此时光标已移出。上一次顺延设的 deadline 是 t0+6s，
        // 早已过期——没有移出沿处理的话这里会立刻 Fade(255)。
        assert_eq!(
            ah.tick_at(t0 + 7 * SEC, false, false),
            AutoHideAction::None,
            "光标刚移出就开始淡出：移出沿没有重置计时"
        );
        // 从移出时刻(t0+7s)起算 5 秒 = t0+12s 才该淡出；差一点点时仍不动。
        assert_eq!(
            ah.tick_at(t0 + 11 * SEC + Duration::from_millis(900), false, false),
            AutoHideAction::None,
            "未满 delay 就淡出"
        );
        assert_eq!(
            ah.tick_at(t0 + 12 * SEC, false, false),
            AutoHideAction::Fade(255),
            "满 delay 后未开始淡出"
        );
    }

    /// `next_deadline` 平时给出那个秒级到期时刻，供消息循环一觉睡到。
    #[test]
    fn next_deadline_reports_pending_timer() {
        let (ah, t0) = armed();
        assert_eq!(ah.next_deadline(t0), Some(t0 + 5 * SEC));

        // 未启用 / 已隐藏 → 无需唤醒。
        let mut idle = AutoHide::new();
        assert_eq!(idle.next_deadline(Instant::now()), None);
        idle.configure(true, 5000);
        idle.on_hidden();
        assert_eq!(idle.next_deadline(Instant::now()), None);
    }

    /// 淡出期间必须改要高频唤醒，否则 alpha 停在第一帧、工具栏「淡到一半卡住」。
    #[test]
    fn next_deadline_requests_frames_while_fading() {
        let (mut ah, t0) = armed();
        let fade_begin = t0 + 5 * SEC;
        assert_eq!(
            ah.tick_at(fade_begin, false, false),
            AutoHideAction::Fade(255)
        );
        assert_eq!(
            ah.next_deadline(fade_begin),
            Some(fade_begin + FADE_FRAME),
            "淡出中应按帧唤醒"
        );
        // 帧间隔必须远小于淡出总时长，否则淡出只有个位数帧。
        assert!(FADE_FRAME * 10 < FADE_DURATION);
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
        // 移出（t0+10s）→ 计时从这一刻起算，见 leaving_toolbar_restarts_timer_from_departure。
        //
        // 本行原先断言 t0+11s 就该淡出，即「deadline 由最后一次光标在内的 tick 决定」。
        // 那是 ~8ms 轮询的副产品：那时两次 tick 相差 8ms，「最后一次在内」与「移出时刻」
        // 无从区分，断言碰巧成立。循环改事件驱动后 tick 变稀疏，两者相差可达数秒，按旧
        // 断言就成了「光标在栏上停久一点，一移开工具栏立刻开始消失」——与「自动隐藏延迟
        // 5 秒」对用户的含义相反。故改按移出时刻起算。
        assert_eq!(
            ah.tick_at(t0 + 10 * SEC, false, false),
            AutoHideAction::None
        );
        assert_eq!(
            ah.tick_at(t0 + 14 * SEC, false, false),
            AutoHideAction::None,
            "距移出未满 delay 就淡出"
        );
        assert_eq!(
            ah.tick_at(t0 + 15 * SEC, false, false),
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
        assert_eq!(ah.tick_at(t0 + 6 * SEC, false, false), AutoHideAction::Hide);
        assert!(!ah.is_active());
    }

    #[test]
    fn cursor_cancels_fade_and_rearms() {
        let (mut ah, t0) = armed();
        assert_eq!(
            ah.tick_at(t0 + 5 * SEC, false, false),
            AutoHideAction::Fade(255)
        );
        // 淡出中移入 → Restore 且重新计时
        assert_eq!(
            ah.tick_at(t0 + 5 * SEC + Duration::from_millis(200), true, false),
            AutoHideAction::Restore
        );
        // 移出于 t0+9s，故淡出重新开始于 t0+14s（同上：按移出时刻起算，不是按最后一次
        // 「在内」的 tick 起算）。本测试的重点是上面那条 Restore，时刻只是佐证「重新计时了」。
        assert_eq!(ah.tick_at(t0 + 9 * SEC, false, false), AutoHideAction::None);
        assert_eq!(
            ah.tick_at(t0 + 13 * SEC, false, false),
            AutoHideAction::None
        );
        assert_eq!(
            ah.tick_at(t0 + 14 * SEC, false, false),
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
