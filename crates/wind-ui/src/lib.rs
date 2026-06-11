//! wind-ui: UI 渲染层（tiny-skia 渲染、Layered Window、多种窗口类型）
//!
//! 与 Go 版本 `wind_input/internal/ui/` 对齐。

pub mod candidate_window;
pub mod manager;
pub mod popup_menu;
pub mod renderer;
pub mod status;
pub mod text;
pub mod toast;
pub mod toolbar;
pub mod tooltip;
pub mod viewbox;
pub mod window;

pub use manager::UiManager;
