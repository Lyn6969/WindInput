//! wind-dict: 词典系统（多层复合词典、mmap 二进制格式、DAT 格式）
//!
//! 与 Go 版本 `wind_input/internal/dict/` 对齐。

pub mod binformat;
pub mod codetable;
pub mod composite;
pub mod datformat;
pub mod hotcache;
pub mod layer;
pub mod manager;
pub mod store_layer;
pub mod trie;

pub use composite::CompositeDict;
pub use layer::{DictLayer, LayerType, MutableLayer};
pub use manager::DictManager;
