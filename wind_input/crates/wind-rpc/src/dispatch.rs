//! 传输无关的 JSON-RPC 分发：system.* / config.*（方法名与 web 前端 contract.ts 一致），
//! 未知/数据类方法转发到注入的 [`CoreRpc`]。
//!
//! 从 wind-webapi/rpc.rs 迁移，去掉 axum/WebState 依赖：改用 [`DispatchState`]
//! 持有 manifest 缓存 + variant + 注入的 core 实现，返回 wind-ipc 的 [`Response`]。

use std::sync::Arc;

use serde_json::{Value, json};
use wind_config::Config;
use wind_ipc::rpc::{Request, Response};

pub(crate) const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 由宿主（service）注入的运行时状态来源（传输无关）。
///
/// 取代原 wind-webapi 的 `CoreStatus`：去掉浏览器授权相关（token/open_url），
/// 仅保留 dispatch 所需的状态查询 + 数据类 RPC 转发 + 字体枚举。
pub trait CoreRpc: Send + Sync {
    fn is_chinese_mode(&self) -> bool;
    fn active_schema_id(&self) -> String;
    /// config.setItems 落盘后重新加载并即时应用用户配置；返回是否仍需重启才能完全生效。
    /// 默认实现保守返回 true（未接入热重载的宿主，如测试 stub）。
    fn apply_config(&self) -> bool {
        true
    }
    /// 数据类 RPC（schema/dict/temp/freq/shadow/stats/theme/phrase）转发到宿主 core 实现。
    /// 默认未接入（测试 stub）：返回 unknown method 错误。
    fn data_rpc(&self, method: &str, _params: &Value) -> anyhow::Result<Value> {
        anyhow::bail!("unknown method: {}", method)
    }
    /// 本机字体枚举（system.fonts）：(family, display_name)。默认空表（无平台字体能力）。
    fn fonts(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// 分发器共享状态：清单缓存 + 变体 + 注入的 core 实现。
pub struct DispatchState {
    pub(crate) core: Arc<dyn CoreRpc>,
    pub(crate) variant: &'static str,
    pub(crate) manifest: Value,
    /// 配置变更事件接收方（setItems/reload 后广播）。
    pub(crate) events: crate::events::EventSink,
}

impl DispatchState {
    pub fn new(core: Arc<dyn CoreRpc>, variant: &'static str) -> anyhow::Result<Self> {
        Self::with_events(core, variant, crate::events::EventSink::disconnected())
    }

    /// 构造并接入事件广播通道（config/dict 变更经此推送）。
    pub fn with_events(
        core: Arc<dyn CoreRpc>,
        variant: &'static str,
        events: crate::events::EventSink,
    ) -> anyhow::Result<Self> {
        let manifest = crate::manifest::load(variant)?;
        Ok(Self {
            core,
            variant,
            manifest,
            events,
        })
    }
}

/// 分发一条请求，返回 JSON-RPC 响应（成功/错误均为 200 等价的 Response）。
pub fn dispatch(state: &DispatchState, req: Request) -> Response {
    match handle(state, &req.method, &req.params) {
        Ok(v) => Response::success(req.id, v),
        Err(e) => Response::error(req.id, e.to_string()),
    }
}

fn handle(state: &DispatchState, method: &str, params: &Value) -> anyhow::Result<Value> {
    match method {
        "system.status" => Ok(json!({
            "running": true,
            "mode": if state.core.is_chinese_mode() { "chinese" } else { "english" },
        })),
        // 字段对齐 web 的 SystemInfo {version, platform, dataDir, running}；其余为附带字段（web 忽略）。
        "system.info" => Ok(json!({
            "version": APP_VERSION,
            "platform": platform_name(),
            "dataDir": Config::data_dir().map(|p| p.display().to_string()).unwrap_or_default(),
            "running": true,
            "engine": APP_VERSION,
            "variant": state.variant,
            "activeSchema": state.core.active_schema_id(),
        })),
        "system.manifest" => Ok(state.manifest.clone()),
        // 本机字体枚举（平台能力经 CoreRpc 注入；默认空表）。
        "system.fonts" => Ok(Value::Array(
            state
                .core
                .fonts()
                .into_iter()
                .map(|(family, display_name)| json!({ "family": family, "display_name": display_name }))
                .collect(),
        )),
        "system.notifyReload" => Ok(json!({ "ok": true })),
        "config.get" => {
            let cfg = Config::load(Config::data_dir().as_deref())?;
            Ok(serde_json::to_value(cfg)?)
        }
        "config.getDefaults" => {
            // 全部顶层字段带 #[serde(default)]，空 TOML 即得纯代码默认配置。
            let cfg: wind_config::Config = toml::from_str("")?;
            Ok(serde_json::to_value(cfg)?)
        }
        "config.setItems" => set_items(state, params),
        // 配置字段注册表（key+type+enum options）：CLI/设置端据此校验与补全。
        "config.schema" => Ok(schema_json()),
        // 单字段当前值（含三层合并）：补 config.get 只能整份的缺口。
        "config.getItem" => get_item(params),
        "config.reload" => {
            // 变更通知：广播一个 config 变更事件，供订阅者（TSF/UI）刷新。
            state
                .events
                .emit_config_changed(json!({ "reason": "reload" }));
            Ok(json!({ "ok": true }))
        }
        // schema/dict/temp/freq/shadow/stats/theme/phrase 等数据类 RPC 转发到宿主 core。
        _ => state.core.data_rpc(method, params),
    }
}

/// 平台名，对齐 web 约定（"windows" | "darwin" | "linux"）。
fn platform_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

fn set_items(state: &DispatchState, params: &Value) -> anyhow::Result<Value> {
    let items = params
        .get("items")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("invalid_params: items missing"))?;
    // 第一遍：解析 + 按注册表校验。合法项收集待写；未知键/类型/枚举错的项**跳过并记录**，
    // 不让整批因一个旧字段失败（保护沿用旧字段的 webview）。malformed item（无 key）仍为硬错误。
    let mut writes: Vec<(String, toml::Value)> = Vec::with_capacity(items.len());
    let mut skipped: Vec<Value> = Vec::new();
    for it in items {
        let key = it
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("invalid_params: item.key missing"))?;
        let value = it.get("value").cloned().unwrap_or(Value::Null);
        let toml_val = match json_to_toml(&value) {
            Ok(v) => v,
            Err(e) => {
                skipped.push(json!({ "key": key, "reason": e.to_string() }));
                continue;
            }
        };
        match wind_config::config_schema::validate(key, &toml_val) {
            Ok(()) => writes.push((key.to_string(), toml_val)),
            Err(e) => skipped.push(json!({ "key": key, "reason": e.to_string() })),
        }
    }
    let applied = writes.len();
    // 第二遍：落盘合法项（IO 失败仍为硬错误）。
    for (key, toml_val) in writes {
        let parts: Vec<&str> = key.split('.').collect();
        Config::set_user_value(&parts, toml_val)?;
    }
    if !skipped.is_empty() {
        tracing::warn!(
            "config.setItems 跳过 {} 个无效项（未登记/类型/枚举）: {:?}",
            skipped.len(),
            skipped
        );
    }
    // 落盘后即时热重载：轻量字段立即生效，引擎结构性变更则 needsRestart=true。
    let needs_restart = state.core.apply_config();
    // 配置变更事件：通知订阅者（含 needsRestart 提示）。
    state
        .events
        .emit_config_changed(json!({ "reason": "setItems", "needsRestart": needs_restart }));
    Ok(json!({ "needsRestart": needs_restart, "applied": applied, "skipped": skipped }))
}

