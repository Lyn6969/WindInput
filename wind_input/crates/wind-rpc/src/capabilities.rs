//! Capability descriptor 生成：从 config_schema::REGISTRY + 系统预置配置
//! （Config::default ⊕ data/config.toml，不含用户层）动态生成 core 能力清单，
//! 经 system.capabilities 暴露给 setting。取代退役的 manifest.toml / manifest.rs。

use std::collections::BTreeMap;
use std::path::Path;

use wind_config::Config;
use wind_config::config_schema::{self, FieldType};

/// FieldType → capability type 字符串。
fn type_name(ty: FieldType) -> &'static str {
    match ty {
        FieldType::Bool => "bool",
        FieldType::Int => "int",
        FieldType::Float => "float",
        FieldType::Str => "str",
        FieldType::Enum(_) => "enum",
        FieldType::StrList => "strlist",
        FieldType::Map => "map",
        FieldType::StructList => "structlist",
    }
}

/// 生成 capability descriptor JSON。
/// `data_dir` 用于读取系统预置 `config.toml`（None 时仅 L1 代码默认）。
pub fn generate(data_dir: Option<&Path>) -> anyhow::Result<serde_json::Value> {
    let preset = Config::system_preset_value(data_dir)?;
    let leaves: BTreeMap<String, toml::Value> =
        config_schema::leaf_entries(&preset).into_iter().collect();

    let mut config_keys = Vec::new();
    for f in config_schema::registry() {
        let mut entry = serde_json::Map::new();
        entry.insert("key".into(), serde_json::Value::String(f.key.to_string()));
        entry.insert(
            "type".into(),
            serde_json::Value::String(type_name(f.ty).to_string()),
        );
        if let FieldType::Enum(allowed) = f.ty {
            entry.insert(
                "values".into(),
                serde_json::Value::Array(
                    allowed
                        .iter()
                        .map(|s| serde_json::Value::String(s.to_string()))
                        .collect(),
                ),
            );
        }
        let default = match leaves.get(f.key) {
            Some(v) => serde_json::to_value(v)?,
            None => serde_json::Value::Null,
        };
        entry.insert("default".into(), default);
        config_keys.push(serde_json::Value::Object(entry));
    }

    Ok(serde_json::json!({
        "appVersion": crate::APP_VERSION,
        "configKeys": config_keys,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_data() -> &'static Path {
        Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../data"))
    }

    #[test]
    fn every_registry_key_present_with_type() {
        let caps = generate(None).expect("生成 capability");
        let arr = caps["configKeys"].as_array().expect("configKeys 应为数组");
        // 每个 entry 的 type 非空、default 非 null：守护「default 无空洞」——
        // 若将来某 Map 配了非空默认值，leaf_entries 会下钻导致该键 default 静默变 null，此处拦截。
        for e in arr {
            let key = e["key"].as_str().expect("key 应为字符串");
            assert!(
                e["type"].as_str().map(|s| !s.is_empty()).unwrap_or(false),
                "{key}: type 应为非空字符串"
            );
            assert!(!e["default"].is_null(), "{key}: default 不应为 null");
        }
        let keys: std::collections::BTreeSet<&str> =
            arr.iter().map(|e| e["key"].as_str().unwrap()).collect();
        for f in config_schema::registry() {
            assert!(
                keys.contains(f.key),
                "capability 缺 registry key: {}",
                f.key
            );
        }
        assert_eq!(
            keys.len(),
            config_schema::registry().len(),
            "capability 键数应与 registry 一致"
        );
    }

    #[test]
    fn enum_fields_carry_values() {
        let caps = generate(None).expect("生成 capability");
        let entry = caps["configKeys"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["key"] == "ui.candidate.layout")
            .expect("应含 ui.candidate.layout");
        assert_eq!(entry["type"], "enum");
        assert_eq!(
            entry["values"],
            serde_json::json!(["horizontal", "vertical"])
        );
    }

    #[test]
    fn defaults_reflect_preset_overrides() {
        let caps = generate(Some(repo_data())).expect("生成 capability");
        let arr = caps["configKeys"].as_array().unwrap();
        let find = |k: &str| {
            arr.iter()
                .find(|e| e["key"] == k)
                .unwrap_or_else(|| panic!("缺 key {k}"))
                .clone()
        };
        // L2 预置覆盖
        assert_eq!(
            find("compat.host_render_processes")["default"],
            serde_json::json!(["SearchHost.exe"])
        );
        // 普通默认值
        assert_eq!(
            find("ui.candidate.per_page")["default"],
            serde_json::json!(7)
        );
    }
}
