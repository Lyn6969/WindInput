//! gen_unigram：从 rime-frost cn_dicts/ 提取词频 → unigram.txt
//!
//! 用法：gen_unigram --rime <cn_dicts_dir> --out <unigram.txt>
//!
//! 输出格式（与 `gen_dict` 的读取端 `weight::load_unigram()` 一致）：
//!   # 注释行
//!   词语\t频次
//!
//! **唯一消费者是 `gen_dict`**：它拿这份词频给五笔扩展词库的 CJK 条目赋权。产物留在
//! `.cache/`，不随 `data/` 分发——引擎的词图打分已改用词条自身的词典权重
//! （见 wind-engine/pinyin/lattice.rs::score_node_inner），不再读语言模型文件。

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

// 同一词条出现在多个词库时**取最高频次**，不是累加。
//
// 这几个词库互相重叠（base/ext/tencent 都收常用词），各自的 weight 是独立标定的词频，
// 累加等于重复计数——一个词恰好被 N 个库收录就被放大 N 倍，而被多库收录的恰恰是常用词，
// 结果是系统性抬高常用词权重。取 max 相当于选最可信的那次标定。
//
// 取 max 使合并结果与文件顺序无关，故此表的次序仅影响日志可读性。
const DICT_FILES: &[&str] = &[
    "base.dict.yaml",
    "ext.dict.yaml",
    "8105.dict.yaml",
    "41448.dict.yaml",
    "others.dict.yaml",
    "corrections.dict.yaml",
    "tencent.dict.yaml",
];

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (rime_dir, out_path) = parse_args(&args)?;

    let mut freqs: HashMap<String, f64> = HashMap::new();

    for fname in DICT_FILES {
        let path = rime_dir.join(fname);
        if !path.exists() {
            continue;
        }
        let loaded = load_dict_freqs(&path)?;
        eprintln!("  {} → {} 条", fname, loaded.len());
        for (text, freq) in loaded {
            freqs
                .entry(text)
                .and_modify(|cur| {
                    if freq > *cur {
                        *cur = freq;
                    }
                })
                .or_insert(freq);
        }
    }

    if freqs.is_empty() {
        anyhow::bail!("未找到词频数据，检查 --rime 路径: {}", rime_dir.display());
    }

    // 按 text 字典序排序，输出稳定
    let mut sorted: Vec<(String, f64)> = freqs.into_iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = out_path.with_extension("txt.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        writeln!(f, "# WindInput Unigram 语言模型")?;
        writeln!(f, "# 格式: 词语\t频次")?;
        writeln!(f, "# 词频来源: 白霜拼音 (rime-frost)")?;
        writeln!(f, "########################################")?;
        for (text, freq) in &sorted {
            writeln!(f, "{}\t{}", text, *freq as u64)?;
        }
    }
    std::fs::rename(&tmp, &out_path)?;
    eprintln!("gen_unigram: {} 条 → {}", sorted.len(), out_path.display());
    Ok(())
}

/// 解析单个 rime .dict.yaml，返回 (text, freq) 列表
fn load_dict_freqs(path: &Path) -> anyhow::Result<Vec<(String, f64)>> {
    let content = std::fs::read_to_string(path)?;
    let mut result = Vec::new();
    let mut in_body = false;

    for line in content.lines() {
        if !in_body {
            if line == "..." {
                in_body = true;
            }
            continue;
        }
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 2 {
            continue;
        }
        // rime-frost 拼音格式：text\tpinyin[\tweight]
        // text 第一列为中文（非纯 ASCII）
        let text = parts[0];
        if text.is_ascii() {
            continue;
        }
        let freq: f64 = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(1.0);
        if freq > 0.0 {
            result.push((text.to_string(), freq));
        }
    }
    Ok(result)
}

fn parse_args(args: &[String]) -> anyhow::Result<(PathBuf, PathBuf)> {
    let mut rime = None;
    let mut out = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--rime" | "-rime" => {
                i += 1;
                rime = args.get(i).map(PathBuf::from);
            }
            "--out" | "-out" => {
                i += 1;
                out = args.get(i).map(PathBuf::from);
            }
            _ => {}
        }
        i += 1;
    }
    Ok((
        rime.ok_or_else(|| {
            anyhow::anyhow!("用法: gen_unigram --rime <cn_dicts_dir> --out <unigram.txt>")
        })?,
        out.ok_or_else(|| {
            anyhow::anyhow!("用法: gen_unigram --rime <cn_dicts_dir> --out <unigram.txt>")
        })?,
    ))
}
