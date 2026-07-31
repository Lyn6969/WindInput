//! 函数注册表
//!
//! 对照 Go `wind_input/internal/cmdbar/registry.go` + `funcs/register.go`。
//!
//! 优化：取消 Go 的「先注册 stub、再 RegisterActions 覆盖」两段式——动作函数直接以
//! `pure=false` 注册，缺服务时在调用期返回 [`CmdbarError::ServiceUnavailable`]。
//! [`Registry::full`] 一次装齐纯函数 + 动作函数。

use crate::context::EvalContext;
use crate::error::Result;
use std::collections::HashMap;

/// 已注册函数的求值入口签名（全字符串语义，对齐 Go EvalFunc）。
pub type EvalFn = fn(&dyn EvalContext, &[String]) -> Result<String>;

/// 支持具名参数的函数的求值入口。第三参是**已求值**的具名参数，保留源顺序。
///
/// 与 [`EvalFn`] 分开而不是统一改签名：全仓 ~60 个函数里只有少数需要选项，
/// 统一加参数会让每个函数都背上一个恒为空的入参。
pub type EvalFnNamed = fn(&dyn EvalContext, &[String], &[(String, String)]) -> Result<String>;

/// 函数语义分组，供设置 UI 分类显示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Value,
    Text,
    Calc,
    Action,
    Clip,
    Key,
    Proc,
    Dict,
    Ime,
    Setting,
    Config,
    Web,
    Meta,
}

/// 单个函数的元信息 + 求值入口。
#[derive(Clone)]
pub struct FuncSpec {
    pub name: &'static str,
    pub category: Category,
    pub min_args: usize,
    /// -1 表示可变参数。
    pub max_args: isize,
    /// 纯函数（无副作用）：仅纯函数允许出现在 `$CC` display 表达式中。
    pub pure: bool,
    /// 同输入同输出（预留求值缓存；依赖外部状态者为 false）。
    pub deterministic: bool,
    pub deprecated: bool,
    pub alias_of: &'static str,
    pub description: &'static str,
    pub example: &'static str,
    pub eval: EvalFn,
    /// 允许的具名参数名白名单（`(名, 说明)`，说明供设置页手册展示）。
    /// 空 = 该函数不接受具名参数。写错的名字必须报错而非静默忽略，
    /// 与配置层「不凭空创建键」同哲学。
    pub named_params: &'static [(&'static str, &'static str)],
    /// 带具名参数时的求值入口。`named_params` 非空则必须提供。
    pub eval_named: Option<EvalFnNamed>,
}

impl FuncSpec {
    /// n 是否在 arity 边界内。
    pub fn accepts(&self, n: usize) -> bool {
        if n < self.min_args {
            return false;
        }
        if self.max_args >= 0 && n as isize > self.max_args {
            return false;
        }
        true
    }

    /// 该名字是否为本函数登记的具名参数。
    pub fn accepts_named(&self, key: &str) -> bool {
        self.named_params.iter().any(|(k, _)| *k == key)
    }

    /// 已登记的具名参数名连成一行，供报错提示直接嵌入。
    pub fn named_help(&self) -> String {
        self.named_params
            .iter()
            .map(|(k, _)| *k)
            .collect::<Vec<_>>()
            .join(" / ")
    }
}

/// 函数注册表（按名查找）。
#[derive(Default, Clone)]
pub struct Registry {
    specs: HashMap<&'static str, FuncSpec>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            specs: HashMap::new(),
        }
    }

    /// 注册（同名覆盖）。
    pub fn register(&mut self, spec: FuncSpec) {
        self.specs.insert(spec.name, spec);
    }

    pub fn register_all(&mut self, specs: impl IntoIterator<Item = FuncSpec>) {
        for s in specs {
            self.register(s);
        }
    }

    pub fn lookup(&self, name: &str) -> Option<&FuncSpec> {
        self.specs.get(name)
    }

    /// 全部已注册函数（设置 UI 渲染手册用；顺序无定，调用方自行排序）。
    pub fn list(&self) -> Vec<&FuncSpec> {
        self.specs.values().collect()
    }

    /// 仅纯函数 + meta（value/text/calc/help）。用于无宿主的纯模板求值/测试。
    pub fn with_builtins() -> Self {
        let mut r = Registry::new();
        r.register_all(crate::funcs::value::specs());
        r.register_all(crate::funcs::text::specs());
        r.register_all(crate::funcs::calc::specs());
        r.register_all(crate::funcs::help::specs());
        r
    }

    /// 纯函数 + 全部动作函数（宿主运行时用）。
    pub fn full() -> Self {
        let mut r = Self::with_builtins();
        r.register_all(crate::funcs::action::specs());
        r.register_all(crate::funcs::dict_ime::specs());
        r.register_all(crate::funcs::config::specs());
        r
    }
}

/// 进程级默认注册表（全函数），供 `help()` 等内省函数查询元信息。
pub fn default_registry() -> &'static Registry {
    static DEFAULT: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
    DEFAULT.get_or_init(Registry::full)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 白名单与求值入口必须成对声明。只声明白名单 = 用户写了参数、解析通过、
    /// 校验通过，最后在调用期才失败；只声明入口 = 白名单永远拒绝，入口从不被调用。
    /// 两种都是「半接线」，靠这条在 CI 拦住。
    #[test]
    fn named_params_and_eval_named_are_declared_together() {
        for s in Registry::full().list() {
            assert_eq!(
                s.named_params.is_empty(),
                s.eval_named.is_none(),
                "{}: named_params 与 eval_named 必须同时声明或同时省略",
                s.name
            );
        }
    }
}
