//! 拼音输入引擎
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/` 对齐。
//!
//! 候选生成流程（对齐 Go convertCore）：
//! 1. 精确查找（完整音节 join 无空格）
//! 2. Viterbi 长句解码（>=2 音节）
//! 3. DAG 子短语查找
//! 4. 前缀查找
//! 5. 缩写/简拼匹配
//!
//! 注意：运行时词频 boost 由上层（协调器）应用，本引擎只产出基础权重候选。

pub mod dag;
pub mod fuzzy;
pub mod generate;
pub mod interp;
pub mod lattice;
pub mod lm;
pub mod mixed_abbrev;
pub mod parser;
pub mod scorer;
pub mod shuangpin;
pub mod syllable;
pub mod viterbi;

use crate::engine::{ConvertResult, Engine, EngineType};
use dag::{Dag, SegGraph};
use fuzzy::FuzzyConfig;
use generate::CharPinyinIndex;
use lattice::LatticeBuilder;
use lm::UnigramLookup;
use scorer::AbbrevMatcher;
use shuangpin::ShuangpinConverter;
use std::sync::{Arc, OnceLock};
use syllable::SyllableTrie;
use viterbi::{ViterbiDecoder, WordNode};
use wind_candidate::{Candidate, CandidateSource};
use wind_dict::DictManager;
use wind_dict::cached::CachedDict;

/// 整句候选权重基准（高于拼音词频上限 ~19260817，确保整句置顶且不被截断）
const SENTENCE_WEIGHT_BASE: i32 = 30_000_000;

/// 模糊音命中的权重折扣（对齐 Go `ranker.go` 的 `IsFuzzy → score -= 100`）。
///
/// **为何是惩罚而非层级**：模糊命中是「召回来源」，不是「匹配质量」——`si` 经 s↔sh 命中的
/// 「是」在音节结构上与精确命中的「四」完全对齐，两者本就该同层按权重竞争。此前 `is_fuzzy`
/// 是 `cmp_match_layers` 的首要键（等价于惩罚 ∞），真实词典下打 `si` 时「是」落在第 231 位、
/// 打 `zong` 时「中」落在第 158 位，而生产候选上限仅 50（临拼/混输）~300（拼音方案），
/// 模糊音在全部三条路径上等价于未实现。
///
/// **为何用乘性而非 Go 的加性常数**：Go 的分数是归一化后的加权和（音节对齐 +500、用户词 +300、
/// 词频仅 ×0.00001），而本侧 weight 直接就是词频量纲且跨来源差异极大（词典词 ~1e2、
/// 前缀补全可达 2e9、整句 3e7）。固定减法在不同量纲上效果天差地别，乘性折扣则量纲无关。
///
/// **取值依据（build_dev 真实词库实测，非估算）**：汉语单字词频跨数量级，同音字之间常差
/// 1~2 个量级——「是」=1799848 vs「四」=22625（80 倍），「中」=497871 vs「总」=20874（24 倍）。
/// 要让精确命中守住首选位，折扣必须小于二者之比，即 `si` 需 <0.013、`zong` 需 <0.042。
/// 取 **0.01** 同时满足两者并留余量；实测下 `si`→「四 死 是\* 斯」、`zong`→「总 中\* 纵」，
/// 精确守首位而模糊命中仍稳定落在首屏可见区（第 2~3 位），这正是模糊音要的效果。
///
/// 更大的值（如 0.5）会让模糊高频字直接夺走首选位（`si`→「是\* 时\* 四」），等于把
/// 「我分不清 s/sh」曲解成「我要的就是 sh」。
///
/// **本常数不作用于整句路径**：Viterbi 整句拿 [`SENTENCE_WEIGHT_BASE`] (3e7) 基准分，
/// 与词频量纲差几个数量级，任何比例折扣都压不下来。模糊整句改走 step 6.5 的
/// `is_sentence_demoted` 降级（降到精确整词之下），见该处注释。
const FUZZY_WEIGHT_SCALE: f64 = 0.01;

/// 对模糊命中施加权重折扣，见 [`FUZZY_WEIGHT_SCALE`]。
/// 饱和到 `>= 1`：折扣不该把候选压成 0/负权重而改变它与「无权重」候选的关系。
fn fuzzy_penalized(weight: i32) -> i32 {
    if weight <= 1 {
        return weight;
    }
    ((weight as f64) * FUZZY_WEIGHT_SCALE).round().max(1.0) as i32
}

/// 裸声母（无完整音节，如 "m"）单字提权：使单字候选（吗/么）排在多字前缀补全词
/// （没有/目前）之前——对齐主流输入法「首字优先」。取 1e7：高于常规词频（单字基础权重上限
/// ~2e6），稳压多字词。（历史注记：此值原本还须刻意低于 freq_rerank 的 2e7 阈值以免被误当
/// 整句锚定——该阈值已改为按 `Candidate::is_sentence` 标记判定，此处不必再避让任何数值线。）
/// 提权改的是 weight，故能穿过协调器按权重的重排（否则引擎内单字优先会被 build_candidates
/// 重排冲掉）。仅裸声母（syllables 为空）时应用——完整音节输入的单字已靠 is_prefix 层级就位。
const BARE_INITIAL_SINGLE_CHAR_BOOST: i32 = 10_000_000;

/// 残码补全的「近距离」上限（音节数）：补全结果比已完成音节多出不超过此数时，
/// 视为「补完手头正在输入的这个音节（及紧随的一两个）」，置信度天然高，无条件上浮。
///
/// 取 2 而非 1 有实测依据：`beijingd`→「北京大学」、`jisuanjik`→「计算机科学」都是 +2，
/// 若取 1 会直接干掉这类极常见场景。
const COMPLETION_NEAR_SYLLABLES: u32 = 2;

/// 远距离补全的权重门槛：超出近距离的补全属于「预测用户尚未输入的内容」，
/// 需足够高频才配上浮，否则沉回前缀补全层级（仍在候选中，只是排到精确匹配之后）。
///
/// 门槛落在实测数据的空隙里——合理项最低是「中国人民解放军」w=252（`zhongguorenm`，距离 +4）
/// 与「你好吗」w=166（距离 +1，本就走近距离豁免）；噪音项（`zhonghuarenmingongheg` 前缀下
/// 的「中华人民共和国XXX法」条文名）最高 w=60。60~166 之间取 100，双向都有余量。
///
/// 注意不能对近距离补全也套这道门槛：词库 weight_spec 的 median 仅 200，
/// 一半的词低于它，会误沉大量高频使用但低词频的日常词。
const COMPLETION_FAR_WEIGHT_FLOOR: i32 = 100;

/// 前缀补全的取数上限（见 convert 中 `completion_limit` 处的完整说明）。
///
/// 取 1000 是 `push_unique` 的 O(n²) 查重与「单字母可持续翻页」之间的平衡点：
/// 实测 1000 条约 3.5ms，5000 条则达 54ms。1000 条按每页 9 项可翻 110 页，
/// 远超实用范围；该上限只约束**补全**，精确匹配候选不受影响。
const MAX_COMPLETION_CANDIDATES: usize = 1000;

/// 混合简拼查 `AbbrevSection` 时的取码上限（step 5b）。
///
/// 比纯简拼的 10 大一截：纯简拼那边「键即答案」，按权重取前 10 条就是最终候选；混合路径
/// 拿到的码还要过一道逐段校验（`nh` 下的 `nihao`/`nanhai`/`naihe`… 只有第二段等于 `hao`
/// 的能活下来），**绝大多数会被滤掉**，取 10 条几乎必然一条不剩。
/// 索引点查本身是 DAT 前缀走位 + 定长条目读取，放宽到 64 的成本远小于「查了等于没查」。
const MIXED_ABBREV_INDEX_LIMIT: usize = 64;

/// 混合整句的**质量闸门**：路径平均每字 log_prob 低于此值就不插入候选。
///
/// 为什么需要它：整句靠 [`SENTENCE_WEIGHT_BASE`] (3e7) 无条件置顶，而混合整句的简拼段
/// 歧义极大——`bzd` 要在 12 个同简拼词里选，靠 unigram 常常选错。没有闸门时**错误整句
/// 100% 占据首选**，连原本可用的部分候选（分步上屏的「不知道」）都被顶掉。
/// 调 `ABBREV_NODE_PENALTY` 治不了这个：`log_offset` 那点差异撼动不了 3e7 的基准
/// （实测 1.2 / 2.5 / 4.0 三档，D 类指标几乎不动）。
///
/// **取值依据：D 类评测的受控对比（常用词口径，n=1000）**，不是估算：
///
/// | 阈值 | 整句 top-1 | 首词命中 |
/// |---|---|---|
/// | 不设闸门 | 12.10% | 0.00% |
/// | **-8.0（本值）** | **12.10%** | 0.00% |
/// | -6.5 | 10.30% | 0.50% |
/// | -5.0 | 1.10% | 3.30% |
///
/// -8.0 是**零代价点**：整句命中与不设闸门完全相同，却挡掉了最离谱的那批——
/// `nhaoma` 原本出「你会熬吗」（每字 -11.01），闸掉后正确的「你好吗」上位。
///
/// ★ 收紧到 -5.0 曾看似合理（用户真会打的组合每字落在 -3.7 ~ -4.8：`bzdhaobuhao`
/// →不知道好不好 -3.90、`wmyiqizou`→我们一起走 -4.81），但受控对比显示它**砍掉 91%
/// 的正确整句**，只换来 3.3% 的首词命中——很不划算。且整句占首位时部分候选就在第 2、3 位，
/// 按一下数字键即可选到，「首词命中 0%」并不等于分步上屏那条路断了。
///
/// ⚠️ 正确与错误的分布**在 -5 ~ -6.5 区间大幅重叠**，任何阈值都必然既误伤正解又放行错解；
/// -8.0 只保证「不误伤」。真正的区分需要上下文概率（`lm.rs` 的 bigram 插值已实现、缺语料）。
/// 改动本值必须重跑 `pinyin_eval` 的 D 类并做同样的受控对比。
const MIXED_SENTENCE_MIN_LOGP_PER_CHAR: f64 = -8.0;

/// 混合整句解码（step 2b）的最短击键串。
///
/// 至少要容得下「一个简拼段（2 字母）+ 一个全拼音节（2 字母）」，再短就不存在混合形态，
/// 白建一次词图。
const MIN_MIXED_SENTENCE_LEN: usize = 4;

/// 简拼族前缀回退（step 6.2）的最短击键段。与 `is_abbreviation` 的下限一致：
/// 单字母不构成简拼，退到 1 只会拖出一堆高频单字。
const MIN_ABBREV_STROKE: usize = 2;

/// 前缀回退**单个切点**的产出上限。
///
/// 逐切点限流而非只设总额，是为了保证**短切点也挤得进来**：`bzdhaobuhao` 的
/// `bzdh` 切点若把配额占满，`bzd`（→「不知道」，词频高 672 倍）就一条都进不来。
const MAX_FALLBACK_PER_CUT: usize = 6;

/// 前缀回退**参与竞争的切点数**（有产出的才计数）。
///
/// 取 2 = 「最长 + 次长」。相邻切点的竞争才是真实场景（`bzdhaobuhao` 里 `bzdh`→「表彰大会」
/// 与 `bzd`→「不知道」），再往下只会引入越来越短、越来越不相关的解释：试到 `bz` 时
/// 「标准/帮助/保证」会凭着高词频（~5 万）挤进前几位，而它们只解释了 11 键里的 2 键。
///
/// 这是**软偏好**的落点——不给短切点打折扣（那要按未解释字母数调一个乘性系数，量纲难定
/// 且依赖本轮最大消费长度、不稳定），而是限制它们**是否入场**。入场后一律同层按词频竞争。
///
/// ★ **放宽它已被评测否决，勿再尝试。** D 类（简拼+全拼混合整句）实测：
/// 取 2 → 首词召回 40.30% / 首词命中 2.30%；取 8 → **14.60% / 0.00%**，大幅恶化。
/// 原因是短切点的高频词会占据首选并把长切点的正确候选挤出 top-10
/// （`dltqbandaoer` 首选变成 `dl`→「到了」、`dqsmayou` 变成 `dq`→「地球」）。
/// 召回率要越过 40% 这个天花板，得让所有切点同时参与、由语言模型仲裁 —— 即简拼段入
/// lattice（文档 §4.8），不是调这个常量能做到的。
const MAX_FALLBACK_CUTS: usize = 2;

/// 用户/临时词的**前缀补全**是否上浮进完整匹配层（贴合「长词打到第 3-4 个音节就给出」）。
///
/// 用户长词（如「清风输入法」qingfengshurufa，5 音节）在部分拼音下由 store 层前缀命中，
/// 但恒带 `is_prefix=true`，会被首音节一大批同音子短语（清/青/情…，`is_prefix=false`）整层
/// 压到候选最底、翻页翻不到。此判据决定何时把它提升到完整匹配层（`is_promoted_completion`）：
///
/// **尾部残码**（未成音节的声母，如 `qingfengs` 的 `s`）算作「已起头的一个音节」——用户已
/// 明确要接着打这个音节，意图强于停在整音节边界（`qingfeng`）。`started` = 完整音节数 +
/// (有残码 ? 1 : 0)：
/// - **有边界**（GUI 加词/学习词带音节真值）：`started ≥ 2` 且**距词尾 ≤ `COMPLETION_NEAR_SYLLABLES`**
///   才上浮——`qingfengshu`(started 3, 剩 2) 给、`qingfengs`(started 3, 剩 2) 给、`qingfeng`(started 2, 剩 3) 不给、`qing`(1) 不给。
/// - **无边界**（手输码用户词 `boundary=0`，算不出剩余）：退化为「`started ≥ 3`」门槛，
///   同样对齐「打到第 3 个音节才给」，避免 1-2 音节时被一堆冷僻长词占满前排。
fn should_promote_user_completion(
    completed_syls: usize,
    trailing_partial: bool,
    boundary: u64,
) -> bool {
    let started = completed_syls + usize::from(trailing_partial);
    if boundary != 0 {
        let word_syls = boundary.count_ones() as usize;
        let remaining = word_syls.saturating_sub(started);
        started >= 2 && remaining <= COMPLETION_NEAR_SYLLABLES as usize
    } else {
        started >= 3
    }
}

/// 拼音引擎配置
#[derive(Debug, Clone)]
pub struct Config {
    pub show_code_hint: bool,
    pub use_smart_compose: bool,
    /// 是否产出简拼候选（声母缩写，nh→你好）。默认 true = 历史行为（简拼此前恒开、无开关）。
    ///
    /// 混输经 `schema.mix.enable_pinyin_abbrev` 关闭它：简拼让「几乎任何字母串都可能是拼音」
    /// （`is_abbreviation` 只要求每字母是某音节首字母），而混输里有人只拿拼音做临时输入补位。
    /// 关闭还顺带省掉用户词层的全量扫描（见 convert step6：`search_prefix("", 0)` 枚举全部用户词）。
    pub enable_abbrev: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            show_code_hint: false,
            use_smart_compose: true,
            enable_abbrev: true,
        }
    }
}

/// 拼音引擎
pub struct PinyinEngine {
    /// 引擎配置（show_code_hint / use_smart_compose 等）
    config: Config,
    dict: CachedDict,
    trie: SyllableTrie,
    viterbi: ViterbiDecoder,
    lattice_builder: LatticeBuilder,
    fuzzy_config: FuzzyConfig,
    /// Unigram 语言模型（长句 Viterbi 打分；缺失时回退词典权重）
    unigram: Option<Arc<dyn UnigramLookup>>,
    /// 用户/临时造词层（L 造词显现）：仅含 StoreUserLayer/StoreTempLayer，无系统层。
    /// 拼音候选除主词典外，按相同的码并入这些层的用户造词（None=无持久化，如纯测试）。
    store_layers: Option<Arc<DictManager>>,
    /// 造词反推用的单字读音索引（懒构建：首次 generate_word_pinyin 时从词典派生）。
    char_pinyin_idx: OnceLock<CharPinyinIndex>,
    /// 全库最大 weight，词频等效权重的量纲基准（懒取：内存模式是 O(码数)）。
    /// `Some(None)` = 已求值且词库为空。见 [`Self::max_dict_weight`]。
    max_dict_weight_cache: OnceLock<Option<i32>>,
    /// 双拼转换器（None 表示全拼模式，输入原样传递）。
    shuangpin: Option<ShuangpinConverter>,
}

impl PinyinEngine {
    pub fn new(config: Config, dict: CachedDict) -> Self {
        Self::with_unigram(config, dict, None)
    }

    pub fn with_unigram(
        config: Config,
        dict: CachedDict,
        unigram: Option<Arc<dyn UnigramLookup>>,
    ) -> Self {
        Self {
            config,
            dict,
            trie: SyllableTrie::new(),
            viterbi: ViterbiDecoder::new(),
            lattice_builder: LatticeBuilder::new(),
            fuzzy_config: FuzzyConfig::default(),
            unigram,
            store_layers: None,
            char_pinyin_idx: OnceLock::new(),
            max_dict_weight_cache: OnceLock::new(),
            shuangpin: None,
        }
    }

    /// 全库最大 weight——词频等效权重的**量纲基准**（`docs/design/freq-weight-model.md` §5）。
    ///
    /// 词频分不能用跨模式的硬编码常数：混输把拼音候选整体 `/= PINYIN_TIER_SCALE`，同一个词
    /// 两种模式差 100 倍（`的` 15,378,475 vs 153,784，p50 甚至被整数除法归零），而混输在
    /// 「码不全走双路线」与「超码长走拼音」之间的切换发生在**同一次输入过程中**，配置项
    /// 跟不上。故基准必须由持有词库的引擎按自身量纲给出。
    ///
    /// 只反映**主词典**：用户词/临时词层另有自己的量纲（`ADD_WORD_WEIGHT = 1200`）与提升
    /// 通道（step 6 的 `promotion_cap`），不并入基准。
    ///
    /// 懒求值 + 缓存：mmap 模式 O(1)，内存模式 O(码数)。
    pub fn max_dict_weight(&self) -> Option<i32> {
        *self
            .max_dict_weight_cache
            .get_or_init(|| self.dict.max_weight().filter(|w| *w > 0))
    }

    /// 注入用户/临时造词层（L 造词显现）。链式 builder：构造后由 EngineManager 按 schema 挂上。
    pub fn with_store_layers(mut self, layers: Arc<DictManager>) -> Self {
        self.store_layers = Some(layers);
        self
    }

    /// 注入模糊音配置（取代 with_unigram 中的 FuzzyConfig::default()）。
    pub fn with_fuzzy(mut self, fuzzy: FuzzyConfig) -> Self {
        self.fuzzy_config = fuzzy;
        self
    }

    /// 注入双拼转换器（链式 builder）。注入后 convert/compute_composition 均先把输入转为全拼。
    pub fn with_shuangpin(mut self, conv: ShuangpinConverter) -> Self {
        self.shuangpin = Some(conv);
        self
    }

    /// 仅测试用：读取 fuzzy_config.zh_z 以验证 with_fuzzy 注入是否生效。
    #[cfg(test)]
    pub fn fuzzy_zh_z(&self) -> bool {
        self.fuzzy_config.zh_z
    }

    /// 总条目数
    pub fn entry_count(&self) -> usize {
        self.dict.len()
    }

    /// 从起始位置贪心切出连续完整音节（每步取最长匹配），返回 (音节序列, 结束字节位置)。
    /// 对齐 Go `ContiguousCompletedFromStart`：遇到无完整音节即停（残缺尾部不计入）。
    fn contiguous_completed_from_start(&self, prefix: &str) -> (Vec<String>, usize) {
        let mut syllables = Vec::new();
        let mut pos = 0;
        while pos < prefix.len() {
            // match_at 返回该位置所有完整音节，最长优先；取最长贪心推进。
            let matches = self.trie.match_at(prefix, pos);
            let Some(syl) = matches.into_iter().next() else {
                break;
            };
            pos += syl.len();
            syllables.push(syl);
        }
        (syllables, pos)
    }

    /// 计算 preedit 显示与音节信息。
    /// `full_pinyin` 必须已是全拼串（调用方负责转换），本方法不再内部做双拼→全拼转换。
    ///
    /// 含手动分隔符 `'` 时：按 `'` 分段各自组合，段间以 `'` 重新连接——保留全部手动边界
    /// （含开头 / 结尾 / 连续 `''`），使末尾 `'` 立即可见。段内仍走自动分词。
    fn compute_composition(&self, full_pinyin: &str) -> (String, Vec<String>, String) {
        if !full_pinyin.contains('\'') {
            return self.compose_segment(full_pinyin);
        }
        let mut all_syllables: Vec<String> = Vec::new();
        let mut last_partial = String::new();
        let mut rendered: Vec<String> = Vec::new();
        for seg in full_pinyin.split('\'') {
            if seg.is_empty() {
                rendered.push(String::new());
                continue;
            }
            let (seg_pre, seg_syls, seg_partial) = self.compose_segment(seg);
            rendered.push(seg_pre);
            all_syllables.extend(seg_syls);
            last_partial = seg_partial;
        }
        let preedit = rendered.join("'");
        (preedit, all_syllables, last_partial)
    }

    /// 对单个「无分隔符」片段做自动分词并组合 preedit（原 compute_composition 逻辑）。
    fn compose_segment(&self, full_pinyin: &str) -> (String, Vec<String>, String) {
        let input = full_pinyin;
        let dag = Dag::build(input, &self.trie);
        let syllables = dag.maximum_match();
        let consumed: usize = syllables.iter().map(|s| s.len()).sum();
        let partial = if consumed < input.len() {
            input[consumed..].to_string()
        } else {
            String::new()
        };

        let mut preedit = syllables.join("'");
        if !partial.is_empty() {
            if !preedit.is_empty() {
                preedit.push('\'');
            }
            preedit.push_str(&partial);
        }
        if preedit.is_empty() {
            preedit = input.to_string();
        }
        (preedit, syllables, partial)
    }

    /// 尊重手动分隔符 `'` 的音节分段：按 `'` 切段、各段独立 DAG 最大匹配，
    /// 拼接为纯音节序列（不含 `'`）。`'` 为硬边界，任何音节不得跨越。
    /// 段内未成音节的残码（partial）不计入（仅用于 completed 音节序列）。
    fn segment_with_separators(&self, input: &str) -> Vec<String> {
        self.spans_with_separators(input)
            .into_iter()
            .map(|s| s.pinyin)
            .collect()
    }

    /// 同 [`Self::segment_with_separators`]，但保留每个音节在 raw / flat 两域的偏移。
    ///
    /// `consumed_length` 要回映射到**含 `'` 的原始输入空间**，需要的正是这些偏移；
    /// 只拿音节文本的那个版本丢了它们，此前只能靠 `map_consumed_over_separators`
    /// 逐字节数分隔符补算。两者共用同一次切分，故取 span 版本不多花开销。
    fn spans_with_separators(&self, input: &str) -> Vec<interp::SylSpan> {
        interp::spans_for_full_pinyin(input, &self.trie)
    }

    /// 由全拼码取简拼（各音节声母拼接），供用户/临时造词层动态简拼匹配。
    /// 系统词库规模大，离线预建 AbbrevSection 索引（性能考量）；用户词库规模小，
    /// 现场取声母足够快，无需为其单独建索引/维护写入时的双写一致性。
    ///
    /// **优先采信候选自带的 `boundary`（音节起始字节位 bitmask）**——那是造词/词库解析
    /// 期留下的真值，直接取这些位置的字符即得声母。仅当 `boundary == 0`（旧数据、
    /// 手输码、五笔码）才退回 DAG 切分去猜。
    ///
    /// ⚠️ 重猜在**歧义切分码**上必错，且既漏又错。用户词「西安宁」真值 `xi|an|ning`
    /// 应给 `xan`，而 `maximum_match` 切成 `xian|ning` 给出 `xn` —— 真简拼打不出、
    /// 假简拼反而命中。这是「`maximum_match` 不是真相」的第二次现场（第一次是整句
    /// boundary，见 `pinyin_multipath.rs`：必须用解码器实际走的那条路径）。
    ///
    /// 切分未完全覆盖 code（残码/非法拼音）时返回 None，不参与简拼匹配。
    ///
    /// ⚠️ 混合简拼另有 [`mixed_abbrev::syllables_from_boundary`]，对同一个 `boundary` 做
    /// 同一件事的另一半（那边要整段音节，这边只要首字母）。**两者对 boundary 的解释必须
    /// 一致**，改动其一时同步核对另一处。
    fn abbrev_of_code(&self, code: &str, boundary: u64) -> Option<String> {
        if boundary != 0 {
            // bit 位是**字节**偏移；拼音码为 ASCII，char_indices 的下标即字节位。
            return Some(
                code.char_indices()
                    .filter(|(i, _)| *i < 64 && (boundary >> i) & 1 == 1)
                    .map(|(_, ch)| ch)
                    .collect(),
            );
        }
        let syllables = self.segment_with_separators(code);
        let consumed: usize = syllables.iter().map(|s| s.len()).sum();
        if syllables.is_empty() || consumed != code.len() {
            return None;
        }
        Some(syllables.iter().filter_map(|s| s.chars().next()).collect())
    }

    /// 简拼族召回，但只针对击键串的一个**前缀**（step 6.2 的前缀回退用）。
    ///
    /// `stroke` = 击键串的前 `consumed` 个字节；产出的候选只消费这么多击键，余下的字母
    /// 留给下一次转换（分步上屏）。覆盖四条来源，与整串路径一一对应：
    /// 纯简拼系统词（step5）、混合简拼系统词（step5b）、以及二者的用户词版本（step6）。
    ///
    /// ⚠️ **与那三处是重复实现，改判据时必须同步。** 没有合并是有意的取舍：那三处各自
    /// 带着大段历史踩坑注释和专门的回归测试（层级一致、音节数过滤、边界豁免…），把它们
    /// 抽成公共函数的回归风险大于这里重复三十行的维护成本。判据本身很短，且都由
    /// `mixed_abbrev` 的同一套模式表达。
    fn recall_abbrev_prefix(&self, stroke: &str, consumed: usize, cands: &mut Vec<Candidate>) {
        let trie = &self.trie;
        let dict = &self.dict;
        // 本切点的产出配额（见 MAX_FALLBACK_PER_CUT：不逐切点限流，长切点会把额度占满，
        // 词频高得多的短切点一条都进不来）。
        let start = cands.len();

        // 部分候选统一形态：`is_abbrev` 归入简拼层、`is_partial` 表示只覆盖了输入前缀
        // （沉在完整匹配之后），`consumed_length` **自带击键域的消费数**——下方那个按
        // code/query 关系统一计算 consumed 的循环会跳过它们（见其注释）。
        let push =
            |cands: &mut Vec<Candidate>, text: String, code: String, w: i32, boundary: u64| {
                if cands.len() - start >= MAX_FALLBACK_PER_CUT {
                    return;
                }
                if text.is_empty() || cands.iter().any(|c| c.text == text) {
                    return;
                }
                cands.push(Candidate {
                    text,
                    code,
                    weight: w,
                    natural_order: 999999,
                    source: CandidateSource::Pinyin,
                    is_abbrev: true,
                    is_partial: true,
                    boundary,
                    consumed_length: consumed,
                    ..Default::default()
                });
            };

        // ① 纯简拼系统词（同 step5，含「音节数 == 简拼字母数」过滤）
        if AbbrevMatcher::is_abbreviation(stroke, trie) {
            for abbr_code in dict.search_abbrev(stroke, 10) {
                for h in dict.search_with_boundary(&abbr_code) {
                    if h.boundary != 0 && h.boundary.count_ones() as usize != stroke.len() {
                        continue;
                    }
                    push(cands, h.text, abbr_code.clone(), h.weight, h.boundary);
                }
            }
        }

        // ② 混合简拼系统词（同 step5b）。`stroke` 已是完整音节序列时不做混合解释——
        //    那属于全拼语义，交给主路径（此处的 stroke 是前缀，主路径按整串查，故这里
        //    直接跳过即可）。
        let covered: usize = Dag::build(stroke, trie)
            .maximum_match()
            .iter()
            .map(|s| s.len())
            .sum();
        let pats = if covered >= stroke.len() {
            Vec::new()
        } else {
            mixed_abbrev::mixed_patterns(stroke, trie)
        };
        if !pats.is_empty() {
            let mut keys: Vec<&str> = pats.iter().map(|p| p.key()).collect();
            keys.sort_unstable();
            keys.dedup();
            for key in keys {
                for abbr_code in dict.search_abbrev(key, MIXED_ABBREV_INDEX_LIMIT) {
                    for h in dict.search_with_boundary(&abbr_code) {
                        let Some(syls) =
                            mixed_abbrev::syllables_from_boundary(&abbr_code, h.boundary)
                        else {
                            continue;
                        };
                        if !pats.iter().any(|p| p.key() == key && p.matches(&syls)) {
                            continue;
                        }
                        push(cands, h.text, abbr_code.clone(), h.weight, h.boundary);
                    }
                }
            }
        }

        // ③④ 用户/临时造词层的纯简拼与混合简拼（同 step6 末段）
        if let Some(store_dm) = &self.store_layers {
            if AbbrevMatcher::is_abbreviation(stroke, trie) || !pats.is_empty() {
                for c in store_dm.search_prefix("", 0) {
                    let plain = self.abbrev_of_code(&c.code, c.boundary).as_deref() == Some(stroke);
                    let mixed = !plain
                        && mixed_abbrev::syllables_from_boundary(&c.code, c.boundary)
                            .is_some_and(|syls| pats.iter().any(|p| p.matches(&syls)));
                    if plain || mixed {
                        push(cands, c.text, c.code, c.weight, c.boundary);
                    }
                }
            }
        }
    }

    /// 带模糊拼音扩展的词库查找（对齐 Go lookupWithFuzzy）。
    /// `code` 为待查询的全拼码（整串或前缀子码）；`syllables` 为该码对应的音节切分，
    /// 用于生成模糊变体。返回与 `dict.search` 相同的 `(text, weight, order)`。
    ///
    /// fuzzy 全 false 时 fuzzy_variants 返回空 → 天然退化为纯 `dict.search`（无需 enabled 判断）。
    /// 返回 `(text, weight, order, is_fuzzy)`：原 code 精确命中 is_fuzzy=false；
    /// 模糊变体命中 is_fuzzy=true（供排序时整体降到精确候选之后）。
    fn lookup_with_fuzzy(&self, code: &str, syllables: &[String]) -> Vec<LookupHit> {
        // 精确匹配：候选码即查询码 `code`，故词典 boundary 与之同域，可直接采信。
        // 注意此处必须用 search_with_boundary——拼音引擎直接持有 CachedDict、不经
        // SystemDictLayer，用 search() 会把边界丢在这里。
        let mut results: Vec<LookupHit> = self
            .dict
            .search_with_boundary(code)
            .into_iter()
            .map(|h| LookupHit {
                text: h.text,
                weight: h.weight,
                order: h.order,
                is_fuzzy: false,
                boundary: h.boundary,
            })
            .collect();
        let mut seen: std::collections::HashSet<String> =
            results.iter().map(|h| h.text.clone()).collect();

        // 模糊变体命中一律 boundary=0（不设防）：词典给的是**变体码**（如 zhongguo）的切分，
        // 而候选对外的 code 是用户实际输入的原码（zongguo）——两者不同域，位偏移对不上，
        // 直接采信会错位误杀。模糊音本就是放宽匹配，不校验边界是合理的。
        if syllables.len() <= 1 {
            // 单音节：对该音节（无切分时退化为整码）生成变体逐个查询。
            let syllable: &str = if syllables.len() == 1 {
                &syllables[0]
            } else {
                code
            };
            for variant in fuzzy::FuzzyMatcher::fuzzy_variants(syllable, &self.fuzzy_config) {
                for (text, weight, order) in self.dict.search(&variant) {
                    if seen.insert(text.clone()) {
                        results.push(LookupHit {
                            text,
                            weight: fuzzy_penalized(weight),
                            order,
                            is_fuzzy: true,
                            boundary: 0,
                        });
                    }
                }
            }
        } else {
            // 多音节：笛卡尔积展开各音节变体，拼成完整 altCode 查询。
            for alt_code in self.expand_code(syllables) {
                if alt_code == code {
                    continue;
                }
                for (text, weight, order) in self.dict.search(&alt_code) {
                    if seen.insert(text.clone()) {
                        results.push(LookupHit {
                            text,
                            weight: fuzzy_penalized(weight),
                            order,
                            is_fuzzy: true,
                            boundary: 0,
                        });
                    }
                }
            }
        }

        results
    }

    /// 对多音节做模糊变体笛卡尔积展开（对齐 Go `FuzzyConfig.ExpandCode`）。
    ///
    /// 实现收口在 [`fuzzy::FuzzyMatcher::expand_syllables`]，与 `lattice.rs` 的整句路径共用
    /// **同一份**逐音节展开逻辑——两处曾各写一套，且 lattice 那套对整串求变体，非首音节的
    /// 模糊永远命中不了（见该函数文档）。
    fn expand_code(&self, syllables: &[String]) -> Vec<String> {
        fuzzy::FuzzyMatcher::expand_syllables(syllables, &self.fuzzy_config)
    }
}

