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
