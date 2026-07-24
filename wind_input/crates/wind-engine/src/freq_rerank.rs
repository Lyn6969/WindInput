//! 词频重排（排序独立维度，frequency.md §3/§4）。
//!
//! **绝不改 weight**：词频是与权重解耦的独立维度，只按 redb 词频记录 `{count, last_used}`
//! 对引擎已排好序的候选做**稳定**重排。
//!
//! ⚠ 「不改 weight」≠「不改顺序」。`rerank_codetable_usedfirst` 的**首要键是 `freq_tier`**，
//! 与协调器 `candidate_display_order` 的匹配层级是两个正交维度——只要存在词频记录，档位序
//! 就整体压过前一步的排序结果（稳定排序只保住档内相对序）。`rerank_pinyin_decay` 则显式
//! 复刻了层级（调用 `cmp_match_layers`）故不会跨层提拔。改本模块前先想清楚要改的是哪一种。
//!
//! 两种语义按引擎类型分流：
//! - 码表/混输（§3）：**永久** used-first——用过的（count>0）档内上浮，count/last_used 不衰减。
//! - 纯拼音（§4）：**衰减软置前**——整句豁免 + 阈值褪色，"用过"随半衰期褪色。
//!
//! 设计归属：frequency.md §5/§7 明确把词频重排放在 engine 排序层（持 store freq 只读访问），
//! 而非 dict 查询层或 coordinator。本模块即该排序层的纯函数实现，由 coordinator 在排序后调用。

use crate::manager::FreqStrategy;
use std::collections::HashMap;
use wind_candidate::Candidate;
use wind_store::freq::{FreqProfile, FreqRecord};

/// 拼音词频衰减分阈值（§4③ 阈值褪色）：衰减分 < ε 的候选失去 used-first 资格，落回引擎权重序
/// （拼音"用过"随半衰期褪色，不同于码表的永久 used-first）。
const PINYIN_FREQ_EPSILON: f64 = 10.0;

/// 候选来源档位（数字越小越靠前）。五笔优先的硬约束：码表精确全码恒在拼音之上，
/// 词频重排只在同档内调整。纯拼音/纯码表模式下同源候选档位相同，退化为按词频排序。
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
        // - 前缀短语（`lookup_prefix` 命中 → `is_exact_code=false`）降到 tier 2，与码表前缀补全同档。
        //   否则混输/拼音下打 `da` 会让 `date` 短语只因 is_phrase 就压过码表前缀补全（如 矼）。
        //   与协调器 `candidate_display_order`（is_exact_code/is_prefix）、混输侧口径对齐。
        return if c.is_exact_code { 1 } else { 2 };
    }
    match c.source {
        CodeTable if c.code == input => 0, // 码表精确全码（如五笔 cang→駏）
        CodeTable => 2,                    // 码表前缀补全
        Pinyin => 3,
        English => 3,
        _ => 2,
    }
}

