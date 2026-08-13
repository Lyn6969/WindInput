//! 文本渲染后端
//!
//! 与 Go 版本 `wind_input/internal/ui/text_drawer*.go` 对齐。

pub mod dwrite;

// macOS：CoreText 真字形后端，提供与 dwrite 同契约的 TextRenderer（dwrite.rs 在
// target_os="macos" 下 re-export 它），让候选窗在 mac 上渲染真实汉字（非 mock 桩）。
#[cfg(target_os = "macos")]
pub mod coretext;
