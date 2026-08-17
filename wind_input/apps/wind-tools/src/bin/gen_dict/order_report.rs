//! 候选顺序变化报告：上游原序 vs 产物最终序。
//!
//! 回答一个别处答不了的问题——**我们的赋权把上游的候选安排改成了什么样**。
//!
//! 其余三份报告都只看产物内部（谁被过滤、哪些简码冲突、哪些够格降权），唯独
//! 「上游本来怎么排」在赋权阶段就被覆盖掉了（`Entry.weight` 一字两用，见 entry.rs）。
//! 快照必须在**过滤与赋权之前**拍下，事后无从重建。
//!
//! ## 为什么按「上游是否表过态」分档
//!
//! 上游 `weight` 列是极点作者的**码位优先级**（10/20/30…60），不是词频。同一个码里
//! 优先级不同 ⇒ 上游明确安排了次序；全组同为最低档 10 ⇒ 上游没表态，此时产物换首选
//! 不算破坏。把两者混在一起统计，会让「1659 个首选变了」这个数字失去判读价值。

use crate::config::Config;
use crate::entry::Entry;
use crate::weight::Unigram;
use std::collections::{BTreeMap, BTreeSet};

/// 上游候选序快照：`code → [(text, 原始优先级)]`，已按上游显示序排好。
///
/// 上游头部声明 `sort: by_weight`，故显示序 = 优先级降序、并列时文件序升序。
pub struct Snapshot {
    groups: BTreeMap<String, Vec<(String, i64)>>,
}

impl Snapshot {
    /// 从**过滤前**的 jidian 条目建快照。
    ///
    /// 用过滤后的条目建会丢掉「上游首选被我们过滤掉了」这一类变化——那正是最该被
    /// 看见的一类（首选凭空换人，且原因不在赋权里）。
    pub fn capture(entries: &[Entry]) -> Self {
        let mut groups: BTreeMap<String, Vec<(String, i64, usize)>> = BTreeMap::new();
        for e in entries {
            groups
                .entry(e.code.clone())
                .or_default()
                .push((e.text.clone(), e.weight, e.orig_pos));
        }
        let groups = groups
            .into_iter()
            .map(|(code, mut list)| {
                list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
                (code, list.into_iter().map(|(t, w, _)| (t, w)).collect())
            })
            .collect();
        Self { groups }
    }

    /// 某个码的上游序：`[(text, 原始优先级)]`，已按上游显示序排好。
    ///
    /// 供 [`crate::upstream_order`] 复用——上游序只解析一次，重排与报告共用同一份，
    /// 两处各解析一遍必然在某次改动后分叉（报告说回归了、产物其实没回归）。
    pub fn group(&self, code: &str) -> Option<&[(String, i64)]> {
        self.groups.get(code).map(|v| v.as_slice())
    }
}

/// 一个码的顺序变化。
pub struct Change {
    pub code: String,
    /// 是否换了首选（false = 仅次序变动，首选未变）
    pub top_changed: bool,
    /// 上游是否对这一变化表过态（首选与新首选的原始优先级不同）
    pub upstream_had_opinion: bool,
    pub cause: &'static str,
    pub up_top: String,
    pub up_top_priority: i64,
    pub gen_top: String,
    pub gen_top_priority: i64,
    /// 产物首选的最终权重
    pub gen_top_weight: i64,
    pub up_top_freq: i64,
    pub gen_top_freq: i64,
    /// 参与比较的候选条数（双方共有）
    pub count: usize,
    /// 上游序，`文本(原始优先级)` 形式，最多 8 条
    pub up_order: Vec<String>,
    /// 产物序，`文本(最终权重)` 形式，最多 8 条
    pub gen_order: Vec<String>,
}

/// 报告的汇总计数，供 stderr 摘要与回归观测。
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Summary {
    /// 双方均有 ≥2 个共存候选、值得比较的码
    pub comparable: usize,
    pub order_changed: usize,
    pub top_changed: usize,
    /// 首选变化里，上游明确表过态的（原始优先级不同）
    pub top_changed_against_upstream: usize,
}

const MAX_LISTED: usize = 8;

