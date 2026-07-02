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

#[cfg(test)]
mod tests {
    use super::*;

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
}
