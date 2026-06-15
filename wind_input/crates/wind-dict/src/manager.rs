//! 词典管理器
//!
//! 与 Go 版本 `wind_input/internal/dict/manager.go` 对齐。

use crate::composite::CompositeDict;
use std::sync::Arc;

/// 词典管理器
pub struct DictManager {
    composite: Arc<CompositeDict>,
    // TODO: store, system layers, etc.
}

impl DictManager {
    pub fn new() -> Self {
        Self {
            composite: Arc::new(CompositeDict::new()),
        }
    }

    /// 切换活跃 schema 的词典层
    pub fn switch_schema(&self, _schema_id: &str) -> anyhow::Result<()> {
        // TODO
        Ok(())
    }

    pub fn composite(&self) -> &CompositeDict {
        &self.composite
    }
}
