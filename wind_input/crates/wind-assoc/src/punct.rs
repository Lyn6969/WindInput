//! 标点/符号联想：按上文**末尾字符的类别**给出候选。
//!
//! ## 为什么是规则表而不是模型
//!
//! 主流移动端输入法的标点联想是**把标点当普通 token 训进语言模型**的——「你好」后面
//! 出「，」，是因为语料里 `你好 ，` 的 bigram 频次高，而不是有人写了张表。那样更细腻：
//! 「吗→？」「谢谢→！」这类区分是规则表写不出来的。
//!
//! 但我们手上的模型都做不到。实测（2026-08-15）三份 `.gram` 全部**不含任何标点、
//! 数字、拉丁字符**，只有汉字：
//!
//! ```text
//! zh-hans-bgc / zh-hans-bgw / wanxiang-lts-zh-hans
//!   「，」「。」「？」「！」「、」「；」「：」  → traverse 全部 FAIL
//!   数字 0-9、字母 A-Z                        → traverse 全部 FAIL
//! ```
//!
//! 中文语料在分词前普遍剥掉标点，这三份都不例外。⇒ 标点只能另有来源。
//!
//! ⚠️ **将来若自建语料，预处理必须显式保留标点 token**——这与业界默认的清洗行为相反，
//! 不写下来一定会被下一个人默默"清洗"掉。
//!
//! ## 规则表的定位
//!
//! 它是**冷启动兜底**，不是最终形态。用户的个人学习（[`crate::AssocSource::History`]）
//! 优先级更高，会逐渐覆盖它——用户总在「好的」后面打句号，就该学会。

use crate::{AssocContext, AssocHit, AssocProvider, AssocSource};

/// 上文末尾字符的类别，决定推荐哪一组符号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailKind {
    /// 汉字结尾——最常见。
    Han,
    /// 数字结尾（含全角数字）：推荐单位而非标点。
    Digit,
    /// 已经是标点了：**不再联想**，否则会出现「。」后面接「，」这种明显错误。
    Punct,
    /// 拉丁字母、空白或其它：不联想（英文有自己的输入习惯，别插手）。
    Other,
}

/// 判断上文末尾属于哪一类。
///
/// 只看**最后一个字符**：联想是即时反应，多字符回溯的收益抵不上它带来的规则复杂度。
pub fn tail_kind(text: &str) -> TailKind {
    let Some(c) = text.chars().next_back() else {
        return TailKind::Other;
    };
    if is_han(c) {
        TailKind::Han
    } else if c.is_ascii_digit() || ('０'..='９').contains(&c) {
        TailKind::Digit
    } else if is_punct_char(c) {
        TailKind::Punct
    } else {
        TailKind::Other
    }
}

fn is_han(c: char) -> bool {
    // CJK 统一表意文字基本区。扩展区不纳入：那些字极少出现在日常输入的句尾，
    // 而放宽范围会把一堆符号误判成汉字。
    ('\u{4E00}'..='\u{9FFF}').contains(&c)
}

fn is_punct_char(c: char) -> bool {
    matches!(
        c,
        '，' | '。'
            | '？'
            | '！'
            | '、'
            | '；'
            | '：'
            | '…'
            | '—'
            | '“'
            | '”'
            | '‘'
            | '’'
            | '（'
            | '）'
            | '《'
            | '》'
            | ','
            | '.'
            | '?'
            | '!'
            | ';'
            | ':'
            | '"'
            | '\''
            | '('
            | ')'
    )
}

/// 汉字后的默认推荐序。
///
/// 顺序即优先级：逗号在日常文本里出现频次远高于其它，句号次之。
/// 问号叹号靠后——它们依赖语气，规则表判断不了，放前面会经常碍事。
const AFTER_HAN: &[&str] = &["，", "。", "、", "？", "！", "：", "；"];

/// 数字后的默认推荐序。
///
/// ★ 数字后推荐的是**单位**而非标点——「2026」后面用户想要的是「年」，不是「，」。
/// 这是移动端体感差异最明显的一处：触屏切到符号键盘打一个「年」很贵。
const AFTER_DIGIT: &[&str] = &["年", "月", "日", "元", "个", "点", "%", "℃"];

/// 静态规则表实现的标点联想源。
///
/// 无状态、无 IO；分数只用于**源内**排序，取「表内位置的倒序」，与其它源不可比。
#[derive(Debug, Default, Clone, Copy)]
pub struct PunctRules;

impl AssocProvider for PunctRules {
    fn suggest(&self, ctx: &AssocContext<'_>, limit: usize) -> Vec<AssocHit> {
        let list = match tail_kind(ctx.text) {
            TailKind::Han => AFTER_HAN,
            TailKind::Digit => AFTER_DIGIT,
            // 标点后再接标点、以及英文/空白结尾，一律不联想。
            TailKind::Punct | TailKind::Other => return Vec::new(),
        };
        list.iter()
            .take(limit)
            .enumerate()
            .map(|(i, s)| AssocHit {
                text: (*s).to_string(),
                // 标点不是上文的延伸，上屏的就是它自己。
                commit: None,
                source: AssocSource::Punct,
                // 表内越靠前分越高；只在源内比较。
                score: (list.len() - i) as i64,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(text: &str) -> AssocContext<'_> {
        AssocContext {
            text,
            boundary_broken: false,
        }
    }

    #[test]
    fn tail_kinds() {
        assert_eq!(tail_kind("你好"), TailKind::Han);
        assert_eq!(tail_kind("2026"), TailKind::Digit);
        assert_eq!(tail_kind("１２"), TailKind::Digit, "全角数字同样算数字");
        assert_eq!(tail_kind("你好，"), TailKind::Punct);
        assert_eq!(tail_kind("hello"), TailKind::Other);
        assert_eq!(tail_kind(""), TailKind::Other);
    }

    #[test]
    fn han_tail_suggests_punctuation() {
        let out = PunctRules.suggest(&ctx("你好"), 3);
        let texts: Vec<_> = out.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, ["，", "。", "、"], "逗号最优先");
    }

    /// ★ 数字后推荐单位而非标点——移动端体感差异最大的一处。
    #[test]
    fn digit_tail_suggests_units() {
        let out = PunctRules.suggest(&ctx("2026"), 3);
        let texts: Vec<_> = out.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, ["年", "月", "日"]);
    }

    /// 标点后不再联想，否则会出现「。」后面接「，」。
    #[test]
    fn punct_tail_yields_nothing() {
        assert!(PunctRules.suggest(&ctx("你好。"), 5).is_empty());
        assert!(PunctRules.suggest(&ctx("你好,"), 5).is_empty(), "半角同样");
    }

    /// 英文结尾不插手——英文有自己的输入习惯。
    #[test]
    fn latin_tail_yields_nothing() {
        assert!(PunctRules.suggest(&ctx("hello"), 5).is_empty());
        assert!(
            PunctRules.suggest(&ctx("你好 "), 5).is_empty(),
            "空白结尾同样"
        );
    }

    #[test]
    fn respects_limit() {
        assert_eq!(PunctRules.suggest(&ctx("你好"), 2).len(), 2);
        assert_eq!(PunctRules.suggest(&ctx("你好"), 0).len(), 0);
        assert_eq!(
            PunctRules.suggest(&ctx("你好"), 99).len(),
            AFTER_HAN.len(),
            "limit 超过表长时给全部，不越界"
        );
    }

    #[test]
    fn score_is_descending_in_table_order() {
        let out = PunctRules.suggest(&ctx("你好"), 4);
        for w in out.windows(2) {
            assert!(w[0].score > w[1].score, "源内分数须严格降序");
        }
    }
}
