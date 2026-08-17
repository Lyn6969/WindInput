//! 快捷输入的**用户设置文件**：设置页「导入 / 导出」读写的那份 TOML。
//!
//! ## 与 `system.quick.toml` 的分工
//!
//! ```text
//! system.quick.toml   基表：格式模板与出厂顺序（出厂，或高级用户整份覆盖）
//! 本文件              用户改动：调序 / 停用（+ 将来的自定义条目）
//! ```
//!
//! **只导出改动、不导出完整表**是刻意的：换机导入后，出厂新增的格式仍会自然生效；
//! 若导出完整表，就把当前版本的出厂内容固化进了文件，日后升级新增的格式在导入后
//! **永远不会出现**，而用户完全无从察觉（他只会觉得新版本没带来新写法）。
//!
//! 这与 [`crate::FormatAdjust`] 存稀疏 `(id, position)` 而非完整 id 序列是同一条理由。
//!
//! ## 序列化手写而非 `toml::to_string`
//!
//! 这份文件是**给人看、也能手改**的（与出厂文件同族），需要头部注释与稳定的引号风格，
//! 而 serde 的 TOML 输出带不了注释。解析反过来走 serde（容错与错误信息都更好）。
//! 与 `wind-store` 的 wdict 一样：导出手写、解析用解析器。

use crate::{FormatAdjust, FormatEntry, FormatKind, validate_format_text};

/// 一份用户设置（导入/导出的内存形态）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserSettings {
    /// 按类别的调整。同一 kind 只出现一次。
    pub adjust: Vec<(FormatKind, FormatAdjust)>,
    /// 用户自定义条目。
    ///
    /// P1 阶段尚无存储落点，导入时**如实报告为「已忽略 N 条」**而不是静默丢弃——
    /// 静默丢弃的话，用户从新版本导出、在旧版本导入，会以为自定义格式带过来了。
    pub formats: Vec<FormatEntry>,
}

impl UserSettings {
    pub fn is_empty(&self) -> bool {
        self.formats.is_empty() && self.adjust.iter().all(|(_, a)| a.is_empty())
    }
}

/// 解析结果：设置本体 + **被跳过的条目及原因**。
///
/// 跳过必须能报到用户眼前（导入预览会列出来）。「静默截断」在本仓是明令禁止的——
/// 用户看到「导入成功」却少了两条规则，无从判断是文件坏了还是程序吃了。
#[derive(Debug, Clone, Default)]
pub struct ParseOutcome {
    pub settings: UserSettings,
    pub skipped: Vec<String>,
}

/// 文件头注释。导出的文件应当自解释：用户过几个月打开它还能看懂。
const HEADER: &str = "\
# WindInput 快捷输入 · 用户设置
#
# 本文件只记录**你的改动**（调序 / 停用 / 自定义条目），不含出厂格式表本身。
# 因此升级到新版本后，出厂新增的写法照样会出现——导出完整表就没有这个好处了。
#
# 格式模板与出厂顺序在 system.quick.toml，与本文件各管一段，互不覆盖。
# 可在设置页「词库管理 → 快捷输入」里导入本文件，也可手工编辑后再导入。
";

/// 序列化为 TOML 文本。
pub fn serialize_user_settings(s: &UserSettings) -> String {
    let mut out = String::from(HEADER);
    if !s.formats.is_empty() {
        out.push_str("\n# 自定义条目\n");
        for e in &s.formats {
            out.push_str("\n[[formats]]\n");
            out.push_str(&format!("id = {}\n", toml_string(&e.id)));
            out.push_str(&format!("kind = {}\n", toml_string(e.kind.as_str())));
            out.push_str(&format!("text = {}\n", toml_string(&e.text)));
            out.push_str(&format!("position = {}\n", e.position));
        }
    }
    // 只写非空的类别：空记录写出去除了占地方没有别的作用。
    let adjust: Vec<_> = s.adjust.iter().filter(|(_, a)| !a.is_empty()).collect();
    if !adjust.is_empty() {
        out.push_str("\n# 调序与停用（position 是该类候选内的 0-based 下标）\n");
        for (kind, a) in adjust {
            out.push_str("\n[[adjust]]\n");
            out.push_str(&format!("kind = {}\n", toml_string(kind.as_str())));
            if !a.moved.is_empty() {
                // 顺序即 LIFO：第一条是最新的，导入时必须逆序重放（见 `apply` 侧注释）。
                let items: Vec<String> = a
                    .moved
                    .iter()
                    .map(|(id, pos)| format!("{{ id = {}, position = {} }}", toml_string(id), pos))
                    .collect();
                out.push_str(&format!("moved = [{}]\n", items.join(", ")));
            }
            if !a.disabled.is_empty() {
                let items: Vec<String> = a.disabled.iter().map(|d| toml_string(d)).collect();
                out.push_str(&format!("disabled = [{}]\n", items.join(", ")));
            }
        }
    }
    out
}