/// 候选码 `code` 是否恰好落在前 k 个音节的边界上（`syllables[..k].join("") == code`）。
/// 命中返回 `Some(k)`（k>=1）；不落任何边界（如前缀补全的超长码）返回 `None`。
/// 供手动分隔符边界过滤：判断候选字数是否与所跨音节数一致。
fn syllable_span(syllables: &[String], code: &str) -> Option<usize> {
    if code.is_empty() {
        return None;
    }
    let mut acc = String::new();
    for (i, s) in syllables.iter().enumerate() {
        acc.push_str(s);
        if acc.len() > code.len() {
            break;
        }
        if acc == code {
            return Some(i + 1);
        }
    }
    None
}

/// 词典查询命中（含音节边界），供 `lookup_with_fuzzy` 返回。
struct LookupHit {
    text: String,
    weight: i32,
    order: i32,
    is_fuzzy: bool,
    /// 该候选 code 的音节边界；0=无信息（模糊变体/非拼音词库/旧数据），不参与校验。
    boundary: u64,
}

/// 按边界 bitmask 渲染 preedit：`code` 以 `'` 在各音节起点断开，尾部残码另起一段。
/// 供「预编辑区跟随首选候选」使用（见 `convert`）。
fn render_preedit(code: &str, boundary: u64, partial: &str) -> String {
    let mut out = String::with_capacity(code.len() + 8);
    for (i, ch) in code.char_indices() {
        if i > 0 && i < 64 && (boundary >> i) & 1 == 1 {
            out.push('\'');
        }
        out.push(ch);
    }
    if !partial.is_empty() {
        if !out.is_empty() {
            out.push('\'');
        }
        out.push_str(partial);
    }
    out
}

/// 由音节列表算边界 bitmask（全拼空间），只取覆盖前 `limit_len` 字节的部分。
///
/// 用于 **DAG 切分出来的**候选（Viterbi 整句、前缀子短语）——它们的 code 是把
/// `syllables` 拼起来的，故其"边界"就是这份切分本身。这与词典真值边界同域、可直接比对：
/// 双拼 `nihao` 被 DAG 重切成 `ni|hao` 拼出「你好」时，标上 DAG 的切分，正好会被
/// 双拼真值 `ni|ha|o` 拒掉——这正是我们要的。
fn syllables_boundary_mask(syllables: &[String], limit_len: usize) -> u64 {
    let mut mask = 0u64;
    let mut pos = 0usize;
    for s in syllables {
        if pos >= limit_len {
            break;
        }
        if pos >= 64 {
            return 0;
        }
        mask |= 1u64 << pos;
        pos += s.len();
    }
    mask
}

/// 双拼给出的**分段边界**（全拼空间 bitmask，与候选 `boundary` 同域）。
///
/// 双拼每 2 键 = 1 段，边界免费且精确——这正是双拼相对全拼的信息优势，此前却被拼成
/// `full_pinyin` 后交给 DAG 重猜。
///
/// **回写段也算一个段起点**。`convert` 拼不出合法音节时会把两个键原样写进 full
/// （注释所谓「简拼/无效键对」）且不产生 `SylSpan`——但它照样**占据 full 的一段**、
/// 用户也确实是当一个单元敲的，故它的起点同样是真值。曾以为这类段"无从表达"而让整个
/// mask 作废（返回 0 = 不校验），结果 `nihaoya` 的「你好呀」从 step4 前缀补全漏网：
/// 校验一关，全拼命中就畅通无阻。给回写段标上起点后，`ni|ha|oy…` = {0,2,4} 与词典的
/// `ni|hao|ya` = {0,2,5} 自然不符，拒绝生效。
///
/// 返回 0 仅表示无可用信息（空输入 / 越出 64 位表达范围）。
fn sp_boundary_mask(sp: &shuangpin::SpConvertResult) -> u64 {
    let mut mask = 0u64;
    let mut cursor = 0usize;
    let mark = |pos: usize, mask: &mut u64| -> bool {
        if pos >= 64 {
            return false;
        }
        *mask |= 1u64 << pos;
        true
    };
    for s in &sp.syllables {
        // 音节之前的空隙 = 回写段（如 `omni` 的 om），其起点同样是段边界。
        if s.fp_start > cursor && !mark(cursor, &mut mask) {
            return 0;
        }
        if !mark(s.fp_start, &mut mask) {
            return 0;
        }
        cursor = s.fp_end;
    }
    // 尾部剩余：partial 声母（nihao 的 o）或回写段（nihaoya 的 oy+a）——两者都开一个新段。
    // 注：回写段内部可能不止一段（每 2 键一段），但其细分无从得知；只标首个起点即可，
    // 已足以让「跨越该点的词典切分」失配。
    if cursor < sp.full_pinyin.len() && !mark(cursor, &mut mask) {
        return 0;
    }
    mask
}

/// 候选的音节切分是否与双拼解释相容。
///
/// 双拼定死了每个音节的边界，候选（词典真值边界）必须与之吻合，否则它根本不是用户打的那串音。
/// 典型：输入 `nihao`(5键) 双拼解释为 `ni|ha|o`，而「你好」的词典边界是 `ni|hao`——两者不符，
/// 「你好」应被拒绝（该词的正确双拼是 4 键）。此前因边界信息全丢，只能靠 DAG 把
/// `nihao` 重新切成 `ni|hao`，于是 5 键也能出「你好」。
///
/// 比较窗口取 `min(候选 code 长, 全拼串长)`：
/// - 候选码更短（子短语，如 `ni`→「你」）→ 只比其覆盖的前缀范围；
/// - 候选码更长（前缀补全，输入 `ni` 补出「你好」`nihao`）→ 只比已输入的部分，
///   补全部分尚未键入、无从校验。
///
/// 任一侧无边界信息（0）即放行——降级回原有 DAG 行为，不误杀。
fn boundary_compatible(cand_boundary: u64, sp_mask: u64, code_len: usize, full_len: usize) -> bool {
    if cand_boundary == 0 || sp_mask == 0 {
        return true; // 无信息 → 不设防（用户手输码/五笔/超长码/含回写段）
    }
    let win = code_len.min(full_len);
    if win == 0 {
        return true;
    }
    let win_mask = if win >= 64 {
        u64::MAX
    } else {
        (1u64 << win) - 1
    };
    cand_boundary & win_mask == sp_mask & win_mask
}

/// Fix A：用双拼原始按键重建 preedit（按音节边界以 `'` 分隔）。
///
/// **必须完整覆盖 `raw_input` 的每个字节**：已完成音节取其 `[raw_start, raw_end)`，音节之间与尾部
/// 未被任何音节覆盖的字节原样作独立段。不可只在 `has_partial` 时补尾——无匹配键对（`convert`
/// 的 else 分支「原样回写」，如首道双拼的 `om`）既不进 `syllables` 也不置 `has_partial`，
/// 早期实现据此判尾会把它们静默吞掉：`nihaom` → `ni'ha`（om 消失）、再按 `a` 又诡异复现。
/// 分隔符与全拼自动分词一致用 `'`。双拼键均为 ASCII，字节切片安全。
fn build_raw_preedit(raw_input: &str, sp: &shuangpin::SpConvertResult) -> String {
    if raw_input.is_empty() {
        return String::new();
    }
    let mut segments: Vec<&str> = Vec::new();
    let mut cursor = 0usize;
    for s in &sp.syllables {
        // 音节之前未被覆盖的字节：无匹配键对的原样回写段。
        if s.raw_start > cursor {
            segments.push(&raw_input[cursor..s.raw_start]);
        }
        segments.push(&raw_input[s.raw_start..s.raw_end]);
        cursor = s.raw_end;
    }
    // 尾部剩余：partial 尾键 或 无匹配回写段。
    if cursor < raw_input.len() {
        segments.push(&raw_input[cursor..]);
    }
    if segments.is_empty() {
        // 无 syllables：原样返回。
        return raw_input.to_string();
    }
    segments.join("'")
}

impl Engine for PinyinEngine {
    /// 转发到同名 inherent 方法（带 `OnceLock` 缓存）。纯拼音量纲即词库原始量纲。
    fn max_dict_weight(&self) -> Option<i32> {
        PinyinEngine::max_dict_weight(self)
    }

