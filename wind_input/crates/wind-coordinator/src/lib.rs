//! wind-coordinator: 中央协调器（按键路由、候选管理、模式切换）
//!
//! 与 Go 版本 `wind_input/internal/coordinator/` 对齐。

pub mod coordinator;
pub mod handle_addword;
pub mod handle_candidate;
pub mod handle_cmdbar;
pub mod handle_config;
pub mod handle_key;
pub mod handle_lifecycle;
pub mod handle_mode;
pub mod handle_punct;
pub mod handle_temp;
pub mod handle_tooltip;
pub mod hotkey_match;
pub mod pipeline;
pub mod reverse;
pub mod stats;
pub mod watchdog;

pub use coordinator::{Coordinator, request_restart, restart_signal};
