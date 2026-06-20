//! gen_pinyin：从 mozillazg/pinyin-data 原始数据生成汉字拼音反查表 pinyin_map.txt
//!
//! 用法：gen_pinyin --src <pinyin-data 目录> --out <pinyin_map.txt>
//!
//! 与 Go 版 `cmd/gen_pinyin_data` 对齐：源文件由 dev.sh 在线下载到 .cache/pinyin-data/，
//! 本工具仅做本地合并（不内置 HTTP，与 gen_unigram/gen_opencc 一致）。
//!
//! 数据源：https://github.com/mozillazg/pinyin-data
//! 优先级（从高到低）：
//!  1. overwrite.txt      — 手工纠正，最终权威（可选）
//!  2. kXHC1983.txt       — 现代新华字典多音字（最常用音在前）
//!  3. kTGHZ2013.txt      — 通用规范汉字多音字（补 XHC 遗漏）
//!  4. kMandarin_8105.txt — 8105 标准汉字首音（迭代字集 + fallback）
//!
//! 刻意排除 kHanyuPinyin.txt（汉语大字典，含大量古音/方言音）。
//!
//! 输出格式（与 wind-reverse ReverseLookup::load_pinyin 一致，pinyin-data 原生风格）：
//!   U+XXXX: py1,py2  # 字
//! 多音字按常用频率排序（最常用在前）。

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (src, out) = parse_args(&args)?;

    let xhc = parse_file(&src.join("kXHC1983.txt"))?;
    let tghz = parse_file(&src.join("kTGHZ2013.txt"))?;
    let m8105 = parse_file(&src.join("kMandarin_8105.txt"))?;
    let overwrite = parse_file_optional(&src.join("overwrite.txt"))?;

    eprintln!(
        "  kXHC1983={} kTGHZ2013={} kMandarin_8105={} overwrite={}",
        xhc.len(),
        tghz.len(),
        m8105.len(),
        overwrite.len()
    );

    // 为 8105 中每个字确定现代读音
    let mut entries: Vec<(char, Vec<String>)> = Vec::with_capacity(m8105.len());
    for (code, primary) in &m8105 {
        let Some(ch) = code_to_char(code) else {
            continue;
        };
        let readings = if let Some(ow) = overwrite.get(code) {
            ow.clone()
        } else {
            let mut seen: HashMap<&str, ()> = HashMap::new();
            let mut readings: Vec<String> = Vec::new();
            for py in xhc.get(code).into_iter().flatten() {
                if seen.insert(py.as_str(), ()).is_none() {
                    readings.push(py.clone());
                }
            }
            for py in tghz.get(code).into_iter().flatten() {
                if seen.insert(py.as_str(), ()).is_none() {
                    readings.push(py.clone());
                }
            }
            if readings.is_empty() {
                readings = primary.clone();
            }
            readings
        };
        if !readings.is_empty() {
            entries.push((ch, readings));
        }
    }

    // 按 Unicode 码位排序，保证输出稳定
    entries.sort_by_key(|(c, _)| *c);

    let poly = entries.iter().filter(|(_, r)| r.len() > 1).count();
    eprintln!("生成: {} 个汉字，其中多音字 {} 个", entries.len(), poly);

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = out.with_extension("txt.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        writeln!(f, "# WindInput 汉字拼音反查表")?;
        writeln!(f, "# 格式: U+XXXX: py1,py2  # 字   (多音字按常用频率排序)")?;
        writeln!(
            f,
            "# 数据源: pinyin-data (kXHC1983 + kTGHZ2013 + kMandarin_8105 + overwrite)"
        )?;
        writeln!(f, "# 已排除 kHanyuPinyin（汉语大字典古音）")?;
        writeln!(f, "########################################")?;
        for (ch, readings) in &entries {
            writeln!(f, "U+{:04X}: {}  # {}", *ch as u32, readings.join(","), ch)?;
        }
    }
    std::fs::rename(&tmp, &out)?;
    eprintln!("gen_pinyin: {} 条 → {}", entries.len(), out.display());
    Ok(())
}

/// 解析 pinyin-data 格式文件，返回 "U+XXXX" -> [读音] 映射。
/// 格式：`U+XXXX: py1,py2  # 汉字`
fn parse_file(path: &Path) -> anyhow::Result<HashMap<String, Vec<String>>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("读取 {} 失败: {}", path.display(), e))?;
    Ok(parse_content(&content))
}

/// 解析可选文件；不存在时返回空表。
fn parse_file_optional(path: &Path) -> anyhow::Result<HashMap<String, Vec<String>>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    parse_file(path)
}

fn parse_content(content: &str) -> HashMap<String, Vec<String>> {
    let mut result: HashMap<String, Vec<String>> = HashMap::new();
    for line in content.lines() {
        let mut line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // 去掉行内 `# 汉字` 注释
        if let Some(idx) = line.find('#') {
            line = line[..idx].trim();
        }
        let Some((code, rest)) = line.split_once(':') else {
            continue;
        };
        let code = code.trim();
        // 跳过非 BMP 字符（U+2XXXX 等，码位 > 4 位十六进制）
        if code.trim_start_matches("U+").len() > 4 {
            continue;
        }
        let readings: Vec<String> = rest
            .trim()
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        if !readings.is_empty() {
            result.insert(code.to_string(), readings);
        }
    }
    result
}

/// 将 "U+675C" 转为 char。
fn code_to_char(code: &str) -> Option<char> {
    let hex = code.trim().trim_start_matches("U+");
    u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
}

fn parse_args(args: &[String]) -> anyhow::Result<(PathBuf, PathBuf)> {
    let mut src = None;
    let mut out = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--src" | "-src" => {
                i += 1;
                src = args.get(i).map(PathBuf::from);
            }
            "--out" | "-out" => {
                i += 1;
                out = args.get(i).map(PathBuf::from);
            }
            _ => {}
        }
        i += 1;
    }
    let usage = "用法: gen_pinyin --src <pinyin-data 目录> --out <pinyin_map.txt>";
    Ok((
        src.ok_or_else(|| anyhow::anyhow!(usage))?,
        out.ok_or_else(|| anyhow::anyhow!(usage))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_content_single_and_multi() {
        let m = parse_content("U+4E00: yī  # 一\nU+4E07: wàn,mò  # 万\n# 注释\n");
        assert_eq!(m.get("U+4E00").unwrap(), &vec!["yī".to_string()]);
        assert_eq!(
            m.get("U+4E07").unwrap(),
            &vec!["wàn".to_string(), "mò".to_string()]
        );
    }

    #[test]
    fn test_parse_skips_non_bmp() {
        let m = parse_content("U+20000: foo  # 𠀀\n");
        assert!(m.is_empty(), "非 BMP 字符应跳过");
    }

    #[test]
    fn test_code_to_char() {
        assert_eq!(code_to_char("U+675C"), Some('杜'));
        assert_eq!(code_to_char("U+4E00"), Some('一'));
    }
}
