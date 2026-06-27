//! wind-config: 配置系统（TOML 三层合并、Schema YAML 定义、热键编译）
//!
//! 与 Go 版本 `wind_input/pkg/config/` 和 `wind_input/internal/schema/` 对齐。

pub mod app_compat;
pub mod config;
pub mod config_schema;
pub mod hotkey;
pub mod runtime_state;
pub mod schema;
pub mod variant;

pub use config::{
    CodeCommitConfig, Config, ModeIndicatorStyle, PinyinFuzzy, PinyinGlobalConfig, PreeditDisplay,
};
pub use runtime_state::RuntimeState;
pub use schema::Schema;