    fn convert(&self, input: &str, max_candidates: usize) -> anyhow::Result<ConvertResult> {
        if input.is_empty() {
            return Ok(ConvertResult::default());
        }

        // 双拼方案不支持手动音节分隔符 `'`：若含 `'` 先整体剥除，保持双拼转换/preedit 原语义。
        // 手动分隔符仅在全拼路径（shuangpin=None）生效。
        let sp_stripped: String;
        let input: &str = if self.shuangpin.is_some() && input.contains('\'') {
            sp_stripped = input.chars().filter(|&c| c != '\'').collect();
            &sp_stripped
        } else {
            input
        };

        // Fix A：在任何 shadow 之前保存用户实际输入的原始字符（双拼键序列或全拼）。
        // 用于重建 preedit_display（显示原始按键），以及简拼判定（见 `abbr_query`）。
        let raw_input = input;

        // 简拼判定与比对的基准串：**原始击键**，不是转换后的全拼。
        //
        // 简拼的定义是「每个字母取一个音节的声母」，这件事只跟用户敲下的字母有关，与双拼
        // 编码方案无关。而下面 `input` 会被双拼转换结果覆盖、`query` 由它派生——双拼下打
        // `xan` 得到的是「某音节 + partial 声母」，拿它去判简拼永远匹配不到用户实际敲的
        // `xan`，用户词「西安宁」的简拼在双拼下因此完全不可达。
        //
        // 全拼下 `raw_input == query`，故此改动对全拼零影响。简拼串不含 `'`
        //（`is_abbreviation` 对分隔符判假），故与手动分隔符路径也不冲突。
        let abbr_query = raw_input;

        // 双拼激活时保留 SpConvertResult，以便后续用 map_consumed_length 回算消费键数。
        let sp_result: Option<shuangpin::SpConvertResult> =
            self.shuangpin.as_ref().map(|conv| conv.convert(input));
        let full_owned: String = match &sp_result {
            Some(r) if !r.full_pinyin.is_empty() => r.full_pinyin.clone(),
            Some(_) => input.to_string(),
            None => input.to_string(),
        };
        let input = full_owned.as_str();

        // 手动音节分隔符 `'` 支持（全拼路径）：
        // - `has_sep`：输入含手动分隔符，走边界感知分词 + 剥除查询。
        // - `query`：剥除 `'` 后的纯拼音串（词典查询用）；音节边界信息来自带 `'` 的分段。
        let has_sep = input.contains('\'');
        let query_owned: String = if has_sep {
            input.chars().filter(|&c| c != '\'').collect()
        } else {
            String::new()
        };
        let query: &str = if has_sep { query_owned.as_str() } else { input };

        // 纯分隔符输入（如 `'` / `''`）：无可查询拼音，仅回显分隔符，不产候选。
        if has_sep && query.is_empty() {
            let (preedit, _, _) = self.compute_composition(input);
            return Ok(ConvertResult {
                preedit_pinyin: preedit.clone(),
                preedit_display: preedit,
                is_empty: true,
                ..Default::default()
            });
        }

        let dict = &self.dict;
        let trie = &self.trie;
        let mut candidates: Vec<Candidate> = Vec::new();

        let push_unique = |cands: &mut Vec<Candidate>,
                           text: String,
                           code: String,
                           weight: i32,
                           order: i32,
                           is_fuzzy: bool,
                           is_prefix: bool,
                           boundary: u64,
                           is_promoted: bool| {
            if text.is_empty() || cands.iter().any(|c| c.text == text) {
                return;
            }
            // 子短语候选：code 是输入的真前缀（比输入短，如 baoan 的「报」(bao)）。
            // Viterbi 整句走 insert(0) 不经此闭包，故无需 weight 启发式即可排除整句。
            // 注：以剥除分隔符后的 query 为基准（无分隔符时 query==input，行为不变）。
            let is_partial = !is_prefix && code.len() < query.len() && query.starts_with(&code);
            cands.push(Candidate {
                text,
                code,
                weight,
                natural_order: order,
                source: CandidateSource::Pinyin,
                is_fuzzy,
                is_prefix,
                is_partial,
                boundary,
                is_promoted_completion: is_promoted,
                ..Default::default()
            });
        };

        // DAG 分词提前到 step1 之前：lookup_with_fuzzy 需要音节列表生成模糊变体。
        // 含手动分隔符时按 `'` 分段独立分词（`'` 为硬边界，音节不得跨越），否则整串 DAG。
        //
        // **双拼激活时用双拼自己的真值切分**，不让 DAG 对拼平后的 full_pinyin 重猜——
        // 双拼每 2 键 = 1 音节，边界免费且精确。让 DAG 重猜会造成「查询按猜测、校验按真值」
        // 两套切分打架：`hao`(3键) 双拼解释为 ha|o，DAG 却重切成 [hao] 只查了「好」，
        // 随后被真值拒掉，而真正该查的 `ha`（→「哈」）压根没查 → 候选全空。
        // 双拼激活：取**从 0 起连续覆盖**的音节前缀，遇断裂即止。
        //
        // 断裂 = 「无匹配键对原样回写」段（convert 的 else 分支，如 `oy`——o 非声母、拼不出
        // 音节）。它没有 SylSpan，其后音节的 fp 偏移也已被它污染，故断裂处之后
        // **不解释**：那本就是用户打错的键，不该产生候选。
        //
        // 不可整串退回 DAG——那等于「打错一个键对反而解锁全拼」，与 nihao(5键) 不出「你好」
        // 自相矛盾。注释里「简拼/无效键对」的**简拼**那半由 AbbrevMatcher 兜底，它走 query、
        // 不看音节切分，本就不需要 DAG（见 shuangpin_writeback_keeps_abbrev_input_intact）。
        //
        // 尾部 partial（未完成音节的声母）不是完成音节，不计入——由 step4 前缀补全承接。
        let sp_syllables: Option<Vec<String>> = sp_result.as_ref().map(|r| {
            let mut v = Vec::with_capacity(r.syllables.len());
            let mut cursor = 0usize;
            for s in &r.syllables {
                if s.fp_start != cursor {
                    break; // 断裂：其后 fp 偏移不可信，停止解释
                }
                v.push(s.pinyin.clone());
                cursor = s.fp_end;
            }
            v
        });
        // `fixed_segmentation` = 切分是**真值**、只有一条（双拼每 2 键 1 音节；`'` 是硬边界），
        // 词图必须照单全收，绝不可让 DAG 重猜。全拼则相反：切分是猜的，词图应看到**全部**
        // 候选切法（见 lattice::LatticeBuilder::build）。
        let fixed_segmentation = sp_syllables.is_some() || has_sep;
        // 带分隔符时保留 span 序列：下方 consumed_length 要靠它回映射到含 `'` 的原始空间，
        // 与双拼的 map_consumed_length 共用同一套 SylSpan 表示（见 interp 模块文档）。
        let sep_spans = if has_sep {
            self.spans_with_separators(input)
        } else {
            Vec::new()
        };
        let syllables = if let Some(v) = sp_syllables {
            v
        } else if has_sep {
            sep_spans.iter().map(|s| s.pinyin.clone()).collect()
        } else {
            Dag::build(input, trie).maximum_match()
        };

        // 完成音节覆盖的连续前缀（从起点算）。
        //
        // **多路径切分下这个值依然唯一确定**，无须在多条路径间做选择：所有切分路径都从 0
        // 连续覆盖，故「覆盖长度」恒等于「路径终点」，而 `maximum_match` 取的正是**最远
        // 可达位置**——该位置是图的性质，与走哪条路径无关。于是 `completed_len` /
        // `consumed_length`（分段上屏字节数）保持单一确定值，多路径只影响词图**内部**
        // 查哪些跨度，不影响引擎对外承诺消费多少输入。
        //
        // 尾部不成音节的残码（如「nihaom」的「m」）
        // 不参与精确匹配/整句解码——否则 lattice 到不了残码末端、Viterbi 失败、整句退化成单字，
        // 且精确层会把「nihao」当模糊变体误标 is_fuzzy 沉底被截断（bug①）。
        let completed_len: usize = syllables.iter().map(|s| s.len()).sum();
        // 含分隔符时用音节直接拼接（避免 `'` 字节位错位）；无分隔符时 query==input，等价原切片。
        let completed_owned: String;
        let completed: &str = if has_sep {
            completed_owned = syllables.join("");
            &completed_owned
        } else {
            &query[..completed_len]
        };

        // 1. 精确查找（完整匹配，含模糊扩展，对齐 Go lookupWithFuzzy）。
        //    以 completed（完成音节前缀）而非 query（可能含尾部残码）为查询码与存储 code：
        //    残码存在时（nihaom）query 非合法音节序列，search(query) 为空，而 lookup_with_fuzzy
        //    的 expand_code「全原音节」组合会命中 completed 的精确匹配——但因 `alt_code == code`
        //    守卫按 query 比较而失配，被误标 is_fuzzy=true 沉底、遭 truncate 截断（bug①）。
        //    传 completed 后守卫正确跳过全原组合（精确匹配 is_fuzzy=false）；code 存 completed 使
        //    残码输入的 consumed_length 只覆盖完成音节（nihao 消费 5 留 m 续输）。
        for h in self.lookup_with_fuzzy(completed, &syllables) {
            push_unique(
                &mut candidates,
                h.text,
                completed.to_string(),
                h.weight,
                h.order,
                h.is_fuzzy,
                false,
                h.boundary,
                false,
            );
        }

        // 1.5 超长词典整词兜底：与整句同量纲化
        //
        // 整句权重 = SENTENCE_WEIGHT_BASE(30M) + 各节点 log_prob 之和；而词典精确命中只带
        // 原始词频（「中华人民共和国」= 3113）。二者量纲不同却在同一个 weight 维度上比较
        // （排序键三个布尔位此时全部打平），词典整词必然输给整句——哪怕整句是由语义碎片
        // 拼出的错误切分。这里按 lattice 的同一个 score_node 公式给它算「单节点等价整句分」，
        // 使二者公平比较：合理的整句照样赢（冷僻精确词自身 log_prob 很低），词典里确实存在
        // 的长词不再被结构性埋没。
        //
        // 仅限音节数超过词图上限的词。上限内的词 Viterbi 已能作为单节点自行选中（这正是
        // max_word_len 6→10 修好「中华人民共和国」的原因），无需在此干预；若不加这道限制，
        // 所有精确整词（gonghe 的恭贺/共贺等）都会被授予 is_sentence 身份而锚定在顶部，
        // 永久失去词频学习能力——「整句解」应是引擎对整串输入的最优解读，不是"凡精确即整句"。
        //
        // 只在无残码时生效（query == completed）：有残码时精确命中只覆盖完成音节前缀，
        // 本就不该与覆盖全输入的整句同级竞争。
        // 放在 step 2 之前：整句尚未插入，无需排除整句自身；同文时 step 2 的
        // `existing.weight.max(weight)` 会自然取二者较高者。
        if query.len() == completed.len() && syllables.len() > self.lattice_builder.max_word_len() {
            for c in candidates.iter_mut() {
                if c.is_fuzzy || c.is_prefix || c.code != completed {
                    continue;
                }
                let log_prob = lattice::score_node(&c.text, c.weight, self.unigram.as_deref());
                let log_offset =
                    (log_prob * 1000.0).clamp(-(SENTENCE_WEIGHT_BASE as f64), 0.0) as i32;
                c.weight = c
                    .weight
                    .max(SENTENCE_WEIGHT_BASE.saturating_add(log_offset));
                // 与整句同量纲即同身份：它是引擎对整串输入的最优解读，只是恰好由一个词典
                // 整词构成。不标的话 freq_rerank 会把 is_sentence 的拼接整句锚定到它之上，
                // 「冠状动脉粥样硬化性心脏病」又会被「罐装动脉…」压回去。
                c.is_sentence = true;
            }
        }

        // Viterbi **新合成**的整句文本（词典里没有这个词，只能由多个节点拼出来）。
        // 与词典整词同文而被合并的那一支不记入——它本身就是精确整词，不存在「让位」问题。
        // 供 step 6.5 的降级判定使用（须等 step 6 并入用户/临时层后才能定夺）。
        let mut synthesized_sentence: Option<String> = None;

        // 整串是否已被完整音节覆盖。**两条简拼路径共用**：step 2b 的混合整句与 step 5b/6.2
        // 的混合简拼都只在「输入里有成不了音节的字母」时才该启动，纯全拼一律不碰。
        //
        // 全拼下比较 `completed_len` 与击键长度；双拼下 `completed_len` 说的是转换后的全拼域，
        // 与 `abbr_query`（原始击键）不同域，故改判「转换结果覆盖了整串击键」。
        let mixed_covered = match &sp_result {
            Some(r) => r
                .syllables
                .last()
                .is_some_and(|s| s.raw_end >= raw_input.len()),
            None => completed_len >= abbr_query.len(),
        };

        // 2. Viterbi 长句解码（>=2 音节，仅在完成音节前缀上跑；use_smart_compose=false 时跳过）
        if self.config.use_smart_compose && syllables.len() >= 2 {
            // 切分图：全拼取 DAG 的全部路径；双拼/手动分隔符取真值链（行为与改造前一致）。
            let seg_graph = if fixed_segmentation {
                SegGraph::from_syllables(&syllables)
            } else {
                SegGraph::from_dag(&Dag::build(completed, trie))
            };
            let lattice_nodes = self.lattice_builder.build(
                completed,
                &seg_graph,
                dict,
                Some(&self.fuzzy_config),
                self.unigram.as_deref(),
                true,
            );
            let input_len = completed.len();
            let mut lattice: Vec<Vec<WordNode>> = vec![Vec::new(); input_len + 1];
            for (end_pos, nodes_at_end) in lattice_nodes.iter().enumerate() {
                if end_pos > input_len {
                    continue;
                }
                for node in nodes_at_end {
                    lattice[end_pos].push(WordNode {
                        start: node.start,
                        end: node.end,
                        word: node.word.clone(),
                        syl_mask: node.syl_mask,
                        log_prob: node.log_prob,
                    });
                }
            }
            let result = self.viterbi.decode(&lattice, input_len);
            // 仅接受有限概率的完整路径：解码失败时 log_prob 为 NEG_INFINITY，
            // 不能把这种空/错误路径强插到首选位置。
            if !result.words.is_empty() && result.log_prob.is_finite() {
                let sentence: String = result.words.join("");
                if !sentence.is_empty() {
                    // 整句优先：给予高权重置顶（log_prob 为负，原 .max(1) 会被截断淘汰）。
                    // clamp + saturating_add 防止超长低频句的 log_prob 溢出 i32 导致沉底/panic。
                    let log_offset = (result.log_prob * 1000.0)
                        .clamp(-(SENTENCE_WEIGHT_BASE as f64), 0.0)
                        as i32;
                    let weight = SENTENCE_WEIGHT_BASE.saturating_add(log_offset);
                    if let Some(existing) = candidates.iter_mut().find(|c| c.text == sentence) {
                        // 整句与已有候选（如精确匹配 你好）同文：提升其权重置顶，
                        // 同时抹去 is_partial（step1 标了 true，但整句是完整解读并非子短语），
                        // 否则残码场景下 is_partial=true 会在排序时被 is_partial=false 的前缀补全
                        // （如「你好吗」）压下去——后者经 trailing_partial 优化也是 false。
                        // 留存加成前的词典权重：3e7 是「排第一」的编码而非可比量级，
                        // contested 摘掉锚定后要靠这个值下场与同码词按量级竞争
                        // （寺院 491 vs 思源 245）。见 [[Candidate::dict_weight]]。
                        // 只在真被抬高时记录，且不覆盖已有值——同一候选可能多次进入本分支。
                        if weight > existing.weight {
                            existing.dict_weight.get_or_insert(existing.weight);
                        }
                        existing.weight = existing.weight.max(weight);
                        existing.is_partial = false;
                        // 同文合并后它就是整句解本身，须继承整句身份，
                        // 否则 freq_rerank 会把它当普通候选而让别的整句锚定到它之上。
                        existing.is_sentence = true;
                    } else {
                        synthesized_sentence = Some(sentence.clone());
                        candidates.insert(
                            0,
                            Candidate {
                                text: sentence,
                                // 码为完成音节前缀（不含残码），使 consumed_length=completed_len，
                                // 整句上屏后残码留缓冲续输（你好m → 选你好留 m）。
                                code: completed.to_string(),
                                weight,
                                natural_order: 0,
                                source: CandidateSource::Pinyin,
                                is_sentence: true,
                                // 整句的边界 = 解码器**实际选中**的那条路径（多路径下同一串
                                // 输入可有多种切法，只有解码器知道走的是哪条）。回退到
                                // maximum_match 仅用于解码器给不出边界的极端情形（超 64 字节）。
                                boundary: if result.boundary != 0 {
                                    result.boundary
                                } else {
                                    syllables_boundary_mask(&syllables, completed.len())
                                },
                                ..Default::default()
                            },
                        );
                    }
                }
            }
        }

        // 2b. **混合整句解码**：简拼段与全拼段在同一张词图里由 Viterbi 选路径
        //     （`bzdhaobuhao` → 「不知道」+「好不好」→ 整句「不知道好不好」）。
        //
        //     为什么不能复用 step 2：它的入口是 `syllables.len() >= 2`，而 `syllables` 是
        //     **从 0 起连续的完整音节覆盖**。`bzdhaobuhao` 的 DAG 从位置 0 就走不通（`b` 不
        //     成音节）⇒ `syllables` 为空、`completed` 为空串 ⇒ 整句解码压根不启动。step 2
        //     依赖 `completed` 来处理残码（`nihaom` 只在 `nihao` 上跑整句），不能改成整串，
        //     故这里另起一条在**整串**上建图的路径。
        //
        //     与 step 2 的三处差别：
        //     ① 在 `abbr_query`（整串击键）而非 `completed` 上建图；
        //     ② `require_reachable=false` —— 简拼段打断了音节图的可达性，而 `[3,6) hao`
        //        这些边其实都在图里，补上连接的是随后追加的简拼节点；
        //     ③ 追加 `add_abbrev_nodes`，跨度独立枚举（音节图给不出简拼段的终点）。
        //
        //     门槛：`!mixed_covered`（整串有成不了音节的字母）挡掉纯全拼输入——那种输入
        //     step 2 已经处理，再跑一遍只会重复建图并引入简拼噪音。双拼跳过：`input` 是
        //     转换后的全拼、与击键不同域，简拼判据会全部失配（文档 §5 约束 4）。
        if self.config.use_smart_compose
            && self.config.enable_abbrev
            && sp_result.is_none()
            && !has_sep
            // **未被音节覆盖的部分要够构成一个简拼段**，比 `!mixed_covered` 更严。
            // 尾部单字母残码是「还没打完」而不是简拼：`zhongguorenm` 的 `m` 也让
            // `!mixed_covered` 成立，整句便插到首位、把「中国人民解放军」挤后一格
            // （`pinyin_completion::test_useful_completions_still_float` 当场抓到）。
            // 这道闸门只加在 step 2b —— 只有它会抢首选；step 5b 的混合召回不受影响，
            // `nihaom` → 「你好吗」正该走混合模式。
            && abbr_query.len().saturating_sub(completed_len) >= MIN_ABBREV_STROKE
            && abbr_query.len() >= MIN_MIXED_SENTENCE_LEN
        {
            let graph = SegGraph::from_dag(&Dag::build(abbr_query, trie));
            let mut lattice_nodes = self.lattice_builder.build(
                abbr_query,
                &graph,
                dict,
                Some(&self.fuzzy_config),
                self.unigram.as_deref(),
                false,
            );
            self.lattice_builder.add_abbrev_nodes(
                abbr_query,
                dict,
                self.unigram.as_deref(),
                &mut lattice_nodes,
            );

            let input_len = abbr_query.len();
            let mut lattice: Vec<Vec<WordNode>> = vec![Vec::new(); input_len + 1];
            for (end_pos, at_end) in lattice_nodes.iter().enumerate() {
                if end_pos > input_len {
                    continue;
                }
                for node in at_end {
                    lattice[end_pos].push(WordNode {
                        start: node.start,
                        end: node.end,
                        word: node.word.clone(),
                        syl_mask: node.syl_mask,
                        log_prob: node.log_prob,
                    });
                }
            }
            let result = self.viterbi.decode(&lattice, input_len);
            if !result.words.is_empty() && result.log_prob.is_finite() {
                let sentence: String = result.words.join("");
                // 质量闸门：低置信整句宁可不出，也不该顶掉可用的部分候选
                // （见 MIXED_SENTENCE_MIN_LOGP_PER_CHAR 的取值依据）。
                let logp_per_char = result.log_prob / sentence.chars().count().max(1) as f64;
                // **至少两个节点才算「组句」。** 单节点路径（`dblg` → 只有「夺不了冠」）
                // 本质就是一条简拼候选，包装成整句有实际危害：整句的 code 是**击键串**
                // （`dblg`），而简拼候选的 code 是全拼码（`duobuliaoguan`）——同一个词的
                // 词频会记到两个互不相认的键上，正是 wdat v5 改「索引存码」时修掉的那个坑。
                // 交给 step5/6.2 的简拼路径处理即可，那边的 code 是对的。
                if result.words.len() >= 2
                    && !sentence.is_empty()
                    && logp_per_char >= MIXED_SENTENCE_MIN_LOGP_PER_CHAR
                    && !candidates.iter().any(|c| c.text == sentence)
                {
                    let log_offset = (result.log_prob * 1000.0)
                        .clamp(-(SENTENCE_WEIGHT_BASE as f64), 0.0)
                        as i32;
                    candidates.insert(
                        0,
                        Candidate {
                            text: sentence,
                            // code = 整串**击键**。混合整句本就不是词库主键（它是解读不是词），
                            // 与 step 2 的整句一样以「本次输入」为码；消费整串由此自然成立。
                            code: abbr_query.to_string(),
                            weight: SENTENCE_WEIGHT_BASE.saturating_add(log_offset),
                            natural_order: 0,
                            source: CandidateSource::Pinyin,
                            is_sentence: true,
                            // 解码器实际走的那条路径。简拼段每字母一位，故回填出的
                            // preedit 是 `b'z'd'hao'bu'hao` —— 与击键同域，正是要的。
                            boundary: result.boundary,
                            // **不标 is_abbrev**：它是整句解读，不是简拼候选。标了会沉进
                            // 简拼层，被前缀回退的部分候选压在下面。
                            ..Default::default()
                        },
                    );
                }
            }
        }

        // 3. DAG 前缀子短语查找（仅 start==0）：给出输入的各级前缀词，供从左到右分段上屏
        //    （「nihao」→「你」「你好」）。不取中段/后段子串（如「hao」→「好」）——非前缀词
        //    应在前缀提交后、剩余拼音重转时才出现，否则会污染整串候选并破坏分段语义。
        if syllables.len() >= 2 {
            for end in 1..syllables.len().min(6) {
                let code: String = syllables[..end].join("");
                if code == query {
                    continue;
                }
                // 子词组 code 是输入的真前缀（比输入*短*，如 nihao 的「你」(ni)），是合法的
                // 分段上屏候选，与精确同层按权重排（不可降权——否则罕见全长词「拟好」会压过
                // 常用子词组「你」）。只有 code 比输入*长*的补全词(step4)才算前缀补全降权。
                for h in self.lookup_with_fuzzy(&code, &syllables[..end]) {
                    push_unique(
                        &mut candidates,
                        h.text,
                        code.clone(),
                        h.weight,
                        h.order,
                        h.is_fuzzy,
                        false,
                        h.boundary,
                        false,
                    );
                }
            }
        }

        // 4. 前缀查找（补全词，code 比输入长，如 si→思考）→ 前缀层级，降到精确之后。
        //
        // 尾部残码存在时（如 meiy 的 "y" 未成音节，completed="mei" ⊂ query="meiy"）：**提升进
        // 完整匹配层**（`is_promoted_completion=true`）。否则 is_prefix=true 会被协调器
        // build_candidates 的有效前缀层比较压到全部精确匹配之后，数百条单字「没/每/美/…」
        // 会淹掉「没有」（用户翻 15+ 页才见，与 "不处理" 无异）。提升后有效前缀层为 false，
        // 且 code("meiyou") 长于 query("meiy") → is_partial=false（由 push_unique 自动计算），
        // 落进精确/子短语层、浮到 is_partial=true 的精确子串（没/每）之前。
        // is_prefix 本身恒表结构真值（码更长），排序提升与结构事实拆开，见
        // `wind_candidate::Candidate::is_promoted_completion`。
        // 无残码时（meiyou）不提升，前缀补全沉在精确匹配之后（正常行为）。
        // 残码时的上浮特权不是无条件的：双拼每 2 键 1 音节，奇数键必然有残码，
        // 若 30 条补全全部上浮，长输入下候选 2~5 位会被该前缀下的冷僻长词占满
        // （`zhonghuarenmingongheg` → 一串「中华人民共和国XXX法」w≤60），且随每次
        // 按键在「整句+单字」与「整句+条文名」两种形态间反复跳动。
        //
        // 用「补全距离 + 置信度」约束：近距离（补完手头音节）无条件上浮；远距离属于
        // 预测未输入内容，需 weight 达门槛。距离**不能单独用**——实测 +4 上既有合理的
        // 「中国人民解放军」(w=252) 也有噪音「…物权法」(w=21)，判别力全在 weight。
        //
        // boundary=0（无边界信息的旧词典/用户手输码）→ 距离算作 0 → 放行，
        // 与本文件其他位置「无边界信息一律降级放行」的处理一致。
        let trailing_partial = completed != query;
        let completed_syls = syllables.len() as u32;
        // 补全条数**跟随调用方请求量**，并 clamp 到 [30, MAX_COMPLETION_CANDIDATES]。
        //
        // 原先写死 30，代价是单字母输入（`b`/`y` 这类不成音节、无精确匹配的码）候选恒为 30 条、
        // 翻页翻不动。而**那个限制并没有省下成本**：wdat 前缀查询原是「遍历整棵子树 + 堆淘汰」，
        // 实测 `b` 取 30 条与取 5000 条同为 8~11ms，限制只压低了 String 分配，白白牺牲功能。
        // wdat v6 改为分支限界后（docs/design/prefix-topk-branch-and-bound.md），取数成本随条数
        // 而非子树规模增长，放开才真正划算：`b` 现在 300 条只要 0.9ms（改造前 30 条要 8ms）。
        //
        // ⚠️ **上限不能去掉**，瓶颈已从词库层转移到本函数的 `push_unique`：它按
        // `cands.iter().any(|c| c.text == text)` 线性查重，整体是 O(n²)。实测 `b`：
        // 300 条 0.9ms → 1000 条 3.5ms → 5000 条 **54.5ms**（5 倍数据量、15 倍耗时）。
        // 治本要把查重换成哈希索引，但 Viterbi 整句走 `insert(0)` 绕过该闭包、后续还有
        // `retain`，两者都会让哈希集与实际列表失同步，须单独立项处理。
        let completion_limit = max_candidates.clamp(30, MAX_COMPLETION_CANDIDATES);
        for h in dict.search_prefix_with_boundary(query, completion_limit) {
            let demote_to_prefix_layer = if trailing_partial {
                let distance = h.boundary.count_ones().saturating_sub(completed_syls);
                distance > COMPLETION_NEAR_SYLLABLES && h.weight < COMPLETION_FAR_WEIGHT_FLOOR
            } else {
                true // 无残码：正常前缀补全，沉在精确匹配之后
            };
            // is_prefix 恒表结构事实（search_prefix_with_boundary 返回的都是码更长的补全）；
            // 「是否沉到非精确层」的排序决策由 is_promoted_completion 承接（残码上浮即提升）。
            push_unique(
                &mut candidates,
                h.text,
                h.code,
                h.weight,
                h.order,
                false,
                true,
                h.boundary,
                !demote_to_prefix_layer,
            );
        }

        // 简拼族（纯简拼 step5 / 混合 step5b / 用户词 step6）是否命中了**整串**击键。
        // step 6.2 的前缀回退只在这三处全都落空时才启动——整串能打出词就不该降级。
        let mut abbrev_full_hit = false;

        // 整串是否**纯简拼形态**（每字母均为某音节首字母、且非完整音节序列）。
        // 提成变量供 step5 / step6 / step6.2 共用，`enable_abbrev` 仍在短路前（关闭时
        // 连 is_abbreviation 的 Dag 构建都省掉）。
        let stroke_is_plain_abbrev =
            self.config.enable_abbrev && AbbrevMatcher::is_abbreviation(abbr_query, trie);

        // 5. 简拼匹配（声母缩写，如 nh→你好）：查 wdat 预存的独立 AbbrevSection。
        //    仅当输入像简拼时才查（is_abbreviation：每字母均为某音节首字母、且非完整音节序列），
        //    避免对全拼输入做无谓查找。natural_order=999999 让简拼候选默认排在全拼之后。
        //    `enable_abbrev` 置于短路前：关闭时连 is_abbreviation 的 Dag 构建都省掉。
        //
        //    AbbrevSection 存的是**全拼码**（v5），故这里是「查索引拿码 → 走主表装配」两步。
        //    候选因此带上真实的 code 与 boundary：词频记账走 `cand_code` 取候选的 code，
        //    此前设成简拼串 `nh`，同一个词在简拼与全拼下遂走两个互不相认的计数。
        if stroke_is_plain_abbrev {
            for abbr_code in dict.search_abbrev(abbr_query, 10) {
                // 用 search_with_boundary 而非 search：拼音引擎直接持有 CachedDict、
                // 不经 SystemDictLayer，用 search() 会把边界丢在这里（P2b 踩过同款）。
                for h in dict.search_with_boundary(&abbr_code) {
                    // **音节数必须等于简拼字母数**。扁平码有损：`xian` 既是「西安」的
                    // xi|an（2 音节），也是「先」的 xian（1 音节）。索引里 `xa` 指向的是
                    // 前者的码，回查主表却会把后者一并捞出来 —— 实测 `xa` 出「先/线/弦/
                    // 现/县」一串单字。这是「存词」改「存码」引入的，存词时不会发生。
                    //
                    // boundary=0（无边界信息）放行，与全仓「任一侧为 0 即降级」的约定一致。
                    if h.boundary != 0 && h.boundary.count_ones() as usize != abbr_query.len() {
                        continue;
                    }
                    let before = candidates.len();
                    push_unique(
                        &mut candidates,
                        h.text,
                        abbr_code.clone(),
                        h.weight,
                        999999,
                        false,
                        // is_prefix=false：简拼不是前缀补全，层级由 is_abbrev 表达（见下）。
                        false,
                        h.boundary,
                        false,
                    );
                    // **简拼层标记，必须与 step6 的用户词简拼一致。**
                    //
                    // 此前这里借 `is_prefix=true` 沉底，而 step6 用 `is_abbrev=true`——
                    // 二者在 `cmp_match_layers` 里是**两个不同层级**（`is_abbrev` 是第一键、
                    // 比前缀层更沉），于是用户词简拼被整层压在系统词简拼之后，怎么调频都
                    // 翻不过来（层级是硬闸门）：`dblg` 下用户词「大菠萝哥」永远排在系统词
                    // 「夺不了冠」之后。同层之后两者才能按权重/词频正常竞争。
                    if candidates.len() > before {
                        candidates[before].is_abbrev = true;
                        abbrev_full_hit = true;
                    }
                }
            }
        }

        // 5b. 混合简拼：同一串里混用声母与完整音节（`nhao` = n + hao、`nih` = ni + h）。
        //     设计文档 `docs/design/pinyin-mixed-abbrev.md`；模式表示见 `mixed_abbrev`。
        //
        //     召回复用 step5 的同一条索引：模式的**声母投影键**退化成纯简拼串（`nhao` → `nh`），
        //     AbbrevSection 的键正是这个形状，故索引一个字节都不用改。混合信息留在模式里做
        //     后置校验（按 boundary 把全拼码切回音节序列，逐段比对），因此不会像纯简拼那样
        //     把 `nh` 下的词一股脑捞出来。
        //
        //     **短路：整串已被音节完整覆盖时不进这里。** `nihao`/`xian` 这类正常全拼输入
        //     因此零开销——而它们正是绝大多数击键。判据用 `completed_len`（step1 之前已算好，
        //     不额外建 Dag），但只在全拼下可用：双拼的 `completed_len` 说的是转换后的全拼串，
        //     与 `abbr_query`（原始击键，文档 §5 约束 4）不同域。双拼下改为「转换结果覆盖了
        //     整串击键」——双拼每 2 键 1 音节，覆盖完整即无残码可作声母段。
        let mixed_pats = if self.config.enable_abbrev && !mixed_covered {
            mixed_abbrev::mixed_patterns(abbr_query, trie)
        } else {
            Vec::new()
        };
        if !mixed_pats.is_empty() {
            // 同一串的多条解释常投影到同一个键（`nhao` 的 [n][hao] 与 `nih` 的 [ni][h] 都是
            // `nh`），按键去重后每个键只点查一次索引。
            let mut keys: Vec<&str> = mixed_pats.iter().map(|p| p.key()).collect();
            keys.sort_unstable();
            keys.dedup();
            for key in keys {
                // limit 比 step5 的 10 大一截：那边键即答案、取权重前 10 就够；这里拿到的
                // 码还要过一道逐段校验，**绝大多数会被滤掉**，取 10 条几乎必然一条不剩。
                for abbr_code in dict.search_abbrev(key, MIXED_ABBREV_INDEX_LIMIT) {
                    for h in dict.search_with_boundary(&abbr_code) {
                        // 无边界信息 → 判据不存在 → 不参与（不是放行，见 syllables_from_boundary）。
                        let Some(syls) =
                            mixed_abbrev::syllables_from_boundary(&abbr_code, h.boundary)
                        else {
                            continue;
                        };
                        if !mixed_pats
                            .iter()
                            .any(|p| p.key() == key && p.matches(&syls))
                        {
                            continue;
                        }
                        let before = candidates.len();
                        push_unique(
                            &mut candidates,
                            h.text,
                            abbr_code.clone(),
                            h.weight,
                            999999,
                            false,
                            false,
                            h.boundary,
                            false,
                        );
                        // **并入 is_abbrev 层，不新造层级、更不借用别的层级键**（文档 §5 约束 2）：
                        // 混合简拼与纯简拼是同质候选，分属两层就会让其中一侧被硬闸门整层压住、
                        // 词频永远翻不过来。这个标记同时让候选走在双拼边界校验的豁免侧（约束 1）
                        // ——它的 code 是词的全拼码，与当次击键根本不同域。
                        if candidates.len() > before {
                            candidates[before].is_abbrev = true;
                            abbrev_full_hit = true;
                        }
                    }
                }
            }
        }

        // 6. 用户/临时造词层（L：让拼音造的词显现）。查询与主词典相同的码——整串精确 +
        //    各前缀子码（你好 coded「nihao」当输入「nihaoma」时部分消费）+ 前缀补全——
        //    并入候选（dedup by text，已在系统词典出现的不重复加）。weight 由 store 记录给出，
        //    随后统一按 weight 排序；词频上浮交协调器 apply_freq_rerank（衰减软置前，frequency.md §4）。
        if let Some(store_dm) = &self.store_layers {
            let limit = max_candidates.max(50);
            let mut store_cands: Vec<Candidate> = store_dm.search(query, limit);
            if syllables.len() >= 2 {
                for end in 1..syllables.len().min(6) {
                    let code: String = syllables[..end].join("");
                    if code == query {
                        continue;
                    }
                    store_cands.extend(store_dm.search(&code, limit));
                }
            }
            store_cands.extend(store_dm.search_prefix(query, limit));

            // 用户长词上浮的**封顶基准**：提升后的补全不得越过「本次输入的最佳完整解」——
            // 码 == completed 的顶层候选（精确整词 / Viterbi 整句，均在此前步骤产出）。取其最大
            // 权重 - 1，与 step 6.5 整句降级同款手法，给出可预期的「就在最佳解之后」位置。
            // 无此类候选（如 qingfengshu 无精确词/整句）→ None → 不封顶，用户词落顶层按自身权重排。
            let completed_syls = syllables.len();
            let promotion_cap = candidates
                .iter()
                .filter(|c| {
                    !c.is_fuzzy
                        && !(c.is_prefix && !c.is_promoted_completion)
                        && !c.is_partial
                        && c.code == completed
                })
                .map(|c| c.weight)
                .max()
                .map(|w| w.saturating_sub(1));

            for mut c in store_cands {
                if c.text.is_empty() {
                    continue;
                }
                // 同文时**合并**而非整条丢弃。
                //
                // 旧行为（`any(|x| x.text == c.text) → continue`）让用户词在系统词典已有同文时
                // 完全失声：用户把「自激」配到 w=2_000_000_000 也纹丝不动，最终 weight 仍是系统的
                // 18 —— 用户词的 weight **从不参与比较**，「提权」这个动作在词已存在时无效。
                //
                // 合并规则：
                // - `weight` 取 **max**：用户配高权重即生效；用户权重更低时保留系统值，
                //   因为用户加词的意图是「提权」而非「降权」（降权应由词频/屏蔽机制表达）。
                // - `code` / `boundary` **保留已有候选的**，不换成用户词的。二者必须同进同出
                //   （`d4084b8` 已踩过此坑：composite 去重换 code 时 boundary 未跟随，配出
                //   「A 层的 code + B 层的 boundary」）。用户手输码常无边界信息（boundary=0），
                //   换过去等于把系统词典的真值边界抹成未知。
                // - 置 `meta.is_user_dict = true` 使来源可追溯（该字段目前无比较器读取，
                //   仅供诊断/UI）。
                // - 其余层级标志（is_fuzzy/is_prefix/is_partial/is_exact_code）**一律不动**：
                //   它们描述的是「这条候选相对本次输入处在哪一层」，由已有候选的来源路径决定，
                //   与用户是否也收录了同一个词无关。
                if let Some(existing) = candidates.iter_mut().find(|x| x.text == c.text) {
                    // ⚠️ 用户权重的生效条件必须与下面「新增」分支**对称**，否则用户词会借合并
                    // 路径绕过 `should_promote_user_completion` 与 `promotion_cap`：
                    //
                    // 系统前缀补全放开条数后（`completion_limit`），用户词与系统候选同文的概率
                    // 大增。此处若无条件 `max`，一个**远距离**补全词会被抬到用户权重并留在
                    // 补全提升层——实测单字母 `s` 配上 w=2e9 的用户词「筛选」，它直接夺走首选，
                    // 把「是」「上」这些高频单字挤到第 2 位起。补全只取 30 条时该词进不了系统
                    // 候选、走的是新增分支、被判据正确拦下，于是问题只在放开后显形。
                    //
                    // 分层处理，与新增分支一一对应：
                    // - 精确匹配（`is_prefix=false`）：用户提权全效，这是「加词提权」的本义。
                    // - 前缀补全：须满足同一个提升判据，且同样受 `promotion_cap` 封顶。
                    // - 不满足判据的远距离补全：保留系统权重，沉在补全层（不是丢弃）。
                    if !existing.is_prefix {
                        existing.weight = existing.weight.max(c.weight);
                    } else if should_promote_user_completion(
                        completed_syls,
                        trailing_partial,
                        existing.boundary,
                    ) {
                        let w = existing.weight.max(c.weight);
                        existing.weight = promotion_cap.map_or(w, |cap| w.min(cap));
                    }
                    existing.meta.is_user_dict = true;
                    continue;
                }
                c.source = CandidateSource::Pinyin;
                // 与 push_unique 一致：store 层的前缀子码命中也是子短语，降到完整匹配之后。
                c.is_partial =
                    !c.is_prefix && c.code.len() < query.len() && query.starts_with(&c.code);
                // 用户/临时词的前缀补全（is_prefix=true，码更长）：打到词尾附近就提升进完整
                // 匹配层，否则被首音节同音子短语整层淹没（长词打到第 3-4 音节才给的根因）。
                // is_prefix 保持结构真值不动，排序提升由 is_promoted_completion 承接。
                if c.is_prefix
                    && should_promote_user_completion(completed_syls, trailing_partial, c.boundary)
                {
                    c.is_promoted_completion = true;
                    if let Some(cap) = promotion_cap {
                        c.weight = c.weight.min(cap);
                    }
                }
                candidates.push(c);
            }

            // 简拼匹配（用户/临时造词层）：用户词写入时只存全拼码，不像系统词库那样离线
            // 预建 AbbrevSection——规模小，现算即可（枚举该 schema 下全部用户/临时词，
            // 按各词自带的音节边界取声母比对，见 abbrev_of_code）。natural_order 对齐
            // step5 系统简拼候选，同样排在全拼之后。
            // 混合简拼（step 5b）在这里共用同一次全表枚举：`nih` 这类形态 `is_abbreviation`
            // 判假（`i` 不是任何音节首字母）却有合法混合解释，故入口条件是两者取或。
            if stroke_is_plain_abbrev || !mixed_pats.is_empty() {
                for mut c in store_dm.search_prefix("", 0) {
                    if c.text.is_empty() || candidates.iter().any(|x| x.text == c.text) {
                        continue;
                    }
                    // 比对基准是原始击键（见 `abbr_query`）：双拼下 query 已是转换结果，
                    // 拿它比对永远匹配不上用户敲的简拼。
                    let plain =
                        self.abbrev_of_code(&c.code, c.boundary).as_deref() == Some(abbr_query);
                    // 混合简拼：按 boundary 切回音节序列逐段比对（无边界 → 无判据 → 不参与）。
                    // 与系统词侧走同一批 `mixed_pats`，判据完全一致，只是这边不经索引——
                    // 用户词规模小，现算即可（与 `abbrev_of_code` 那条注释同理）。
                    let mixed = !plain
                        && mixed_abbrev::syllables_from_boundary(&c.code, c.boundary)
                            .is_some_and(|syls| mixed_pats.iter().any(|p| p.matches(&syls)));
                    if !plain && !mixed {
                        continue;
                    }
                    c.source = CandidateSource::Pinyin;
                    // **保留全拼码**（连同同域的 boundary），不覆盖成简拼串。
                    //
                    // 词频记账走 `cand_code`（取候选的 code），覆盖成 `xan` 会让同一个词在
                    // 简拼与全拼下走两个互不相认的计数——用简拼练熟的词切回全拼一点不认。
                    //
                    // `consumed_length` 不受影响：它的判据是 `query.starts_with(&c.code)`，
                    // 简拼下 `xan` 不以 `xianning` 开头 ⇒ 落 else 分支取 `query.len()`，
                    // 仍是「消费整串」，与覆盖时同值。
                    c.is_prefix = false;
                    c.is_partial = false;
                    // 简拼层标记。此前借 `is_fuzzy` 沉底——那是模糊音的「召回来源」标记，
                    // 与简拼无关；`is_fuzzy` 退出 `cmp_match_layers` 后借用会把简拼一起放上来。
                    c.is_abbrev = true;
                    c.natural_order = 999999;
                    candidates.push(c);
                    abbrev_full_hit = true;
                }
            }
        }

        // 6.2 简拼族**前缀回退**：整串一无所获时，退到最长的能命中的前缀，
        //     余下字母作残码留给下一次输入（分步上屏）。
        //
        //     真机现场（连打 `bzdnihaobuhao`）：输到 `bzdha` **整串空码**。`bzd` 明明能出
        //     「不知道」，但简拼族的召回一直是「全串或无」——索引按完整简拼串点查、混合
        //     模式要求覆盖整串，于是简拼一旦长过任何单个词就彻底没有候选。全拼下有 step3
        //     子短语与 step4 前缀补全兜着（`nihaobu` 照样出「你好」且只消费 5 字节），
        //     简拼下此前没有任何对应机制。**这不是混合简拼引入的，纯简拼一直如此。**
        //
        //     **只在整串一无所获时降级**，故整串能命中的情形与改动前逐字节一致（零回归）。
        //
        //     ★★ **不做「最长匹配、一有产出即停」——多个切点的候选共存，同层按词频竞争。**
        //     首版那样 break 掉更短的切点，等于把「切点长短」做成了**硬闸门（惩罚 ∞）**：
        //     `bzdhaobuhao` 的 `bzdh` 恰好是「表彰大会」的简拼（w=93），于是它把 `h` 抢走、
        //     短切点 `bzd`（→「不知道」，**w=62492，高 672 倍**）一条都进不来，剩下的
        //     `aobuhao` 成了垃圾。一个结构性偏好压过了三个数量级的词频差异。
        //     切点长短是**来源差异**、不是结构质量差异，只配走 weight，不配做布尔层级键
        //     （同款教训见模糊拼音 `is_fuzzy` 从层级键改惩罚的那一轮）。
        //     代价是候选变多，用 `MAX_FALLBACK_PER_CUT` / `MAX_FALLBACK_TOTAL` 两道限流兜住。
        //
        //     ⚠️ 这仍是**启发式**，不是智能组句：简拼段不进 lattice/Viterbi，`bzd|haobuhao`
        //     与 `bzdh|aobuhao` 之间没有语言模型仲裁，只靠词频。真正的解法是把简拼段作为
        //     词图节点纳入整句解码（见 §4.8 待办）。
        //
        //     ⚠️ 门槛问的必须是「这串里**有成不了音节的部分**吗」（`!mixed_covered`），
        //     而不是「整串本身是不是一条合法的简拼/混合模式」。两处都栽过：
        //
        //     - 只看 `!abbrev_full_hit`（简拼族没命中）⇒ **纯全拼也被拖进降级**：`meiyou`
        //       的 is_abbreviation 因 `i` 判假、整串又被音节完整覆盖使混合模式为空，于是
        //       「一无所获」成立 ⇒ 退到 `meiy` 后 `[mei][y]` 命中「没有」，凭空多出
        //       `is_abbrev` 候选、破坏「前缀补全排在精确匹配之后」的层级
        //       （`test_pinyin_trailing_partial_prefix_floats_above_exact` 当场抓到）。
        //     - 改用「整串是简拼族形态」又**把长串挡在门外**：`bzdnihaobuh` 最少需要
        //       `[b][z][d][ni][hao][bu][h]` 七段、超过 `MAX_SEGMENTS`，于是整串模式为空、
        //       门槛不过 ⇒ 连打到第 11 键**彻底空码**（真机报的正是这个）。
        //
        //     `!mixed_covered` 两边都对：`meiyou`/`nihao` 被音节完整覆盖 ⇒ 不降级；
        //     凡含成不了音节的字母（`bzdha`、`bzdnihaobuh`）⇒ 允许降级，且**逐前缀的严格
        //     判定在 `recall_abbrev_prefix` 内部**（每个 stroke 各自过 is_abbreviation /
        //     mixed_patterns），门槛只负责挡掉纯全拼、不负责替前缀把关。
        if self.config.enable_abbrev
            && !abbrev_full_hit
            && !mixed_covered
            && abbr_query.len() > MIN_ABBREV_STROKE
        {
            let mut cuts_with_hits = 0usize;
            for take in (MIN_ABBREV_STROKE..abbr_query.len()).rev() {
                if !abbr_query.is_char_boundary(take) {
                    continue;
                }
                let before = candidates.len();
                self.recall_abbrev_prefix(&abbr_query[..take], take, &mut candidates);
                if candidates.len() > before {
                    cuts_with_hits += 1;
                    if cuts_with_hits >= MAX_FALLBACK_CUTS {
                        break;
                    }
                }
            }
        }

        // 手动分隔符边界过滤：用户以 `'` 强制音节边界后，凡「码恰好落在某音节边界、但字数
        // 与所跨音节数不符」的候选被剔除——如 xi'an 强制 [xi,an]，则单字「先」(code=xian,
        // 跨 2 音节却仅 1 字) 不该出现；「西」(code=xi,1 字 1 音节)、整句「西安」(2 字 2 音节) 保留。
        // 码不落在任何边界的候选（如前缀补全）不受影响。
        if has_sep {
            let syls = &syllables;
            candidates.retain(|c| match syllable_span(syls, &c.code) {
                Some(k) if k >= 1 => c.text.chars().count() == k,
                _ => true,
            });
        }

        // 裸声母（无完整音节，如 "m"/"zh"）单字优先：候选全为前缀补全词（is_prefix=true），
        // 纯按词频排会让高频多字词（没有/目前）压过单字（吗/么）——不合直觉。给单字提权使其
        // 排在多字词之前（对齐主流输入法首字优先，见 BARE_INITIAL_SINGLE_CHAR_BOOST）。
        // 仅此情形——完整音节输入的单字已靠精确层级(is_prefix=false)就位，无需提权。
        if syllables.is_empty() {
            for c in candidates.iter_mut() {
                if c.text.chars().count() == 1 {
                    c.weight = c.weight.saturating_add(BARE_INITIAL_SINGLE_CHAR_BOOST);
                }
            }
        }

        // 6.5 整句让位于精确整词：**降级，不销毁**
        //
        // 输入 `lianzhengtixing` 时用户词「廉政提醒」严格覆盖整串，而 Viterbi 拼出的
        // 「连整体性」靠 SENTENCE_WEIGHT_BASE(30M) 的量纲优势恒占首位——30M 碾压一切词频，
        // 用户把词加进词库、配再高的权重也换不回首选。
        //
        // 早先试过的「有精确整词就不构造整句」是**销毁**：整句连候选都不在，用户想选也
        // 选不到，代价不可挽回。这里改为降级——整句仍在候选里，只是排到精确整词之后，
        // 代价是「多按一次」。
        //
        // 位置必须在 step 6 之后：用户/临时层的词到 step 6 才并进 `candidates`，
        // 而「用户加词」正是本功能要服务的场景，放在 step 2 旁边会看不见用户词。
        //
        // 只降 **Viterbi 新合成** 的整句（`synthesized_sentence`）。与词典整词同文而合并的
        // 那一支（nihao→你好、gonghe→共和）本身就是精确整词，无处可让。
        //
        // ## 为什么用「相对权重」而不是固定的降级基数
        //
        // 精确整词的权重是原始词频，量纲跨度极大（系统词条 1~2e6，用户词可配到 2e9）。
        // 任何固定常数都会在某一侧翻车：偏高则用户词压不住整句（原问题复发），偏低则整句
        // 沉到普通候选里。取相对值使结果与词频数值无关。
        //
        // ## 为什么是 `max - 1` 而不是 `min - 1`
        //
        // 取 `max`：整句只让位给**最强的那个**精确整词，恒定停在它之后。这给用户一个
        // 可预测的位置（「整句就在第二条」），而 `min - 1` 会让 w=8 的冷僻同码词也压过
        // 引擎对整串输入的最优解读——说不通，且名次随该输入下同码词条数浮动、无从预期。
        //
        // **多个精确整词并列于 max 时，整句排在它们全部之后**：并列者权重皆为 `max`，
        // 严格大于 `max - 1`，由算式保证，无需额外判据（见
        // `demoted_sentence_falls_below_all_max_weight_exact_words`）。
        //
        // ## `max - 1` 与其它候选在 weight 上并列时
        //
        // 同层内**只有**精确整词与整句本身：子短语（`is_partial`）、前缀补全（`is_prefix`）、
        // 模糊命中（`is_fuzzy`）由 `cmp_match_layers` 整体压在下一层，与权重无关；协调器侧
        // 的短语（`is_prefix=true`）同理，引导键导航候选与码表精确候选则由
        // `cmp_exact_first` 挡在上一层——两者都在 `candidate_display_order` 中先于权重比较。
        // 故权重并列只可能发生在整句与**另一个精确整词**之间，此时落到 base_order /
        // natural_order 决定谁先，无论结果如何都不破坏「整句在普通候选之前」这条不变量。
        //
        // 三个排序器（本函数、协调器 `candidate_display_order`、`freq_rerank`）都以
        // `cmp_match_layers` 为首要键，故这个位置在整条链路上一致。
        //
        // `is_sentence` 不清：它表达「引擎对整串输入的最优解读」这个**来源**语义，
        // 降级是**排序**决策，另立 `is_sentence_demoted` 表达（`freq_rerank` 的顶部锚定
        // 据此豁免，否则那里不看 weight，本处降权会被整个顶回去）。
        // 两类整句需要让位于精确整词：
        // ① Viterbi **新合成**的整句（词典无此词，由多节点拼出）；
        // ② **模糊命中**的整句（词典有此词，但经模糊变体召回——如 `sixiang` 经 s↔sh 命中
        //    词条「是想」，在词图里成为覆盖全串的单节点被 Viterbi 选中）。
        //
        // ② 必须走这条路而非 `fuzzy_penalized` 的比例折扣：整句拿的是 `SENTENCE_WEIGHT_BASE`
        // (3e7) 基准分，与词典词的词频量纲（1e2~1e6）差几个数量级，任何比例折扣都压不下来
        // （0.01 折扣后仍有 3e5，照样碾过「思想」的 26133）。而「降到精确整词之下」在语义上
        // 恰好对：模糊解读让位于精确解读。
        //
        // 该判据还天然区分了两种场景，无需额外条件：`sixiang` 存在精确整词「思想」故
        // 「是想」让位；`zongguo` 下没有以 zongguo 为码的精确整词（exact_max=None），
        // 「中国」照常居首——这正是模糊音想要的效果。
        let mut demote_targets: Vec<String> = synthesized_sentence.iter().cloned().collect();
        for c in candidates.iter() {
            if c.is_sentence && c.is_fuzzy && !demote_targets.contains(&c.text) {
                demote_targets.push(c.text.clone());
            }
        }
        for sent in demote_targets {
            // 「精确整词」判据：码恰好等于已消费输入 `completed`，且非模糊命中、
            // 不在前缀补全/子短语层。含系统词库与 step 6 并入的用户/临时层。
            let exact_max = candidates
                .iter()
                .filter(|c| {
                    c.text != sent
                        && !c.is_fuzzy
                        && !c.is_prefix
                        && !c.is_partial
                        && c.code == completed
                })
                .map(|c| c.weight)
                .max();
            if let Some(max_w) = exact_max
                && let Some(c) = candidates.iter_mut().find(|c| c.text == sent)
            {
                c.weight = max_w.saturating_sub(1);
                c.is_sentence_demoted = true;
            }
        }

        // 6.6 整句解「有同码竞争者」标记：**只摘词频锚定，不动 weight**
        //
        // 上面的 6.5 处理的是「整句该不该让位」（合成解/模糊解 vs 精确整词），本节处理的是
        // 另一件事：整句解**自己就是**一个词典精确整词，而同码还有别的精确整词。
        // `siyuan` 的「寺院」即如此——它经 step 2 的同文合并分支继承了整句身份，而
        // `freq_rerank` 的顶部锚定是硬闸门（衰减分连算都不算），于是同码的「思源」
        // 无论选中多少次都翻不过它。`gonghe` 共和/恭贺、`nihao` 你好/拟好同构。
        //
        // **判据与 6.5 的 `exact_max` 过滤器逐条一致**（同码、非模糊、不在补全/子短语层），
        // 差别只在这里要的是「存在性」而非「最大权重」。两处若不一致，会出现「6.5 认为有
        // 竞争者而降级、6.6 认为没有而仍锚定」这类自相矛盾的状态。
        //
        // 不动 weight 是有意的：无词频记录时整句仍须凭 `SENTENCE_WEIGHT_BASE` 量纲居首
        // （它确实是引擎对整串的最优解读），本标记只让它在**有用户实际选择数据时**接受
        // 挑战。已降级的整句跳过——它已经让过位了，weight 也已被压低。
        //
        // 无竞争者的整句（`woshizhongguoren` 这类纯合成解、step 1.5 的超长词典整词）
        // 不置位、维持锚定：那里没有「用户明确选过另一个同码词」这个事实可依据。
        let contested: Vec<String> = candidates
            .iter()
            .filter(|c| c.is_sentence && !c.is_sentence_demoted)
            .filter(|s| {
                candidates.iter().any(|o| {
                    o.text != s.text
                        && !o.is_fuzzy
                        && !o.is_prefix
                        && !o.is_partial
                        && o.code == completed
                })
            })
            .map(|c| c.text.clone())
            .collect();
        for text in contested {
            // 同码最强竞争者的权重——供**合成整句**当可比量级用（判据与上面 `contested`
            // 的过滤器逐条一致）。先算完再取可变借用。
            let peer_max = candidates
                .iter()
                .filter(|o| {
                    o.text != text
                        && !o.is_fuzzy
                        && !o.is_prefix
                        && !o.is_partial
                        && o.code == completed
                })
                .map(|o| o.weight)
                .max();
            if let Some(c) = candidates.iter_mut().find(|c| c.text == text) {
                c.is_sentence_contested = true;
                // contested 会摘掉词频锚定、下场按量级竞争，而它的 weight 是
                // `SENTENCE_WEIGHT_BASE` 量纲（「排第一」的编码，不可比）。经 step 2 同文
                // 合并的整句已在那里留了 `dict_weight`（词库原值），但**合成整句没有**
                // ——本判据只要求「是整句且有同码竞争者」，不要求它自己是词典词，
                // `woshizhongguoren` 那类纯合成解同样会走到这里。
                //
                // 漏填的后果是静默的：比较器 `unwrap_or(weight)` 会拿到 3e7，竞争者
                // 无论被选中多少次都翻不过，词频维度对该编码整体失效——正是本次改造
                // 要修的那个 bug 换个入口重现。故用同码最强竞争者的量级顶上：语义即
                // 「与最强竞争者平级」，无词频时靠整句 tie-break 居首，有词频时可被反超。
                if c.dict_weight.is_none()
                    && let Some(pm) = peer_max
                {
                    c.dict_weight = Some(pm);
                }
            }
        }

        // 引擎内部排序（层级对齐 Go：完整匹配 >> 子短语 >> 前缀补全 >> 模糊）：
        // ① 非模糊优先于模糊（is_fuzzy=false 在前）；② 完整匹配/子短语(is_prefix=false)优先于
        // 前缀补全(is_prefix=true)；③ 完整匹配(is_partial=false)优先于子短语(is_partial=true)
        // ——对齐 Go coverage 分层，避免高频单字「报/宝」插进完整词「保安」「报案」之间；
        // ④ 同层内按权重降序、自然序升序。
        // 使输入 si 时：精确单字「四/死」> 前缀补全「思考/似乎」> 模糊命中「是」；
        // 输入 baoan 时：完整词「保安」「报案」> 子短语单字「报/宝」。
        // 双拼真值边界校验：双拼把音节边界定死了，候选的词典边界必须与之吻合。
        // 在排序/截断**之前**过滤——否则会先截断再过滤，把该出的候选挤掉。
        // 词典无边界信息的候选（用户手输码/五笔/旧数据）boundary=0，一律放行（降级回 DAG 行为）。
        if let Some(r) = &sp_result {
            let sp_mask = sp_boundary_mask(r);
            if sp_mask != 0 {
                let full_len = r.full_pinyin.len();
                candidates.retain(|c| {
                    // **简拼候选豁免。** 它的 code 是词的全拼码（`xianning`），与当次击键
                    // （`xan`）根本不同域，边界比较无意义且必然不符——双拼下打简拼会被整批
                    // 误杀。原设计里简拼候选 boundary 恒为 0，靠「任一侧为 0 即放行」自然
                    // 豁免；wdat v5 让简拼改走主表装配、带上了真实边界，那条隐式豁免随之
                    // 失效，故须显式写出。同理于模糊变体——那边至今仍靠 boundary=0 豁免。
                    c.is_abbrev || boundary_compatible(c.boundary, sp_mask, c.code.len(), full_len)
                });
            }
        }

        candidates.sort_by(|a, b| {
            wind_candidate::cmp_match_layers(a, b)
                .then(b.weight.cmp(&a.weight))
                .then(a.natural_order.cmp(&b.natural_order))
        });
        candidates.truncate(max_candidates);

        // 分段上屏所需：标注每个候选实际消费的输入字节数。
        // code 为 input（全拼）的前缀（如 "ni" ⊂ "nihao"）→ 只消费该前缀，选中后保留剩余拼音续转；
        // 否则（整句/前缀补全/非前缀子串）消费整串。0 表示未知（由调用方按整串处理）。
        // 双拼激活时：全拼字节数需通过 map_consumed_length 回算为双拼原始键数，
        // 否则变长音节（2键→3字节，如 zh/ch/sh）会错误消费/越界双拼键缓冲区。
        for c in candidates.iter_mut() {
            // 简拼族前缀回退（step 6.2）的候选**自带击键域的消费数**，必须跳过这套计算。
            //
            // 简拼的 code 是词的全拼码（`buzhidao`），不以 query（`bzdha`）开头 ⇒ 会落到
            // else 分支取 `query.len()`，即「消费整串」——选中「不知道」后余下的 `ha` 会被
            // 一起吃掉，分步上屏当场失效。整串简拼候选的 consumed_length 保持 0、照旧走
            // 这条计算（消费整串正是它要的），故此判据不影响它们。
            //
            // 双拼下同样跳过：简拼的判据本就走 raw_input（击键域），consumed_length 的语义
            // 就是「消费多少击键」，无须再过 map_consumed_length 换算。
            if c.is_abbrev && c.consumed_length != 0 {
                continue;
            }
            // 以剥除分隔符后的 query 为基准计算消费长度（无分隔符时 query==input）。
            let fp_consumed = if !c.code.is_empty() && query.starts_with(&c.code) {
                c.code.len()
            } else {
                query.len()
            };
            c.consumed_length = match &sp_result {
                Some(r) => r.map_consumed_length(fp_consumed),
                // 全拼含手动分隔符：query 是剥除 `'` 的串，需回映射到含 `'` 的原始输入空间，
                // 否则协调器按含 `'` 缓冲切片时会残留尾字符（xi'an 选「西安」残 "n"）。
                // 与双拼分支同一套 SylSpan 表示，只是 span 来源不同（切分 vs 双拼转换）。
                None if has_sep => interp::map_fp_to_raw(&sep_spans, fp_consumed, input),
                None => fp_consumed,
            };
        }

        let (mut preedit_display, completed_syllables, partial_syllable) =
            self.compute_composition(input);

        // 预编辑区**跟随首选候选**（用户拍板的策略）。
        //
        // 多路径切分后，`maximum_match` 那条不再必然是首选候选走的那条：`xianjiaotongdaxue`
        // 首选「西安交通大学」实走 `xi|an|jiao|tong|da|xue`，而 mm 给的是 `xian|jiao|…`。
        // 显示 mm 就与用户看到的候选自相矛盾。候选自带 `boundary`（整句由解码器回填真实
        // 路径，词典命中则是词库真值），据此还原其切分即可，无须另建通道。
        //
        // 只在「无双拼、无手动分隔符、首选覆盖已完成音节前缀且带边界信息」时接管——
        // 双拼的 preedit 另有 build_raw_preedit 负责（下方覆盖），分隔符段的 `'` 是用户
        // 亲手打的硬边界不容改写，无边界信息则无从跟随。其余情形保持 mm 显示不变。
        //
        // 简拼/混合简拼另走一支：它们的 code 与击键**不同域**，故 `top.code == completed`
        // 恒不成立（`nhao` 一个完整音节都切不出，`completed` 是空串），此前因此完全没有
        // 分隔显示——`nhao` 原样显示为 `nhao`，`nh` 显示为 `nh`。见下方分支。
        if sp_result.is_none() && !has_sep {
            if let Some(top) = candidates.first() {
                if top.boundary != 0 && top.code == completed && !completed.is_empty() {
                    preedit_display = render_preedit(completed, top.boundary, &partial_syllable);
                } else if top.is_sentence && top.boundary != 0 && top.code == raw_input {
                    // 混合整句（step 2b）：它的 code 是整串**击键**、boundary 也在击键空间
                    // （简拼段每字母一位），故直接渲染击键串即可，得 `b'z'd'hao'bu'hao`。
                    // 走不到上一支是因为 `completed` 对这类输入恒为空串——`bzdhaobuhao`
                    // 从位置 0 就切不出完整音节。
                    preedit_display = render_preedit(raw_input, top.boundary, "");
                } else if top.is_abbrev && top.boundary != 0 {
                    // 简拼（`nh`→`n'h`）与混合简拼（`nhao`→`n'hao`）的分段显示。
                    //
                    // **切的是击键串，不是候选的 code。** 直接渲染 code 会显示成 `ni'hao`：
                    // 用户只敲了 4 键却看到 5 个字母，退格与光标编辑立刻错位。这里用候选
                    // 自带的真值音节序列去切 `raw_input`，声母段吃 1 字节、音节段吃整段
                    // （见 `render_keystroke_preedit`）。
                    //
                    // 对不上就保持原显示：preedit 是显示层，宁可少一个分隔符，
                    // 也不能给出与击键长度不符的串。
                    if let Some((head, used)) =
                        mixed_abbrev::syllables_from_boundary(&top.code, top.boundary).and_then(
                            |syls| mixed_abbrev::render_keystroke_preedit(raw_input, &syls),
                        )
                    {
                        if used == raw_input.len() {
                            preedit_display = head;
                        } else if top.consumed_length != 0 && used == top.consumed_length {
                            // 部分匹配（step 6.2 前缀回退）：余下的击键**自己再切一遍**，
                            // 别整段甩上去。`bzdnihaob` 选中「不知道」(消费 bzd) 后尾巴是
                            // `nihaob`，含完整音节 `ni`/`hao` + 残码 `b`——整段追加会显示成
                            // `b'z'd'nihaob`，该切的地方没切。走 compose_segment 得
                            // `ni'hao'b`，最终 `b'z'd'ni'hao'b`。
                            let (tail, _, _) = self.compose_segment(&raw_input[used..]);
                            preedit_display = format!("{head}'{tail}");
                        }
                        // 其余情形（走不完且不是部分候选）保持原显示：那说明模式与击键
                        // 对不上，硬拼只会给出与击键长度不符的串。
                    }
                }
            }
        }

        // Fix A：双拼激活时，preedit 改为显示用户实际输入的原始按键（按双拼音节边界以 `'` 分隔），
        // 而非转换后的全拼。仅覆盖 preedit_display；候选/completed_syllables/partial_syllable/
        // consumed_length 仍保持全拼语义不变。
        if let Some(r) = &sp_result {
            preedit_display = build_raw_preedit(raw_input, r);
        }

        let has_partial = !partial_syllable.is_empty();
        let is_empty = candidates.is_empty();

        Ok(ConvertResult {
            candidates,
            // 拼音恒为拆分形态（供混输高亮跟随：高亮拼音候选时取此串）。
            preedit_pinyin: preedit_display.clone(),
            preedit_display,
            completed_syllables,
            partial_syllable,
            has_partial,
            should_commit: false,
            commit_text: String::new(),
            is_empty,
            should_clear: false,
            // 拼音无「全码/空码补全」概念（`single_code_*` 是码表专属）。
            completion_hint: None,
        })
    }