/// 对比上游快照与最终产物，产出顺序变化清单。
///
/// `demoted` 是 [`crate::shortcode::apply_demotion`] 实际降权的 `(code, text)`，
/// `boosted` 是 boost 表命中的 `(code, text)`——两者都是**观测值而非推断值**。
/// 成因若靠权重形状反推（比如「差 1 就是降权」），会把恰好差 1 的词频结果也算进去。
pub fn diff(
    snapshot: &Snapshot,
    entries: &[Entry],
    unigram: &Unigram,
    cfg: &Config,
    demoted: &BTreeSet<(String, String)>,
    boosted: &BTreeSet<(String, String)>,
) -> (Vec<Change>, Summary) {
    // 产物侧分组：同码按最终权重降序，与 write_main_dict 的写出序一致
    // （`gen` 是 Rust 2024 的保留关键字，不能拿来做变量名）
    let mut produced: BTreeMap<&str, Vec<(&str, i64)>> = BTreeMap::new();
    for e in entries {
        produced
            .entry(&e.code)
            .or_default()
            .push((&e.text, e.weight));
    }
    for list in produced.values_mut() {
        list.sort_by_key(|e| std::cmp::Reverse(e.1));
    }

    let mut summary = Summary::default();
    let mut changes = Vec::new();

    for (code, up_list) in &snapshot.groups {
        let Some(gen_list) = produced.get(code.as_str()) else {
            continue; // 整码被过滤，属 .filtered.tsv 的辖区
        };
        let gen_texts: BTreeSet<&str> = gen_list.iter().map(|(t, _)| *t).collect();
        let up_texts: BTreeSet<&str> = up_list.iter().map(|(t, _)| t.as_str()).collect();

        // 只比双方共有的条目：新增（custom_words）与被过滤的不构成「顺序变化」，
        // 但它们**顶掉首选**的情形要单独认出来，故下面仍看完整的产物首选。
        let up_common: Vec<&(String, i64)> = up_list
            .iter()
            .filter(|(t, _)| gen_texts.contains(t.as_str()))
            .collect();
        let gen_common: Vec<&(&str, i64)> = gen_list
            .iter()
            .filter(|(t, _)| up_texts.contains(t))
            .collect();
        if up_common.len() < 2 {
            continue;
        }
        summary.comparable += 1;

        let same_order = up_common
            .iter()
            .zip(gen_common.iter())
            .all(|(a, b)| a.0 == b.0);
        if same_order && up_list[0].0 == gen_list[0].0 {
            continue;
        }
        summary.order_changed += 1;

        let up_top = &up_list[0].0;
        let up_top_priority = up_list[0].1;
        let (gen_top, gen_top_weight) = gen_list[0];
        let top_changed = up_top != gen_top;
        if top_changed {
            summary.top_changed += 1;
        }

        // 新首选在上游的优先级；不在上游 = 我们新增的词条
        let gen_top_priority = up_list
            .iter()
            .find(|(t, _)| t == gen_top)
            .map(|(_, w)| *w)
            .unwrap_or(-1);
        let upstream_had_opinion = top_changed && gen_top_priority != up_top_priority;
        if upstream_had_opinion {
            summary.top_changed_against_upstream += 1;
        }

        let cause = classify(
            code,
            up_top,
            gen_top,
            gen_top_priority,
            gen_top_weight,
            upstream_had_opinion,
            cfg,
            demoted,
            boosted,
            &gen_texts,
        );

        changes.push(Change {
            code: code.clone(),
            top_changed,
            upstream_had_opinion,
            cause,
            up_top: up_top.clone(),
            up_top_priority,
            gen_top: gen_top.to_string(),
            gen_top_priority,
            gen_top_weight,
            up_top_freq: unigram.get(up_top).copied().unwrap_or(0),
            gen_top_freq: unigram.get(gen_top).copied().unwrap_or(0),
            count: up_common.len(),
            up_order: up_list
                .iter()
                .take(MAX_LISTED)
                .map(|(t, w)| format!("{t}({w})"))
                .collect(),
            gen_order: gen_list
                .iter()
                .take(MAX_LISTED)
                .map(|(t, w)| format!("{t}({w})"))
                .collect(),
        });
    }

    // 先按「是否换首选」再按「是否违逆上游」分组，组内按新首选词频降序——
    // 越常用的码越先被打到，人工复核的收益也就越高。
    changes.sort_by(|a, b| {
        b.top_changed
            .cmp(&a.top_changed)
            .then_with(|| b.upstream_had_opinion.cmp(&a.upstream_had_opinion))
            .then_with(|| b.gen_top_freq.cmp(&a.gen_top_freq))
            .then_with(|| a.code.cmp(&b.code))
    });
    (changes, summary)
}

