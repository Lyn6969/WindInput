//! 短语系统：静态/动态短语模板展开 + 命令栏（cmdbar）双路径。
//!
//! 与 Go 版本 `wind_input/internal/dict/phrase.go` + `internal/cmdbar` 对齐。
//! 加载 `system.phrases.toml`，输入码命中短语 code 时生成候选。
//!
//! **双路径**（对齐 Go design §7.2）：
//! - 短语 text 使用命令栏语法（含 `$CC(`/`$SS(` marker 或顶层 `{expr}` 插值）→ 经
//!   `wind-cmdbar` 解析求值（`{date()}`/`{calc(code)}`/`{upper(code)}`/`$SS` 数组等）。
//! - 否则 → 旧的简单模板变量展开（$Y/$M/$MM/$D/$DD/$HH/$mm/$ss/$WC/$YC/$MC/$DC/$ts/$tsms）。
//!
//! 命令栏 display 侧只用纯函数（无需宿主服务）；`$CC` 的副作用动作需平台服务（按键/剪贴板/
//! 进程注入），Rust 端平台层尚缺，故当前仅显现 display 候选，动作执行待平台服务补齐。

use chrono::{DateTime, Datelike, Local, Timelike};
use std::collections::HashMap;
use std::path::Path;
use tracing::warn;
use wind_cmdbar::{default_registry, evaluate_phrase, is_cmdbar_grammar, PhraseEval, Services};

/// 一条短语（同 code 下按 weight 降序、position 升序排列）
#[derive(Debug, Clone)]
pub struct PhraseEntry {
    pub text: String,
    pub weight: i32,
    pub position: i32,
}

/// 一条短语命中：展开后的候选文本 + 权重 + 可选命令源。
/// `command_src` 非空表示这是 `$CC` 命令短语（选中时执行动作而非上屏 text），
/// 其值为待重新求值/执行的命令源（如 `$CC("切简繁", ime.toggle("s2t"))`）。
#[derive(Debug, Clone, PartialEq)]
pub struct PhraseHit {
    pub text: String,
    pub weight: i32,
    pub command_src: Option<String>,
}

impl PhraseHit {
    fn plain(text: String, weight: i32) -> Self {
        Self {
            text,
            weight,
            command_src: None,
        }
    }
}

/// 短语层：code → 多条短语
#[derive(Debug, Default)]
pub struct PhraseLayer {
    map: HashMap<String, Vec<PhraseEntry>>,
}

#[derive(serde::Deserialize)]
struct PhrasesFile {
    #[serde(default)]
    phrases: Vec<RawPhrase>,
}

#[derive(serde::Deserialize)]
struct RawPhrase {
    code: String,
    text: String,
    #[serde(default)]
    weight: Option<i32>,
    #[serde(default)]
    position: Option<i32>,
    #[serde(default)]
    platform: Option<String>,
}

impl PhraseLayer {
    /// 从 system.phrases.toml 加载（文件缺失/解析失败 → 空层）
    pub fn load(path: &Path) -> Self {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return Self::default(),
        };
        let parsed: PhrasesFile = match toml::from_str(&content) {
            Ok(p) => p,
            Err(e) => {
                warn!("Parse phrases failed {}: {}", path.display(), e);
                return Self::default();
            }
        };
        let mut map: HashMap<String, Vec<PhraseEntry>> = HashMap::new();
        for r in parsed.phrases {
            // 平台过滤：空/"all"/"windows" 接受
            if let Some(p) = &r.platform {
                let p = p.to_lowercase();
                if !p.is_empty() && p != "all" && p != "windows" {
                    continue;
                }
            }
            map.entry(r.code).or_default().push(PhraseEntry {
                text: r.text,
                weight: r.weight.unwrap_or(1000),
                position: r.position.unwrap_or(0),
            });
        }
        for v in map.values_mut() {
            v.sort_by(|a, b| b.weight.cmp(&a.weight).then(a.position.cmp(&b.position)));
        }
        Self { map }
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 查 code 对应的展开短语；跳过含不支持变量的项。
    pub fn lookup(&self, code: &str) -> Vec<PhraseHit> {
        self.lookup_at(code, Local::now())
    }

