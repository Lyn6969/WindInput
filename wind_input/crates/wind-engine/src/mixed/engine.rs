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
/// 英文精确匹配（整词 code==input）提权：完整英文词可靠前，但低于码表精确/短语档。
const ENGLISH_EXACT_BOOST: i32 = 500_000;
/// 英文前缀补全提权：不额外提权（保留词库原始权重），使前缀英文沉在码表/拼音候选之后，
/// 避免短前缀（如「d」）刷屏。真机若仍偏高可继续下调。
const ENGLISH_PREFIX_BOOST: i32 = 0;

/// 混输引擎的标量配置（融合策略参数）。引擎部件 primary/secondary/english 单独传入 `new`；
/// 此处仅聚合可配开关/阈值，避免 `new` 参数膨胀。字段语义见 [`MixedEngine`] 同名字段。
#[derive(Debug, Clone)]
pub struct MixConfig {
    pub min_pinyin_length: usize,
    pub codetable_weight_boost: i32,
    pub auto_commit_block_on_pinyin: bool,
    pub pinyin_only_overflow: bool,
    pub top_code_override_pinyin: bool,
    pub show_source_hint: bool,
    pub min_english_length: usize,
    pub auto_commit_block_on_english: bool,
    pub block_commit_on_pinyin_word: bool,
    pub pinyin_word_min_weight: i32,
}

impl Default for MixConfig {
    fn default() -> Self {
        Self {
            min_pinyin_length: 2,
            codetable_weight_boost: 10_000_000,
            auto_commit_block_on_pinyin: false,
            pinyin_only_overflow: true,
            top_code_override_pinyin: false,
            show_source_hint: false,
            min_english_length: 2,
            auto_commit_block_on_english: false,
            block_commit_on_pinyin_word: true,
            pinyin_word_min_weight: 0,
        }
    }
}

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
    /// 全码自动上屏时，若存在拼音候选则否决（保护拼音用户，对齐 Go AutoCommitBlockOnPinyin）。
    /// 默认关（与 data/config.toml 一致）：粗粒度一票否决太激进，细粒度拦截由
    /// `block_commit_on_pinyin_word`（默认开）承担。
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
    /// 英文词库引擎（schema.mix.enable_english 开且 english 方案可加载时为 Some）。
    /// 混输各路径按精确/前缀加权混入英文候选；None = 关闭（零开销）。
    english: Option<Box<dyn Engine>>,
    /// 英文最小触发长度：输入短于此值时不查英文（2 字符以内不匹配 → 默认 3）。
    min_english_length: usize,
    /// 满码自动上屏时若存在英文候选（含前缀）则否决（保护正在输入英文词的用户）。
    auto_commit_block_on_english: bool,
    /// 拼音歧义拦截（词强度）：整串是强拼音词时否决五笔自动/顶码上屏，让拼音赢
    /// （wangba→网吧；aipu 无强词则放行落实）。默认开；独立于 auto_commit_block_on_pinyin。
    block_commit_on_pinyin_word: bool,
    /// 词强度权重阈值（0=仅结构判据：拼音首选须 ≥2 汉字且消费整串；预留真机调）。
    pinyin_word_min_weight: i32,
}

impl MixedEngine {
    /// 构造混输引擎：primary（码表主）/ secondary（拼音次）/ english（英文词库，可空）为引擎部件，
    /// 其余融合策略参数经 [`MixConfig`] 传入。
    pub fn new(
        primary: Box<dyn Engine>,
        secondary: Option<Box<dyn Engine>>,
        english: Option<Box<dyn Engine>>,
        cfg: MixConfig,
    ) -> Self {
        let max_code_len = primary.max_code_length();
        Self {
            primary,
            secondary,
            min_pinyin_length: cfg.min_pinyin_length,
            codetable_weight_boost: cfg.codetable_weight_boost,
            auto_commit_block_on_pinyin: cfg.auto_commit_block_on_pinyin,
            pinyin_only_overflow: cfg.pinyin_only_overflow,
            top_code_override_pinyin: cfg.top_code_override_pinyin,
            max_code_len,
            show_source_hint: cfg.show_source_hint,
            english,
            min_english_length: cfg.min_english_length,
            auto_commit_block_on_english: cfg.auto_commit_block_on_english,
            block_commit_on_pinyin_word: cfg.block_commit_on_pinyin_word,
            pinyin_word_min_weight: cfg.pinyin_word_min_weight,
        }
    }

