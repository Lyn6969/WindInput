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
/// ① 整句/短语豁免：`is_sentence`（Viterbi 整句/超长词典整词）或 `is_phrase`（自定义短语）
///    的候选恒锚定顶部，互相维持引擎权重序（稳定排序）。② 非整句：衰减分 ≥ ε 的"近用"候选软置前于其余，按分降序。
/// ③ 阈值褪色：衰减分 < ε → 失去 used-first 资格，落回引擎权重序。
/// `now` 为当前 unix 秒（由调用方注入，便于测试与确定性）。
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
        // ① 整句/短语锚定顶部（按来源语义判定，不看权重数值——见 Candidate::is_sentence）
        let sa = a.is_sentence || a.is_phrase;
        let sb = b.is_sentence || b.is_phrase;
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

    /// 短语（is_phrase）与整句同享锚定豁免。
    #[test]
    fn pinyin_phrase_is_anchored_like_sentence() {
        let mut cands = vec![pin("普通词", 5000), {
            let mut c = pin("我的邮箱", 40_000_000);
            c.is_phrase = true;
            c
        }];
        let r = recs(&[("普通词", 30, NOW)]);
        rerank_pinyin_decay(&mut cands, &r, NOW, FreqProfile::default());
        assert_eq!(cands[0].text, "我的邮箱", "短语须与整句同享锚定");
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
}
