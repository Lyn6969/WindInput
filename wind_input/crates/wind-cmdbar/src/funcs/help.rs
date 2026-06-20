//! 内省函数 `help(name)`：返回指定函数的简介（对照 Go funcs/help.go）。

use super::func_specs;
use crate::context::EvalContext;
use crate::error::Result;
use crate::registry::{FuncSpec, default_registry};

pub fn specs() -> Vec<FuncSpec> {
    func_specs! {
        "help": Meta (1, 1) det => fn_help, "返回指定函数的简介 (查不到时返回空字符串)", "help(\"open\")";
    }
}

fn fn_help(_: &dyn EvalContext, args: &[String]) -> Result<String> {
    match default_registry().lookup(&args[0]) {
        Some(spec) if spec.deprecated && !spec.alias_of.is_empty() => {
            Ok(format!("{} — {}", spec.description, spec.alias_of))
        }
        Some(spec) => Ok(spec.description.to_string()),
        None => Ok(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MemoryContext;

    #[test]
    fn help_known_and_unknown() {
        let ctx = MemoryContext::new();
        assert!(!fn_help(&ctx, &["open".into()]).unwrap().is_empty());
        assert_eq!(fn_help(&ctx, &["nope".into()]).unwrap(), "");
    }
}