    /// 拼音词否决判据（`block_commit_on_pinyin_word` 开时生效；满码/顶码共用）。命中任一即判为
    /// 「用户意图是拼音（词）」→ 否决五笔上屏。`secondary` 为 None / 开关关时恒 false。
    ///
    /// **(b) 单音节前缀（中途态）**：前 N 码前缀恰是「1 个完整拼音音节」（如 wang）→ 用户多在打
    /// 拼音词的中途（wangb→wangba→网吧），保护拼音。≥2 音节前缀（aipu=ai+pu）已是完整多音节
    /// 单元、多为恰好像拼音的五笔码 → 不拦（放行落实）。这是区分 wang（拦）/ aipu（放）的关键。
    ///
    /// **(a) 整串强拼音词**：整串是完整拼音音节序列、且拼音首选是「≥2 汉字、消费整串」的真实词
    /// （权重 ≥ `pinyin_word_min_weight`）——借拼音引擎自身排序识别（真词排 #1 且消费整串）。
    fn is_ambiguous_pinyin_word(&self, input: &str) -> bool {
        if !self.block_commit_on_pinyin_word {
            return false;
        }
        let Some(sec) = &self.secondary else {
            return false;
        };
        // (b) 前 N 码前缀是单个完整拼音音节 → 中途打拼音词，保护拼音。
        let plen = self.max_code_len.min(input.chars().count());
        if plen >= 1 {
            let prefix: String = input.chars().take(plen).collect();
            if sec.is_whole_syllable_pinyin(&prefix) && sec.completed_syllable_count(&prefix) == 1 {
                return true;
            }
        }
        // (a) 整串是完整拼音强词。
        if !sec.is_whole_syllable_pinyin(input) {
            return false;
        }
        let Ok(r) = sec.convert(input, 8) else {
            return false;
        };
        let Some(top) = r.candidates.first() else {
            return false;
        };
        let input_len = input.chars().count();
        // consumed_length==0 表示引擎未标注（视为整串匹配）。
        let consumes_all = top.consumed_length == 0 || top.consumed_length >= input_len;
        top.text.chars().count() >= 2 && consumes_all && top.weight >= self.pinyin_word_min_weight
    }

    /// 五笔上屏拼音否决（**满码全码自动上屏 / 顶码上屏共用同一套**，保证两条通路一致）：
    /// - ① `auto_commit_block_on_pinyin` 且存在拼音候选（`has_pinyin`）→ 否决（有拼音就让路，粗粒度）；
    /// - ② `block_commit_on_pinyin_word` 且整串是强拼音词（词强度）→ 否决。
    ///
    /// `has_pinyin` 由调用方按各自可见的候选给出（满码=引擎合并前的拼音候选；顶码=对整串查拼音）。
    fn pinyin_vetoes_commit(&self, input: &str, has_pinyin: bool) -> bool {
        (self.auto_commit_block_on_pinyin && has_pinyin) || self.is_ambiguous_pinyin_word(input)
    }

