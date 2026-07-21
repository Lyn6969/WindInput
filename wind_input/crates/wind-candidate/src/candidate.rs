//! 候选词数据类型
//!
//! 与 Go 版本 `wind_input/internal/candidate/candidate.go` 对齐。

use serde::{Deserialize, Serialize};

/// 候选词来源
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CandidateSource {
    #[serde(rename = "")]
    None,
    #[serde(rename = "codetable")]
    CodeTable,
    #[serde(rename = "pinyin")]
    Pinyin,
    #[serde(rename = "english")]
    English,
    #[serde(rename = "phrase")]
    Phrase,
}

impl Default for CandidateSource {
    fn default() -> Self {
        Self::None
    }
}

/// 候选词元数据
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CandidateMeta {
    pub lexicon_name: String,
    pub is_user_dict: bool,
    pub is_temp_dict: bool,
    pub raw_weight: i32,
    pub freq_boost: i32,
    /// 短语来源归属：`is_phrase` 候选时有意义，true=系统短语，false=用户短语。
    /// 仅供悬停调试提示区分来源（`wind_phrase::PhraseHit::is_system` 透传而来）。
    #[serde(default)]
    pub is_system_phrase: bool,
}

/// 命令栏动作
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub kind: ActionKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionKind {
    /// 文本插入
    Text,
    /// 副作用（不插入文本）
    Effect,
}

/// 候选词
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub text: String,
    pub pinyin: String,
    pub code: String,
    pub weight: i32,
    /// 词库**层级基序档位**（`[[dictionaries]].base_order`，默认 0）。排序时作为**独立层级**：
    /// weight 之后、natural_order 之前（见 `better`/`by_natural`）。小整数即可把整库排到另一库
    /// 前/后，与 natural_order 大小无关（不同于把偏移加进 natural_order 的旧做法）。
    pub base_order: i32,
    pub natural_order: i32,
    pub comment: String,
    pub is_common: bool,
    pub is_phrase: bool,
    pub is_command: bool,
    /// 是否来自模糊音变体命中（非原拼音精确匹配）。排序时模糊候选整体降到非模糊之后，
    /// 使"原对应拼音"优先（如输入 si 时「四」优先于模糊命中的「是」）。
    pub is_fuzzy: bool,
    /// 是否为前缀补全候选（候选编码比输入更长，如输入 si 补全出「思考」(sikao)）。
    /// 排序时前缀补全整体降到精确匹配（code==输入）之后，使等长精确候选优先
    /// （如输入 si 时单字「四」优先于补全词「思考」），对齐 Go 的 Exact>>Partial 层级。
    pub is_prefix: bool,
    /// 是否为子短语候选（候选编码是输入的真前缀、比输入短，如输入 baoan 时「报」(bao)）。
    /// 供分段上屏（你好→你）使用，但排序时整体降到完整匹配之后，避免高频单字插进
    /// 完整词之间（如 baoan 时「报/宝」塞在「保安」「报案」之间）。对齐 Go 的 coverage
    /// 分层：完整覆盖输入的词恒先于只覆盖部分输入的子短语单字。
    pub is_partial: bool,
    /// 是否属于**精确匹配档**：候选 `code` 与本次输入完全相等（如五笔简码 `usr`→「新」），
    /// 区别于编码更长的前缀补全（`usrq`→「新的」）。排序时精确档整体先于前缀补全。
    ///
    /// **谁置位**（新增候选来源时须对照，漏标 = 被压到精确候选之下，且编译器抓不到）：
    /// - 码表引擎 `CodeTableEngine::convert`：按 `code == input` 置位，覆盖文件词库/用户词/临时词
    ///   （它们都经 `dm.search`/`dm.search_prefix` 返回）及薄封装的英文引擎；
    /// - 混输 overflow 分支：以**完整输入**重新归一（其码表半边是按前 N 码查的）；
    /// - 协调器精确码短语（`phrases.lookup`）：按定义即精确匹配；
    /// - 协调器引导键导航候选（`$CC`/`$SS`/`$AA` 前缀命中）：**按既有设计恒置顶**而非因编码相等，
    ///   用户正是按引导键为了看到它们。这是本字段唯一的「非 code==input」成员，故字段语义取
    ///   「精确**档**」而非字面的「编码相等」。
    /// - 拼音引擎不置位：混输下码表精确恒先于拼音（与 `freq_rerank::freq_tier` 的档位设计一致）；
    ///   纯拼音模式全体为 `false`，本键退化为无操作。
    ///
    /// **为何不复用 `is_prefix`**：该字段已被自定义短语借作「非精确层」标记（协调器
    /// `build_candidates` 中短语恒 `is_prefix=true` 且带 `PHRASE_WEIGHT_BASE`=40M）。若给码表
    /// 前缀候选也标 `is_prefix`，短语会与码表词组落进同层，靠 40M 权重整体浮到词组之上——
    /// 一个字段承担两种含义，复用即耦合两件无关的事。
    ///
    /// **为何需要独立层级而非靠权重**：词组权重来自词频、单字权重来自字频，两套量纲不可比。
    /// 「新的」(usrq, 47487) 纯按权重会压过简码「新」(usr, 11777)，把简码字挤到第三位——
    /// 跨类别比 weight，比的其实是类别。
    #[serde(default)]
    pub is_exact_code: bool,
    /// 是否为**引擎合成的整句解**（Viterbi 多词拼接，或超长词典整词的等价整句分）。
    ///
    /// 语义 = "这是引擎对整串输入的最优解读"，词频重排（`freq_rerank`）据此把它连同
    /// `is_phrase` 一起锚定在顶部，不因用户词频而下沉。
    ///
    /// 此前该判定靠 `weight >= 20_000_000` 的数值阈值实现，把"来源语义"编码进了权重数值，
    /// 导致两类问题：① 任何因别的原因被提权到 20M 以上的候选都会被误锚定，永久失去词频
    /// 学习能力；② 不相关的提权功能（如 `BARE_INITIAL_SINGLE_CHAR_BOOST`）必须小心避让
    /// 这条阈值线。改用显式标记后，权重只表达"多重要"，来源语义由本字段表达。
    #[serde(default)]
    pub is_sentence: bool,
    pub consumed_length: usize,
    /// 该候选 `code` 的**音节边界**（各音节起始字节位 bitmask），见
    /// `wind_dict::binformat::DictEntry::boundary`。`0` = 无边界信息 → 消费方降级回 DAG 猜切分。
    ///
    /// 来自词典真值（rime 源数据 `ni hao` 的空格），供双拼按真实边界校验候选：
    /// 输入 nihao(5键) 双拼解释为 ni|ha|o，而「你好」的 boundary 是 ni|hao，二者不符即拒绝。
    ///
    /// **与 `code` 同进同出**：`composite::merge_search` 同 text 去重时 code 取高优先层、
    /// 换最短码时也换 code，boundary 必须跟着一起换，否则会配出「A 层的 code + B 层的 boundary」。
    ///
    /// 引擎内部用，不推送 UI（`serde(skip)`，省 IPC 带宽）。
    #[serde(skip)]
    pub boundary: u64,
    pub source: CandidateSource,
    pub phrase_template: String,
    pub is_group: bool,
    pub is_group_member: bool,
    pub group_code: String,
    pub group_name: String,
    pub group_template: String,
    pub index: usize,
    pub has_shadow: bool,
    pub index_label: String,
    pub meta: CandidateMeta,
    pub id: String,
    pub display_text: String,
    pub actions: Vec<Action>,
}