/// TOML 字符串字面量。
///
/// ⚠️ 优先用单引号字面量（与出厂文件风格一致、无需转义反斜杠），但模板里**确实会出现
/// 单引号**——`{amt(unit='圆')}` 就是出厂就有的写法，而 TOML 的单引号字面量里
/// 无法转义单引号。故含单引号时改用双引号并转义。
///
/// 少了这道分支，含函数参数的自定义条目导出后就是一份语法错误的文件，且导出时不报错，
/// 只在导入时炸。
fn toml_string(s: &str) -> String {
    if s.contains('\'') {
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        format!("'{s}'")
    }
}

/// 解析用户设置文件。整份语法错误返回 `Err`；单条非法只跳过该条并记入
/// [`ParseOutcome::skipped`]。
pub fn parse_user_settings(text: &str) -> Result<ParseOutcome, toml::de::Error> {
    let raw: RawFile = toml::from_str(text)?;
    let mut out = ParseOutcome::default();

    for r in raw.formats {
        // id 先查：它是后续所有报错信息里的定位手段，缺了的话那些报错会写成
        // 「条目 ：未知类别」这种没头没尾的样子。
        if r.id.trim().is_empty() {
            out.skipped.push("有一条自定义条目缺 id".to_string());
            continue;
        }
        let Some(kind) = FormatKind::parse(&r.kind) else {
            out.skipped
                .push(format!("条目 {}：未知类别 {}", r.id, r.kind));
            continue;
        };
        if let Err(e) = validate_format_text(kind, &r.text) {
            out.skipped.push(format!("条目 {}：{}", r.id, e));
            continue;
        }
        out.settings.formats.push(FormatEntry {
            id: r.id,
            kind,
            text: r.text,
            position: r.position.unwrap_or(0),
        });
    }

    for r in raw.adjust {
        let Some(kind) = FormatKind::parse(&r.kind) else {
            out.skipped.push(format!("调整段：未知类别 {}", r.kind));
            continue;
        };
        // 同一类别写了两段：合并而不是后者顶替前者——两段都是用户的意图，
        // 丢掉一段属于静默截断。
        let entry = match out.settings.adjust.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, a)) => a,
            None => {
                out.settings.adjust.push((kind, FormatAdjust::default()));
                &mut out.settings.adjust.last_mut().expect("刚 push").1
            }
        };
        for m in r.moved {
            if m.id.trim().is_empty() {
                out.skipped
                    .push(format!("{} 类的一条移动规则缺 id", kind.as_str()));
                continue;
            }
            entry.moved.push((m.id, m.position));
        }
        for d in r.disabled {
            if d.trim().is_empty() {
                continue;
            }
            entry.disabled.push(d);
        }
    }
    Ok(out)
}

#[derive(serde::Deserialize)]
struct RawFile {
    #[serde(default)]
    formats: Vec<RawFormat>,
    #[serde(default)]
    adjust: Vec<RawAdjust>,
}

#[derive(serde::Deserialize)]
struct RawFormat {
    #[serde(default)]
    id: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    position: Option<i32>,
}

#[derive(serde::Deserialize)]
struct RawAdjust {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    moved: Vec<RawMove>,
    #[serde(default)]
    disabled: Vec<String>,
}

#[derive(serde::Deserialize)]
struct RawMove {
    #[serde(default)]
    id: String,
    #[serde(default)]
    position: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> UserSettings {
        UserSettings {
            adjust: vec![
                (
                    FormatKind::Date,
                    FormatAdjust {
                        moved: vec![("date.lunar".into(), 0), ("date.iso".into(), 2)],
                        disabled: vec!["date.basic".into()],
                    },
                ),
                (
                    FormatKind::Number,
                    FormatAdjust {
                        moved: vec![],
                        disabled: vec!["number.digits".into()],
                    },
                ),
            ],
            formats: vec![],
        }
    }

    /// ★★ 往返是这个模块的全部职责：导出的文件必须能被自己读回来，且**一字不差**。
    #[test]
    fn roundtrip_preserves_everything() {
        let s = sample();
        let text = serialize_user_settings(&s);
        let back = parse_user_settings(&text).expect("导出的文件必须能解析");
        assert!(back.skipped.is_empty(), "自产文件不该有跳过项");
        assert_eq!(back.settings, s);
    }