    /// 同 lookup，但显式传入时间（便于测试）。
    pub fn lookup_at(&self, code: &str, now: DateTime<Local>) -> Vec<PhraseHit> {
        let entries = match self.map.get(code) {
            Some(e) => e,
            None => return Vec::new(),
        };
        let mut out = Vec::new();
        for e in entries {
            if is_cmdbar_grammar(&e.text) {
                // 命令栏路径：纯函数 display 求值（无服务）。失败跳过并记 WARN，不阻断输入。
                let ctx = PhraseCtx {
                    input: code.to_string(),
                    now,
                };
                match evaluate_phrase(&e.text, &ctx, default_registry()) {
                    // 无动作（literal/template，如 {date()}）→ 显示即上屏文本。
                    Ok(PhraseEval::Single { display, actions }) if actions.is_empty() => {
                        out.push(PhraseHit::plain(display, e.weight))
                    }
                    // $CC 命令短语（有动作）：携带命令源，选中时由 coordinator 执行动作。
                    Ok(PhraseEval::Single { display, .. }) => out.push(PhraseHit {
                        text: display,
                        weight: e.weight,
                        command_src: Some(e.text.clone()),
                    }),
                    Ok(PhraseEval::Array(arr)) => {
                        for el in arr.elements {
                            // 仅显现无动作的字面元素（符号等）；带动作的嵌入 $CC 需元素级源，后续补。
                            if el.actions.is_empty() {
                                out.push(PhraseHit::plain(el.display, e.weight));
                            }
                        }
                    }
                    Err(err) => warn!("cmdbar phrase eval failed ({:?}): {}", e.text, err),
                }
            } else if let Some(text) = expand_template(&e.text, &now) {
                out.push(PhraseHit::plain(text, e.weight));
            }
        }
        out
    }
}

/// 命令栏 display 侧的 [`wind_cmdbar::EvalContext`] 适配器（短语候选生成用）。
/// 仅提供纯函数所需的 input/now/env；交互态（last/clip/sel/app/title）与服务侧留空，
/// 待宿主平台层补齐后由 coordinator 提供完整实现。
struct PhraseCtx {
    input: String,
    now: DateTime<Local>,
}

impl wind_cmdbar::EvalContext for PhraseCtx {
    fn input(&self) -> String {
        self.input.clone()
    }
    fn last(&self, _n: i64) -> String {
        String::new()
    }
    fn clip(&self, _n: i64) -> String {
        String::new()
    }
    fn sel(&self) -> String {
        String::new()
    }
    fn app(&self) -> String {
        String::new()
    }
    fn title(&self) -> String {
        String::new()
    }
    fn env(&self, name: &str) -> String {
        std::env::var(name).unwrap_or_default()
    }
    fn now(&self) -> DateTime<Local> {
        self.now
    }
    fn services(&self) -> Option<&Services> {
        None
    }
}

/// 展开模板字符串；遇到不支持的变量返回 None（该短语项被跳过）。
/// 支持 `$name`、`${name}`，`$$` 转义为字面 `$`。
pub fn expand_template(text: &str, now: &DateTime<Local>) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if bytes[i] == b'$' {
            // $$ → 字面 $
            if i + 1 < text.len() && bytes[i + 1] == b'$' {
                out.push('$');
                i += 2;
                continue;
            }
            // ${name} 或 $name
            let (name, next) = if i + 1 < text.len() && bytes[i + 1] == b'{' {
                let rel = text[i + 2..].find('}')?;
                let close = i + 2 + rel;
                (&text[i + 2..close], close + 1)
            } else {
                let start = i + 1;
                let mut j = start;
                while j < text.len() && bytes[j].is_ascii_alphabetic() {
                    j += 1;
                }
                if j == start {
                    // 孤立的 $，原样输出
                    out.push('$');
                    i += 1;
                    continue;
                }
                (&text[start..j], j)
            };
            let val = expand_var(name, now)?;
            out.push_str(&val);
            i = next;
        } else {
            // 拷贝一个 UTF-8 字符
            let len = utf8_len(bytes[i]);
            out.push_str(&text[i..i + len]);
            i += len;
        }
    }
    Some(out)
}

fn utf8_len(lead: u8) -> usize {
    if lead < 0x80 {
        1
    } else if lead < 0xE0 {
        2
    } else if lead < 0xF0 {
        3
    } else {
        4
    }
}

const CN_DIGITS: [&str; 10] = ["〇", "一", "二", "三", "四", "五", "六", "七", "八", "九"];
const WEEKDAY_CN: [&str; 7] = ["日", "一", "二", "三", "四", "五", "六"];

/// 展开单个变量；不支持的返回 None。
fn expand_var(name: &str, now: &DateTime<Local>) -> Option<String> {
    Some(match name {
        "Y" => now.year().to_string(),
        "M" => now.month().to_string(),
        "MM" => format!("{:02}", now.month()),
        "D" => now.day().to_string(),
        "DD" => format!("{:02}", now.day()),
        "HH" => format!("{:02}", now.hour()),
        "mm" => format!("{:02}", now.minute()),
        "ss" => format!("{:02}", now.second()),
        "WC" => WEEKDAY_CN[now.weekday().num_days_from_sunday() as usize].to_string(),
        "YC" => year_chinese(now.year()),
        "MC" => small_int_chinese(now.month()),
        "DC" => small_int_chinese(now.day()),
        "ts" => now.timestamp().to_string(),
        "tsms" => now.timestamp_millis().to_string(),
        _ => return None,
    })
}

/// 年份逐位中文："2026" → "二〇二六"
fn year_chinese(year: i32) -> String {
    year.to_string()
        .bytes()
        .filter(|b| b.is_ascii_digit())
        .map(|b| CN_DIGITS[(b - b'0') as usize])
        .collect()
}

