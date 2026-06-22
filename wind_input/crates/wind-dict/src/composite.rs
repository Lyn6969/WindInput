//! 多层复合词典
//!
//! 与 Go 版本 `wind_input/internal/dict/composite.go` 对齐。

use crate::layer::DictLayer;
use std::collections::HashMap;
use std::sync::RwLock;
use wind_candidate::Candidate;

/// 每层 natural_order 偏移：等权重时让**声明序更靠前的层**(主库先于扩展、用户先于系统)
/// 的候选排在前。取值需大于单层内最大 natural_order，使「层序」优先于「层内序」。
/// 与 Go composite.go `perLayerNOOffset` 同义。
const PER_LAYER_NO_OFFSET: i32 = 10_000_000;

/// 多层复合词典
#[derive(Default)]
pub struct CompositeDict {
    layers: RwLock<Vec<Box<dyn DictLayer>>>,
}

impl CompositeDict {
    pub fn new() -> Self {
        Self {
            layers: RwLock::new(Vec::new()),
        }
    }

    /// 注册词典层（按 layer_type 稳定排序：相同类型保持注册顺序，
    /// 故同为 System 的主库与扩展库按注册先后决定层内优先级）。
    pub fn register_layer(&self, layer: Box<dyn DictLayer>) {
        let mut layers = self.layers.write().unwrap();
        layers.push(layer);
        layers.sort_by_key(|l| l.layer_type() as u8);
    }

    /// 按名注销词典层
    pub fn unregister_layer(&self, name: &str) {
        let mut layers = self.layers.write().unwrap();
        layers.retain(|l| l.name() != name);
    }

    /// 运行时启停某层（按名）：用于码表扩展词库热插拔，无需重建引擎。
    /// 返回是否命中该层。仅需读锁（层的 enabled 是内部原子标志）。
    pub fn set_layer_enabled(&self, name: &str, enabled: bool) -> bool {
        let layers = self.layers.read().unwrap();
        let mut hit = false;
        for l in layers.iter() {
            if l.name() == name {
                l.set_enabled(enabled);
                hit = true;
            }
        }
        hit
    }

    /// 精确查找：跨层合并去重。
    pub fn search(&self, code: &str, limit: usize) -> Vec<Candidate> {
        self.merge_search(code, limit, false)
    }

    /// 前缀查找：跨层合并去重。
    pub fn search_prefix(&self, prefix: &str, limit: usize) -> Vec<Candidate> {
        self.merge_search(prefix, limit, true)
    }

    /// 跨层合并：遍历各层收集候选，按 text 去重——
    ///   - 保留**高优先级层**(先出现)的词条信息(code/natural_order)；
    ///   - 但**继承后续层中同 text 的更高权重**(用户词不因低权重丢失码表词的自然排序位)；
    ///   - 前缀查询时，同 text 多码取**最短码**(离输入最近)及其更小 natural_order；
    ///   - 每层叠加 `layer_idx * PER_LAYER_NO_OFFSET`，使无权重差时按层序排列。
    /// 与 Go composite.go `searchInternal` 对齐。
    fn merge_search(&self, query: &str, limit: usize, is_prefix: bool) -> Vec<Candidate> {
        let layers = self.layers.read().unwrap();
        let mut results: Vec<Candidate> = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();

        for (layer_idx, layer) in layers.iter().enumerate() {
            // 禁用层（如关闭的码表扩展词库）跳过；layer_idx 仍按位置计，
            // 保持启用层之间的层序偏移稳定。
            if !layer.enabled() {
                continue;
            }
            let layer_results = if is_prefix {
                layer.search_prefix(query, limit)
            } else {
                layer.search(query, limit)
            };
            let offset = (layer_idx as i32).saturating_mul(PER_LAYER_NO_OFFSET);
            for mut cand in layer_results {
                cand.natural_order = cand.natural_order.saturating_add(offset);
                if let Some(&idx) = seen.get(&cand.text) {
                    // 同 text 已存在：继承更高权重。注意 weight 可能来自后续低优先级层，而
                    // code/natural_order 仍保留首个出现层（高优先层）的值——跨层取值，刻意为之
                    // （对齐 Go searchInternal：用户词不因低权重丢失码表词的自然排序位）。
                    if cand.weight > results[idx].weight {
                        results[idx].weight = cand.weight;
                    }
                    // 前缀：保留最短码及其更早出现位置
                    if is_prefix && cand.code.len() < results[idx].code.len() {
                        results[idx].code = cand.code.clone();
                        if cand.natural_order < results[idx].natural_order {
                            results[idx].natural_order = cand.natural_order;
                        }
                    }
                    continue;
                }
                seen.insert(cand.text.clone(), results.len());
                results.push(cand);
            }
        }

        results.sort_by(wind_candidate::better);
        // limit==0 视为「无上限」（与各 DictLayer::search 的 `if limit>0` 守卫、Go
        // searchInternal 一致），仅在 limit>0 时截断。调用方需要空结果时不应传 0。
        if limit > 0 && results.len() > limit {
            results.truncate(limit);
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layer::LayerType;

    /// 测试用层：固定候选集，可指定 layer_type 与名字。
    struct MockLayer {
        name: String,
        ltype: LayerType,
        items: Vec<Candidate>, // (text, code, weight, natural_order) 预置
    }

    fn cand(text: &str, code: &str, weight: i32, no: i32) -> Candidate {
        Candidate {
            text: text.into(),
            code: code.into(),
            weight,
            natural_order: no,
            ..Default::default()
        }
    }

    impl DictLayer for MockLayer {
        fn name(&self) -> &str {
            &self.name
        }
        fn layer_type(&self) -> LayerType {
            self.ltype
        }
        fn search(&self, code: &str, _limit: usize) -> Vec<Candidate> {
            self.items.iter().filter(|c| c.code == code).cloned().collect()
        }
        fn search_prefix(&self, prefix: &str, _limit: usize) -> Vec<Candidate> {
            self.items
                .iter()
                .filter(|c| c.code.starts_with(prefix))
                .cloned()
                .collect()
        }
    }

    #[test]
    fn dedup_same_text_inherits_higher_weight() {
        let c = CompositeDict::new();
        // 主系统层：你 weight 100；扩展系统层：同 text「你」weight 500（更高）
        c.register_layer(Box::new(MockLayer {
            name: "system-main".into(),
            ltype: LayerType::System,
            items: vec![cand("你", "ni", 100, 0)],
        }));
        c.register_layer(Box::new(MockLayer {
            name: "system-extra".into(),
            ltype: LayerType::System,
            items: vec![cand("你", "ni", 500, 0)],
        }));
        let r = c.search("ni", 10);
        assert_eq!(r.len(), 1, "同 text 应去重为一条");
        assert_eq!(r[0].text, "你");
        assert_eq!(r[0].weight, 500, "应继承更高权重");
    }

    #[test]
    fn distinct_text_kept_and_layer_order_breaks_ties() {
        let c = CompositeDict::new();
        // 两层各一条不同 text、同权重：靠前层(先注册)的应排前(natural_order 偏移更小)
        c.register_layer(Box::new(MockLayer {
            name: "system-main".into(),
            ltype: LayerType::System,
            items: vec![cand("主", "x", 100, 0)],
        }));
        c.register_layer(Box::new(MockLayer {
            name: "system-extra".into(),
            ltype: LayerType::System,
            items: vec![cand("扩", "x", 100, 0)],
        }));
        let r = c.search("x", 10);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].text, "主", "等权重时靠前层优先");
        assert_eq!(r[1].text, "扩");
    }

