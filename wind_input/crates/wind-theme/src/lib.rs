//! wind-theme: 主题系统（YAML 主题加载、调色板、View 定义）
//!
//! 与 Go 版本 `wind_input/pkg/theme/` 对齐。

pub mod palette;
pub mod resolve;
pub mod rvnode;
pub mod schema;
pub mod theme;

pub use palette::Rgba;
pub use resolve::{Resolved, ResolvedBehavior, load_resolved, load_resolved_dirs, resolve};
pub use rvnode::{RvGradient, RvImage, RvNode, RvViews};
pub use schema::Meta;
pub use theme::{find_theme_dir, load_merged_dirs, meta_from_text, read_meta, validate_text};
