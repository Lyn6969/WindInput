//! 词库 / IME / 设置动作（对照 Go funcs/dict_ime.go）。`pure=false`，经
//! [`DictService`](crate::services::DictService) / [`ImeController`](crate::services::ImeController) /
//! [`ConfigService`](crate::services::ConfigService) 取真实后端。

use super::func_specs;
use super::util::{runtime_err, services};
use crate::context::EvalContext;
use crate::error::{CmdbarError, Result};
use crate::registry::FuncSpec;

pub fn specs() -> Vec<FuncSpec> {
    func_specs! {
        "dict.add"        : Dict    (1, 2) effect => fn_dict_add,    "把文本加入用户词库; code 可选, 不传时按当前方案规则推导", "dict.add(clip())";
        "ime.toggle"      : Ime     (1, 1) effect => fn_ime_toggle,  "切换 IME 状态 (cn-en / fullshape / layout / candwin / s2t / preedit / toolbar)", "ime.toggle(\"cn-en\")";
        "ime.schema"      : Ime     (1, 1) effect => fn_ime_schema,  "切换输入方案并持久化", "ime.schema(\"pinyin\")";
        "ime.theme"       : Ime     (1, 1) effect => fn_ime_theme,   "切换主题并持久化 (= config.set ui.theme.name)", "ime.theme(\"msime\")";
        "ime.theme_cycle" : Ime     (0, 1) effect => fn_theme_cycle, "循环切换主题并持久化; dir 可选 next(默认)/prev", "ime.theme_cycle()";
        "ime.undo_commit" : Ime     (0, 0) effect => fn_undo_commit,"撤销最近一次上屏 (删刚上屏的字符数; 焦点变化或又输入其它内容后退化删 1 个)", "ime.undo_commit()";
        "setting.open"    : Setting (1, 1) effect => fn_setting_open,"打开 wind_setting 设置窗口的指定页面", "setting.open(\"dict\")";
        "setting.web"     : Setting (1, 1) effect => fn_setting_web, "以 --web 启动 wind_setting 打开 Web 版设置", "setting.web(\"\")";
    }
}

/// 配置键：主题名（与 Go configkey.UiThemeName 对齐）。
const UI_THEME_NAME: &str = "ui.theme.name";

fn fn_dict_add(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let s = services("dict.add", ctx)?;
    let dict = s
        .dict
        .as_ref()
        .ok_or_else(|| CmdbarError::service("dict.add"))?;
    let code = args.get(1).map(String::as_str).unwrap_or("");
    dict.add_word(&args[0], code)
        .map_err(|e| runtime_err("dict.add", e))?;
    Ok(String::new())
}

fn fn_ime_toggle(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let ime = ime(ctx, "ime.toggle")?;
    ime.toggle(&args[0])
        .map_err(|e| runtime_err("ime.toggle", e))?;
    Ok(String::new())
}

fn fn_ime_schema(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let ime = ime(ctx, "ime.schema")?;
    ime.set_schema(&args[0])
        .map_err(|e| runtime_err("ime.schema", e))?;
    Ok(String::new())
}

fn fn_theme_cycle(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let ime = ime(ctx, "ime.theme_cycle")?;
    let dir = args.first().map(String::as_str).unwrap_or("");
    let next = ime
        .theme_cycle(dir)
        .map_err(|e| runtime_err("ime.theme_cycle", e))?;
    Ok(next)
}

fn fn_undo_commit(ctx: &dyn EvalContext, _args: &[String]) -> Result<String> {
    let ime = ime(ctx, "ime.undo_commit")?;
    ime.undo_commit()
        .map_err(|e| runtime_err("ime.undo_commit", e))?;
    Ok(String::new())
}

fn fn_setting_open(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let ime = ime(ctx, "setting.open")?;
    ime.open_setting(&args[0])
        .map_err(|e| runtime_err("setting.open", e))?;
    Ok(String::new())
}

fn fn_setting_web(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let ime = ime(ctx, "setting.web")?;
    ime.open_setting_web(&args[0])
        .map_err(|e| runtime_err("setting.web", e))?;
    Ok(String::new())
}

/// `ime.theme(name)` 经 ConfigService 设置主题名（与 config.set 等价）。
fn fn_ime_theme(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let s = services("ime.theme", ctx)?;
    let config = s
        .config
        .as_ref()
        .ok_or_else(|| CmdbarError::service("ime.theme"))?;
    config
        .set(UI_THEME_NAME, &args[0])
        .map_err(|e| runtime_err("ime.theme", e))?;
    Ok(String::new())
}

fn ime<'a>(
    ctx: &'a dyn EvalContext,
    func: &str,
) -> Result<&'a std::sync::Arc<dyn crate::services::ImeController>> {
    let s = services(func, ctx)?;
    s.ime
        .as_ref()
        .ok_or_else(|| CmdbarError::service(func.to_string()))
}
