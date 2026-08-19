//! gen_aux_code：把上游辅助码原始档转成本仓的 `字=码` 辅助码表
//!
//! 用法：`gen_aux_code --cache <.cache 目录> --out <schemas/aux_code 目录>`
//!
//! # 为什么需要这个工具
//!
//! 辅助码表全部来自第三方仓库，按 `NOTICE.md` 的既定政策**不入版本库**——尤其
//! `rime-stroke` 是 LGPL-3.0，与本仓 MIT 不同（同 rime-frost 的 GPL-3.0 处理方式：
//! 构建时下载、产物随发行版分发并适用原许可）。本工具把 `.cache/aux-code/` 下的
//! 上游原始档转成运行时格式，写进构建产物。
//!
//! # 三张表的加工差异
//!
//! | 表 | 上游 | 加工 |
//! |---|---|---|
//! | `flypy_full.txt`（小鹤形码） | rime-lua-aux-code | **零转换**，只补元数据头 |
//! | `ZRM-wanxiang.txt`（自然码形码） | 同上 | **零转换**，只补元数据头 |
//! | `stroke.txt`（笔画） | rime-stroke `.dict.yaml` | 剥 YAML 头 + `\t`→`=` + **按字集裁剪** |
//!
//! 前两张已是 `字=码` 行格式，逐行与上游一致（本工具只在首部补
//! `# name/version/source/license`，供运行时显示码表名与追溯来源）。
//!
//! # ★ stroke 的字集裁剪：为什么必须由脚本定义
//!
//! 上游笔画表覆盖 11 万字（含扩展 B/C/…），全量入内存对一个默认关闭的功能过重。
//! 故按常用字集裁剪——**但这个字集必须写在代码里、可复现**：
//! PR #68 最初提交的 `stroke.txt` 是手工加工产物（14738 字），其裁剪规则既没有脚本
//! 也没有记录，我们逐表比对过 hanzi-chars 的全部 81 个字表也**无法逆向复原**
//! （任何单表与并集都对不上）。那份数据一旦上游更新就再没人能重做一遍。
//!
//! 本工具改用两个有名有姓的国标字表求并集（见 [`CHARSET_FILES`]），产物是 PR 版的
//! **超集**——只会让更多字有笔画码，不会让任何字失去。

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// stroke 裁剪用的字集文件（相对 `.cache/aux-code/charset/`，取并集）。
///
/// - `GB 18030-2000`：国标基本集，覆盖 CJK 基本区 + 扩展 A 的通行部分
/// - `《通用规范汉字表》（2013年）`：现代汉语规范字，补 GB18030 之外的规范字形
/// - `Unicode-CJK 〇`：`〇`（U+3007）不在任何汉字区块里，单列一张表
///
/// 要覆盖更多字（如日韩专用汉字）在此加表即可——加表只会让表变大，不改变已有字的码。
const CHARSET_FILES: &[&str] = &[
    "GB 18030-2000.txt",
    "《通用规范汉字表》（2013年）.txt",
    "Unicode-CJK 〇.txt",
];

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (cache, out) = parse_args(&args)?;
    let src = cache.join("aux-code");
    std::fs::create_dir_all(&out)?;

    // 1. 已是 `字=码` 格式的两张：原样透传 + 补元数据头。
    for (file, name, source) in [
        (
            "flypy_full.txt",
            "小鹤",
            "https://github.com/HowcanoeWang/rime-lua-aux-code/blob/main/aux_code/flypy_full.txt",
        ),
        (
            "ZRM-wanxiang.txt",
            "自然码",
            "https://github.com/HowcanoeWang/rime-lua-aux-code/blob/main/aux_code/ZRM-wanxiang.txt",
        ),
    ] {
        let n = passthrough(&src.join(file), &out.join(file), name, source, "MIT")?;
        eprintln!("  {file} ({n} 条，零转换) → {}", out.join(file).display());
    }

    // 2. 笔画表：YAML → `字=码` + 字集裁剪。
    let n = convert_stroke(&src, &out.join("stroke.txt"))?;
    eprintln!(
        "  stroke.txt ({n} 条) → {}",
        out.join("stroke.txt").display()
    );

    eprintln!("gen_aux_code: 完成");
    Ok(())
}

/// 元数据头。运行时只读第 1 行的 `# name:`（见 wind-aux-code::loader），
/// 其余行是给人看的来源与许可追溯，程序不解析。
fn header(name: &str, source: &str, license: &str) -> String {
    format!("# name: {name}\n# version: 1.0\n# source: {source}\n# license: {license}\n\n")
}

