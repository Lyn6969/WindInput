//! 词频重排（排序独立维度，frequency.md §3/§4）。
//!
//! **绝不改 weight**：词频只按 redb 记录 `{count, last_used}` 参与排序，不回写
//! `Candidate::weight`，词库数据不被污染。
//!
//! 拼音侧走**位置提升**模型（`docs/design/freq-rerank-model.md`）：候选按**位次**前移
//! （`base_pos / 2^count`），完全不看权重数值。此前三版都试图在权重轴上解决问题，
//! 全部失败于同一点——体感由词库的权重分布决定，而那个分布不可控（`de` 下「的」是第二名
//! 的 486 倍，`siyuan` 下「寺院」只是「思源」的 2 倍）。
//!
//! ⚠ 「不改 weight」≠「不改顺序」。`rerank_codetable_usedfirst` 的**首要键是 `freq_tier`**，
//! 与协调器 `candidate_display_order` 的匹配层级是两个正交维度——只要存在词频记录，档位序
//! 就整体压过前一步的排序结果（稳定排序只保住档内相对序）。`rerank_pinyin_positional` 则显式
//! 复刻了层级（调用 `cmp_match_layers`）故不会跨层提拔。改本模块前先想清楚要改的是哪一种。
//!
//! 两种语义按引擎类型分流，**它们的模型已经不同**：
//! - 码表/混输（§3）：**永久布尔 used-first**——用过的（count>0）档内上浮，不衰减。
//!   码表调频默认关闭且有 `ProtectPolicy` 按码长保护首选，实测无越权问题，故维持原样。
//! - 纯拼音：**位置提升**——`target_pos = base_pos / 2^count`，衰减乘在次数上。取代了原先
//!   的「布尔 used-first + 衰减分 + 阈值褪色」，也取代了中途试过的等效权重方案。
//!
//! 设计归属：frequency.md §5/§7 明确把词频重排放在 engine 排序层（持 store freq 只读访问），
//! 而非 dict 查询层或 coordinator。本模块即该排序层的纯函数实现，由 coordinator 在排序后调用。

use crate::manager::FreqStrategy;
use std::collections::HashMap;
use wind_candidate::Candidate;
use wind_store::freq::{FreqProfile, FreqRecord};

/// 按**输入码长**分级的首选保护策略（见 docs/design/codetable-freq-short-code-protection.md）。
///
/// 保护的是「用户当前所在这个码位的钦定首选」，故按**输入码长**分级而非候选码长——
/// 精确档内两者相等，但前缀补全候选的码更长，用候选码长会把分级判据搅乱。
///
/// 五笔一简 25 个码**每个都是二选一**（`a` → 工 9999 / 戈 9998），词库靠权重表达的钦定
/// 地位在本模块完全失效（比较链不含 weight），故简码位默认保护首选 1 位；全码位默认放开，
/// 那里才是调频该起作用的地方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtectPolicy {
    /// 索引 0/1/2 = 码长 1/2/3（一简/二简/三简位）。
    pub by_len: [usize; 3],
    /// 码长 ≥ 4（未单列的深码位）。
    pub fallback: usize,
}

impl Default for ProtectPolicy {
    fn default() -> Self {
        Self {
            by_len: [1, 1, 0],
            fallback: 0,
        }
    }
}

impl ProtectPolicy {
    /// 空策略（全不保护）：拼音路径与「关闭保护」用。
    pub const NONE: Self = Self {
        by_len: [0; 3],
        fallback: 0,
    };

    /// 本次输入码长对应的保护位数。空码不保护；码长 ≥ 4 落兜底档。
    pub fn resolve(&self, code: &str) -> usize {
        match code.chars().count() {
            0 => 0,
            n if n <= self.by_len.len() => self.by_len[n - 1],
            _ => self.fallback,
        }
    }
}

/// 候选来源档位（数字越小越靠前）。五笔优先的硬约束：码表精确全码恒在拼音之上，
/// 词频重排只在同档内调整。纯拼音/纯码表模式下同源候选档位相同，退化为按词频排序。
///
/// ```text
/// 0  码表精确全码（code == input）
/// 1  精确码短语（is_phrase && is_exact_code）
/// 2  拼音精确档（is_pinyin_exact_tier：精确音节 + 常用字）  ← 先于码表前缀补全
/// 3  码表前缀补全 + 前缀短语
/// 4  拼音其余（前缀补全/子短语/简拼/模糊/生僻） + 英文
/// ```
///
/// ★ 档 2 是「五笔优先」的一处**有意松动**：码表**精确**仍恒先于拼音（档 0 < 档 2），但码表
/// **前缀补全**要让位于拼音精确匹配。理由是短输入下二者置信度恰好反相关——`xu` 的 124 条码表
/// 前缀补全全都要打满 4 码才精确，而拼音 `xu` 已是完整音节。
///
/// ⚠ 下面的 `c.code == input` 与 `Candidate::is_exact_code`（见 `wind_candidate::cmp_exact_first`）
/// 是同一概念的两份判据，纯码表路径结论一致。未合并是因为本档位还承载词频语义（`is_phrase`
/// 独占档 1、按来源分 Pinyin/English 档）。**改动任一处须同步核对另一处**——本函数的档位是
/// `rerank_codetable_usedfirst` 的首要键，开启自动调频时会整体压过协调器的显示序，
/// 也因此会掩盖 `is_exact_code` 的效果（验证精确匹配优先时须关闭自动调频）。
fn freq_tier(c: &Candidate, input: &str) -> u8 {
    use wind_candidate::CandidateSource::*;
    if c.is_phrase {
        // 短语按「完全匹配 vs 前缀匹配」再分档，勿因 is_phrase 一刀切抬到码表前缀补全之上：
        // - 精确码短语（`lookup`，码==输入的完全匹配 → `is_exact_code=true`）留 tier 1、紧随码表精确；
        // - 前缀短语（`lookup_prefix` 命中 → `is_exact_code=false`）降到 tier 3，与码表前缀补全同档。
        //   否则混输/拼音下打 `da` 会让 `date` 短语只因 is_phrase 就压过码表前缀补全（如 矼）。
        //   与协调器 `candidate_display_order`（is_exact_code/is_prefix）、混输侧口径对齐。
        return if c.is_exact_code { 1 } else { 3 };
    }
    // 拼音精确档（tier 2）：先于码表前缀补全。与协调器 `candidate_display_order` 的
    // `cmp_pinyin_exact_first` **共用同一个判据函数**（红线③：三套排序系统口径必须一致）。
    // 混输打 `xu` 时拼音「需」若只按来源落 tier 4，会被码表 `xu*` 的 124 条前缀补全整体压住 ——
    // 那正是本档要修的现场，且开自动调频时 `freq_tier` 是首要键、会整体压过协调器显示序，
    // 只改协调器一侧等于没改。
    //
    // ⚠️ 这里**不必**像协调器那样区分「是否混输」：本函数只服务
    // `rerank_codetable_usedfirst`（码表 / 混输），纯拼音走 `rerank_pinyin_positional` 不经过此处；
    // 而纯码表下没有 `Pinyin` 来源候选，本档天然是空操作。
    if wind_candidate::is_pinyin_exact_tier(c, input.len()) {
        return 2;
    }
    match c.source {
        CodeTable if c.code == input => 0, // 码表精确全码（如五笔 cang→駏）
        CodeTable => 3,                    // 码表前缀补全
        Pinyin => 4,                       // 拼音（非精确档：前缀补全/子短语/简拼/模糊/生僻）
        English => 4,
        _ => 3,
    }
}