    fn reset(&self) {}

    fn engine_type(&self) -> EngineType {
        EngineType::Pinyin
    }

    /// 反查 `(code, text)` 在词典里的音节边界（点查，不做推断）。查不到返回 0。
    fn syllable_boundary_of(&self, code: &str, text: &str) -> u64 {
        self.dict
            .search_with_boundary(code)
            .into_iter()
            .find(|h| h.text == text)
            .map(|h| h.boundary)
            .unwrap_or(0)
    }

    /// 为词语生成带空格的全拼音节码（多音字按词典权重消歧）。
    /// 单字读音索引按词典懒构建并缓存。含无读音字符时返回 `None`。
    fn generate_word_pinyin(&self, word: &str) -> Option<String> {
        let idx = self
            .char_pinyin_idx
            .get_or_init(|| CharPinyinIndex::build(&self.dict));
        generate::generate_word_pinyin(&self.dict, idx, word)
    }

    fn is_possible_pinyin_sequence(&self, prefix: &str) -> bool {
        // 条件1：整个前缀本身是某合法音节的前缀（如 zhon→zhong），长度 >=2 过滤单字母简拼。
        if prefix.len() >= 2 && self.trie.is_prefix(prefix) {
            return true;
        }
        // 条件2：从起始连续完整音节 + 合法尾部前缀。首音节须非单字母。
        let (completed, end_pos) = self.contiguous_completed_from_start(prefix);
        if completed.is_empty() || completed[0].len() < 2 {
            return false;
        }
        if end_pos >= prefix.len() {
            return true;
        }
        self.trie.is_prefix(&prefix[end_pos..])
    }

