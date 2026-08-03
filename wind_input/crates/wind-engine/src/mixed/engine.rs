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
/// 拼音候选的**保底配额分母**：截断时至少给拼音留 `max_candidates / 此值` 席
/// （生产 `max_candidates=300` ⇒ 60 席）。见 [`MixedEngine::truncate_with_pinyin_quota`]。
const PINYIN_QUOTA_DIVISOR: usize = 5;

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
            // ⚠️ 三处同源：本处 / `MixGlobal::default()`（wind-config）/ `data/config.toml
            // [schema.mix]` 必须一致，改默认须同步全部三处。出厂默认以 L1⊕L2 为准（L2 覆盖 L1），
            // 即 data/config.toml 里的值。本处曾长期为 false 而另两处为 true，导致引擎单测跑在一个
            // 现实中不存在的配置下（测试全绿但保护实际是开着的）。
            auto_commit_block_on_pinyin: true,
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
    /// 全码自动上屏 / 顶码上屏 / **满码空码清空**时，若存在拼音候选则否决（保护拼音用户，
    /// 对齐 Go AutoCommitBlockOnPinyin）。**默认开**（三处同源：`MixConfig::default()` /
    /// `MixGlobal::default()` / `data/config.toml`）。粗粒度：整串只要查得出拼音候选就让路，
    /// 不看拼音成不成词；细粒度拦截另由 `block_commit_on_pinyin_word`（亦默认开）承担，两者叠加。
    ///
    /// 清空那条通路（`convert` 的 `should_clear`）除「有拼音候选」外还受 `pinyin_may_continue`
    /// （拼音还没打完）支配，二者同归本开关——关闭即「拼音一律不干预码表处置」。
    auto_commit_block_on_pinyin: bool,
    /// 输入超过码表最大码长时仅查拼音（主流混输行为，对齐 Go PinyinOnlyOverflow）。
    /// false 时走「码表前 N 码 + 拼音完整输入」混合 overflow。
    ///
    /// 「仅查拼音」有一个例外口 [`Self::codetable_owns_overflow`]：前 N 码是码表精确全码而拼音
    /// 主张不了整串时，码表候选照样回捞、顶码照样放行。它同时管着本项在 `convert_overflow` 与
    /// `handle_top_code` 两处的表现，改判据须两处一起验。
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
    /// **已知取舍：不认简拼**（`schema.mix.enable_pinyin_abbrev` 开时）。本判据只认全拼音节前缀，
    /// 故简拼中途态若暂无候选仍可能被清空。未做联动是有意的——若一并认 `is_abbreviation`，由于它
    /// 只要求每字母是某音节首字母，几乎任何字母串都会被守护住，清空将形同虚设。现有多个上屏阻止
    /// 选项已能覆盖大部分场景，待真机反馈再定；届时勿只改本函数，须连带重估清空功能的存在意义。
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

    /// 拼音是否**主张**这个超码长串（「这串确实归拼音管」）。两条任一成立即主张：
    /// - `pinyin_may_continue`：还没打完（`youyo` = you + `yo`，`yo` 是合法音节前缀）；
    /// - 拼音首选**解释了整串**（`consumed_length` 覆盖全长；0 = 引擎未标注，按整串算，与
    ///   `is_ambiguous_pinyin_word` 同口径）。简拼串走的正是这一支——`pinyin_may_continue`
    ///   只认全拼音节前缀，对简拼恒 false（见其文档）。
    ///
    /// 反面即「拼音打岔了」：`yijga`（五笔全码 `yijg`=就是 再多打一个字母）拼音只切得出 `yi`、
    /// 余下 `jga` 连音节前缀都不是，首选「以」只消费 2/5 —— 这种串不该由拼音独占。
    ///
    /// 与 `is_ambiguous_pinyin_word` 的分工：那个判「拼音**已经**成词」（看词典权重，用于否决
    /// 上屏），本函数判「拼音**够不够格接管整串**」（看覆盖度，用于超码长归属）。
    fn pinyin_claims_overflow(&self, input: &str) -> bool {
        if self.pinyin_may_continue(input) {
            return true;
        }
        let Some(sec) = &self.secondary else {
            return false;
        };
        let Ok(r) = sec.convert(input, 1) else {
            return false;
        };
        let input_len = input.chars().count();
        r.candidates
            .first()
            .is_some_and(|c| c.consumed_length == 0 || c.consumed_length >= input_len)
    }

    /// 英文是否**主张**这个超码长串：英文词库里有**精确整串**词条（`github` 是完整英文词）。
    ///
    /// 与 [`Self::pinyin_claims_overflow`] 对称 —— 超码长归属问的是「谁解释得了整串」。码表只
    /// 解释得了前 N 码（`gith`=不算），英文却吃得下整个 `github`，归属就不该判给码表。
    ///
    /// ⚠️ 判据刻意用**精确整串**而非 `english_candidates` 的「有候选（含前缀）」：英文库 21918 条，
    /// 前缀面极大，按前缀判会让一堆恰好撞上某英文词开头的五笔全码平白丢掉归属。也不走
    /// `english_candidates` 取候选再比对 —— 那会被 `max_candidates` 截断，精确词未必在前几条。
    ///
    /// ⚠️ **不读 `auto_commit_block_on_english`**：那是**上屏否决**开关（出厂 `false`），本判据决定的
    /// 是**候选归属/排序**，两者正交。若受其支配，默认配置的用户照样会看到 `github` 首选是「不算」。
    fn english_claims_overflow(&self, input: &str) -> bool {
        let Some(eng) = &self.english else {
            return false;
        };
        // 英文词库 code 列已小写化（`type = "english"`），查询侧同口径小写。
        eng.has_full_input_match(&input.to_lowercase())
    }

    /// 超码长时**码表前 N 码是否比拼音/英文更有话说**：⓪ `pinyin_only_overflow` 的例外口，
    /// 顶码（`handle_top_code`）与候选装配（`convert_overflow`）共用同一判据。四条缺一不可：
    /// - 前 N 码前缀恰是码表**精确全码**（`yijg` = 唯一编码「就是」）——只有前缀确实成码才值得
    ///   让码表回来；否则捞回的全是前缀补全候选，纯属刷屏。拼音打错一个字母（`nihxo`）也靠这条
    ///   兜住：`nihx` 在五笔没有精确全码 → 仍归拼音，不会被五笔顶码截胡；
    /// - 拼音并不**主张**这一串（见 [`Self::pinyin_claims_overflow`]）；
    /// - 英文并不**主张**这一串（见 [`Self::english_claims_overflow`]）——开着英文词库时 `words`
    ///   的前 4 码 `word` 若在码表成词，码表精确 `+1e7` 会把英文精确档 `+500K` 整层压掉；
    /// - 拼音至少**交得出候选**（见 [`Self::pinyin_has_any`]）——这条与上面第二条方向相反，
    ///   两头夹出「还在中文语境里、但拼音接管不了整串」这个窄带。
    ///
    /// ⚠️ **前三条的判据是「谁解释得了整串」，第四条问的却是「这串还算不算中文」**，别把它们当成
    /// 一类。真机回归 `github`（英文词库关着）四条里前三条全放行：`gith` 在五笔主库确是精确全码
    /// 「不算」（1822）、`gi` 不成音节所以拼音主张不了、英文引擎压根不在场 —— 于是归属判给码表，
    /// 首选变成「不算」，空格上屏还把整个缓冲吃掉。可这串连开头都解释不出一个字，判给码表毫无
    /// 依据（对比 `yijga` 至少出得来「以」）。第四条即为此而设，落回 249f486 之前的行为：候选
    /// 保持为空，用户空格/回车直接上屏原码。
    ///
    /// ⚠️ 第四条对**顶码通路无影响**：⓪ 的判据是 `pinyin_only_overflow && has_pinyin && !ct_owns`，
    /// `has_pinyin=false` 时整条本就不成立。顶码侧的英文场景另由 ③ `auto_commit_block_on_english`
    /// 负责（出厂 `false`），两者是不同维度，勿混。
    ///
    /// ⚠️ 判据落在**前 N 码前缀**而非整串，这是本函数存在的全部理由：`convert_overflow` 原有的
    /// 逃生口 `has_full_input_match(input) || has_longer_code(input)` 问的是**整串**，而定长码表
    /// （五笔 4 码封顶）里根本不存在 5 码词条 —— 那个条件对五笔恒假，等于没有逃生口，于是
    /// `yijg` + **任意**字母都被拼音「以」整串接管，且关掉全部上屏否决开关也无济于事
    /// （①②③ 与 ⓪ 是独立通路）。真机实测即由此而来。
    fn codetable_owns_overflow(&self, input: &str) -> bool {
        if self.max_code_len == 0 {
            return false;
        }
        if self.english_claims_overflow(input) {
            return false;
        }
        if !self.pinyin_has_any(input) {
            return false;
        }
        let prefix: String = input.chars().take(self.max_code_len).collect();
        self.primary.has_full_input_match(&prefix) && !self.pinyin_claims_overflow(input)
    }

    /// 拼音对这串**交得出候选**（哪怕只解释开头一小截）——「这串还在中文语境里」的最低证据。
    ///
    /// 与 [`Self::pinyin_claims_overflow`] 是**方向相反**的一对：那个问「拼音够不够格接管整串」
    /// （够格就别让码表插手），这个问「拼音是不是连一个字都读不出来」（读不出来说明这串根本不是
    /// 中文码，码表也别硬解释）。`yijga` 出得来「以」→ 归码表；`github` 什么都出不来 → 谁都不接，
    /// 候选留空让用户上屏原码。
    fn pinyin_has_any(&self, input: &str) -> bool {
        self.secondary.as_ref().is_some_and(|sec| {
            sec.convert(input, 1)
                .map(|r| !r.candidates.is_empty())
                .unwrap_or(false)
        })
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

    /// 合并（码表在前、拼音在后）→ 按权重稳定排序 → 按文本去重 → 带拼音保底配额截断。
    fn merge_sort_dedup(
        mut codetable: Vec<Candidate>,
        pinyin: Vec<Candidate>,
        max_candidates: usize,
    ) -> Vec<Candidate> {
        codetable.extend(pinyin);
        Self::sort_dedup_truncate(&mut codetable, max_candidates);
        codetable
    }

    /// 按权重稳定排序 → 按文本去重 → 带拼音保底配额截断（`convert` 主路径与 overflow 共用）。
    fn sort_dedup_truncate(cands: &mut Vec<Candidate>, max_candidates: usize) {
        cands.sort_by(|a, b| {
            b.weight
                .cmp(&a.weight)
                .then(a.natural_order.cmp(&b.natural_order))
        });
        // 按 text 去重，并把被丢弃那条所占的码位并进幸存者（`Candidate::merged_codes`）：
        // 否则「检索范围」过滤按 (source, code) 分组时会丢掉「该码位下有常用字」这一事实，
        // 同一个字打前缀出、打全码反而不出。跨来源（码表 vs 拼音）由 `absorb_codes_from`
        // 自行挡掉——两套编码不同域，并入会造出假的同码关系。
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut deduped: Vec<Candidate> = Vec::with_capacity(cands.len());
        for c in std::mem::take(cands) {
            if let Some(&idx) = seen.get(&c.text) {
                deduped[idx].absorb_codes_from(&c);
                continue;
            }
            seen.insert(c.text.clone(), deduped.len());
            deduped.push(c);
        }
        *cands = deduped;
        Self::truncate_with_pinyin_quota(cands, max_candidates);
    }

    /// 截断到 `max_candidates`，但**保证拼音候选至少留 `max/PINYIN_QUOTA_DIVISOR` 席**。
    ///
    /// **为什么需要**：码表候选带 `PARTIAL_MATCH_BOOST`(500K)、拼音 `÷100`，于是截断时码表恒
    /// 排在前。而五笔 2 码前缀的候选量常常超过整个配额——实测 **52 个 2 码前缀条目数 > 300**
    /// （最多 `kh` 663 条），其中 `pu`（495 条）正是「既是五笔 2 码、又是完整拼音音节」的交集。
    /// 那种输入下拼音候选**一条都进不了列表**，下游协调器的拼音精确档
    /// （`cmp_pinyin_exact_first`）就无从下手——提档提不了不在场的候选。
    ///
    /// **只补不挤空**：尾部确实没有拼音候选时（纯五笔溢出串、`kh` 这类非音节码）`extra` 为空，
    /// 一条码表候选都不会被挤掉，行为与改动前完全一致。
    ///
    /// ⚠️ 补进来的拼音候选**追加在尾部、不保证有序**——这依赖协调器
    /// `candidate_display_order` 会**无条件重排全部候选**（见 candidate-sorting-rules.md §6）。
    /// 本函数的职责只是「让候选进得来」，不是「排好序」。
    fn truncate_with_pinyin_quota(cands: &mut Vec<Candidate>, max_candidates: usize) {
        if cands.len() <= max_candidates {
            return;
        }
        let quota = max_candidates / PINYIN_QUOTA_DIVISOR;
        let is_py = |c: &Candidate| c.source == CandidateSource::Pinyin;
        if quota == 0 {
            cands.truncate(max_candidates);
            return;
        }
        let kept = cands[..max_candidates].iter().filter(|c| is_py(c)).count();
        if kept >= quota {
            cands.truncate(max_candidates);
            return;
        }
        // 从被截掉的部分按序取拼音补足（拼音引擎已按其排序链排好，前几条即精确候选优先）。
        let extra: Vec<Candidate> = cands[max_candidates..]
            .iter()
            .filter(|c| is_py(c))
            .take(quota - kept)
            .cloned()
            .collect();
        cands.truncate(max_candidates);
        // 腾位：从尾部往前挤掉等量的非拼音候选（权重最低的那些）。
        let mut to_remove = extra.len();
        let mut i = cands.len();
        while to_remove > 0 && i > 0 {
            i -= 1;
            if !is_py(&cands[i]) {
                cands.remove(i);
                to_remove -= 1;
            }
        }
        cands.extend(extra);
    }

    /// 组合区**默认**形态（`preedit_display`）：≥2 完成音节且有 preedit 时才用拼音拆分串。
    /// 单音节不拆——纯五笔码更不该被拆（cang 显示 cang，不是 cang）。
    fn pinyin_preedit_of(py: &ConvertResult) -> Option<String> {
        if py.completed_syllables.len() >= 2 && !py.preedit_display.is_empty() {
            Some(py.preedit_display.clone())
        } else {
            None
        }
    }

    /// 拼音拆分形态（`preedit_pinyin` → 协调器 `preedit_split_body`），供**高亮跟随**取用：
    /// 高亮拼音候选时显示它，高亮码表候选时显示原始码（`effective_preedit_body`）。
    ///
    /// 判据是「**拆分串与原始输入不同**」，而非 `pinyin_preedit_of` 的「≥2 完成音节」。
    /// 后者是「默认显示什么」的保守取舍，套到本字段上会漏掉**单音节 + 尾部残码**：
    /// `nunl` = 完成音节 `nun`（稀有音节，见 `syllable.rs` 末尾）+ 残码 `l`，拼音候选「嫩」
    /// 只消费 3 个字符，而编码栏却显示整串 `nunl` —— 候选按 `nun|l` 算、显示按整串算，
    /// 用户看不出引擎已把 `l` 划到音节外，空格上屏残留一个 `l` 便显得无由来。
    ///
    /// 拆分串 == 原始输入时返回 None（`nun`、纯五笔码 `aaaa` 的 `a'a'a'a` 除外——那确实不同，
    /// 但它只在高亮到拼音候选时才显示，那时拆分正是该候选的真实解读）。空串同样返回 None：
    /// 空 = 「无拆分形态」，协调器据此恒用原始码。
    fn pinyin_split_of(py: &ConvertResult, input: &str) -> Option<String> {
        if py.preedit_display.is_empty() || py.preedit_display == input {
            return None;
        }
        Some(py.preedit_display.clone())
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
            let pinyin_split = Self::pinyin_split_of(&py, input);
            let mut pinyin = py.candidates;
            // 英文候选（enable_english 开时）：独立加权档，与拼音/码表统一混入（对齐 Go 各路径处理英文）。
            let english = self.english_candidates(input, max_candidates);
            // 码表回捞（两条互补的口子，任一成立即把码表候选并回来，拼音同时归一化降档避免
            // 档位重叠）：
            // - 长码特例 `has_full_or_longer`：**整串**在码表有精确匹配/更长后继。只有码长可变
            //   的码表够得着——五笔这类定长码表恒假（4 码封顶，不存在 5 码词条）。
            // - `codetable_owns_overflow`：**前 N 码**是精确全码而拼音并不主张这一串
            //   （`yijg`+任意字母）。这条才是定长码表的逃生口，与顶码 ⓪ 共用判据。
            let ct_owns = self.codetable_owns_overflow(input);
            let mut merged = if has_full_or_longer || ct_owns {
                Self::normalize_pinyin(&mut pinyin);
                let mut ct = if has_full_or_longer {
                    let mut full = self
                        .primary
                        .convert(input, max_candidates)
                        .unwrap_or_default()
                        .candidates;
                    self.boost_codetable(&mut full, input);
                    full
                } else {
                    // 前 N 码前缀候选：前缀视作精确全码加权（同混合 overflow 分支的口径），
                    // 但 `is_exact_code` 归一到**完整输入** —— 前缀恒短于 input，故一律 false，
                    // 免得下游（协调器 `candidate_display_order` / `freq_rerank`）把只匹配
                    // 前缀的候选当成本次输入的精确匹配提拔进精确档。
                    let prefix: String = input.chars().take(self.max_code_len).collect();
                    let mut pre = self
                        .primary
                        .convert(&prefix, max_candidates)
                        .unwrap_or_default()
                        .candidates;
                    for c in &mut pre {
                        c.is_exact_code = false;
                        // ★ 这条候选只解释得了**前 N 码**，必须如实标注消费长度。不标（码表候选
                        // 恒 0）的话协调器 `commit_selected` 的
                        // `partial = consumed > 0 && consumed < total` 恒为 false ⇒ 按「消费整串」
                        // 处理，选中即把没解释的尾码一并吃掉（`yijga` 选「就是」→ 尾巴上的 `a`
                        // 凭空消失；`github` 选「不算」→ `ub` 消失）。
                        //
                        // ⚠️ 这是**码表候选带 `consumed_length` 的唯一出口**。协调器侧有两处判据
                        // 原本依赖「码表恒 0 ⇒ 永不部分匹配」这个不变量，已随本改动一并对齐：
                        // `build_candidates` 的分段续转（改看最后一段来源）与
                        // `learn_phrase_on_commit`（混输下跳过码表段）。
                        //
                        // 字节长度：协调器按字节切缓冲（`input_buffer[consumed..]` +
                        // `is_char_boundary`），而输入缓冲在此恒为 ASCII 码字符，与字符数相等。
                        c.consumed_length = prefix.len();
                    }
                    self.boost_codetable(&mut pre, &prefix);
                    pre
                };
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
                preedit_pinyin: pinyin_split.unwrap_or_default(),
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
            // `is_exact_code` 归一到**完整输入**：上面两次 convert 分别以 prefix 和 input 为输入，
            // 码表引擎按各自的输入串置位，于是同一个 Vec 里混着两种「精确」定义。而下游一律以
            // 完整输入为准（协调器 `candidate_display_order`、`freq_rerank::freq_tier` 的
            // `code == input`），不归一会让只匹配前缀的候选被提拔进精确档。
            // 注意与紧随其后的 `boost_codetable(.., &prefix)` 判据不同：那是混输自身的权重档策略
            // （前 N 码视作全码加权），与「候选编码是否等于本次输入」是两回事，不可合并。
            for c in &mut codetable {
                c.is_exact_code = c.code == input;
            }
            self.boost_codetable(&mut codetable, &prefix);
            // 英文候选（enable_english 开时）：独立加权档并入码表位，与拼音一同竞争。
            codetable.extend(self.english_candidates(input, max_candidates));
            let py = sec.convert(input, max_candidates).unwrap_or_default();
            let pinyin_preedit = Self::pinyin_preedit_of(&py);
            let pinyin_split = Self::pinyin_split_of(&py, input);
            let mut pinyin = py.candidates;
            Self::normalize_pinyin(&mut pinyin);
            let mut merged = Self::merge_sort_dedup(codetable, pinyin, max_candidates);
            if self.show_source_hint {
                Self::add_source_hints(&mut merged);
            }
            let is_empty = merged.is_empty();
            ConvertResult {
                candidates: merged,
                preedit_pinyin: pinyin_split.unwrap_or_default(),
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
        // 主码表的精确空码补全备选原样上浮：混输合并后仍可能一条候选都没有（拼音也未命中），
        // 那时才由协调器采纳。此处若就地并入 `codetable` 会重蹈引擎自行判空的覆辙——拼音候选
        // 尚未合入，这一层的「空」同样不是最终的空。见 `ConvertResult::completion_hint`。
        let ct_completion_hint = ct.completion_hint;
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
        // 高亮跟随用的拆分形态：判据比上面宽（见 `pinyin_split_of`），单音节 + 残码也提供。
        let mut pinyin_split: Option<String> = None;
        if input_len >= self.min_pinyin_length {
            if let Some(sec) = &self.secondary {
                if let Ok(py) = sec.convert(input, max_candidates) {
                    pinyin_preedit = Self::pinyin_preedit_of(&py);
                    pinyin_split = Self::pinyin_split_of(&py, input);
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
        // 排序 → 去重 → 带拼音保底配额截断（与 overflow 路径共用，见 `sort_dedup_truncate`）。
        Self::sort_dedup_truncate(&mut merged, max_candidates);
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

        // 满码空码清空：主码表请求清空 + 拼音守护未拦截。
        //
        // 两道守护，**同受 `auto_commit_block_on_pinyin` 支配**（这是第四条「拼音让路」通路，
        // 与 `convert` 满码上屏 / `recheck_auto_commit` 显示态复评 / `handle_top_code` 顶码同源）：
        // - `has_pinyin`：拼音此刻已出候选 → 留给拼音（粗粒度，且合并后非空，协调器亦会复核）；
        // - `pinyin_may_continue`：拼音**还没打完** → 保护中途态。这一项才是无候选时的关键守护：
        //   如 zhon（码表满码无候选无后继、拼音此刻也无候选）合并结果为空，协调器的
        //   `state.candidates.is_empty()` 复核挡不住，若不看后续可能性就会把用户正在输入的
        //   zhong 吞掉。经 `&&` 短路，仅在码表确有清空意向且守护开时才查音节表。
        //
        // ⚠️ 两道**必须一起**受开关支配，只放开 `has_pinyin` 等于没放开：`nunl` 这类
        // 「完整音节 + 单个声母字母」串即便词库里一条候选都没有，`pinyin_may_continue` 仍判
        // 「还没打完」（单字母恒是某音节前缀）而独立拦住清空。见
        // `clear_still_vetoed_even_without_the_nun_entry`。
        //
        // 关闭该开关**不会**牺牲「拼音还没打完」的中途态——那由协调器的第三道门
        // （`clear_blocked_by_candidates`）按候选实际形态兜住，比本处的音节表推测精确得多：
        // 真实词库下 `wanl` 出的是前缀补全候选（code=`wanle`，消费整串）→ 拦住清空，
        // 用户接着打 `wanle` 不会被吞；`zhon`(→zhong 系列) 同理。真正会被清空的只有
        // 「候选全是部分匹配」的串（`nunl` 的「嫩」只解释了 `nun`），即确实打岔了的那些。
        // 实测见 `input_flow.rs` 的 `..._clears_when_only_partial_pinyin` /
        // `..._keeps_prefix_completion_candidates` 单一变量对照。
        let pinyin_guards_clear =
            self.auto_commit_block_on_pinyin && (has_pinyin || self.pinyin_may_continue(input));
        let should_clear = ct_should_clear && !pinyin_guards_clear;

        let is_empty = merged.is_empty();
        Ok(ConvertResult {
            candidates: merged,
            // 组合区：多音节拼音用音节分隔（ni'hao），否则原始码（五笔为主，简明）。
            // 拼音拆分形态单独留存，供协调器「按高亮候选类型」选择显示原始码 / 拆分串——
            // 它的判据比 preedit_display 宽（单音节 + 残码也给），见 `pinyin_split_of`。
            preedit_pinyin: pinyin_split.unwrap_or_default(),
            preedit_display: pinyin_preedit.unwrap_or_else(|| input.to_string()),
            is_empty,
            should_commit,
            commit_text,
            should_clear,
            completion_hint: ct_completion_hint,
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
    /// - ⓪ `pinyin_only_overflow` 且整串有拼音候选 → 超码长即纯拼音语境，抑制顶码（见下）。
    ///   例外：`codetable_owns_overflow`（前 N 码是精确全码 + 拼音主张不了整串）时放行；
    /// - ① `auto_commit_block_on_pinyin` 且整串有拼音候选 → 抑制顶码（打开时 wangba/aipu 等含拼音
    ///   读法的串都让路拼音）；
    /// - ② `block_commit_on_pinyin_word` 且整串是强拼音词（wangba→网吧）→ 抑制顶码；
    /// - ③ `auto_commit_block_on_english` 且整串有英文候选 → 抑制顶码（github→GitHub，见下）；
    /// - `top_code_override_pinyin` 开启 = 顶码优先，**无视**上述全部否决强制倒向五笔。
    ///   （该名字只提 pinyin 属历史局限，它实际是顶码总开关，⓪①②③ 一律受其压制。）
    ///
    /// ⓪ 与 [`Self::convert`] 的超长分流**共用同一个判据**（`input_len > max_code_len` +
    /// `pinyin_only_overflow`）。此前本函数完全不读 `pinyin_only_overflow`，于是同一次按键里
    /// `convert` 判「切入纯拼音语境」、`handle_top_code` 却委托纯码表顶掉前 N 码；而协调器
    /// （`coordinator.rs` 字母键臂）让顶码**先于**候选刷新执行 → 顶码恒赢，`convert_overflow`
    /// 的纯拼音分支只在拼音否决①②恰好命中时才够得着。混输下打 `youyoud`（悠悠的）在第 5 键
    /// 被顶出「变凉」+ 余码 `oud` 即此漏的实例。
    ///
    /// 与码表侧判据天然互补，不重复拦截：`CodeTableEngine::handle_top_code` 仅在整串**既无精确
    /// 匹配也无更长后继**时才返回 Some，而 `convert_overflow` 的「长码特例」（`has_full_or_longer`）
    /// 恰是它的补集 —— 顶码想触发的那些串，在 overflow 侧走的正是纯拼音分支。
    fn handle_top_code(&self, input: &str) -> Option<(String, String)> {
        let input_len = input.chars().count();
        if self.max_code_len == 0 || input_len <= self.max_code_len {
            return self.primary.handle_top_code(input);
        }
        // 顶码优先开关关闭时，应用 ⓪③ 与满码同一套拼音①②否决。
        if !self.top_code_override_pinyin {
            // ③ 英文守护：与满码上屏（`convert`）/ 显示态复评（`recheck_auto_commit`）**同一个
            // 开关**，补齐第三条上屏通路。此前 `auto_commit_block_on_english` 全仓只有那两个
            // 使用点，顶码一个都没有 —— 用户开了「有英文候选时否决上屏」，打 github 到第 5 键
            // 仍被顶出五笔词「不算」（`gith` 在主码表有词），与 ⓪ 同构的漏。
            //
            // 自带防卡死，无需 ⓪ 那样的额外条件：判据要求「英文确有候选」，而它与
            // `convert_overflow` 调的是同一个 `english_candidates`、同一个 `input` ——
            // 拦下顶码后 overflow 必然交得出那批候选。
            //
            // ⚠️ 刻意放在下方 `Some(sec)` 块**之外**：英文守护与拼音子引擎无关，
            // 纯码表 + 英文的混输（secondary=None）同样该生效。
            if self.auto_commit_block_on_english && !self.english_candidates(input, 1).is_empty() {
                return None;
            }
            if let Some(sec) = &self.secondary {
                // ①的 has_pinyin：整串是否有拼音候选（与满码"合并前拼音候选非空"同义）。
                let has_pinyin = sec
                    .convert(input, 1)
                    .map(|r| !r.candidates.is_empty())
                    .unwrap_or(false);
                // ⓪ 超码长仅查拼音：本串已归拼音管，只要拼音真给得出候选，顶码就不该抢。
                //
                // **必须叠 `has_pinyin`，不可只看开关**：纯五笔溢出串（aaaab 之类，拼音一条
                // 候选都没有）若也禁顶码，`convert_overflow` 的纯拼音分支同样交不出候选——
                // 用户会卡在一个既不上屏、又没候选的长串上，没有出口。
                //
                // **例外口 `codetable_owns_overflow`**：前 N 码是精确全码而拼音只解释得了开头
                // 一小截（`yijg`=就是，再打任意字母 → 拼音只切出 `yi`，余下连音节前缀都不是）。
                // 这种串归码表，放行顶码。没有这个例外时 ⓪ 是一票独占：用户把 ①②③ 全关也
                // 改变不了结果——它们是彼此独立的通路，⓪ 只受 `top_code_override_pinyin` 压制。
                // 不必担心卡死：例外成立⇒前缀确有精确全码，码表侧顶码必给得出结果；即便码表侧
                // 因整串有更长后继而返回 None，`convert_overflow` 的长码特例也照样交得出候选。
                //
                // 与 ① 的分工：① 不限超码长（满码时同样生效）、由 `auto_commit_block_on_pinyin`
                // 驱动；⓪ 只在本函数成立（此处已确认 `input_len > max_code_len`）、由
                // `pinyin_only_overflow` 驱动。两者独立配置，任一命中即否决。
                if self.pinyin_only_overflow && has_pinyin && !self.codetable_owns_overflow(input) {
                    return None;
                }
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

    // ── 截断的拼音保底配额（`truncate_with_pinyin_quota`）──

    fn ct_cand(text: &str, weight: i32) -> Candidate {
        Candidate {
            text: text.into(),
            weight,
            source: CandidateSource::CodeTable,
            ..Default::default()
        }
    }

    fn py_cand(text: &str, weight: i32) -> Candidate {
        Candidate {
            text: text.into(),
            weight,
            source: CandidateSource::Pinyin,
            ..Default::default()
        }
    }

    /// 复刻 `pu`（495 条码表 + 拼音）现场：码表候选多到吃满整个配额，拼音一条都进不来。
    /// 保底后拼音应拿到 `max/PINYIN_QUOTA_DIVISOR` 席。
    #[test]
    fn pinyin_gets_minimum_quota_when_codetable_floods() {
        let mut cands: Vec<Candidate> = (0..20)
            .map(|i| ct_cand(&format!("码{i}"), 500_000))
            .collect();
        cands.extend((0..5).map(|i| py_cand(&format!("拼{i}"), 100 - i)));
        MixedEngine::truncate_with_pinyin_quota(&mut cands, 10);
        assert_eq!(cands.len(), 10, "总数仍受 max_candidates 约束");
        let py = cands
            .iter()
            .filter(|c| c.source == CandidateSource::Pinyin)
            .count();
        assert_eq!(py, 2, "10/5=2 席保底（否则协调器的拼音精确档无候选可提）");
        // 挤掉的是权重最低的码表候选，码表仍占多数。
        assert_eq!(cands.len() - py, 8);
    }

    /// ★ 反向锁：尾部**没有**拼音候选时（`kh` 663 条这类非音节码、纯五笔溢出串），
    /// 一条码表候选都不许被挤掉 —— 行为与改动前完全一致。
    #[test]
    fn no_pinyin_means_no_codetable_is_evicted() {
        let mut cands: Vec<Candidate> = (0..20)
            .map(|i| ct_cand(&format!("码{i}"), 500_000))
            .collect();
        MixedEngine::truncate_with_pinyin_quota(&mut cands, 10);
        assert_eq!(cands.len(), 10);
        assert!(
            cands.iter().all(|c| c.source == CandidateSource::CodeTable),
            "无拼音可补时不得腾位"
        );
        assert_eq!(cands[0].text, "码0", "顺序不应被打乱");
    }

    /// 未超上限时原样不动（不触发任何腾位逻辑）。
    #[test]
    fn under_limit_is_untouched() {
        let mut cands = vec![ct_cand("码", 500_000), py_cand("拼", 69)];
        MixedEngine::truncate_with_pinyin_quota(&mut cands, 10);
        assert_eq!(cands.len(), 2);
    }

    /// 拼音本就够席位时不额外腾位（避免把配额当成"必须凑满"的硬指标）。
    #[test]
    fn existing_pinyin_above_quota_needs_no_eviction() {
        let mut cands: Vec<Candidate> = (0..5)
            .map(|i| py_cand(&format!("拼{i}"), 900 - i))
            .collect();
        cands.extend((0..20).map(|i| ct_cand(&format!("码{i}"), 100)));
        MixedEngine::truncate_with_pinyin_quota(&mut cands, 10);
        let py = cands
            .iter()
            .filter(|c| c.source == CandidateSource::Pinyin)
            .count();
        assert_eq!(py, 5, "前 10 条里已有 5 条拼音 ≥ 配额 2，不动");
        assert_eq!(cands.len(), 10);
    }

    /// 接线验证：配额逻辑必须真的挂在 `convert` 的截断上，不能只是个没人调的函数
    /// （「函数写对了但生产端不调」是本仓反复出现的欠账形态）。
    #[test]
    fn convert_applies_pinyin_quota() {
        // 码表 20 条同码候选（权重高，会吃满配额）；拼音 5 条。
        let entries: Vec<(String, String, i32)> = (0..20)
            .map(|i| ("aa".to_string(), format!("码{i}"), 9000 - i))
            .collect();
        let refs: Vec<(&str, &str, i32)> = entries
            .iter()
            .map(|(c, t, w)| (c.as_str(), t.as_str(), *w))
            .collect();
        let e = MixedEngine::new(
            ct_engine(&refs, false),
            Some(Box::new(FakePinyinMulti { n: 5 })),
            None,
            MixConfig::default(),
        );
        let r = e.convert("aa", 10).unwrap();
        assert_eq!(r.candidates.len(), 10);
        let py = r
            .candidates
            .iter()
            .filter(|c| c.source == CandidateSource::Pinyin)
            .count();
        assert_eq!(py, 2, "convert 必须走带配额的截断");
    }

    /// 产出多条 `source=Pinyin` 候选的假拼音引擎（`FakePinyin` 只给一条，测不了配额）。
    struct FakePinyinMulti {
        n: usize,
    }
    impl Engine for FakePinyinMulti {
        fn convert(&self, input: &str, _max: usize) -> anyhow::Result<ConvertResult> {
            let candidates = (0..self.n)
                .map(|i| Candidate {
                    text: format!("拼{i}"),
                    code: input.to_string(),
                    weight: 100 - i as i32,
                    source: CandidateSource::Pinyin,
                    ..Default::default()
                })
                .collect();
            Ok(ConvertResult {
                candidates,
                ..Default::default()
            })
        }
        fn reset(&self) {}
        fn engine_type(&self) -> EngineType {
            EngineType::Pinyin
        }
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
        assert!(
            !r.should_clear,
            "拼音仍可能有后续时不得清空，否则吞掉中途输入"
        );
    }

    /// 开关关 → 拼音「还没打完」不再拦清空。用户明确关掉「有拼音候选时否决上屏」即表态
    /// 不要拼音干预，此时满码无候选就该清空（真机诉求：nunl 打满 4 码不清空）。
    #[test]
    fn clear_fires_when_pinyin_guard_disabled() {
        let e = MixedEngine::new(
            ct_engine_clear(&[("aaaa", "工", 100)]),
            Some(Box::new(FakePinyinPrefix { may_continue: true })),
            None,
            MixConfig {
                auto_commit_block_on_pinyin: false,
                ..Default::default()
            },
        );
        let r = e.convert("zhon", 50).unwrap();
        assert!(
            r.should_clear,
            "① 关时拼音后续可能性不得再拦清空（用户已表态不要拼音干预）"
        );
    }

    /// 开关关 + 拼音**确有候选** → 同样清空。锁住「两道守护一起受开关支配」，
    /// 只放开其中一道等于没放开（nunl 即便无候选也会被 may_continue 拦住）。
    #[test]
    fn clear_fires_when_guard_disabled_even_with_pinyin_candidates() {
        let e = MixedEngine::new(
            ct_engine_clear(&[("aaaa", "工", 100)]),
            Some(Box::new(FakePinyin {
                word: "嫩",
                syllables: 1,
            })),
            None,
            MixConfig {
                auto_commit_block_on_pinyin: false,
                ..Default::default()
            },
        );
        let r = e.convert("nunl", 50).unwrap();
        assert!(
            r.candidates.iter().any(|c| c.text == "嫩"),
            "前置：拼音此刻确有候选"
        );
        assert!(r.should_clear, "① 关时有拼音候选也不得拦清空");
    }

    /// 反向锁：开关**开**（出厂默认）时两道守护照常拦住，勿把上面两例误改成无条件清空。
    #[test]
    fn clear_still_vetoed_when_guard_enabled() {
        let e = MixedEngine::new(
            ct_engine_clear(&[("aaaa", "工", 100)]),
            Some(Box::new(FakePinyinPrefix { may_continue: true })),
            None,
            MixConfig {
                auto_commit_block_on_pinyin: true,
                ..Default::default()
            },
        );
        assert!(
            !e.convert("zhon", 50).unwrap().should_clear,
            "① 开时中途态必须守住（zhon→zhong 不得被吞）"
        );
    }

    #[test]
    fn overflow_never_clears() {
        // 超长（>max_code_len）**有意**不清空：已切入纯拼音语境，「码表满码无候选」前提不成立。
        let r = mixed_with_prefix_pinyin(false)
            .convert("qqqqq", 50)
            .unwrap();
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
        // 默认下 ⓪ 亦为开，此例同时守着它的 `has_pinyin` 前提（详见
        // `topcode_allowed_when_overflow_has_no_pinyin`）。
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
        // ⓪ 须显式关以隔离变量——`MixConfig::default()` 的 pinyin_only_overflow 为 true，
        // 而本串有拼音候选，不关掉的话拦截来自 ⓪ 而非被测的 ①②。
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
                pinyin_only_overflow: false,
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
        // ⓪ 须显式关以隔离变量（理由同 `topcode_allowed_when_both_guards_off`）。
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
                pinyin_only_overflow: false,
                ..Default::default()
            },
        );
        assert_eq!(
            e.handle_top_code("aipux"),
            Some(("落实".to_string(), "x".to_string())),
            "① 关 + ② 开：多音节前缀无强词应放行顶码"
        );
    }

    // ── ⓪ pinyin_only_overflow：超码长归拼音管，顶码不得抢 ──

    #[test]
    fn topcode_vetoed_by_pinyin_only_overflow() {
        // 与 `topcode_allowed_when_both_guards_off` 构成**单一变量对照**：同样的码表/假拼音/
        // 输入串、同样 ①② 都关，唯一差别是 ⓪ 开 → 拦截只可能来自 ⓪。
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
                pinyin_only_overflow: true,
                ..Default::default()
            },
        );
        assert_eq!(
            e.handle_top_code("wangba"),
            None,
            "⓪ 开 + 有拼音候选：超码长归拼音管，顶码不得抢"
        );
    }

    #[test]
    fn topcode_pinyin_only_overflow_protects_youyoud() {
        // 真机回归（用户实测）：混输下打 `youyoud`（悠悠的），第 5 键 `o` 使缓冲 "youyo" 超 4 码
        // → 旧实现顶出 `youy` 的首选「变凉」+ 余码 `oud`。
        //
        // 本例精确复刻当时 ①② 双双落空的判据状态，故只有 ⓪ 能救：
        // - ① 关（用户层 auto_commit_block_on_pinyin=false 覆盖了系统层的 true）；
        // - ②(b) 落空：前 4 码 "youy" = you + 残尾 y，不是完整音节（syllables=2 使
        //   completed_syllable_count != 1）；
        // - ②(a) 落空：整串 "youyo" 拼不出「≥2 汉字」的强词（word 只 1 字）。
        let primary = ct_engine_topcode(&[("youy", "变凉", 864)]);
        let e = MixedEngine::new(
            primary,
            Some(Box::new(FakePinyin {
                word: "悠",
                syllables: 2,
            })),
            None,
            MixConfig {
                auto_commit_block_on_pinyin: false,
                pinyin_only_overflow: true,
                ..Default::default()
            },
        );
        assert_eq!(
            e.handle_top_code("youyo"),
            None,
            "①② 落空时 ⓪ 应兜住：超码长的拼音串不得被五笔顶码截胡"
        );
    }

    #[test]
    fn topcode_allowed_when_overflow_has_no_pinyin() {
        // ⓪ 开但整串**无**拼音候选（纯五笔溢出）→ 必须放行顶码。
        // 一刀切禁顶会让用户卡死：convert_overflow 此时只查拼音，同样交不出候选，
        // 那串既不上屏也没候选，没有出口。
        let primary = ct_engine_topcode(&[("aaaa", "工", 100)]);
        let e = MixedEngine::new(
            primary,
            Some(Box::new(FakePinyin {
                word: "",
                syllables: 0,
            })),
            None,
            MixConfig {
                pinyin_only_overflow: true,
                ..Default::default()
            },
        );
        assert_eq!(
            e.handle_top_code("aaaab"),
            Some(("工".to_string(), "b".to_string())),
            "⓪ 开但无拼音候选：纯五笔溢出应正常顶码"
        );
    }

    #[test]
    fn topcode_override_beats_pinyin_only_overflow() {
        // top_code_override_pinyin 是总开关，压过 ⓪ 与 ①②。
        let primary = ct_engine_topcode(&[("wang", "王", 100)]);
        let e = MixedEngine::new(
            primary,
            Some(Box::new(FakePinyin {
                word: "网吧",
                syllables: 2,
            })),
            None,
            MixConfig {
                pinyin_only_overflow: true,
                top_code_override_pinyin: true,
                ..Default::default()
            },
        );
        assert_eq!(
            e.handle_top_code("wangba"),
            Some(("王".to_string(), "ba".to_string())),
            "顶码优先应无视 ⓪"
        );
    }

    // ── ③ auto_commit_block_on_english：有英文候选时顶码不得抢 ──

    /// 真机场景：`gith` 在五笔主码表是「不算」，英文词库有 GitHub。打 github 到第 5 键 `u`
    /// 时缓冲 `githu` 超 4 码，旧实现顶出「不算」+ 余码 `u`。
    /// **secondary=None 是刻意的**：一并锁住「③ 必须在 `Some(sec)` 块之外」——英文守护与
    /// 拼音子引擎无关，纯码表 + 英文的混输同样该生效。
    #[test]
    fn topcode_vetoed_by_english_candidate() {
        let primary = ct_engine_topcode(&[("gith", "不算", 1822)]);
        let english = english_engine(&[("github", "GitHub", 100)]);
        let e = MixedEngine::new(
            primary,
            None,
            Some(english),
            MixConfig {
                auto_commit_block_on_english: true,
                ..Default::default()
            },
        );
        assert_eq!(
            e.handle_top_code("githu"),
            None,
            "③ 开 + 有英文候选：顶码不得抢（且无拼音子引擎时也须生效）"
        );
    }

    #[test]
    fn topcode_allowed_when_no_english_candidate() {
        // ③ 开但整串无英文候选 → 顶码正常（判据要求英文确有候选，不是开关一开就禁）。
        let primary = ct_engine_topcode(&[("gith", "不算", 1822)]);
        let english = english_engine(&[("hello", "hello", 50)]);
        let e = MixedEngine::new(
            primary,
            None,
            Some(english),
            MixConfig {
                auto_commit_block_on_english: true,
                ..Default::default()
            },
        );
        assert_eq!(
            e.handle_top_code("githu"),
            Some(("不算".to_string(), "u".to_string())),
            "③ 开但无英文候选：顶码应正常"
        );
    }

    #[test]
    fn topcode_english_guard_off_allows_topcode() {
        // ③ 关（出厂默认）→ 即便有英文候选也顶码，保持零回归。
        let primary = ct_engine_topcode(&[("gith", "不算", 1822)]);
        let english = english_engine(&[("github", "GitHub", 100)]);
        let e = MixedEngine::new(
            primary,
            None,
            Some(english),
            MixConfig {
                auto_commit_block_on_english: false,
                ..Default::default()
            },
        );
        assert_eq!(
            e.handle_top_code("githu"),
            Some(("不算".to_string(), "u".to_string())),
            "③ 关时应保持旧行为"
        );
    }

    #[test]
    fn topcode_override_beats_english_guard() {
        // top_code_override_pinyin 是顶码总开关，压过 ③（名字只提 pinyin 属历史局限）。
        let primary = ct_engine_topcode(&[("gith", "不算", 1822)]);
        let english = english_engine(&[("github", "GitHub", 100)]);
        let e = MixedEngine::new(
            primary,
            None,
            Some(english),
            MixConfig {
                auto_commit_block_on_english: true,
                top_code_override_pinyin: true,
                ..Default::default()
            },
        );
        assert_eq!(
            e.handle_top_code("githu"),
            Some(("不算".to_string(), "u".to_string())),
            "顶码优先应无视 ③"
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
