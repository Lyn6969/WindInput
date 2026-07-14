//! 外部词库格式导入：格式自动探测 + Rime / TSV 解析。
//!
//! 与旧 Go pkg/dictio 的 import_rime.go / import_tsv.go 对齐，产出与 wdict
//! words 段相同的 [`wdict::WordIo`] 行，后续复用 import_user_words 管线。
//!
//! 格式探测按内容而非扩展名（UI 不要求用户选格式）：
//!  - WindDict：头部（首个 `\n---` 之前）含 `wind_dict:`
//!  - Rime：存在整行 `...`（YAML 文档结束标记，头/体分隔）
//!  - TSV：任一非空非注释行含制表符
//!  - 其余 → Unknown

use crate::wdict::WordIo;

/// 词库文本格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictFormat {
    WindDict,
    Rime,
    Tsv,
    Unknown,
}

impl DictFormat {
    /// RPC/UI 用的稳定标识。
    pub fn as_str(&self) -> &'static str {
        match self {
            DictFormat::WindDict => "winddict",
            DictFormat::Rime => "rime",
            DictFormat::Tsv => "tsv",
            DictFormat::Unknown => "unknown",
        }
    }
}

/// 按内容探测词库格式。
pub fn detect_dict_format(text: &str) -> DictFormat {
    let text = strip_bom(text);
    if text
        .split("\n---")
        .next()
        .unwrap_or("")
        .contains("wind_dict:")
    {
        return DictFormat::WindDict;
    }
    if text.lines().any(|l| l.trim() == "...") {
        return DictFormat::Rime;
    }
    let has_tsv_line = text.lines().any(|l| {
        let t = l.trim();
        !t.is_empty() && !t.starts_with('#') && l.contains('\t')
    });
    if has_tsv_line {
        return DictFormat::Tsv;
    }
    DictFormat::Unknown
}

/// 探测格式并解析为 words 行。返回 (格式, 行, 跳过数)。
/// Unknown 格式报错并列出支持的格式。
pub fn parse_words_auto(text: &str) -> Result<(DictFormat, Vec<WordIo>, usize), String> {
    let text = strip_bom(text);
    let fmt = detect_dict_format(text);
    let (rows, skipped) = match fmt {
        DictFormat::WindDict => crate::wdict::parse_words_wdict(text)?,
        DictFormat::Rime => parse_words_rime(text)?,
        DictFormat::Tsv => parse_words_tsv(text)?,
        DictFormat::Unknown => {
            return Err(
                "无法识别的词库格式（支持 WindDict .wdict.yaml / Rime .dict.yaml / TSV 文本）"
                    .into(),
            );
        }
    };
    Ok((fmt, rows, skipped))
}

/// 解析 Rime 词库（.dict.yaml）。返回 (行, 跳过的非法行数)。
///
/// 头 = 整行 `...` 之前；缺 `columns:` 声明用 Rime 默认列 `[text, code, weight]`。
/// 编码列去内部空格（拼音音节 `ni hao` → `nihao`）；缺 text/code 的行跳过；
/// 权重解析失败回退 0。
pub fn parse_words_rime(text: &str) -> Result<(Vec<WordIo>, usize), String> {
    let text = strip_bom(text);
    let mut lines = text.lines();
    let mut header_lines: Vec<&str> = Vec::new();
    let mut found_sep = false;
    for line in lines.by_ref() {
        if line.trim() == "..." {
            found_sep = true;
            break;
        }
        header_lines.push(line);
    }
    if !found_sep {
        return Err("无效的 Rime 词库格式（缺 `...` 头部分隔行）".into());
    }
    let cols = rime_columns_from_header(&header_lines);

    let mut rows = Vec::new();
    let mut skipped = 0usize;
    for line in lines {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let get = |name: &str| -> &str {
            cols.iter()
                .position(|c| c == name)
                .and_then(|i| fields.get(i).copied())
                .map(str::trim)
                .unwrap_or("")
        };
        let word = get("text");
        let code_raw = get("code");
        if word.is_empty() || code_raw.is_empty() {
            skipped += 1;
            continue;
        }
        rows.push(WordIo {
            code: normalize_code(code_raw),
            text: word.to_string(),
            weight: parse_weight(get("weight")),
        });
    }
    Ok((rows, skipped))
}

/// 从 Rime 头部提取 `columns:` 多行列表（`  - text` 形态）；缺则默认列。
fn rime_columns_from_header(header: &[&str]) -> Vec<String> {
    let mut cols = Vec::new();
    let mut in_columns = false;
    for line in header {
        let t = line.trim();
        if in_columns {
            if let Some(item) = t.strip_prefix("- ") {
                cols.push(item.trim().trim_matches('"').to_string());
                continue;
            }
            if t.starts_with('-') && t.len() > 1 {
                cols.push(t[1..].trim().trim_matches('"').to_string());
                continue;
            }
            break; // 非 `- xxx` 行即列表结束
        }
        if t == "columns:" {
            in_columns = true;
        }
    }
    if cols.is_empty() {
        vec!["text".into(), "code".into(), "weight".into()]
    } else {
        cols
    }
}