    fn is_whole_syllable_pinyin(&self, prefix: &str) -> bool {
        // 整体即单个完整音节（wang/shen 等填满码长的场景）。
        if self.trie.is_syllable(prefix) {
            return true;
        }
        // 多音节：连续完整音节恰好覆盖整个前缀，且首音节非单字母简拼。
        let (completed, end_pos) = self.contiguous_completed_from_start(prefix);
        if completed.is_empty() || completed[0].len() < 2 {
            return false;
        }
        end_pos == prefix.len()
    }

    fn has_non_initial_single_letter_syllable(&self, prefix: &str) -> bool {
        let (completed, _) = self.contiguous_completed_from_start(prefix);
        completed.iter().skip(1).any(|s| s.len() == 1)
    }

    fn completed_syllable_count(&self, prefix: &str) -> usize {
        self.contiguous_completed_from_start(prefix).0.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pinyin::shuangpin::{Layout, ShuangpinConverter};
    use wind_dict::codetable::CodetableDict;
    use wind_store::Store;

    fn empty_engine() -> PinyinEngine {
        PinyinEngine::new(
            Config::default(),
            CachedDict::Memory(CodetableDict::empty()),
        )
    }

    // ── 音节分析（顶码歧义裁决用；对齐 Go isPossiblePinyinSequence / isWholeSyllablePinyin /
    //    hasNonInitialSingleLetterSyllable）。trie 为封闭标准音节集，不依赖词典。──

    #[test]
    fn possible_pinyin_sequence_matches_go_cases() {
        let e = empty_engine();
        // 完整音节 / 音节前缀 / 完整音节+尾部前缀 → true
        assert!(e.is_possible_pinyin_sequence("wang")); // 单完整音节
        assert!(e.is_possible_pinyin_sequence("zhon")); // zhong 的前缀
        assert!(e.is_possible_pinyin_sequence("yans")); // yan + 尾部前缀 s
        assert!(e.is_possible_pinyin_sequence("naap")); // na + a + 前缀 p
        // 非拼音 / 首音节单字母 → false
        assert!(!e.is_possible_pinyin_sequence("rcqn")); // 无完整音节也非前缀
        assert!(!e.is_possible_pinyin_sequence("gggg")); // g 非合法音节/前缀
        assert!(!e.is_possible_pinyin_sequence("abcd")); // 首音节 a 为单字母
    }

    #[test]
    fn whole_syllable_pinyin_matches_go_cases() {
        let e = empty_engine();
        assert!(e.is_whole_syllable_pinyin("wang")); // 单完整音节
        assert!(e.is_whole_syllable_pinyin("aipu")); // ai+pu 恰好覆盖
        assert!(!e.is_whole_syllable_pinyin("zhon")); // 残缺前缀（非完整音节）
        assert!(!e.is_whole_syllable_pinyin("yans")); // yan + 残缺 s
        assert!(!e.is_whole_syllable_pinyin("abcd")); // 首音节单字母简拼
    }

    #[test]
    fn non_initial_single_letter_syllable_matches_go_cases() {
        let e = empty_engine();
        assert!(e.has_non_initial_single_letter_syllable("naap")); // na + a（第二音节单字母）
        assert!(!e.has_non_initial_single_letter_syllable("yans")); // yan + 残缺 s（残缺不计）
        assert!(!e.has_non_initial_single_letter_syllable("aipu")); // ai + pu 皆双字母
        assert!(!e.has_non_initial_single_letter_syllable("abcd")); // 首位单字母不算「非首位」
    }

    /// 构造含指定 (code, text) 词条的最小拼音引擎（每条 weight=100，order 递增）。
    fn engine_with_words(words: &[(&str, &str)]) -> PinyinEngine {
        let mut raw = CodetableDict::empty();
        for (i, (code, text)) in words.iter().enumerate() {
            raw.merge_single(code.to_string(), text.to_string(), 100, i as i32);
        }
        PinyinEngine::new(Config::default(), CachedDict::Memory(raw))
    }

    /// Task 8 Step 2：手动分隔符强制音节硬边界。
    /// 词典含 "xian"→"先" 与 "xi"/"an" 单字；带分隔符 xi'an 强制切分 [xi,an]，
    /// 跨界单音节词「先」(code=xian 却仅 1 字) 不得出现；preedit 保留手动 `'`。
    #[test]
    fn separator_forces_syllable_boundary() {
        let e = engine_with_words(&[("xian", "先"), ("xi", "西"), ("an", "安")]);
        let r = e.convert("xi'an", 50).unwrap();
        assert!(
            !r.candidates.iter().any(|c| c.text == "先"),
            "分隔符应阻止跨界音节 xian 匹配，实际: {:?}",
            r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
        assert!(
            r.preedit_display.contains('\''),
            "preedit 应保留手动分隔符，实际: {:?}",
            r.preedit_display
        );
        assert!(
            !r.candidates.is_empty(),
            "分隔符切分后仍应有候选（如「西」）"
        );
    }

    /// Task 8 Step 2：末尾分隔符必须立即显示，且不清空候选。
    #[test]
    fn trailing_separator_kept_in_preedit() {
        let e = engine_with_words(&[("ni", "你")]);
        let r = e.convert("ni'", 50).unwrap();
        assert!(
            r.preedit_display.ends_with('\''),
            "末尾分隔符必须立即显示，实际: {:?}",
            r.preedit_display
        );
        assert!(!r.candidates.is_empty(), "末尾分隔符不应清空候选");
    }

    /// Task 8 自审：五个边界（空段/开头 '/连续 ''/纯 '/末尾 '）均不 panic，
    /// 且手动分隔符在 preedit 中原样保留。
    #[test]
    fn separator_edge_cases_no_panic() {
        let e = engine_with_words(&[("ni", "你"), ("hao", "好"), ("xi", "西"), ("an", "安")]);

        // 开头 '：xi 段仍应产候选，preedit 以 ' 起头
        let r = e.convert("'xi", 20).unwrap();
        assert!(r.preedit_display.starts_with('\''), "开头分隔符应保留");
        assert!(r.candidates.iter().any(|c| c.text == "西"));

        // 连续 ''：等价单边界，preedit 保留双分隔符
        let r = e.convert("ni''hao", 20).unwrap();
        assert!(r.preedit_display.contains("''"), "连续分隔符应原样保留");
        assert!(r.candidates.iter().any(|c| c.text == "你"));

        // 纯 ' / 连续纯 '：无拼音可查，无候选、仅回显分隔符，不 panic
        for pure in ["'", "''", "'''"] {
            let r = e.convert(pure, 20).unwrap();
            assert!(r.candidates.is_empty(), "纯分隔符输入 {pure:?} 不应有候选");
            assert_eq!(r.preedit_display, pure, "纯分隔符应原样回显");
        }
    }

    fn tmp_store(name: &str) -> Arc<Store> {
        let p = std::env::temp_dir().join(format!("wind_pinyin_{name}.redb"));
        let _ = std::fs::remove_file(&p);
        Arc::new(Store::open(&p).unwrap())
    }

    /// L 造词显现：挂上用户/临时层后，拼音造的词应进入候选（即便主词典为空）。
    #[test]
    fn store_layer_words_appear_in_candidates() {
        let store = tmp_store("layer_show");
        store
            .add_user_word("pinyin", "nihao", "你好", 500, 0)
            .unwrap();
        store
            .learn_temp_word("pinyin", "lanshou", "蓝瘦", 800, 0)
            .unwrap();
        let dm = DictManager::new();
        dm.register_layer(Box::new(wind_dict::StoreUserLayer::new(
            store.clone(),
            "pinyin",
        )));
        dm.register_layer(Box::new(wind_dict::StoreTempLayer::new(
            store.clone(),
            "pinyin",
        )));
        let engine = empty_engine().with_store_layers(Arc::new(dm));

        // 整串精确命中用户词
        let r = engine.convert("nihao", 20).unwrap();
        assert!(
            r.candidates.iter().any(|c| c.text == "你好"),
            "用户造词「你好」应出现在拼音候选"
        );
        // 临时词同样显现
        let r2 = engine.convert("lanshou", 20).unwrap();
        let shou = r2.candidates.iter().find(|c| c.text == "蓝瘦");
        assert!(shou.is_some(), "临时造词「蓝瘦」应出现在拼音候选");
        assert_eq!(
            shou.unwrap().source,
            CandidateSource::Pinyin,
            "来源应标为拼音"
        );
    }

    /// 无 store 层时行为不变（不 panic、空词典无候选）。
    #[test]
    fn no_store_layer_is_inert() {
        let engine = empty_engine();
        let r = engine.convert("nihao", 20).unwrap();
        assert!(r.candidates.is_empty(), "空词典无 store 层应无候选");
    }

    /// 部分消费：用户词码是输入的前缀（nihao ⊂ nihaoma）→ consumed_length 标为前缀长度，
    /// 选中后保留剩余拼音续转（分段上屏）。
    #[test]
    fn store_word_prefix_marks_partial_consumption() {
        let store = tmp_store("layer_partial");
        store
            .add_user_word("pinyin", "nihao", "你好", 500, 0)
            .unwrap();
        let dm = DictManager::new();
        dm.register_layer(Box::new(wind_dict::StoreUserLayer::new(
            store.clone(),
            "pinyin",
        )));
        let engine = empty_engine().with_store_layers(Arc::new(dm));
        let r = engine.convert("nihaoma", 20).unwrap();
        let c = r.candidates.iter().find(|c| c.text == "你好");
        assert!(c.is_some(), "前缀用户词应作为分段候选出现");
        assert_eq!(
            c.unwrap().consumed_length,
            "nihao".len(),
            "应只消费前缀 nihao"
        );
    }

    /// 构造「带 qing 同音字洪泛的系统词典 + 用户长词」的引擎（复用于长词上浮系列测试）。
    fn engine_with_qing_flood_and_user_word(store_name: &str) -> PinyinEngine {
        let mut raw = CodetableDict::empty();
        for (i, ch) in ["清", "青", "情", "请", "轻", "晴", "倾", "氢", "卿", "顷"]
            .iter()
            .enumerate()
        {
            raw.merge_single(
                "qing".to_string(),
                ch.to_string(),
                1000 - i as i32,
                i as i32,
            );
        }
        raw.merge_single("feng".to_string(), "风".to_string(), 900, 0);
        raw.merge_single("qingfeng".to_string(), "清风".to_string(), 800, 0);

        let store = tmp_store(store_name);
        // boundary=0：模拟手输码用户词（无音节真值）→ 走 completed_syls>=3 兜底门槛。
        store
            .add_user_word("pinyin", "qingfengshurufa", "清风输入法", 5000, 0)
            .unwrap();
        let dm = DictManager::new();
        dm.register_layer(Box::new(wind_dict::StoreUserLayer::new(store, "pinyin")));
        PinyinEngine::new(Config::default(), CachedDict::Memory(raw))
            .with_store_layers(Arc::new(dm))
    }

    /// 【核心回归】用户长词「清风输入法」在打到第 3-4 音节时应上浮到同音子短语之上，
    /// 而非被压到候选最底（本次修复的用户反馈现场：打到完整全拼才出现）。
    #[test]
    fn user_long_word_surfaces_at_partial_pinyin() {
        let engine = engine_with_qing_flood_and_user_word("long_word_surface");

        for input in ["qingfengshu", "qingfengshuruf"] {
            let r = engine.convert(input, 300).unwrap();
            let pos_word = r
                .candidates
                .iter()
                .position(|c| c.text == "清风输入法")
                .unwrap_or_else(|| panic!("{input}: 清风输入法 应在候选中"));
            let pos_qing = r
                .candidates
                .iter()
                .position(|c| c.text == "清")
                .expect("清 子短语应存在");
            assert!(
                pos_word < pos_qing,
                "{input}: 用户长词应上浮到同音子短语「清」之上，实际 word@{pos_word} qing@{pos_qing}: {:?}",
                r.candidates
                    .iter()
                    .take(5)
                    .map(|c| &c.text)
                    .collect::<Vec<_>>()
            );
            assert!(
                r.candidates[pos_word].is_promoted_completion,
                "{input}: 上浮的用户长词应标 is_promoted_completion"
            );
            // is_prefix 结构真值保持不变（码确实更长）。
            assert!(
                r.candidates[pos_word].is_prefix,
                "{input}: is_prefix 结构事实（码更长）不应被抹掉"
            );
        }

        // 完整全拼：精确命中，本就在首位（is_prefix=false，非提升）。
        let r_full = engine.convert("qingfengshurufa", 300).unwrap();
        assert_eq!(
            r_full.candidates[0].text, "清风输入法",
            "完整全拼应精确命中首位"
        );
        assert!(
            !r_full.candidates[0].is_prefix,
            "完整全拼是精确匹配，非补全"
        );
        assert!(
            !r_full.candidates[0].is_promoted_completion,
            "精确命中不经上浮通道"
        );
    }

    /// 【边界守卫】音节太少时不上浮：`qing`(1 音节) / `qingfeng`(2 音节) 下用户长词
    /// 仍沉在补全层，且精确词「清风」在 qingfeng 下仍居首——不被用户长词越过。
    #[test]
    fn user_long_word_not_promoted_when_too_few_syllables() {
        let engine = engine_with_qing_flood_and_user_word("long_word_guard");

        // qing：1 音节，boundary=0 兜底门槛 completed_syls>=3 未达 → 不上浮。
        let r1 = engine.convert("qing", 300).unwrap();
        if let Some(p) = r1.candidates.iter().position(|c| c.text == "清风输入法") {
            assert!(
                !r1.candidates[p].is_promoted_completion,
                "qing(1 音节)不应上浮用户长词"
            );
        }

        // qingfeng：2 音节，未达门槛 → 不上浮；精确「清风」应排在用户长词之前。
        let r2 = engine.convert("qingfeng", 300).unwrap();
        let pos_qf = r2.candidates.iter().position(|c| c.text == "清风");
        let pos_word = r2.candidates.iter().position(|c| c.text == "清风输入法");
        if let Some(pw) = pos_word {
            assert!(
                !r2.candidates[pw].is_promoted_completion,
                "qingfeng(2 音节)不应上浮用户长词"
            );
            if let Some(pqf) = pos_qf {
                assert!(pqf < pw, "qingfeng 下精确「清风」应排在用户长词之前");
            }
        }
    }

    /// 上浮判据单测：距词尾 ≤2（有边界）/ 已打 ≥3 音节（无边界）才上浮。
    #[test]
    fn promote_user_completion_thresholds() {
        // 5 音节词（boundary 五个音节起始位；此处只关心 count_ones()=5）。
        let b5: u64 = 0b11111; // 5 个置位（count_ones=5，模拟 5 音节词）
        assert_eq!(b5.count_ones(), 5);
        // 无残码：completed_syls 即 started。
        assert!(
            !should_promote_user_completion(2, false, b5),
            "5 音节词打 2 音节剩 3 > 2，不上浮"
        );
        assert!(
            should_promote_user_completion(3, false, b5),
            "5 音节词打 3 音节剩 2 = 2，上浮"
        );
        assert!(
            should_promote_user_completion(4, false, b5),
            "5 音节词打 4 音节剩 1，上浮"
        );
        assert!(
            !should_promote_user_completion(1, false, b5),
            "1 音节 < 2，无条件不上浮"
        );
        // 尾部残码算作已起头的一个音节：qingfengs = 2 完整音节 + 残码 → started 3 → 上浮。
        assert!(
            should_promote_user_completion(2, true, b5),
            "2 完整音节 + 残码（started 3, 剩 2）应上浮"
        );
        assert!(
            !should_promote_user_completion(1, true, b5),
            "1 完整音节 + 残码（started 2, 剩 3 > 2）不上浮"
        );
        // 无边界兜底：started>=3。
        assert!(
            !should_promote_user_completion(2, false, 0),
            "无边界 2 音节不上浮"
        );
        assert!(
            should_promote_user_completion(3, false, 0),
            "无边界 3 音节上浮"
        );
        assert!(
            should_promote_user_completion(2, true, 0),
            "无边界 2 音节 + 残码（started 3）上浮"
        );
    }

    /// 用户造词的简拼应可命中（现算，非离线索引）：用户新增「菜鸟驿站」（全拼
    /// cainiaoyizhan），键入简拼 cnyz 应能查到；临时词同理。
    #[test]
    fn store_layer_words_match_abbreviation() {
        let store = tmp_store("layer_abbrev");
        store
            .add_user_word("pinyin", "cainiaoyizhan", "菜鸟驿站", 500, 0)
            .unwrap();
        store
            .learn_temp_word("pinyin", "lanshoubing", "蓝瘦蘑菇", 800, 0)
            .unwrap();
        let dm = DictManager::new();
        dm.register_layer(Box::new(wind_dict::StoreUserLayer::new(
            store.clone(),
            "pinyin",
        )));
        dm.register_layer(Box::new(wind_dict::StoreTempLayer::new(
            store.clone(),
            "pinyin",
        )));
        let engine = empty_engine().with_store_layers(Arc::new(dm));

        let r = engine.convert("cnyz", 20).unwrap();
        assert!(
            r.candidates.iter().any(|c| c.text == "菜鸟驿站"),
            "简拼 cnyz 应命中用户造词「菜鸟驿站」"
        );

        let r2 = engine.convert("lsb", 20).unwrap();
        assert!(
            r2.candidates.iter().any(|c| c.text == "蓝瘦蘑菇"),
            "简拼 lsb 应命中临时造词「蓝瘦蘑菇」"
        );

        // 全拼整串输入仍应正常命中（无回归）
        let r3 = engine.convert("cainiaoyizhan", 20).unwrap();
        assert!(r3.candidates.iter().any(|c| c.text == "菜鸟驿站"));
    }

    /// `enable_abbrev=false`（混输经 schema.mix.enable_pinyin_abbrev 注入）时不产简拼候选，
    /// 但全拼一切照旧。与上一个用例同构，只翻转开关——用于锁住「关掉的是简拼、不是拼音」。
    #[test]
    fn abbrev_disabled_suppresses_abbrev_candidates_only() {
        let store = tmp_store("layer_abbrev_off");
        store
            .add_user_word("pinyin", "cainiaoyizhan", "菜鸟驿站", 500, 0)
            .unwrap();
        let dm = DictManager::new();
        dm.register_layer(Box::new(wind_dict::StoreUserLayer::new(
            store.clone(),
            "pinyin",
        )));
        let engine = PinyinEngine::new(
            Config {
                enable_abbrev: false,
                ..Default::default()
            },
            CachedDict::Memory(CodetableDict::empty()),
        )
        .with_store_layers(Arc::new(dm));

        let r = engine.convert("cnyz", 20).unwrap();
        assert!(
            !r.candidates.iter().any(|c| c.text == "菜鸟驿站"),
            "关闭简拼后 cnyz 不应命中「菜鸟驿站」"
        );

        // 全拼不受影响——这一条是关键：开关关的是简拼，不是拼音本身。
        let r2 = engine.convert("cainiaoyizhan", 20).unwrap();
        assert!(
            r2.candidates.iter().any(|c| c.text == "菜鸟驿站"),
            "关闭简拼不得影响全拼命中"
        );
    }

    /// C1：query→原始输入空间的 consumed 回映射。无 `'` 恒等；边界紧跟 `'` 归入已消费侧；
    /// 连续 `''` 一并吸收；越过分隔符时正确计数；nih'ao 段内残码边界不 panic。
    /// Task 1.4 TDD：with_fuzzy builder 注入的配置应被引擎持有（探针验证）。
    #[test]
    fn engine_applies_fuzzy_config() {
        let dict = CachedDict::Memory(CodetableDict::empty());
        let mut fz = FuzzyConfig::default();
        fz.zh_z = true;
        let eng = PinyinEngine::new(Config::default(), dict).with_fuzzy(fz);
        assert!(eng.fuzzy_zh_z(), "with_fuzzy 注入的 zh_z=true 应被引擎持有");
    }

    /// Task 4.1 TDD Step 2：多音节双拼——consumed_length 必须回算为双拼键数，
    /// compute_composition 不能对已是全拼的串再做一次双拼转换。
    /// 输入小鹤双拼 "nihc"（ni+hc → 全拼 "nihao"），词典含「你好」。
    #[test]
    fn pinyin_engine_shuangpin_multisyllable_consumed_length() {
        // 构造含 "nihao"->"你好" 的最小词典
        let mut raw = CodetableDict::empty();
        raw.merge_single("nihao".to_string(), "你好".to_string(), 200, 0);
        raw.merge_single("ni".to_string(), "你".to_string(), 100, 1);
        let dict = CachedDict::Memory(raw);

        let schema_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../data/schemas/shuangpin");
        let layout = Layout::from_toml(&schema_dir.join("xiaohe.toml")).expect("加载小鹤布局失败");
        let conv = ShuangpinConverter::new(layout);

        let eng = PinyinEngine::new(Config::default(), dict).with_shuangpin(conv);
        // 小鹤双拼 "nihc" → 全拼 "nihao"
        let r = eng.convert("nihc", 10).unwrap();

        // 1. 候选含「你好」
        assert!(
            r.candidates.iter().any(|c| c.text == "你好"),
            "双拼输入 \"nihc\" 应包含候选「你好」，实际候选: {:?}",
            r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );

        // 2. 「你好」的 consumed_length 必须是双拼键数 4，而非全拼字节数 5
        let nihao = r.candidates.iter().find(|c| c.text == "你好").unwrap();
        assert_eq!(
            nihao.consumed_length, 4,
            "「你好」consumed_length 应为双拼键数 4（\"nihc\" 的长度），实际为 {}",
            nihao.consumed_length
        );
    }

    /// 边界相容判定：双拼定死音节边界，候选的词典边界须与之吻合。
    #[test]
    fn boundary_compatible_rules() {
        // 输入 nihao(5键) 双拼解释 ni|ha|o → 全拼 "nihao"，边界 {0,2,4}
        let sp = 0b10101u64;
        // 「你好」词典边界 ni|hao = {0,2}，与解释不符 → 拒绝（这正是 5 键出「你好」的病灶）
        assert!(
            !boundary_compatible(0b101, sp, 5, 5),
            "ni|hao 不该匹配 ni|ha|o"
        );
        // 「你」code=ni(2B) 边界 {0} → 只比前 2 字节窗口 → 相容
        assert!(boundary_compatible(0b1, sp, 2, 5));
        // 「你哈」code=niha(4B) 边界 {0,2} → 前 4 字节窗口相容
        assert!(boundary_compatible(0b101, sp, 4, 5));

        // 正确双拼 nihc(4键,小鹤) 解释 ni|hao → 全拼 "nihao"，边界 {0,2}
        let sp2 = 0b101u64;
        assert!(
            boundary_compatible(0b101, sp2, 5, 5),
            "ni|hao 应匹配 ni|hao"
        );

        // 前缀补全：输入 ni（全拼串仅 2B），候选「你好」code=nihao(5B) 边界 {0,2}
        // → 窗口取 min(5,2)=2，只比已输入部分 → 相容（补全部分尚未键入，无从校验）
        assert!(boundary_compatible(0b101, 0b1, 5, 2));

        // 任一侧无信息 → 放行（用户手输码/五笔/模糊变体/含回写段）
        assert!(boundary_compatible(0, sp, 5, 5));
        assert!(boundary_compatible(0b101, 0, 5, 5));
    }

    /// 双拼分段边界：音节、尾部 partial、**以及无匹配键对回写段**，各开一个段起点。
    #[test]
    fn sp_boundary_mask_rules() {
        use crate::pinyin::shuangpin::{SpConvertResult, SylSpan};
        let syl = |p: &str, fs, fe| SylSpan {
            pinyin: p.to_string(),
            raw_start: 0,
            raw_end: 0,
            fp_start: fs,
            fp_end: fe,
        };
        // ni|ha + partial o → full "nihao"，边界 {0,2,4}
        // 注：has_partial 与 partial_initial 须同设——真实 convert 二者恒同时写入，
        // 只设其一是不可能出现的状态（fixture 造假会测出假结论）。
        let sp = SpConvertResult {
            syllables: vec![syl("ni", 0, 2), syl("ha", 2, 4)],
            has_partial: true,
            partial_initial: Some("o".into()),
            full_pinyin: "nihao".into(),
            ..Default::default()
        };
        assert_eq!(sp_boundary_mask(&sp), 0b10101);
        // ni|hao 恰好覆盖，无尾部 → {0,2}
        let sp2 = SpConvertResult {
            syllables: vec![syl("ni", 0, 2), syl("hao", 2, 5)],
            has_partial: false,
            full_pinyin: "nihao".into(),
            ..Default::default()
        };
        assert_eq!(sp_boundary_mask(&sp2), 0b101);
        // 回写段夹在音节**中间**（omni 的 om 占 0..2）：其起点同样是段边界 → {0,2}
        let sp3 = SpConvertResult {
            syllables: vec![syl("ni", 2, 4)],
            full_pinyin: "omni".into(),
            ..Default::default()
        };
        assert_eq!(
            sp_boundary_mask(&sp3),
            0b101,
            "回写段在中间时其起点也是边界"
        );
        // 回写段在**尾部**（nihaoya 的 oy+a 占 4..7）→ {0,2,4}。
        // 位 4 的存在正是关键：它让词典的 ni|hao|ya({0,2,5}) 失配。曾在此弃用为 0
        // （以为回写段"无从表达"），校验被整个关掉，「你好呀」就从 step4 前缀补全漏网。
        let sp4 = SpConvertResult {
            syllables: vec![syl("ni", 0, 2), syl("ha", 2, 4)],
            has_partial: true,
            partial_initial: Some("a".into()),
            full_pinyin: "nihaoya".into(),
            ..Default::default()
        };
        assert_eq!(
            sp_boundary_mask(&sp4),
            0b10101,
            "尾部回写段须标起点，否则校验失效"
        );
        assert!(
            !boundary_compatible(0b100101, 0b10101, 7, 7),
            "词典 ni|hao|ya 应与双拼 ni|ha|oy… 失配"
        );
        // 全是回写段（如 oy）→ 仅首段起点 {0}
        let sp5 = SpConvertResult {
            full_pinyin: "oy".into(),
            ..Default::default()
        };
        assert_eq!(sp_boundary_mask(&sp5), 0b1);
        // 空输入 → 无信息
        assert_eq!(sp_boundary_mask(&SpConvertResult::default()), 0);
    }

    /// 回归：无匹配键对（convert 的「原样回写」分支）既不进 syllables 也不置 has_partial，
    /// build_raw_preedit 必须仍覆盖它们，否则编码被静默吞掉。
    /// 真机现象（首道双拼）：nihaom → 显示 niha（om 消失），再按 a → ni'ha'oma 又复现。
    #[test]
    fn build_raw_preedit_covers_unmatched_pairs() {
        use crate::pinyin::shuangpin::{SpConvertResult, SylSpan};
        let syl = |raw_start, raw_end| SylSpan {
            pinyin: String::new(), // build_raw_preedit 只用 sp 区间切原始串，不读 pinyin
            raw_start,
            raw_end,
            fp_start: 0,
            fp_end: 0,
        };

        // ① 尾部无匹配键对（om）：has_partial=false，早期实现漏掉尾巴 → "ni'ha"。
        let sp = SpConvertResult {
            syllables: vec![syl(0, 2), syl(2, 4)],
            has_partial: false,
            ..Default::default()
        };
        assert_eq!(
            build_raw_preedit("nihaom", &sp),
            "ni'ha'om",
            "尾部无匹配键对不得被吞"
        );

        // ② 尾部 partial 单键（o）：has_partial=true，行为与早期实现一致。
        let sp = SpConvertResult {
            syllables: vec![syl(0, 2), syl(2, 4)],
            has_partial: true,
            ..Default::default()
        };
        assert_eq!(build_raw_preedit("nihao", &sp), "ni'ha'o");

        // ③ 无匹配键对在中间（om 在前）：音节前的空隙也须原样保留。
        let sp = SpConvertResult {
            syllables: vec![syl(2, 4), syl(4, 6)],
            has_partial: true,
            ..Default::default()
        };
        assert_eq!(
            build_raw_preedit("omnihao", &sp),
            "om'ni'ha'o",
            "音节之间的无匹配段不得被吞"
        );

        // ④ 全无音节：原样返回。
        let sp = SpConvertResult::default();
        assert_eq!(build_raw_preedit("xq", &sp), "xq");
        assert_eq!(build_raw_preedit("", &sp), "");
    }

    /// **双拼真值边界校验（本功能的验收点）**：双拼把音节边界定死了，候选的词典边界必须吻合。
    ///
    /// 真机现象：双拼下打 5 键 `nihao` 出「你好」。那是巧合——双拼解释为 `ni|ha|o`，拼成
    /// `full_pinyin="nihao"` 恰好撞上全拼的 nihao，DAG 再把它重切成 `ni|hao` 查到「你好」。
    /// 而「你好」的正确双拼是 4 键（`nihc`）。
    ///
    /// 注意必须用 rime 源构造词典：`merge_single` 造的条目 boundary 恒 0（无信息→放行），
    /// 用它根本测不出校验。
    #[test]
    fn shuangpin_rejects_mismatched_syllable_split() {
        use crate::pinyin::shuangpin::{Layout, ShuangpinConverter};
        use std::io::Write;
        let path = std::env::temp_dir().join("wind_sp_boundary_check.dict.yaml");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "---\nname: py\n...").unwrap();
            writeln!(f, "你好\tni hao\t2000").unwrap(); // 边界 ni|hao = {0,2}
            writeln!(f, "你\tni\t900").unwrap();
            writeln!(f, "哈\tha\t500").unwrap();
            writeln!(f, "哦\to\t300").unwrap();
        }
        let dict = CachedDict::Memory(CodetableDict::load(&path).unwrap());
        let _ = std::fs::remove_file(&path);

        let schema_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../data/schemas/shuangpin");
        let layout = Layout::from_toml(&schema_dir.join("xiaohe.toml")).expect("加载小鹤布局失败");
        let eng = PinyinEngine::new(Config::default(), dict)
            .with_shuangpin(ShuangpinConverter::new(layout));

        // 5 键 nihao → 双拼 ni|ha|o（o 为 partial），与「你好」的 ni|hao 不符 → 拒绝。
        let r = eng.convert("nihao", 20).unwrap();
        let texts: Vec<&String> = r.candidates.iter().map(|c| &c.text).collect();
        assert!(
            !texts.contains(&&"你好".to_string()),
            "5 键 nihao 解释为 ni|ha|o，不该出「你好」（其双拼是 4 键 nihc），实际: {texts:?}"
        );
        // 与解释相容的候选仍在：ni → 「你」
        assert!(
            texts.contains(&&"你".to_string()),
            "「你」(ni) 与 ni|ha|o 的首音节相容，应保留，实际: {texts:?}"
        );

        // 4 键 nihc → 双拼 ni|hao，与「你好」的词典边界一致 → 正常出。
        let r2 = eng.convert("nihc", 20).unwrap();
        assert!(
            r2.candidates.iter().any(|c| c.text == "你好"),
            "4 键 nihc 解释为 ni|hao，应出「你好」，实际: {:?}",
            r2.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    /// 回归（真机报告）：双拼下 `nihao` 选「你」后，剩余 `hao`(3键) 变成空候选。
    ///
    /// 病灶不在校验本身，而在「查询仍按 DAG 的猜测、校验却按双拼的真值」——两套切分打架：
    /// 双拼解释 `hao` = `ha`+`o`(partial)，而 DAG 把 `full="hao"` 重切成 `[hao]` 只查了
    /// 「好」，随后被真值 {0,2} 拒掉；而双拼真正该查的 `ha`（→「哈」）压根没被查。
    /// 于是 DAG 查来的被拒、双拼该查的没查 → 空。
    #[test]
    fn shuangpin_uses_own_split_for_lookup_not_dag() {
        use crate::pinyin::shuangpin::{Layout, ShuangpinConverter};
        use std::io::Write;
        let path = std::env::temp_dir().join("wind_sp_lookup_split.dict.yaml");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "---\nname: py\n...").unwrap();
            writeln!(f, "好\thao\t2000").unwrap(); // 单音节，边界 {0}
            writeln!(f, "哈\tha\t900").unwrap();
            writeln!(f, "哦\to\t300").unwrap();
        }
        let dict = CachedDict::Memory(CodetableDict::load(&path).unwrap());
        let _ = std::fs::remove_file(&path);

        let schema_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../data/schemas/shuangpin");
        let layout = Layout::from_toml(&schema_dir.join("xiaohe.toml")).expect("加载小鹤布局失败");
        let eng = PinyinEngine::new(Config::default(), dict)
            .with_shuangpin(ShuangpinConverter::new(layout));

        // 3 键 hao → 双拼 ha|o：应按**双拼自己的切分**去查，出「哈」(ha)。
        let r = eng.convert("hao", 20).unwrap();
        let texts: Vec<&String> = r.candidates.iter().map(|c| &c.text).collect();
        assert!(
            texts.contains(&&"哈".to_string()),
            "ha 是双拼解释出的完成音节，应查到「哈」，实际: {texts:?}（空候选=查询仍按 DAG 猜）"
        );
        // 「好」的双拼是 2 键 hc，3 键 h/a/o 不该出它。
        assert!(
            !texts.contains(&&"好".to_string()),
            "3 键 hao 解释为 ha|o，不该出「好」（其双拼是 hc），实际: {texts:?}"
        );

        // 2 键 hc → 双拼 hao 单音节 → 正常出「好」。
        let r2 = eng.convert("hc", 20).unwrap();
        assert!(
            r2.candidates.iter().any(|c| c.text == "好"),
            "2 键 hc 解释为 hao，应出「好」，实际: {:?}",
            r2.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    /// 含「无匹配键对」时**仍按双拼语义**：取从 0 起连续的音节前缀，断裂处之后不解释。
    ///
    /// `oy`（o 非声母，拼不出音节）属 convert 注释里的「无效键对」——用户打错了，
    /// 它及其后的内容不该产生候选。曾误把这里当成「整串降级回全拼 DAG」，于是 `nihaoya`
    /// 出了「你好呀」——那与 `nihao`(5键) 不出「你好」自相矛盾：同是双拼下打全拼串，
    /// 一个拒一个收，反倒是**打错一个键对就解锁了全拼**。
    ///
    /// 注释里的「简拼」指的是另一半：`nh` 这种 per-串简拼由 AbbrevMatcher 兜底（走 query，
    /// 不依赖 syllables），无需退回 DAG 也照常工作——见 shuangpin_abbrev_still_works。
    #[test]
    fn shuangpin_keeps_own_semantics_with_unmatched_pair() {
        use crate::pinyin::shuangpin::{Layout, ShuangpinConverter};
        use std::io::Write;
        let path = std::env::temp_dir().join("wind_sp_writeback_strict.dict.yaml");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "---\nname: py\n...").unwrap();
            writeln!(f, "你好呀\tni hao ya\t2000").unwrap();
            writeln!(f, "你好\tni hao\t1500").unwrap();
            writeln!(f, "你\tni\t900").unwrap();
            writeln!(f, "哈\tha\t500").unwrap();
        }
        let dict = CachedDict::Memory(CodetableDict::load(&path).unwrap());
        let _ = std::fs::remove_file(&path);

        let schema_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../data/schemas/shuangpin");
        let layout = Layout::from_toml(&schema_dir.join("xiaohe.toml")).expect("加载小鹤布局失败");
        let conv = ShuangpinConverter::new(layout);

        // 前提：nihaoya 在小鹤下确有「无匹配键对」（oy）——即音节未覆盖到 full 末尾、
        // 且缺口大于 partial 声母。否则本测试没意义。
        let sp = conv.convert("nihaoya");
        let covered: usize = sp.syllables.last().map_or(0, |s| s.fp_end);
        let partial_len = sp.partial_initial.as_ref().map_or(0, |s| s.len());
        assert!(
            sp.full_pinyin.len() - covered > partial_len,
            "前提失效：nihaoya 应含无匹配回写段，实际 syllables={:?} full={:?}",
            sp.syllables.iter().map(|s| &s.pinyin).collect::<Vec<_>>(),
            sp.full_pinyin
        );

        let eng = PinyinEngine::new(Config::default(), dict).with_shuangpin(conv);
        let r = eng.convert("nihaoya", 20).unwrap();
        let texts: Vec<&String> = r.candidates.iter().map(|c| &c.text).collect();
        // 断裂前的 ni|ha 照常出候选。
        assert!(
            texts.contains(&&"你".to_string()),
            "连续前缀 ni 应出「你」，实际: {texts:?}"
        );
        // 不得把整串当全拼——那会与「nihao 不出你好」自相矛盾。
        assert!(
            !texts.contains(&&"你好呀".to_string()) && !texts.contains(&&"你好".to_string()),
            "oy 是无效键对，不该整串降级成全拼解释，实际: {texts:?}"
        );
    }

