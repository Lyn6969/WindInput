//! 主题原始加载 + base 单链继承深合并
//!
//! 与 Go 版本 `wind_input/pkg/theme/theme.go` 对齐（v3 schema）。
//! 用 serde_yaml::Value 作中间表示：base 提供全量，派生主题深合并覆盖。

use crate::schema::Theme;
use serde_yaml::Value;
use std::path::Path;

/// 加载并 base 深合并主题，解析为类型化 `Theme`（未求值的原始 schema）。
/// 合并在 Value 层完成（先合并后类型化），未知字段忽略（前向兼容）。
pub fn load_typed(themes_dir: &Path, name: &str) -> anyhow::Result<Theme> {
    let merged = load_merged(themes_dir, name, 0)?;
    let theme: Theme = serde_yaml::from_value(merged)
        .map_err(|e| anyhow::anyhow!("type theme {}: {}", name, e))?;
    Ok(theme)
}

/// 读取 themes_dir/<name>/theme.yaml 并按 base 链深合并（base 在下、派生在上）。
/// 防御循环继承（最多 8 层）。
pub fn load_merged(themes_dir: &Path, name: &str, depth: usize) -> anyhow::Result<Value> {
    if depth > 8 {
        anyhow::bail!("theme base chain too deep (cycle?) at {}", name);
    }
    let path = themes_dir.join(name).join("theme.yaml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("read theme {}: {}", path.display(), e))?;
    let value: Value = serde_yaml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parse theme {}: {}", path.display(), e))?;

    // base 继承：先加载 base，再用本主题覆盖
    if let Some(base_name) = value.get("base").and_then(|b| b.as_str()) {
        if !base_name.is_empty() && base_name != name {
            let base = load_merged(themes_dir, base_name, depth + 1)?;
            return Ok(merge(base, value));
        }
    }
    Ok(value)
}

/// 深合并：over 覆盖 base。映射递归合并；其余（标量/序列）由 over 覆盖。
pub fn merge(base: Value, over: Value) -> Value {
    match (base, over) {
        (Value::Mapping(mut b), Value::Mapping(o)) => {
            for (k, ov) in o {
                let merged = match b.remove(&k) {
                    Some(bv) => merge(bv, ov),
                    None => ov,
                };
                b.insert(k, merged);
            }
            Value::Mapping(b)
        }
        // 非映射：over 优先
        (_, over) => over,
    }
}