/// 1~99 的中文读法（月/日用）：6→六，12→十二，25→二十五，20→二十
fn small_int_chinese(n: u32) -> String {
    if n < 10 {
        return CN_DIGITS[n as usize].to_string();
    }
    if n < 20 {
        return format!(
            "十{}",
            if n % 10 == 0 {
                ""
            } else {
                CN_DIGITS[(n % 10) as usize]
            }
        );
    }
    let tens = n / 10;
    let ones = n % 10;
    format!(
        "{}十{}",
        CN_DIGITS[tens as usize],
        if ones == 0 {
            ""
        } else {
            CN_DIGITS[ones as usize]
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixed() -> DateTime<Local> {
        // 2026-06-14 09:05:07 周日
        Local.with_ymd_and_hms(2026, 6, 14, 9, 5, 7).unwrap()
    }

    #[test]
    fn test_expand_date() {
        let now = fixed();
        assert_eq!(
            expand_template("$Y年$M月$D日", &now).unwrap(),
            "2026年6月14日"
        );
        assert_eq!(expand_template("$Y-$MM-$DD", &now).unwrap(), "2026-06-14");
    }

    #[test]
    fn test_expand_time_and_week() {
        let now = fixed();
        assert_eq!(expand_template("$HH:$mm:$ss", &now).unwrap(), "09:05:07");
        assert_eq!(expand_template("星期$WC", &now).unwrap(), "星期日");
    }

    #[test]
    fn test_expand_chinese() {
        let now = fixed();
        assert_eq!(
            expand_template("${YC}年${MC}月${DC}日", &now).unwrap(),
            "二〇二六年六月十四日"
        );
    }

    #[test]
    fn test_escape_and_unsupported() {
        let now = fixed();
        assert_eq!(expand_template("$$5", &now).unwrap(), "$5");
        // 含不支持变量（$AA）→ None
        assert!(expand_template("$AA", &now).is_none());
    }

    #[test]
    fn test_cmdbar_dual_path() {
        // 命令栏语法（含 {expr}）走 cmdbar；其余走简单模板。
        let mut map: HashMap<String, Vec<PhraseEntry>> = HashMap::new();
        map.insert(
            "rq".into(),
            vec![PhraseEntry {
                text: r#"{date("YYYY-MM-DD")}"#.into(),
                weight: 1000,
                position: 0,
            }],
        );
        map.insert(
            "js".into(),
            vec![PhraseEntry {
                text: "{calc(\"1+2*3\")}".into(),
                weight: 900,
                position: 0,
            }],
        );
        map.insert(
            "old".into(),
            vec![PhraseEntry {
                text: "$Y-$MM-$DD".into(),
                weight: 800,
                position: 0,
            }],
        );
        // $CC 命令短语（有动作）：暂不显现（待动作执行通路），避免误上屏 display 标签。
        map.insert(
            "cmd".into(),
            vec![PhraseEntry {
                text: r#"$CC("切简繁", ime.toggle("s2t"))"#.into(),
                weight: 700,
                position: 0,
            }],
        );
        let layer = PhraseLayer { map };
        let now = fixed();
        assert_eq!(
            layer.lookup_at("rq", now),
            vec![PhraseHit::plain("2026-06-14".into(), 1000)]
        );
        assert_eq!(
            layer.lookup_at("js", now),
            vec![PhraseHit::plain("7".into(), 900)]
        );
        // 旧简单模板路径仍工作
        assert_eq!(
            layer.lookup_at("old", now),
            vec![PhraseHit::plain("2026-06-14".into(), 800)]
        );
        // 命令短语：display 为标签，携带命令源（选中时执行动作）。
        let cmd = layer.lookup_at("cmd", now);
        assert_eq!(cmd.len(), 1);
        assert_eq!(cmd[0].text, "切简繁");
        assert_eq!(
            cmd[0].command_src.as_deref(),
            Some(r#"$CC("切简繁", ime.toggle("s2t"))"#)
        );
    }

    #[test]
    fn test_cmdbar_array_phrase_expands() {
        let mut map: HashMap<String, Vec<PhraseEntry>> = HashMap::new();
        map.insert(
            "fh".into(),
            vec![PhraseEntry {
                text: r#"$SS("符号", "（）", "【】")"#.into(),
                weight: 500,
                position: 0,
            }],
        );
        let layer = PhraseLayer { map };
        let got = layer.lookup_at("fh", fixed());
        assert_eq!(
            got,
            vec![
                PhraseHit::plain("（）".into(), 500),
                PhraseHit::plain("【】".into(), 500)
            ]
        );
    }

    #[test]
    fn test_small_int_chinese() {
        assert_eq!(small_int_chinese(6), "六");
        assert_eq!(small_int_chinese(10), "十");
        assert_eq!(small_int_chinese(12), "十二");
        assert_eq!(small_int_chinese(20), "二十");
        assert_eq!(small_int_chinese(25), "二十五");
        assert_eq!(small_int_chinese(31), "三十一");
    }
}
