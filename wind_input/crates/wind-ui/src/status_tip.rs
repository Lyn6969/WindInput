//! 状态提示气泡：切换中英/标点/全半角/方案时短暂显示当前状态。
//!
//! 与 Go 版本的 showModeIndicator / CmdStatusShow 对齐（简化版）。
//! 统一到 View 盒模型 + DirectWrite：深色半透明圆角底 + 居中白字，约 1 秒后自动隐藏。

use crate::text::dwrite::TextRenderer;
use crate::view::{Align, Edges, View};
use crate::window::LayeredWindow;

/// 状态提示气泡窗口
pub struct StatusTip {
    window: LayeredWindow,
    renderer: TextRenderer,
    scale: f32,
    bg: [u8; 4],
    fg: [u8; 4],
}

impl StatusTip {
    pub fn new() -> Result<Self, String> {
        let scale = Self::dpi_scale();
        let window = LayeredWindow::create(None, 200, 80, "WindInputStatusTip")?;
        let renderer = TextRenderer::new("Microsoft YaHei UI", 22.0 * scale)?;
        Ok(Self {
            window,
            renderer,
            scale,
            bg: [40, 40, 40, 235],
            fg: [245, 245, 245, 255],
        })
    }

    /// 应用主题（状态气泡底色/文字色）。
    pub fn set_theme(&mut self, theme: &wind_theme::Resolved) {
        self.bg = theme.color("status_bg", self.bg);
        self.fg = theme.color("status_text", self.fg);
    }

    /// 显示提示文本，居中于 (cx, cy) 上方
    pub fn show(&mut self, text: &str, cx: i32, cy: i32) {
        let s = self.scale;
        // 单个 View 叶子即气泡：背景 + 圆角 + 内边距 + 居中文字
        let mut tip = View::leaf(text, self.fg)
            .bg(self.bg)
            .pad(Edges::xy(18.0 * s, 10.0 * s))
            .text_align(Align::Center);
        // 圆角随高度（估算字高 + 内边距）
        tip.corner_radius = (self.renderer.measure_text("国").height + 20.0 * s) * 0.28;

        tip.layout(0.0, 0.0, &self.renderer);
        let (w_f, h_f) = tip.measured_size();
        let w = (w_f.ceil() as u32).max(48);
        let h = (h_f.ceil() as u32).max(36);

        self.window.resize(w, h);
        {
            let buf = self.window.buffer_mut();
            let n = (w * h * 4) as usize;
            buf[..n].fill(0);
            tip.paint(buf, w, h, &self.renderer);
        }
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
}