impl Default for Candidate {
    fn default() -> Self {
        Self {
            text: String::new(),
            pinyin: String::new(),
            code: String::new(),
            weight: 0,
            base_order: 0,
            natural_order: 0,
            comment: String::new(),
            is_common: false,
            is_phrase: false,
            is_command: false,
            is_fuzzy: false,
            is_prefix: false,
            is_partial: false,
            is_exact_code: false,
            is_sentence: false,
            consumed_length: 0,
            boundary: 0,
            source: CandidateSource::None,
            phrase_template: String::new(),
            is_group: false,
            is_group_member: false,
            group_code: String::new(),
            group_name: String::new(),
            group_template: String::new(),
            index: 0,
            has_shadow: false,
            index_label: String::new(),
            meta: CandidateMeta::default(),
            id: String::new(),
            display_text: String::new(),
            actions: Vec::new(),
        }
    }
}

/// 候选「匹配层级」比较——`Exact >> 子短语 >> 前缀补全 >> 模糊` 的**唯一真相**。
///
/// ① 非模糊优先于模糊（输入 si 时精确「四」先于模糊命中「是」）；
/// ② 精确/子短语（`is_prefix=false`）优先于前缀补全（输入 si 时「四」先于补全「思考」）；
/// ③ 完整匹配优先于子短语（输入 baoan 时「保安」「报案」先于单字「报」「宝」）。
///
/// 该层级此前在三处各写了一遍——引擎内部排序、协调器 `candidate_display_order`、
/// 词频重排 `rerank_pinyin_decay`——三份必须手工保持同步，漏改任何一处都不会编译报错，
/// 只会让候选顺序在某条路径上静默发散。三处现统一调用本函数，各自的额外维度
/// （权重/`base_order`/衰减分/整句锚定）在其前后自行追加。
pub fn cmp_match_layers(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    a.is_fuzzy
        .cmp(&b.is_fuzzy)
        .then(a.is_prefix.cmp(&b.is_prefix))
        .then(a.is_partial.cmp(&b.is_partial))
}

