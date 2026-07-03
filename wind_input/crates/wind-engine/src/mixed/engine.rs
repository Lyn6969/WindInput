//! 混合引擎实现（码表主 + 拼音次，分层加权合并）
//!
//! 与 Go 版本 `wind_input/internal/engine/mixed/mixed.go` 对齐（核心分层）。
//!
//! 加权策略（双向夹击）：
//! - 码表：精确匹配(code==input) +CodetableWeightBoost(默认 1e7)；短语 +1M；前缀补全 +500K
//! - 拼音：weight ÷ PinyinTierScale(100) 归一化到低档（0~100K），与码表/短语严格隔离
//! - 合并后按权重排序、按文本去重；输入短于 min_pinyin_length 时仅码表
//!
//! 后置：英文候选、简拼长度惩罚（HasFullSyllable）、convertMixedOverflow 精细档。

use crate::engine::{ConvertResult, Engine, EngineType};
use wind_candidate::{Candidate, CandidateSource};

/// 短语候选提权（高于拼音、低于码表词）
const PHRASE_WEIGHT_BOOST: i32 = 1_000_000;
/// 码表前缀补全（拆分组合）提权
const PARTIAL_MATCH_BOOST: i32 = 500_000;
/// 拼音候选归一化系数（÷ 后落入低档）
const PINYIN_TIER_SCALE: i32 = 100;

/// 混合引擎
pub struct MixedEngine {
    /// 主引擎（码表，如五笔）
    primary: Box<dyn Engine>,
    /// 次引擎（拼音）
    secondary: Option<Box<dyn Engine>>,
    /// 拼音生效的最小输入长度
    min_pinyin_length: usize,
    /// 码表精确匹配提权
    codetable_weight_boost: i32,
    /// 全码自动上屏时，若存在拼音候选则否决（保护拼音用户，对齐 Go AutoCommitBlockOnPinyin）
    auto_commit_block_on_pinyin: bool,
    /// 输入超过码表最大码长时仅查拼音（主流混输行为，对齐 Go PinyinOnlyOverflow）。
    /// false 时走「码表前 N 码 + 拼音完整输入」混合 overflow。
    pinyin_only_overflow: bool,
    /// 顶码歧义裁决（对齐 Go TopCodeOverridePinyin）：前缀既是完整拼音又是唯一五笔全码时，
    /// true 放行顶码倒向五笔，false（默认）维持拼音保护。
    top_code_override_pinyin: bool,
    /// 主码表最大码长（构建期由 primary.max_code_length() 注入；0 表示未知/不启用溢出分支）。
    max_code_len: usize,
    /// 候选来源标记（对齐 Go addSourceHints）：true 时给拼音候选 comment 加「拼」前缀，
    /// 帮助用户区分混输候选来源。默认 false（零回归）。
    show_source_hint: bool,
}

