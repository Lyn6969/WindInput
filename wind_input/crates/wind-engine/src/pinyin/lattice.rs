//! 格子构建（Lattice）+ 多切分评分
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/lattice.go` 对齐。
//! 构建词图并支持多路径评分，用于 Viterbi 解码。

use crate::pinyin::dag::{MaskCheck, SegGraph};
use crate::pinyin::fuzzy::{FuzzyConfig, FuzzyMatcher};
use crate::pinyin::lm::UnigramLookup;
use wind_dict::cached::CachedDict;

/// 虚词集合（单字时轻微惩罚，对齐 Go functionWords）
fn is_function_word(w: &str) -> bool {
    matches!(
        w,
        "了" | "的"
            | "地"
            | "得"
            | "着"
            | "过"
            | "我"
            | "你"
            | "他"
            | "她"
            | "它"
            | "们"
            | "这"
            | "那"
            | "和"
            | "与"
            | "在"
            | "把"
            | "被"
            | "让"
            | "从"
            | "到"
            | "对"
            | "向"
            | "跟"
            | "不"
            | "没"
            | "也"
            | "都"
            | "就"
            | "才"
            | "还"
            | "又"
            | "再"
            | "很"
            | "太"
            | "最"
            | "是"
            | "有"
            | "会"
            | "能"
            | "要"
            | "可"
            | "去"
            | "来"
            | "做"
            | "说"
            | "看"
            | "想"
    )
}

/// V+助词尾字（多字词以此结尾时降权，对齐 Go particleSuffixes）
fn is_particle_suffix(c: char) -> bool {
    matches!(c, '了' | '的' | '着' | '过' | '得' | '地')
}

/// 每个词的固定惩罚，对应 librime `Grammar::Evaluate` 的 `kPenalty`
/// （`ref/weasel/librime/src/rime/gear/grammar.h:18-26`）。
///
/// 取值远小于 librime 的 18.42：两边**同为自然对数概率、量纲一致**（已核对），
/// 差异来自机制而非单位——librime 的 `kPenalty` 是**无语言模型时的兜底值**，有
/// grammar 时整个被替换掉；我们始终有 unigram，且 `score_node` 另有单字罚 −3.0、
/// 实词加成等项同时作用，等效的「多一个词」代价不止这一处。
pub(crate) const WORD_PENALTY: f64 = 3.0;

/// 歧义接缝上每个音节的惩罚，对应 librime `kPenaltyForAmbiguousSyllable`
/// （`ref/weasel/librime/src/rime/algo/syllabifier.cc:243-245`）。
///
/// librime 用 −23.03（1e-10）近乎硬禁，因其以隔音符号为消歧出口；我们不引入那套
/// 产品语义（§4.1 实测：Rime 中缩合音词在候选层仍是第 4/5 位），故取「恰好压过
/// 歧义拆分的收益」的量级即可。
///
/// **0.35 是一个刀刃值，改动前务必重跑 `pinyin_eval` 的定点**：≤0.30 时
/// `lianzhengtixing` 退回「李安整体性」（本次改造的原始缺陷）；≥0.5 时
/// `liandaoyan` 劣化为「连导演」。**聚合指标在 0.30~0.35 之间完全不变**，
/// 这两个定点是仅有的差异——因为它们是同一个词、同一个 `li|an` 拆分、同一个
/// 歧义接缝，切分层没有可区分二者的信息。真正的区分需要 bigram 上下文
/// （`lm.rs:327-337` 已实现插值，缺磁盘语料）。
pub(crate) const AMBIGUOUS_PENALTY: f64 = 0.35;

/// 简拼节点每个音节的惩罚（混合整句解码用，见 [`LatticeBuilder::add_abbrev_nodes`]）。
///
/// **按音节数计而非固定值**：简拼段越长，「每个字母只给了一个声母」积累的不确定性越大
/// ——`bzd` 要在 12 个同简拼词里选，`bzdh` 的候选面更宽。固定罚会让长简拼段不合理地便宜。
///
/// 量纲参照同文件的 `WORD_PENALTY`(3.0) 与模糊命中的 0.5：简拼的不确定性远大于模糊音
/// （一个声母对应几十个音节 vs z↔zh 两个变体），但又不能大到让混合整句根本出不来。
/// **本值由 `pinyin_eval` 的 D 类对账定出，改动前必须重跑**（见 `pinyin-mixed-abbrev.md` §4.8）。
pub(crate) const ABBREV_NODE_PENALTY: f64 = 1.2;

/// 单个简拼跨度最多取几个词进图。
///
/// 简拼召回面宽（`bzd` 真实词库下 12 个词），全塞进去会让节点数与 Viterbi 的边数一起膨胀，
/// 而排在后面的低频词几乎不可能赢下整句路径。按权重取前 N 即可。
const ABBREV_NODE_LIMIT: usize = 8;

/// 简拼跨度的最大字母数（= 最大音节数）。与 `AbbrevMatcher::find_candidates` 的上限一致。
const MAX_ABBREV_SPAN: usize = 6;

/// 节点对数概率打分（对齐 Go lattice calcLogProb + 惩罚/加成）。
/// 无 unigram 时回退到归一化词典权重。
///
/// 对 crate 内可见：`PinyinEngine::convert` 用它给「覆盖全部输入的词典精确整词」
/// 算单节点等价分，使其与 Viterbi 整句在同一量纲比较（见 mod.rs step 1.5）。
pub(crate) fn score_node(word: &str, weight: i32, unigram: Option<&dyn UnigramLookup>) -> f64 {
    const SINGLE_CHAR_PENALTY: f64 = -3.0;
    const FUNCTION_WORD_BONUS: f64 = 2.0; // 虚词加成（Go 原名 functionWordPenalty，值为正）
    const VERB_PARTICLE_PENALTY: f64 = -1.0;
    const BASE_CONTENT_WORD_BONUS: f64 = 3.0;
    const CHAR_BASED_PENALTY: f64 = -2.0; // 多字 OOV 用字符平均估算时的惩罚（对齐 Go）
    const LOG_PROB_MIN: f64 = -15.0;
    const LOG_PROB_RANGE: f64 = 12.0;

    let chars: Vec<char> = word.chars().collect();
    let char_count = chars.len();

    let Some(ug) = unigram else {
        // 无 unigram：用词典权重归一化（与 Go calcLogProb 的 nil 分支一致）
        return weight as f64 / 100_000.0;
    };

    // 基础 logProb：单字或在 unigram 中的词直接取；多字 OOV 用字符平均 + 惩罚，
    // 避免高频字组合（如"接了"）虚高碾压有真实词频的词（如"和解"）。
    let mut log_prob = if char_count <= 1 || ug.contains(word) {
        ug.log_prob(word)
    } else {
        ug.char_based_score(word) + CHAR_BASED_PENALTY
    };

    if char_count == 1 {
        if is_function_word(word) {
            log_prob += FUNCTION_WORD_BONUS;
        } else {
            log_prob += SINGLE_CHAR_PENALTY;
        }
    } else if char_count > 1 {
        if chars
            .last()
            .map(|c| is_particle_suffix(*c))
            .unwrap_or(false)
        {
            log_prob += VERB_PARTICLE_PENALTY;
        } else if ug.contains(word) {
            let freq_factor = ((log_prob - LOG_PROB_MIN) / LOG_PROB_RANGE).clamp(0.0, 1.0);
            log_prob += BASE_CONTENT_WORD_BONUS * (char_count as f64).sqrt() * freq_factor;
        }
    }
    // Weight ≤ 0 = 字典标记的非标准读音映射（如 那→ne 方言读法 w=0）。其 unigram
    // 高频不应凌驾字典的显式判断——否则 Viterbi 会在 ne 音节选 那 而非 呢(w=262461)。
    // -10.0 足够压过典型虚词-实词间的 unigram 差距（~2-8），又留足余量使正确
    // 但低频的单字（w>0 正常条目）不被误伤。
    if weight <= 0 {
        log_prob -= 10.0;
    }
    // Phase 4：每词固定罚。Viterbi 的路径分是各节点 log_prob 之和，故「每节点减 W」
    // 等价于「按路径词数罚 k·W」——把低频词打碎成两个高频片段不再免费。
    // 也施加于 mod.rs step 1.5 的「单节点等价整句分」（那是一句一词，罚一次，量纲一致）。
    //
    // **虚词（是/的/了…）豁免每词罚**：WORD_PENALTY 意在阻止「把低频词打碎成高频
    // 片段」的投机拆分，而单字虚词随内容词出现是语法黏着、不是碎片。unigram 的独立性
    // 假设对 P(内容词)·P(虚词) 双重扣了 ln(total)（每词一份），一个低频 3 字整词
    // （填鸭式 w=152）便能压过「天涯+是」这种 2 词正解——这正是 bigram P(是|天涯)
    // 该解决而 unigram 解决不了的（lm.rs SimpleBigramModel 已备、缺磁盘语料）。豁免虚词
    // 的每词罚是对该缺陷的近似补偿：不让「虚词自成一词」这件语法必然的事付投机拆分的代价。
    if !(char_count == 1 && is_function_word(word)) {
        log_prob -= WORD_PENALTY;
    }
    log_prob
}

/// 格子节点
#[derive(Debug, Clone)]
pub struct LatticeNode {
    pub start: usize,
    pub end: usize,
    pub word: String,
    pub syllables: Vec<String>,
    /// 本节点所采用切分的音节起始位 bitmask，**相对节点自身的 code 起点**
    /// （与词典 `DictEntry::boundary` 同域）。多路径下同一跨度可有多种切法，
    /// 故必须逐节点记录：Viterbi 选中哪条节点，整句的真实边界就是哪条。
    pub syl_mask: u64,
    pub log_prob: f64,
}

/// 格子构建器
pub struct LatticeBuilder {
    /// 最大词长（音节数）
    max_word_len: usize,
}

impl LatticeBuilder {
    pub fn new() -> Self {
        // 10 而非 6：6 会把「中华人民共和国」(7 音节) 挡在词图外，却放行它的语义碎片
        // 「中华人民共和」(freq=2，法律条文名切出来的残片)，于是 Viterbi 只能在
        // 「中华人民共和」+「过」之类的错误切分里挑最优。上限须覆盖常见长专名。
        Self { max_word_len: 10 }
    }

    /// 词图能容纳的最长词（音节数）。超过它的词典整词进不了 Viterbi，
    /// 需由 `PinyinEngine::convert` 的 step 1.5 单独兜底。
    pub fn max_word_len(&self) -> usize {
        self.max_word_len
    }

    /// 构建格子（**多路径切分**）
    ///
    /// 枚举的是**字节跨度** `(p, q)` 而非音节路径。这是本次改造的核心决策：
    ///
    /// 1. 音节恒为输入的连续子串，故查询码只由跨度决定 —— `input[p..q]`。
    ///    跨度对至多 O(n²)，而完整切分路径条数可指数增长（实测见
    ///    `tests/pinyin_path_scale.rs`）。
    /// 2. 「这个词是不是按某条合法路径敲出来的」不靠枚举路径回答，而是把词条自带的
    ///    `boundary` **当作一条待验证的路径**逐段查图（`SegGraph::mask_path`），
    ///    代价 O(音节数)。**路径爆炸因此在结构上不可能发生，无需剪枝。**
    ///
    /// 于是「西安交通大学」以真值 `xi|an|jiao|tong|da|xue` 合法入图，
    /// 而「李安」（真值 `li|an`）仍进不了单音节边 `lian` —— 前者是 Phase 1 里
    /// 被边界校验误杀的 4362 个词，后者是原始缺陷。两者第一次可以同时成立。
    ///
    /// `graph` 的形状决定切分来源：全拼用 `SegGraph::from_dag`（多路径），
    /// 双拼/手动分隔符用 `SegGraph::from_syllables`（真值链，行为与改造前完全一致）。
    /// `require_reachable`：是否只在「音节图上从 0 可达」的位置建节点。
    ///
    /// 常规路径传 `true`——不可达的位置上建节点纯属浪费，Viterbi 的 dp 永远到不了那里。
    /// **混合整句路径必须传 `false`**：简拼段会打断音节图的可达性（`bzdhaobuhao` 里
    /// b/z/d 都不成音节，位置 3 从 0 不可达），而 `[3,6) hao`、`[6,8) bu` 这些边其实
    /// 都在图里、只是被这道守卫挡住了；补上连接的是随后追加的简拼节点
    /// （见 [`Self::add_abbrev_nodes`]），那是音节图看不见的。
    pub fn build(
        &self,
        input: &str,
        graph: &SegGraph,
        dict: &CachedDict,
        fuzzy_config: Option<&FuzzyConfig>,
        unigram: Option<&dyn UnigramLookup>,
        require_reachable: bool,
    ) -> Vec<Vec<LatticeNode>> {
        let input_len = input.len();

        // nodes[end_pos] = 所有在 end_pos 结束的节点
        let mut nodes: Vec<Vec<LatticeNode>> = vec![Vec::new(); input_len + 1];

        for p in 0..input_len.min(graph.len()) {
            if require_reachable && !graph.is_reachable(p) {
                continue;
            }
            for q in graph.ends_within(p, self.max_word_len) {
                if q > input_len {
                    continue;
                }
                let code = &input[p..q];

                for hit in dict.search_with_boundary(code) {
                    // 词条真值边界必须是本跨度上的一条合法切分路径，否则该词根本不是
                    // 用户按这串键敲出来的：「李安」真值 li|an 与单音节边 lian 不符。
                    // boundary == 0（五笔码 / code 超 64 字节 / 旧格式）降级放行 ——
                    // 不设防好过误杀（与全仓其余边界判据一致）。
                    let offsets = match graph.mask_path(p, q, hit.boundary) {
                        MaskCheck::Path(syl_count) => {
                            if syl_count > self.max_word_len {
                                continue;
                            }
                            mask_offsets(hit.boundary, q - p)
                        }
                        MaskCheck::NoInfo => match graph.any_path(p, q, self.max_word_len) {
                            Some(o) => o,
                            None => continue,
                        },
                        MaskCheck::Reject => continue,
                    };
                    let log_prob = score_node(&hit.text, hit.weight, unigram)
                        - AMBIGUOUS_PENALTY * graph.ambiguous_count(p, q, &offsets) as f64;
                    nodes[q].push(LatticeNode {
                        start: p,
                        end: q,
                        word: hit.text,
                        syllables: slice_syllables(code, &offsets),
                        syl_mask: offsets_mask(&offsets),
                        log_prob,
                    });
                }

                // 模糊拼音变体
                //
                // **刻意不做边界校验**：词典返回的 boundary 是**变体码**空间的偏移
                // （zhongguo 的 {0,5}），而本跨度在用户**原码**空间（zong|guo 的
                // {0,4}）。z→zh 这类变体改变码长，两者位偏移不同域，直接比对会把正确的
                // 模糊命中整片误杀。这与 mod.rs 对模糊变体一律置 boundary=0
                // 的既有决策一致，是已记录的永久缺口（待跨域偏移映射），本阶段不碰。
                // 音节标注取图上任意一条最短路径——模糊命中没有可信真值切分，
                // 但节点仍需一个自洽的标注供整句边界回填。
                if let Some(fuzzy) = fuzzy_config.filter(|f| f.any_enabled()) {
                    // **先取切分，再逐音节展开变体**。此前这里对整串 `code` 调
                    // `fuzzy_variants`，而其声母规则是 `starts_with`、韵母规则是 `find`，
                    // 对多音节串只能改到首音节声母与第一处韵母——`zhongzou`→`zhongzhou`
                    // （中州）这类非首音节模糊整片丢失。切分本就在下面 `slice_syllables`
                    // 里用着，只是没回头喂给变体生成（同 P1 记的「信息拿在手上，用完即弃」）。
                    let Some(offsets) = graph.any_path(p, q, self.max_word_len) else {
                        continue;
                    };
                    let syls = slice_syllables(code, &offsets);
                    for variant in FuzzyMatcher::expand_syllables(&syls, fuzzy) {
                        // 全原音节组合 == 原码，属精确命中，已由上面的 search_with_boundary
                        // 循环加入（且带真值边界校验），不可在此重复添加为模糊节点。
                        if variant == code {
                            continue;
                        }
                        for (text, weight, _order) in &dict.search(&variant) {
                            // 去重
                            if nodes[q].iter().any(|n| n.word == *text && n.start == p) {
                                continue;
                            }
                            // 模糊命中同样按图上那条标注路径计歧义罚：惩罚是**切分**的
                            // 属性（该路径是否踩在歧义接缝上），与词条来源无关。
                            let log_prob = score_node(text, *weight, unigram)
                                - 0.5 // 模糊匹配轻微惩罚
                                - AMBIGUOUS_PENALTY * graph.ambiguous_count(p, q, &offsets) as f64;
                            nodes[q].push(LatticeNode {
                                start: p,
                                end: q,
                                word: text.clone(),
                                syllables: slice_syllables(code, &offsets),
                                syl_mask: offsets_mask(&offsets),
                                log_prob,
                            });
                        }
                    }
                }
            }
        }

        nodes
    }

    /// 在已建好的词图上**追加简拼节点**，供混合整句解码（`bzd` + `haobuhao` → 不知道好不好）。
    ///
    /// ## 为什么必须独立枚举跨度
    ///
    /// [`Self::build`] 的跨度来自 `graph.ends_within(p, ..)` —— 音节图的合法终点，且开头
    /// 有 `graph.is_reachable(p)` 守卫。简拼段两条都不满足：`bzdhaobuhao` 的 b/z/d 都不成
    /// 音节，位置 0 在音节图上根本不可达，从它出发也没有任何终点。所以简拼节点走独立枚举：
    /// 任意 `(p, q)` 且 `q - p ∈ [2, MAX_ABBREV_SPAN]`。
    ///
    /// ## 与全拼节点的兼容性
    ///
    /// [`LatticeNode`] 的 `start`/`end` 是**字节跨度**、Viterbi 的 dp 也按字节位置推进，
    /// 故简拼节点与全拼节点在同一张图里天然可串：`[0,3)` 的「不知道」接上 `[3,6)` 的
    /// 「好」，dp 一路推到串尾。**Viterbi 一行都不用改。**
    ///
    /// ## 音节标注
    ///
    /// 简拼段里**每个字母就是一个音节的位置**（击键空间），故 `syllables` 逐字母切、
    /// `syl_mask` 每字母一位。整句 boundary 由此回填出 `b'z'd'hao'bu'hao` 这样的显示，
    /// 与击键串同域——这正是简拼候选一贯的做法（见 `mixed_abbrev` 模块文档）。
    ///
    /// ⚠️ `input` 必须是**原始击键串**。双拼下 `input` 是转换后的全拼、与击键不同域，
    /// 简拼判据会全部失配（文档 §5 约束 4），故调用方须在双拼下跳过本方法。
    pub fn add_abbrev_nodes(
        &self,
        input: &str,
        dict: &CachedDict,
        unigram: Option<&dyn UnigramLookup>,
        nodes: &mut [Vec<LatticeNode>],
    ) {
        let input_len = input.len();
        let bytes = input.as_bytes();
        for p in 0..input_len {
            // 简拼段每个字母都必须是小写 ASCII（声母），一遇到非法字符即可停止从此处出发
            if !bytes[p].is_ascii_lowercase() {
                continue;
            }
            for span in 2..=MAX_ABBREV_SPAN {
                let q = p + span;
                if q > input_len || q >= nodes.len() {
                    break;
                }
                if !bytes[p..q].iter().all(|b| b.is_ascii_lowercase()) {
                    break;
                }
                let stroke = &input[p..q];
                for abbr_code in dict.search_abbrev(stroke, ABBREV_NODE_LIMIT) {
                    for hit in dict.search_with_boundary(&abbr_code) {
                        // **音节数必须等于简拼字母数**（同 mod.rs step5 的过滤）：扁平码有损，
                        // `xa` 指向的 `xian` 回查主表会把 1 音节的「先」一并捞出来。
                        // boundary==0 无从校验，直接跳过——混合整句的每个节点都要求真值切分。
                        if hit.boundary.count_ones() as usize != span {
                            continue;
                        }
                        if nodes[q].iter().any(|n| n.word == hit.text && n.start == p) {
                            continue;
                        }
                        let log_prob = score_node(&hit.text, hit.weight, unigram)
                            - ABBREV_NODE_PENALTY * span as f64;
                        nodes[q].push(LatticeNode {
                            start: p,
                            end: q,
                            word: hit.text,
                            // 击键空间：每个字母一个音节位
                            syllables: stroke.chars().map(|c| c.to_string()).collect(),
                            syl_mask: (0..span).fold(0u64, |m, i| m | (1u64 << i)),
                            log_prob,
                        });
                    }
                }
            }
        }
    }
}

/// 由 bitmask 还原音节起始偏移列表（升序，恒以 0 开头）。
fn mask_offsets(mask: u64, len: usize) -> Vec<usize> {
    let mut out = Vec::new();
    for i in 0..len.min(64) {
        if (mask >> i) & 1 == 1 {
            out.push(i);
        }
    }
    out
}

fn offsets_mask(offsets: &[usize]) -> u64 {
    let mut m = 0u64;
    for &o in offsets {
        if o < 64 {
            m |= 1u64 << o;
        }
    }
    m
}

/// 按起始偏移把 code 切成音节串。
fn slice_syllables(code: &str, offsets: &[usize]) -> Vec<String> {
    let mut out = Vec::with_capacity(offsets.len());
    for (i, &o) in offsets.iter().enumerate() {
        let end = offsets.get(i + 1).copied().unwrap_or(code.len());
        if o <= end && end <= code.len() {
            out.push(code[o..end].to_string());
        }
    }
    out
}
