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
