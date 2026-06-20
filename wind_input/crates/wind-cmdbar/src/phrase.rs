//! 短语级高层 API（宿主集成入口）。
//!
//! 把「解析 → 求值 / `$SS` 展开」打包成一次调用，并提供 [`is_cmdbar_grammar`] 供宿主
//! 与旧的简单模板路径分流（对应 Go coordinator 的 phrase hook + 双路径策略 design §7.2）。
//!
//! 线程/求值：display 侧只用纯函数（[`Registry::with_builtins`]）即可；命令动作需要
//! 宿主注入 [`Services`](crate::services::Services) 后用 [`Registry::full`]。

use crate::context::EvalContext;
use crate::error::Result;
use crate::eval::{ArrayExpansion, evaluate, expand_array};
use crate::parser::{self, parse};
use crate::registry::Registry;
use crate::{ActionKind, ResolvedAction};

pub use parser::is_cmdbar_grammar;

/// 一条短语求值后的形态：单候选或 `$SS` 多候选。
#[derive(Debug, Clone)]
pub enum PhraseEval {
    /// literal / template / command：单个 display + 动作链（command 才有动作）。
    Single {
        display: String,
        actions: Vec<ResolvedAction>,
    },
    /// `$SS` 数组：组名 + 多元素。
    Array(ArrayExpansion),
}

impl PhraseEval {
    /// 便捷取首个 display（Array 取组名）。
    pub fn primary_display(&self) -> &str {
        match self {
            PhraseEval::Single { display, .. } => display,
            PhraseEval::Array(a) => &a.name,
        }
    }
}

/// 解析并求值一条短语文本。`$SS` 走 [`expand_array`]，其余走 [`evaluate`]。
pub fn evaluate_phrase(text: &str, ctx: &dyn EvalContext, reg: &Registry) -> Result<PhraseEval> {
    match parse(text)? {
        crate::Phrase::Array(a) => Ok(PhraseEval::Array(expand_array(&a, ctx, reg)?)),
        other => {
            let ev = evaluate(&other, ctx, reg)?;
            Ok(PhraseEval::Single {
                display: ev.display,
                actions: ev.actions,
            })
        }
    }
}

