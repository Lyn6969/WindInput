//! 用 librime-octagram `.gram` 模型实现 [`Grammar`]。
//!
//! 查询流程复刻 `ref/librime-octagram/src/octagram.cc::Query`：
//! 取前文尾部 ≤N 字与当前词首部 ≤N 字，把 context 从最长逐字缩短、每轮查一次，
//! 对所有命中取**最大分**。
//!
//! 分数的**量纲映射不能照抄**，见 [`OctagramConfig`] 的长注释。

use std::path::Path;

use wind_dict::gramdb::{
    self, EncodeBuf, GramDb, MAX_ENCODED_UNICODE, MAX_RESULTS, VALUE_SCALE, encode_chars,
};

use super::grammar::Grammar;

/// 打分参数。
///
/// ## ★★★ 为什么不能照抄 octagram 的常数
///
/// octagram 的语义是：**未命中** `= non_collocation_penalty (−12)`，
/// **命中** `= ln(频次) + collocation_penalty (−12)`。两者共用同一个 −12，
/// 所以那个常数实际是**每转移的固定罚**，而 `ln(频次)` 是纯增益。
///
/// 而我们这一侧 [`super::lattice::WORD_PENALTY`]（= 3.0）**已经在扮演每词固定罚**，
/// 且它是连同 `DICT_TOTAL` 一起被 `pinyin_eval` 标定出来的。直接照搬 −12，
/// 每词罚会变成 15，词数偏好整个崩掉。
///
/// 量纲也对不上：librime 的 `entry_weight = ln(freq)`，我们的
/// `log_prob = ln(freq / DICT_TOTAL)`，差着 `ln(DICT_TOTAL) ≈ 19.3`。
///
/// ## ★★★ 两条约束，以及它们为什么只能靠 `weight` 调和
///
/// 打分形态是 `weight × (ln(频次) − baseline)`，未命中时 `ln` 部分取 0，
/// 于是未命中恰为 `−weight × baseline`。这个式子要同时满足两条约束：
///
/// **① 未命中不能优于命中。** 否则奖励的是「碎片」——完全没有搭配记录的组合拿 0 分，
/// 反而赢过有记录但频次中等的正确词组。实测教训：一版让未命中返回 0 之后，
/// 「建议修改」输给了「见一修改」、「他的意思就是」输给了「他的一死就是」。
///
/// **② 不能每转移重罚。** 否则按词数惩罚长句。实测教训：`weight=1, baseline=12`
/// （即照搬 octagram 的 −12）时，`pinyin_eval` 的 D 类（简拼混合**长句**）
/// top-1 从 12.20% 崩到 3.90%，而 A/B/C（短词）纹丝不动——差异恰好落在词数上。
///
/// librime 能直接用 −12 满足①而不触发②，是因为它的 `entry_weight = ln(freq)` 是
/// **正值**（6.9~19.7），词数增加时 `Σln(freq)` 同步增长、把固定罚抵消掉；
/// 我们的 `log_prob = ln(freq / DICT_TOTAL)` 本来就是负的，抵消不了。
///
/// ⇒ 两条约束并不真正冲突，冲突的是**量级**。保持 octagram 的符号语义（①），
/// 把 `weight` 压小让每词额外罚落在 1~2 而非 12（②）。
/// `baseline` 取实测 `ln(频次)` 中位数（8.34，设计文档 §2.2.3b）。
///
/// - `weight = 0` ⇒ **完全无影响**，逐位退化回接入前。这是标定的安全起点，也是当前默认。
#[derive(Debug, Clone)]
pub struct OctagramConfig {
    /// 整体权重。**0 = 关闭**（逐位等价于不挂模型）。
    pub weight: f64,
    /// 命中时的零点：`ln(频次)` 高于它得正分、低于它得负分。
    /// 取实测中位数 8.34，使命中项的期望≈0，从而不改变词数偏好。
    pub baseline: f64,
    /// context 与 word 各取几个字，对齐 `collocation_max_length`。
    pub collocation_max_length: usize,
    /// 搭配总长不足此值且非整串匹配时，额外扣 [`Self::weak_extra_penalty`]。
    ///
    /// ⚠️ octagram 默认 3，那是给**词级**模型（bgw）用的。
    /// 我们实测 `bgc` 是纯 2-gram（设计文档 §2.2.3d），搭配总长恒为 2，
    /// 默认 3 会让**每一次命中都被判为 weak**。故这里默认 2。
    pub collocation_min_length: usize,
    /// 弱搭配的额外扣分，对齐 `weak_collocation_penalty − collocation_penalty`（= 12）。
    pub weak_extra_penalty: f64,
    /// 句末搭配的额外扣分，对齐 `rear_penalty − collocation_penalty`（= 6）。
    pub rear_extra_penalty: f64,
}

