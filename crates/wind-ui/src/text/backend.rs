//! 文本渲染后端 trait
//!
//! 与 Go 版本 `wind_input/internal/ui/text_drawer.go` 中的 TextDrawer 对齐。

/// 文本渲染后端接口
pub trait TextBackend: Send + Sync {
    /// 测量文本宽度
    fn measure_text(&self, text: &str, font_size: f64) -> f64;

    /// 绘制文本到缓冲区
    fn draw_text(&self, text: &str, x: f64, y: f64, font_size: f64, color: u32);
}
