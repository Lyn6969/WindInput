//! 传输无关的 JSON-RPC 分发：system.* / config.*（方法名与 web 前端 contract.ts 一致），
//! 未知/数据类方法转发到注入的 [`CoreRpc`]。
//!
//! 从 wind-webapi/rpc.rs 迁移，去掉 axum/WebState 依赖：改用 [`DispatchState`]
//! 持有 capabilities 缓存 + variant + 注入的 core 实现，返回 wind-ipc 的 [`Response`]。

use std::sync::Arc;

use serde_json::{Value, json};
use wind_config::Config;
use wind_ipc::rpc::{Request, Response};

// 产品版本取 build.rs 从 docs/VERSION 注入的 WIND_APP_VERSION（= 0.100.0），
// 而非 workspace 的 CARGO_PKG_VERSION（兜底 0.x）。上报进 system_info.version / engine / appVersion。
pub(crate) const APP_VERSION: &str = env!("WIND_APP_VERSION");

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

/// 分发器共享状态：能力清单缓存 + 变体 + 注入的 core 实现。
pub struct DispatchState {
    pub(crate) core: Arc<dyn CoreRpc>,
    pub(crate) variant: &'static str,
    pub(crate) capabilities: Value,
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
        let capabilities = crate::capabilities::generate(Config::data_dir().as_deref())?;
        Ok(Self {
            core,
            variant,
            capabilities,
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
        "system.capabilities" => Ok(state.capabilities.clone()),
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
            // 出厂默认 = 系统预置（代码默认 L1 ⊕ data/config.toml L2），与 capability
            // 的 default 同源（system_preset_value）。不可用 toml::from_str("") 的纯 L1：
            // 顶码上屏/拼音自动学习等键出厂经 L2 置开、L1 为关，二者分叉会让设置端
            // 「恢复默认」把这些项误关。
            let v = Config::system_preset_value(Config::data_dir().as_deref())?;
            Ok(serde_json::to_value(v)?)
        }
        "config.setItems" => set_items(state, params),
        // 配置片段（TOML 文本）逐键预览：键/当前值/新值/错误，只读不落盘。
        "config.previewPatch" => preview_patch(params),
        // 配置片段应用：与 previewPatch 同一套校验，任何一条有错即整体拒绝（不做半应用）。
        "config.applyPatch" => apply_patch(state, params),
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
    if !skipped.is_empty() {
        tracing::warn!(
            "config.setItems 跳过 {} 个无效项（未登记/类型/枚举）: {:?}",
            skipped.len(),
            skipped
        );
    }
    // 第二遍：落盘合法项（IO 失败仍为硬错误）+ 热重载 + 事件广播。
    let needs_restart = apply_writes(state, writes, "setItems")?;
    Ok(json!({ "needsRestart": needs_restart, "applied": applied, "skipped": skipped }))
}

/// setItems / applyPatch 共用的落盘通路：逐键 `set_user_value`（继承「等出厂默认即删」
/// 的 prune 收口），随后即时热重载（轻量字段立即生效，引擎结构性变更则 needsRestart=true）
/// 并广播配置变更事件。返回 needsRestart。
fn apply_writes(
    state: &DispatchState,
    writes: Vec<(String, toml::Value)>,
    reason: &str,
) -> anyhow::Result<bool> {
    for (key, toml_val) in writes {
        let parts: Vec<&str> = key.split('.').collect();
        Config::set_user_value(&parts, toml_val)?;
    }
    let needs_restart = state.core.apply_config();
    state
        .events
        .emit_config_changed(json!({ "reason": reason, "needsRestart": needs_restart }));
    Ok(needs_restart)
}

/// 解析 + 展平 + 校验配置片段（previewPatch / applyPatch 共用）。
/// TOML 解析失败是整体错误；当前值与 `config.get` 同源（三层合并后的生效配置）。
///
/// 一并返回当前配置树：applyPatch 折算 Map 键的落盘整表要用它作合并种子，
/// 重新加载一次会引入「预览用 A 树、落盘用 B 树」的窗口。
fn patch_entries(
    params: &Value,
) -> anyhow::Result<(Vec<wind_config::patch::PatchEntry>, toml::Value)> {
    let text = params
        .get("text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("invalid_params: text missing"))?;
    let fragment = wind_config::patch::parse_fragment(text)
        .map_err(|e| anyhow::anyhow!("invalid_patch: {e}"))?;
    let current = toml::Value::try_from(Config::load(Config::data_dir().as_deref())?)?;
    let entries = wind_config::patch::preview(&fragment, &current);
    Ok((entries, current))
}

/// `config.previewPatch { text }` → `{ ok, entries: [{ key, mapEntry?, current?, next, error? }] }`，只读。
fn preview_patch(params: &Value) -> anyhow::Result<Value> {
    let (entries, _) = patch_entries(params)?;
    let ok = entries.iter().all(|e| e.error.is_none());
    Ok(json!({ "ok": ok, "entries": entries }))
}

/// `config.applyPatch { text }`：先跑与 preview 相同的校验，任何一条有错 → 整体 Err、
/// 不做半应用；全部合法 → 走 setItems 的批量落盘通路（继承 prune 与生效通知）。
/// 0 条目视为成功 no-op（不落盘、不触发热重载）。
///
/// `written` = **落盘后的最终键值**，Map 父键携带合并后的整表。设置端用它回灌配置镜像：
/// Map 合并后客户端无法从 entries 自行拼出整表（它手里没有 core 的当前表），必须由此回传。
/// `applied` 计的是**片段条目数**（Map 逐条目各计一条），与 preview 的 entries 条数对得上；
/// 落盘键数（`written.len()`）因 Map 合并而更少，两者刻意分开报。
fn apply_patch(state: &DispatchState, params: &Value) -> anyhow::Result<Value> {
    let (entries, current) = patch_entries(params)?;
    let bad: Vec<String> = entries
        .iter()
        .filter_map(|e| e.error.as_ref().map(|err| format!("{}: {}", e.key, err)))
        .collect();
    if !bad.is_empty() {
        anyhow::bail!("invalid_patch: {}", bad.join("; "));
    }
    if entries.is_empty() {
        return Ok(json!({ "ok": true, "applied": 0, "needsRestart": false, "written": [] }));
    }
    let applied = entries.len();
    let writes = wind_config::patch::writes(&entries, &current);
    let written: Vec<Value> = writes
        .iter()
        .map(|(key, value)| -> anyhow::Result<Value> {
            Ok(json!({ "key": key, "value": serde_json::to_value(value)? }))
        })
        .collect::<anyhow::Result<_>>()?;
    let needs_restart = apply_writes(state, writes, "applyPatch")?;
    Ok(json!({
        "ok": true,
        "applied": applied,
        "needsRestart": needs_restart,
        "written": written,
    }))
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
        DispatchState::new(FakeCore::new(), "dev").expect("capabilities 应能加载")
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
    fn capabilities_shape() {
        let resp = dispatch(&state(), req("system.capabilities", json!({})));
        let r = resp.result.expect("system.capabilities 应成功");
        assert!(
            r.get("configKeys").and_then(|v| v.as_array()).is_some(),
            "capabilities 应含 configKeys 数组"
        );
        assert!(
            r.get("appVersion").is_some(),
            "capabilities 应含 appVersion"
        );
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

    // ── config.previewPatch / applyPatch 契约（照 scheme.previewImport 先例：只测
    // 只读与错误路径）。applyPatch 的成功写路径**刻意不在此测**——它会真写用户层
    // config.toml（%APPDATA%），校验+展平的纯逻辑已在 wind-config::patch 层覆盖。

    #[test]
    fn preview_patch_reports_entries_readonly() {
        let core = FakeCore::new();
        let st = DispatchState::new(core.clone(), "dev").unwrap();
        let resp = dispatch(
            &st,
            req(
                "config.previewPatch",
                json!({ "text": "[ui.candidate]\nper_page = 9\n" }),
            ),
        );
        assert!(resp.error.is_none(), "{:?}", resp.error);
        let r = resp.result.unwrap();
        assert_eq!(r["ok"], json!(true));
        let entries = r["entries"].as_array().expect("entries 应为数组");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["key"], json!("ui.candidate.per_page"));
        assert_eq!(entries[0]["next"], json!(9));
        assert!(entries[0].get("error").is_none(), "合法条目不应有 error");
        // 只读：不得触发热重载（也证明未走落盘通路）。
        assert!(!core.config_applied.load(Ordering::SeqCst));
    }

    /// Map 键在 RPC 面上逐条目呈现：`key` = 父 Map 键，条目名走 `mapEntry`（serde rename）。
    /// 设置端的确认对话框据此逐条列出「哪个绑定改成了什么」。
    #[test]
    fn preview_patch_reports_map_entries_with_map_entry_field() {
        let core = FakeCore::new();
        let st = DispatchState::new(core.clone(), "dev").unwrap();
        let resp = dispatch(
            &st,
            req(
                "config.previewPatch",
                json!({ "text": "[keys.key_actions]\nf4 = \"english\"\nf5 = \"半角\"\n" }),
            ),
        );
        assert!(resp.error.is_none(), "{:?}", resp.error);
        let r = resp.result.unwrap();
        assert_eq!(r["ok"], json!(true));
        let entries = r["entries"].as_array().expect("entries 应为数组");
        assert_eq!(entries.len(), 2, "Map 两个条目应各占一行: {entries:?}");
        for e in entries {
            assert_eq!(e["key"], json!("keys.key_actions"), "key 恒为父 Map 键");
            assert!(e.get("mapEntry").is_some(), "Map 条目须带 mapEntry: {e}");
        }
        assert_eq!(entries[0]["mapEntry"], json!("f4"));
        assert_eq!(entries[0]["next"], json!("english"));
        assert!(!core.config_applied.load(Ordering::SeqCst), "预览只读");
    }

    #[test]
    fn preview_patch_flags_unknown_and_invalid_values() {
        let resp = dispatch(
            &state(),
            req(
                "config.previewPatch",
                json!({ "text": "[ui.candidate]\nlayout = \"diagonal\"\n[input.foo]\nbar = 1\n" }),
            ),
        );
        assert!(resp.error.is_none(), "逐键错误不是 RPC 错误");
        let r = resp.result.unwrap();
        assert_eq!(r["ok"], json!(false));
        let entries = r["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            entries.iter().all(|e| e.get("error").is_some()),
            "两条都应带 error: {entries:?}"
        );
    }

    #[test]
    fn preview_patch_rejects_invalid_toml_as_whole() {
        let resp = dispatch(
            &state(),
            req("config.previewPatch", json!({ "text": "= not toml =" })),
        );
        assert!(resp.result.is_none());
        assert!(resp.error.is_some(), "整体解析失败应为 RPC 错误");
    }

    #[test]
    fn apply_patch_rejects_fragment_with_any_error() {
        let core = FakeCore::new();
        let st = DispatchState::new(core.clone(), "dev").unwrap();
        // 一条合法 + 一条未知键：整体拒绝，不做半应用。
        let resp = dispatch(
            &st,
            req(
                "config.applyPatch",
                json!({ "text": "[ui.candidate]\nper_page = 9\nbogus = 1\n" }),
            ),
        );
        assert!(resp.result.is_none());
        let err = resp.error.expect("应整体拒绝");
        assert!(err.contains("bogus"), "错误应点名出错的键: {err}");
        assert!(
            !core.config_applied.load(Ordering::SeqCst),
            "整体拒绝不得触发热重载（也证明未走落盘通路）"
        );
    }

    #[test]
    fn apply_patch_empty_fragment_is_noop_success() {
        let core = FakeCore::new();
        let st = DispatchState::new(core.clone(), "dev").unwrap();
        let resp = dispatch(&st, req("config.applyPatch", json!({ "text": "" })));
        assert!(resp.error.is_none(), "{:?}", resp.error);
        let r = resp.result.unwrap();
        assert_eq!(r["ok"], json!(true));
        assert_eq!(r["applied"], json!(0));
        // written 恒在场（空数组），设置端回灌逻辑不必分「有没有这个字段」。
        assert_eq!(r["written"], json!([]));
        assert!(
            !core.config_applied.load(Ordering::SeqCst),
            "no-op 不落盘也不热重载"
        );
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
