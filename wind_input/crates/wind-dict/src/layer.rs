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

    /// 该层当前是否启用：禁用层在 composite 查询时被跳过（不出候选）。默认始终启用。
    /// 用于码表扩展词库的运行时热插拔——禁用的扩展层仍常驻（已 mmap），仅不参与查询。
    fn enabled(&self) -> bool {
        true
    }

    /// 运行时启停该层（支持热插拔的层覆盖此方法；默认 no-op）。
    /// 取 `&self`（内部用原子标志），故无需重建引擎即可即时生效。
    fn set_enabled(&self, _enabled: bool) {}

    /// 该层候选的 `natural_order` **基偏移**：合并各层时 `natural_order += base_order()`。
    /// 等权重（或 `base_sort = "natural"`）时决定**层间先后**——设计者在 `[[dictionaries]]`
    /// 配 `base_order`（如 50000 把某扩展库整体压到基础库之后）。默认 0 = 不偏移。
    ///
    /// 取代旧的 `PER_LAYER_NO_OFFSET`（按注册位置 × 常量）机制：偏移量由设计者显式配置，
    /// 不再依赖词库的注册/出现顺序。默认 0 意味着**未配置时各库不强制分带**（可能交错）。
    fn base_order(&self) -> i32 {
        0
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