    /// 无匹配键对**原样回写**进 full_pinyin（不产 SylSpan），输入不被吞——
    /// 这是简拼兜底的前提：AbbrevMatcher 走 `query`（即 full_pinyin），**不看音节切分**，
    /// 故双拼真值切分不影响它。
    ///
    /// 这也是「含回写段须退回 DAG」的反证：保住简拼根本不需要退回 DAG。
    /// （简拼表只存在于 wdat AbbrevSection，端到端查询由 wind-dict 侧覆盖。）
    #[test]
    fn shuangpin_writeback_keeps_input_intact() {
        use crate::pinyin::shuangpin::{Layout, ShuangpinConverter};
        let schema_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../data/schemas/shuangpin");
        let layout = Layout::from_toml(&schema_dir.join("xiaohe.toml")).expect("加载小鹤布局失败");
        let conv = ShuangpinConverter::new(layout);
        // oy：o 非声母，拼不出合法音节 → 整对原样回写。
        let sp = conv.convert("oy");
        assert!(
            sp.syllables.is_empty(),
            "oy 拼不出音节，不该产出 SylSpan，实际 {:?}",
            sp.syllables.iter().map(|s| &s.pinyin).collect::<Vec<_>>()
        );
        assert_eq!(sp.full_pinyin, "oy", "无匹配键对须原样回写，输入不得被吞");
    }

