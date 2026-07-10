//! 词典管理器：拥有 CompositeDict，作为引擎的统一查询面。
//!
//! 见 docs/redesign/dict.md。Rust 采用"每方案引擎各持一个 composite"的模型
//! （EngineManager 已按方案缓存引擎），故 DictManager 是每方案 composite 的持有者 +
//! 查询入口，而非 Go 那种"单 composite + 切换时换层"。
//!
//! 词频（排序独立维度，frequency.md）与 shadow（Provider，引擎排序后应用）不在查询层。

use crate::cached::CachedDict;
use crate::composite::CompositeDict;
use crate::layer::{DictLayer, LayerType};
use wind_candidate::{Candidate, CandidateSource, better};

/// 词典管理器：持有一个方案的多层复合词典。
#[derive(Default)]
pub struct DictManager {
    composite: CompositeDict,
}

impl DictManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一层（System/User/Temp…）。
    pub fn register_layer(&self, layer: Box<dyn DictLayer>) {
        self.composite.register_layer(layer);
    }

    /// 按名注销一层。
    pub fn unregister_layer(&self, name: &str) {
        self.composite.unregister_layer(name);
    }

    /// 运行时启停某层（按名），用于码表扩展词库热插拔。返回是否命中。
    pub fn set_layer_enabled(&self, name: &str, enabled: bool) -> bool {
        self.composite.set_layer_enabled(name, enabled)
    }

    /// 精确查找（跨层合并、排序、截断）。
    pub fn search(&self, code: &str, limit: usize) -> Vec<Candidate> {
        self.composite.search(code, limit)
    }

    /// 前缀查找。
    pub fn search_prefix(&self, prefix: &str, limit: usize) -> Vec<Candidate> {
        self.composite.search_prefix(prefix, limit)
    }

    pub fn composite(&self) -> &CompositeDict {
        &self.composite
    }
}

/// 系统主词库层：把不可变 mmap `CachedDict` 包成 DictLayer（System 优先级最低）。
/// 候选 source 不在此设定（系统词库被码表/拼音引擎共用），由引擎按自身类型标注。
///
/// `enabled` 为原子标志：码表扩展词库支持运行时热插拔——禁用的扩展层仍常驻（已 mmap），
/// 仅在查询时被 composite 跳过；`set_enabled` 取 `&self`，故无需重建引擎即可即时启停。
pub struct SystemDictLayer {
    dict: CachedDict,
    name: String,
    enabled: std::sync::atomic::AtomicBool,
    /// natural_order 层基偏移（见 DictLayer::base_order）。设计者经 [[dictionaries]].base_order 配置。
    base_order: i32,
}

impl SystemDictLayer {
    /// 默认启用。
    pub fn new(dict: CachedDict, name: impl Into<String>) -> Self {
        Self::with_enabled(dict, name, true)
    }

    /// 指定初始启用状态（码表扩展层按方案配置的 enabled 传入）。
    pub fn with_enabled(dict: CachedDict, name: impl Into<String>, enabled: bool) -> Self {
        Self {
            dict,
            name: name.into(),
            enabled: std::sync::atomic::AtomicBool::new(enabled),
            base_order: 0,
        }
    }

    /// 链式设置层基偏移（`[[dictionaries]].base_order`）。默认 0。
    pub fn with_base_order(mut self, base_order: i32) -> Self {
        self.base_order = base_order;
        self
    }

    /// 系统层条目总数（日志/调试用）。
    pub fn len(&self) -> usize {
        self.dict.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dict.is_empty()
    }
}

impl DictLayer for SystemDictLayer {
    fn name(&self) -> &str {
        &self.name
    }

    fn layer_type(&self) -> LayerType {
        LayerType::System
    }

    fn enabled(&self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn set_enabled(&self, enabled: bool) {
        self.enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    fn base_order(&self) -> i32 {
        self.base_order
    }

    fn search(&self, code: &str, limit: usize) -> Vec<Candidate> {
        let mut v: Vec<Candidate> = self
            .dict
            .search(code)
            .into_iter()
            .map(|(text, weight, order)| Candidate {
                text,
                code: code.to_string(),
                weight,
                natural_order: order,
                source: CandidateSource::None,
                ..Default::default()
            })
            .collect();
        v.sort_by(better);
        if limit > 0 {
            v.truncate(limit);
        }
        v
    }

    fn search_prefix(&self, prefix: &str, limit: usize) -> Vec<Candidate> {
        let mut v: Vec<Candidate> = self
            .dict
            .search_prefix(prefix, limit)
            .into_iter()
            .map(|(code, text, weight, order)| Candidate {
                text,
                code,
                weight,
                natural_order: order,
                source: CandidateSource::None,
                ..Default::default()
            })
            .collect();
        v.sort_by(better);
        if limit > 0 {
            v.truncate(limit);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codetable::CodetableDict;

    #[test]
    fn test_system_layer_via_manager() {
        // 用内存码表词典构造系统层（避免依赖文件）
        let mut d = CodetableDict::empty();
        d.merge_single("a".into(), "工".into(), 100, 0);
        d.merge_single("a".into(), "戈".into(), 50, 1);
        d.merge_single("aa".into(), "式".into(), 30, 0);
        let cached = CachedDict::Memory(d);

        let dm = DictManager::new();
        dm.register_layer(Box::new(SystemDictLayer::new(cached, "codetable-system")));

        let exact = dm.search("a", 10);
        assert_eq!(exact.len(), 2);
        assert_eq!(exact[0].text, "工", "权重高者在前");
        // 前缀 a → a/aa
        assert!(dm.search_prefix("a", 10).len() >= 3);
    }
}
