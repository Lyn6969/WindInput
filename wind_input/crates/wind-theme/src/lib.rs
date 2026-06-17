//! wind-theme: 主题系统（YAML 主题加载、调色板、View 定义）
//!
//! 与 Go 版本 `wind_input/pkg/theme/` 对齐。

pub mod bgimage;
pub mod manager;
pub mod palette;
pub mod resolve;
pub mod resolved;
pub mod rvnode;
pub mod schema;
pub mod theme;

pub use manager::ThemeManager;
pub use palette::Rgba;
pub use resolve::{Resolved, ResolvedBehavior};
pub use resolved::{Pad, ResolvedTheme};
pub use rvnode::{RvGradient, RvImage, RvNode, RvViews};