    /// Fix A TDD：双拼 preedit 应显示用户实际输入的原始按键（按音节边界以 `'` 分隔，
    /// 与全拼自动分词一致），而非转换后的全拼。输入小鹤 "nihc"（→全拼 nihao）应显示
    /// "ni'hc"，候选仍含「你好」。
    #[test]
    fn shuangpin_preedit_shows_raw_keys() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("nihao".to_string(), "你好".to_string(), 200, 0);
        raw.merge_single("ni".to_string(), "你".to_string(), 100, 1);
        let dict = CachedDict::Memory(raw);

        let schema_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../data/schemas/shuangpin");
        let layout = Layout::from_toml(&schema_dir.join("xiaohe.toml")).expect("加载小鹤布局失败");
        let conv = ShuangpinConverter::new(layout);
        let eng = PinyinEngine::new(Config::default(), dict).with_shuangpin(conv);

        // 完整双音节：preedit 为原始键按音节 ' 分隔 "ni'hc"（而非全拼 "ni'hao"）
        let r = eng.convert("nihc", 10).unwrap();
        assert_eq!(
            r.preedit_display, "ni'hc",
            "双拼 preedit 应显示原始按键并按音节 ' 分隔，实际: {:?}",
            r.preedit_display
        );
        // 候选仍走全拼语义，含「你好」
        assert!(
            r.candidates.iter().any(|c| c.text == "你好"),
            "候选仍应含「你好」，实际: {:?}",
            r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );

        // 单音节：ni → "ni"
        let r2 = eng.convert("ni", 10).unwrap();
        assert_eq!(r2.preedit_display, "ni", "单音节 preedit 应为 \"ni\"");