#[allow(clippy::too_many_arguments)]
fn classify(
    code: &str,
    up_top: &str,
    gen_top: &str,
    gen_top_priority: i64,
    gen_top_weight: i64,
    upstream_had_opinion: bool,
    cfg: &Config,
    demoted: &BTreeSet<(String, String)>,
    boosted: &BTreeSet<(String, String)>,
    gen_texts: &BTreeSet<&str>,
) -> &'static str {
    // 顺序从「最具体的已知动作」到「兜底」：一个码可能同时满足多条，
    // 先报告人工/规则动作，因为那是可以直接去改的地方。
    if !gen_texts.contains(up_top) {
        return "上游首选被过滤";
    }
    if boosted.contains(&(code.to_string(), gen_top.to_string()))
        || boosted.contains(&(code.to_string(), up_top.to_string()))
    {
        return "词序提升表";
    }
    if demoted.contains(&(code.to_string(), up_top.to_string())) {
        return "简码降权";
    }
    if gen_top_priority < 0 {
        return "自定义新增词";
    }
    if cfg.is_protected_code(code) {
        // 保护码只在上游**表过态**时才必须逐条照抄。上游整组同为最低档时它没表态，
        // `apply_protected_codes` 就是要用词频拆并列（`qqqq` 的「金 > 狗狗」正是此意，
        // 上游「无 weight 列」与「显式 10」解析后同形，只靠文件序会让狗狗占首选）。
        // 把这两种情况都叫「异常」，真异常就淹没在设计意图里了。
        return if upstream_had_opinion {
            "保护码异常"
        } else {
            "保护码并列裁决"
        };
    }
    if cfg.shortcodes.enabled && gen_top_weight >= cfg.shortcodes.level3_base_weight {
        return "简码带";
    }
    "词频补权"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            jidian_path: "a".into(),
            unigram_path: "b".into(),
            output_path: "c".into(),
            ..Default::default()
        }
    }

    fn e(text: &str, code: &str, weight: i64, pos: usize) -> Entry {
        Entry::new(text.into(), code.into(), weight, pos)
    }

    fn empty() -> BTreeSet<(String, String)> {
        BTreeSet::new()
    }

    /// ★ 本报告存在的理由：上游用优先级明确表过态，补权把它翻了过来。
    /// 现场取自实际产物 `tgyn`（上游 重启 20 > 生词 10，补权后 生词 1497 > 重启 1479）。
    #[test]
    fn flags_upstream_priority_being_overturned() {
        let upstream = vec![e("重启", "tgyn", 20, 0), e("生词", "tgyn", 10, 1)];
        let snap = Snapshot::capture(&upstream);
        let produced = vec![e("生词", "tgyn", 1497, 1), e("重启", "tgyn", 1479, 0)];
        let (changes, sum) = diff(
            &snap,
            &produced,
            &Unigram::new(),
            &cfg(),
            &empty(),
            &empty(),
        );
        assert_eq!(changes.len(), 1);
        assert!(changes[0].top_changed);
        assert!(changes[0].upstream_had_opinion, "20 vs 10 = 上游表过态");
        assert_eq!(changes[0].cause, "词频补权");
        assert_eq!(sum.top_changed_against_upstream, 1);
    }

    /// 上游整组同为最低档时它没表态，换首选不该被计入「违逆上游」。
    /// 这条区分是整份报告的判读基准——混在一起数，1659 这个数字就没有意义。
    #[test]
    fn same_priority_group_is_not_counted_as_against_upstream() {
        let upstream = vec![e("甲", "abcd", 10, 0), e("乙", "abcd", 10, 1)];
        let snap = Snapshot::capture(&upstream);
        let produced = vec![e("乙", "abcd", 900, 1), e("甲", "abcd", 500, 0)];
        let (changes, sum) = diff(
            &snap,
            &produced,
            &Unigram::new(),
            &cfg(),
            &empty(),
            &empty(),
        );
        assert_eq!(sum.top_changed, 1);
        assert_eq!(sum.top_changed_against_upstream, 0);
        assert!(!changes[0].upstream_had_opinion);
    }

    #[test]
    fn unchanged_order_produces_no_row() {
        let upstream = vec![e("甲", "abcd", 30, 0), e("乙", "abcd", 10, 1)];
        let snap = Snapshot::capture(&upstream);
        let produced = vec![e("甲", "abcd", 900, 0), e("乙", "abcd", 500, 1)];
        let (changes, sum) = diff(
            &snap,
            &produced,
            &Unigram::new(),
            &cfg(),
            &empty(),
            &empty(),
        );
        assert!(changes.is_empty());
        assert_eq!(sum.comparable, 1);
        assert_eq!(sum.order_changed, 0);
    }

    /// 成因取自实际动作而非权重形状：降权后权重恰好是「第二候选 -1」，
    /// 但词频补权也能凑出同样的差值，靠形状反推会把两者混为一谈。
    #[test]
    fn cause_comes_from_observed_action_not_weight_shape() {
        let upstream = vec![e("中", "khkg", 30, 0), e("口中", "khkg", 10, 1)];
        let snap = Snapshot::capture(&upstream);
        let produced = vec![e("口中", "khkg", 4500, 1), e("中", "khkg", 4499, 0)];

        let (no_hint, _) = diff(
            &snap,
            &produced,
            &Unigram::new(),
            &cfg(),
            &empty(),
            &empty(),
        );
        assert_eq!(no_hint[0].cause, "词频补权", "没有降权记录就不得报成降权");

        let demoted: BTreeSet<(String, String)> = [("khkg".to_string(), "中".to_string())]
            .into_iter()
            .collect();
        let (with_hint, _) = diff(
            &snap,
            &produced,
            &Unigram::new(),
            &cfg(),
            &demoted,
            &empty(),
        );
        assert_eq!(with_hint[0].cause, "简码降权");
    }

    /// 新增词条顶掉上游首选：它不在上游快照里，优先级记 -1 并单列成因。
    #[test]
    fn custom_word_taking_top_is_labelled() {
        let upstream = vec![e("甲", "abcd", 30, 0), e("乙", "abcd", 10, 1)];
        let snap = Snapshot::capture(&upstream);
        let produced = vec![
            e("新词", "abcd", 5000, 9),
            e("甲", "abcd", 900, 0),
            e("乙", "abcd", 500, 1),
        ];
        let (changes, _) = diff(
            &snap,
            &produced,
            &Unigram::new(),
            &cfg(),
            &empty(),
            &empty(),
        );
        assert_eq!(changes[0].cause, "自定义新增词");
        assert_eq!(changes[0].gen_top_priority, -1);
    }

    /// 上游首选被过滤掉时首选必然换人，但那不是赋权造成的，要与词频翻转分开。
    #[test]
    fn filtered_top_is_its_own_cause() {
        let upstream = vec![
            e("弃", "abcd", 30, 0),
            e("甲", "abcd", 10, 1),
            e("乙", "abcd", 10, 2),
        ];
        let snap = Snapshot::capture(&upstream);
        let produced = vec![e("甲", "abcd", 900, 1), e("乙", "abcd", 500, 2)];
        let (changes, _) = diff(
            &snap,
            &produced,
            &Unigram::new(),
            &cfg(),
            &empty(),
            &empty(),
        );
        assert_eq!(changes[0].cause, "上游首选被过滤");
    }

    /// ★ 保护码换首选未必是缺陷：上游整组同为最低档时，`apply_protected_codes` 本就
    /// 用词频拆并列。现场取自实际产物 `qqqq`（上游 狗狗/金 同为 10，产物「金」在前）。
    /// 只有上游用不同优先级表过态却仍被翻转，才是保护失效。
    #[test]
    fn protected_code_tie_break_is_not_an_anomaly() {
        let upstream = vec![e("狗狗", "qqqq", 10, 0), e("金", "qqqq", 10, 1)];
        let snap = Snapshot::capture(&upstream);
        let produced = vec![e("金", "qqqq", 8020, 1), e("狗狗", "qqqq", 8010, 0)];
        let (changes, _) = diff(
            &snap,
            &produced,
            &Unigram::new(),
            &cfg(),
            &empty(),
            &empty(),
        );
        assert_eq!(changes[0].cause, "保护码并列裁决");

        // 上游表过态却被翻 = 保护确实失效，必须叫得出名字
        let upstream2 = vec![e("又", "cccc", 40, 0), e("双双", "cccc", 30, 1)];
        let snap2 = Snapshot::capture(&upstream2);
        let produced2 = vec![e("双双", "cccc", 1319, 1), e("又", "cccc", 1318, 0)];
        let (changes2, _) = diff(
            &snap2,
            &produced2,
            &Unigram::new(),
            &cfg(),
            &empty(),
            &empty(),
        );
        assert_eq!(changes2[0].cause, "保护码异常");
    }

    /// 单候选码没有「顺序」可言，不该占据报告篇幅。
    #[test]
    fn single_candidate_code_is_skipped() {
        let upstream = vec![e("甲", "abcd", 10, 0)];
        let snap = Snapshot::capture(&upstream);
        let produced = vec![e("甲", "abcd", 900, 0)];
        let (changes, sum) = diff(
            &snap,
            &produced,
            &Unigram::new(),
            &cfg(),
            &empty(),
            &empty(),
        );
        assert!(changes.is_empty());
        assert_eq!(sum.comparable, 0);
    }

    /// 快照必须复刻上游 `sort: by_weight` 的显示序：优先级降序，并列时文件序。
    #[test]
    fn snapshot_reproduces_upstream_display_order() {
        let upstream = vec![
            e("丙", "abcd", 10, 0),
            e("甲", "abcd", 30, 1),
            e("乙", "abcd", 10, 2),
        ];
        let snap = Snapshot::capture(&upstream);
        let order: Vec<&str> = snap.groups["abcd"]
            .iter()
            .map(|(t, _)| t.as_str())
            .collect();
        assert_eq!(order, ["甲", "丙", "乙"], "并列时保持文件序");
    }
}