impl MixedEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        primary: Box<dyn Engine>,
        secondary: Option<Box<dyn Engine>>,
        min_pinyin_length: usize,
        codetable_weight_boost: i32,
        auto_commit_block_on_pinyin: bool,
        pinyin_only_overflow: bool,
        top_code_override_pinyin: bool,
        show_source_hint: bool,
    ) -> Self {
        let max_code_len = primary.max_code_length();
        Self {
            primary,
            secondary,
            min_pinyin_length,
            codetable_weight_boost,
            auto_commit_block_on_pinyin,
            pinyin_only_overflow,
            top_code_override_pinyin,
            max_code_len,
            show_source_hint,
        }
    }

    /// 码表候选按混输策略提权（短语独立档 +1M / 精确 +boost / 前缀补全 +500K）。
    /// `exact_input` 为「视作精确全码」的判据串（正常路径=input，overflow 混合路径=前 N 码前缀）。
    fn boost_codetable(&self, candidates: &mut [Candidate], exact_input: &str) {
        for c in candidates.iter_mut() {
            if c.is_phrase {
                c.weight = c.weight.saturating_add(PHRASE_WEIGHT_BOOST);
            } else if c.code == exact_input {
                c.weight = c.weight.saturating_add(self.codetable_weight_boost);
            } else {
                c.weight = c.weight.saturating_add(PARTIAL_MATCH_BOOST);
            }
        }
    }

    /// 拼音候选归一化降档（÷ PINYIN_TIER_SCALE，与码表/短语档严格隔离）。
    fn normalize_pinyin(candidates: &mut [Candidate]) {
        for c in candidates.iter_mut() {
            c.weight /= PINYIN_TIER_SCALE;
            if c.weight < 0 {
                c.weight = 0;
            }
        }
    }

    /// 合并（码表在前、拼音在后）→ 按权重稳定排序 → 按文本去重 → 截断。
    fn merge_sort_dedup(
        mut codetable: Vec<Candidate>,
        pinyin: Vec<Candidate>,
        max_candidates: usize,
    ) -> Vec<Candidate> {
        codetable.extend(pinyin);
        codetable.sort_by(|a, b| {
            b.weight
                .cmp(&a.weight)
                .then(a.natural_order.cmp(&b.natural_order))
        });
        let mut seen = std::collections::HashSet::new();
        codetable.retain(|c| seen.insert(c.text.clone()));
        codetable.truncate(max_candidates);
        codetable
    }

    /// 拼音音节拆分显示（≥2 完成音节且有 preedit 时采用，供组合区分隔显示）。
    fn pinyin_preedit_of(py: &ConvertResult) -> Option<String> {
        if py.completed_syllables.len() >= 2 && !py.preedit_display.is_empty() {
            Some(py.preedit_display.clone())
        } else {
            None
        }
    }

    /// 来源标记（对齐 Go addSourceHints）：给拼音候选 comment 加「拼」前缀，助用户区分混输来源。
    fn add_source_hints(candidates: &mut [Candidate]) {
        for c in candidates.iter_mut() {
            if c.source == CandidateSource::Pinyin {
                if c.comment.is_empty() {
                    c.comment = "拼".to_string();
                } else {
                    c.comment = format!("拼|{}", c.comment);
                }
            }
        }
    }

    /// 超长输入（input_len > max_code_len）分支：按 pinyin_only_overflow 分流。
    /// - true（默认）：仅查拼音；长码特例下（完整 input 有精确/更长后继）追加码表候选。
    /// - false：码表取前 N 码（+ 长码特例追加完整 input）+ 拼音完整输入，混合竞争。
    fn convert_overflow(&self, input: &str, max_candidates: usize) -> ConvertResult {
        let Some(sec) = &self.secondary else {
            // 无拼音子引擎：退化为码表查完整输入（保持有候选）。
            return self
                .primary
                .convert(input, max_candidates)
                .unwrap_or_default();
        };
        let has_full_or_longer =
            self.primary.has_full_input_match(input) || self.primary.has_longer_code(input);

        if self.pinyin_only_overflow {
            let py = sec.convert(input, max_candidates).unwrap_or_default();
            let pinyin_preedit = Self::pinyin_preedit_of(&py);
            let mut pinyin = py.candidates;
            // 长码特例：完整 input 在码表有精确/更长后继 → 追加码表候选，拼音归一化降档避免档位重叠。
            let mut merged = if has_full_or_longer {
                Self::normalize_pinyin(&mut pinyin);
                let mut ct = self
                    .primary
                    .convert(input, max_candidates)
                    .unwrap_or_default()
                    .candidates;
                self.boost_codetable(&mut ct, input);
                Self::merge_sort_dedup(ct, pinyin, max_candidates)
            } else {
                pinyin
            };
            if self.show_source_hint {
                Self::add_source_hints(&mut merged);
            }
            let is_empty = merged.is_empty();
            ConvertResult {
                candidates: merged,
                preedit_pinyin: pinyin_preedit.clone().unwrap_or_default(),
                preedit_display: pinyin_preedit.unwrap_or_else(|| input.to_string()),
                is_empty,
                ..Default::default()
            }
        } else {
            // 混合 overflow：码表前 N 码 + 拼音完整输入。
            let prefix: String = input.chars().take(self.max_code_len).collect();
            let mut codetable = self
                .primary
                .convert(&prefix, max_candidates)
                .unwrap_or_default()
                .candidates;
            if has_full_or_longer {
                let full = self
                    .primary
                    .convert(input, max_candidates)
                    .unwrap_or_default();
                codetable.extend(full.candidates);
            }
            self.boost_codetable(&mut codetable, &prefix);
            let py = sec.convert(input, max_candidates).unwrap_or_default();
            let pinyin_preedit = Self::pinyin_preedit_of(&py);
            let mut pinyin = py.candidates;
            Self::normalize_pinyin(&mut pinyin);
            let mut merged = Self::merge_sort_dedup(codetable, pinyin, max_candidates);
            if self.show_source_hint {
                Self::add_source_hints(&mut merged);
            }
            let is_empty = merged.is_empty();
            ConvertResult {
                candidates: merged,
                preedit_pinyin: pinyin_preedit.clone().unwrap_or_default(),
                preedit_display: pinyin_preedit.unwrap_or_else(|| input.to_string()),
                is_empty,
                ..Default::default()
            }
        }
    }
}

