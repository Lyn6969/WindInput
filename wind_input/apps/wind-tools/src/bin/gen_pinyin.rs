//! gen_pinyin：从 mozillazg/pinyin-data 生成汉字拼音反查表 pinyin_map.txt
//!
//! 用法：gen_pinyin --src <pinyin-data 目录> --out <pinyin_map.txt>
//!
//! 源文件由 dev.sh 在线下载到 .cache/pinyin-data/，本工具仅做本地合并
//! （不内置 HTTP，与 gen_unigram/gen_opencc 一致）。
//!
//! 数据源：https://github.com/mozillazg/pinyin-data
//!
//! 策略（全量骨架 + 古音白名单裁剪）：
//!  1. pinyin.txt        — 官方合成全量底表（~4.4 万字，首音已按 kMandarin 最常用音排序）
//!  2. 现代字典白名单     — kMandarin_8105 ∪ kTGHZ2013 ∪ kXHC1983 的读音集合
//!  3. overwrite.txt     — 手工纠正，最高优先级（可选）
//!
//! 裁剪规则：某字若被现代字典收录（present），则只保留读音同时出现在白名单中的项——
//! 只在 pinyin.txt（即仅来自汉语大字典 kHanyuPinyin）出现的读音判为古音/方言音丢弃。
//! 过滤后为空（声调标注分歧等边角）则保留 pinyin.txt 原读音兜底；现代字典未收的生僻字
//! 无从判别，原样保留。首音（最常用音）不受影响——白名单必含首音，裁剪只作用于长尾。
//!
//! 输出格式（与 wind-reverse ReverseLookup::load_pinyin 一致，pinyin-data 原生风格）：
//!   U+XXXX: py1,py2  # 字
//! 多音字按常用频率排序（最常用在前，承自 pinyin.txt）。

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (src, out) = parse_args(&args)?;

    // 全量底表
    let base = parse_file(&src.join("pinyin.txt"))?;
    // 现代字典白名单：present[code] + seen[(code, reading)]
    let mut present: HashSet<String> = HashSet::new();
    let mut white: HashSet<(String, String)> = HashSet::new();
    for name in ["kMandarin_8105.txt", "kTGHZ2013.txt", "kXHC1983.txt"] {
        for (code, readings) in parse_file(&src.join(name))? {
            present.insert(code.clone());
            for r in readings {
                white.insert((code.clone(), r));
            }
        }
    }
    let overwrite = parse_file_optional(&src.join("overwrite.txt"))?;

    eprintln!(
        "  pinyin.txt(底表)={} 白名单字={} overwrite={}",
        base.len(),
        present.len(),
        overwrite.len()
    );

    let mut entries: Vec<(char, Vec<String>)> = Vec::with_capacity(base.len());
    let mut trimmed_chars = 0usize; // 被裁掉 ≥1 古音的字
    let mut trimmed_readings = 0usize; // 共裁掉的古音读音数
    let mut fallback_empty = 0usize; // 过滤后为空、走原样兜底的字
    for (code, base_readings) in &base {
        let Some(ch) = code_to_char(code) else {
            continue;
        };
        let (readings, dropped, empty_fallback) =
            resolve_readings(code, base_readings, &white, &present, &overwrite);
        if empty_fallback {
            fallback_empty += 1;
        } else if dropped > 0 {
            trimmed_chars += 1;
            trimmed_readings += dropped;
        }
        if !readings.is_empty() {
            entries.push((ch, readings));
        }
    }

    // 按 Unicode 码位排序，保证输出稳定
    entries.sort_by_key(|(c, _)| *c);

    let poly = entries.iter().filter(|(_, r)| r.len() > 1).count();
    eprintln!(
        "生成: {} 个汉字（多音字 {}）；裁古音 {} 字/{} 读音，空则兜底 {} 字",
        entries.len(),
        poly,
        trimmed_chars,
        trimmed_readings,
        fallback_empty
    );

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
            "# 数据源: pinyin-data (pinyin.txt 全量底表 + kMandarin_8105/kTGHZ2013/kXHC1983 白名单裁古音 + overwrite)"
        )?;
        writeln!(f, "########################################")?;
        for (ch, readings) in &entries {
            writeln!(f, "U+{:04X}: {}  # {}", *ch as u32, readings.join(","), ch)?;
        }
    }
    std::fs::rename(&tmp, &out)?;
    eprintln!("gen_pinyin: {} 条 → {}", entries.len(), out.display());
    Ok(())
}

