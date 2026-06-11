//! 函数注册表
//!
//! 与 Go 版本 `wind_input/internal/cmdbar/registry.go` 对齐。

use std::collections::HashMap;

/// 函数规格
pub struct FuncSpec {
    pub name: String,
    pub arity: usize,
    pub is_pure: bool,
    pub description: String,
}

/// 函数注册表
pub struct Registry {
    funcs: HashMap<String, FuncSpec>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            funcs: HashMap::new(),
        }
    }

    pub fn register(&mut self, spec: FuncSpec) {
        self.funcs.insert(spec.name.clone(), spec);
    }

    pub fn get(&self, name: &str) -> Option<&FuncSpec> {
        self.funcs.get(name)
    }
}
