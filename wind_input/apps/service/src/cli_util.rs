//! CLI 子命令共享工具：RPC 调用（区分「core 未运行」与远端错误）、
//! JSON 点路径读写、按现值类型转换命令行字符串。
//!
//! `config` 之外的子命令（schema/dict/phrase/backup）统一**仅在线**：
//! 它们操作 redb 单写者库或 coordinator 内的 override 合并逻辑，离线直写
//! 会与运行中实例冲突，故连不上 core 一律报错退出（区别于 config 的离线降级）。

use serde_json::Value;

/// RPC 调用失败的分类：连接失败（core 未运行）与远端错误须给用户不同提示。
pub enum RpcFailure {
    /// 控制通道不可达（core 未运行 / 管道打开或收发失败）。原始错误对用户
    /// 无行动价值（一律「请先启动输入法」），不携带。
    Offline,
    /// core 收到请求但返回错误文本。
    Remote(anyhow::Error),
}

/// 发一条控制通道 RPC。连接类失败（错误链含 `io::Error`）归为 [`RpcFailure::Offline`]。
pub fn rpc(method: &str, params: Value) -> Result<Value, RpcFailure> {
    match wind_rpc::client::call(wind_config::variant::pipe_suffix(), method, params) {
        Ok(v) => Ok(v),
        Err(e) => {
            if e.chain().any(|c| c.is::<std::io::Error>()) {
                Err(RpcFailure::Offline)
            } else {
                Err(RpcFailure::Remote(e))
            }
        }
    }
}

/// 仅在线调用：连不上 → 统一话术；远端错误原样透传。
pub fn rpc_online(method: &str, params: Value) -> anyhow::Result<Value> {
    match rpc(method, params) {
        Ok(v) => Ok(v),
        Err(RpcFailure::Offline) => {
            anyhow::bail!("core 未运行（此命令需要输入法服务在线，请先启动输入法）")
        }
        Err(RpcFailure::Remote(e)) => Err(e),
    }
}

/// 按 `a.b.c` 点路径下钻只读取值。不支持数组下标。
pub fn json_get_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = root;
    for part in path.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

/// 按点路径写入值；沿途任一段缺失或终点父级不是对象则失败返回 false。
pub fn json_set_path(root: &mut Value, path: &str, new: Value) -> bool {
    let parts: Vec<&str> = path.split('.').collect();
    let Some((last, walk)) = parts.split_last() else {
        return false;
    };
    let mut cur = root;
    for part in walk {
        let Some(next) = cur.get_mut(*part) else {
            return false;
        };
        cur = next;
    }
    match cur {
        Value::Object(map) if map.contains_key(*last) => {
            map.insert((*last).to_string(), new);
            true
        }
        _ => false,
    }
}

/// 按「现有值的类型」把命令行原始字符串转换为 JSON 值。
///
/// 方案配置没有 config_schema 那样的注册表，类型信息只能取自合并后配置的现值；
/// 现值为 null 的键（未定义/Option 未设）由调用方先行拒绝。
pub fn coerce_like(current: &Value, raw: &str) -> Result<Value, String> {
    match current {
        Value::Bool(_) => match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(Value::Bool(true)),
            "false" | "0" | "no" | "off" => Ok(Value::Bool(false)),
            _ => Err(format!("'{raw}' 不是布尔值（true/false）")),
        },
        Value::Number(n) if n.is_i64() || n.is_u64() => raw
            .trim()
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| format!("'{raw}' 不是整数")),
        // 拒绝 nan/inf/溢出：`Value::from(非有限 f64)` 会静默变 null，落盘后键类型损坏。
        Value::Number(_) => match raw.trim().parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Value::from(f)),
            _ => Err(format!("'{raw}' 不是有限数字")),
        },
        Value::String(_) => Ok(Value::String(raw.to_string())),
        Value::Array(_) => match serde_json::from_str::<Value>(raw) {
            Ok(v @ Value::Array(_)) => Ok(v),
            Ok(_) => Err(format!("'{raw}' 不是 JSON 数组")),
            Err(e) => Err(format!("数组值需为 JSON: {e}")),
        },
        Value::Object(_) => match serde_json::from_str::<Value>(raw) {
            Ok(v @ Value::Object(_)) => Ok(v),
            Ok(_) => Err(format!("'{raw}' 不是 JSON 对象")),
            Err(e) => Err(format!("对象值需为 JSON: {e}")),
        },
        Value::Null => Err("该键当前无值，无法推断类型".to_string()),
    }
}

/// 显示一个 JSON 值：字符串去引号，复合值 pretty，其余紧凑。
pub fn format_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn get_path_walks_nested_objects() {
        let v = json!({ "a": { "b": { "c": 7 } } });
        assert_eq!(json_get_path(&v, "a.b.c"), Some(&json!(7)));
        assert_eq!(json_get_path(&v, "a.b"), Some(&json!({ "c": 7 })));
        assert_eq!(json_get_path(&v, "a.x.c"), None);
        assert_eq!(json_get_path(&v, "a.b.c.d"), None);
    }

    #[test]
    fn set_path_only_replaces_existing_keys() {
        let mut v = json!({ "a": { "b": 1 }, "s": "x" });
        assert!(json_set_path(&mut v, "a.b", json!(2)));
        assert_eq!(v["a"]["b"], json!(2));
        // 不存在的键不凭空创建（防 typo 静默写进 override）。
        assert!(!json_set_path(&mut v, "a.zzz", json!(3)));
        assert!(!json_set_path(&mut v, "no.such", json!(3)));
        // 终点父级不是对象。
        assert!(!json_set_path(&mut v, "s.sub", json!(3)));
    }

    #[test]
    fn coerce_follows_current_type() {
        assert_eq!(coerce_like(&json!(true), "off").unwrap(), json!(false));
        assert_eq!(coerce_like(&json!(3), "9").unwrap(), json!(9));
        assert_eq!(coerce_like(&json!(1.5), "2").unwrap(), json!(2.0));
        assert_eq!(coerce_like(&json!("a"), "9").unwrap(), json!("9"));
        assert_eq!(
            coerce_like(&json!(["x"]), r#"["y","z"]"#).unwrap(),
            json!(["y", "z"])
        );
        assert!(coerce_like(&json!(3), "seven").is_err());
        // 非有限浮点若放行会经 Value::from 静默变 null
        assert!(coerce_like(&json!(1.5), "nan").is_err());
        assert!(coerce_like(&json!(1.5), "inf").is_err());
        assert!(coerce_like(&json!(1.5), "1e400").is_err());
        assert!(coerce_like(&json!(["x"]), "{}").is_err());
        assert!(coerce_like(&Value::Null, "1").is_err());
    }

    #[test]
    fn format_value_unquotes_strings_only() {
        assert_eq!(format_value(&json!("vertical")), "vertical");
        assert_eq!(format_value(&json!(7)), "7");
        assert!(format_value(&json!({ "a": 1 })).contains('\n'));
    }
}
