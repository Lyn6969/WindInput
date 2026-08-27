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
use wind_candidate::{Candidate, CandidateMeta, CandidateSource, better};

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

    /// 全量枚举各启用层的 `(code, text, weight)`，供离线索引构建。
    /// ⚠️ O(全表)，只在索引构建这类一次性场合调用，绝不能进按键链路。
    pub fn for_each_entry(&self, f: &mut dyn FnMut(&str, &str, i32)) {
        self.composite.for_each_entry(f);
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
    /// `Arc<str>` 而非 `String`：层名要随每条候选填进 `meta.weight_layer`（供调试段标出
    /// 权重来源），而候选在按键热路径上成百上千地造——克隆一次原子计数远比堆分配便宜。
    name: std::sync::Arc<str>,
    enabled: std::sync::atomic::AtomicBool,
    /// 层级基序档位（见 DictLayer::base_order）。设计者经 [[dictionaries]].base_order 配置。
    base_order: i32,
    /// 默认权重（`[[dictionaries]].default_weight`）：Some(w) 时**覆盖**本库所有条目的权重为 w。
    /// 用于**无权重的附加库**——与带权重主库合并、按权重排序时，让其条目落在设计者选定的权重档，
    /// 而非因 weight=0 全部沉底。None = 用词库自身权重。
    default_weight: Option<i32>,
    /// 权重归一化（方案级 `[weight_spec]`，施加到本方案全部词库层）：Some 时把权重映射回
    /// 约定值域 `0~WEIGHT_RANGE_MAX`，使其与短语权重同轴可比。None = 不归一化（守约词库的常态）。
    ///
    /// 与 `default_weight` 的分工：后者**抹平**整库权重（退化为文件顺序），前者**保序压缩**。
    /// 两者同时配时 `default_weight` 优先——它是更强的声明（「本库不参与权重排序」）。
    weight_norm: Option<crate::WeightNorm>,
    /// 越界告警的一次性闸门。见 [`SystemDictLayer::warn_out_of_range`]。
    over_range_warned: std::sync::atomic::AtomicBool,
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
            name: std::sync::Arc::from(name.into().as_str()),
            enabled: std::sync::atomic::AtomicBool::new(enabled),
            base_order: 0,
            default_weight: None,
            weight_norm: None,
            over_range_warned: std::sync::atomic::AtomicBool::new(false),
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
                None => {
                    if raw > crate::WEIGHT_RANGE_MAX {
                        self.warn_out_of_range(raw);
                    }
                    raw
                }
            },
        }
    }

    /// 「权重越界且未配归一化」的一次性告警。
    ///
    /// ## 为什么在**查询期**而不是加载期
    ///
    /// 加载期已有一份诊断（`ParseStats::log_weight_range`），但它只在**解析 yaml** 时触发，
    /// 而词库一旦建了 `.wdat` 缓存就直接 mmap、不再解析。于是「老词库 + 新版本」这个最需要
    /// 报警的组合**一次也不会响**（实测：首次加载报一次，删掉缓存才会再报）。
    /// 查询期看到的是真实流过的权重，绕开了缓存这一层。
    ///
    /// ## 为什么需要它
    ///
    /// 协调器已删除 `PHRASE_WEIGHT_BASE`(40M)，短语改按自身权重与码表候选同场竞争。
    /// 那个比较成立的前提是双方同轴：短语在 `0~10000`（默认 1000），本仓自产词库亦然
    /// （五笔主库 median 941 / max 9999）。而 Rime 生态导入的方案常是未归一的原始词频
    /// （虎码 p99=343,880），不配方案级 `[weight_spec]` 就会让短语全线沉底——**且没有任何
    /// 报错**，用户只看到「我的短语突然没了」。本告警是这条静默失效路径上唯一的提示。
    ///
    /// 只报一次：热路径上，且同一库的越界条目往往成千上万。
    /// 不打候选文本，只打词库名与权重数值（日志隐私红线）。
    #[cold]
    fn warn_out_of_range(&self, raw: i32) {
        use std::sync::atomic::Ordering;
        if self
            .over_range_warned
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            tracing::warn!(
                "词库 {} 出现越界权重 {}（约定值域 0~{}），且所属方案未配 [weight_spec]。\
                 短语权重在 0~{} 轴上，与之同场比较会被压到底。\
                 请跑 `wind_input dict weight-check` 体检并按建议配置方案级 [weight_spec]。",
                self.name,
                raw,
                crate::WEIGHT_RANGE_MAX,
                crate::WEIGHT_RANGE_MAX,
            );
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

    /// 委托给底层 `CachedDict`，并按本层的 `default_weight` / `weight_norm` 换算权重
    /// ——与 [`Self::search`] 同域（见 trait 文档）。
    fn for_each_entry(&self, f: &mut dyn FnMut(&str, &str, i32)) {
        self.dict.for_each_entry(&mut |code, text, weight| {
            f(code, text, self.effective_weight(weight));
        });
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
                // 归一化会改写 `weight`，原值留在这里——否则排查问题时看到的数与词库里
                // 的数对不上，而「候选权重为什么是这个」正是最常问的问题。
                // `weight_layer` 同理，但答的是另一个问题：这个权重出自**哪本词库**。
                // 跨层合并会让 weight 与 code 分属不同层，只看数字分不出来（见该字段文档）。
                meta: CandidateMeta {
                    raw_weight: hit.weight,
                    weight_layer: Some(self.name.clone()),
                    ..Default::default()
                },
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
                // 归一化会改写 `weight`，原值留在这里——否则排查问题时看到的数与词库里
                // 的数对不上，而「候选权重为什么是这个」正是最常问的问题。
                // `weight_layer` 同理，但答的是另一个问题：这个权重出自**哪本词库**。
                meta: CandidateMeta {
                    raw_weight: hit.weight,
                    weight_layer: Some(self.name.clone()),
                    ..Default::default()
                },
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

    /// 反向对照的基线：**不配 `default_weight` 时，扩展库的高权重如实参与跨库取最大值**。
    ///
    /// 与下一条配对存在。只有基线在，才能说明下一条的「没生效」是 `default_weight` 造成的，
    /// 而不是跨库合并本身没做——否则两种根因在测试上长得一模一样。
    #[test]
    fn cross_layer_max_weight_with_real_system_layers() {
        let mut main = CodetableDict::empty();
        main.merge_single("a".into(), "工".into(), 800, 0);
        let mut ext = CodetableDict::empty();
        ext.merge_single("a".into(), "工".into(), 9999, 0);

        let dm = DictManager::new();
        dm.register_layer(Box::new(SystemDictLayer::new(
            CachedDict::Memory(main),
            "main",
        )));
        dm.register_layer(Box::new(SystemDictLayer::new(
            CachedDict::Memory(ext),
            "ext",
        )));

        let r = dm.search("a", 10);
        assert_eq!(r.len(), 1, "同 text 跨层合并成一条");
        assert_eq!(r[0].weight, 9999, "扩展库的高权重应如实生效");
        assert_eq!(
            r[0].meta.raw_weight, 800,
            "raw_weight 留的是首个出现层（主库）的库内原值，只有 weight 是跨层继承的——\
             排查时若只看 raw_weight 会误判成「扩展库权重没进来」"
        );
        assert_eq!(
            r[0].meta.weight_layer.as_deref(),
            Some("ext"),
            "权重来源须随权重一起换到扩展库（悬停调试段据此显示 `权 9999 ←ext`）"
        );
    }

    /// **`default_weight` 先抹平本库权重，再参与跨库取最大值。**
    ///
    /// 这是「扩展词库明明权重更高却没生效」的一条真实路径：库文件里写着 9999，但
    /// `[[dictionaries]].default_weight` 把整库压成了一个档位，参与比较的是那个档位。
    /// 换一个没配该项的词库就复现不出来——正是「同一版本、有人中招有人没有」的成因之一。
    ///
    /// 与归一化的分工别搞混：`weight_spec` 是**保序压缩**（不颠倒库内高低），
    /// `default_weight` 是**抹平**（库内高低全部消失）。
    #[test]
    fn default_weight_flattens_before_cross_layer_max() {
        let mut main = CodetableDict::empty();
        main.merge_single("a".into(), "工".into(), 800, 0);
        let mut ext = CodetableDict::empty();
        ext.merge_single("a".into(), "工".into(), 9999, 0); // 库里权重远高于主库

        let dm = DictManager::new();
        dm.register_layer(Box::new(SystemDictLayer::new(
            CachedDict::Memory(main),
            "main",
        )));
        dm.register_layer(Box::new(
            SystemDictLayer::new(CachedDict::Memory(ext), "ext").with_default_weight(Some(100)),
        ));

        let r = dm.search("a", 10);
        assert_eq!(r.len(), 1);
        assert_eq!(
            r[0].weight, 800,
            "扩展库配了 default_weight=100，参与比较的是 100 而非词条原值 9999，\
             故主库的 800 胜出"
        );
        assert_eq!(
            r[0].meta.weight_layer.as_deref(),
            Some("main"),
            "主库胜出时来源标注须留在主库——标错会把排查引向扩展库"
        );
    }

    /// 禁用的扩展库**完全不参与** max：关掉后回退到剩余启用库，开回来即恢复。
    /// 与 composite 层的同名语义同轴，这里用真实 `SystemDictLayer` 再验一遍——
    /// 层的 `enabled` 是它自己的原子标志，composite 的 mock 层验不到这份实现。
    #[test]
    fn disabled_system_layer_excluded_from_cross_layer_max() {
        let mut main = CodetableDict::empty();
        main.merge_single("a".into(), "工".into(), 800, 0);
        let mut ext = CodetableDict::empty();
        ext.merge_single("a".into(), "工".into(), 9999, 0);

        let dm = DictManager::new();
        dm.register_layer(Box::new(SystemDictLayer::new(
            CachedDict::Memory(main),
            "main",
        )));
        // 扩展库初始即关闭（对齐方案配置 enabled=false 的加载路径）。
        dm.register_layer(Box::new(SystemDictLayer::with_enabled(
            CachedDict::Memory(ext),
            "ext",
            false,
        )));

        assert_eq!(
            dm.search("a", 10)[0].weight,
            800,
            "扩展库关闭时不得把 9999 带进来"
        );
        assert!(dm.set_layer_enabled("ext", true));
        assert_eq!(dm.search("a", 10)[0].weight, 9999, "开启后即时生效");
        assert!(dm.set_layer_enabled("ext", false));
        let back = dm.search("a", 10);
        assert_eq!(back[0].weight, 800, "再关掉应回退，不留残值");
        assert_eq!(
            back[0].meta.weight_layer.as_deref(),
            Some("main"),
            "来源标注须跟着回退，不得残留已关闭的 ext"
        );
    }
}