/// 把 config_schema 注册表序列化为 JSON（`{ fields: [{key, type, options?}] }`）。
/// 供 `config.schema` RPC；CLI/设置端据此列出、补全、校验。
fn schema_json() -> Value {
    use wind_config::config_schema::{FieldType, registry};
    let fields: Vec<Value> = registry()
        .iter()
        .map(|f| {
            let (ty, options): (&str, Option<&[&str]>) = match f.ty {
                FieldType::Bool => ("bool", None),
                FieldType::Int => ("int", None),
                FieldType::Float => ("float", None),
                FieldType::Str => ("string", None),
                FieldType::Enum(vs) => ("enum", Some(vs)),
                FieldType::StrList => ("string[]", None),
                FieldType::Map => ("map", None),
                FieldType::StructList => ("array", None),
            };
            let mut obj = json!({ "key": f.key, "type": ty });
            if let Some(vs) = options {
                obj["options"] = json!(vs);
            }
            obj
        })
        .collect();
    json!({ "fields": fields })
}

/// `config.getItem`：返回单个已登记键的当前值（三层合并后）。
fn get_item(params: &Value) -> anyhow::Result<Value> {
    let key = params
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("invalid_params: key missing"))?;
    if !wind_config::config_schema::is_known_key(key) {
        anyhow::bail!("invalid_config: 键 '{}' 未登记", key);
    }
    let cfg = Config::load(Config::data_dir().as_deref())?;
    let full = serde_json::to_value(cfg)?;
    let mut cur = &full;
    for part in key.split('.') {
        cur = cur
            .get(part)
            .ok_or_else(|| anyhow::anyhow!("config 缺少键 {}", key))?;
    }
    Ok(json!({ "key": key, "value": cur.clone() }))
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