    /// ★★ `moved` 的**顺序**必须原样保留：它是 LIFO 列表（index 0 = 最新 = 优先级最高），
    /// 顺序反了，用户导入后会发现「我最后调的那条被更早的调整顶掉了」。
    #[test]
    fn moved_order_survives_roundtrip() {
        let s = sample();
        let text = serialize_user_settings(&s);
        let back = parse_user_settings(&text).unwrap();
        let (_, a) = back
            .settings
            .adjust
            .iter()
            .find(|(k, _)| *k == FormatKind::Date)
            .unwrap();
        assert_eq!(a.moved[0].0, "date.lunar", "最新的仍在队首");
        assert_eq!(a.moved[1].0, "date.iso");
    }

    /// ⚠️ 模板含单引号（`{amt(unit='圆')}` 是出厂就有的写法）时必须改用双引号，
    /// 否则导出的文件是语法错误的 TOML，而导出时不会报错，只在导入时炸。
    #[test]
    fn single_quote_in_template_survives_roundtrip() {
        let s = UserSettings {
            adjust: vec![],
            formats: vec![FormatEntry {
                id: "number.yuan".into(),
                kind: FormatKind::Number,
                text: "{amt(unit='圆')}".into(),
                position: 9,
            }],
        };
        let text = serialize_user_settings(&s);
        let back = parse_user_settings(&text).expect("含单引号的模板必须能解析回来");
        assert_eq!(back.settings.formats[0].text, "{amt(unit='圆')}");
    }

    /// 空设置导出后仍是一份合法（只有注释）的文件，解析回来为空。
    #[test]
    fn empty_settings_export_is_valid_toml() {
        let text = serialize_user_settings(&UserSettings::default());
        let back = parse_user_settings(&text).unwrap();
        assert!(back.settings.is_empty());
    }

    /// 全空的 adjust 段不写进文件（写了也没用，只让人以为有改动）。
    #[test]
    fn empty_adjust_sections_are_omitted() {
        let s = UserSettings {
            adjust: vec![(FormatKind::Date, FormatAdjust::default())],
            formats: vec![],
        };
        let text = serialize_user_settings(&s);
        assert!(!text.contains("[[adjust]]"), "空调整不该写出来:\n{text}");
    }

    /// 未知类别与非法模板**逐条跳过并给出原因**，不是整份失败、也不是静默丢弃。
    #[test]
    fn bad_entries_are_reported_not_swallowed() {
        let text = r#"
[[formats]]
id = 'x.weather'
kind = 'weather'
text = '$Y'

[[formats]]
id = 'ym.bad'
kind = 'year_month'
text = '$Y年$M月$D日'

[[adjust]]
kind = 'nope'
disabled = ['a']

[[adjust]]
kind = 'date'
disabled = ['date.basic']
"#;
        let out = parse_user_settings(text).unwrap();
        assert_eq!(
            out.skipped.len(),
            3,
            "两条坏条目 + 一个坏类别: {:?}",
            out.skipped
        );
        assert!(out.skipped.iter().any(|s| s.contains("weather")));
        // year_month 不支持 $D——白名单在 kind 上，这正是最容易「设了没反应」的地方
        assert!(
            out.skipped.iter().any(|s| s.contains("$D")),
            "{:?}",
            out.skipped
        );
        // 好的那段照常收下
        assert_eq!(out.settings.adjust.len(), 1);
        assert_eq!(out.settings.adjust[0].0, FormatKind::Date);
    }

    /// 同一类别写两段：合并，不是后者顶替前者（丢一段就是静默截断）。
    #[test]
    fn duplicate_kind_sections_merge() {
        let text = r#"
[[adjust]]
kind = 'date'
disabled = ['a']

[[adjust]]
kind = 'date'
disabled = ['b']
"#;
        let out = parse_user_settings(text).unwrap();
        assert_eq!(out.settings.adjust.len(), 1);
        assert_eq!(out.settings.adjust[0].1.disabled, vec!["a", "b"]);
    }

    /// 整份语法错误才返回 Err（调用方据此提示「这不是一份有效文件」）。
    #[test]
    fn syntax_error_is_an_error() {
        assert!(parse_user_settings("[[adjust]]\nkind = ").is_err());
    }

    /// 空文件 / 不相关的 TOML 解析为空设置而非报错——用户选错文件时，
    /// 「导入了 0 条」比「解析失败」更容易看懂。
    #[test]
    fn unrelated_toml_parses_as_empty() {
        let out = parse_user_settings("[something]\nkey = 1\n").unwrap();
        assert!(out.settings.is_empty());
    }
}