/// 执行一条已求值短语的动作链（command 选中时调用）：拼接所有 [`ActionKind::Text`]
/// 的上屏文本，并按序触发 [`ActionKind::Effect`] 副作用。返回待上屏文本。
///
/// 动作在此延迟求值（按当前 `ctx`）；Effect 错误不中断后续动作，只随结果返回首个错误。
pub fn run_actions(
    actions: &[ResolvedAction],
    ctx: &dyn EvalContext,
    reg: &Registry,
) -> (String, Option<crate::CmdbarError>) {
    let mut insert = String::new();
    let mut first_err = None;
    // 先 Effect（text 之前）保持与 Go 时序一致：副作用在落字前同步执行。
    for act in actions.iter().filter(|a| a.kind == ActionKind::Effect) {
        if let Err(e) = act.run(ctx, reg)
            && first_err.is_none()
        {
            first_err = Some(e);
        }
    }
    for act in actions.iter().filter(|a| a.kind == ActionKind::Text) {
        match act.run(ctx, reg) {
            Ok(s) => insert.push_str(&s),
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    (insert, first_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MemoryContext;

    #[test]
    fn grammar_detection() {
        assert!(is_cmdbar_grammar("{date()}"));
        assert!(is_cmdbar_grammar(r#"$CC("x", type("x"))"#));
        assert!(is_cmdbar_grammar(r#"$SS("g", "a")"#));
        // 旧简单模板（无顶层 `{`）不算命令栏语法
        assert!(!is_cmdbar_grammar("$Y年$M月"));
        assert!(!is_cmdbar_grammar("纯文本"));
    }

    #[test]
    fn evaluate_template_phrase() {
        let reg = Registry::with_builtins();
        let ctx = MemoryContext::new().with_input("abc");
        let r = evaluate_phrase("len={len(code)}", &ctx, &reg).unwrap();
        match r {
            PhraseEval::Single { display, actions } => {
                assert_eq!(display, "len=3");
                assert!(actions.is_empty());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn evaluate_array_phrase() {
        let reg = Registry::full();
        let ctx = MemoryContext::new();
        let r = evaluate_phrase(r#"$SS("符号", "（）", "【】")"#, &ctx, &reg).unwrap();
        match r {
            PhraseEval::Array(a) => {
                assert_eq!(a.name, "符号");
                assert_eq!(a.elements.len(), 2);
                assert_eq!(a.elements[0].display, "（）");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn run_actions_collects_text_and_effects() {
        use crate::services::{KeyInjector, Services};
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct Log(Mutex<Vec<String>>);
        impl KeyInjector for Log {
            fn tap(&self, c: &str) -> anyhow::Result<()> {
                self.0.lock().unwrap().push(c.into());
                Ok(())
            }
            fn sequence(&self, _: &[String]) -> anyhow::Result<()> {
                Ok(())
            }
            fn hold(&self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn release(&self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn type_text(&self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let log = Arc::new(Log::default());
        let mut svc = Services::new();
        svc.keys = Some(log.clone());
        let ctx = MemoryContext::new().with_services(svc);
        let reg = Registry::full();

        let r =
            evaluate_phrase(r#"$CC("《》", type("《》"), key.tap("Left"))"#, &ctx, &reg).unwrap();
        let actions = match r {
            PhraseEval::Single { actions, .. } => actions,
            _ => panic!(),
        };
        let (insert, err) = run_actions(&actions, &ctx, &reg);
        assert!(err.is_none());
        assert_eq!(insert, "《》");
        assert_eq!(log.0.lock().unwrap().as_slice(), &["Left".to_string()]);
    }

    /// 端到端 ime 命令（宿主 $CC 执行通路依赖此流程）：
    /// `$CC("切简繁", ime.toggle("s2t"))` 选中 → 派发到 ImeController.toggle("s2t")，无上屏文本。
    #[test]
    fn run_ime_command_dispatches_to_controller() {
        use crate::services::{ImeController, Services};
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct Ime(Mutex<Vec<String>>);
        impl ImeController for Ime {
            fn toggle(&self, target: &str) -> anyhow::Result<()> {
                self.0.lock().unwrap().push(target.into());
                Ok(())
            }
            fn open_setting(&self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn open_setting_web(&self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn set_schema(&self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn theme_cycle(&self, _: &str) -> anyhow::Result<String> {
                Ok(String::new())
            }
        }

        let ime = Arc::new(Ime::default());
        let mut svc = Services::new();
        svc.ime = Some(ime.clone());
        let ctx = MemoryContext::new().with_services(svc);
        let reg = Registry::full();

        let r = evaluate_phrase(r#"$CC("切简繁", ime.toggle("s2t"))"#, &ctx, &reg).unwrap();
        let (insert, err) = match r {
            PhraseEval::Single { actions, .. } => run_actions(&actions, &ctx, &reg),
            _ => panic!(),
        };
        assert!(err.is_none());
        assert_eq!(insert, ""); // 纯副作用命令无上屏文本
        assert_eq!(ime.0.lock().unwrap().as_slice(), &["s2t".to_string()]);
    }

    /// 缺服务时命令优雅降级：动作返回 ServiceUnavailable，run_actions 收集错误但不 panic。
    #[test]
    fn missing_service_degrades_gracefully() {
        let ctx = MemoryContext::new().with_services(crate::services::Services::new());
        let reg = Registry::full();
        let r = evaluate_phrase(r#"$CC("x", open("https://y"))"#, &ctx, &reg).unwrap();
        let (insert, err) = match r {
            PhraseEval::Single { actions, .. } => run_actions(&actions, &ctx, &reg),
            _ => panic!(),
        };
        assert_eq!(insert, "");
        assert!(matches!(
            err,
            Some(crate::CmdbarError::ServiceUnavailable { .. })
        ));
    }
}