/// 候选「精确匹配档优先」比较（`is_exact_code` 降序）。
///
/// 与 `cmp_match_layers` 分设：后者表达「匹配质量层级」（模糊/前缀/子短语），本函数表达
/// 「是否属于精确档」，两者正交。
///
/// **必须两处共用**：码表引擎排完序后，协调器合并短语时还会用 `candidate_display_order`
/// 无条件重排全部候选。若只在引擎内排好而不落到 `Candidate::is_exact_code` 字段上，下游重排
/// 无从得知谁是精确匹配，只能按纯权重重来，引擎的结果被静默推翻——本函数即为修此断层而抽出。
///
/// **两处调用位置不同，是有意为之**：
/// - 协调器 `candidate_display_order`：置于 `cmp_match_layers` **之后**、权重之前——精确优先
///   只在同一匹配层内生效，不跨层提拔（`is_prefix=true` 的静态短语前缀枚举仍留在下层）。
/// - 码表引擎 `CodeTableEngine::convert`：作**顶层首要键**，不叠 `cmp_match_layers`。因其基础
///   排序器 `better`/`by_natural` 本就不含匹配层级，贸然引入会改变用户词（`store_layer` 会设
///   `is_prefix`）与文件词库候选的既有相对序，超出本键的职责范围。
///
/// **另有一份同概念判据**：`wind_engine::freq_rerank::freq_tier` 的 `code == input`（码表档位）。
/// 二者在纯码表路径结论一致；未合并是因为 `freq_tier` 只在开启自动调频时参与，且其档位划分
/// 还承载词频语义。改动任一处时须同步核对另一处。
pub fn cmp_exact_first(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    b.is_exact_code.cmp(&a.is_exact_code)
}

/// 比较两个候选词的排序优先级（权重降序）
///
/// 与 Go 版本 `candidate.Better` 对齐。
pub fn better(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    // 层级：weight 降 → base_order 升（词库档位）→ natural_order 升（出现序）→ code → text。
    // base_order 默认 0 时该级为空操作，故不设 base_order 的路径（拼音/混输等）行为不变。
    a.weight
        .cmp(&b.weight)
        .reverse()
        .then(a.base_order.cmp(&b.base_order))
        .then(a.natural_order.cmp(&b.natural_order))
        .then(a.code.cmp(&b.code))
        .then(a.consumed_length.cmp(&b.consumed_length).reverse())
        .then(a.text.cmp(&b.text))
}

/// 比较两个候选词的自然排序优先级（精确匹配优先）
///
/// 与 Go 版本 `candidate.BetterNatural` 对齐。
pub fn better_natural(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    let a_exact = a.weight >= 0;
    let b_exact = b.weight >= 0;
    match (a_exact, b_exact) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a
            .natural_order
            .cmp(&b.natural_order)
            .then_with(|| better(a, b)),
    }
}

/// 比较两个候选词的**纯自然序**优先级（`base_sort = "natural"` 用）：**完全忽略权重**，
/// 只按 `natural_order`（词库出现序，含 base_order 层偏移）升序，再以 code/text 作稳定兜底。
///
/// 与 `better_natural` 的区别：后者精确匹配优先且以 `better`（权重）兜底；本函数不看权重，
/// 纯按设计者在词库文件里的排列顺序呈现（对齐用户"只按设计顺序"的诉求）。
pub fn by_natural(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    // 忽略权重：base_order 升（词库档位）→ natural_order 升（出现序）→ code → text。
    a.base_order
        .cmp(&b.base_order)
        .then(a.natural_order.cmp(&b.natural_order))
        .then(a.code.cmp(&b.code))
        .then(a.consumed_length.cmp(&b.consumed_length).reverse())
        .then(a.text.cmp(&b.text))
}

/// 排序候选词列表（权重降序）
pub fn sort_candidates(candidates: &mut [Candidate]) {
    candidates.sort_by(better);
}

/// 排序候选词列表（自然顺序，精确匹配优先）
pub fn sort_candidates_natural(candidates: &mut [Candidate]) {
    candidates.sort_by(better_natural);
}