    #[test]
    fn prefix_keeps_shortest_code_for_same_text() {
        let c = CompositeDict::new();
        // 同 text「好」在两层有不同码：hao(3) 与 h(1)；前缀查应保留最短码 h
        c.register_layer(Box::new(MockLayer {
            name: "system-main".into(),
            ltype: LayerType::System,
            items: vec![cand("好", "hao", 100, 5)],
        }));
        c.register_layer(Box::new(MockLayer {
            name: "system-extra".into(),
            ltype: LayerType::System,
            items: vec![cand("好", "h", 100, 9)],
        }));
        let r = c.search_prefix("h", 10);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].text, "好");
        assert_eq!(r[0].code, "h", "同 text 多码前缀查应保留最短码");
    }

    #[test]
    fn disabled_layer_skipped_and_hot_toggle() {
        struct Toggle {
            name: String,
            enabled: std::sync::atomic::AtomicBool,
            items: Vec<Candidate>,
        }
        impl DictLayer for Toggle {
            fn name(&self) -> &str {
                &self.name
            }
            fn layer_type(&self) -> LayerType {
                LayerType::System
            }
            fn enabled(&self) -> bool {
                self.enabled.load(std::sync::atomic::Ordering::Relaxed)
            }
            fn set_enabled(&self, e: bool) {
                self.enabled.store(e, std::sync::atomic::Ordering::Relaxed);
            }
            fn search(&self, code: &str, _l: usize) -> Vec<Candidate> {
                self.items.iter().filter(|c| c.code == code).cloned().collect()
            }
            fn search_prefix(&self, p: &str, _l: usize) -> Vec<Candidate> {
                self.items.iter().filter(|c| c.code.starts_with(p)).cloned().collect()
            }
        }
        let c = CompositeDict::new();
        c.register_layer(Box::new(MockLayer {
            name: "system-main".into(),
            ltype: LayerType::System,
            items: vec![cand("主", "e", 100, 0)],
        }));
        c.register_layer(Box::new(Toggle {
            name: "codetable-extra-emoji".into(),
            enabled: std::sync::atomic::AtomicBool::new(false), // 初始禁用
            items: vec![cand("😀", "e", 100, 0)],
        }));
        // 禁用时：扩展候选不出
        let r = c.search("e", 10);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].text, "主");
        // 热开启：无需重建，扩展候选即时出现
        assert!(c.set_layer_enabled("codetable-extra-emoji", true));
        let r = c.search("e", 10);
        assert_eq!(r.len(), 2, "开启后扩展候选应即时加入: {r:?}");
        assert!(r.iter().any(|c| c.text == "😀"));
        // 热关闭：又消失
        assert!(c.set_layer_enabled("codetable-extra-emoji", false));
        assert_eq!(c.search("e", 10).len(), 1);
        // 未命中的名字返回 false
        assert!(!c.set_layer_enabled("no-such-layer", true));
    }

    #[test]
    fn higher_priority_layer_type_wins_over_system() {
        let c = CompositeDict::new();
        // User 层权重低，但同 text 仍应继承 System 高权重，且只保留一条
        c.register_layer(Box::new(MockLayer {
            name: "system".into(),
            ltype: LayerType::System,
            items: vec![cand("中", "z", 900, 0)],
        }));
        c.register_layer(Box::new(MockLayer {
            name: "user".into(),
            ltype: LayerType::User,
            items: vec![cand("中", "z", 10, 0)],
        }));
        let r = c.search("z", 10);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].weight, 900, "去重后继承更高权重(无视层优先级)");
    }
}
