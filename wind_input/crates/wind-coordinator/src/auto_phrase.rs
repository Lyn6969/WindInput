//! 码表自动造词：连续单字缓冲 + 终止信号 = 自动组词。
//!
//! # 为什么不复用 `committed_segs`
//!
//! `committed_segs` 是**拼音专属**的「组合区逐步转换」态（见 `AGENTS.md`：「码表选词消费整串、
//! **绝不进入此态**」）。其分段判据 `partial = consumed < total` 依赖 `consumed_length`，而码表
//! 候选恒为 0 → `partial` 永远 false → 每次选词后 `reset_pinyin_composition` 立即清空。
//! 旧 `learn_phrase_on_commit` 的入口守卫 `committed_segs.len() < 2` 因此**对码表恒真**，
//! 这是自动造词「完全不工作」的根因之一。码表必须有自己的缓冲。
//!
//! # 状态机
//!
//! ```text
//! 单字上屏  → 距上字超 idle_timeout 则先 flush 旧序列，再追加
//! 多字词上屏 → 视为终止符：flush 并清空（选了词组说明这不是散字序列）
//! 终止信号  → flush 并清空（标点/回车/空格/焦点/模式切换/光标移动）
//! ```
//!
//! 本模块**只做决策、不做 IO**：`flush` 类方法返回「待造词的字序列」，取码/查重/落库由
//! 协调器完成。这样打断语义可以全部单测覆盖。

use std::time::{Duration, Instant};

/// 造词最小字数的兜底值（配置为 0 时用）。
pub const DEFAULT_MIN_PHRASE_LEN: usize = 2;
/// 造词最大字数的兜底值（配置为 0 时用）。Go 版默认 5；旧 Rust 默认 10 —— 五笔场景下
/// 10 字序列几乎必是跨句杂词，统一到 5。
pub const DEFAULT_MAX_PHRASE_LEN: usize = 5;
/// 连续单字之间的最大间隔；超过则把已累积序列视作终止（防跨句拼出「加好加好」）。
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

/// 连续单字缓冲。
#[derive(Debug, Default)]
pub struct AutoPhraseBuf {
    chars: Vec<char>,
    /// 上一个单字进入缓冲的时刻；`None` = 缓冲为空。
    last_at: Option<Instant>,
}

impl AutoPhraseBuf {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn chars(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// 文本上屏。返回**需要造词的序列**（`None` = 无需造词）。
    ///
    /// - 单字：距上字超 `idle_timeout` 时先把旧序列吐出来造词，再以本字起新序列。
    /// - 多字词：视为终止符 —— 先吐出旧序列，本词自身**不入缓冲**
    ///   （它已经是词了，再参与组词只会造出更长的杂词）。
    ///
    /// 非汉字文本（标点/英文/数字）由调用方在进入前就该判为终止信号，不应走到这里；
    /// 但为稳妥，空文本按无操作处理。
    pub fn on_commit(
        &mut self,
        text: &str,
        now: Instant,
        idle_timeout: Duration,
    ) -> Option<Vec<char>> {
        let mut it = text.chars();
        let first = it.next()?;
        let is_single = it.next().is_none();

        if !is_single {
            // 多字词 = 终止符。
            return self.take();
        }
        // 单字：先判 idle。缓冲为空时 last_at 为 None，无需判。
        let stale = self
            .last_at
            .is_some_and(|t| now.saturating_duration_since(t) > idle_timeout);
        let flushed = if stale { self.take() } else { None };
        self.chars.push(first);
        self.last_at = Some(now);
        flushed
    }

    /// 终止信号（标点/回车/空格/焦点丢失/IME 停用/模式切换/光标移动）：吐出并清空。
    pub fn terminate(&mut self) -> Option<Vec<char>> {
        self.take()
    }

    /// 丢弃当前序列，**不造词**。用于「序列已不可信」的场合。
    pub fn discard(&mut self) {
        self.chars.clear();
        self.last_at = None;
    }

    fn take(&mut self) -> Option<Vec<char>> {
        self.last_at = None;
        if self.chars.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.chars))
    }
}