/// 解析纯文本 TSV（`编码\t词条[\t权重]`）。返回 (行, 跳过的非法行数)。
///
/// 列数 <2、code/text 为空、code 含非可打印 ASCII（乱码/列序颠倒防护）的行跳过；
/// 权重缺省或解析失败回退 0。
pub fn parse_words_tsv(text: &str) -> Result<(Vec<WordIo>, usize), String> {
    let text = strip_bom(text);
    let mut rows = Vec::new();
    let mut skipped = 0usize;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 2 {
            skipped += 1;
            continue;
        }
        let code_raw = fields[0].trim();
        let word = fields[1].trim();
        if code_raw.is_empty() || word.is_empty() || !is_valid_code(code_raw) {
            skipped += 1;
            continue;
        }
        rows.push(WordIo {
            code: normalize_code(code_raw),
            text: word.to_string(),
            weight: parse_weight(fields.get(2).map(|s| s.trim()).unwrap_or("")),
        });
    }
    Ok((rows, skipped))
}

/// 编码归一化：去内部空白（拼音音节合并；码表码无空格时幂等）。
fn normalize_code(code: &str) -> String {
    code.split_whitespace().collect()
}

/// 编码合法性：可打印 ASCII（0x20-0x7E）。拦乱码与"词在前码在后"的列序颠倒文件。
fn is_valid_code(code: &str) -> bool {
    code.chars().all(|c| ('\x20'..='\x7e').contains(&c))
}

/// 权重解析：整数优先，浮点截断兜底（部分 Rime 词库权重为浮点），失败回退 0。
fn parse_weight(s: &str) -> i32 {
    if s.is_empty() {
        return 0;
    }
    if let Ok(v) = s.parse::<i64>() {
        return v.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    }
    if let Ok(v) = s.parse::<f64>() {
        return v.clamp(i32::MIN as f64, i32::MAX as f64) as i32;
    }
    0
}

fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RIME_SAMPLE: &str = "# Rime dictionary\n---\nname: luna_pinyin\nversion: \"0.9\"\nsort: by_weight\n...\n\n你好\tni hao\t100\n世界\tshi jie\t50\n";

    #[test]
    fn detect_winddict_rime_tsv_unknown() {
        let wd = "# c\nwind_dict:\n  version: 1\n\n--- !words\na\t工\t1\n";
        assert_eq!(detect_dict_format(wd), DictFormat::WindDict);
        assert_eq!(detect_dict_format(RIME_SAMPLE), DictFormat::Rime);
        assert_eq!(detect_dict_format("nihao\t你好\t10\n"), DictFormat::Tsv);
        assert_eq!(
            detect_dict_format("只有词\n没有制表符\n"),
            DictFormat::Unknown
        );
        // BOM 不影响探测
        assert_eq!(
            detect_dict_format("\u{feff}wind_dict:\n  version: 1\n\n--- !words\n"),
            DictFormat::WindDict
        );
    }

    #[test]
    fn rime_default_columns_and_space_join() {
        let (rows, skipped) = parse_words_rime(RIME_SAMPLE).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(rows.len(), 2);
        // 默认列序 text 在前;拼音码去空格连写
        assert_eq!(rows[0].text, "你好");
        assert_eq!(rows[0].code, "nihao");
        assert_eq!(rows[0].weight, 100);
    }

    #[test]
    fn rime_explicit_columns_reorder() {
        let s = "---\nname: t\ncolumns:\n  - code\n  - text\n  - weight\n...\nnihao\t你好\t7\n";
        let (rows, _) = parse_words_rime(s).unwrap();
        assert_eq!(rows[0].code, "nihao");
        assert_eq!(rows[0].text, "你好");
        assert_eq!(rows[0].weight, 7);
    }

    #[test]
    fn rime_skips_incomplete_and_comment_lines() {
        let s = "---\nname: t\n...\n# 注释行\n\n只有词没有码\n好\tni hao\n";
        let (rows, skipped) = parse_words_rime(s).unwrap();
        // 缺 code 的行跳过;缺 weight 列回退 0
        assert_eq!(rows.len(), 1);
        assert_eq!(skipped, 1);
        assert_eq!(rows[0].weight, 0);
    }

    #[test]
    fn rime_missing_separator_is_error() {
        assert!(parse_words_rime("name: t\n你好\tni hao\t1\n").is_err());
    }

    #[test]
    fn rime_float_weight_truncates() {
        let s = "---\nname: t\n...\n你好\tni hao\t520.9\n";
        let (rows, _) = parse_words_rime(s).unwrap();
        assert_eq!(rows[0].weight, 520, "浮点权重截断取整");
    }

    #[test]
    fn tsv_basic_and_optional_weight() {
        let s = "# 注释\nnihao\t你好\t10\nshijie\t世界\n";
        let (rows, skipped) = parse_words_tsv(s).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].weight, 10);
        assert_eq!(rows[1].weight, 0, "缺权重列回退 0");
    }

    #[test]
    fn tsv_skips_bad_lines() {
        let s = "单列无制表符x\n你好\tnihao\t1\nab\t好\t2\n";
        let (rows, skipped) = parse_words_tsv(s).unwrap();
        // 第 1 行列数不足;第 2 行 code 为汉字(列序颠倒防护)→ 均跳过
        assert_eq!(rows.len(), 1);
        assert_eq!(skipped, 2);
        assert_eq!(rows[0].code, "ab");
    }

    #[test]
    fn auto_dispatch_matches_detection() {
        let (fmt, rows, _) = parse_words_auto(RIME_SAMPLE).unwrap();
        assert_eq!(fmt, DictFormat::Rime);
        assert_eq!(rows.len(), 2);
        let (fmt, rows, _) = parse_words_auto("a\t工\t5\n").unwrap();
        assert_eq!(fmt, DictFormat::Tsv);
        assert_eq!(rows[0].code, "a");
        assert!(parse_words_auto("既无头也无制表符\n").is_err());
    }
}
