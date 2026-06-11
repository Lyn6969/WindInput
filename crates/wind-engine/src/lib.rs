//! wind-engine: 输入引擎（拼音、码表、混合）
//!
//! 与 Go 版本 `wind_input/internal/engine/` 对齐。

pub mod codetable;
pub mod engine;
pub mod manager;
pub mod mixed;
pub mod pinyin;

pub use engine::{Engine, EngineType, ExtendedEngine};
pub use manager::EngineManager;
