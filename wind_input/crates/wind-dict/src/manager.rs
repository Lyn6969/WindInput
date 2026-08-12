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

    /// 按声母串查找（简拼召回）。返回超集，判据仍在引擎侧，
    /// 见 [`crate::layer::DictLayer::search_abbrev`]。
    pub fn search_abbrev(&self, abbrev: &str, limit: usize) -> Vec<Candidate> {
        self.composite.search_abbrev(abbrev, limit)
    }

    /// 是否存在**严格长于** `prefix` 的编码（跨层，见 `DictLayer::has_longer_code`）。
    pub fn has_longer_code(&self, prefix: &str) -> bool {
        self.composite.has_longer_code(prefix)
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
    /// 层级基序档位（见 DictLayer::base_order）。设计者经 [[dictionaries]].base_order 配置。
    base_order: i32,
    /// 默认权重（`[[dictionaries]].default_weight`）：Some(w) 时**覆盖**本库所有条目的权重为 w。
    /// 用于**无权重的附加库**——与带权重主库合并、按权重排序时，让其条目落在设计者选定的权重档，
    /// 而非因 weight=0 全部沉底。None = 用词库自身权重。
    default_weight: Option<i32>,
    /// 权重归一化（`[dictionaries.weight_spec]`）：Some 时把本库权重映射回约定值域
    /// `0~WEIGHT_RANGE_MAX`，使其与短语权重同轴可比。None = 不归一化（守约词库的常态）。
    ///
    /// 与 `default_weight` 的分工：后者**抹平**整库权重（退化为文件顺序），前者**保序压缩**。
    /// 两者同时配时 `default_weight` 优先——它是更强的声明（「本库不参与权重排序」）。
    weight_norm: Option<crate::WeightNorm>,
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
            default_weight: None,
            weight_norm: None,
        }
    }

    /// 链式设置层基序档位（`[[dictionaries]].base_order`）。默认 0。
    pub fn with_base_order(mut self, base_order: i32) -> Self {
        self.base_order = base_order;
        self
    }

    /// 链式设置默认权重（`[[dictionaries]].default_weight`）。Some(w) 覆盖本库所有条目权重。
    pub fn with_default_weight(mut self, default_weight: Option<i32>) -> Self {
        self.default_weight = default_weight;
        self
    }

    /// 链式设置权重归一化（`[dictionaries.weight_spec]`）。见字段文档与
    /// `docs/design/dict-weight-normalization.md`。
    pub fn with_weight_norm(mut self, weight_norm: Option<crate::WeightNorm>) -> Self {
        self.weight_norm = weight_norm;
        self
    }

    /// 本层最终对外的权重：`default_weight` 覆盖 > `weight_norm` 归一化 > 词库原值。
    ///
    /// ⚠️ 在**查询时**换算而非加载时改写：词库可能是 mmap 共享的只读产物（wdat），改不得；
    /// 且同一份词库可被多个方案以不同 `weight_spec` 引用。每次查询至多 `limit` 条、
    /// 每条两次 `ln`，开销可忽略。
    #[inline]
    fn effective_weight(&self, raw: i32) -> i32 {
        match self.default_weight {
            Some(w) => w,
            None => match &self.weight_norm {
                Some(n) => n.apply(raw),
                None => raw,
            },
        }
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
        // 用 search_with_boundary 而非 search：把词典的音节真值边界带进候选（wdat v4）。
        // 非拼音词库（五笔等）与旧格式的 boundary 恒 0，消费方据此降级，行为不变。
        let mut v: Vec<Candidate> = self
            .dict
            .search_with_boundary(code)
            .into_iter()
            .map(|hit| Candidate {
                text: hit.text,
                code: code.to_string(),
                weight: self.effective_weight(hit.weight),
                natural_order: hit.order,
                boundary: hit.boundary,
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
            .search_prefix_with_boundary(prefix, limit)
            .into_iter()
            .map(|hit| Candidate {
                text: hit.text,
                code: hit.code,
                weight: self.effective_weight(hit.weight),
                natural_order: hit.order,
                boundary: hit.boundary,
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

    /// 直接问底层有序索引（DAT 转移表 / BTreeMap），不物化任何候选。
    /// 覆盖 trait 默认实现，规避其 limit 截断导致的漏判。
    fn has_longer_code(&self, prefix: &str) -> bool {
        self.dict.has_longer_code(prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codetable::CodetableDict;
    use std::io::Write;

    /// 系统层须把词典的音节边界带进候选（精确 + 前缀两条路）。
    /// 回归防线：boundary 曾在 SystemDictLayer::search 处被 `dict.search()` 三元组截断，
    /// 使 search_with_boundary 沦为全仓无消费方的死代码。
    #[test]
    fn system_layer_carries_boundary_into_candidates() {
        let path = std::env::temp_dir().join("wind_layer_boundary.dict.yaml");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "---\nname: py\n...").unwrap();
            writeln!(f, "你好\tni hao\t1200").unwrap();
            writeln!(f, "你\tni\t800").unwrap();
        }
        let d = CodetableDict::load(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let layer = SystemDictLayer::new(CachedDict::Memory(d), "py-system");

        let exact = layer.search("nihao", 10);
        let hao = exact.iter().find(|c| c.text == "你好").expect("应命中你好");
        assert_eq!(hao.boundary, 0b101, "候选应带 ni|hao 的真值边界");

        // 前缀补全（输入 ni → 补出「你好」）同样要带边界，供双拼校验。
        let pre = layer.search_prefix("ni", 10);
        let hao_p = pre
            .iter()
            .find(|c| c.text == "你好")
            .expect("前缀应补出你好");
        assert_eq!(hao_p.boundary, 0b101, "前缀候选也应带边界");
        assert_eq!(hao_p.code, "nihao", "前缀候选的 code 应是完整码");
    }

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

    #[test]
    fn default_weight_overrides_all_entry_weights() {
        // 无权重附加库场景：default_weight 覆盖本库所有条目权重，同权后按 natural_order 排。
        let mut d = CodetableDict::empty();
        d.merge_single("a".into(), "甲".into(), 5, 0); // order 0
        d.merge_single("a".into(), "乙".into(), 999, 1); // order 1（权重更高，但会被覆盖）
        let layer =
            SystemDictLayer::new(CachedDict::Memory(d), "ext").with_default_weight(Some(100));
        let r = layer.search("a", 10);
        assert_eq!(r.len(), 2);
        assert!(
            r.iter().all(|c| c.weight == 100),
            "default_weight 应覆盖所有条目权重"
        );
        assert_eq!(r[0].text, "甲", "覆盖同权后按 natural_order 出现序");
        assert_eq!(r[1].text, "乙");
    }
}
