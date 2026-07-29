//! 自动配对标点跟踪器
//!
//! 与 Go 版本 `wind_input/internal/transform/pair_tracker.go` 对齐。
//!
//! 本栈是配对状态的**唯一真相源**（中文、英文全角、英文自定义标点、英文半角四条建立路径
//! 全部入此栈）。DLL 侧的 `_pairPendingDepth` 只是它的镜像计数，充当吃键闸门。
//!
//! 除了栈本身，还携带两项元数据，用来回答「这份状态还作不作数」：
//!
//! - `owner_token`：本栈是全局单栈、不分宿主。用户在 A 的括号里被弹框打断、切到 B 打字后，
//!   栈顶可能已是 B 压的那层，故聚焦时须校验归属。
//! - `last_activity`：失焦不再一律清栈之后，需要时效兜底——用户中途用鼠标点走、删掉括号
//!   这类操作输入法感知不到，没有时效的话陈旧状态会一直存活到吃掉用户的 Tab。

use std::time::Instant;

/// 配对条目
#[derive(Debug, Clone)]
pub struct PairEntry {
    pub left: char,
    pub right: char,
}

/// 配对跟踪器
pub struct PairTracker {
    stack: Vec<PairEntry>,
    /// 压入首层配对时的客户端 token（0 = 未归属）。
    owner_token: u64,
    /// 最后一次活动时间。`None` = 从未活动（此时栈必为空）。
    last_activity: Option<Instant>,
}

impl PairTracker {
    pub fn new() -> Self {
        Self {
            stack: Vec::new(),
            owner_token: 0,
            last_activity: None,
        }
    }

    /// 压入配对。首层压入时记录归属 token；每次压入都刷新活动时间。
    pub fn push(&mut self, left: char, right: char) {
        self.push_at(left, right, Instant::now());
    }

    /// [`Self::push`] 的可注入时钟版本（测试用）。
    pub fn push_at(&mut self, left: char, right: char, now: Instant) {
        self.stack.push(PairEntry { left, right });
        self.last_activity = Some(now);
    }

    /// 查看栈顶
    pub fn peek(&self) -> Option<&PairEntry> {
        self.stack.last()
    }

    /// 弹出栈顶
    pub fn pop(&mut self) -> Option<PairEntry> {
        self.stack.pop()
    }

    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }

    /// 清空（连同归属与活动时间一起复位，避免下次压栈继承旧归属）
    pub fn clear(&mut self) {
        self.stack.clear();
        self.owner_token = 0;
        self.last_activity = None;
    }

    /// 归属的客户端 token（0 = 未归属）
    pub fn owner_token(&self) -> u64 {
        self.owner_token
    }

    /// 认领归属。仅在栈由空变为非空时有意义，故由调用方在压首层前设置。
    pub fn set_owner_token(&mut self, token: u64) {
        self.owner_token = token;
    }

    /// 刷新活动时间（每次按键都应调用）。栈空时不记——没有状态需要保活。
    pub fn touch(&mut self) {
        self.touch_at(Instant::now());
    }

    /// [`Self::touch`] 的可注入时钟版本（测试用）。
    pub fn touch_at(&mut self, now: Instant) {
        if !self.stack.is_empty() {
            self.last_activity = Some(now);
        }
    }

    /// 状态是否已陈旧。`ttl_secs == 0` = 永不过期。
    ///
    /// 栈空或从未活动过时恒返回 `false`——那种情况下没有状态可谈，
    /// 判定交给调用方的「栈非空」前置条件。
    pub fn is_stale(&self, ttl_secs: u32) -> bool {
        self.is_stale_at(Instant::now(), ttl_secs)
    }

    /// [`Self::is_stale`] 的可注入时钟版本（测试用）。
    pub fn is_stale_at(&self, now: Instant, ttl_secs: u32) -> bool {
        if ttl_secs == 0 || self.stack.is_empty() {
            return false;
        }
        match self.last_activity {
            Some(t) => now.saturating_duration_since(t).as_secs() >= ttl_secs as u64,
            None => false,
        }
    }
}

impl Default for PairTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tracker_with_pair(now: Instant) -> PairTracker {
        let mut t = PairTracker::new();
        t.set_owner_token(7);
        t.push_at('（', '）', now);
        t
    }

    #[test]
    fn push_records_owner_and_activity() {
        let now = Instant::now();
        let t = tracker_with_pair(now);
        assert_eq!(t.owner_token(), 7);
        assert!(!t.is_empty());
        assert!(!t.is_stale_at(now, 120), "刚压栈不应陈旧");
    }

    #[test]
    fn stale_after_ttl_elapsed() {
        let now = Instant::now();
        let t = tracker_with_pair(now);
        assert!(
            !t.is_stale_at(now + Duration::from_secs(119), 120),
            "未到 TTL 不应陈旧"
        );
        assert!(
            t.is_stale_at(now + Duration::from_secs(120), 120),
            "到达 TTL 即陈旧"
        );
    }

    /// 时效从**最后一次活动**算起而非压栈时刻：在括号里持续输入不应误过期。
    #[test]
    fn touch_refreshes_ttl() {
        let now = Instant::now();
        let mut t = tracker_with_pair(now);
        t.touch_at(now + Duration::from_secs(119));
        assert!(
            !t.is_stale_at(now + Duration::from_secs(200), 120),
            "刷新后应从新时刻起算，不该陈旧"
        );
    }

    /// 栈空时 touch 不留痕：没有状态需要保活，否则空栈也会攒出一个活动时间。
    #[test]
    fn touch_on_empty_stack_is_noop() {
        let now = Instant::now();
        let mut t = PairTracker::new();
        t.touch_at(now);
        assert!(!t.is_stale_at(now + Duration::from_secs(999), 1));
    }

    #[test]
    fn ttl_zero_never_expires() {
        let now = Instant::now();
        let t = tracker_with_pair(now);
        assert!(!t.is_stale_at(now + Duration::from_secs(86400), 0));
    }

    #[test]
    fn clear_resets_owner_and_activity() {
        let now = Instant::now();
        let mut t = tracker_with_pair(now);
        t.clear();
        assert_eq!(t.owner_token(), 0, "清空须一并复位归属，避免下次压栈继承");
        assert!(t.is_empty());
        assert!(!t.is_stale_at(now + Duration::from_secs(999), 1));
    }
}
