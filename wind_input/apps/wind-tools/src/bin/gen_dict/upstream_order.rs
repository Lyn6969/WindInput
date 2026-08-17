//! 组内权重重排：把同码候选的次序换回极点上游序，**不改变该码在权重轴上的高度**。
//!
//! ## 为什么不是「上游优先级分带」
//!
//! 词频承担着两件被混在一起的事：**这组候选在全库中有多重要**（跨码，五笔按前缀匹配，
//! 打 `tg` 会召出 `tgyn`/`tgab`… 全部后代，它们必须在同一根轴上比）与**这组内部谁排第一**
//! （同码）。上游的 `weight` 列只答得了后者——它是码内局部序号（值域 5~890、86% 是 10），
//! `tgyn` 的 20 与 `tgab` 的 20 之间没有任何可比性。
//!
//! 所以本模块不碰权重的**数值**，只改它们在组内的**分配**：
//!
//! ```text
//! tgyn 的词频权重集合 = {1497, 1479}
//! 上游序             = [重启, 生词]
//! 重新分配           → 重启=1497, 生词=1479
//! ```
//!
//! 每个码占据的权重区间一字不变 ⇒ 跨码可比性完全不受影响；词频退回它真正擅长的两件事：
//! 决定该码整体的高度、以及并列时的裁决。
//!
//! ## 为什么需要护栏
//!
//! 上游的排序里混着大量作者当年随手排上去的低频词。纯回归会把它们顶回首选——
//! 干跑实测 202 条是 unigram 未命中的（`川崎` / `SD卡` / `滨州学院` / `不信谣，不传谣`），
//! 另有一批词频低一两个数量级的。**「词频把上游序压掉」这件事同时干了两件事**：破坏了
//! 一批上游的合理安排，也拦住了一批上游的不合理安排。只看被破坏的那一半就会过度修正。

use crate::config::Config;
use crate::entry::Entry;
use crate::order_report::Snapshot;
use crate::weight::Unigram;
use std::collections::BTreeMap;

/// 重排结果计数。
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Stats {
    /// 上游首选与当前首选不同、进入护栏判定的码
    pub examined: usize,
    /// 实际回归上游序的码
    pub reordered: usize,
    /// 被「上游首选 unigram 未命中」拦下
    pub held_unseen: usize,
    /// 被倍数护栏拦下
    pub held_ratio: usize,
}

