//! 调色板解析：把 colors 段解析为「名称 → RGBA」具体色值。
//!
//! 与 Go 版本 `wind_input/pkg/theme/palette.go` 对齐。
//! 处理 `${var}` 引用（递归 + 环检测）、{light,dark} 变体、#RRGGBB[AA] 十六进制。

use serde_yaml::Value;
use std::collections::{HashMap, HashSet};

/// 颜色 [R, G, B, A]，与 UI 缓冲约定一致。
pub type Rgba = [u8; 4];

/// 解析 #RRGGBB 或 #RRGGBBAA。
pub fn parse_hex(s: &str) -> Option<Rgba> {
    let s = s.trim().trim_start_matches('#');
    let h = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
    match s.len() {
        6 => Some([h(0)?, h(2)?, h(4)?, 255]),
        8 => Some([h(0)?, h(2)?, h(4)?, h(6)?]),
        _ => None,
    }
}

/// 解析整个调色板：colors 段（Mapping）→ {名称: Rgba}。
/// is_dark 选择 {light,dark} 变体。derive 等非颜色项被忽略。
pub fn resolve_palette(colors: Option<&Value>, is_dark: bool) -> HashMap<String, Rgba> {
    let mut out = HashMap::new();
    let map = match colors.and_then(|c| c.as_mapping()) {
        Some(m) => m,
        None => return out,
    };
    let mut visiting = HashSet::new();
    let names: Vec<String> = map
        .keys()
        .filter_map(|k| k.as_str().map(|s| s.to_string()))
        .collect();
    for name in names {
        resolve_name(&name, map, is_dark, &mut out, &mut visiting);
    }
    out
}

/// 按名解析（递归 + 记忆 + 环检测）。
fn resolve_name(
    name: &str,
    map: &serde_yaml::Mapping,
    is_dark: bool,
    out: &mut HashMap<String, Rgba>,
    visiting: &mut HashSet<String>,
) -> Option<Rgba> {
    if let Some(c) = out.get(name) {
        return Some(*c);
    }
    if visiting.contains(name) {
        return None; // 环
    }
    visiting.insert(name.to_string());
    let v = map.get(Value::from(name))?;
    let color = resolve_value(v, map, is_dark, out, visiting);
    visiting.remove(name);
    if let Some(c) = color {
        out.insert(name.to_string(), c);
    }
    color
}

/// 解析一个颜色值（字符串 hex / `${var}` / {light,dark}）。
fn resolve_value(
    v: &Value,
    map: &serde_yaml::Mapping,
    is_dark: bool,
    out: &mut HashMap<String, Rgba>,
    visiting: &mut HashSet<String>,
) -> Option<Rgba> {
    match v {
        Value::String(s) => resolve_str(s, map, is_dark, out, visiting),
        Value::Mapping(m) => {
            // {light: .., dark: ..} 变体
            let key = if is_dark { "dark" } else { "light" };
            if let Some(inner) = m.get(Value::from(key)) {
                resolve_value(inner, map, is_dark, out, visiting)
            } else {
                None // derive 等非颜色映射
            }
        }
        _ => None,
    }
}

fn resolve_str(
    s: &str,
    map: &serde_yaml::Mapping,
    is_dark: bool,
    out: &mut HashMap<String, Rgba>,
    visiting: &mut HashSet<String>,
) -> Option<Rgba> {
    let s = s.trim();
    if let Some(var) = s.strip_prefix("${").and_then(|x| x.strip_suffix('}')) {
        resolve_name(var, map, is_dark, out, visiting)
    } else {
        parse_hex(s)
    }
}

/// 把 views 中的颜色 token（"${name}" 或 "#hex" 或 {light,dark}）按已解析调色板转为 Rgba。
pub fn color_token(v: &Value, palette: &HashMap<String, Rgba>, is_dark: bool) -> Option<Rgba> {
    match v {
        Value::String(s) => {
            let s = s.trim();
            if let Some(var) = s.strip_prefix("${").and_then(|x| x.strip_suffix('}')) {
                palette.get(var).copied()
            } else {
                parse_hex(s)
            }
        }
        Value::Mapping(m) => {
            let key = if is_dark { "dark" } else { "light" };
            m.get(Value::from(key))
                .and_then(|inner| color_token(inner, palette, is_dark))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hex() {
        assert_eq!(parse_hex("#FF8040"), Some([255, 128, 64, 255]));
        assert_eq!(parse_hex("#00000080"), Some([0, 0, 0, 128]));
        assert_eq!(parse_hex("nope"), None);
    }

    #[test]
    fn test_palette_var_and_lightdark() {
        let yaml = r##"
primary: "#4285F4"
accent: "${primary}"
bg: {light: "#FFFFFF", dark: "#2D2D2D"}
selection_text: "${text}"
text: {light: "#1E1E1E", dark: "#E0E0E0"}
"##;
        let v: Value = serde_yaml::from_str(yaml).unwrap();
        let light = resolve_palette(Some(&v), false);
        assert_eq!(light["primary"], [0x42, 0x85, 0xF4, 255]);
        assert_eq!(light["accent"], [0x42, 0x85, 0xF4, 255]); // ${primary}
        assert_eq!(light["bg"], [255, 255, 255, 255]);
        assert_eq!(light["selection_text"], [0x1E, 0x1E, 0x1E, 255]); // ${text}

        let dark = resolve_palette(Some(&v), true);
        assert_eq!(dark["bg"], [0x2D, 0x2D, 0x2D, 255]);
        assert_eq!(dark["text"], [0xE0, 0xE0, 0xE0, 255]);
    }
}
