//! 词频加载与权重归一化。
//!
//! 归一化把 unigram 的原始频次压到 `[weight_min, weight_max]`：以命中条目的**中位频次**
//! 映射到 `target_median`，其余按 log10 等比缩放。用 log10 而非线性是因为词频分布跨越
//! 好几个数量级，线性映射会让绝大多数词挤在底部无法区分。

use crate::config::Config;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

pub type Unigram = HashMap<String, i64>;

/// 加载 unigram.txt：`词语<TAB>频次`，跳过空行与 `#` 注释。
///
/// 频次列容忍浮点写法（截断取整），与 Go 版一致——上游语料换算后偶有小数。
pub fn load_unigram(path: &Path) -> anyhow::Result<Unigram> {
    let f = std::fs::File::open(path)
        .map_err(|e| anyhow::anyhow!("打开 unigram 失败 {}: {e}", path.display()))?;
    let mut freq = HashMap::new();
    for line in BufReader::new(f).lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((text, rest)) = line.split_once('\t') else {
            continue;
        };
        let rest = rest.trim();
        let v = match rest.parse::<i64>() {
            Ok(v) => v,
            Err(_) => match rest.parse::<f64>() {
                Ok(fv) => fv as i64,
                Err(_) => continue,
            },
        };
        if v > 0 {
            freq.insert(text.to_string(), v);
        }
    }
    Ok(freq)
}

/// 命中 unigram 的词条频次中位数；无命中时退回 1000（避免除零）。
pub fn median_raw_freq(entries: &[crate::entry::Entry], unigram: &Unigram) -> f64 {
    let mut freqs: Vec<i64> = entries
        .iter()
        .filter_map(|e| unigram.get(&e.text).copied())
        .collect();
    if freqs.is_empty() {
        return 1000.0;
    }
    freqs.sort_unstable();
    let n = freqs.len();
    if n % 2 == 1 {
        freqs[n / 2] as f64
    } else {
        // 先整数相加再转浮点，与 Go 的 float64(a+b)/2 一致
        (freqs[n / 2 - 1] + freqs[n / 2]) as f64 / 2.0
    }
}

/// 频次 → 权重。`log_median` 为 `log10(中位频次 + 1)`。
pub fn compute_weight(freq: i64, log_median: f64, cfg: &Config) -> i64 {
    if freq <= 0 || log_median == 0.0 {
        return cfg.weight_min;
    }
    let w = cfg.target_median as f64 * ((freq as f64) + 1.0).log10() / log_median;
    clamp_weight(w.round() as i64, cfg)
}

/// unigram 未命中时的保底权重，按 jidian 原始优先级分三档。
pub fn fallback_weight(orig_priority: i64, cfg: &Config) -> i64 {
    if orig_priority >= 30 {
        cfg.fallback.priority_30
    } else if orig_priority >= 20 {
        cfg.fallback.priority_20
    } else {
        cfg.fallback.priority_10
    }
}

pub fn clamp_weight(w: i64, cfg: &Config) -> i64 {
    w.clamp(cfg.weight_min, cfg.weight_max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::Entry;

    fn cfg() -> Config {
        Config {
            jidian_path: "a".into(),
            unigram_path: "b".into(),
            output_path: "c".into(),
            ..Default::default()
        }
    }

    fn e(text: &str) -> Entry {
        Entry::new(text.into(), "abcd".into(), 10, 0)
    }

    #[test]
    fn median_freq_maps_to_target_median() {
        // 归一化的定义点：中位频次的词应拿到 target_median 权重
        let c = cfg();
        let median = 1000.0_f64;
        let log_median = (median + 1.0).log10();
        assert_eq!(compute_weight(1000, log_median, &c), c.target_median);
    }

    #[test]
    fn median_of_even_count_averages_middle_two() {
        let mut u = Unigram::new();
        u.insert("a".into(), 10);
        u.insert("b".into(), 20);
        u.insert("c".into(), 30);
        u.insert("d".into(), 40);
        let entries = vec![e("a"), e("b"), e("c"), e("d")];
        assert_eq!(median_raw_freq(&entries, &u), 25.0);
    }

    #[test]
    fn median_ignores_entries_missing_from_unigram() {
        let mut u = Unigram::new();
        u.insert("a".into(), 100);
        let entries = vec![e("a"), e("不存在"), e("也不存在")];
        assert_eq!(
            median_raw_freq(&entries, &u),
            100.0,
            "未命中条目不该参与中位数"
        );
    }

    #[test]
    fn empty_unigram_falls_back_without_dividing_by_zero() {
        assert_eq!(median_raw_freq(&[e("x")], &Unigram::new()), 1000.0);
        assert_eq!(compute_weight(500, 0.0, &cfg()), cfg().weight_min);
    }

    #[test]
    fn weight_is_clamped_to_range() {
        let c = cfg();
        let tiny_log_median = 0.001; // 放大到超出上限
        assert_eq!(compute_weight(999999, tiny_log_median, &c), c.weight_max);
        assert_eq!(compute_weight(0, 3.0, &c), c.weight_min);
    }

    #[test]
    fn fallback_tiers_follow_jidian_priority() {
        let c = cfg();
        assert_eq!(fallback_weight(30, &c), 180);
        assert_eq!(fallback_weight(25, &c), 150, "20..29 落 priority_20");
        assert_eq!(fallback_weight(20, &c), 150);
        assert_eq!(fallback_weight(10, &c), 120);
        assert_eq!(fallback_weight(0, &c), 120, "无优先级按最低档");
    }

    #[test]
    fn higher_freq_never_yields_lower_weight() {
        let c = cfg();
        let lm = 3.0;
        let mut prev = 0;
        for f in [1, 10, 100, 1000, 10_000, 100_000] {
            let w = compute_weight(f, lm, &c);
            assert!(
                w >= prev,
                "权重须随频次单调不减: freq={f} w={w} prev={prev}"
            );
            prev = w;
        }
    }
}