        // 含 partial：nih（ni 完成 + h 未配对）→ "ni'h"
        let r3 = eng.convert("nih", 10).unwrap();
        assert_eq!(
            r3.preedit_display, "ni'h",
            "含 partial 的双拼 preedit 应为 \"ni'h\"，实际: {:?}",
            r3.preedit_display
        );
    }

    /// Fix B TDD：fuzzy 应接入精确/单音节查询。词典含 "shi"→"是"，
    /// fuzzy sh_s=true 时，输入单音节全拼 "si" 应能命中「是」（sh↔s 模糊）。
    #[test]
    fn fuzzy_lookup_single_syllable() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("shi".to_string(), "是".to_string(), 100, 0);
        let dict = CachedDict::Memory(raw);
        let mut fz = FuzzyConfig::default();
        fz.sh_s = true;
        let eng = PinyinEngine::new(Config::default(), dict).with_fuzzy(fz);

        let r = eng.convert("si", 10).unwrap();
        assert!(
            r.candidates.iter().any(|c| c.text == "是"),
            "fuzzy sh_s 开启时，单音节 \"si\" 应命中「是」，实际: {:?}",
            r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );

        // 反向：词典 "si"→"四"，输入 "shi" 应命中「四」
        let mut raw2 = CodetableDict::empty();
        raw2.merge_single("si".to_string(), "四".to_string(), 100, 0);
        let mut fz2 = FuzzyConfig::default();
        fz2.sh_s = true;
        let eng2 = PinyinEngine::new(Config::default(), CachedDict::Memory(raw2)).with_fuzzy(fz2);
        let r2 = eng2.convert("shi", 10).unwrap();
        assert!(
            r2.candidates.iter().any(|c| c.text == "四"),
            "fuzzy sh_s 开启时，\"shi\" 应命中「四」，实际: {:?}",
            r2.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    /// 优先级 TDD：原对应拼音（精确匹配）应优先于模糊命中——即便模糊词词频更高。
    /// 词典 "si"→"四"(weight 100) 与 "shi"→"是"(weight 9000，更高频)；fuzzy sh_s=true。
    /// 输入 "si"：「四」是精确命中、「是」是模糊命中，「四」必须排在「是」之前。
    #[test]
    fn fuzzy_exact_ranks_above_fuzzy() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("si".to_string(), "四".to_string(), 100, 0);
        raw.merge_single("shi".to_string(), "是".to_string(), 9000, 0);
        let dict = CachedDict::Memory(raw);
        let mut fz = FuzzyConfig::default();
        fz.sh_s = true;
        let eng = PinyinEngine::new(Config::default(), dict).with_fuzzy(fz);

        let r = eng.convert("si", 10).unwrap();
        let texts: Vec<&String> = r.candidates.iter().map(|c| &c.text).collect();
        let pos_si = texts.iter().position(|t| *t == "四");
        let pos_shi = texts.iter().position(|t| *t == "是");
        assert!(pos_si.is_some(), "精确候选「四」应存在，实际: {texts:?}");
        assert!(pos_shi.is_some(), "模糊候选「是」应存在，实际: {texts:?}");
        assert!(
            pos_si < pos_shi,
            "精确命中「四」应排在模糊命中「是」之前（即便「是」词频更高），实际: {texts:?}"
        );
        // 「四」是精确（非模糊），「是」是模糊命中
        assert!(!r.candidates[pos_si.unwrap()].is_fuzzy, "「四」应为非模糊");
        assert!(
            r.candidates[pos_shi.unwrap()].is_fuzzy,
            "「是」应为模糊命中"
        );
    }

    /// 层级 TDD：精确单字应优先于高频前缀补全词。词典 "si"→"四"(weight 100,单字精确) 与
    /// "sikao"→"思考"(weight 9000,补全词，code 比输入长)。输入 "si" 时「四」(精确,code==输入)
    /// 必须排在「思考」(前缀补全)之前——即便「思考」词频高得多。对齐 Go Exact>>Partial。
    #[test]
    fn exact_ranks_above_prefix_completion() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("si".to_string(), "四".to_string(), 100, 0);
        raw.merge_single("sikao".to_string(), "思考".to_string(), 9000, 0);
        let dict = CachedDict::Memory(raw);
        let eng = PinyinEngine::new(Config::default(), dict);

        let r = eng.convert("si", 10).unwrap();
        let texts: Vec<&String> = r.candidates.iter().map(|c| &c.text).collect();
        let pos_si = texts.iter().position(|t| *t == "四");
        let pos_kao = texts.iter().position(|t| *t == "思考");
        assert!(pos_si.is_some(), "精确「四」应存在，实际: {texts:?}");
        assert!(pos_kao.is_some(), "补全「思考」应存在，实际: {texts:?}");
        assert!(
            pos_si < pos_kao,
            "精确单字「四」应优先于高频前缀补全「思考」，实际: {texts:?}"
        );
        assert!(
            !r.candidates[pos_si.unwrap()].is_prefix,
            "「四」应为精确(非前缀)"
        );
        assert!(
            r.candidates[pos_kao.unwrap()].is_prefix,
            "「思考」应为前缀补全"
        );
    }

    /// 完整匹配优先于子短语（对齐 Go coverage 分层）：输入完整音节 "nihao" 时，
    /// 全长精确词「拟好」(code==nihao) 即便权重远低于子短语「你」(code=ni)，也应排在「你」之前。
    /// 「你」只覆盖部分输入(ni)，是分段上屏候选(is_partial)，整体降到完整词之后。
    ///
    /// 注：此前 `subphrase_not_demoted_below_rare_exact` 断言相反（子词组不降权 → 你 > 拟好），
    /// 那是刻意偏离 Go 的旧设计，正是 baoan→报案 被高频单字埋没的根因。现改为对齐 Go：
    /// `score = exp(词频) + initialQuality + coverage`，完整词(cov=1,iq=4) 恒先于子短语单字(cov=.5,iq=2.5)。
    #[test]
    fn full_word_ranks_above_subphrase_singlechar() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("nihao".to_string(), "你好".to_string(), 200, 0);
        raw.merge_single("nihao".to_string(), "拟好".to_string(), 10, 1); // 罕见全长精确词
        raw.merge_single("ni".to_string(), "你".to_string(), 5000, 0); // 常用子短语
        let dict = CachedDict::Memory(raw);
        let eng = PinyinEngine::new(Config::default(), dict);

        let r = eng.convert("nihao", 10).unwrap();
        let texts: Vec<&String> = r.candidates.iter().map(|c| &c.text).collect();
        let pos_ni = texts.iter().position(|t| *t == "你");
        let pos_nihao_rare = texts.iter().position(|t| *t == "拟好");
        assert!(
            pos_ni.is_some() && pos_nihao_rare.is_some(),
            "候选缺失（子短语「你」仍应存在，供分段上屏）: {texts:?}"
        );
        assert!(
            pos_nihao_rare < pos_ni,
            "完整词「拟好」应优先于子短语单字「你」(对齐 Go coverage 分层)，实际: {texts:?}"
        );
        // 「你」是子短语(is_partial)，不是前缀补全(is_prefix)——分段上屏机制不受影响
        let ni = &r.candidates[pos_ni.unwrap()];
        assert!(!ni.is_prefix, "子短语「你」不应是前缀补全");
        assert!(ni.is_partial, "子短语「你」应标记 is_partial");
    }

    /// baoan 回归（用户报告场景）：输入 "baoan" 时，完整 bao'an 词「保安」「报案」必须
    /// 聚集在前，不被高频子短语单字「报」(bao) 插开。修复前「报」(高权重) 会塞进
    /// 「保安」「报案」之间，把「报案」挤到后面几页。
    #[test]
    fn baoan_full_words_group_above_subphrase() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("baoan".to_string(), "保安".to_string(), 3513, 0);
        raw.merge_single("baoan".to_string(), "报案".to_string(), 1374, 1);
        raw.merge_single("bao".to_string(), "报".to_string(), 9000, 0); // 高频单字，权重高于「报案」
        raw.merge_single("an".to_string(), "安".to_string(), 5000, 0);
        let dict = CachedDict::Memory(raw);
        let eng = PinyinEngine::new(Config::default(), dict);

        let r = eng.convert("baoan", 20).unwrap();
        let texts: Vec<&String> = r.candidates.iter().map(|c| &c.text).collect();
        let pos = |t: &str| texts.iter().position(|x| *x == t);
        let p_baoan = pos("保安").expect("「保安」应存在");
        let p_baoan2 = pos("报案").expect("「报案」应存在");
        let p_bao = pos("报").expect("子短语「报」应存在（供分段上屏）");
        assert!(
            p_baoan < p_bao && p_baoan2 < p_bao,
            "完整词「保安」({p_baoan})「报案」({p_baoan2}) 都应排在高频子短语单字「报」({p_bao}) 之前，实际: {texts:?}"
        );
    }

    /// Fix B TDD：fuzzy 应接入多音节整串查询（expand_code 笛卡尔积）。
    /// 词典只存 eng 形式 "shengri"→"生日"，用户输入 en 形式 "shenri"（DAG 切分 shen+ri），
    /// fuzzy en_eng=true 时应通过 expand_code 生成 "shengri" 反查命中「生日」。
    #[test]
    fn fuzzy_lookup_multi_syllable() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("shengri".to_string(), "生日".to_string(), 100, 0);
        let dict = CachedDict::Memory(raw);
        let mut fz = FuzzyConfig::default();
        fz.en_eng = true;
        let eng = PinyinEngine::new(Config::default(), dict).with_fuzzy(fz);

        let r = eng.convert("shenri", 10).unwrap();
        assert!(
            r.candidates.iter().any(|c| c.text == "生日"),
            "fuzzy en_eng 开启时，\"shenri\" 应模糊命中「生日」(shengri)，实际: {:?}",
            r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    /// Fix B TDD：fuzzy 全 false 时 lookup_with_fuzzy 退化为纯精确查找（不引入多余候选）。
    #[test]
    fn fuzzy_disabled_no_extra_candidates() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("shi".to_string(), "是".to_string(), 100, 0);
        let dict = CachedDict::Memory(raw);
        // 无 with_fuzzy → FuzzyConfig::default() 全 false
        let eng = PinyinEngine::new(Config::default(), dict);
        let r = eng.convert("si", 10).unwrap();
        assert!(
            !r.candidates.iter().any(|c| c.text == "是"),
            "fuzzy 关闭时 \"si\" 不应命中「是」，实际: {:?}",
            r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    /// 模糊命中的权重折扣：同一输入下，精确命中恒优先于**同词频**的模糊命中。
    #[test]
    fn fuzzy_penalty_keeps_exact_ahead_at_equal_weight() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("si".to_string(), "四".to_string(), 1000, 0);
        raw.merge_single("shi".to_string(), "是".to_string(), 1000, 1);
        let mut fz = FuzzyConfig::default();
        fz.sh_s = true;
        let eng = PinyinEngine::new(Config::default(), CachedDict::Memory(raw)).with_fuzzy(fz);

        let r = eng.convert("si", 10).unwrap();
        let texts: Vec<&String> = r.candidates.iter().map(|c| &c.text).collect();
        let pos_exact = texts.iter().position(|t| *t == "四").expect("「四」应存在");
        let pos_fuzzy = texts.iter().position(|t| *t == "是").expect("「是」应存在");
        assert!(
            pos_exact < pos_fuzzy,
            "同词频时精确命中须在模糊命中之前（折扣生效），实际: {texts:?}"
        );
    }

    /// **本次修复的回归守卫（原 bug 的直接复现）**：模糊命中不得被大量**前缀补全**挤出候选。
    ///
    /// 原实现把 `is_fuzzy` 当 `cmp_match_layers` 的首要键，所有非模糊候选（含码更长的前缀
    /// 补全）无条件排在模糊命中之前。真实词库下 `si` 的前缀补全有 230 条，把「是」顶到第
    /// 231 位，而生产候选上限仅 50~300 —— 模糊音整体失效。
    ///
    /// 此处用 40 条 `si*` 前缀补全模拟那堵墙：**上限取 20**（小于补全总数），若模糊命中仍
    /// 被整层压在补全之后，它必然落在截断线外。这正是「迷你词典单测全绿、真机全废」的
    /// 那道缺口——测试数据的**规模**本身就是被测条件的一部分。
    #[test]
    fn fuzzy_hit_survives_a_wall_of_prefix_completions() {
        let mut raw = CodetableDict::empty();
        // 一堵前缀补全的墙：码比输入长（is_prefix=true），非模糊，权重普通。
        for i in 0..40 {
            raw.merge_single(format!("si{i:02}"), format!("思{i:02}"), 500, i);
        }
        // 模糊命中：码 shi，经 s↔sh 由输入 si 召回；词频显著高于补全（真实词库中
        // 「是」正是高频字），折扣后仍应有竞争力。
        raw.merge_single("shi".to_string(), "是".to_string(), 900_000, 99);
        let mut fz = FuzzyConfig::default();
        fz.sh_s = true;
        let eng = PinyinEngine::new(Config::default(), CachedDict::Memory(raw)).with_fuzzy(fz);

        const LIMIT: usize = 20;
        let r = eng.convert("si", LIMIT).unwrap();
        let texts: Vec<&String> = r.candidates.iter().map(|c| &c.text).collect();
        assert!(
            texts.iter().any(|t| *t == "是"),
            "模糊命中须能挤进前 {LIMIT} 条，不得被 40 条前缀补全整层压到截断线外，实际: {texts:?}"
        );
    }

    /// 多音节整词的模糊命中（**step1 `lookup_with_fuzzy` 路径**，一直走逐音节 `expand_code`）：
    /// `beijinsi` → 「北京市」(beijingshi) 需要第 2 音节 in→ing **且** 第 3 音节 s→sh。
    ///
    /// 注意本例**测不到 lattice 路径**：词典存有覆盖整串的词条，step1 直接命中。
    /// lattice 的逐音节展开由 `fuzzy_non_initial_initial_via_lattice_sentence` 覆盖。
    #[test]
    fn fuzzy_hits_non_initial_syllables_via_lookup() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("beijingshi".to_string(), "北京市".to_string(), 5000, 0);
        let mut fz = FuzzyConfig::default();
        fz.in_ing = true;
        fz.sh_s = true;
        let eng = PinyinEngine::new(Config::default(), CachedDict::Memory(raw)).with_fuzzy(fz);

        let r = eng.convert("beijinsi", 20).unwrap();
        assert!(
            r.candidates.iter().any(|c| c.text == "北京市"),
            "第 2、3 音节同时模糊时应命中「北京市」，实际: {:?}",
            r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    /// **lattice 逐音节展开的专项回归守卫**（本次修复的核心路径）。
    ///
    /// 设计要点，缺一条就测不到真东西：
    /// - 用**非首音节的声母**变体（`zou`→`zhou`，第 2 音节）。声母规则是 `starts_with`，
    ///   整串调用只能改首音节；而韵母规则是 `find`，第一处匹配常恰好落在非首音节上，
    ///   用韵母做判据会让整串调用也「碰巧」通过（`beijin`→`beijing` 正是如此）。
    /// - 词典**不含**覆盖整串的词条，迫使候选只能由 Viterbi 多节点拼接产生，
    ///   从而必经 lattice；否则 step1 的 `lookup_with_fuzzy` 会先命中，测不到 lattice。
    ///
    /// 把 lattice 改回对整串 `code` 求变体，本测试即挂。
    #[test]
    fn fuzzy_non_initial_initial_via_lattice_sentence() {
        let mut raw = CodetableDict::empty();
        // 覆盖前两音节的词（其码 zhongzhou 需由 zhong|zou 经第 2 音节 z→zh 得到）
        raw.merge_single("zhongzhou".to_string(), "中州".to_string(), 5000, 0);
        // 覆盖末音节的字，供 Viterbi 拼出整句
        raw.merge_single("ming".to_string(), "明".to_string(), 5000, 1);
        let mut fz = FuzzyConfig::default();
        fz.zh_z = true;
        let eng = PinyinEngine::new(Config::default(), CachedDict::Memory(raw)).with_fuzzy(fz);

        // zhong|zou|ming：词典无覆盖整串的词条 → 只能靠词图拼接
        let r = eng.convert("zhongzouming", 20).unwrap();
        // **必须断言整句**（`is_sentence`），不能只断言「中州」出现：后者由 step3 的子短语
        // 查询命中（`partial=true, code=zhongzou`，同样走逐音节 `lookup_with_fuzzy`），
        // 在旧实现下**照样存在**——拿它做判据测不到 lattice，是一条会永远通过的假测试。
        // 只有整句「中州明」需要「中州」先作为**词图节点**存在，才必经 lattice 的模糊展开。
        let dump: Vec<String> = r
            .candidates
            .iter()
            .map(|c| format!("{}(sent={})", c.text, c.is_sentence))
            .collect();
        assert!(
            r.candidates
                .iter()
                .any(|c| c.is_sentence && c.text.contains("中州")),
            "第 2 音节 zou→zhou 须能进入词图，使 Viterbi 拼出整句「中州明」，实际: {dump:?}"
        );
    }

    /// 模糊命中的**整句**让位于精确整词：整句带 3e7 基准分，比例折扣压不下来，
    /// 故走 `is_sentence_demoted` 降级。
    #[test]
    fn fuzzy_sentence_yields_to_exact_word() {
        let mut raw = CodetableDict::empty();
        // 精确整词（码 == 输入）
        raw.merge_single("sixiang".to_string(), "思想".to_string(), 26_000, 0);
        // 模糊命中的整词（码 shixiang，经 s↔sh 由 sixiang 召回）
        raw.merge_single("shixiang".to_string(), "是想".to_string(), 30_000, 1);
        let mut fz = FuzzyConfig::default();
        fz.sh_s = true;
        let eng = PinyinEngine::new(Config::default(), CachedDict::Memory(raw)).with_fuzzy(fz);

        let r = eng.convert("sixiang", 10).unwrap();
        let texts: Vec<&String> = r.candidates.iter().map(|c| &c.text).collect();
        let pos_exact = texts.iter().position(|t| *t == "思想");
        assert_eq!(
            pos_exact,
            Some(0),
            "存在精确整词时它必须居首，模糊整句让位，实际: {texts:?}"
        );
    }

    /// 反面：**没有**精确整词竞争时，模糊命中的整句照常居首——这正是模糊音要的效果
    /// （`zongguo` → 「中国」）。与上一条共用同一判据，二者必须同时成立。
    #[test]
    fn fuzzy_sentence_leads_when_no_exact_word() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("zhongguo".to_string(), "中国".to_string(), 30_000, 0);
        // zongguo 下只有子短语单字，没有码 == zongguo 的精确整词
        raw.merge_single("zong".to_string(), "总".to_string(), 20_000, 1);
        let mut fz = FuzzyConfig::default();
        fz.zh_z = true;
        let eng = PinyinEngine::new(Config::default(), CachedDict::Memory(raw)).with_fuzzy(fz);

        let r = eng.convert("zongguo", 10).unwrap();
        let texts: Vec<&String> = r.candidates.iter().map(|c| &c.text).collect();
        assert_eq!(
            texts.first().map(|s| s.as_str()),
            Some("中国"),
            "无精确整词竞争时模糊整句应居首，实际: {texts:?}"
        );
    }

    /// Bug 复现：双拼下用户词（存储在 "pinyin" 共享 schema）应出现在候选中。
    /// 小鹤双拼 "dabologe" → 全拼 "daboluoge"；store 中有该用户词时应能命中。
    #[test]
    fn shuangpin_store_user_word_appears_in_candidates() {
        let store = tmp_store("sp_userdict");
        store
            .add_user_word("pinyin", "daboluoge", "大菠萝哥", 0, 0)
            .unwrap();

        let dm = DictManager::new();
        dm.register_layer(Box::new(wind_dict::StoreUserLayer::new(
            store.clone(),
            "pinyin",
        )));

        let schema_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../data/schemas/shuangpin");
        let layout = Layout::from_toml(&schema_dir.join("xiaohe.toml")).expect("加载小鹤布局失败");
        let conv = ShuangpinConverter::new(layout);

        // 先确认转换正确：dabologe → daboluoge
        let sp_result = conv.convert("dabologe");
        assert_eq!(
            sp_result.full_pinyin(),
            "daboluoge",
            "小鹤双拼 dabologe 应转换为全拼 daboluoge，实际: {:?}",
            sp_result.full_pinyin()
        );

        let eng = empty_engine()
            .with_shuangpin(conv)
            .with_store_layers(Arc::new(dm));

        let r = eng.convert("dabologe", 20).unwrap();
        assert!(
            r.candidates.iter().any(|c| c.text == "大菠萝哥"),
            "双拼输入 \"dabologe\" 应命中用户词「大菠萝哥」，实际候选: {:?}",
            r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    // ── use_smart_compose 开关 ──────────────────────────────────────────────────

    /// 构造带单字词典的拼音引擎（供整句/Viterbi 相关测试使用）：
    /// 词典含 "ni"→"你"、"hao"→"好"，但无 "nihao"→"你好" 整词。
    /// 任何 "你好" 候选只能来自 Viterbi 整句路径。
    fn engine_for_sentence_tests(config: Config) -> PinyinEngine {
        let mut raw = CodetableDict::empty();
        raw.merge_single("ni".to_string(), "你".to_string(), 100, 0);
        raw.merge_single("hao".to_string(), "好".to_string(), 100, 1);
        PinyinEngine::new(config, CachedDict::Memory(raw))
    }

    /// 判断候选是否为 Viterbi 合成整句（按来源标记，不看权重数值）。
    fn is_viterbi_sentence(c: &Candidate) -> bool {
        c.is_sentence
    }

    // ── 整句有同码竞争者（step 6.6 摘词频锚定）──────────────────────────────────

    /// 整句解**自己就是**词典精确整词、且同码还有别的精确整词时，须标 `is_sentence_contested`。
    ///
    /// 现场 `siyuan`：「寺院」经 step 2 同文合并继承整句身份 → `freq_rerank` 顶部锚定
    /// （硬闸门）→ 同码「思源」灌到 count=5000 都翻不动。此处用 `nihao` 你好/拟好复现
    /// 同一结构：**给整词高权重、单字低权重**，迫使 Viterbi 选「你好」这个单节点，
    /// 从而走同文合并分支（与 `demoted_sentence_falls_below_all_max_weight_exact_words`
    /// 的构造正好相反 —— 那里要的是合成整句）。
    #[test]
    fn dict_word_sentence_with_same_code_peer_is_contested() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("ni".to_string(), "你".to_string(), 100, 0);
        raw.merge_single("hao".to_string(), "好".to_string(), 100, 1);
        raw.merge_single("nihao".to_string(), "你好".to_string(), 50_000, 2);
        raw.merge_single("nihao".to_string(), "拟好".to_string(), 200, 3);
        let e = PinyinEngine::new(Config::default(), CachedDict::Memory(raw));
        let r = e.convert("nihao", 50).unwrap();

        let c = r
            .candidates
            .iter()
            .find(|c| c.text == "你好")
            .expect("候选中应有「你好」");
        assert!(c.is_sentence, "词典整词被 Viterbi 选中 → 继承整句身份");
        assert!(
            !c.is_sentence_demoted,
            "它本身即精确整词，无处可让，不该走 6.5 降级"
        );
        assert!(
            c.is_sentence_contested,
            "同码存在「拟好」→ 须标 contested（否则词频对 nihao 整体失效）"
        );
        assert!(
            c.weight >= SENTENCE_WEIGHT_BASE - 1_000_000,
            "本标记只摘锚定、**不动 weight**，整句仍须保有 SENTENCE_WEIGHT_BASE 量纲，实际 {}",
            c.weight
        );
        assert_eq!(
            r.candidates[0].text, "你好",
            "无词频记录时整句仍居首（引擎侧顺序不因本标记改变）"
        );
    }

    /// 对照组：同码**没有**别的精确整词 → 不标 contested，锚定保留。
    ///
    /// 与上一条合看才证明判据真在看「有无竞争者」，而非恒置位。少了这条，把 6.6 写成
    /// 无条件置位也能让上一条通过 —— 那会连 `woshizhongguoren` 这类无竞争者的整句
    /// 一起摘掉锚定。
    #[test]
    fn dict_word_sentence_without_peer_is_not_contested() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("ni".to_string(), "你".to_string(), 100, 0);
        raw.merge_single("hao".to_string(), "好".to_string(), 100, 1);
        raw.merge_single("nihao".to_string(), "你好".to_string(), 50_000, 2);
        let e = PinyinEngine::new(Config::default(), CachedDict::Memory(raw));
        let r = e.convert("nihao", 50).unwrap();

        let c = r
            .candidates
            .iter()
            .find(|c| c.text == "你好")
            .expect("候选中应有「你好」");
        assert!(c.is_sentence, "仍是整句");
        assert!(
            !c.is_sentence_contested,
            "同码无竞争者 → 不得标 contested，锚定须保留"
        );
    }

    // ── 整句让位于精确整词（step 6.5 降级）─────────────────────────────────────

    /// 用户要求的那条保证：**多个精确整词并列于最大权重时，整句排在它们全部之后**。
    ///
    /// `max - 1` 在算术上蕴含它（并列者皆为 `max`），但这是要靠实测确认的那一类断言，
    /// 不是靠推理就算数的 —— 并列走的是 `better`/`candidate_display_order` 的后续键
    /// （base_order / natural_order），只有真跑一遍才知道整句没混进并列组里。
    #[test]
    fn demoted_sentence_falls_below_all_max_weight_exact_words() {
        let mut raw = CodetableDict::empty();
        // 单字给高权重，确保 Viterbi 选「你+好」而非把某个 nihao 词条当单节点整句
        // （那样会走同文合并分支，压根不触发降级，测试就测空了）。
        raw.merge_single("ni".to_string(), "你".to_string(), 100_000, 0);
        raw.merge_single("hao".to_string(), "好".to_string(), 100_000, 1);
        // 三个精确整词，权重并列且同为最大
        raw.merge_single("nihao".to_string(), "拟好".to_string(), 5000, 2);
        raw.merge_single("nihao".to_string(), "泥好".to_string(), 5000, 3);
        raw.merge_single("nihao".to_string(), "尼好".to_string(), 5000, 4);
        let e = PinyinEngine::new(Config::default(), CachedDict::Memory(raw));
        let r = e.convert("nihao", 50).unwrap();

        let pos = |t: &str| {
            r.candidates
                .iter()
                .position(|c| c.text == t)
                .unwrap_or_else(|| {
                    panic!(
                        "候选中找不到 {t}，实际: {:?}",
                        r.candidates
                            .iter()
                            .map(|c| (&c.text, c.weight))
                            .collect::<Vec<_>>()
                    )
                })
        };
        let sent = pos("你好");
        let sc = &r.candidates[sent];
        assert!(sc.is_sentence, "「你好」应是合成整句");
        assert!(sc.is_sentence_demoted, "存在精确整词时整句须降级");
        assert_eq!(sc.weight, 4999, "权重须为 max(5000) - 1");
        for w in ["拟好", "泥好", "尼好"] {
            assert!(
                pos(w) < sent,
                "并列于 max 的精确整词「{w}」(rank {}) 须排在整句(rank {sent})之前，实际: {:?}",
                pos(w),
                r.candidates
                    .iter()
                    .map(|c| (&c.text, c.weight))
                    .collect::<Vec<_>>()
            );
        }
    }

    /// 不变量：降级整句仍须在**普通候选**之前，无论后者权重多高。
    ///
    /// 守的是 `max - 1` 的权重并列风险 —— 位置靠 `cmp_match_layers` 的层级键保证，
    /// 而非靠权重数值，故权重再离谱也不该翻转。
    #[test]
    fn demoted_sentence_still_precedes_ordinary_candidates() {
        let mut raw = CodetableDict::empty();
        raw.merge_single("ni".to_string(), "你".to_string(), 100_000, 0);
        raw.merge_single("hao".to_string(), "好".to_string(), 100_000, 1);
        raw.merge_single("nihao".to_string(), "拟好".to_string(), 5000, 2);
        // 前缀补全（码比输入长）：权重顶到 2e9，仍应留在整句之后
        raw.merge_single(
            "nihaoma".to_string(),
            "你好吗".to_string(),
            2_000_000_000,
            3,
        );
        let e = PinyinEngine::new(Config::default(), CachedDict::Memory(raw));
        let r = e.convert("nihao", 50).unwrap();

        let sent = r.candidates.iter().position(|c| c.text == "你好").unwrap();
        assert!(r.candidates[sent].is_sentence_demoted, "整句须已降级");
        // 整句之前只允许出现精确整词（码 == 输入且不在下层）
        for (i, c) in r.candidates.iter().enumerate().take(sent) {
            assert!(
                c.code == "nihao" && !c.is_fuzzy && !c.is_prefix && !c.is_partial,
                "整句(rank {sent})之前只应有精确整词，却出现 {}(rank {i}, w={}, code={})",
                c.text,
                c.weight,
                c.code
            );
        }
    }

    /// TDD：use_smart_compose=false 时多音节输入不产生 Viterbi 合成整句候选。
    #[test]
    fn smart_compose_off_skips_viterbi_sentence() {
        let e = engine_for_sentence_tests(Config {
            use_smart_compose: false,
            ..Config::default()
        });
        let r = e.convert("nihao", 50).unwrap();
        assert!(
            !r.candidates.iter().any(|c| {
                c.text.chars().count() >= 2
                    && c.source == CandidateSource::Pinyin
                    && is_viterbi_sentence(c)
            }),
            "关闭智能组句后不应有 Viterbi 合成整句，实际候选: {:?}",
            r.candidates
                .iter()
                .map(|c| (&c.text, c.weight))
                .collect::<Vec<_>>()
        );
    }

    /// 回归：use_smart_compose=true（默认）时整句候选仍产生。
    #[test]
    fn smart_compose_on_produces_viterbi_sentence() {
        let e = engine_for_sentence_tests(Config::default()); // use_smart_compose 默认 true
        let r = e.convert("nihao", 50).unwrap();
        assert!(
            r.candidates.iter().any(|c| {
                c.text.chars().count() >= 2
                    && c.source == CandidateSource::Pinyin
                    && is_viterbi_sentence(c)
            }),
            "启用智能组句时应有 Viterbi 合成整句，实际候选: {:?}",
            r.candidates
                .iter()
                .map(|c| (&c.text, c.weight))
                .collect::<Vec<_>>()
        );
    }

    /// Task 4.1 TDD Step 1：双拼端到端——装配小鹤双拼 converter 后，
    /// 输入双拼键 "ni" 应返回含「你」的候选。
    #[test]
    fn pinyin_engine_shuangpin_input() {
        // 构造含 "ni"->"你" 的最小词典
        let mut raw = CodetableDict::empty();
        raw.merge_single("ni".to_string(), "你".to_string(), 100, 0);
        let dict = CachedDict::Memory(raw);

        // 小鹤双拼：ni → ni（声母 n + 韵母 i=i，即全拼 "ni"，保持不变）
        let schema_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../data/schemas/shuangpin");
        let layout = Layout::from_toml(&schema_dir.join("xiaohe.toml")).expect("加载小鹤布局失败");
        let conv = ShuangpinConverter::new(layout);

        let eng = PinyinEngine::new(Config::default(), dict).with_shuangpin(conv);
        let r = eng.convert("ni", 10).unwrap();
        assert!(
            r.candidates.iter().any(|c| c.text == "你"),
            "双拼输入 \"ni\" 经转换后应返回含「你」的候选，实际候选: {:?}",
            r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }
}
