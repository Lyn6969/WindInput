//! wind-config: 配置系统（TOML 三层合并、Schema YAML 定义、热键编译）
//!
//! 与 Go 版本 `wind_input/pkg/config/` 和 `wind_input/internal/schema/` 对齐。

pub mod app_compat;
pub mod change_hook;
pub mod code_charset;
pub mod config;
pub mod config_schema;
pub mod dir_var;
pub mod hotkey;
pub mod runtime_state;
pub mod schema;
pub mod startup_trace;
pub mod variant;

pub use code_charset::{CodeCharSet, CodeCharSetError};
pub use config::{
    BoundAction, CodetableGlobal, Config, LangBarConfig, LayoutIntent, MixGlobal,
    ModeIndicatorStyle, PinyinFuzzy, PinyinGlobalConfig, PreeditDisplay, SessionAction,
    TopCommitMode,
};
pub use dir_var::{dir_var, dir_var_help, dir_var_names, dir_var_str, is_dir_var};
pub use runtime_state::RuntimeState;
pub use schema::{OverlaySpec, Schema};
