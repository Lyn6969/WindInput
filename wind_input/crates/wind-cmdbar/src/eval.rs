//! AST 求值器
//!
//! 对照 Go `wind_input/internal/cmdbar/eval/eval.go`。把短语对 [`EvalContext`] 求值，
//! 产出 display 文本 + 动作链。`type(arg)` 动作被特例为 [`crate::action::ActionKind::Text`]
//! （宿主走 InsertText 上屏），其余为 [`crate::action::ActionKind::Effect`]。

use crate::action::ResolvedAction;
use crate::ast::{ArrayPhrase, CommandPhrase, Expr, Modifiers, Phrase, StringPart, fmt_number};
use crate::context::EvalContext;
use crate::error::{CmdbarError, Result};
use crate::registry::Registry;

/// 一条短语求值后的产物。
#[derive(Debug, Clone)]
pub struct Evaluated {
    pub display: String,
    pub actions: Vec<ResolvedAction>,
}

/// `$SS` 展开后的一个元素候选。
#[derive(Debug, Clone)]
pub struct ArrayElement {
    pub display: String,
    pub actions: Vec<ResolvedAction>,
    /// 嵌入 `$CC` 的元素级修饰符（组级 prefix 已在解析期禁用）。
    pub modifiers: Modifiers,
}

/// `$SS` 整体展开结果。
#[derive(Debug, Clone)]
pub struct ArrayExpansion {
    pub name: String,
    pub elements: Vec<ArrayElement>,
    pub modifiers: Modifiers,
}

/// 对短语求值。`Array` 须经 [`expand_array`]，此处返回错误。
pub fn evaluate(phrase: &Phrase, ctx: &dyn EvalContext, reg: &Registry) -> Result<Evaluated> {
    match phrase {
        Phrase::Literal(t) => Ok(Evaluated {
            display: t.clone(),
            actions: Vec::new(),
        }),
        Phrase::Template(expr) => Ok(Evaluated {
            display: eval_expr(expr, ctx, reg)?,
            actions: Vec::new(),
        }),
        Phrase::Array(_) => Err(CmdbarError::runtime(
            "eval",
            "ArrayPhrase must be expanded via expand_array, not evaluate",
        )),
        Phrase::Command(cp) => eval_command(cp, ctx, reg),
    }
}

fn eval_command(cp: &CommandPhrase, ctx: &dyn EvalContext, reg: &Registry) -> Result<Evaluated> {
    assert_pure_display(&cp.display, reg)?;
    let display = eval_expr(&cp.display, ctx, reg)?;
    let mut actions = Vec::with_capacity(cp.actions.len());
    for act in &cp.actions {
        // type(arg)：拦截为文本上屏（不经 registry 查找）。
        if let Expr::Call { name, args } = act
            && name == "type"
        {
            if args.len() != 1 {
                return Err(CmdbarError::runtime(
                    "type",
                    format!("expected 1 arg, got {}", args.len()),
                ));
            }
            actions.push(ResolvedAction::text(args[0].clone()));
            continue;
        }
        actions.push(ResolvedAction::effect(act.clone()));
    }
    Ok(Evaluated { display, actions })
}

/// 展开 `$SS`：字面元素→上屏文本候选，嵌入 `$CC`→动作候选。
pub fn expand_array(
    phrase: &ArrayPhrase,
    ctx: &dyn EvalContext,
    reg: &Registry,
) -> Result<ArrayExpansion> {
    let mut out = Vec::with_capacity(phrase.elements.len());
    for (i, elem) in phrase.elements.iter().enumerate() {
        match elem {
            Expr::StringLit(parts) => {
                let display = eval_string_lit(parts, ctx, reg).map_err(|e| wrap_elem(i, e))?;
                out.push(ArrayElement {
                    display,
                    actions: Vec::new(),
                    modifiers: Modifiers::new(),
                });
            }
            Expr::Command(cp) => {
                let ev = eval_command(cp, ctx, reg).map_err(|e| wrap_elem(i, e))?;
                out.push(ArrayElement {
                    display: ev.display,
                    actions: ev.actions,
                    modifiers: cp.modifiers.clone(),
                });
            }
            other => {
                return Err(CmdbarError::runtime(
                    "expand_array",
                    format!("element {} unsupported expr {other:?}", i + 1),
                ));
            }
        }
    }
    Ok(ArrayExpansion {
        name: phrase.name.clone(),
        elements: out,
        modifiers: phrase.modifiers.clone(),
    })
}

fn wrap_elem(i: usize, e: CmdbarError) -> CmdbarError {
    CmdbarError::runtime("$SS element", format!("#{}: {e}", i + 1))
}

/// 检查 display 表达式只引用纯函数（否则副作用会在候选显示阶段触发）。
fn assert_pure_display(expr: &Expr, reg: &Registry) -> Result<()> {
    match expr {
        Expr::StringLit(parts) => {
            for p in parts {
                if let StringPart::Interp(e) = p {
                    assert_pure_display(e, reg)?;
                }
            }
            Ok(())
        }
        Expr::Number { .. } => Ok(()),
        Expr::Ident(name) => check_pure(name, reg),
        Expr::Call { name, args } => {
            check_pure(name, reg)?;
            for a in args {
                assert_pure_display(a, reg)?;
            }
            Ok(())
        }
        other => Err(CmdbarError::runtime(
            "display",
            format!("unsupported expression {other:?}"),
        )),
    }
}