impl Engine for MixedEngine {
    /// 热插拔扩展词库：转发到主/次子引擎（码表子引擎承载 codetable-extra 层）。
    fn set_dict_enabled(&self, dict_id: &str, enabled: bool) -> bool {
        let a = self.primary.set_dict_enabled(dict_id, enabled);
        let b = self
            .secondary
            .as_ref()
            .is_some_and(|s| s.set_dict_enabled(dict_id, enabled));
        a || b
    }

    fn convert(&self, input: &str, max_candidates: usize) -> anyhow::Result<ConvertResult> {
        if input.is_empty() {
            return Ok(ConvertResult::default());
        }
        let input_len = input.chars().count();

        // 超长分支（对齐 Go ConvertEx）：输入超过码表最大码长时，按 pinyin_only_overflow 分流，
        // 不再走下方「码表+拼音等长合并」路径。
        if self.max_code_len > 0 && input_len > self.max_code_len {
            return Ok(self.convert_overflow(input, max_candidates));
        }

        // 1. 码表候选 + 加权
        let ct = self.primary.convert(input, max_candidates)?;
        // 主码表的全码自动上屏意向（下方按拼音守护 + 合并存活性复核后才放行）。
        let ct_should_commit = ct.should_commit;
        let ct_commit_text = ct.commit_text.clone();
        let ct_should_clear = ct.should_clear;
        let mut codetable: Vec<Candidate> = ct.candidates;
        for c in &mut codetable {
            if c.is_phrase {
                c.weight = c.weight.saturating_add(PHRASE_WEIGHT_BOOST);
            } else if c.code == input {
                c.weight = c.weight.saturating_add(self.codetable_weight_boost);
            } else {
                c.weight = c.weight.saturating_add(PARTIAL_MATCH_BOOST);
            }
        }

        // 2. 拼音候选（输入达到最小长度）+ 归一化降档
        let mut pinyin: Vec<Candidate> = Vec::new();
        // 多音节拼音的组合区分隔显示（如 "ni hao"）：仅当拼音解析出 ≥2 完成音节时采用，
        // 否则保持原始码（单音节如 "cang" 无需分隔，纯五笔码更不应被拆）。
        let mut pinyin_preedit: Option<String> = None;
        if input_len >= self.min_pinyin_length {
            if let Some(sec) = &self.secondary {
                if let Ok(py) = sec.convert(input, max_candidates) {
                    if py.completed_syllables.len() >= 2 && !py.preedit_display.is_empty() {
                        pinyin_preedit = Some(py.preedit_display.clone());
                    }
                    pinyin = py.candidates;
                    for c in &mut pinyin {
                        c.weight /= PINYIN_TIER_SCALE;
                        if c.weight < 0 {
                            c.weight = 0;
                        }
                    }
                }
            }
        }

        // 3. 合并（码表在前，拼音在后）→ 按权重稳定排序 → 按文本去重
        let has_pinyin = !pinyin.is_empty();
        let mut merged = codetable;
        merged.extend(pinyin);
        merged.sort_by(|a, b| {
            b.weight
                .cmp(&a.weight)
                .then(a.natural_order.cmp(&b.natural_order))
        });
        let mut seen = std::collections::HashSet::new();
        merged.retain(|c| seen.insert(c.text.clone()));
        merged.truncate(max_candidates);
        if self.show_source_hint {
            Self::add_source_hints(&mut merged);
        }

        // 全码自动上屏重评（对齐 Go recheckAutoCommit）：取主码表意向，
        // 但若开启拼音守护且存在拼音候选则否决（输入可能是拼音，留给用户选）；
        // 并复核上屏目标在合并结果中仍存活。
        let (should_commit, commit_text) = if ct_should_commit
            && !ct_commit_text.is_empty()
            && !(self.auto_commit_block_on_pinyin && has_pinyin)
            && merged.iter().any(|c| c.text == ct_commit_text)
        {
            (true, ct_commit_text)
        } else {
            (false, String::new())
        };

        // 空码清空：仅当主码表请求清空且无拼音候选（合法拼音序列留给拼音，不清空）。
        let should_clear = ct_should_clear && !has_pinyin;

        let is_empty = merged.is_empty();
        Ok(ConvertResult {
            candidates: merged,
            // 组合区：多音节拼音用音节分隔（ni'hao），否则原始码（五笔为主，简明）。
            // 拼音拆分形态单独留存，供协调器「按高亮候选类型」选择显示原始码 / 拆分串。
            preedit_pinyin: pinyin_preedit.clone().unwrap_or_default(),
            preedit_display: pinyin_preedit.unwrap_or_else(|| input.to_string()),
            is_empty,
            should_commit,
            commit_text,
            should_clear,
            ..Default::default()
        })
    }

