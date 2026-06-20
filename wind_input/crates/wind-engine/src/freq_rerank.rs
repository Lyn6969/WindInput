//! 词频重排（排序独立维度，frequency.md §3/§4）。
//!
//! **绝不改 weight**：词频是与权重解耦的独立维度，只按 redb 词频记录 `{count, last_used}`
//! 对引擎已排好序的候选做**稳定**重排。两种语义按引擎类型分流：
//! - 码表/混输（§3）：**永久** used-first——用过的（count>0）档内上浮，count/last_used 不衰减。
//! - 纯拼音（§4）：**衰减软置前**——整句豁免 + 阈值褪色，"用过"随半衰期褪色。
//!
//! 设计归属：frequency.md §5/§7 明确把词频重排放在 engine 排序层（持 store freq 只读访问），
//! 而非 dict 查询层或 coordinator。本模块即该排序层的纯函数实现，由 coordinator 在排序后调用。

use crate::manager::FreqStrategy;
use std::collections::HashMap;
use wind_candidate::Candidate;
use wind_store::freq::{FreqProfile, FreqRecord};

/// 拼音整句/短语豁免阈值（§4①）：weight ≥ 此值的候选（整句 SENTENCE_WEIGHT_BASE=30M、
/// 短语 PHRASE_WEIGHT_BASE=40M）视为"引擎最优解"，词频重排恒不下沉。介于词权重上限(~19M)
/// 与整句基准(30M)之间。
const PINYIN_SENTENCE_FLOOR: i32 = 20_000_000;
/// 拼音词频衰减分阈值（§4③ 阈值褪色）：衰减分 < ε 的候选失去 used-first 资格，落回引擎权重序
/// （拼音"用过"随半衰期褪色，不同于码表的永久 used-first）。
const PINYIN_FREQ_EPSILON: f64 = 10.0;

/// 候选来源档位（数字越小越靠前）。五笔优先的硬约束：码表精确全码恒在拼音之上，
/// 词频重排只在同档内调整。纯拼音/纯码表模式下同源候选档位相同，退化为按词频排序。
fn freq_tier(c: &Candidate, input: &str) -> u8 {
    use wind_candidate::CandidateSource::*;
    if c.is_phrase {
        return 1;
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
/// 先按来源档位（码表精确 < 词/短语 < 码表前缀 < 拼音），档内再 used-first + 策略排序。
/// 稳定排序保证同档无记录者维持引擎权重序，绝不把拼音浮到五笔精确全码之上。
///
/// 策略：`Step`（默认/逐次提升）count 降序、last_used 降序 tiebreak（抗误选）；
/// `Top`（一次到顶/MRU）last_used 降序、count 降序 tiebreak（最近选的置该档之首）。
pub fn rerank_codetable_usedfirst(
    candidates: &mut [Candidate],
    recs: &HashMap<String, FreqRecord>,
    code: &str,
    strategy: FreqStrategy,
) {
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
}

/// 拼音词频重排（§4）：衰减软置前 + 整句豁免 + 阈值褪色。
/// 与码表的"永久 used-first"不同——拼音"用过"随半衰期褪色（久未用 → 落回权重序）。
/// ① 整句/短语豁免：weight ≥ PINYIN_SENTENCE_FLOOR 的候选（Viterbi 整句/自定义短语）恒锚定顶部，
///    互相维持引擎权重序（稳定排序）。② 非整句：衰减分 ≥ ε 的"近用"候选软置前于其余，按分降序。
/// ③ 阈值褪色：衰减分 < ε → 失去 used-first 资格，落回引擎权重序。
/// `now` 为当前 unix 秒（由调用方注入，便于测试与确定性）。
pub fn rerank_pinyin_decay(
    candidates: &mut [Candidate],
    recs: &HashMap<String, FreqRecord>,
    now: i64,
) {
    use std::cmp::Ordering;
    let profile = FreqProfile::default();
    let score = |c: &Candidate| -> f64 {
        recs.get(&c.text)
            .map(|r| profile.pinyin_score(r, now))
            .unwrap_or(0.0)
    };
    candidates.sort_by(|a, b| {
        // ① 整句/短语锚定顶部
        let sa = a.weight >= PINYIN_SENTENCE_FLOOR;
        let sb = b.weight >= PINYIN_SENTENCE_FLOOR;
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
        // ②③ 非整句：阈值褪色 + 衰减分降序
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

    /// 整句豁免：高权重整句恒置顶，即使某非整句词被频繁使用也不能反超。
    #[test]
    fn pinyin_sentence_is_anchored_on_top() {
        let mut cands = vec![
            pin("你好世界", 30_000_000),
            pin("你好", 2000),
            pin("拟", 1000),
        ];
        let r = recs(&[("你好", 20, NOW)]);
        rerank_pinyin_decay(&mut cands, &r, NOW);
        assert_eq!(cands[0].text, "你好世界", "整句必须锚定首位");
        assert_eq!(cands[1].text, "你好", "近用词软置前于未用词");
        assert_eq!(cands[2].text, "拟");
    }

    /// 衰减软置前：近用词（衰减分 ≥ ε）浮到未用词之上，即使权重更低。
    #[test]
    fn pinyin_recent_use_floats_above_higher_weight() {
        let mut cands = vec![pin("低频高权", 5000), pin("近用低权", 100)];
        let r = recs(&[("近用低权", 8, NOW)]);
        rerank_pinyin_decay(&mut cands, &r, NOW);
        assert_eq!(cands[0].text, "近用低权", "近期使用应软置前");
    }

    /// 阈值褪色：久未用（衰减分 < ε）失去 used-first 资格，落回引擎权重序。
    #[test]
    fn pinyin_faded_use_falls_back_to_weight_order() {
        let long_ago = NOW - 365 * 24 * 3600; // 一年前用过一次 → 衰减远小于 ε
        let mut cands = vec![pin("高权未用", 5000), pin("陈旧低权", 100)];
        let r = recs(&[("陈旧低权", 1, long_ago)]);
        rerank_pinyin_decay(&mut cands, &r, NOW);
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
        rerank_codetable_usedfirst(&mut cands, &r, "aaaa", FreqStrategy::Step);
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
        rerank_codetable_usedfirst(&mut cands, &r, "aaaa", FreqStrategy::Top);
        assert_eq!(cands[0].text, "戈", "top：最近使用者置首");
    }

    /// 五笔优先档位：拼音候选即便高频近用，也不能浮到码表精确全码之上（混输硬约束）。
    #[test]
    fn mixed_tier_keeps_codetable_exact_above_pinyin() {
        let mut cands = vec![ct("aaaa", "工", 100), pin("啊", 5000)];
        // 拼音「啊」高频近用，但档位 3 低于码表精确全码档位 0
        let r = recs(&[("啊", 50, NOW)]);
        rerank_codetable_usedfirst(&mut cands, &r, "aaaa", FreqStrategy::Step);
        assert_eq!(cands[0].text, "工", "码表精确全码档位最高，拼音不得反超");
    }
}
