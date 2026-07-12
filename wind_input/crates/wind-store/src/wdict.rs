//! wdict（.wdict.yaml）格式读写 — 复刻旧 Go pkg/dictio。
//! 本期仅实现 phrases 段；结构按可扩展写，便于后续 P2c 复用其它 section。
//!
//! 文件 = `# 注释` + `wind_dict:` YAML 头 + `\n--- !<tag>\n` 分隔的 TSV 数据段。
//! TSV 字段转义：`\`→`\\`、换行→`\n`、制表→`\t`；bool→"1"/"0"。

/// TSV 字段转义（与 Go EscapeField 一致）。
pub fn escape_field(s: &str) -> String {
    if !s.contains(['\\', '\n', '\t']) {
        return s.to_string();
    }
    s.replace('\\', r"\\")
        .replace('\n', r"\n")
        .replace('\t', r"\t")
}

/// TSV 字段反转义（与 Go UnescapeField 一致）。
pub fn unescape_field(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn format_bool(b: bool) -> &'static str {
    if b { "1" } else { "0" }
}

pub fn parse_bool(s: &str) -> bool {
    s == "1" || s.eq_ignore_ascii_case("true")
}

/// wdict phrases 段的一行（导入导出用；不含 is_system——导出仅用户短语）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhraseIo {
    pub code: String,
    pub text: String,
    pub weight: i32,
    pub position: i32,
    pub enabled: bool,
}

const PHRASE_COLUMNS: &[&str] = &["code", "text", "weight", "position", "enabled"];

/// 导出 phrases 为 wdict 文本（YAML 头 + `--- !phrases` TSV 段）。
pub fn export_phrases_wdict(rows: &[PhraseIo], exported_at: &str) -> String {
    let mut s = String::new();
    s.push_str("# WindInput 用户数据文件\n");
    s.push_str("wind_dict:\n");
    s.push_str("  version: 1\n");
    s.push_str("  generator: WindInput\n");
    s.push_str(&format!("  exported_at: {exported_at}\n"));
    s.push_str("  sections:\n");
    s.push_str("    phrases:\n");
    s.push_str(&format!("      columns: [{}]\n", PHRASE_COLUMNS.join(", ")));
    s.push_str("\n--- !phrases\n");
    for r in rows {
        s.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            escape_field(&r.code),
            escape_field(&r.text),
            r.weight,
            r.position,
            format_bool(r.enabled),
        ));
    }
    s
}

/// 解析 wdict 文本的 phrases 段。返回 (行, 跳过的非法行数)。
/// 只认 version==1；无 phrases 段返回空。列按 header 声明顺序解析，缺 header 用默认列。
pub fn parse_phrases_wdict(text: &str) -> Result<(Vec<PhraseIo>, usize), String> {
    // 1. 头部 = 第一个 "\n---" 之前
    let header = text.split("\n---").next().unwrap_or("");
    if !header.contains("wind_dict:") {
        return Err("不是 WindDict 文件（缺 wind_dict 头）".into());
    }
    // version 校验（简单行扫描，避免引入 yaml 依赖）
    let version_ok = header.lines().any(|l| {
        let t = l.trim();
        t.starts_with("version:") && t.trim_start_matches("version:").trim() == "1"
    });
    if !version_ok {
        return Err("不支持的 WindDict 版本（需 version: 1）".into());
    }
    // 2. 定位 phrases 段：`--- !phrases` 之后到下一个 `\n---` 之前
    let Some(after_tag) = find_section_body(text, "phrases") else {
        return Ok((Vec::new(), 0));
    };
    // 3. 逐行 TSV
    let cols = phrase_columns_from_header(header);
    let mut rows = Vec::new();
    let mut skipped = 0usize;
    for line in after_tag.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < cols.len() {
            skipped += 1;
            continue;
        }
        let get = |name: &str| -> &str {
            cols.iter()
                .position(|c| c == name)
                .map(|i| fields[i])
                .unwrap_or("")
        };
        rows.push(PhraseIo {
            code: unescape_field(get("code")),
            text: unescape_field(get("text")),
            weight: get("weight").trim().parse().unwrap_or(0),
            position: get("position").trim().parse().unwrap_or(0),
            enabled: parse_bool(get("enabled").trim()),
        });
    }
    Ok((rows, skipped))
}

