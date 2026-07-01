//! 非 Windows/非 macOS mock 文本后端。
//!
//! 测量用等宽近似（字符数 × 字号 × 0.6），绘制为空操作。
//! 让候选窗/工具栏/菜单等布局逻辑能在 Linux 上编译与跑测试。
//! 真实字形宽度/渲染由各平台原生后端（dwrite / coretext）决定。

use super::backend::TextMetrics;

pub struct TextRenderer {
    font_size: f32,
}

impl TextRenderer {
    pub fn new(_font_family: &str, font_size: f32) -> Result<Self, String> {
        Ok(Self { font_size })
    }

    pub fn base_size(&self) -> f32 {
        self.font_size
    }

    pub fn set_base_size(&mut self, size: f32) {
        self.font_size = size;
    }

    pub fn set_font_family(&mut self, _font_family: &str) {}

    pub fn set_chaizi_font(&mut self, _path: &str) -> Result<(), String> {
        Ok(())
    }

    pub fn measure_text(&self, text: &str) -> TextMetrics {
        self.measure_text_sized(text, self.font_size)
    }

    pub fn measure_text_sized(&self, text: &str, size: f32) -> TextMetrics {
        if text.is_empty() {
            return TextMetrics {
                width: 0.0,
                height: size * 1.2,
            };
        }
        TextMetrics {
            width: text.chars().count() as f32 * size * 0.6,
            height: size * 1.2,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw_text(
        &self,
        _buf: &mut [u8],
        _buf_width: u32,
        _buf_height: u32,
        _x: f32,
        _y: f32,
        _text: &str,
        _color: [u8; 4],
    ) -> Result<(), String> {
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw_text_sized(
        &self,
        _buf: &mut [u8],
        _buf_width: u32,
        _buf_height: u32,
        _x: f32,
        _y: f32,
        _text: &str,
        _size: f32,
        _color: [u8; 4],
    ) -> Result<(), String> {
        Ok(())
    }
}

// mock 文本渲染器的冒烟测试：验证等宽近似测量契约
// （字符数 × 字号 × 0.6）与 draw_text 空操作。
#[cfg(test)]
mod tests {
    use super::TextRenderer;

    #[test]
    fn mock_measure_empty_is_zero_width() {
        let tr = TextRenderer::new("any", 20.0).unwrap();
        let m = tr.measure_text("");
        assert_eq!(m.width, 0.0);
        assert!(m.height > 0.0);
    }

    #[test]
    fn mock_measure_scales_with_char_count() {
        let tr = TextRenderer::new("any", 20.0).unwrap();
        let one = tr.measure_text("中").width;
        let three = tr.measure_text("中文字").width;
        assert!(three > one);
        assert!((three - one * 3.0).abs() < 1e-3);
    }

    #[test]
    fn mock_draw_text_is_ok() {
        let tr = TextRenderer::new("any", 16.0).unwrap();
        let mut buf = vec![0u8; 8 * 8 * 4];
        assert!(
            tr.draw_text(&mut buf, 8, 8, 0.0, 0.0, "x", [0, 0, 0, 255])
                .is_ok()
        );
    }
}
