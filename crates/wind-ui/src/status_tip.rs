//! 状态提示气泡：切换中英/标点/全半角/方案时短暂显示当前状态。
//!
//! 与 Go 版本的 showModeIndicator / CmdStatusShow 对齐（简化版）。
//! 深色半透明圆角底 + 居中白字，显示约 1 秒后自动隐藏。

use crate::text::dwrite::TextRenderer;
use crate::window::LayeredWindow;

/// 状态提示气泡窗口
pub struct StatusTip {
    window: LayeredWindow,
    renderer: TextRenderer,
    bg: [u8; 4],
    fg: [u8; 4],
    pad_x: f32,
    pad_y: f32,
}

impl StatusTip {
    pub fn new() -> Result<Self, String> {
        let dpi_scale = Self::dpi_scale();
        let window = LayeredWindow::create(None, 200, 80, "WindInputStatusTip")?;
        let renderer = TextRenderer::new("Microsoft YaHei UI", 22.0 * dpi_scale)?;
        Ok(Self {
            window,
            renderer,
            bg: [40, 40, 40, 235],
            fg: [245, 245, 245, 255],
            pad_x: 18.0 * dpi_scale,
            pad_y: 10.0 * dpi_scale,
        })
    }

    /// 显示提示文本，居中于 (cx, cy) 附近
    pub fn show(&mut self, text: &str, cx: i32, cy: i32) {
        let m = self.renderer.measure_text(text);
        let w = (m.width + self.pad_x * 2.0).ceil().max(48.0) as u32;
        let h = (m.height + self.pad_y * 2.0).ceil().max(36.0) as u32;
        self.window.resize(w, h);

        let buf_size = (w * h * 4) as usize;
        let buf = self.window.buffer_mut();
        buf[..buf_size].fill(0);
        Self::fill_rounded(buf, w, h, self.bg, (h as f32 * 0.28) as u32);

        // 居中文字
        let tx = ((w as f32 - m.width) * 0.5).max(0.0);
        let ty = ((h as f32 - m.height) * 0.5).max(0.0);
        let _ = self.renderer.draw_text(buf, w, h, tx, ty, text, self.fg);

        if let Err(e) = self.window.update() {
            tracing::warn!("StatusTip update failed: {}", e);
        }
        // 显示在 caret 上方居中
        let x = cx - (w as i32) / 2;
        let y = cy - (h as i32) - 8;
        self.window.show(x.max(0), y.max(0));
    }

    pub fn hide(&self) {
        self.window.hide();
    }

    /// 系统 DPI 缩放因子
    fn dpi_scale() -> f32 {
        #[cfg(windows)]
        {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::Graphics::Gdi::*;
            unsafe {
                let hdc = GetDC(HWND::default());
                let dpi = GetDeviceCaps(hdc, LOGPIXELSY);
                ReleaseDC(HWND::default(), hdc);
                if dpi > 0 {
                    dpi as f32 / 96.0
                } else {
                    1.0
                }
            }
        }
        #[cfg(not(windows))]
        {
            1.0
        }
    }

    /// 绘制圆角填充矩形（四角按半径裁掉，模拟圆角）
    fn fill_rounded(buf: &mut [u8], w: u32, h: u32, color: [u8; 4], radius: u32) {
        let (wi, hi, r) = (w as i32, h as i32, radius as i32);
        for y in 0..hi {
            for x in 0..wi {
                // 圆角裁剪：四角圆心到像素距离 > r 则透明
                let inside = Self::corner_inside(x, y, wi, hi, r);
                if !inside {
                    continue;
                }
                let idx = ((y * wi + x) * 4) as usize;
                if idx + 3 < buf.len() {
                    buf[idx] = color[0];
                    buf[idx + 1] = color[1];
                    buf[idx + 2] = color[2];
                    buf[idx + 3] = color[3];
                }
            }
        }
    }

    fn corner_inside(x: i32, y: i32, w: i32, h: i32, r: i32) -> bool {
        if r <= 0 {
            return true;
        }
        // 各角圆心
        let corners = [
            (r, r, x < r && y < r),
            (w - 1 - r, r, x > w - 1 - r && y < r),
            (r, h - 1 - r, x < r && y > h - 1 - r),
            (w - 1 - r, h - 1 - r, x > w - 1 - r && y > h - 1 - r),
        ];
        for (cx, cy, in_quadrant) in corners {
            if in_quadrant {
                let dx = (x - cx) as f32;
                let dy = (y - cy) as f32;
                return dx * dx + dy * dy <= (r * r) as f32;
            }
        }
        true
    }
}