fn check_pure(name: &str, reg: &Registry) -> Result<()> {
    let spec = reg
        .lookup(name)
        .ok_or_else(|| CmdbarError::UnknownFunc { name: name.into() })?;
    if !spec.pure {
        return Err(CmdbarError::NotPure { name: name.into() });
    }
    Ok(())
}

/// 把表达式归约为字符串值。
pub(crate) fn eval_expr(expr: &Expr, ctx: &dyn EvalContext, reg: &Registry) -> Result<String> {
    match expr {
        Expr::StringLit(parts) => eval_string_lit(parts, ctx, reg),
        Expr::Number { value, raw } => {
            if raw.is_empty() {
                Ok(fmt_number(*value))
            } else {
                Ok(raw.clone())
            }
        }
        Expr::Ident(name) => call_func(name, &[], ctx, reg),
        Expr::Call { name, args } => {
            let mut argv = Vec::with_capacity(args.len());
            for a in args {
                argv.push(eval_expr(a, ctx, reg)?);
            }
            call_func(name, &argv, ctx, reg)
        }
        other => Err(CmdbarError::runtime(
            "eval",
            format!("unsupported expression {other:?}"),
        )),
    }
}

fn call_func(name: &str, args: &[String], ctx: &dyn EvalContext, reg: &Registry) -> Result<String> {
    let spec = reg
        .lookup(name)
        .ok_or_else(|| CmdbarError::UnknownFunc { name: name.into() })?;
    if !spec.accepts(args.len()) {
        return Err(CmdbarError::Arity {
            name: name.into(),
            got: args.len(),
            min: spec.min_args,
            max: spec.max_args,
        });
    }
    (spec.eval)(ctx, args)
}

fn eval_string_lit(parts: &[StringPart], ctx: &dyn EvalContext, reg: &Registry) -> Result<String> {
    let mut out = String::new();
    for p in parts {
        match p {
            StringPart::Text(t) => out.push_str(t),
            StringPart::Interp(e) => out.push_str(&eval_expr(e, ctx, reg)?),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MemoryContext;
    use crate::parser::parse;

    fn eval_str(src: &str) -> String {
        let reg = Registry::with_builtins();
        let ctx = MemoryContext::new().with_input("nihao");
        let p = parse(src).unwrap();
        evaluate(&p, &ctx, &reg).unwrap().display
    }

    #[test]
    fn literal_and_template() {
        assert_eq!(eval_str("hello"), "hello");
        assert_eq!(eval_str("code={code}"), "code=nihao");
        assert_eq!(eval_str("len={len(code)}"), "len=5");
    }

    #[test]
    fn nested_funcs() {
        assert_eq!(eval_str("{upper(sub(code, 1, 2))}"), "NI");
        assert_eq!(eval_str("{concat(code, \"!\")}"), "nihao!");
    }

    #[test]
    fn command_display_must_be_pure() {
        let reg = Registry::full();
        let ctx = MemoryContext::new();
        // open 是副作用函数，不能出现在 display
        let p = parse(r#"$CC(open("u"))"#).unwrap();
        assert!(matches!(
            evaluate(&p, &ctx, &reg),
            Err(CmdbarError::NotPure { .. })
        ));
    }

    #[test]
    fn command_type_action_is_text() {
        let reg = Registry::full();
        let ctx = MemoryContext::new();
        let p = parse(r#"$CC("《》", type("《》"))"#).unwrap();
        let ev = evaluate(&p, &ctx, &reg).unwrap();
        assert_eq!(ev.display, "《》");
        assert_eq!(ev.actions.len(), 1);
        assert_eq!(ev.actions[0].kind, crate::action::ActionKind::Text);
        assert_eq!(ev.actions[0].run(&ctx, &reg).unwrap(), "《》");
    }

    #[test]
    fn array_expansion() {
        let reg = Registry::full();
        let ctx = MemoryContext::new();
        let p = parse(r#"$SS("组", "字面", $CC("动作", type("x")))"#).unwrap();
        let arr = match p {
            Phrase::Array(a) => expand_array(&a, &ctx, &reg).unwrap(),
            _ => panic!(),
        };
        assert_eq!(arr.name, "组");
        assert_eq!(arr.elements.len(), 2);
        assert_eq!(arr.elements[0].display, "字面");
        assert!(arr.elements[0].actions.is_empty());
        assert_eq!(arr.elements[1].display, "动作");
        assert_eq!(arr.elements[1].actions.len(), 1);
    }

    #[test]
    fn aa_marker_expands_to_char_candidates() {
        // $AA 字符组：每个 rune 成为一个无动作的上屏文本候选。
        let reg = Registry::full();
        let ctx = MemoryContext::new();
        let p = parse(r#"$AA("数字", "①②③")"#).unwrap();
        let arr = match p {
            Phrase::Array(a) => expand_array(&a, &ctx, &reg).unwrap(),
            _ => panic!(),
        };
        assert_eq!(arr.name, "数字");
        assert_eq!(arr.elements.len(), 3);
        let displays: Vec<&str> = arr.elements.iter().map(|e| e.display.as_str()).collect();
        assert_eq!(displays, ["①", "②", "③"]);
        assert!(arr.elements.iter().all(|e| e.actions.is_empty()));
    }
}