/// 对吐出的序列施加长度策略，得到待造词的字符串。
///
/// - 短于 `min` → 不造（正常情况：用户只打了一个字就敲了空格）。
/// - 长于 `max` → **不造**。刻意不学 Go 的「取末尾 max 个字」——在连续 8 个字中间切一刀，
///   切出来的多半不是词，属于杂词的主要来源之一。宁可放过，不可错造。
pub fn word_from_seq(seq: &[char], min: usize, max: usize) -> Option<String> {
    let min = if min == 0 { DEFAULT_MIN_PHRASE_LEN } else { min };
    let max = if max == 0 { DEFAULT_MAX_PHRASE_LEN } else { max };
    if seq.len() < min || seq.len() > max {
        return None;
    }
    Some(seq.iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    /// 连续单字累积，不中途造词。
    #[test]
    fn accumulates_single_chars() {
        let mut b = AutoPhraseBuf::new();
        let t = t0();
        assert!(b.on_commit("你", t, DEFAULT_IDLE_TIMEOUT).is_none());
        assert!(b.on_commit("好", t, DEFAULT_IDLE_TIMEOUT).is_none());
        assert_eq!(b.chars(), "你好");
    }

    /// 终止信号吐出序列并清空；再次终止不重复吐。
    #[test]
    fn terminate_flushes_once() {
        let mut b = AutoPhraseBuf::new();
        let t = t0();
        b.on_commit("你", t, DEFAULT_IDLE_TIMEOUT);
        b.on_commit("好", t, DEFAULT_IDLE_TIMEOUT);
        assert_eq!(b.terminate().unwrap(), vec!['你', '好']);
        assert!(b.terminate().is_none(), "已清空，不应重复吐出");
        assert!(b.is_empty());
    }

    /// 多字词上屏 = 终止符：吐出旧序列，且该词自身不入缓冲。
    #[test]
    fn multi_char_commit_terminates_and_does_not_join() {
        let mut b = AutoPhraseBuf::new();
        let t = t0();
        b.on_commit("中", t, DEFAULT_IDLE_TIMEOUT);
        b.on_commit("国", t, DEFAULT_IDLE_TIMEOUT);
        assert_eq!(b.on_commit("人民", t, DEFAULT_IDLE_TIMEOUT).unwrap(), vec!['中', '国']);
        assert!(b.is_empty(), "多字词自身不得进入缓冲");
    }

    /// idle 超时：先吐旧序列，再以新字起新序列——防跨句拼接。
    #[test]
    fn idle_timeout_flushes_then_starts_new() {
        let mut b = AutoPhraseBuf::new();
        let t = t0();
        b.on_commit("加", t, DEFAULT_IDLE_TIMEOUT);
        b.on_commit("好", t, DEFAULT_IDLE_TIMEOUT);
        let late = t + Duration::from_secs(6);
        let flushed = b.on_commit("加", late, DEFAULT_IDLE_TIMEOUT).unwrap();
        assert_eq!(flushed, vec!['加', '好'], "旧序列应先吐出");
        assert_eq!(b.chars(), "加", "新序列只含新字，不得拼成「加好加」");
    }

    /// idle 未超时不吐。边界：恰好等于超时值算未超（判据是 `>`）。
    #[test]
    fn idle_within_timeout_keeps_accumulating() {
        let mut b = AutoPhraseBuf::new();
        let t = t0();
        b.on_commit("你", t, DEFAULT_IDLE_TIMEOUT);
        let exact = t + DEFAULT_IDLE_TIMEOUT;
        assert!(b.on_commit("好", exact, DEFAULT_IDLE_TIMEOUT).is_none());
        assert_eq!(b.chars(), "你好");
    }

    /// 缓冲为空时 idle 判定不应误吐（last_at 为 None）。
    #[test]
    fn empty_buffer_never_flushes_on_idle() {
        let mut b = AutoPhraseBuf::new();
        let late = t0() + Duration::from_secs(999);
        assert!(b.on_commit("你", late, DEFAULT_IDLE_TIMEOUT).is_none());
    }

    /// discard 丢弃且不造词——序列不可信时用。
    #[test]
    fn discard_drops_without_flushing() {
        let mut b = AutoPhraseBuf::new();
        let t = t0();
        b.on_commit("你", t, DEFAULT_IDLE_TIMEOUT);
        b.on_commit("好", t, DEFAULT_IDLE_TIMEOUT);
        b.discard();
        assert!(b.is_empty());
        assert!(b.terminate().is_none());
    }

    /// 长度策略：短于 min 不造，长于 max **不造**（不切末尾 N 字）。
    #[test]
    fn word_from_seq_applies_length_policy() {
        let seq: Vec<char> = "一二三四五六".chars().collect();
        assert_eq!(word_from_seq(&seq[..1], 2, 5), None, "1 字应不造");
        assert_eq!(word_from_seq(&seq[..2], 2, 5).unwrap(), "一二");
        assert_eq!(word_from_seq(&seq[..5], 2, 5).unwrap(), "一二三四五");
        assert_eq!(
            word_from_seq(&seq[..6], 2, 5),
            None,
            "超长应整体放弃，不得切出「二三四五六」这类中间片段"
        );
    }

    /// 0 值回退到默认（min=2 / max=5）。
    #[test]
    fn word_from_seq_zero_falls_back_to_defaults() {
        let seq: Vec<char> = "一二三四五六".chars().collect();
        assert!(word_from_seq(&seq[..1], 0, 0).is_none());
        assert!(word_from_seq(&seq[..2], 0, 0).is_some());
        assert!(word_from_seq(&seq[..6], 0, 0).is_none());
    }

    /// 空文本无操作，不 panic、不清缓冲。
    #[test]
    fn empty_commit_is_noop() {
        let mut b = AutoPhraseBuf::new();
        let t = t0();
        b.on_commit("你", t, DEFAULT_IDLE_TIMEOUT);
        assert!(b.on_commit("", t, DEFAULT_IDLE_TIMEOUT).is_none());
        assert_eq!(b.chars(), "你", "空文本不应影响缓冲");
    }
}