#[cfg(test)]
mod tests {
    //! dispatch 单测：构造假 CoreRpc，发 system.info / config.get 等，断言 Response 形状。
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FakeCore {
        config_applied: AtomicBool,
    }
    impl FakeCore {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                config_applied: AtomicBool::new(false),
            })
        }
    }
    impl CoreRpc for FakeCore {
        fn is_chinese_mode(&self) -> bool {
            true
        }
        fn active_schema_id(&self) -> String {
            "wubi86".to_string()
        }
        fn apply_config(&self) -> bool {
            self.config_applied.store(true, Ordering::SeqCst);
            false // needsRestart=false
        }
        fn data_rpc(&self, method: &str, _params: &Value) -> anyhow::Result<Value> {
            if method == "dict.stats" {
                Ok(json!([]))
            } else {
                anyhow::bail!("unknown method: {}", method)
            }
        }
        fn fonts(&self) -> Vec<(String, String)> {
            vec![("Sans".to_string(), "Sans".to_string())]
        }
    }

    fn state() -> DispatchState {
        DispatchState::new(FakeCore::new(), "dev").expect("manifest 应能加载")
    }

    fn req(method: &str, params: Value) -> Request {
        Request {
            version: 1,
            id: 7,
            method: method.to_string(),
            params,
        }
    }

    #[test]
    fn system_info_shape() {
        let resp = dispatch(&state(), req("system.info", json!({})));
        assert_eq!(resp.id, 7);
        assert!(resp.error.is_none());
        let r = resp.result.unwrap();
        for k in ["version", "platform", "dataDir", "running"] {
            assert!(r.get(k).is_some(), "system.info 缺字段 {k}");
        }
        assert_eq!(r["variant"], json!("dev"));
        assert_eq!(r["activeSchema"], json!("wubi86"));
    }

    #[test]
    fn system_status_shape() {
        let resp = dispatch(&state(), req("system.status", json!({})));
        let r = resp.result.unwrap();
        assert_eq!(r["running"], json!(true));
        assert_eq!(r["mode"], json!("chinese"));
    }

    #[test]
    fn config_get_defaults_is_object() {
        let resp = dispatch(&state(), req("config.getDefaults", json!({})));
        let r = resp.result.unwrap();
        assert!(r.is_object());
        assert!(r["input"].is_object(), "默认配置应含 input 段");
    }

    #[test]
    fn manifest_shape() {
        let resp = dispatch(&state(), req("system.manifest", json!({})));
        let r = resp.result.unwrap();
        for k in [
            "manifest", "app", "engine", "variant", "groups", "items", "features",
        ] {
            assert!(r.get(k).is_some(), "manifest 缺字段 {k}");
        }
        assert!(r["items"].is_array() && r["groups"].is_array());
    }

    #[test]
    fn fonts_shape() {
        let resp = dispatch(&state(), req("system.fonts", json!({})));
        let r = resp.result.unwrap();
        assert!(r.is_array());
        assert_eq!(r[0]["family"], json!("Sans"));
    }

    #[test]
    fn unknown_method_returns_error() {
        let resp = dispatch(&state(), req("bogus.method", json!({})));
        assert!(resp.result.is_none());
        assert!(resp.error.is_some());
    }

    #[test]
    fn data_rpc_forwarded() {
        let resp = dispatch(&state(), req("dict.stats", json!({})));
        assert!(resp.error.is_none());
        assert!(resp.result.unwrap().is_array());
    }

    // ── Stage 2/4: registry 校验 + config.schema / config.getItem ──
    // 容错策略：未知键/类型/枚举错的键被「跳过并在响应 skipped 里报告」，合法项照常应用，
    // 整批不因一个旧字段失败（保护沿用旧字段的 webview）。下列测试单项无合法键，故不写盘。

    /// 取响应里 skipped 数组中的 key 列表。
    fn skipped_keys(resp: &Response) -> Vec<String> {
        resp.result
            .as_ref()
            .and_then(|r| r.get("skipped"))
            .and_then(|s| s.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|it| it.get("key").and_then(|k| k.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn set_items_skips_unknown_key() {
        let resp = dispatch(
            &state(),
            req(
                "config.setItems",
                json!({ "items": [{ "key": "ui.candidate.bogus", "value": 1 }] }),
            ),
        );
        assert!(resp.error.is_none(), "整批不应失败");
        let r = resp.result.clone().unwrap();
        assert_eq!(r["applied"], json!(0));
        assert!(skipped_keys(&resp).contains(&"ui.candidate.bogus".to_string()));
    }

    #[test]
    fn set_items_skips_enum_out_of_range() {
        let resp = dispatch(
            &state(),
            req(
                "config.setItems",
                json!({ "items": [{ "key": "ui.candidate.layout", "value": "diagonal" }] }),
            ),
        );
        assert!(resp.error.is_none());
        assert!(skipped_keys(&resp).contains(&"ui.candidate.layout".to_string()));
    }

    #[test]
    fn set_items_skips_type_mismatch() {
        let resp = dispatch(
            &state(),
            req(
                "config.setItems",
                json!({ "items": [{ "key": "ui.candidate.per_page", "value": "seven" }] }),
            ),
        );
        assert!(resp.error.is_none());
        assert!(skipped_keys(&resp).contains(&"ui.candidate.per_page".to_string()));
    }

    #[test]
    fn config_schema_lists_registered_fields() {
        let resp = dispatch(&state(), req("config.schema", json!({})));
        assert!(resp.error.is_none());
        let r = resp.result.unwrap();
        let fields = r["fields"].as_array().expect("fields 应为数组");
        let layout = fields
            .iter()
            .find(|f| f["key"] == json!("ui.candidate.layout"))
            .expect("应含 ui.candidate.layout");
        assert_eq!(layout["type"], json!("enum"));
        assert!(
            layout["options"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == &json!("vertical")),
            "enum 应带 options"
        );
    }

    #[test]
    fn config_get_item_known_returns_value_unknown_errors() {
        let ok = dispatch(
            &state(),
            req("config.getItem", json!({ "key": "ui.candidate.per_page" })),
        );
        assert!(ok.error.is_none(), "已登记键应成功");
        assert!(ok.result.unwrap()["value"].is_number());

        let bad = dispatch(
            &state(),
            req("config.getItem", json!({ "key": "no.such.key" })),
        );
        assert!(bad.result.is_none());
        assert!(bad.error.is_some());
    }
}