/// 从头部读 phrases 段列定义（`columns: [a, b, ...]`）；缺则默认列。
fn phrase_columns_from_header(header: &str) -> Vec<String> {
    for l in header.lines() {
        let t = l.trim();
        if let Some(rest) = t.strip_prefix("columns:") {
            let inner = rest.trim().trim_start_matches('[').trim_end_matches(']');
            let cols: Vec<String> = inner
                .split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect();
            if !cols.is_empty() {
                return cols;
            }
        }
    }
    PHRASE_COLUMNS.iter().map(|s| s.to_string()).collect()
}

/// wdict words 段的一行（用户词导入导出）。count/created_at 属个人数据，不随导出流转。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordIo {
    pub code: String,
    pub text: String,
    pub weight: i32,
}

const WORD_COLUMNS: &[&str] = &["code", "text", "weight"];

/// 导出 words 为 wdict 文本（YAML 头 + `--- !words` TSV 段）。
pub fn export_words_wdict(rows: &[WordIo], exported_at: &str) -> String {
    let mut s = String::new();
    s.push_str("# WindInput 用户数据文件\n");
    s.push_str("wind_dict:\n");
    s.push_str("  version: 1\n");
    s.push_str("  generator: WindInput\n");
    s.push_str(&format!("  exported_at: {exported_at}\n"));
    s.push_str("  sections:\n");
    s.push_str("    words:\n");
    s.push_str(&format!("      columns: [{}]\n", WORD_COLUMNS.join(", ")));
    s.push_str("\n--- !words\n");
    for r in rows {
        s.push_str(&format!(
            "{}\t{}\t{}\n",
            escape_field(&r.code),
            escape_field(&r.text),
            r.weight,
        ));
    }
    s
}

/// 解析 wdict 文本的 words 段。返回 (行, 跳过的非法行数)。只认 version==1。
pub fn parse_words_wdict(text: &str) -> Result<(Vec<WordIo>, usize), String> {
    let header = text.split("\n---").next().unwrap_or("");
    if !header.contains("wind_dict:") {
        return Err("不是 WindDict 文件（缺 wind_dict 头）".into());
    }
    let version_ok = header.lines().any(|l| {
        let t = l.trim();
        t.starts_with("version:") && t.trim_start_matches("version:").trim() == "1"
    });
    if !version_ok {
        return Err("不支持的 WindDict 版本（需 version: 1）".into());
    }
    let Some(after_tag) = find_section_body(text, "words") else {
        return Ok((Vec::new(), 0));
    };
    let cols = words_columns_from_header(header);
    let mut rows = Vec::new();
    let mut skipped = 0usize;
    for line in after_tag.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < cols.len() {
            skipped += 1;
            continue;
        }
        let get = |name: &str| -> &str {
            cols.iter()
                .position(|c| c == name)
                .map(|i| fields[i])
                .unwrap_or("")
        };
        rows.push(WordIo {
            code: unescape_field(get("code")),
            text: unescape_field(get("text")),
            weight: get("weight").trim().parse().unwrap_or(0),
        });
    }
    Ok((rows, skipped))
}

/// 从头部读 words 段列定义；缺则默认列。
fn words_columns_from_header(header: &str) -> Vec<String> {
    for l in header.lines() {
        let t = l.trim();
        if let Some(rest) = t.strip_prefix("columns:") {
            let inner = rest.trim().trim_start_matches('[').trim_end_matches(']');
            let cols: Vec<String> = inner
                .split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect();
            if !cols.is_empty() {
                return cols;
            }
        }
    }
    WORD_COLUMNS.iter().map(|s| s.to_string()).collect()
}

