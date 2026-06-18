//! §3.4 动作函数（对照 Go funcs/action.go）。`pure=false`，运行时从
//! [`EvalContext::services`](crate::context::EvalContext::services) 取服务；缺服务返回
//! [`CmdbarError::ServiceUnavailable`](crate::error::CmdbarError)。
//!
//! 注意：`type` 不在此——它由 eval 在解析 `$CC` 动作时拦截为文本上屏（ActionText）。

use super::func_specs;
use super::util::{runtime_err, services};
use crate::context::EvalContext;
use crate::error::{CmdbarError, Result};
use crate::registry::FuncSpec;

pub fn specs() -> Vec<FuncSpec> {
    func_specs! {
        "open"       : Action (1, 1)  effect => fn_open,        "打开 URL / 程序 / 文件 (通用 ShellExecute 语义)", "open(\"https://baidu.com\")";
        "proc.run"   : Proc   (1, -1) effect => fn_run,         "启动外部程序, 可带参数", "proc.run(\"notepad.exe\")";
        "proc.shell" : Proc   (1, 2)  effect => fn_shell,       "通过 shell 执行命令行; 第二参可选 flag (term/pwsh)", "proc.shell(\"echo hi\")";
        "key.tap"    : Key    (1, 1)  effect => fn_key_tap,     "模拟单次按键组合, 如 Ctrl+C / Shift+End / Enter", "key.tap(\"Ctrl+C\")";
        "key.seq"    : Key    (1, -1) effect => fn_key_seq,     "顺序模拟多个按键组合", "key.seq(\"Home\", \"Shift+End\", \"Delete\")";
        "key.hold"   : Key    (1, 1)  effect => fn_key_hold,    "按下并保持按键组合 (需与 key.release 成对)", "key.hold(\"Shift\")";
        "key.release": Key    (1, 1)  effect => fn_key_release, "抬起之前 key.hold 按下的组合", "key.release(\"Shift\")";
        "key.type"   : Key    (1, 1)  effect => fn_key_type,    "以 Unicode 扫描码直接输入文本, 不依赖键盘布局", "key.type(\"hello\")";
        "clip.copy"  : Clip   (1, 1)  effect => fn_clip_copy,   "把文本写入系统剪贴板", "clip.copy(last())";
        "clip.paste" : Clip   (0, 0)  effect => fn_clip_paste,  "模拟 Ctrl+V 粘贴剪贴板内容", "clip.paste()";
        "web.search" : Web    (2, 2)  effect => fn_search,      "用搜索引擎搜索 (engine ∈ baidu/bing/google/zdic)", "web.search(\"baidu\", last())";
        "ask"        : Action (1, 1)  effect => fn_unimpl,      "弹小输入框, 阻塞返回用户输入 (未实现)", "ask(\"提示\")";
        "pick"       : Action (1, -1) effect => fn_unimpl,      "弹下拉列表选择 (未实现)", "pick(\"a\", \"b\")";
    }
}

fn fn_open(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let s = services("open", ctx)?;
    let open = s.open.as_ref().ok_or_else(|| CmdbarError::service("open"))?;
    open.open(&args[0]).map_err(|e| runtime_err("open", e))?;
    Ok(String::new())
}

fn fn_run(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let s = services("proc.run", ctx)?;
    let proc = s.proc.as_ref().ok_or_else(|| CmdbarError::service("proc.run"))?;
    proc.run(&args[0], &args[1..]).map_err(|e| runtime_err("proc.run", e))?;
    Ok(String::new())
}

fn fn_shell(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let s = services("proc.shell", ctx)?;
    let proc = s.proc.as_ref().ok_or_else(|| CmdbarError::service("proc.shell"))?;
    if args.len() == 1 {
        proc.shell(&args[0]).map_err(|e| runtime_err("proc.shell", e))?;
    } else {
        let flags: Vec<String> = args[1]
            .split(',')
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(|p| p.to_string())
            .collect();
        proc.shell_ex(&args[0], &flags).map_err(|e| runtime_err("proc.shell", e))?;
    }
    Ok(String::new())
}

fn fn_key_tap(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let keys = keys(ctx, "key.tap")?;
    keys.tap(&args[0]).map_err(|e| runtime_err("key.tap", e))?;
    Ok(String::new())
}

fn fn_key_seq(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let keys = keys(ctx, "key.seq")?;
    keys.sequence(args).map_err(|e| runtime_err("key.seq", e))?;
    Ok(String::new())
}