/// 码表/混输词频重排（§3）：档位感知的**永久** used-first（五笔优先）。
/// 先按来源档位（码表精确 < 精确码短语 < **拼音精确** < {码表前缀补全, 前缀短语} < 拼音其余），
/// 档内再 used-first + 策略排序。前缀短语与码表前缀补全同档＝短语不因 is_phrase 抬到补全之上。
/// 稳定排序保证同档无记录者维持引擎权重序，绝不把拼音浮到五笔精确全码之上。
///
/// 策略：`Step`（默认/逐次提升）count 降序、last_used 降序 tiebreak（抗误选）；
/// `Top`（一次到顶/MRU）last_used 降序、count 降序 tiebreak（最近选的置该档之首）。
pub fn rerank_codetable_usedfirst(
    candidates: &mut [Candidate],
    recs: &HashMap<String, FreqRecord>,
    code: &str,
    strategy: FreqStrategy,
    protect: ProtectPolicy,
) {
    // 呈现层保护：记录基础序前 N 位，重排后原序回填（不动 weight，见 frequency.md §8）。
    // 保护位数按输入码长分级——简码位（一简/二简）的钦定首选不该被一次误选永久改写，
    // 而全码位正是调频该起作用的地方。
    //
    // 名额**只在精确档内取**（`is_exact_code`）：钦定首选一定是精确档，把名额匀给前缀补全
    // 没有语义依据——那会把一个碰巧排在前面的补全词钉死，还挡住用户真正常用的那条。
    // 精确候选不足名额数就少保护；该码位没有精确候选（打了词库里没有的码）则不保护。
    let protect_n = protect.resolve(code);
    let protected: Vec<String> = candidates
        .iter()
        .filter(|c| c.is_exact_code)
        .take(protect_n)
        .map(|c| c.text.clone())
        .collect();

    use std::cmp::Ordering;
    candidates.sort_by(|a, b| {
        let ta = freq_tier(a, code);
        let tb = freq_tier(b, code);
        if ta != tb {
            return ta.cmp(&tb);
        }
        match (recs.get(&a.text), recs.get(&b.text)) {
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal, // 同档均无记录 → 维持引擎权重序（稳定排序）
            (Some(ra), Some(rb)) => match strategy {
                FreqStrategy::Top => rb
                    .last_used
                    .cmp(&ra.last_used)
                    .then(rb.count.cmp(&ra.count)),
                FreqStrategy::Step => rb
                    .count
                    .cmp(&ra.count)
                    .then(rb.last_used.cmp(&ra.last_used)),
            },
        }
    });

    for (i, text) in protected.iter().enumerate() {
        if let Some(pos) = candidates.iter().position(|c| &c.text == text)
            && pos > i
        {
            candidates[i..=pos].rotate_right(1);
        }
    }
}

/// 位置提升的**步长底数**：每积累一次有效使用，候选的目标位次除以它。
///
/// 取 2（每次减半）是拿实测的微软拼音行为标定的——用户观察「中间档的字 1 次提前一半、
/// 2 次进第 2 位、再 1 次到首位」，位次 10→5→2→1 与减半逐条吻合。
///
/// **这是位次的除数，不是权重的步长**，两者性质完全不同：权重步长的体感取决于词库的
/// 权重分布（`de` 下「的」是第二名的 486 倍、`siyuan` 下「寺院」只是「思源」的 2 倍，
/// 同一个步长在两处天差地别），位次除数则与分布无关——第 2 位就是第 2 位。
pub const POSITION_HALVING_BASE: u32 = 2;

/// 衰减殆尽的下限：有效强度低于此值视为「没用过」。
///
/// **必需，不是保险丝**。位次是整数，`base_pos = 1`（第 2 位）时任何 `divisor > 1` 都会让
/// `1 / divisor < 1` 而 floor 到 0——即一年前用过一次、衰减到 0.01 的记录仍能把第 2 位顶上
/// 首位。取 0.5 的语义是「累计有效使用不足半次」。
///
/// 这相当于把旧模型的「阈值褪色」以更合理的形式带回：作用在**归一化的有效次数**上，
/// 而不是在权重分上（后者的阈值随词库分布漂移，正是被推翻的那套）。
const MIN_PROMOTION_POWER: f64 = 0.5;

/// 候选的**有效提升强度**：`count` 经半衰期衰减，**保留小数**。
///
/// ⚠️ **不能取整**。曾写作 `(count as f64 * decay) as u32`，而 `as` 是向零截断：
/// `count = 1` 的记录只要过了一瞬间，`decay` 就略小于 1（选完 1 分钟后 ≈ 0.99984），
/// 截断即得 0——**用一次的记录几乎立刻失效**，真机表现为「选了词，再打还是老样子」。
/// 而单测全部用 `now == last_used`（decay 恰为 1.0）喂入，正好落在唯一能通过的点上，
/// 全绿却掩盖了它。
///
/// 保留小数后除数是连续的（`2^0.99984 ≈ 1.9998`），衰减平滑无跳变。
///
/// 上限 64：`2^64` 已远超任何候选列表长度，同时防止 `powf` 溢出成 `inf`。
fn promotion_power(
    c: &Candidate,
    recs: &HashMap<String, FreqRecord>,
    now: i64,
    profile: FreqProfile,
) -> f64 {
    recs.get(&c.text).map_or(0.0, |r| {
        let eff = r.count as f64 * profile.decay_factor(r, now);
        if eff < MIN_PROMOTION_POWER {
            return 0.0;
        }
        eff.min(64.0)
    })
}

/// 按有效强度算目标位次：`base_pos / BASE^power`。
fn target_position(base_pos: usize, power: f64) -> usize {
    if power <= 0.0 {
        return base_pos;
    }
    let divisor = (POSITION_HALVING_BASE as f64).powf(power);
    (base_pos as f64 / divisor).floor() as usize
}

