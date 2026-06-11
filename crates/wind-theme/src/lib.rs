//! wind-theme: 主题系统（YAML 主题加载、调色板、View 定义）
//!
//! 与 Go 版本 `wind_input/pkg/theme/` 对齐。

pub mod bgimage;
pub mod manager;
pub mod palette;
pub mod resolved;
pub mod theme;
pub mod views;

pub use manager::ThemeManager;
pub use resolved::ResolvedV3;
pub use theme::Theme;
