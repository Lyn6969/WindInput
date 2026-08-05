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
        "ime.pair"        : Ime     (2, 2) effect => fn_ime_pair,   "上屏配对文本并激活配对状态, 光标落两段之间, 可用跳出键 (Tab/Enter) 越过右段", "ime.pair(\"《\", \"》\")"
            named(fn_ime_pair_named, "jump" = "跳出时光标右移的格数; 省略=按右段字符数");
        "setting.open"    : Setting (1, 2) effect => fn_setting_open,"打开设置窗口的指定页面 (schema/input/keys/ui/dict/advanced/about; 空串=默认页); args 可选, 原样直通给设置程序", "setting.open(\"dict\", \"--schema=wubi86 --type=shadow\")";
        "setting.web"     : Setting (1, 2) effect => fn_setting_web, "打开设置页 (page 同 setting.open; --web 已废弃, 降级为原生设置页)", "setting.web(\"\")";
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

fn fn_ime_pair(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    fn_ime_pair_named(ctx, args, &[])
}

/// `ime.pair(left, right, jump="N")`。
///
/// `jump` 省略（或空串）时取 `right` 的 **char 数**，而不是 UTF-16 单元数：跳出靠合成
/// VK_RIGHT，多数宿主一次越过整个字素簇，按 UTF-16 算会在 emoji 右段上多移一格。反过来
/// 也有宿主按单元走，所以留 `jump` 这个显式开口兜底——推导只是默认值，不是唯一真相。
fn fn_ime_pair_named(
    ctx: &dyn EvalContext,
    args: &[String],
    named: &[(String, String)],
) -> Result<String> {
    let ime = ime(ctx, "ime.pair")?;
    let (left, right) = (&args[0], &args[1]);
    let raw = named
        .iter()
        .find(|(k, _)| k == "jump")
        .map(|(_, v)| v.trim())
        .unwrap_or("");
    let jump_steps = if raw.is_empty() {
        right.chars().count() as u32
    } else {
        raw.parse::<u32>().map_err(|_| {
            runtime_err(
                "ime.pair",
                anyhow::anyhow!("jump 需为非负整数，收到 {raw:?}"),
            )
        })?
    };
    ime.pair(left, right, jump_steps)
        .map_err(|e| runtime_err("ime.pair", e))?;
    Ok(String::new())
}

fn fn_undo_commit(ctx: &dyn EvalContext, _args: &[String]) -> Result<String> {
    let ime = ime(ctx, "ime.undo_commit")?;
    ime.undo_commit()
        .map_err(|e| runtime_err("ime.undo_commit", e))?;
    Ok(String::new())
}

fn fn_setting_open(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let ime = ime(ctx, "setting.open")?;
    let extra = args.get(1).map(String::as_str).unwrap_or("");
    ime.open_setting(&args[0], extra)
        .map_err(|e| runtime_err("setting.open", e))?;
    Ok(String::new())
}

fn fn_setting_web(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let ime = ime(ctx, "setting.web")?;
    let extra = args.get(1).map(String::as_str).unwrap_or("");
    ime.open_setting_web(&args[0], extra)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MemoryContext;
    use crate::services::{ImeController, Services};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecIme(Mutex<Vec<String>>);
    impl ImeController for RecIme {
        fn toggle(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn open_setting(&self, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn open_setting_web(&self, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn set_schema(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn theme_cycle(&self, _: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
        fn pair(&self, left: &str, right: &str, jump_steps: u32) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("{left}|{right}|{jump_steps}"));
            Ok(())
        }
    }

    fn ctx_with(rec: Arc<RecIme>) -> MemoryContext {
        let mut svc = Services::new();
        svc.ime = Some(rec);
        MemoryContext::new().with_services(svc)
    }

    /// `jump` 省略时按**右段 char 数**推导，不是 UTF-16 单元数——emoji 右段按单元算会多移一格。
    #[test]
    fn jump_defaults_to_right_char_count() {
        let rec = Arc::new(RecIme::default());
        let ctx = ctx_with(rec.clone());
        fn_ime_pair(&ctx, &["《".into(), "》".into()]).unwrap();
        fn_ime_pair(&ctx, &["<!--".into(), "-->".into()]).unwrap();
        // 星标 emoji 是一个 char / 两个 UTF-16 单元：按 char 记 1。
        fn_ime_pair(&ctx, &["[".into(), "]🌟".into()]).unwrap();
        let log = rec.0.lock().unwrap();
        assert_eq!(log[0], "《|》|1");
        assert_eq!(log[1], "<!--|-->|3");
        assert_eq!(log[2], "[|]🌟|2", "emoji 按 char 计 1，与 `]` 合计 2");
    }

    /// 显式 `jump` 覆盖推导值——宿主对 VK_RIGHT 的越过粒度不一致时，这是唯一的兜底开口。
    #[test]
    fn explicit_jump_overrides_default() {
        let rec = Arc::new(RecIme::default());
        let ctx = ctx_with(rec.clone());
        fn_ime_pair_named(
            &ctx,
            &["<!--".into(), "-->".into()],
            &[("jump".into(), "1".into())],
        )
        .unwrap();
        assert_eq!(rec.0.lock().unwrap()[0], "<!--|-->|1");
    }

    /// 非法 `jump` 必须报错而不是静默取默认：词条写错了要看得见，
    /// 静默兜底会让「跳出移错格数」变成一个查不出根因的现场。
    #[test]
    fn invalid_jump_errors_before_dispatch() {
        let rec = Arc::new(RecIme::default());
        let ctx = ctx_with(rec.clone());
        for bad in ["x", "-1", "1.5"] {
            let err = fn_ime_pair_named(
                &ctx,
                &["《".into(), "》".into()],
                &[("jump".into(), bad.into())],
            )
            .expect_err("非法 jump 应报错");
            assert!(err.to_string().contains("jump"), "{err}");
        }
        assert!(rec.0.lock().unwrap().is_empty(), "报错时不该已经派发出去");
    }
}
