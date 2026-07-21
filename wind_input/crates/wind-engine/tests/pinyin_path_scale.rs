//! 多路径切分的**路径规模分析**（Phase 3 立项前置）
//!
//! 目的：回答「朴素枚举所有音节路径是否可行」。结论决定 lattice 的实现形态。
//!
//! 三个指标，都在真实词库产生的真实输入串上测：
//!
//! | 指标 | 含义 |
//! |---|---|
//! | `paths` | 从 0 到最远可达位置的**完整切分路径条数**（朴素枚举的规模） |
//! | `spans` | 存在 ≤ `max_word_len` 条边路径的 `(p,q)` 跨度对数 —— **本实现的词典查询次数** |
//! | `mm_spans` | 单路径切分下的查询次数（现状基线） |
//!
//! 运行：
//! ```text
//! cargo test -p wind-engine --release --test pinyin_path_scale -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use wind_engine::pinyin::dag::{Dag, SegGraph};
use wind_engine::pinyin::syllable::SyllableTrie;

const MAX_WORD_LEN: usize = 10;

fn data_dir() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("build_dev")
        .join("data");
    p.join("schemas/pinyin/cn_dicts/base.dict.yaml")
        .exists()
        .then_some(p)
}

fn read_inputs(dir: &Path, limit: usize) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let root = dir.join("schemas/pinyin");
    for name in ["cn_dicts/base.dict.yaml", "cn_dicts/ext.dict.yaml"] {
        let Ok(raw) = std::fs::read_to_string(root.join(name)) else {
            continue;
        };
        let mut in_data = false;
        for line in raw.lines() {
            if !in_data {
                if line.trim() == "..." {
                    in_data = true;
                }
                continue;
            }
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut it = line.split('\t');
            let (Some(text), Some(code)) = (it.next(), it.next()) else {
                continue;
            };
            if !code.bytes().all(|b| b.is_ascii_lowercase() || b == b' ') {
                continue;
            }
            let input: String = code.split_whitespace().collect::<Vec<_>>().concat();
            if input.is_empty() {
                continue;
            }
            out.push((text.to_string(), input));
            if out.len() >= limit {
                return out;
            }
        }
    }
    out
}

/// 朴素枚举规模：从 0 出发到最远可达位置的完整切分路径条数（饱和计数，防溢出）。
fn count_paths(graph: &SegGraph) -> u128 {
    let n = graph.len();
    // 最远可达位置（与 maximum_match 的 best_end 同义：路径皆从 0 连续覆盖）
    let mut reach = vec![false; n + 1];
    reach[0] = true;
    let mut best = 0usize;
    for p in 0..=n {
        if !reach[p] {
            continue;
        }
        best = p;
        for &q in graph.edges_from(p) {
            reach[q] = true;
        }
    }
    let mut ways = vec![0u128; n + 1];
    ways[0] = 1;
    for p in 0..best {
        if ways[p] == 0 {
            continue;
        }
        for &q in graph.edges_from(p) {
            if q <= best {
                ways[q] = ways[q].saturating_add(ways[p]);
            }
        }
    }
    ways[best]
}

/// 本实现的词典查询次数：所有存在 ≤ MAX_WORD_LEN 条边路径的 (p,q) 对。
fn count_spans(graph: &SegGraph) -> usize {
    let n = graph.len();
    let mut total = 0usize;
    for p in 0..n {
        if !graph.is_reachable(p) {
            continue;
        }
        total += graph.ends_within(p, MAX_WORD_LEN).len();
    }
    total
}

/// 现状（单路径）的查询次数：sum over start of min(max_word_len, syls-start)
fn count_mm_spans(syls: &[String]) -> usize {
    (0..syls.len())
        .map(|s| syls.len().min(s + MAX_WORD_LEN) - s)
        .sum()
}

#[test]
#[ignore = "路径规模分析：依赖 build_dev 真实词库，用 --ignored 运行"]
fn pinyin_path_scale_census() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：build_dev 拼音词库不存在");
        return;
    };
    let trie = SyllableTrie::new();
    let inputs = read_inputs(&dir, 200_000);
    println!("样本（真实词库编码去空格）: {} 条", inputs.len());

    let mut hist: HashMap<&'static str, usize> = HashMap::new();
    let mut sum_paths = 0u128;
    let mut sum_spans = 0usize;
    let mut sum_mm = 0usize;
    let mut worst: Vec<(u128, usize, usize, String, String)> = Vec::new();

    for (text, input) in &inputs {
        let dag = Dag::build(input, &trie);
        let graph = SegGraph::from_dag(&dag);
        let paths = count_paths(&graph);
        let spans = count_spans(&graph);
        let mm = count_mm_spans(&dag.maximum_match());

        sum_paths = sum_paths.saturating_add(paths);
        sum_spans += spans;
        sum_mm += mm;

        let bucket = match paths {
            0..=1 => "1",
            2..=4 => "2-4",
            5..=16 => "5-16",
            17..=64 => "17-64",
            65..=256 => "65-256",
            257..=1024 => "257-1K",
            1025..=1_048_576 => "1K-1M",
            _ => ">1M",
        };
        *hist.entry(bucket).or_default() += 1;
        worst.push((paths, spans, mm, text.clone(), input.clone()));
    }

    let n = inputs.len().max(1);
    println!("\n=== 朴素枚举的完整切分路径数分布 ===");
    for b in [
        "1", "2-4", "5-16", "17-64", "65-256", "257-1K", "1K-1M", ">1M",
    ] {
        let c = hist.get(b).copied().unwrap_or(0);
        if c > 0 {
            println!(
                "  {:>8} 条路径: {:>7} 个输入 ({:.2}%)",
                b,
                c,
                c as f64 / n as f64 * 100.0
            );
        }
    }

    worst.sort_by(|a, b| b.0.cmp(&a.0));
    println!("\n=== 路径数最多的 15 个真实输入 ===");
    for (p, s, m, t, i) in worst.iter().take(15) {
        println!(
            "  {:<28} ({:<8}) 路径 {:>14}  本实现查询 {:>4}  现状查询 {:>3}",
            i, t, p, s, m
        );
    }

    println!("\n=== 平均查询次数（词典 search 调用/次转换）===");
    println!("  现状（单路径）: {:.2}", sum_mm as f64 / n as f64);
    println!(
        "  本实现（跨度枚举）: {:.2}  （放大 {:.2}x）",
        sum_spans as f64 / n as f64,
        sum_spans as f64 / sum_mm.max(1) as f64
    );
    println!(
        "  朴素枚举路径总数: {} （平均 {:.1}/输入）",
        sum_paths,
        sum_paths as f64 / n as f64
    );

    // 最坏情况：人造长串。全由 1 字母音节 a/e/o 与其组合构成，歧义最密。
    println!("\n=== 最坏情况（人造）===");
    for s in [
        "aaaaaaaaaaaaaaaaaaaa",
        "anananananananananan",
        "xianxianxianxianxianxian",
        "nianianianianianianiania",
        "zhonghuarenmingongheguowansui",
    ] {
        let dag = Dag::build(s, &trie);
        let graph = SegGraph::from_dag(&dag);
        println!(
            "  {:<32} len={:<3} 路径 {:>18}  本实现查询 {:>4}  现状查询 {:>3}",
            s,
            s.len(),
            count_paths(&graph),
            count_spans(&graph),
            count_mm_spans(&dag.maximum_match())
        );
    }
}
