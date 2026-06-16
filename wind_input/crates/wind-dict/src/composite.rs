//! 多层复合词典
//!
//! 与 Go 版本 `wind_input/internal/dict/composite.go` 对齐。

use crate::layer::DictLayer;
use std::sync::RwLock;
use wind_candidate::Candidate;

/// 多层复合词典
pub struct CompositeDict {
    layers: RwLock<Vec<Box<dyn DictLayer>>>,
}

impl CompositeDict {
    pub fn new() -> Self {
        Self {
            layers: RwLock::new(Vec::new()),
        }
    }

    /// 注册词典层
    pub fn register_layer(&self, layer: Box<dyn DictLayer>) {
        let mut layers = self.layers.write().unwrap();
        layers.push(layer);
        layers.sort_by_key(|l| l.layer_type() as u8);
    }

    /// 按类型注销词典层
    pub fn unregister_layer(&self, name: &str) {
        let mut layers = self.layers.write().unwrap();
        layers.retain(|l| l.name() != name);
    }

    /// 精确查找
    pub fn search(&self, code: &str, limit: usize) -> Vec<Candidate> {
        let layers = self.layers.read().unwrap();
        let mut results = Vec::new();
        for layer in layers.iter() {
            results.extend(layer.search(code, limit));
        }
        results.sort_by(wind_candidate::better);
        results.truncate(limit);
        results
    }

    /// 前缀查找
    pub fn search_prefix(&self, prefix: &str, limit: usize) -> Vec<Candidate> {
        let layers = self.layers.read().unwrap();
        let mut results = Vec::new();
        for layer in layers.iter() {
            results.extend(layer.search_prefix(prefix, limit));
        }
        results.sort_by(wind_candidate::better);
        results.truncate(limit);
        results
    }
}