/// 拼音词频重排：**位置提升**模型（`docs/design/freq-rerank-model.md`）。
///
/// ```text
/// target_pos = base_pos / (HALVING_BASE ^ effective_count)
/// ```
///
/// 排序键：`(锚定, 匹配层级, target_pos, 是否被提升, base_pos)`。
///
/// # 为什么是位次而不是权重
///
/// 前三版都试图在**权重轴**上解决问题（布尔闸门 → 等效权重 → 局部插值），全部失败于同一点：
/// **体感由词库的权重分布决定，而那个分布不可控**。`de` 下 rime-ice 把「的」抬到第二名的
/// 486 倍，`siyuan` 下「寺院」只是「思源」的 2 倍——同一套参数在前者慢得离谱、在后者快得
/// 离谱，调参只是在两个坏结果之间挪，换本词库还要重调。
///
/// 位次把体感与分布**彻底解耦**：第 2 位用一次就能到首位，第 40 位要六次，与它们的绝对
/// 权重无关。这也是实测的微软拼音行为，且与搜狗/fcitx5 的框架自洽——它们都是「加权混合」，
/// 而混合的前提是**先归一化**；用位次归一化，加权混合即退化为本模型。
///
/// # 契约：入参必须已按显示序排好
///
/// `base_pos` 取的就是入参下标，所以调用方必须先跑 `candidate_display_order`。协调器
/// `handle_candidate.rs` 的顺序（display_order → filter → **本函数** → shadow）满足这一点，
/// 且本函数是最后一道整体排序，其结果不会被后续按权重重排推翻。
///
/// # 三层的分工
///
/// ① **锚定**（`is_sentence` 或精确码短语）恒占顶部、互相维持原序，**不参与位置提升**。
///    `is_sentence_demoted` / `is_sentence_contested` 的整句不在此列——后者正是为了让同码
///    竞争者能靠位置提升反超它（`siyuan` 寺院/思源）。
/// ② **匹配层级** `cmp_match_layers`：词频不得跨层提拔（模糊「是」不能压过精确「四」）。
/// ③ **目标位次**升序；同位次时**被提升者在前**——否则提升到 0 的候选会与原本就在 0 位的
///    并列，再按 `base_pos` 排又回到原位，提升等于没做。
pub fn rerank_pinyin_positional(
    candidates: &mut [Candidate],
    recs: &HashMap<String, FreqRecord>,
    now: i64,
    profile: FreqProfile,
) {
    let n = candidates.len();
    if n < 2 {
        return;
    }
    let anchored = |c: &Candidate| {
        (c.is_sentence && !c.is_sentence_demoted && !c.is_sentence_contested)
            || (c.is_phrase && c.is_exact_code)
    };
    // 预计算 (锚定, 目标位次, 是否真的前移)。base_pos 即入参下标，见上文契约。
    let meta: Vec<(bool, usize, bool)> = candidates
        .iter()
        .enumerate()
        .map(|(pos, c)| {
            let power = promotion_power(c, recs, now, profile);
            let target = target_position(pos, power);
            (anchored(c), target, target < pos)
        })
        .collect();

    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| {
        let (aa, ab) = (meta[a].0, meta[b].0);
        if aa != ab {
            return ab.cmp(&aa); // 锚定者在前
        }
        if aa {
            return a.cmp(&b); // 均锚定 → 维持原序，不参与提升
        }
        wind_candidate::cmp_match_layers(&candidates[a], &candidates[b])
            .then_with(|| meta[a].1.cmp(&meta[b].1)) // 目标位次升序
            .then_with(|| meta[b].2.cmp(&meta[a].2)) // 同位次：被提升者在前
            .then(a.cmp(&b)) // 其余维持原序（稳定）
    });

    // 诊断：本模块此前**零日志**——真机上「选了词、再打还是老样子」时无从判断是没记录、
    // 没调用、还是提升被算成 0（那次的根因是 `as u32` 截断）。这里补一条。
    //
    // 用 `trace!` 而非更高级别：候选文本属用户输入内容，INFO 及以上不得记录。
    // 先判 `enabled!` 再拼串，避免热路径上白白格式化。
    if tracing::enabled!(tracing::Level::TRACE) {
        let moved: Vec<String> = idx
            .iter()
            .enumerate()
            .filter(|&(new_pos, &old_pos)| new_pos != old_pos)
            .map(|(new_pos, &old_pos)| {
                format!(
                    "{}:{}->{}(p={:.3})",
                    candidates[old_pos].text,
                    old_pos,
                    new_pos,
                    promotion_power(&candidates[old_pos], recs, now, profile)
                )
            })
            .collect();
        if moved.is_empty() {
            tracing::trace!(records = recs.len(), "词频重排：无候选移位");
        } else {
            tracing::trace!(records = recs.len(), moved = %moved.join(" "), "词频重排");
        }
    }

    // 应用置换：move 而非 clone（候选含 String，上千条时 clone 不可忽略）
    let mut reordered: Vec<Candidate> = Vec::with_capacity(n);
    for &i in &idx {
        reordered.push(std::mem::take(&mut candidates[i]));
    }
    for (slot, c) in candidates.iter_mut().zip(reordered) {
        *slot = c;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wind_candidate::CandidateSource;

    fn pin(text: &str, weight: i32) -> Candidate {
        Candidate {
            text: text.to_string(),
            weight,
            consumed_length: 0,
            source: CandidateSource::Pinyin,
            ..Default::default()
        }
    }

    fn ct(code: &str, text: &str, weight: i32) -> Candidate {
        Candidate {
            code: code.to_string(),
            text: text.to_string(),
            weight,
            consumed_length: 0,
            source: CandidateSource::CodeTable,
            ..Default::default()
        }
    }

    fn recs(items: &[(&str, u32, i64)]) -> HashMap<String, FreqRecord> {
        items
            .iter()
            .map(|(t, c, lu)| {
                (
                    t.to_string(),
                    FreqRecord {
                        count: *c,
                        last_used: *lu,
                    },
                )
            })
            .collect()
    }

    const NOW: i64 = 1_700_000_000;

    fn pin_sentence(text: &str, weight: i32) -> Candidate {
        let mut c = pin(text, weight);
        c.is_sentence = true;
        c
    }

    /// 整句豁免：整句恒置顶，即使某非整句词被频繁使用也不能反超。
    #[test]
    fn pinyin_sentence_is_anchored_on_top() {
        let mut cands = vec![
            pin_sentence("你好世界", 30_000_000),
            pin("你好", 2000),
            pin("拟", 1000),
        ];
        let r = recs(&[("你好", 20, NOW)]);
        rerank_pinyin_positional(&mut cands, &r, NOW, FreqProfile::default());
        assert_eq!(cands[0].text, "你好世界", "整句必须锚定首位");
        assert_eq!(cands[1].text, "你好", "近用词软置前于未用词");
        assert_eq!(cands[2].text, "拟");
    }

    /// 降级整句**不参与**锚定：让位于精确整词后，须停在精确整词之后、普通候选之前。
    ///
    /// 这一条是整个降级方案的另一半。引擎侧只改了 weight，而本比较器**不看 weight**——
    /// 只要标志为真就置顶。若此处不豁免，引擎那半边等于白改：实测过权重顶到 2e9
    /// 都赢不过这里的锚定。
    ///
    /// **入参顺序即协调器 `candidate_display_order` 的输出**（`handle_candidate.rs:528`
    /// 先按权重层级排好，`:530` 才调本函数）。本函数在同层同衰减分时返回
    /// `Ordering::Equal`，靠稳定排序保住入参序 —— 它**从不自己比权重**，故测试必须
    /// 按显示序喂入，否则测的是一个生产中不存在的状态。
    #[test]
    fn pinyin_demoted_sentence_yields_to_exact_word() {
        let mut cands = vec![
            pin("廉政提醒", 100_000),
            {
                // 引擎侧已把权重降到「最低的精确整词 - 1」
                let mut c = pin_sentence("连整体性", 99_999);
                c.is_sentence_demoted = true;
                c
            },
            {
                // 子短语层：无论权重多高都该留在整句之后（由 cmp_match_layers 保证）
                let mut c = pin("连", 500_000);
                c.is_partial = true;
                c
            },
        ];
        let r = recs(&[]);
        rerank_pinyin_positional(&mut cands, &r, NOW, FreqProfile::default());
        assert_eq!(cands[0].text, "廉政提醒", "精确整词须在降级整句之前");
        assert_eq!(cands[1].text, "连整体性", "降级整句仍须在普通候选之前");
        assert_eq!(cands[2].text, "连");
    }

    /// 对照组：**未**降级的整句即使排在后面也会被锚定拉回首位。与上一条合看，
    /// 才证明 `is_sentence_demoted` 确实是起作用的那个开关，而非「本函数恰好没动它」。
    #[test]
    fn pinyin_undemoted_sentence_still_anchors_from_below() {
        let mut cands = vec![pin("廉政提醒", 100_000), pin_sentence("连整体性", 99_999)];
        let r = recs(&[]);
        rerank_pinyin_positional(&mut cands, &r, NOW, FreqProfile::default());
        assert_eq!(
            cands[0].text, "连整体性",
            "未降级的整句仍恒锚定首位（本函数不看 weight）"
        );
    }

    /// 降级整句失去锚定后，词频学习对它生效（未降级的整句则恒锚定，见上一条）。
    #[test]
    fn pinyin_demoted_sentence_participates_in_freq() {
        let mut cands = vec![pin("精确整词", 100_000), {
            let mut c = pin_sentence("降级整句", 99_999);
            c.is_sentence_demoted = true;
            c
        }];
        // 需 W_freq > 100_000 才能翻过「精确整词」⇒ count > 100000/1307 ≈ 76.5。
        // 旧值 20 是布尔闸门时代的遗留——那时 count 只用来过 ε 阈值，多少都一样。
        let r = recs(&[("降级整句", 100, NOW)]);
        rerank_pinyin_positional(&mut cands, &r, NOW, FreqProfile::default());
        assert_eq!(
            cands[0].text, "降级整句",
            "降级整句不再锚定，故可凭词频重新浮上来"
        );
    }

    /// 有同码竞争者的整句（`is_sentence_contested`）**不再锚定**：同码精确整词可凭词频反超。
    ///
    /// 现场 `siyuan`：「寺院」既是词典精确整词、又被 Viterbi 选为最优解（step 2 同文合并
    /// 继承整句身份，weight 被抬到 SENTENCE_WEIGHT_BASE 量纲），而「思源」同码。锚定是
    /// 硬闸门，实测灌到 count=5000 都翻不动 —— 词频对该编码整体失效。
    #[test]
    fn pinyin_contested_sentence_yields_to_used_peer() {
        let mut cands = vec![pin_contested("寺院"), pin("思源", 245)];
        // 第 2 位用一次即可提升到第 0 位。
        let r = recs(&[("思源", 1, NOW)]);
        rerank_pinyin_positional(&mut cands, &r, NOW, FreqProfile::default());
        assert_eq!(
            cands[0].text,
            "思源",
            "有同码竞争者的整句须接受词频挑战，实际: {:?}",
            cands.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
        assert_eq!(cands[1].text, "寺院", "整句退居第二，不是被赶出列表");
    }

    /// 与上一条配对：**无词频记录时 contested 整句仍居首**。
    ///
    /// 本字段只摘锚定、不动 weight —— 引擎那边「寺院」拿的仍是 3e7 量纲，靠调用方喂进来的
    /// 权重序 + 稳定排序保住首位。若哪天误把本字段做成降权，这条会挂。
    #[test]
    fn pinyin_contested_sentence_still_leads_without_freq() {
        let mut cands = vec![
            {
                let mut c = pin_sentence("寺院", 29_984_561);
                c.is_sentence_contested = true;
                c
            },
            pin("思源", 245),
        ];
        let r = recs(&[]);
        rerank_pinyin_positional(&mut cands, &r, NOW, FreqProfile::default());
        assert_eq!(
            cands[0].text, "寺院",
            "无使用记录时整句仍是最优解读，须维持首位"
        );
    }

    /// 锚定按来源语义而非权重数值：高权重但非整句的候选（如别的功能提权到 30M 的词）
    /// 仍须正常参与词频重排，不得被误当整句锚定而永久失去词频学习能力。
    /// 这正是把 `weight >= 20M` 阈值换成 `is_sentence` 标记要解决的问题。
    #[test]
    fn pinyin_high_weight_non_sentence_still_learns() {
        // 92,194 = 8105 常用字表的 p99，是非整句候选够得到的真实量级
        // （早先用例写 30,000,000，那是 SENTENCE_WEIGHT_BASE 量级，非整句候选到不了）。
        let mut cands = vec![pin("高权非整句", 92_194), pin("近用低权", 100)];
        let r = recs(&[("近用低权", 1, NOW)]);
        rerank_pinyin_positional(&mut cands, &r, NOW, FreqProfile::default());
        assert_eq!(
            cands[0].text, "近用低权",
            "非整句候选不享锚定豁免，第 2 位用一次即可超过它"
        );

        // 配对用例（分别在别处）：`anchored_sentence_is_immune_to_promotion` 验证锚定者
        // 用 50 次也纹丝不动，`no_record_means_no_movement` 验证无记录时一步不挪。
        //
        // 此处不再写「次数不足时压不动」的对照——位置模型下只有两个候选，第 2 位用一次
        // 就到顶，不存在「次数不足」这个状态。那是等效权重模型的遗留断言。
    }

    /// 精确码短语（`is_phrase && is_exact_code`，lookup 完全匹配）与整句同享锚定豁免。
    /// 前缀短语不锚定，见 `pinyin_prefix_phrase_not_anchored`。
    #[test]
    fn pinyin_exact_phrase_is_anchored_like_sentence() {
        let mut cands = vec![pin("普通词", 5000), {
            let mut c = pin("我的邮箱", 40_000_000);
            c.is_phrase = true;
            c.is_exact_code = true; // 精确码短语才锚定
            c
        }];
        let r = recs(&[("普通词", 30, NOW)]);
        rerank_pinyin_positional(&mut cands, &r, NOW, FreqProfile::default());
        assert_eq!(cands[0].text, "我的邮箱", "精确码短语须与整句同享锚定");
    }

    /// 前缀短语**不锚定**：`is_phrase && !is_exact_code`（`lookup_prefix` 命中）落到匹配层，
    /// 靠 is_prefix 降到精确候选之下，不因 is_phrase 被顶到首位。
    ///
    /// 回归拼音潜伏现场：打 `da` 时 `date` 前缀短语曾被 `|| is_phrase` 一刀切锚定到首位（开
    /// 自动调频且有词频记录时触发）。入参按 candidate_display_order 输出序（精确拼音字在前、
    /// 前缀短语在后）。旧码（`|| is_phrase`）下前缀短语会锚到首位 → 本用例会红。
    #[test]
    fn pinyin_prefix_phrase_not_anchored() {
        let exact_word = pin("大", 5000); // 精确拼音字：is_prefix=false, is_exact_code=false
        let prefix_phrase = {
            let mut c = pin("date短语", 40_000_000); // 高权重也不该把它顶起
            c.is_phrase = true;
            c.is_prefix = true; // is_exact_code 默认 false
            c
        };
        let mut cands = vec![exact_word, prefix_phrase];
        let r = recs(&[("大", 3, NOW)]); // 有词频记录 → 重排确实生效
        rerank_pinyin_positional(&mut cands, &r, NOW, FreqProfile::default());
        assert_eq!(
            cands[0].text,
            "大",
            "前缀短语(is_phrase && !is_exact_code)不锚定，须留在精确拼音候选之下，实际: {:?}",
            cands.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
        assert_eq!(cands[1].text, "date短语");
    }

    /// 衰减软置前：近用词（衰减分 ≥ ε）浮到未用词之上，即使权重更低。
    #[test]
    fn pinyin_recent_use_floats_above_higher_weight() {
        let mut cands = vec![pin("低频高权", 5000), pin("近用低权", 100)];
        let r = recs(&[("近用低权", 8, NOW)]);
        rerank_pinyin_positional(&mut cands, &r, NOW, FreqProfile::default());
        assert_eq!(cands[0].text, "近用低权", "近期使用应软置前");
    }

    fn pin_fuzzy(text: &str, weight: i32) -> Candidate {
        let mut c = pin(text, weight);
        c.is_fuzzy = true;
        c
    }

    /// 词频 used-first **不因「模糊来源」而豁免**：用户在 "si" 下反复选过模糊命中「是」，
    /// 词频重排应照常把它软置前——`is_fuzzy` 只是「召回来源」标记，不构成学习壁垒。
    ///
    /// **本测试断言的是与从前相反的行为**（原名 `pinyin_exact_ranks_above_used_fuzzy`，
    /// 断言「精确恒优先于被使用过的模糊命中」）。那套语义依赖 `cmp_match_layers` 把
    /// `is_fuzzy` 当首要层级键，而该分层在真实词库下把模糊候选整体压到 200 名开外
    /// （`si` 下「是」第 231 位），远超 50~300 的生产候选上限，使模糊音在拼音 / 混输 /
    /// 临拼三条路径上全部等价于未实现。层级键已废除，模糊惩罚改由引擎在 weight 上施加
    /// （见 `pinyin::FUZZY_WEIGHT_SCALE`）——真实链路进入本函数时「是」的权重已被折过，
    /// 词频再据用户实际选择调整是合理的：用户反复选它，说明他要的就是它。
    #[test]
    fn pinyin_used_fuzzy_can_rank_above_exact() {
        let mut cands = vec![
            pin_fuzzy("是", 9000), // 模糊命中，且被频繁使用过
            pin("四", 100),        // 精确命中，未使用
        ];
        let r = recs(&[("是", 4, NOW)]); // 「是」有使用记录（模拟 si→是 count=4）
        rerank_pinyin_positional(&mut cands, &r, NOW, FreqProfile::default());
        assert_eq!(
            cands[0].text,
            "是",
            "用户反复选过的模糊命中应被词频软置前，实际: {:?}",
            cands.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
        assert_eq!(cands[1].text, "四");
    }

    /// 阈值褪色：久未用（衰减分 < ε）失去 used-first 资格，落回引擎权重序。
    #[test]
    fn pinyin_faded_use_falls_back_to_weight_order() {
        let long_ago = NOW - 365 * 24 * 3600; // 一年前用过一次 → 衰减远小于 ε
        let mut cands = vec![pin("高权未用", 5000), pin("陈旧低权", 100)];
        let r = recs(&[("陈旧低权", 1, long_ago)]);
        rerank_pinyin_positional(&mut cands, &r, NOW, FreqProfile::default());
        assert_eq!(cands[0].text, "高权未用", "褪色词应落回权重序，高权在前");
    }

    /// 码表 used-first（step）：用过的词按 count 降序置前，未用维持权重序殿后。
    #[test]
    fn codetable_step_orders_by_count() {
        let mut cands = vec![
            ct("aaaa", "工", 100),
            ct("aaaa", "戈", 90),
            ct("aaaa", "啊", 80),
        ];
        let r = recs(&[("戈", 5, NOW), ("工", 2, NOW)]);
        rerank_codetable_usedfirst(
            &mut cands,
            &r,
            "aaaa",
            FreqStrategy::Step,
            ProtectPolicy::NONE,
        );
        assert_eq!(cands[0].text, "戈", "step：count 高者置前");
        assert_eq!(cands[1].text, "工");
        assert_eq!(cands[2].text, "啊", "未用词维持权重序殿后");
    }

    /// 码表 used-first（top/MRU）：最近用的置该档之首，与 count 无关。
    #[test]
    fn codetable_top_orders_by_recency() {
        let mut cands = vec![ct("aaaa", "工", 100), ct("aaaa", "戈", 90)];
        // 工 用 10 次但很久前；戈 仅 1 次但刚用
        let r = recs(&[("工", 10, NOW - 10_000), ("戈", 1, NOW)]);
        rerank_codetable_usedfirst(
            &mut cands,
            &r,
            "aaaa",
            FreqStrategy::Top,
            ProtectPolicy::NONE,
        );
        assert_eq!(cands[0].text, "戈", "top：最近使用者置首");
    }

    /// 五笔优先档位：拼音候选即便高频近用，也不能浮到码表精确全码之上（混输硬约束）。
    #[test]
    fn mixed_tier_keeps_codetable_exact_above_pinyin() {
        let mut cands = vec![ct("aaaa", "工", 100), pin("啊", 5000)];
        // 拼音「啊」高频近用，但档位 3 低于码表精确全码档位 0
        let r = recs(&[("啊", 50, NOW)]);
        rerank_codetable_usedfirst(
            &mut cands,
            &r,
            "aaaa",
            FreqStrategy::Step,
            ProtectPolicy::NONE,
        );
        assert_eq!(cands[0].text, "工", "码表精确全码档位最高，拼音不得反超");
    }

    /// 兜底档 `fallback=1`：重排后基础序首位被回填锁定，高词频候选在保护位之后正常上浮。
    ///
    /// 三条候选的 `code` 均等于输入，故都属精确档（保护名额只在该档内取，见
    /// `protect_slots_taken_from_exact_only`）。
    #[test]
    fn protect_top_n_pins_original_head() {
        // 基础序：甲(高weight) 乙 丙；"丙"有词频记录本应浮首。
        let mut cands = vec![
            ct_exact("abcd", "甲", 300),
            ct_exact("abcd", "乙", 200),
            ct_exact("abcd", "丙", 100),
        ];
        let mut recs_map = HashMap::new();
        recs_map.insert(
            "丙".to_string(),
            FreqRecord {
                count: 9,
                last_used: 1000,
            },
        );
        rerank_codetable_usedfirst(
            &mut cands,
            &recs_map,
            "abcd",
            FreqStrategy::Step,
            ProtectPolicy {
                by_len: [0; 3],
                fallback: 1,
            },
        );
        assert_eq!(cands[0].text, "甲", "protect_top_n=1 应锁定原首位");
        assert_eq!(cands[1].text, "丙", "词频候选在保护位之后正常上浮");
    }

    /// 精确档候选（`code == input`）。五笔简码字与全码字都属此档，是「词库钦定首选」的载体。
    fn ct_exact(code: &str, text: &str, weight: i32) -> Candidate {
        let mut c = ct(code, text, weight);
        c.is_exact_code = true;
        c
    }

    /// 分级表按输入码长取值，码长 ≥ 4 落兜底档。
    #[test]
    fn protect_policy_resolve_by_len() {
        let p = ProtectPolicy {
            by_len: [3, 2, 1],
            fallback: 9,
        };
        assert_eq!(p.resolve("a"), 3, "一简位");
        assert_eq!(p.resolve("aa"), 2, "二简位");
        assert_eq!(p.resolve("aaa"), 1, "三简位");
        assert_eq!(p.resolve("aaaa"), 9, "全码位落兜底");
        assert_eq!(p.resolve("aaaaaa"), 9, "超长码同样落兜底");
        assert_eq!(p.resolve(""), 0, "空码不保护");
    }

    /// **主用例**：一简位（码长 1）的词库钦定首选不被词频顶掉。
    ///
    /// 现场取自发行词库 `wubi86_jidian.dict.yaml`：`a` → 工(9999) / 戈(9998)。一简 25 个码
    /// **每个都是二选一**，而本模块的比较链不含 weight——词库靠 9999/9998 表达的钦定次序
    /// 在这里完全失效，「戈」被误选一次即永久翻转（码表侧 used-first 不衰减）。
    ///
    /// 必须与 `full_code_still_reranks_freely` 合看：只有这一条时，一个「全局硬保护」的
    /// 错误实现同样会绿，证明不了分级真的分了。
    #[test]
    fn short_code_len1_protects_dict_head() {
        let mut cands = vec![ct_exact("a", "工", 9999), ct_exact("a", "戈", 9998)];
        let r = recs(&[("戈", 5, NOW)]); // 用户选过 5 次「戈」
        rerank_codetable_usedfirst(
            &mut cands,
            &r,
            "a",
            FreqStrategy::Step,
            ProtectPolicy::default(),
        );
        assert_eq!(
            cands[0].text,
            "工",
            "一简位钦定首选须恒居首，实际: {:?}",
            cands.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
        assert_eq!(cands[1].text, "戈", "被保护的只是位次，戈仍在列表里");
    }

    /// **对照组**：同一组数据放到全码位（码长 4）→ 词频照常生效。
    /// 这一条锁住「分级」本身：若把简码保护做成全局硬保护，本用例会红。
    #[test]
    fn full_code_still_reranks_freely() {
        let mut cands = vec![ct_exact("aaaa", "甲", 9999), ct_exact("aaaa", "乙", 9998)];
        let r = recs(&[("乙", 5, NOW)]);
        rerank_codetable_usedfirst(
            &mut cands,
            &r,
            "aaaa",
            FreqStrategy::Step,
            ProtectPolicy::default(),
        );
        assert_eq!(
            cands[0].text,
            "乙",
            "全码位不设保护，用过的候选须正常上浮，实际: {:?}",
            cands.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    /// 二简位同样受保护，且 `Top`（MRU）策略下结论一致——MRU 在简码位危害最大（选一次到顶）。
    #[test]
    fn short_code_len2_protects_under_mru() {
        let mut cands = vec![ct_exact("aa", "式", 9950), ct_exact("aa", "戒", 9949)];
        let r = recs(&[("戒", 1, NOW)]); // MRU 下一次即可到顶
        rerank_codetable_usedfirst(
            &mut cands,
            &r,
            "aa",
            FreqStrategy::Top,
            ProtectPolicy::default(),
        );
        assert_eq!(cands[0].text, "式", "二简位在 MRU 策略下同样须保住钦定首选");
    }

    /// 保护名额**只在精确档内取**：名额多于精确候选时，多出来的不落到前缀补全头上。
    ///
    /// 现场：打 `a`，精确档只有一简字「工」，其后全是 `a*` 的前缀补全。若名额按位置无差别
    /// 取（旧实现），第 2 个名额会把补全词「工艺」钉死在第 2 位——它不是任何码位的钦定首选，
    /// 没有被保护的语义依据，且会挡住用户真正常用的「工区」。
    #[test]
    fn protect_slots_taken_from_exact_only() {
        let mut cands = vec![
            ct_exact("a", "工", 9999),
            ct("aaan", "工艺", 1717),
            ct("aaaq", "工区", 737),
        ];
        let r = recs(&[("工区", 5, NOW)]);
        rerank_codetable_usedfirst(
            &mut cands,
            &r,
            "a",
            FreqStrategy::Step,
            ProtectPolicy {
                by_len: [2, 0, 0], // 名额 2 > 精确候选数 1
                fallback: 0,
            },
        );
        let order: Vec<&str> = cands.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            order,
            vec!["工", "工区", "工艺"],
            "名额只在精确档内取：工受保护，补全档内部照常按词频重排"
        );
    }

    /// 该码位没有精确候选（打了词库里没有的码，候选全是前缀补全）→ 保护集为空。
    /// 没有钦定首选可言，不该平白钉死一个补全词。
    #[test]
    fn no_exact_candidate_means_no_protection() {
        let mut cands = vec![ct("aaan", "工艺", 1717), ct("aaaq", "工区", 737)];
        let r = recs(&[("工区", 5, NOW)]);
        rerank_codetable_usedfirst(
            &mut cands,
            &r,
            "a",
            FreqStrategy::Step,
            ProtectPolicy {
                by_len: [1, 0, 0],
                fallback: 0,
            },
        );
        assert_eq!(cands[0].text, "工区", "无精确候选时不设保护，词频照常生效");
    }

    /// 空策略退化为「无保护」，与分级引入前逐字节一致。
    #[test]
    fn none_policy_degrades_to_no_protection() {
        let mut cands = vec![ct_exact("a", "工", 9999), ct_exact("a", "戈", 9998)];
        let r = recs(&[("戈", 5, NOW)]);
        rerank_codetable_usedfirst(&mut cands, &r, "a", FreqStrategy::Step, ProtectPolicy::NONE);
        assert_eq!(cands[0].text, "戈", "NONE 策略下词频照常生效");
    }

    /// freq_tier 短语再分档：精确码短语（is_exact_code）tier 1 紧随码表精确；前缀短语
    /// （!is_exact_code）tier 2 与码表前缀补全同档，**不因 is_phrase 抬到补全之上**。
    ///
    /// 回归 `da`→`date` 现场：混输下 `date` 前缀短语曾只因 is_phrase 拿 tier 1、压过码表
    /// 前缀补全（如 矼/509000）。入参按 `candidate_display_order` 的输出序喂入（精确码 →
    /// 精确码短语 → 码表前缀补全 → 前缀短语），验证 rerank 维持该档位结构而非把短语顶起。
    /// 旧码（`is_phrase => 1`）下前缀短语会跳到 tier 1、排到 矼 之前 → 本用例会红。
    #[test]
    fn codetable_tier_prefix_phrase_stays_with_completion() {
        let exact_code = ct("da", "左", 3000); // 码表精确全码 tier 0
        let exact_phrase = {
            let mut c = ct("", "精确短语", 10); // lookup 精确码短语 tier 1
            c.is_phrase = true;
            c.is_exact_code = true;
            c
        };
        let completion = ct("dax", "矼", 509_000); // 码表前缀补全 tier 2
        let prefix_phrase = {
            let mut c = ct("", "date短语", 10); // lookup_prefix 前缀短语 tier 2
            c.is_phrase = true;
            c.is_prefix = true; // is_exact_code 默认 false
            c
        };
        let mut cands = vec![exact_code, exact_phrase, completion, prefix_phrase];
        let r = recs(&[]); // 无词频记录 → 纯按档位 + 稳定序（维持入参显示序）
        rerank_codetable_usedfirst(
            &mut cands,
            &r,
            "da",
            FreqStrategy::Step,
            ProtectPolicy::NONE,
        );
        let order: Vec<&str> = cands.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            order,
            vec!["左", "精确短语", "矼", "date短语"],
            "精确码短语 tier1 紧随码表精确；前缀短语 tier2 与码表前缀补全同档、不抬到补全之上"
        );
    }

    // ─────────────── 位置提升（docs/design/freq-rerank-model.md）───────────────
    //
    // 下列常量是 `build_dev/data/schemas/pinyin/cn_dicts/` 的实测权重，用来构造**真实的
    // 位次关系**——位置模型本身不看权重，但入参顺序必须与生产一致（按显示序排好）。

    /// `de` 音单字实测：的 > 得 > 地 > 德，「的」是第二名的 486 倍。
    /// 位置模型下这个悬殊比例**不影响**提升速度，这正是它取代权重模型的理由。
    const W_DE: i32 = 15_378_475;
    const W_DEI: i32 = 31_646;
    const W_DI: i32 = 13_039;
    /// `si yuan` 实测：寺院 491 > 思源 245，仅 2 倍。
    const W_SIYUAN_TEMPLE: i32 = 491;
    const W_SIYUAN_PRODUCT: i32 = 245;

    /// contested 整句：weight 是真机实测的整句量级（3e7 + log_offset）。
    /// 位置模型不看这个数值，它只决定 base_pos——整句因 3e7 天然占据第 0 位。
    fn pin_contested(text: &str) -> Candidate {
        let mut c = pin(text, 29_984_561);
        c.is_sentence = true;
        c.is_sentence_contested = true;
        c
    }

    /// **真机回归**：`count=1` 的记录在**经过一段时间后**仍须生效。
    ///
    /// 这是真机上「选了『思源』，再打 siyuan 还是老样子」的根因用例。此前
    /// `promotion_power` 写作 `(count * decay) as u32`，而 `as` 向零截断——选完仅仅
    /// 1 分钟，decay 就掉到 0.99984，截断即得 0，提升完全消失。
    ///
    /// ⚠️ **本用例的关键在于 `last_used != now`**。全部旧用例都用 `now == last_used`
    /// （decay 恰为 1.0）喂入，正好落在唯一能通过的点上，32 项全绿却漏掉了这个缺陷。
    /// 涉及衰减的用例**必须让时间真的流逝**。
    #[test]
    fn single_use_still_promotes_after_realistic_delay() {
        for (label, secs) in [("1 分钟", 60i64), ("1 小时", 3600), ("1 天", 86_400)] {
            let mut c = vec![pin("寺院", W_SIYUAN_TEMPLE), pin("思源", W_SIYUAN_PRODUCT)];
            let r = recs(&[("思源", 1, NOW - secs)]);
            rerank_pinyin_positional(&mut c, &r, NOW, FreqProfile::default());
            assert_eq!(
                c[0].text, "思源",
                "选过一次、{label}后再打，仍应居首（decay 略小于 1 不得被截断成 0）"
            );
        }
    }

    /// 配对：衰减**够久**之后必须失效，否则上面那条可以靠「永不衰减」满足。
    ///
    /// 半衰期 72h，`MIN_PROMOTION_POWER = 0.5` ⇒ `count=1` 的记录约一个半衰期后归零。
    #[test]
    fn single_use_expires_after_long_disuse() {
        let mut c = vec![pin("寺院", W_SIYUAN_TEMPLE), pin("思源", W_SIYUAN_PRODUCT)];
        // 30 天 ≈ 10 个半衰期，decay ≈ 0.001
        let r = recs(&[("思源", 1, NOW - 30 * 86_400)]);
        rerank_pinyin_positional(&mut c, &r, NOW, FreqProfile::default());
        assert_eq!(
            c[0].text, "寺院",
            "一个月前用过一次不应再影响排序——否则位次的 floor 会让任何残余强度都置顶第 2 位"
        );
    }

    /// 高频记录的衰减边界——**含一条已知局限**。
    ///
    /// 半衰期 72h：一周（≈2.3 个半衰期）后 `count=50` 的有效强度仍有 9.9，正常生效。
    /// 但 30 天（≈10 个半衰期）后衰减到 0.049，低于 `MIN_PROMOTION_POWER` 而归零——
    /// **用过 50 次的词，一个月不用就被完全遗忘**。
    ///
    /// ⚠️ 这是**墙钟衰减的固有行为**，不是缺陷，但也确实反直觉。fcitx5 用「被后续输入挤出
    /// 分级 LRU 池」而非墙钟老化，正是为了避免它（放假两周回来词频不失效）。换老化机制
    /// 需要改词频存储模型，已列为独立立项（设计文档「不在本次范围」一节）。
    ///
    /// 本用例把当前行为钉死：换机制时它会红，那时应连同本注释一起更新。
    #[test]
    fn frequent_use_decay_boundary_including_known_limitation() {
        // 一周后仍生效：50 × decay(168h)=0.198 → 9.9
        let mut week = vec![pin("寺院", W_SIYUAN_TEMPLE), pin("思源", W_SIYUAN_PRODUCT)];
        let r_week = recs(&[("思源", 50, NOW - 7 * 86_400)]);
        rerank_pinyin_positional(&mut week, &r_week, NOW, FreqProfile::default());
        assert_eq!(week[0].text, "思源", "用过 50 次的词一周后仍应保持提升");

        // 30 天后归零 —— 已知局限
        let mut month = vec![pin("寺院", W_SIYUAN_TEMPLE), pin("思源", W_SIYUAN_PRODUCT)];
        let r_month = recs(&[("思源", 50, NOW - 30 * 86_400)]);
        rerank_pinyin_positional(&mut month, &r_month, NOW, FreqProfile::default());
        assert_eq!(
            month[0].text, "寺院",
            "墙钟衰减下高频词一个月后也会被遗忘——已知局限，换分级 LRU 老化可解"
        );
    }

    /// **模型的核心断言**：提升速度与权重差距无关。
    ///
    /// `de` 下「的」是第二名的 486 倍，`siyuan` 下「寺院」只是「思源」的 2 倍——两处的
    /// 第 2 位候选都应**用一次到首位**。前三版权重模型正是死在这里：同一套参数在 486 倍
    /// 的落差下慢得离谱、在 2 倍的落差下快得离谱。
    #[test]
    fn promotion_speed_is_independent_of_weight_gap() {
        // ① 486 倍落差
        let mut de = vec![pin("的", W_DE), pin("得", W_DEI), pin("地", W_DI)];
        let r = recs(&[("得", 1, NOW)]);
        rerank_pinyin_positional(&mut de, &r, NOW, FreqProfile::default());
        assert_eq!(de[0].text, "得", "第 2 位用一次即到首位（486 倍落差）");

        // ② 2 倍落差——同样一次
        let mut sy = vec![pin("寺院", W_SIYUAN_TEMPLE), pin("思源", W_SIYUAN_PRODUCT)];
        let r2 = recs(&[("思源", 1, NOW)]);
        rerank_pinyin_positional(&mut sy, &r2, NOW, FreqProfile::default());
        assert_eq!(sy[0].text, "思源", "第 2 位用一次即到首位（2 倍落差）");
    }

    /// 位次逐次减半：第 8 位 → 4 → 2 → 1 → 0。
    ///
    /// 对应实测的微软行为「1 次提前一半、2 次进第 2 位、再 1 次到首位」。
    #[test]
    fn position_halves_on_each_use() {
        let names = ["a", "b", "c", "d", "e", "f", "g", "h", "i"];
        // 权重递减，保证入参即显示序；目标候选 "i" 在第 8 位（idx 8）
        let build = || -> Vec<Candidate> {
            names
                .iter()
                .enumerate()
                .map(|(i, t)| pin(t, 1000 - i as i32))
                .collect()
        };
        for (count, want) in [(1u32, 4usize), (2, 2), (3, 1), (4, 0)] {
            let mut c = build();
            let r = recs(&[("i", count, NOW)]);
            rerank_pinyin_positional(&mut c, &r, NOW, FreqProfile::default());
            let got = c.iter().position(|x| x.text == "i").unwrap();
            assert_eq!(got, want, "用 {count} 次后应在第 {want} 位，实际 {got}");
        }
    }

    /// **反向对照**：没有词频记录就一步都不动。
    ///
    /// 缺了它，「无条件把某些候选前移」也能让上面两条通过。
    #[test]
    fn no_record_means_no_movement() {
        let mut c = vec![pin("的", W_DE), pin("得", W_DEI), pin("地", W_DI)];
        let before: Vec<String> = c.iter().map(|x| x.text.clone()).collect();
        rerank_pinyin_positional(&mut c, &recs(&[]), NOW, FreqProfile::default());
        let after: Vec<String> = c.iter().map(|x| x.text.clone()).collect();
        assert_eq!(before, after, "无词频记录时顺序必须原样保持");
    }

    /// contested 整句可被同码竞争者靠位置提升反超（`siyuan` 寺院/思源）。
    ///
    /// 位置模型下**不需要 `dict_weight`**：整句的 3e7 只决定它的 `base_pos=0`，
    /// 竞争者提升到同一目标位次后靠「被提升者在前」胜出。
    #[test]
    fn contested_sentence_can_be_overtaken_by_promoted_peer() {
        let mut c = vec![pin_contested("寺院"), pin("思源", W_SIYUAN_PRODUCT)];
        let r = recs(&[("思源", 1, NOW)]);
        rerank_pinyin_positional(&mut c, &r, NOW, FreqProfile::default());
        assert_eq!(c[0].text, "思源", "选过一次的同码词应反超 contested 整句");
        assert_eq!(c[1].text, "寺院", "整句退居第二，不是被赶出列表");
    }

    /// **反向对照**：无词频记录时 contested 整句仍居首，不得平白掉位。
    #[test]
    fn contested_sentence_still_leads_without_record() {
        let mut c = vec![pin_contested("寺院"), pin("思源", W_SIYUAN_PRODUCT)];
        rerank_pinyin_positional(&mut c, &recs(&[]), NOW, FreqProfile::default());
        assert_eq!(c[0].text, "寺院", "无词频记录时整句解仍是最优解读");
    }

    /// 锚定候选**不参与位置提升**：用再多次也不会把整句挤下去。
    ///
    /// 与 contested 的区别正在于此——后者被摘掉锚定才下场竞争。
    #[test]
    fn anchored_sentence_is_immune_to_promotion() {
        let mut c = vec![pin_sentence("整句解", 30_000_000), pin("普通词", 5000)];
        let r = recs(&[("普通词", 50, NOW)]);
        rerank_pinyin_positional(&mut c, &r, NOW, FreqProfile::default());
        assert_eq!(c[0].text, "整句解", "锚定整句不受位置提升影响");
    }

    /// 衰减：久未用则有效次数归零，位次回落原处。
    #[test]
    fn promotion_decays_with_time() {
        let long_ago = NOW - 365 * 24 * 3600;
        let mut c = vec![pin("的", W_DE), pin("得", W_DEI), pin("地", W_DI)];
        let r = recs(&[("得", 1, long_ago)]);
        rerank_pinyin_positional(&mut c, &r, NOW, FreqProfile::default());
        assert_eq!(c[0].text, "的", "一年前用过一次 → 衰减殆尽 → 不再提升");

        // 对照：同样的记录、刚用过则生效——否则上面可能只是「提升从未工作」
        let mut fresh = vec![pin("的", W_DE), pin("得", W_DEI), pin("地", W_DI)];
        let r2 = recs(&[("得", 1, NOW)]);
        rerank_pinyin_positional(&mut fresh, &r2, NOW, FreqProfile::default());
        assert_eq!(fresh[0].text, "得", "刚用过必须生效");
    }

    /// 位置提升**不得跨匹配层级**：前缀补全再常用也压不过精确候选。
    ///
    /// ⚠️ 必须同时钉住「层内提升确实生效」——只断言「补全没上来」的话，
    /// 「提升机制整个失效」也能让它通过。反向验证（令 `promotion_steps` 恒 0）
    /// 当场抓到过这一点：其余 8 条全红，唯独本条还绿着。
    #[test]
    fn promotion_does_not_cross_match_layers() {
        let completion = |t: &str, w: i32| {
            let mut c = pin(t, w);
            c.is_prefix = true;
            c
        };
        // 入参顺序即显示序：精确「四」在前，两条补全按权重降序
        let mut c = vec![
            pin("四", 100),
            completion("思考", 900_000),
            completion("思路", 800_000),
        ];
        let r = recs(&[("思路", 50, NOW)]);
        rerank_pinyin_positional(&mut c, &r, NOW, FreqProfile::default());
        assert_eq!(c[0].text, "四", "补全用 50 次也不得跨层压过精确候选");
        assert_eq!(
            c[1].text, "思路",
            "但层内提升必须生效——否则本用例测的是「提升从未工作」"
        );
    }
}