/// 返回 `--- !<tag>\n` 之后、下一个 `\n---` 之前的正文。
fn find_section_body<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let marker = format!("--- !{tag}");
    let start = text.find(&marker)? + marker.len();
    // 跳到该行行尾之后
    let after = &text[start..];
    let body_start = after
        .find('\n')
        .map(|i| start + i + 1)
        .unwrap_or(text.len());
    let body = &text[body_start..];
    // 到下一个 "\n---" 为止
    match body.find("\n---") {
        Some(i) => Some(&body[..i]),
        None => Some(body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phrases_wdict_roundtrip() {
        let rows = vec![
            PhraseIo {
                code: "bj".into(),
                text: "北京".into(),
                weight: 1000,
                position: 0,
                enabled: true,
            },
            PhraseIo {
                code: "ml".into(),
                text: "多行\n第二行\t带制表".into(),
                weight: 500,
                position: 2,
                enabled: false,
            },
        ];
        let s = export_phrases_wdict(&rows, "2026-07-02T00:00:00+08:00");
        assert!(s.contains("wind_dict:"));
        assert!(s.contains("--- !phrases"));
        let (parsed, skipped) = parse_phrases_wdict(&s).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(parsed, rows, "导出→解析应无损往返（含换行/制表）");
    }

    #[test]
    fn parse_rejects_bad_version() {
        let bad = "wind_dict:\n  version: 2\n  sections:\n    phrases:\n      columns: [code, text, weight, position, enabled]\n\n--- !phrases\nx\t词\t1\t0\t1\n";
        assert!(parse_phrases_wdict(bad).is_err(), "version!=1 应拒绝");
    }

    #[test]
    fn parse_tolerates_bad_lines() {
        let s = "wind_dict:\n  version: 1\n  sections:\n    phrases:\n      columns: [code, text, weight, position, enabled]\n\n--- !phrases\nok\t好\t1000\t0\t1\nbadline_no_tabs\nkw\t坏权重\tNaN\t0\t1\n";
        let (rows, skipped) = parse_phrases_wdict(s).unwrap();
        // 第 2 行列数不足跳过；第 3 行权重非数字 → 回退 0，仍收（不算跳过）
        assert_eq!(rows.len(), 2);
        assert_eq!(skipped, 1);
        assert_eq!(rows[0].code, "ok");
        assert_eq!(rows[1].weight, 0, "非法数字回退 0");
    }

    #[test]
    fn escape_roundtrip() {
        for s in ["普通", "含\t制表", "含\n换行", "反斜杠\\结尾", "混\\t\n合"] {
            assert_eq!(unescape_field(&escape_field(s)), s, "往返应还原: {s:?}");
        }
    }
    #[test]
    fn escape_only_special() {
        assert_eq!(escape_field("abc"), "abc");
        assert_eq!(escape_field("a\tb"), r"a\tb");
        assert_eq!(escape_field("a\nb"), r"a\nb");
        assert_eq!(escape_field(r"a\b"), r"a\\b");
    }
    #[test]
    fn bool_format_parse() {
        assert_eq!(format_bool(true), "1");
        assert_eq!(format_bool(false), "0");
        assert!(parse_bool("1"));
        assert!(!parse_bool("0"));
        assert!(!parse_bool(""));
    }

    #[test]
    fn words_wdict_roundtrip() {
        let rows = vec![
            WordIo {
                code: "a".into(),
                text: "工".into(),
                weight: 100,
            },
            WordIo {
                code: "ml".into(),
                text: "多行\n带\t制表".into(),
                weight: 0,
            },
        ];
        let s = export_words_wdict(&rows, "2026-07-11T00:00:00+08:00");
        assert!(s.contains("wind_dict:"));
        assert!(s.contains("--- !words"));
        let (parsed, skipped) = parse_words_wdict(&s).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(parsed, rows, "导出→解析应无损往返(含换行/制表)");
    }

    #[test]
    fn words_parse_rejects_bad_version() {
        let bad = "wind_dict:\n  version: 2\n\n--- !words\na\t工\t1\n";
        assert!(parse_words_wdict(bad).is_err(), "version!=1 应拒绝");
    }

    #[test]
    fn words_parse_tolerates_bad_lines() {
        let s = "wind_dict:\n  version: 1\n  sections:\n    words:\n      columns: [code, text, weight]\n\n--- !words\nok\t好\t10\nbadline_no_tabs\nkw\t坏权重\tNaN\n";
        let (rows, skipped) = parse_words_wdict(s).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(skipped, 1, "列数不足的行跳过");
        assert_eq!(rows[1].weight, 0, "非法数字回退 0");
    }
}