/// 决定某字的最终读音（承 pinyin.txt 顺序）。
/// 返回 `(读音, 裁掉的古音数, 是否过滤为空走原样兜底)`。
/// 优先级：overwrite > 白名单裁剪 > 生僻字原样。
fn resolve_readings(
    code: &str,
    base_readings: &[String],
    white: &HashSet<(String, String)>,
    present: &HashSet<String>,
    overwrite: &HashMap<String, Vec<String>>,
) -> (Vec<String>, usize, bool) {
    if let Some(ow) = overwrite.get(code) {
        return (ow.clone(), 0, false);
    }
    if present.contains(code) {
        let kept: Vec<String> = base_readings
            .iter()
            .filter(|r| white.contains(&(code.to_string(), (*r).clone())))
            .cloned()
            .collect();
        if kept.is_empty() {
            // 声调标注分歧等边角：白名单一个都没命中 → 保留原样兜底
            return (base_readings.to_vec(), 0, true);
        }
        let dropped = base_readings.len() - kept.len();
        return (kept, dropped, false);
    }
    // 现代字典未收的生僻字：无从判别古音，原样保留
    (base_readings.to_vec(), 0, false)
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
        // 按逗号或空白分割：音节内部无空格/逗号，故对上游偶见的空格分隔笔误
        // （如 overwrite.txt `U+3D14: yì xì sè`）天然容错。
        let readings: Vec<String> = rest
            .trim()
            .split(|c: char| c == ',' || c.is_whitespace())
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

/// 将 "U+675C" 转为 char（含增补平面 U+2XXXX）。
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
    fn test_parse_tolerates_space_separator() {
        // 上游 overwrite.txt 偶见空格分隔笔误（U+3D14: yì xì sè）→ 应按空白切成多音。
        let m = parse_content("U+3D14: yì xì sè  # 㴔\n");
        assert_eq!(
            m.get("U+3D14").unwrap(),
            &vec!["yì".to_string(), "xì".to_string(), "sè".to_string()]
        );
    }

    #[test]
    fn test_parse_keeps_non_bmp() {
        // 全量底表包含增补平面字符（CJK 扩展 B+），不再跳过。
        let m = parse_content("U+20000: qiū  # 𠀀\n");
        assert_eq!(m.get("U+20000").unwrap(), &vec!["qiū".to_string()]);
    }

    #[test]
    fn test_code_to_char() {
        assert_eq!(code_to_char("U+675C"), Some('杜'));
        assert_eq!(code_to_char("U+4E00"), Some('一'));
        // 增补平面
        assert_eq!(code_to_char("U+20000"), char::from_u32(0x20000));
    }

    fn white_set(pairs: &[(&str, &str)]) -> (HashSet<(String, String)>, HashSet<String>) {
        let mut white = HashSet::new();
        let mut present = HashSet::new();
        for (c, r) in pairs {
            white.insert((c.to_string(), r.to_string()));
            present.insert(c.to_string());
        }
        (white, present)
    }

    #[test]
    fn test_resolve_trims_archaic_readings() {
        // 重: zhòng,chóng,tóng —— tóng 只在大字典 → 裁掉；zhòng/chóng 在白名单保留。
        let (white, present) = white_set(&[("U+91CD", "zhòng"), ("U+91CD", "chóng")]);
        let base = vec!["zhòng".to_string(), "chóng".to_string(), "tóng".to_string()];
        let (r, dropped, empty) =
            resolve_readings("U+91CD", &base, &white, &present, &HashMap::new());
        assert_eq!(r, vec!["zhòng".to_string(), "chóng".to_string()]);
        assert_eq!(dropped, 1);
        assert!(!empty);
    }

    #[test]
    fn test_resolve_keeps_first_reading_order() {
        // 首音顺序承自底表，裁剪不打乱前序。
        let (white, present) = white_set(&[("U+884C", "xíng"), ("U+884C", "háng")]);
        let base = vec![
            "xíng".to_string(),
            "háng".to_string(),
            "xìng".to_string(), // 古音，裁掉
        ];
        let (r, dropped, _) = resolve_readings("U+884C", &base, &white, &present, &HashMap::new());
        assert_eq!(r, vec!["xíng".to_string(), "háng".to_string()]);
        assert_eq!(dropped, 1);
    }

    #[test]
    fn test_resolve_empty_after_filter_falls_back() {
        // 声调标注分歧：底表读音一个都不在白名单 → 原样兜底，不清空。
        let (white, present) = white_set(&[("U+3D14", "xī"), ("U+3D14", "jí")]);
        let base = vec!["yì".to_string(), "xì".to_string(), "sè".to_string()];
        let (r, dropped, empty) =
            resolve_readings("U+3D14", &base, &white, &present, &HashMap::new());
        assert_eq!(r, base);
        assert_eq!(dropped, 0);
        assert!(empty, "过滤为空应标记兜底");
    }

    #[test]
    fn test_resolve_rare_char_passthrough() {
        // 现代字典未收（present 无此 code）→ 原样保留，不裁。
        let white = HashSet::new();
        let present = HashSet::new();
        let base = vec!["zhú".to_string()];
        let (r, dropped, empty) =
            resolve_readings("U+529A", &base, &white, &present, &HashMap::new());
        assert_eq!(r, base);
        assert_eq!(dropped, 0);
        assert!(!empty);
    }

    #[test]
    fn test_resolve_overwrite_wins() {
        // overwrite 最高优先级，绕过白名单裁剪。
        let (white, present) = white_set(&[("U+4E07", "wàn")]);
        let base = vec!["wàn".to_string(), "mò".to_string()];
        let mut ow = HashMap::new();
        ow.insert(
            "U+4E07".to_string(),
            vec!["wàn".to_string(), "mò".to_string()],
        );
        let (r, dropped, empty) = resolve_readings("U+4E07", &base, &white, &present, &ow);
        assert_eq!(r, vec!["wàn".to_string(), "mò".to_string()]);
        assert_eq!(dropped, 0);
        assert!(!empty);
    }
}
