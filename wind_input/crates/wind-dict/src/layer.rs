//! 词典层接口
//!
//! 与 Go 版本 `wind_input/internal/dict/layer.go` 对齐。

use wind_candidate::Candidate;

/// 词典层类型（数值越小优先级越高）。
/// 注：Shadow（置顶/删除）**不是查询层**，而是 ShadowProvider，在引擎排序后应用
/// （见 docs/redesign/dict.md §2）——故不在此枚举中。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum LayerType {
    Logic = 0,  // 命令（日期、UUID）
    User = 1,   // 用户自造词
    Temp = 2,   // 临时学习词
    Cell = 3,   // 单元词典
    System = 4, // 系统主词典
}

/// 词典层接口
pub trait DictLayer: Send + Sync {
    /// 层名称
    fn name(&self) -> &str;

    /// 层类型
    fn layer_type(&self) -> LayerType;

    /// 精确查找
    fn search(&self, code: &str, limit: usize) -> Vec<Candidate>;

    /// 前缀查找
    fn search_prefix(&self, prefix: &str, limit: usize) -> Vec<Candidate>;

    /// **按声母串查找**（简拼召回）：返回该层里声母投影等于 `abbrev` 的词条。
    ///
    /// 返回的是**超集**，调用方仍须逐条过自己的判据（音节数、逐段全等、混合模式校验）。
    /// 本方法只负责把「候选集」从全层缩到一个声母组——判据一律留在引擎侧，
    /// 这样索引不会悄悄改变简拼的语义，只改变取候选的代价。
    ///
    /// 默认返回空：只有背后有声母索引的层才实现（当前是 `StoreUserLayer` /
    /// `StoreTempLayer`）。系统词库层走的是另一条路——`CachedDict::search_abbrev`
    /// 查 wdat 的 `AbbrevSection` 拿到**码**再回查，由引擎直接调用，不经本层接口。
    ///
    /// ⚠️ 默认实现返回空**而不是回退到全层枚举**：静默的全表扫正是简拼卡顿的根因，
    /// 与其让新层不知不觉继承那个代价，不如让它召不回、在测试里立刻暴露。
    fn search_abbrev(&self, _abbrev: &str, _limit: usize) -> Vec<Candidate> {
        Vec::new()
    }

    /// 该层是否存在**严格长于** `prefix` 的编码——「更长后继」存在性判据，供上屏安全阀
    /// （自动上屏 / 满码清空 / 顶码）使用：还能接着打就别急着替用户上屏。
    ///
    /// 默认实现沿用「取一批前缀候选再看有没有更长 code」的老办法，仅为无法廉价判断的层
    /// （redb / 内存 trie）兜底。它**受 limit 截断影响**：更长编码的候选权重偏低被挤出
    /// 前 64 名时会漏判成 false。由有序结构支撑的层应覆盖本方法直接问索引，
    /// 见 `SystemDictLayer`。
    fn has_longer_code(&self, prefix: &str) -> bool {
        let n = prefix.chars().count();
        self.search_prefix(prefix, 64)
            .iter()
            .any(|c| c.code.chars().count() > n)
    }

    /// 该层当前是否启用：禁用层在 composite 查询时被跳过（不出候选）。默认始终启用。
    /// 用于码表扩展词库的运行时热插拔——禁用的扩展层仍常驻（已 mmap），仅不参与查询。
    fn enabled(&self) -> bool {
        true
    }

    /// 运行时启停该层（支持热插拔的层覆盖此方法；默认 no-op）。
    /// 取 `&self`（内部用原子标志），故无需重建引擎即可即时生效。
    fn set_enabled(&self, _enabled: bool) {}

    /// 该层候选的**层级基序档位**：排序时 `base_order` 作为独立层级（weight 之后、
    /// natural_order 之前，见 `candidate::better`/`by_natural`），值越小越靠前。
    ///
    /// 默认按**层类型**给小整数档位：非系统层（命令/用户词/临时词/单元）恒排在系统词库层
    /// 之前（等权时）。因是独立排序层级（非加进 natural_order），**小整数即可**分档——`-1`
    /// 就能排在 `0` 前，与 natural_order 大小无关，无需魔法常量。系统层默认 0，由
    /// `SystemDictLayer` 覆盖为 `[[dictionaries]].base_order`（设计者配 0/1/2… 小整数）。
    fn base_order(&self) -> i32 {
        match self.layer_type() {
            LayerType::Logic => -4,
            LayerType::User => -3,
            LayerType::Temp => -2,
            LayerType::Cell => -1,
            LayerType::System => 0,
        }
    }
}

/// 可变词典层接口
pub trait MutableLayer: DictLayer {
    /// 添加词条
    fn add(&mut self, code: &str, text: &str, weight: i32) -> anyhow::Result<()>;

    /// 删除词条
    fn remove(&mut self, code: &str, text: &str) -> anyhow::Result<()>;

    /// 更新词条权重
    fn update(&mut self, code: &str, text: &str, new_weight: i32) -> anyhow::Result<()>;

    /// 保存到持久化存储
    fn save(&self) -> anyhow::Result<()>;
}