fn fn_key_hold(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let keys = keys(ctx, "key.hold")?;
    keys.hold(&args[0]).map_err(|e| runtime_err("key.hold", e))?;
    Ok(String::new())
}

fn fn_key_release(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let keys = keys(ctx, "key.release")?;
    keys.release(&args[0]).map_err(|e| runtime_err("key.release", e))?;
    Ok(String::new())
}

fn fn_key_type(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let keys = keys(ctx, "key.type")?;
    keys.type_text(&args[0]).map_err(|e| runtime_err("key.type", e))?;
    Ok(String::new())
}

fn keys<'a>(
    ctx: &'a dyn EvalContext,
    func: &str,
) -> Result<&'a std::sync::Arc<dyn crate::services::KeyInjector>> {
    let s = services(func, ctx)?;
    s.keys.as_ref().ok_or_else(|| CmdbarError::service(func.to_string()))
}

fn fn_clip_copy(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let s = services("clip.copy", ctx)?;
    let clip = s.clip.as_ref().ok_or_else(|| CmdbarError::service("clip.copy"))?;
    clip.set_text(&args[0]).map_err(|e| runtime_err("clip.copy", e))?;
    Ok(String::new())
}

fn fn_clip_paste(ctx: &dyn EvalContext, _args: &[String]) -> Result<String> {
    let s = services("clip.paste", ctx)?;
    let clip = s.clip.as_ref().ok_or_else(|| CmdbarError::service("clip.paste"))?;
    clip.paste().map_err(|e| runtime_err("clip.paste", e))?;
    Ok(String::new())
}

/// engine id → 查询 URL 前缀（%s 处接 URL 编码后的 query）。
const SEARCH_URLS: &[(&str, &str)] = &[
    ("baidu", "https://www.baidu.com/s?wd="),
    ("bing", "https://www.bing.com/search?q="),
    ("google", "https://www.google.com/search?q="),
    ("zdic", "https://www.zdic.net/hans/"),
];

fn fn_search(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let s = services("web.search", ctx)?;
    let engine = args[0].trim().to_lowercase();
    let query = &args[1];
    // 宿主自定义搜索优先。
    if let Some(search) = &s.search {
        search.search(&engine, query).map_err(|e| runtime_err("web.search", e))?;
        return Ok(String::new());
    }
    // 默认：合成 URL 转发给 open。
    let prefix = SEARCH_URLS
        .iter()
        .find(|(k, _)| *k == engine)
        .map(|(_, v)| *v)
        .ok_or_else(|| CmdbarError::runtime("web.search", format!("unknown engine {engine:?}")))?;
    let open = s.open.as_ref().ok_or_else(|| CmdbarError::service("web.search"))?;
    let target = format!("{prefix}{}", super::text::query_escape(query));
    open.open(&target).map_err(|e| runtime_err("web.search", e))?;
    Ok(String::new())
}

fn fn_unimpl(_: &dyn EvalContext, _args: &[String]) -> Result<String> {
    Err(CmdbarError::NotImplemented {
        name: "ask/pick".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MemoryContext;
    use crate::services::{Services, UrlOpener};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecordOpener(Mutex<Vec<String>>);
    impl UrlOpener for RecordOpener {
        fn open(&self, target: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(target.to_string());
            Ok(())
        }
    }

    #[test]
    fn open_dispatches_to_service() {
        let rec = Arc::new(RecordOpener::default());
        let mut svc = Services::new();
        svc.open = Some(rec.clone());
        let ctx = MemoryContext::new().with_services(svc);
        fn_open(&ctx, &["https://x".into()]).unwrap();
        assert_eq!(rec.0.lock().unwrap().as_slice(), &["https://x".to_string()]);
    }

    #[test]
    fn search_composes_url() {
        let rec = Arc::new(RecordOpener::default());
        let mut svc = Services::new();
        svc.open = Some(rec.clone());
        let ctx = MemoryContext::new().with_services(svc);
        fn_search(&ctx, &["baidu".into(), "a b".into()]).unwrap();
        assert_eq!(
            rec.0.lock().unwrap()[0],
            "https://www.baidu.com/s?wd=a+b"
        );
    }

    #[test]
    fn missing_service_errors() {
        let ctx = MemoryContext::new().with_services(Services::new());
        assert!(matches!(
            fn_open(&ctx, &["x".into()]),
            Err(CmdbarError::ServiceUnavailable { .. })
        ));
        // 完全无 services
        let bare = MemoryContext::new();
        assert!(matches!(
            fn_open(&bare, &["x".into()]),
            Err(CmdbarError::ServiceUnavailable { .. })
        ));
    }
}