/// 把同码条目的权重按上游序重新分配。
///
/// 只重排**上游也有的条目**：`custom_words` 注入的新词不在上游序里，其权重原样保留，
/// 最终排序时自然落位——否则新词会被挤进一个由上游序决定的位置，而上游对它没有意见。
///
/// 简码带（`shortcode_level > 0`）与受保护码整组跳过：它们的权重是**档位**不是词频，
/// 参与重排等于把档位打散。
pub fn reapply_upstream_order(
    entries: &mut [Entry],
    snapshot: &Snapshot,
    unigram: &Unigram,
    cfg: &Config,
) -> Stats {
    let mut stats = Stats::default();
    if !cfg.upstream_order.enabled {
        return stats;
    }

    // code → 该码下参与重排的条目下标
    let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, e) in entries.iter().enumerate() {
        if e.shortcode_level > 0 || cfg.is_protected_code(&e.code) {
            continue;
        }
        groups.entry(&e.code).or_default().push(i);
    }
    // 借用检查：分组阶段借了 entries，改权重前先断开
    let groups: Vec<(String, Vec<usize>)> = groups
        .into_iter()
        .map(|(c, v)| (c.to_string(), v))
        .collect();

    for (code, idxs) in groups {
        if idxs.len() < 2 {
            continue;
        }
        let Some(up) = snapshot.group(&code) else {
            continue;
        };

        // 当前序：权重降序。并列时保持 orig_pos 升序，与写出阶段的稳定排序同口径。
        let mut cur = idxs.clone();
        cur.sort_by(|&a, &b| {
            entries[b]
                .weight
                .cmp(&entries[a].weight)
                .then_with(|| entries[a].orig_pos.cmp(&entries[b].orig_pos))
        });

        // 上游序里仍存活于本组的条目（被过滤的、简码带的都已不在 idxs 中）
        let present: BTreeMap<&str, usize> =
            cur.iter().map(|&i| (entries[i].text.as_str(), i)).collect();
        let up_alive: Vec<usize> = up
            .iter()
            .filter_map(|(t, _)| present.get(t.as_str()).copied())
            .collect();
        if up_alive.len() < 2 {
            continue;
        }

        let up_top = &entries[up_alive[0]].text;
        let cur_top = &entries[cur[0]].text;
        if up_top == cur_top {
            continue; // 首选本就一致，组内次序不值得为它翻动
        }
        stats.examined += 1;

        // ── 护栏 ────────────────────────────────────────
        // ① 上游首选不在 unigram 里 ⇒ 它是生僻词，上游把它排第一多半是随手排的
        let f_up = unigram.get(up_top).copied().unwrap_or(0);
        if f_up == 0 {
            stats.held_unseen += 1;
            continue;
        }
        // ② 当前首选的词频高出太多 ⇒ 词频的意见更可信，不回归
        let f_cur = unigram.get(cur_top).copied().unwrap_or(0);
        if f_cur > f_up.saturating_mul(cfg.upstream_order.max_freq_ratio) {
            stats.held_ratio += 1;
            continue;
        }

        // ── 重排：把这些条目**当前持有的权重值**按上游序重新发一遍 ──
        // 取值集合而非重新计算，是「不改变该码在权重轴上的高度」的实现方式。
        let mut pool: Vec<i64> = up_alive.iter().map(|&i| entries[i].weight).collect();
        pool.sort_unstable_by(|a, b| b.cmp(a));
        for (rank, &i) in up_alive.iter().enumerate() {
            entries[i].weight = pool[rank];
        }
        stats.reordered += 1;
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UpstreamOrderConfig;

    fn cfg() -> Config {
        Config {
            jidian_path: "a".into(),
            unigram_path: "b".into(),
            output_path: "c".into(),
            upstream_order: UpstreamOrderConfig {
                enabled: true,
                max_freq_ratio: 26,
            },
            ..Default::default()
        }
    }

    fn e(text: &str, code: &str, weight: i64, pos: usize) -> Entry {
        Entry::new(text.into(), code.into(), weight, pos)
    }

    /// 按最终权重降序取文本，即用户看到的候选顺序。
    fn order(entries: &[Entry]) -> Vec<&str> {
        let mut v: Vec<&Entry> = entries.iter().collect();
        v.sort_by(|a, b| {
            b.weight
                .cmp(&a.weight)
                .then_with(|| a.orig_pos.cmp(&b.orig_pos))
        });
        v.iter().map(|e| e.text.as_str()).collect()
    }

    fn uni(pairs: &[(&str, i64)]) -> Unigram {
        pairs.iter().map(|(t, f)| (t.to_string(), *f)).collect()
    }

    /// ★ 主场景，现场取自实际产物 `tgyn`：上游「重启 20 > 生词 10」被词频补权翻成
    /// 「生词 1497 > 重启 1479」。倍数 1.1 远在护栏内，应回归上游序。
    #[test]
    fn restores_upstream_order_within_guardrail() {
        let upstream = vec![e("重启", "tgyn", 20, 0), e("生词", "tgyn", 10, 1)];
        let snap = Snapshot::capture(&upstream);
        let mut produced = vec![e("生词", "tgyn", 1497, 1), e("重启", "tgyn", 1479, 0)];
        let u = uni(&[("重启", 2476), ("生词", 2717)]);

        let s = reapply_upstream_order(&mut produced, &snap, &u, &cfg());
        assert_eq!(s.reordered, 1);
        assert_eq!(order(&produced), ["重启", "生词"]);
    }

    /// ★★ 权重轴高度守恒：重排只重新分配组内已有的权重值，集合必须一字不变——
    /// 这是「跨码可比性不受影响」的实现保证，也是本方案区别于「优先级分带」的地方。
    #[test]
    fn weight_multiset_is_preserved() {
        let upstream = vec![
            e("丙", "abcd", 30, 0),
            e("甲", "abcd", 20, 1),
            e("乙", "abcd", 10, 2),
        ];
        let snap = Snapshot::capture(&upstream);
        let mut produced = vec![
            e("甲", "abcd", 900, 1),
            e("乙", "abcd", 850, 2),
            e("丙", "abcd", 700, 0),
        ];
        let u = uni(&[("甲", 900), ("乙", 850), ("丙", 700)]);

        reapply_upstream_order(&mut produced, &snap, &u, &cfg());
        let mut ws: Vec<i64> = produced.iter().map(|e| e.weight).collect();
        ws.sort_unstable();
        assert_eq!(ws, vec![700, 850, 900], "权重值集合不得变动");
        assert_eq!(order(&produced), ["丙", "甲", "乙"], "次序回归上游");
    }

    /// 护栏 ①：上游首选是 unigram 未命中的生僻词（`SD卡` 顶掉 `顶上` 那一类），不回归。
    #[test]
    fn holds_when_upstream_top_is_unseen() {
        let upstream = vec![e("川崎", "ktmd", 30, 0), e("噢", "ktmd", 10, 1)];
        let snap = Snapshot::capture(&upstream);
        let mut produced = vec![e("噢", "ktmd", 1500, 1), e("川崎", "ktmd", 180, 0)];
        let u = uni(&[("噢", 4092)]); // 「川崎」未命中

        let s = reapply_upstream_order(&mut produced, &snap, &u, &cfg());
        assert_eq!((s.examined, s.reordered, s.held_unseen), (1, 0, 1));
        assert_eq!(order(&produced), ["噢", "川崎"], "维持词频序");
    }

    /// 护栏 ②：倍数超限不回归。现场取自 `wgbb`「鸽子(1019) ← 例子(29650)」= 29.1 倍。
    #[test]
    fn holds_when_ratio_exceeds_threshold() {
        let upstream = vec![e("鸽子", "wgbb", 40, 0), e("例子", "wgbb", 30, 1)];
        let snap = Snapshot::capture(&upstream);
        let mut produced = vec![e("例子", "wgbb", 1949, 1), e("鸽子", "wgbb", 1311, 0)];
        let u = uni(&[("鸽子", 1019), ("例子", 29650)]);

        let s = reapply_upstream_order(&mut produced, &snap, &u, &cfg());
        assert_eq!((s.examined, s.reordered, s.held_ratio), (1, 0, 1));
        assert_eq!(order(&produced), ["例子", "鸽子"]);
    }

    /// 护栏边界就在阈值上：`sgsv` 25.3 倍回归、`ddkh` 26.2 倍拦下（用户以样本夹定 26）。
    #[test]
    fn threshold_boundary_matches_calibration_samples() {
        // 25.3 倍：352 × 26 = 9152 > 8903 ⇒ 放行
        let up1 = vec![e("梗概", "sgsv", 30, 0), e("要不要", "sgsv", 10, 1)];
        let mut p1 = vec![e("要不要", "sgsv", 1600, 1), e("梗概", "sgsv", 900, 0)];
        let u1 = uni(&[("梗概", 352), ("要不要", 8903)]);
        reapply_upstream_order(&mut p1, &Snapshot::capture(&up1), &u1, &cfg());
        assert_eq!(order(&p1), ["梗概", "要不要"], "25.3 倍应回归");

        // 26.2 倍：475 × 26 = 12350 < 12449 ⇒ 拦下
        let up2 = vec![e("大路", "ddkh", 30, 0), e("套路", "ddkh", 10, 1)];
        let mut p2 = vec![e("套路", "ddkh", 1700, 1), e("大路", "ddkh", 800, 0)];
        let u2 = uni(&[("大路", 475), ("套路", 12449)]);
        reapply_upstream_order(&mut p2, &Snapshot::capture(&up2), &u2, &cfg());
        assert_eq!(order(&p2), ["套路", "大路"], "26.2 倍应拦下");
    }

    /// 简码带整组不参与：其权重是档位（9000+）不是词频，重排会把档位打散。
    #[test]
    fn shortcode_band_is_excluded() {
        let upstream = vec![e("乙", "abc", 30, 0), e("甲", "abc", 10, 1)];
        let snap = Snapshot::capture(&upstream);
        let mut produced = vec![e("甲", "abc", 9000, 1), e("乙", "abc", 8999, 0)];
        produced[0].shortcode_level = 3;
        produced[1].shortcode_level = 3;
        let u = uni(&[("甲", 500), ("乙", 400)]);

        let s = reapply_upstream_order(&mut produced, &snap, &u, &cfg());
        assert_eq!(s.examined, 0, "简码条目不该进入判定");
        assert_eq!(produced[0].weight, 9000);
    }

    /// 受保护码整组不参与：`apply_protected_codes` 已按上游优先级赋过权，
    /// 再重排一次会用「当前权重值集合」覆盖掉保护带的等距档位。
    #[test]
    fn protected_codes_are_excluded() {
        let upstream = vec![e("双双", "cccc", 30, 0), e("又", "cccc", 40, 1)];
        let snap = Snapshot::capture(&upstream);
        let mut produced = vec![e("又", "cccc", 8020, 1), e("双双", "cccc", 8010, 0)];
        let u = uni(&[("又", 1318), ("双双", 500)]);

        let s = reapply_upstream_order(&mut produced, &snap, &u, &cfg());
        assert_eq!(s.examined, 0);
        assert_eq!(produced[0].weight, 8020, "保护带档位不得被覆盖");
    }

    /// 自定义新增词不在上游序里，权重原样保留——上游对它没有意见，
    /// 不该被挤进一个由上游序决定的位置。
    #[test]
    fn custom_word_weight_is_untouched() {
        let upstream = vec![e("丙", "abcd", 30, 0), e("甲", "abcd", 10, 1)];
        let snap = Snapshot::capture(&upstream);
        let mut produced = vec![
            e("新词", "abcd", 5000, 9), // custom_words 注入，上游无此条
            e("甲", "abcd", 900, 1),
            e("丙", "abcd", 700, 0),
        ];
        let u = uni(&[("甲", 900), ("丙", 700), ("新词", 300)]);

        reapply_upstream_order(&mut produced, &snap, &u, &cfg());
        assert_eq!(produced[0].weight, 5000, "新增词权重不得变动");
        assert_eq!(
            order(&produced),
            ["新词", "丙", "甲"],
            "上游条目之间回归上游序"
        );
    }

    /// 首选本就一致时不进入判定——组内后段的次序不值得为它翻动权重。
    #[test]
    fn untouched_when_top_already_matches() {
        let upstream = vec![
            e("甲", "abcd", 30, 0),
            e("乙", "abcd", 20, 1),
            e("丙", "abcd", 10, 2),
        ];
        let snap = Snapshot::capture(&upstream);
        let mut produced = vec![
            e("甲", "abcd", 900, 0),
            e("丙", "abcd", 800, 2),
            e("乙", "abcd", 700, 1),
        ];
        let u = uni(&[("甲", 900), ("乙", 700), ("丙", 800)]);

        let s = reapply_upstream_order(&mut produced, &snap, &u, &cfg());
        assert_eq!(s.examined, 0);
        assert_eq!(produced[1].weight, 800, "权重一动不动");
    }

    /// 开关关闭时整条逻辑不执行。
    #[test]
    fn disabled_switch_is_a_no_op() {
        let upstream = vec![e("重启", "tgyn", 20, 0), e("生词", "tgyn", 10, 1)];
        let snap = Snapshot::capture(&upstream);
        let mut produced = vec![e("生词", "tgyn", 1497, 1), e("重启", "tgyn", 1479, 0)];
        let u = uni(&[("重启", 2476), ("生词", 2717)]);
        let c = Config {
            upstream_order: UpstreamOrderConfig {
                enabled: false,
                max_freq_ratio: 26,
            },
            ..cfg()
        };

        assert_eq!(
            reapply_upstream_order(&mut produced, &snap, &u, &c),
            Stats::default()
        );
        assert_eq!(order(&produced), ["生词", "重启"]);
    }
}