    /// 拼音后续可能性（满码空码清空守护专用）：整串是否**可能**通过继续输入产生拼音候选
    /// （含残缺尾音节，如 zhon→zhong）。这是码表侧 `has_longer_code` 的拼音对偶——码表问
    /// 「有无更长后继码」，拼音问「是不是合法音节前缀」，两者共同构成「这串码还有后续」。
    ///
    /// 与上屏否决 `is_ambiguous_pinyin_word` 的分工：那个判「拼音**已经**成词」（看词典权重），
    /// 这个判「拼音**还没打完**」（只查标准音节表，不查词典）。清空发生在无候选时，正需要后者。
    /// `secondary` 为 None（纯码表混输）时恒 false。
    ///
    /// **前提：混输不接双拼**（码长太接近，产品上不支持）。`is_possible_pinyin_sequence` 与另三个
    /// 音节判据一样，把入参当全拼直喂音节表、不走 `ShuangpinConverter`（不同于 `convert()`）。
    /// 若将来给混输接入双拼，此处会**静默**误判：如小鹤 `nihc`(=ni+hao) 判为「无后续」→ 清空吞掉
    /// 用户正在输入的串。届时须先给这四个判据加统一的双拼前置转换，勿只改本函数。
    fn pinyin_may_continue(&self, input: &str) -> bool {
        self.secondary
            .as_ref()
            .is_some_and(|sec| sec.is_possible_pinyin_sequence(input))
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

    /// 英文候选（enable_english 开时）：查英文词库，按精确(整词)/前缀独立加权，供混入合并。
    /// 英文档独立于拼音（不被 ÷100 降档）：精确 +ENGLISH_EXACT_BOOST(500K)、前缀 +0（保留原始权重）。
    /// `english` 为 None（关闭）时返回空。输入小写化以匹配英文词库（code 列已小写化）。
    fn english_candidates(&self, input: &str, max_candidates: usize) -> Vec<Candidate> {
        let Some(eng) = &self.english else {
            return Vec::new();
        };
        // 英文最小长度：短输入（默认 2 字符以内）不查英文，避免短前缀刷屏（对齐拼音 min 思路）。
        if input.chars().count() < self.min_english_length {
            return Vec::new();
        }
        let lower = input.to_lowercase();
        let Ok(r) = eng.convert(&lower, max_candidates) else {
            return Vec::new();
        };
        let mut out = r.candidates;
        for c in &mut out {
            let boost = if c.code == lower {
                ENGLISH_EXACT_BOOST
            } else {
                ENGLISH_PREFIX_BOOST
            };
            c.weight = c.weight.saturating_add(boost);
        }
        out
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
            // 英文候选（enable_english 开时）：独立加权档，与拼音/码表统一混入（对齐 Go 各路径处理英文）。
            let english = self.english_candidates(input, max_candidates);
            // 长码特例：完整 input 在码表有精确/更长后继 → 追加码表候选，拼音归一化降档避免档位重叠。
            let mut merged = if has_full_or_longer {
                Self::normalize_pinyin(&mut pinyin);
                let mut ct = self
                    .primary
                    .convert(input, max_candidates)
                    .unwrap_or_default()
                    .candidates;
                self.boost_codetable(&mut ct, input);
                ct.extend(english);
                Self::merge_sort_dedup(ct, pinyin, max_candidates)
            } else if !english.is_empty() {
                // 纯拼音 + 英文：拼音归一化降档，英文独立档排前。
                Self::normalize_pinyin(&mut pinyin);
                Self::merge_sort_dedup(english, pinyin, max_candidates)
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
            // 英文候选（enable_english 开时）：独立加权档并入码表位，与拼音一同竞争。
            codetable.extend(self.english_candidates(input, max_candidates));
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
        //
        // 注：此分支**有意不产生 `should_clear`**（`convert_overflow` 恒返回 false）。超长即已切入
        // 纯拼音语境，「码表满码却无候选」这个前提不再成立，此时清空会打断正常的长拼音输入。
        // 故满码空码清空仅在 `input_len == max_code_len` 生效，勿按「缺口」补齐。
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

        // 3. 合并（码表在前，拼音在后，英文独立档混入）→ 按权重稳定排序 → 按文本去重
        let has_pinyin = !pinyin.is_empty();
        let mut merged = codetable;
        merged.extend(pinyin);
        // 英文候选（enable_english 开时）：独立加权档混入，与码表/拼音一同竞争排序。
        merged.extend(self.english_candidates(input, max_candidates));
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

        // 英文守护（对齐拼音守护）：满码上屏时若存在英文候选（含前缀），说明用户可能正在
        // 输入更长的英文词，否决自动上屏留给用户选择。仅 auto_commit_block_on_english 开时生效。
        let has_english = self.auto_commit_block_on_english
            && merged.iter().any(|c| c.source == CandidateSource::English);

        // 全码自动上屏重评（对齐 Go recheckAutoCommit）：取主码表意向，但若英文守护命中、或
        // 拼音否决①②命中（`pinyin_vetoes_commit`，与顶码同一套）则否决（输入可能是拼音/英文，
        // 留给用户选）；并复核上屏目标在合并结果中仍存活。
        // `pinyin_vetoes_commit` 经短路仅在码表确有满码上屏意向时求值（避免每键多跑一次转换）。
        let (should_commit, commit_text) = if ct_should_commit
            && !ct_commit_text.is_empty()
            && !has_english
            && !self.pinyin_vetoes_commit(input, has_pinyin)
            && merged.iter().any(|c| c.text == ct_commit_text)
        {
            (true, ct_commit_text)
        } else {
            (false, String::new())
        };

        // 满码空码清空：主码表请求清空 + 拼音侧既无候选、也无后续可能。
        // - `!has_pinyin`：拼音此刻已出候选 → 留给拼音（粗粒度，且合并后非空，协调器亦会复核）；
        // - `!pinyin_may_continue`：拼音**还没打完** → 保护中途态。这一项才是无候选时的关键守护：
        //   如 zhon（码表满码无候选无后继、拼音此刻也无候选）合并结果为空，协调器的
        //   `state.candidates.is_empty()` 复核挡不住，若不看后续可能性就会把用户正在输入的
        //   zhong 吞掉。经 `&&` 短路，仅在码表确有清空意向时才查音节表。
        let should_clear = ct_should_clear && !has_pinyin && !self.pinyin_may_continue(input);

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

    /// 满码自动上屏「显示态」复评：先按**与 should_commit 同一套**拼音①②/英文守护否决
    /// （避免复评绕过否决——修"满码全码唯一自动上屏时不否决"），再在**码表来源**候选中判唯一
    /// 精确全码（拼音/英文不参与满码上屏）委托主码表复评。智能过滤掉生僻同码字后剩唯一精确全码
    /// 时放行。`has_pinyin`/`has_english` 按显示候选来源判定（与所见一致）。
    fn recheck_auto_commit(&self, input: &str, candidates: &[Candidate]) -> Option<String> {
        let has_pinyin = candidates
            .iter()
            .any(|c| c.source == CandidateSource::Pinyin);
        let has_english = self.auto_commit_block_on_english
            && candidates
                .iter()
                .any(|c| c.source == CandidateSource::English);
        if has_english || self.pinyin_vetoes_commit(input, has_pinyin) {
            return None;
        }
        let ct: Vec<Candidate> = candidates
            .iter()
            .filter(|c| c.source == CandidateSource::CodeTable)
            .cloned()
            .collect();
        self.primary.recheck_auto_commit(input, &ct)
    }

    /// 顶码裁决（对齐 Go HandleTopCode）：超码长时**用与满码全码自动上屏完全相同的拼音①②否决**
    /// （`pinyin_vetoes_commit`），未被否决才委托主码表顶码。两条上屏通路同一套判据，杜绝
    /// "满码不否决、顶码却否决"的不一致。
    ///
    /// - ① `auto_commit_block_on_pinyin` 且整串有拼音候选 → 抑制顶码（打开时 wangba/aipu 等含拼音
    ///   读法的串都让路拼音）；
    /// - ② `block_commit_on_pinyin_word` 且整串是强拼音词（wangba→网吧）→ 抑制顶码；
    /// - `top_code_override_pinyin` 开启 = 顶码优先，**无视**拼音否决强制倒向五笔。
    fn handle_top_code(&self, input: &str) -> Option<(String, String)> {
        let input_len = input.chars().count();
        if self.max_code_len == 0 || input_len <= self.max_code_len {
            return self.primary.handle_top_code(input);
        }
        // 顶码优先开关关闭时，应用与满码同一套拼音①②否决。
        if !self.top_code_override_pinyin {
            if let Some(sec) = &self.secondary {
                // ①的 has_pinyin：整串是否有拼音候选（与满码"合并前拼音候选非空"同义）。
                let has_pinyin = sec
                    .convert(input, 1)
                    .map(|r| !r.candidates.is_empty())
                    .unwrap_or(false);
                if self.pinyin_vetoes_commit(input, has_pinyin) {
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
        let e = MixedEngine::new(primary, None, None, MixConfig::default());
        let r = e.convert("aaaa", 50).unwrap();
        assert!(r.should_commit, "无拼音候选时应放行全码上屏");
        assert_eq!(r.commit_text, "工");
    }

    #[test]
    fn mixed_blocks_auto_commit_when_pinyin_present() {
        // 次引擎对同一输入也产出候选（模拟拼音命中）+ 守护①显式开 → 否决上屏。
        let primary = ct_engine(&[("aaaa", "工", 100)], true);
        let secondary = ct_engine(&[("aaaa", "啊啊", 50)], false);
        let e = MixedEngine::new(
            primary,
            Some(secondary),
            None,
            MixConfig {
                auto_commit_block_on_pinyin: true,
                ..Default::default()
            },
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
            None,
            MixConfig {
                auto_commit_block_on_pinyin: false,
                ..Default::default()
            },
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

    /// 可配假拼音引擎：`word`="" 表示无候选（has_pinyin=false）；`syllables` 同时驱动
    /// is_whole_syllable_pinyin(=`syllables>0`) 与 completed_syllable_count(=`syllables`)——
    /// 用于单测顶码/满码共用的拼音①②否决（含 ②(b) 单音节前缀保护）。
    struct FakePinyin {
        word: &'static str,
        syllables: usize,
    }
    impl Engine for FakePinyin {
        fn convert(&self, input: &str, _max: usize) -> anyhow::Result<ConvertResult> {
            let candidates = if self.word.is_empty() {
                vec![]
            } else {
                vec![Candidate {
                    text: self.word.to_string(),
                    code: input.to_string(),
                    weight: 1000,
                    consumed_length: input.chars().count(),
                    source: CandidateSource::Pinyin,
                    ..Default::default()
                }]
            };
            Ok(ConvertResult {
                candidates,
                ..Default::default()
            })
        }
        fn reset(&self) {}
        fn engine_type(&self) -> EngineType {
            EngineType::Pinyin
        }
        fn is_whole_syllable_pinyin(&self, _prefix: &str) -> bool {
            self.syllables > 0
        }
        fn completed_syllable_count(&self, _prefix: &str) -> usize {
            self.syllables
        }
    }

    // ── 满码空码清空：拼音「后续可能性」守护 ──

    /// 构建开启「满码空码清空」的码表引擎（max_code_len=4）。
    fn ct_engine_clear(entries: &[(&str, &str, i32)]) -> Box<dyn Engine> {
        let mut d = CodetableDict::empty();
        for (i, (code, text, w)) in entries.iter().enumerate() {
            d.merge_single(code.to_string(), text.to_string(), *w, i as i32);
        }
        let dm = DictManager::new();
        dm.register_layer(Box::new(SystemDictLayer::new(CachedDict::Memory(d), "sys")));
        let opts = CommitOptions {
            clear_on_empty_max: true,
            ..Default::default()
        };
        Box::new(CodeTableEngine::new(4, opts, Arc::new(dm)))
    }

    /// 清空守护专用假拼音：**恒无候选**（has_pinyin=false，把协调器的候选非空复核排除在外），
    /// 仅可配「整串是否为合法拼音前缀」——正是本守护要验的那一位。
    struct FakePinyinPrefix {
        may_continue: bool,
    }
    impl Engine for FakePinyinPrefix {
        fn convert(&self, _input: &str, _max: usize) -> anyhow::Result<ConvertResult> {
            Ok(ConvertResult::default())
        }
        fn reset(&self) {}
        fn engine_type(&self) -> EngineType {
            EngineType::Pinyin
        }
        fn is_possible_pinyin_sequence(&self, _prefix: &str) -> bool {
            self.may_continue
        }
    }

    fn mixed_with_prefix_pinyin(may_continue: bool) -> MixedEngine {
        MixedEngine::new(
            ct_engine_clear(&[("aaaa", "工", 100)]),
            Some(Box::new(FakePinyinPrefix { may_continue })),
            None,
            MixConfig::default(),
        )
    }

    #[test]
    fn clear_fires_when_pinyin_cannot_continue() {
        // 满码(4) 码表无候选无后继 + 拼音无候选且非合法前缀 → 清空。
        let r = mixed_with_prefix_pinyin(false).convert("qqqq", 50).unwrap();
        assert!(r.candidates.is_empty(), "前置：此输入确无候选");
        assert!(r.should_clear, "拼音无后续可能时应清空");
    }

    #[test]
    fn clear_vetoed_when_pinyin_may_continue() {
        // 同上，但拼音判「还没打完」（zhon→zhong 中途态）→ 守护住，不得清空。
        // 合并候选为空，协调器的 `state.candidates.is_empty()` 复核挡不住——只能靠这一位。
        let r = mixed_with_prefix_pinyin(true).convert("zhon", 50).unwrap();
        assert!(r.candidates.is_empty(), "前置：此刻确无候选");
        assert!(!r.should_clear, "拼音仍可能有后续时不得清空，否则吞掉中途输入");
    }

    #[test]
    fn overflow_never_clears() {
        // 超长（>max_code_len）**有意**不清空：已切入纯拼音语境，「码表满码无候选」前提不成立。
        let r = mixed_with_prefix_pinyin(false).convert("qqqqq", 50).unwrap();
        assert!(!r.should_clear, "overflow 分支不得产生清空");
    }

    // ── 顶码上屏：与满码全码自动上屏**共用同一套**拼音①②否决 ──

    #[test]
    fn topcode_vetoed_by_pinyin_candidate() {
        // ① auto_commit_block_on_pinyin 显式开（默认关）+ 整串有拼音候选 → 抑制顶码。
        let primary = ct_engine_topcode(&[("wang", "王", 100)]);
        let e = MixedEngine::new(
            primary,
            Some(Box::new(FakePinyin {
                word: "网",
                syllables: 0,
            })),
            None,
            MixConfig {
                auto_commit_block_on_pinyin: true,
                ..Default::default()
            },
        );
        assert_eq!(
            e.handle_top_code("wangb"),
            None,
            "① 开 + 有拼音候选应抑制顶码"
        );
    }

    #[test]
    fn topcode_allowed_when_no_pinyin_candidate() {
        // 纯五笔溢出（整串无拼音候选）→ 顶码正常上屏（② 默认开也不拦）。
        let primary = ct_engine_topcode(&[("aaaa", "工", 100)]);
        let e = MixedEngine::new(
            primary,
            Some(Box::new(FakePinyin {
                word: "",
                syllables: 0,
            })),
            None,
            MixConfig::default(),
        );
        assert_eq!(
            e.handle_top_code("aaaab"),
            Some(("工".to_string(), "b".to_string())),
            "无拼音候选时顶码应正常上屏"
        );
    }

    #[test]
    fn topcode_vetoed_by_pinyin_word_when_block_on_pinyin_off() {
        // ① 关、② 开：整串是强拼音词（网吧）→ 仍抑制顶码。
        let primary = ct_engine_topcode(&[("wang", "王", 100)]);
        let e = MixedEngine::new(
            primary,
            Some(Box::new(FakePinyin {
                word: "网吧",
                syllables: 2,
            })),
            None,
            MixConfig {
                auto_commit_block_on_pinyin: false,
                ..Default::default()
            },
        );
        assert_eq!(e.handle_top_code("wangba"), None, "② 强拼音词应抑制顶码");
    }

    #[test]
    fn topcode_allowed_when_both_guards_off() {
        // ①② 都关：即便整串像拼音也顶码倒向五笔（王 + 余码 ba）。
        let primary = ct_engine_topcode(&[("wang", "王", 100)]);
        let e = MixedEngine::new(
            primary,
            Some(Box::new(FakePinyin {
                word: "网吧",
                syllables: 2,
            })),
            None,
            MixConfig {
                auto_commit_block_on_pinyin: false,
                block_commit_on_pinyin_word: false,
                ..Default::default()
            },
        );
        assert_eq!(
            e.handle_top_code("wangba"),
            Some(("王".to_string(), "ba".to_string())),
            "①② 都关时顶码倒向五笔"
        );
    }

    #[test]
    fn topcode_override_ignores_pinyin_veto() {
        // top_code_override_pinyin 开 = 顶码优先，无视拼音①②否决，强制倒向五笔。
        let primary = ct_engine_topcode(&[("wang", "王", 100)]);
        let e = MixedEngine::new(
            primary,
            Some(Box::new(FakePinyin {
                word: "网吧",
                syllables: 2,
            })),
            None,
            MixConfig {
                top_code_override_pinyin: true,
                ..Default::default()
            },
        );
        assert_eq!(
            e.handle_top_code("wangba"),
            Some(("王".to_string(), "ba".to_string())),
            "顶码优先应无视拼音否决"
        );
    }

    #[test]
    fn topcode_vetoed_by_single_syllable_prefix_when_block_on_pinyin_off() {
        // ① 关、② 开：前缀 "wang" 是单个完整拼音音节（中途打拼音词 wangba）→ 抑制顶码，
        // 即便 "wangb" 尚未构成完整拼音词（用户实测：① 关时 wangb 仍顶 佢 的 bug）。
        let primary = ct_engine_topcode(&[("wang", "王", 100)]);
        let e = MixedEngine::new(
            primary,
            Some(Box::new(FakePinyin {
                word: "网",
                syllables: 1,
            })),
            None,
            MixConfig {
                auto_commit_block_on_pinyin: false,
                ..Default::default()
            },
        );
        assert_eq!(
            e.handle_top_code("wangb"),
            None,
            "① 关 + ② 开：单音节前缀（中途打拼音词）应抑制顶码"
        );
    }

    #[test]
    fn topcode_allowed_for_multi_syllable_prefix_when_block_on_pinyin_off() {
        // ① 关、② 开：前缀 "aipu"=ai+pu 是完整多音节单元、无强词 → 放行顶码倒向五笔（落实）。
        let primary = ct_engine_topcode(&[("aipu", "落实", 100)]);
        let e = MixedEngine::new(
            primary,
            Some(Box::new(FakePinyin {
                word: "矮",
                syllables: 2,
            })),
            None,
            MixConfig {
                auto_commit_block_on_pinyin: false,
                ..Default::default()
            },
        );
        assert_eq!(
            e.handle_top_code("aipux"),
            Some(("落实".to_string(), "x".to_string())),
            "① 关 + ② 开：多音节前缀无强词应放行顶码"
        );
    }

    #[test]
    fn mixed_recheck_auto_commit_after_filter() {
        // 引擎按未过滤候选（含生僻同码字）判不唯一而否决满码上屏；智能过滤后只剩唯一精确
        // 全码码表候选 → 复评据显示候选放行（bug: 显示只剩一个却不上屏）。
        let primary = ct_engine(&[("hhnu", "X", 100), ("hhnu", "愳", 1)], true);
        let e = MixedEngine::new(primary, None, None, MixConfig::default());
        // 原始转换：两个精确 hhnu → 不唯一，引擎不给上屏意向。
        let r = e.convert("hhnu", 50).unwrap();
        assert!(!r.should_commit, "两个精确同码候选时引擎不自动上屏");
        // 模拟智能过滤后仅剩一个码表精确全码候选 → 复评放行。
        let filtered = vec![Candidate {
            text: "X".into(),
            code: "hhnu".into(),
            source: CandidateSource::CodeTable,
            ..Default::default()
        }];
        assert_eq!(
            e.recheck_auto_commit("hhnu", &filtered),
            Some("X".to_string()),
            "过滤后唯一精确全码应复评放行"
        );
        // 拼音/英文来源不参与满码自动上屏：即便过滤后剩一个拼音候选也不放行。
        let py_only = vec![Candidate {
            text: "往".into(),
            code: "hhnu".into(),
            source: CandidateSource::Pinyin,
            ..Default::default()
        }];
        assert_eq!(e.recheck_auto_commit("hhnu", &py_only), None);
    }

    #[test]
    fn mixed_blocks_auto_commit_when_pinyin_word() {
        // 主码表 mama 唯一全码本会自动上屏；① 关但整串是强拼音词 妈妈（②）→ 否决满码上屏。
        let primary = ct_engine(&[("mama", "X", 100)], true);
        let e = MixedEngine::new(
            primary,
            Some(Box::new(FakePinyin {
                word: "妈妈",
                syllables: 2,
            })),
            None,
            MixConfig {
                auto_commit_block_on_pinyin: false,
                ..Default::default()
            },
        );
        let r = e.convert("mama", 50).unwrap();
        assert!(!r.should_commit, "整串是强拼音词时应否决满码上屏");
    }

    #[test]
    fn mixed_allows_auto_commit_when_pinyin_word_guard_off() {
        // ①② 都关 → 即便整串是强拼音词也放行满码上屏（零回归）。
        let primary = ct_engine(&[("mama", "X", 100)], true);
        let e = MixedEngine::new(
            primary,
            Some(Box::new(FakePinyin {
                word: "妈妈",
                syllables: 2,
            })),
            None,
            MixConfig {
                auto_commit_block_on_pinyin: false,
                block_commit_on_pinyin_word: false,
                ..Default::default()
            },
        );
        let r = e.convert("mama", 50).unwrap();
        assert!(r.should_commit, "①② 都关时应放行满码上屏");
        assert_eq!(r.commit_text, "X");
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

    /// 内存英文引擎（EnglishEngine 包码表；code=小写英文词，前缀匹配）。
    fn english_engine(entries: &[(&str, &str, i32)]) -> Box<dyn Engine> {
        let mut d = CodetableDict::empty();
        for (i, (code, text, w)) in entries.iter().enumerate() {
            d.merge_single(code.to_string(), text.to_string(), *w, i as i32);
        }
        let dm = DictManager::new();
        dm.register_layer(Box::new(SystemDictLayer::new(CachedDict::Memory(d), "en")));
        let ct = CodeTableEngine::new(32, CommitOptions::default(), Arc::new(dm));
        Box::new(crate::english::EnglishEngine::new(ct))
    }

    #[test]
    fn mixed_mixes_english_when_enabled() {
        // enable_english（english=Some）：混输主路径应混入英文词库候选（前缀匹配）。
        let primary = ct_engine(&[("hao", "好", 100)], false);
        let english = english_engine(&[("hello", "hello", 50), ("help", "help", 40)]);
        let e = MixedEngine::new(
            primary,
            None,
            Some(english),
            MixConfig {
                auto_commit_block_on_pinyin: false,
                ..Default::default()
            },
        );
        let r = e.convert("hel", 50).unwrap();
        assert!(
            r.candidates.iter().any(|c| c.text == "hello"),
            "开启英文时混输应含英文候选 hello，实际: {:?}",
            r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
        assert!(
            r.candidates
                .iter()
                .filter(|c| c.text == "hello" || c.text == "help")
                .all(|c| c.source == CandidateSource::English),
            "英文候选来源应标记 English"
        );
    }

    #[test]
    fn mixed_no_english_when_disabled() {
        // english=None：不混入英文候选（零回归）。
        let primary = ct_engine(&[("hao", "好", 100)], false);
        let e = MixedEngine::new(
            primary,
            None,
            None,
            MixConfig {
                auto_commit_block_on_pinyin: false,
                ..Default::default()
            },
        );
        let r = e.convert("hel", 50).unwrap();
        assert!(
            !r.candidates.iter().any(|c| c.text == "hello"),
            "关闭英文时不应有英文候选"
        );
    }

    #[test]
    fn mixed_english_respects_min_length() {
        // min_english_length=3：2 字符以内不查英文，3 字符起才混入。
        let primary = ct_engine(&[("x", "叉", 100)], false);
        let english = english_engine(&[("hello", "hello", 50)]);
        let e = MixedEngine::new(
            primary,
            None,
            Some(english),
            MixConfig {
                auto_commit_block_on_pinyin: false,
                min_english_length: 3,
                ..Default::default()
            },
        );
        let r2 = e.convert("he", 50).unwrap();
        assert!(
            !r2.candidates.iter().any(|c| c.text == "hello"),
            "2 字符（< min 3）不应出英文候选"
        );
        let r3 = e.convert("hel", 50).unwrap();
        assert!(
            r3.candidates.iter().any(|c| c.text == "hello"),
            "3 字符（>= min 3）应出英文候选"
        );
    }

    #[test]
    fn mixed_blocks_auto_commit_when_english_present() {
        // 主码表唯一全码本会自动上屏；开英文守护 + 有英文候选 → 否决（留给用户选英文）。
        let primary = ct_engine(&[("good", "工", 100)], true);
        let english = english_engine(&[("good", "good", 50), ("goodbye", "goodbye", 40)]);
        let e = MixedEngine::new(
            primary,
            None,
            Some(english),
            MixConfig {
                auto_commit_block_on_pinyin: false,
                auto_commit_block_on_english: true,
                ..Default::default()
            },
        );
        let r = e.convert("good", 50).unwrap();
        assert!(!r.should_commit, "开英文守护且有英文候选时应否决全码上屏");
        assert!(
            r.candidates.iter().any(|c| c.text == "good"),
            "应含英文候选 good"
        );
    }

    #[test]
    fn mixed_allows_auto_commit_when_english_guard_off() {
        // 英文守护关 → 即便有英文候选也放行全码上屏（零回归）。
        let primary = ct_engine(&[("good", "工", 100)], true);
        let english = english_engine(&[("good", "good", 50)]);
        let e = MixedEngine::new(
            primary,
            None,
            Some(english),
            MixConfig {
                auto_commit_block_on_pinyin: false,
                ..Default::default()
            },
        );
        let r = e.convert("good", 50).unwrap();
        assert!(r.should_commit, "英文守护关时应放行全码上屏");
        assert_eq!(r.commit_text, "工");
    }
}
