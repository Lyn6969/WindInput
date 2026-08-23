//! 英文候选的**头部候选**（输入原文 + 大小写变形）——临时英文与英文方案共用。
//!
//! 设计见 `docs/design/schema-scoped-behavior.md` §5。
//!
//! # 为什么是共用函数
//!
//! 两条路径（`handle_temp.rs` 的临英、`handle_candidate.rs` 的主输入路）的**配置各自独立**
//! （四个键，两侧各一对，默认值还刻意相反），但**产出必须逐字节相同**——否则同一串输入在
//! 两个入口给出的候选不一样，而用户根本不知道自己此刻在哪条路径上。
//!
//! 配置分开、实现共用：分歧只允许出现在「要不要生成」，不允许出现在「生成什么」。
//! 两份实现分叉只是时间问题——`phrase_owns_code` 的注释里已记过一次同型教训。
//!
//! # 为什么头部候选**不带** `source` / `code`
//!
//! 它们没有词库来源。写端 `record_selection_in` 的守卫是 `cand.source != English { return }`，
//! 据此把它们排除在词频之外；否则会写出「读端按候选码永远查不中」的孤儿键
//! （与「短语有文本无码位恒不记词频」同一先例）。
//!
//! # 为什么调用方必须把它们钉在词库段**之前**
//!
//! 「首候选恒是所打原文」是这条能力的全部意义：打词库里没有的词时，原文是唯一能上屏的
//! 东西。词频重排若作用到整个列表，用户按空格就会上屏一个他没打的词。故两处调用方都
//! 只对**词库段**跑 `apply_freq_rerank_in` / `apply_shadow_in`。

use crate::key_convert::en_case_variants;
use wind_candidate::Candidate;

/// 生成头部候选：`[原文] + [大小写变形…]`，内部已去重（变形与原文相同者不产出）。
///
/// `with_raw` / `with_variants` 由调用方按**自己那一侧**的配置传入
/// （临英 `input.temp_english.*`，英文方案 `schema.english.*`）。
///
/// 两者都为 `false`，或 `raw` 为空时返回空表——调用方据此走「无头部候选」的既有路径。
/// ⚠️ 两者都关且词库无命中时最终候选会是空的，那不是缺陷，但上屏出口必须能接住
/// （见 §5.5：临英空格臂判的是「实际候选是否为空」，不是本配置）。
pub(crate) fn english_head_candidates(
    raw: &str,
    with_raw: bool,
    with_variants: bool,
) -> Vec<Candidate> {
    if raw.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Candidate> = Vec::new();
    if with_raw {
        out.push(Candidate {
            text: raw.to_string(),
            ..Default::default()
        });
    }
    if with_variants {
        for v in en_case_variants(raw) {
            // `en_case_variants` 已剔除与原文相同者；这里再挡一次是为了 `with_raw = false`
            // 时不重复——那种配置下原文不入列，但变形里可能恰好有一条等于原文。
            if v == raw && with_raw {
                continue;
            }
            if out.iter().any(|c| c.text == v) {
                continue;
            }
            out.push(Candidate {
                text: v,
                ..Default::default()
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(v: &[Candidate]) -> Vec<&str> {
        v.iter().map(|c| c.text.as_str()).collect()
    }

    /// 四种开关组合的产出，逐格钉住。
    #[test]
    fn switch_matrix() {
        assert_eq!(
            texts(&english_head_candidates("Hel", true, true)),
            vec!["Hel", "hel", "HEL"],
            "原文在最前，变形跟随（首字母大写形态等于原文，已被剔除）"
        );
        assert_eq!(
            texts(&english_head_candidates("Hel", true, false)),
            vec!["Hel"]
        );
        assert_eq!(
            texts(&english_head_candidates("Hel", false, true)),
            vec!["hel", "HEL"],
            "不要原文时只剩变形"
        );
        assert!(
            english_head_candidates("Hel", false, false).is_empty(),
            "两者皆关 = 无头部候选，调用方走既有路径"
        );
    }

    /// 空输入恒空——别让它产出一条空文本候选（那会在候选窗里显示成一个空行）。
    #[test]
    fn empty_input_yields_nothing() {
        assert!(english_head_candidates("", true, true).is_empty());
    }

    /// 头部候选**不带** `source` / `code`：写端据此把它们排除在词频之外。
    ///
    /// 这条不是形式检查——带上 source 的后果是往词频表里写一批读端永远查不中的孤儿键，
    /// 且完全静默。
    #[test]
    fn head_candidates_carry_no_source_or_code() {
        for c in english_head_candidates("Hel", true, true) {
            assert_eq!(c.source, wind_candidate::CandidateSource::default());
            assert!(c.code.is_empty(), "头部候选不得带码：{}", c.text);
        }
    }

    /// 无字母的输入三形态相同，变形为空——只剩原文。
    #[test]
    fn non_alpha_input_has_no_variants() {
        assert_eq!(
            texts(&english_head_candidates("123", true, true)),
            vec!["123"]
        );
    }
}
