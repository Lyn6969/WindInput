//! 热键编译器
//!
//! 与 Go 版本 `wind_input/internal/hotkey/compiler.go` 对齐。

use crate::config::Config;

/// 热键编译器
pub struct Compiler {
    config: Config,
}

impl Compiler {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// 编译配置中的热键为 KeyHash 列表
    ///
    /// 返回 (key_down_hashes, key_up_hashes)
    pub fn compile(&self) -> (Vec<u32>, Vec<u32>) {
        // TODO: 从配置编译热键
        (Vec::new(), Vec::new())
    }
}
