//! JSON-RPC 分发：system.* / config.*（方法名与 web 前端 contract.ts 一致）。

use serde_json::{Value, json};
use wind_config::Config;
use wind_ipc::rpc::{Request, Response};

use crate::session::WebState;

pub async fn dispatch(state: &WebState, req: Request) -> Response {
    match handle(state, &req.method, &req.params) {
        Ok(v) => Response::success(req.id, v),
        Err(e) => Response::error(req.id, e.to_string()),
    }
}

fn handle(state: &WebState, method: &str, params: &Value) -> anyhow::Result<Value> {
    match method {
        "system.status" => Ok(json!({
            "running": true,
            "mode": if state.status.is_chinese_mode() { "chinese" } else { "english" },
        })),
        // 字段对齐 web 的 SystemInfo {version, platform, dataDir, running}；其余为附带字段（web 忽略）。
        "system.info" => Ok(json!({
            "version": crate::APP_VERSION,
            "platform": platform_name(),
            "dataDir": Config::data_dir().map(|p| p.display().to_string()).unwrap_or_default(),
            "running": true,
            "engine": crate::APP_VERSION,
            "variant": state.variant,
            "activeSchema": state.status.active_schema_id(),
        })),
        "system.manifest" => Ok(state.manifest.clone()),
        // 本机字体枚举（平台能力经 CoreStatus 注入；dev server 默认空表）。
        "system.fonts" => Ok(Value::Array(
            state.status.fonts().into_iter().map(|f| json!({ "family": f })).collect(),
        )),
        "system.notifyReload" => Ok(json!({ "ok": true })),
        "config.get" => {
            let cfg = Config::load(Config::data_dir().as_deref())?;
            Ok(serde_json::to_value(cfg)?)
        }
        "config.getDefaults" => {
            // 全部顶层字段带 #[serde(default)]，空 TOML 即得纯代码默认配置。
            let cfg: Config = toml::from_str("")?;
            Ok(serde_json::to_value(cfg)?)
        }
        "config.setItems" => set_items(state, params),
        "config.reload" => Ok(json!({ "ok": true })),
        // schema/dict/temp/freq/shadow/stats/theme/phrase 等数据类 RPC 转发到宿主 core。
        _ => state.status.data_rpc(method, params),
    }
}

/// 平台名，对齐 web 约定（"windows" | "darwin" | "linux"）。
fn platform_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

fn set_items(state: &WebState, params: &Value) -> anyhow::Result<Value> {
    let items = params
        .get("items")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("invalid_params: items missing"))?;
    for it in items {
        let key = it
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("invalid_params: item.key missing"))?;
        let value = it.get("value").cloned().unwrap_or(Value::Null);
        let parts: Vec<&str> = key.split('.').collect();
        let toml_val = json_to_toml(&value)?;
        Config::set_user_value(&parts, toml_val)?;
    }
    // 落盘后即时热重载：轻量字段立即生效，引擎结构性变更则 needsRestart=true。
    let needs_restart = state.status.apply_config();
    Ok(json!({ "needsRestart": needs_restart }))
}

/// JSON 标量/容器 → toml::Value（用于写用户层配置）。
fn json_to_toml(v: &Value) -> anyhow::Result<toml::Value> {
    Ok(match v {
        Value::Null => anyhow::bail!("不支持 null 配置值"),
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                toml::Value::Float(f)
            } else {
                anyhow::bail!("不支持的数字 {}", n)
            }
        }
        Value::String(s) => toml::Value::String(s.clone()),
        Value::Array(a) => {
            let mut out = Vec::with_capacity(a.len());
            for e in a {
                out.push(json_to_toml(e)?);
            }
            toml::Value::Array(out)
        }
        Value::Object(o) => {
            let mut t = toml::map::Map::new();
            for (k, val) in o {
                t.insert(k.clone(), json_to_toml(val)?);
            }
            toml::Value::Table(t)
        }
    })
}