/// 原样透传：上游已是 `字=码` 行格式，只在首部补元数据头。
///
/// 刻意不重新解析再序列化——那会引入「我们以为的格式」与上游实际格式的偏差；
/// 逐行透传使产物与上游**逐字节可比**（剥掉头部后 `diff` 应为空）。
fn passthrough(
    src: &Path,
    dst: &Path,
    name: &str,
    source: &str,
    license: &str,
) -> anyhow::Result<usize> {
    let content = std::fs::read_to_string(src)
        .map_err(|e| anyhow::anyhow!("读取 {} 失败: {e}（先跑 gen-data 下载）", src.display()))?;
    let body: Vec<&str> = content
        .lines()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let mut f = std::fs::File::create(dst)?;
    f.write_all(header(name, source, license).as_bytes())?;
    for line in &body {
        writeln!(f, "{line}")?;
    }
    Ok(body.len())
}

/// 笔画表：rime-stroke 的 `.dict.yaml`（`字\t笔画码`，YAML 头以 `...` 结束）
/// → `字=码`，并按 [`CHARSET_FILES`] 的并集裁剪。
fn convert_stroke(src_dir: &Path, dst: &Path) -> anyhow::Result<usize> {
    let charset = load_charset(&src_dir.join("charset"))?;
    let yaml_path = src_dir.join("stroke.dict.yaml");
    let content = std::fs::read_to_string(&yaml_path).map_err(|e| {
        anyhow::anyhow!(
            "读取 {} 失败: {e}（先跑 gen-data 下载）",
            yaml_path.display()
        )
    })?;

    // 同字多码保留上游行序（行序即优先级，见 wind-aux-code::table 的 first-seen 语义）。
    let mut rows: BTreeMap<char, Vec<String>> = BTreeMap::new();
    let mut total = 0usize;
    let mut in_body = false;
    for line in content.lines() {
        if !in_body {
            // YAML front matter 以单独一行 `...` 结束（librime 词典约定）。
            if line.trim_end() == "..." {
                in_body = true;
            }
            continue;
        }
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut cols = line.split('\t');
        let (Some(text), Some(code)) = (cols.next(), cols.next()) else {
            continue;
        };
        // 只取单字：max_phrase_length = 1 的表本就无词条，多字行是异常数据。
        let mut chars = text.chars();
        let (Some(ch), None) = (chars.next(), chars.next()) else {
            continue;
        };
        if code.is_empty() || !charset.contains(&ch) {
            continue;
        }
        let codes = rows.entry(ch).or_default();
        if !codes.iter().any(|c| c == code) {
            codes.push(code.to_string());
            total += 1;
        }
    }

    let mut f = std::fs::File::create(dst)?;
    f.write_all(
        header(
            "笔画",
            "https://github.com/rime/rime-stroke/blob/master/stroke.dict.yaml",
            "GNU Lesser General Public License v3.0",
        )
        .as_bytes(),
    )?;
    writeln!(
        f,
        "# 字集裁剪（gen_aux_code::CHARSET_FILES）：{}",
        CHARSET_FILES.join(" ∪ ")
    )?;
    writeln!(f, "# 字集来源：https://github.com/zispace/hanzi-chars\n")?;
    for (ch, codes) in &rows {
        for code in codes {
            writeln!(f, "{ch}={code}")?;
        }
    }
    Ok(total)
}

/// 读字集目录下 [`CHARSET_FILES`] 列出的文件，取汉字并集。
///
/// 字表文件是「每行若干汉字 + `#` 注释」的自由格式，故按字符收集而非按行——
/// 只收 U+2E80 以上（CJK 相关区段起点），滤掉行内的 ASCII 序号与标点。
fn load_charset(dir: &Path) -> anyhow::Result<std::collections::HashSet<char>> {
    let mut set = std::collections::HashSet::new();
    for name in CHARSET_FILES {
        let path = dir.join(name);
        let content = std::fs::read_to_string(&path).map_err(|e| {
            anyhow::anyhow!(
                "读取字集 {} 失败: {e}（先跑 gen-data 下载）",
                path.display()
            )
        })?;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            set.extend(line.chars().filter(|c| *c as u32 > 0x2E80));
        }
    }
    anyhow::ensure!(
        !set.is_empty(),
        "字集为空，检查 {} 下的字表文件",
        dir.display()
    );
    Ok(set)
}

fn parse_args(args: &[String]) -> anyhow::Result<(PathBuf, PathBuf)> {
    let mut cache = None;
    let mut out = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--cache" if i + 1 < args.len() => {
                cache = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--out" if i + 1 < args.len() => {
                out = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            _ => i += 1,
        }
    }
    match (cache, out) {
        (Some(c), Some(o)) => Ok((c, o)),
        _ => {
            anyhow::bail!("用法: gen_aux_code --cache <.cache 目录> --out <schemas/aux_code 目录>")
        }
    }
}
