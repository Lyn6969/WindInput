//! 引擎管理器
//!
//! 与 Go 版本 `wind_input/internal/engine/manager.go` 对齐。

use crate::engine::Engine;

/// 引擎管理器
pub struct EngineManager {
    // TODO: active engine, schema manager, dict manager
}

impl EngineManager {
    pub fn new() -> Self {
        Self {}
    }

    /// 切换 schema
    pub fn switch_schema(&self, _schema_id: &str) -> anyhow::Result<()> {
        // TODO
        Ok(())
    }
}
