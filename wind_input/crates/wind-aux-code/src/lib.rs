//! 辅助码过滤模块
//!
//! 为拼音/双拼候选提供字形层面的二次筛选：用户在拼音码之后追加输入辅助码
//! （通常是偏旁/部首/笔画对应的按键串），本模块据此裁减掉同音字中字形不匹配者。
//!
//! 过滤规则：
//! - 单字候选：查其辅助码并做前缀匹配，匹配不上或码表未收录一律滤掉（**不做字集判断**，
//!   非汉字单字有码同样命中，字集由输入法自身的选项在上游决定）
//! - 词组候选：逐字首码匹配——顺序输入每字的第一个辅助码（第 i 位命中第 i 字任一码的
//!   首字符；辅助码**可短于** N 位 = 前缀态，词组保留、边打边缩；**超过** N 位或某位
//!   不中 → 过滤）
//! - **词组长度上限**（`AuxCodeFilterOptions::max_phrase_len`，默认 0，0 = 不限）：字数
//!   > 上限的词组一律排除、不参与辅助码筛选——长词组（整词补全/组合词）首字辅助码前缀
//!   > 匹配会让它们大量残留、污染逐字词筛选；单字恒参与匹配
//! - 辅助码输入为空、或未挂载任何码表：不过滤（原样放行，防御语义——触发键进入的
//!   辅助码模式正常不会空手筛选，空输入/空表若参与筛选会把候选窗整个滤光）
//!
//! 数据存储参照 `wind-reverse::PinyinTable`（字→读音列表）的三段式紧凑布局：
//! - 单张码表 / 多张合并表统一用 [`AuxCodeTable`] 表示；
//! - 多表挂载（拆分 + 小鹤 + …）通过 [`AuxCodeTable::merge`] / [`AuxCodeTable::append`]
//!   一次性坍缩为合并表，查询阶段零额外开销（无需 flat_map / HashSet）。
//! - 数据文件：辅助码 txt 经 [`crate::loader::load_from_file`] 一次性载入（码表普遍很小，
//!   直接整体读取即可，路径由调用方解析）；懒加载时机由调用方决定——首次输入辅助码时才
//!   触发读取，本模块不持有加载状态。
//!
//! 核心不变量：
//! > `filter_by_aux_code` 输出的 `kept` 是输入候选的子序列——不会凭空加/丢后重排，
//! > 被保留候选的相对顺序与原列表完全一致，不得再做其它基于辅助码的重排。
//! > 注：主排序的首要键是消费长度（`by_consumed`，librime 对齐），会让低
//! > 词频长子短语排在短单字前——这是主排序的有意行为，本模块不纠正（见 filter 模块文档）。

pub mod filter;
pub mod loader;
pub mod session;
pub mod table;

pub use filter::{AuxCodeFilterOptions, aux_code_matches, filter_by_aux_code};
pub use loader::{load_from_file, load_merged};
pub use session::AuxCodeSession;
pub use table::AuxCodeTable;
