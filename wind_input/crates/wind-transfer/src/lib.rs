//! 导入导出/备份还原的复用底座:Bundle(manifest + zip)与 Merge 骨架。
//! 编解码在 wind-store(与 redb 表同处);本 crate 负责聚合打包与合并策略。
pub mod bundle;
// pub mod merge;  // Task 5 创建 merge.rs 时取消注释