impl Default for OctagramConfig {
    fn default() -> Self {
        Self {
            // 默认关闭：接上模型但不改变任何结果，留给标定逐步放开。
            weight: 0.0,
            // 实测 ln(频次) 的中位数（设计文档 §2.2.3b）
            baseline: 8.34,
            collocation_max_length: 4,
            collocation_min_length: 2,
            weak_extra_penalty: 12.0,
            rear_extra_penalty: 6.0,
        }
    }
}

/// 挂载 `.gram` 的上下文打分器。
pub struct OctagramGrammar {
    db: GramDb,
    config: OctagramConfig,
}

impl OctagramGrammar {
    pub fn open(path: &Path, config: OctagramConfig) -> anyhow::Result<Self> {
        Ok(Self {
            db: GramDb::open(path)?,
            config,
        })
    }

    pub fn unit_count(&self) -> usize {
        self.db.unit_count()
    }

    /// 查出「`context` 之后接 `word`」的最佳搭配分（自然对数域的 `ln(频次)`）。
    /// 没有任何命中时返回 0.0。
    fn best_collocation(&self, context: &str, word: &str, is_rear: bool) -> f64 {
        let n = self
            .config
            .collocation_max_length
            .saturating_sub(1)
            .min(MAX_ENCODED_UNICODE);
        if n == 0 || context.is_empty() {
            return 0.0;
        }

        // context 取**尾部** n 字：用 char_indices().rev() 定位起点，避免收集 Vec<char>。
        let ctx_start = context
            .char_indices()
            .rev()
            .take(n)
            .last()
            .map_or(0, |(i, _)| i);
        let ctx_tail = &context[ctx_start..];

        // word 取**首部** n 字。
        let word_end = word.char_indices().nth(n).map_or(word.len(), |(i, _)| i);
        let word_head = &word[..word_end];
        let word_full = word_end == word.len();

        let mut ctx_buf = EncodeBuf::default();
        let mut word_buf = EncodeBuf::default();
        let mut ctx_len = encode_chars(ctx_tail.chars(), n, &mut ctx_buf);
        let word_len = encode_chars(word_head.chars(), n, &mut word_buf);
        if ctx_len == 0 || word_len == 0 {
            return 0.0;
        }
        let word_key = word_buf.as_slice();

        let mut best = 0.0f64;
        let mut results = [(0i32, 0usize); MAX_RESULTS];

        // context 从最长逐字缩短，每轮查一次，取最大分（对齐 octagram.cc:125-148）。
        let mut ctx_ptr = 0usize;
        let ctx_key_all = ctx_buf.as_slice();
        while ctx_len > 0 {
            let ctx_key = &ctx_key_all[ctx_ptr..];
            if let Some(node) = self.db.traverse(ctx_key, 0) {
                let found = self.db.common_prefix_search(word_key, node, &mut results);
                for &(val, byte_len) in results.iter().take(found) {
                    let match_chars = GramDb::encoded_unicode_len(word_key, byte_len);
                    let collocation_len = ctx_len + match_chars;
                    // 整串匹配（context 用满 + word 全中）豁免 weak 罚，
                    // 对齐 `matches_whole_query`（octagram.cc:99-105）。
                    let whole = ctx_ptr == 0 && byte_len == word_key.len();
                    let penalty = if collocation_len >= self.config.collocation_min_length || whole
                    {
                        0.0
                    } else {
                        -self.config.weak_extra_penalty
                    };
                    best = best.max(val as f64 / VALUE_SCALE + penalty);
                }
            }
            ctx_ptr += gramdb::encoded_char_len(ctx_key_all[ctx_ptr]);
            ctx_len -= 1;
        }

        // 句末：额外查一次 `word + "$"`（`$` 是 octagram 的句末标记）。
        // 仅在 word 未被截断时进行，对齐 octagram.cc:149-158。
        if is_rear
            && word_full
            && let Some(node) = self.db.traverse(word_key, 0)
        {
            let found = self.db.common_prefix_search(b"$", node, &mut results);
            if found > 0 {
                best = best.max(results[0].0 as f64 / VALUE_SCALE - self.config.rear_extra_penalty);
            }
        }

        best
    }
}

impl Grammar for OctagramGrammar {
    fn query(&self, context: &str, word: &str, is_rear: bool) -> f64 {
        if self.config.weight == 0.0 {
            return 0.0;
        }
        let hit = self.best_collocation(context, word, is_rear);
        // 未命中时 `hit == 0`，于是本式给出 `−weight × baseline`——**未命中必须是负的**，
        // 理由见类型注释「两条约束」。分寸完全由 `weight` 控制。
        self.config.weight * (hit - self.config.baseline)
    }
}
