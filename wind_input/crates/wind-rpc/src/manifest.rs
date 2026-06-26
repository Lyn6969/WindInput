//! 统一声明式设置清单加载：解析 data/settings/manifest.toml，
//! 组装为 system.manifest 的返回 JSON（注入运行时 app/engine/variant）。
//!
//! 从 wind-webapi/manifest.rs 原样迁移（仅 APP_VERSION 改为引用本 crate）。

use std::path::PathBuf;

pub fn load(variant: &str) -> anyhow::Result<serde_json::Value> {
    let path = locate()?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("读取 manifest 失败 {}: {}", path.display(), e))?;
    let toml_val: toml::Value = toml::from_str(&text)?;
    let raw = serde_json::to_value(toml_val)?;

    let version = raw
        .get("meta")
        .and_then(|m| m.get("version"))
        .cloned()
        .unwrap_or(serde_json::json!(1));
    let groups = raw.get("groups").cloned().unwrap_or(serde_json::json!([]));
    let items = raw.get("items").cloned().unwrap_or(serde_json::json!([]));
    let features = raw
        .get("features")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    Ok(serde_json::json!({
        "manifest": version,
        "app": crate::APP_VERSION,
        "engine": crate::APP_VERSION,
        "variant": variant,
        "groups": groups,
        "items": items,
        "features": features,
    }))
}

/// 定位 manifest.toml：env 覆盖 → exe 同级 data → 源码树 data（开发期）。
fn locate() -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var("WIND_MANIFEST_PATH") {
        return Ok(PathBuf::from(p));
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(d) = wind_config::Config::data_dir() {
        candidates.push(d.join("settings/manifest.toml"));
    }
    candidates.push(PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../data/settings/manifest.toml"
    )));
    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    anyhow::bail!("未找到 settings/manifest.toml（尝试 {:?}）", candidates)
}

#[cfg(test)]
mod schema_binding {
    //! manifest（展示层）↔ config_schema registry（类型/校验层）一致性绑定。
    //! 三层真相源（struct/registry/manifest）靠这些测试两两锁定，杜绝漂移。
    use wind_config::config_schema::{FieldType, field};

    fn items() -> Vec<serde_json::Value> {
        let m = super::load("test").expect("加载 manifest");
        m.get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    }

    /// 每个 manifest item.key 必须在 registry 登记——否则写入会被 serde 静默丢弃。
    #[test]
    fn every_manifest_key_is_registered() {
        let unknown: Vec<String> = items()
            .iter()
            .filter_map(|it| it.get("key").and_then(|v| v.as_str()))
            .filter(|k| field(k).is_none())
            .map(|k| k.to_string())
            .collect();
        assert!(
            unknown.is_empty(),
            "manifest 含 {} 个未在 config_schema registry 登记的 key（写入会被静默丢弃，设置不生效）: {:?}",
            unknown.len(),
            unknown
        );
    }

    /// manifest 控件类型与 registry 字段类型相容。
    #[test]
    fn manifest_widget_type_matches_registry_type() {
        let mut bad = Vec::new();
        for it in items() {
            let Some(key) = it.get("key").and_then(|v| v.as_str()) else {
                continue;
            };
            // key 覆盖性由 every_manifest_key_is_registered 保证；此处仅校验已登记项。
            let Some(f) = field(key) else { continue };
            let widget = it.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let ok = match widget {
                "toggle" => matches!(f.ty, FieldType::Bool),
                "slider" | "number" => matches!(f.ty, FieldType::Int | FieldType::Float),
                "string" | "select" => matches!(f.ty, FieldType::Str | FieldType::Enum(_)),
                other => {
                    bad.push(format!("{key}: 未知控件类型 {other}"));
                    continue;
                }
            };
            if !ok {
                bad.push(format!("{key}: 控件={widget} 与 registry {:?} 不符", f.ty));
            }
        }
        assert!(
            bad.is_empty(),
            "manifest 控件与 registry 类型不符:\n{}",
            bad.join("\n")
        );
    }

    /// select 选项值必须 ⊆ registry 的 Enum 合法值（registry 为 Str 时不校验）。
    #[test]
    fn manifest_select_options_subset_of_registry_enum() {
        let mut bad = Vec::new();
        for it in items() {
            let Some(key) = it.get("key").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(f) = field(key) else { continue };
            let FieldType::Enum(allowed) = f.ty else {
                continue;
            };
            if let Some(opts) = it.get("options").and_then(|v| v.as_array()) {
                for o in opts {
                    if let Some(val) = o.get("value").and_then(|v| v.as_str()) {
                        if !allowed.contains(&val) {
                            bad.push(format!(
                                "{key}: 选项 {val:?} 不在 registry Enum {allowed:?}"
                            ));
                        }
                    }
                }
            }
        }
        assert!(
            bad.is_empty(),
            "manifest select 选项越出 registry Enum:\n{}",
            bad.join("\n")
        );
    }
}