    fn reset(&self) {
        self.primary.reset();
        if let Some(s) = &self.secondary {
            s.reset();
        }
    }

    fn engine_type(&self) -> EngineType {
        EngineType::Mixed
    }

    /// 顶码裁决（对齐 Go HandleTopCode）：超码长时先做拼音保护，未命中或被开关放行才委托主码表。
    ///
    /// 前 N 码构成合法拼音序列 → 默认抑制顶码（用户可能在打拼音，如 yans→颜色）。仅当
    /// `top_code_override_pinyin` 开启 + 前缀为「终止性精确五笔全码」+ 拼音读法「非真实拼音」
    /// （整音节歧义 wang/aipu，或含非首位单字母音节的退化解析 naap/buap）时放行顶码倒向五笔。
    fn handle_top_code(&self, input: &str) -> Option<(String, String)> {
        let input_len = input.chars().count();
        if self.max_code_len == 0 || input_len <= self.max_code_len {
            return self.primary.handle_top_code(input);
        }
        let prefix: String = input.chars().take(self.max_code_len).collect();
        if let Some(sec) = &self.secondary {
            if sec.is_possible_pinyin_sequence(&prefix) {
                // 终止性精确五笔全码：前缀恰是唯一全码（精确匹配 + 无更长后继）。
                let is_terminal_exact = self.primary.has_full_input_match(&prefix)
                    && !self.primary.has_longer_code(&prefix);
                let override_topcode = self.top_code_override_pinyin
                    && is_terminal_exact
                    && (sec.is_whole_syllable_pinyin(&prefix)
                        || sec.has_non_initial_single_letter_syllable(&prefix));
                if !override_topcode {
                    return None;
                }
            }
        }
        self.primary.handle_top_code(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codetable::{CodeTableEngine, CommitOptions};
    use std::sync::Arc;
    use wind_dict::cached::CachedDict;
    use wind_dict::codetable::CodetableDict;
    use wind_dict::{DictManager, SystemDictLayer};

    /// 构建一个内存码表引擎（可选开启全码自动上屏）。
    fn ct_engine(entries: &[(&str, &str, i32)], at_full: bool) -> Box<dyn Engine> {
        let mut d = CodetableDict::empty();
        for (i, (code, text, w)) in entries.iter().enumerate() {
            d.merge_single(code.to_string(), text.to_string(), *w, i as i32);
        }
        let dm = DictManager::new();
        dm.register_layer(Box::new(SystemDictLayer::new(CachedDict::Memory(d), "sys")));
        let opts = CommitOptions {
            auto_commit_at_full: at_full,
            auto_commit_min_len: 4,
            ..Default::default()
        };
        Box::new(CodeTableEngine::new(4, opts, Arc::new(dm)))
    }

    #[test]
    fn mixed_propagates_auto_commit_without_pinyin() {
        // 主码表唯一全码自动上屏；无次引擎 → 无拼音候选 → 放行。
        let primary = ct_engine(&[("aaaa", "工", 100)], true);
        let e = MixedEngine::new(primary, None, 2, 10_000_000, true, true, false, false);
        let r = e.convert("aaaa", 50).unwrap();
        assert!(r.should_commit, "无拼音候选时应放行全码上屏");
        assert_eq!(r.commit_text, "工");
    }

    #[test]
    fn mixed_blocks_auto_commit_when_pinyin_present() {
        // 次引擎对同一输入也产出候选（模拟拼音命中）+ 守护开 → 否决上屏。
        let primary = ct_engine(&[("aaaa", "工", 100)], true);
        let secondary = ct_engine(&[("aaaa", "啊啊", 50)], false);
        let e = MixedEngine::new(
            primary,
            Some(secondary),
            2,
            10_000_000,
            true,
            true,
            false,
            false,
        );
        let r = e.convert("aaaa", 50).unwrap();
        assert!(!r.should_commit, "有拼音候选且守护开时应否决全码上屏");
    }

    #[test]
    fn mixed_allows_auto_commit_when_guard_off() {
        // 守护关 → 即便有拼音候选也放行。
        let primary = ct_engine(&[("aaaa", "工", 100)], true);
        let secondary = ct_engine(&[("aaaa", "啊啊", 50)], false);
        let e = MixedEngine::new(
            primary,
            Some(secondary),
            2,
            10_000_000,
            false,
            true,
            false,
            false,
        );
        let r = e.convert("aaaa", 50).unwrap();
        assert!(r.should_commit, "守护关时应放行");
        assert_eq!(r.commit_text, "工");
    }

    /// 构建开启顶码上屏的码表引擎（max_code_len=4）。
    fn ct_engine_topcode(entries: &[(&str, &str, i32)]) -> Box<dyn Engine> {
        let mut d = CodetableDict::empty();
        for (i, (code, text, w)) in entries.iter().enumerate() {
            d.merge_single(code.to_string(), text.to_string(), *w, i as i32);
        }
        let dm = DictManager::new();
        dm.register_layer(Box::new(SystemDictLayer::new(CachedDict::Memory(d), "sys")));
        let opts = CommitOptions {
            top_code_commit: true,
            ..Default::default()
        };
        Box::new(CodeTableEngine::new(4, opts, Arc::new(dm)))
    }

    /// 真实拼音次引擎（空词典；音节分析只依赖标准音节 trie）。
    fn pinyin_secondary() -> Box<dyn Engine> {
        Box::new(crate::pinyin::PinyinEngine::new(
            crate::pinyin::Config::default(),
            CachedDict::Memory(CodetableDict::empty()),
        ))
    }

    #[test]
    fn topcode_suppressed_for_pinyin_prefix_when_override_off() {
        // "wang" 前缀既是完整拼音又是唯一五笔全码；override 关 → 抑制顶码，保护拼音。
        let primary = ct_engine_topcode(&[("wang", "王", 100)]);
        let e = MixedEngine::new(
            primary,
            Some(pinyin_secondary()),
            2,
            10_000_000,
            true,
            true,
            false,
            false,
        );
        assert_eq!(
            e.handle_top_code("wangb"),
            None,
            "override 关时应抑制顶码（拼音保护）"
        );
    }

    #[test]
    fn source_hint_marks_pinyin_candidates() {
        let mut cands = vec![
            Candidate {
                text: "工".into(),
                source: CandidateSource::CodeTable,
                ..Default::default()
            },
            Candidate {
                text: "你好".into(),
                source: CandidateSource::Pinyin,
                ..Default::default()
            },
            Candidate {
                text: "拟".into(),
                source: CandidateSource::Pinyin,
                comment: "ni".into(),
                ..Default::default()
            },
        ];
        MixedEngine::add_source_hints(&mut cands);
        assert_eq!(cands[0].comment, "", "码表候选不标记");
        assert_eq!(cands[1].comment, "拼");
        assert_eq!(cands[2].comment, "拼|ni", "已有 comment 时前置拼接");
    }

    #[test]
    fn topcode_released_for_ambiguous_prefix_when_override_on() {
        // override 开 + 终止性精确全码 + 整音节歧义（wang）→ 放行顶码倒向五笔。
        let primary = ct_engine_topcode(&[("wang", "王", 100)]);
        let e = MixedEngine::new(
            primary,
            Some(pinyin_secondary()),
            2,
            10_000_000,
            true,
            true,
            true,
            false,
        );
        assert_eq!(
            e.handle_top_code("wangb"),
            Some(("王".to_string(), "b".to_string())),
            "override 开 + 整音节歧义全码应放行顶码"
        );
    }
}
