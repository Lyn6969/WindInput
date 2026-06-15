//! wind-engine: 输入引擎（拼音、码表、混合）
//!
//! 与 Go 版本 `wind_input/internal/engine/` 对齐。

pub mod codetable;
pub mod engine;
pub mod manager;
pub mod mixed;
pub mod pinyin;

pub use codetable::CodeTableEngine;
pub use engine::{ConvertResult, Engine, EngineType, ExtendedEngine};
pub use manager::EngineManager;
pub use pinyin::PinyinEngine;