/// 码表/混输词频重排（§3）：档位感知的**永久** used-first（五笔优先）。
/// 先按来源档位（码表精确 < 精确码短语 < {码表前缀补全, 前缀短语} < 拼音），档内再
/// used-first + 策略排序。前缀短语与码表前缀补全同档＝短语不因 is_phrase 抬到补全之上。
/// 稳定排序保证同档无记录者维持引擎权重序，绝不把拼音浮到五笔精确全码之上。
///
/// 策略：`Step`（默认/逐次提升）count 降序、last_used 降序 tiebreak（抗误选）；
/// `Top`（一次到顶/MRU）last_used 降序、count 降序 tiebreak（最近选的置该档之首）。
pub fn rerank_codetable_usedfirst(
    candidates: &mut [Candidate],
    recs: &HashMap<String, FreqRecord>,
    code: &str,
    strategy: FreqStrategy,
    protect_top_n: usize,
) {
    // 呈现层保护：记录基础序前 N 位，重排后原序回填（不动 weight，见 frequency.md §8）。
    let protected: Vec<String> = candidates
        .iter()
        .take(protect_top_n)
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

/// 拼音词频重排（§4）：衰减软置前 + 整句豁免 + 阈值褪色。
/// 与码表的"永久 used-first"不同——拼音"用过"随半衰期褪色（久未用 → 落回权重序）。
/// ① 整句/短语豁免：`is_sentence`（Viterbi 整句/超长词典整词）或**精确码短语**
///    （`is_phrase && is_exact_code`，即 `lookup` 完全匹配）的候选恒锚定顶部，互相维持引擎权重序
///    （稳定排序）。**前缀短语（`is_phrase && !is_exact_code`）不锚定**——落到下面的匹配层，靠
///    `is_prefix` 降到精确候选之下（与 `freq_tier` 的 tier1/tier2 分档、协调器 `candidate_display_order`
///    同口径：完全匹配才提前、前缀避让）。② 非整句：衰减分 ≥ ε 的"近用"候选软置前于其余，按分降序。
/// ③ 阈值褪色：衰减分 < ε → 失去 used-first 资格，落回引擎权重序。
/// `now` 为当前 unix 秒（由调用方注入，便于测试与确定性）。
///
/// # ⚠️ 隐式契约：本函数**从不比较 weight**
///
/// 所有「维持引擎权重序」的分支返回的都是 `Ordering::Equal`，靠 `sort_by` 的**稳定性**
/// 保住入参既有顺序 —— 权重序不是本函数算出来的，是**调用方喂进来的**。
///
/// 调用点 `handle_candidate.rs:528` 先用 `candidate_display_order`（含权重）排好，`:530`
/// 才调本函数。两行的先后是本函数正确性的前提，不是巧合。
///
/// 由此推出两条，改排序时极易踩：
/// - **调换这两步、或在其间插入任何重排，本函数的输出即失去权重语义**，且不会报错，
///   只会让候选顺序静默发散。
/// - **单测必须按 `candidate_display_order` 的输出顺序喂入**，否则测的是一个生产中
///   不存在的状态（本文件已有一版这样的错误用例，现已订正）。
pub fn rerank_pinyin_decay(
    candidates: &mut [Candidate],
    recs: &HashMap<String, FreqRecord>,
    now: i64,
    profile: FreqProfile,
) {
    use std::cmp::Ordering;
    let score = |c: &Candidate| -> f64 {
        recs.get(&c.text)
            .map(|r| profile.pinyin_score(r, now))
            .unwrap_or(0.0)
    };
    candidates.sort_by(|a, b| {
        // ① 整句/精确码短语锚定顶部（按来源语义判定，不看权重数值——见 Candidate::is_sentence）
        //
        // `is_sentence_demoted` 的整句**不参与**锚定：它已让位于精确整词，若仍锚定，
        // 引擎侧的降权会被本步整个顶回去（本比较器不看 weight，只看标志）。落选锚定后
        // 它走下面的层级+衰减+权重序，恰好停在精确整词之后、普通候选之前。
        //
        // 短语只锚定**精确码短语**（`is_phrase && is_exact_code`）：前缀短语（`lookup_prefix`，
        // `!is_exact_code`）不锚定，落到下面 `cmp_match_layers` 靠 is_prefix 降到精确候选之下。
        // 否则打 `da` 时 `date` 前缀短语只因 is_phrase 就被顶到首位（与 freq_tier tier1/tier2、
        // 协调器 candidate_display_order 对齐：完全匹配才提前、前缀避让）。
        let sa = (a.is_sentence && !a.is_sentence_demoted) || (a.is_phrase && a.is_exact_code);
        let sb = (b.is_sentence && !b.is_sentence_demoted) || (b.is_phrase && b.is_exact_code);
        if sa != sb {
            return if sa {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        if sa {
            return Ordering::Equal; // 均为整句/短语 → 维持引擎权重序
        }
        // ①.5 匹配层级（模糊/前缀补全/子短语）：与引擎、协调器共用同一比较函数。
        //     词频 used-first 不得跨层级提拔——用户曾在 si 下误选模糊「是」，不能让它
        //     永久压过精确「四」；频繁使用的补全「思考」也不能压过精确「四」；baoan 下
        //     常用单字「报」不能压过完整词「报案」。对齐 Go 的 Exact/coverage 硬分层。
        let layers = wind_candidate::cmp_match_layers(a, b);
        if layers != Ordering::Equal {
            return layers;
        }
        // ②③ 非整句、同层级：阈值褪色 + 衰减分降序
        let (pa, pb) = (score(a), score(b));
        let ua = pa >= PINYIN_FREQ_EPSILON;
        let ub = pb >= PINYIN_FREQ_EPSILON;
        if ua != ub {
            return if ua {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        if ua {
            return pb.partial_cmp(&pa).unwrap_or(Ordering::Equal);
        }
        Ordering::Equal // 均褪色/未用 → 维持引擎权重序（稳定排序）
    });
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
        rerank_pinyin_decay(&mut cands, &r, NOW, FreqProfile::default());
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
        rerank_pinyin_decay(&mut cands, &r, NOW, FreqProfile::default());
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
        rerank_pinyin_decay(&mut cands, &r, NOW, FreqProfile::default());
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
        let r = recs(&[("降级整句", 20, NOW)]);
        rerank_pinyin_decay(&mut cands, &r, NOW, FreqProfile::default());
        assert_eq!(
            cands[0].text, "降级整句",
            "降级整句不再锚定，故可凭词频重新浮上来"
        );
    }

    /// 锚定按来源语义而非权重数值：高权重但非整句的候选（如别的功能提权到 30M 的词）
    /// 仍须正常参与词频重排，不得被误当整句锚定而永久失去词频学习能力。
    /// 这正是把 `weight >= 20M` 阈值换成 `is_sentence` 标记要解决的问题。
    #[test]
    fn pinyin_high_weight_non_sentence_still_learns() {
        let mut cands = vec![pin("高权非整句", 30_000_000), pin("近用低权", 100)];
        let r = recs(&[("近用低权", 20, NOW)]);
        rerank_pinyin_decay(&mut cands, &r, NOW, FreqProfile::default());
        assert_eq!(
            cands[0].text, "近用低权",
            "非整句候选无论权重多高都应可被词频重排下沉"
        );
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
        rerank_pinyin_decay(&mut cands, &r, NOW, FreqProfile::default());
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
        rerank_pinyin_decay(&mut cands, &r, NOW, FreqProfile::default());
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
        rerank_pinyin_decay(&mut cands, &r, NOW, FreqProfile::default());
        assert_eq!(cands[0].text, "近用低权", "近期使用应软置前");
    }

    fn pin_fuzzy(text: &str, weight: i32) -> Candidate {
        let mut c = pin(text, weight);
        c.is_fuzzy = true;
        c
    }

    /// 模糊层级优先于词频（完全模拟线上 si→是 词频污染场景）：
    /// 用户曾在 "si" 下选过模糊命中「是」(有使用记录、权重更高)，但精确命中「四」(非模糊、
    /// 未使用、权重更低) 仍须排在「是」之前——词频 used-first 不得把模糊提到精确之上。
    #[test]
    fn pinyin_exact_ranks_above_used_fuzzy() {
        let mut cands = vec![
            pin_fuzzy("是", 9000), // 模糊命中，高权重，且被频繁使用过
            pin("四", 100),        // 精确命中，低权重，未使用
        ];
        let r = recs(&[("是", 4, NOW)]); // 「是」有使用记录（模拟 si→是 count=4）
        rerank_pinyin_decay(&mut cands, &r, NOW, FreqProfile::default());
        assert_eq!(
            cands[0].text,
            "四",
            "精确命中「四」须优先于被使用过的模糊命中「是」，实际: {:?}",
            cands.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
        assert_eq!(cands[1].text, "是");
    }

    /// 阈值褪色：久未用（衰减分 < ε）失去 used-first 资格，落回引擎权重序。
    #[test]
    fn pinyin_faded_use_falls_back_to_weight_order() {
        let long_ago = NOW - 365 * 24 * 3600; // 一年前用过一次 → 衰减远小于 ε
        let mut cands = vec![pin("高权未用", 5000), pin("陈旧低权", 100)];
        let r = recs(&[("陈旧低权", 1, long_ago)]);
        rerank_pinyin_decay(&mut cands, &r, NOW, FreqProfile::default());
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
        rerank_codetable_usedfirst(&mut cands, &r, "aaaa", FreqStrategy::Step, 0);
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
        rerank_codetable_usedfirst(&mut cands, &r, "aaaa", FreqStrategy::Top, 0);
        assert_eq!(cands[0].text, "戈", "top：最近使用者置首");
    }

    /// 五笔优先档位：拼音候选即便高频近用，也不能浮到码表精确全码之上（混输硬约束）。
    #[test]
    fn mixed_tier_keeps_codetable_exact_above_pinyin() {
        let mut cands = vec![ct("aaaa", "工", 100), pin("啊", 5000)];
        // 拼音「啊」高频近用，但档位 3 低于码表精确全码档位 0
        let r = recs(&[("啊", 50, NOW)]);
        rerank_codetable_usedfirst(&mut cands, &r, "aaaa", FreqStrategy::Step, 0);
        assert_eq!(cands[0].text, "工", "码表精确全码档位最高，拼音不得反超");
    }

    /// protect_top_n=1：重排后基础序首位被回填锁定，高词频候选在保护位之后正常上浮。
    #[test]
    fn protect_top_n_pins_original_head() {
        // 基础序：甲(高weight) 乙 丙；"丙"有词频记录本应浮首。
        let mut cands = vec![
            ct("abcd", "甲", 300),
            ct("abcd", "乙", 200),
            ct("abcd", "丙", 100),
        ];
        let mut recs_map = HashMap::new();
        recs_map.insert(
            "丙".to_string(),
            FreqRecord {
                count: 9,
                last_used: 1000,
            },
        );
        rerank_codetable_usedfirst(&mut cands, &recs_map, "abcd", FreqStrategy::Step, 1);
        assert_eq!(cands[0].text, "甲", "protect_top_n=1 应锁定原首位");
        assert_eq!(cands[1].text, "丙", "词频候选在保护位之后正常上浮");
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
        rerank_codetable_usedfirst(&mut cands, &r, "da", FreqStrategy::Step, 0);
        let order: Vec<&str> = cands.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            order,
            vec!["左", "精确短语", "矼", "date短语"],
            "精确码短语 tier1 紧随码表精确；前缀短语 tier2 与码表前缀补全同档、不抬到补全之上"
        );
    }
}
