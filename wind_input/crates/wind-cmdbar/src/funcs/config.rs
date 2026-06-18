//! 配置读写动作（对照 Go funcs/config_funcs.go）。`config.get` 为纯函数（依赖配置状态，
//! 非确定），`config.set` / `config.toggle` 为副作用。经
//! [`ConfigService`](crate::services::ConfigService)，key 为 YAML 路径。

use super::func_specs;
use super::util::{runtime_err, services};
use crate::context::EvalContext;
use crate::error::{CmdbarError, Result};
use crate::registry::FuncSpec;

pub fn specs() -> Vec<FuncSpec> {
    func_specs! {
        "config.get"   : Config (1, 1) pure   => fn_get,    "读取配置项当前值; key 为 YAML 路径 (如 ui.candidate.layout)", "config.get(\"ui.theme.style\")";
        "config.set"   : Config (2, 2) effect => fn_set,    "设置配置项并持久化; key 为 YAML 路径, value 为字符串", "config.set(\"ui.theme.style\", \"dark\")";
        "config.toggle": Config (1, 1) effect => fn_toggle, "枚举循环切换 / bool 翻转, 持久化并返回新值", "config.toggle(\"ui.theme.style\")";
    }
}

fn fn_get(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let s = services("config.get", ctx)?;
    let config = s.config.as_ref().ok_or_else(|| CmdbarError::service("config.get"))?;
    config.get(&args[0]).map_err(|e| runtime_err("config.get", e))
}

fn fn_set(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let s = services("config.set", ctx)?;
    let config = s.config.as_ref().ok_or_else(|| CmdbarError::service("config.set"))?;
    config.set(&args[0], &args[1]).map_err(|e| runtime_err("config.set", e))?;
    Ok(String::new())
}

fn fn_toggle(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let s = services("config.toggle", ctx)?;
    let config = s.config.as_ref().ok_or_else(|| CmdbarError::service("config.toggle"))?;
    config.toggle(&args[0]).map_err(|e| runtime_err("config.toggle", e))
}
