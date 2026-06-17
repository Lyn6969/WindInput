//! 候选悬停提示气泡（编码反查）：悬停候选时显示其编码/拼音，教学如何输入。
//!
//! 与 Go 版本 `wind_input/internal/ui/tooltip.go` 对齐（简化版）。
//! 深色圆角小气泡 + DirectWrite 文本。

use crate::text::dwrite::TextRenderer;
use crate::view::{Align, Edges, View, ViewImage, ViewLayer};
use crate::window::LayeredWindow;

const FONT_PX: f32 = 13.0;
const BG: [u8; 4] = [60, 60, 64, 240]; // 深灰底（RGBA）
const FG: [u8; 4] = [240, 240, 245, 255];

/// 提示气泡窗口
pub struct Tooltip {
    window: LayeredWindow,
    renderer: TextRenderer,
    scale: f32,
    visible: bool,
    bg: [u8; 4],
    fg: [u8; 4],
    /// 主题位图背景 + z 层（jidian tooltip 吃九宫格 panel + 角标水印）。
    bg_image: Option<ViewImage>,
    layers: Vec<ViewLayer>,
}

impl Tooltip {
    pub fn new() -> Result<Self, String> {
        let scale = dpi_scale();
        let window = LayeredWindow::create(None, 120, 40, "WindInputTooltip")?;
        let renderer = TextRenderer::new("Microsoft YaHei UI", FONT_PX * scale)?;
        Ok(Self {
            window,
            renderer,
            scale,
            visible: false,
            bg: BG,
            fg: FG,
            bg_image: None,
            layers: Vec::new(),
        })
    }

    /// 应用主题（tooltip 底色/文字色 + 位图背景/层）。
    pub fn set_theme(&mut self, theme: &wind_theme::Resolved) {
        self.bg = theme.color("tooltip_bg", BG);
        self.fg = theme.color("tooltip_text", FG);
        if let Some(node) = &theme.views.tooltip {
            self.bg_image = crate::theme_assets::rv_image(theme, node.bg_image.as_ref());
            self.layers = crate::theme_assets::rv_layers(theme, &node.layers, self.scale);
        } else {
            self.bg_image = None;
            self.layers = Vec::new();
        }
    }

    /// 显示提示，左上角对齐 (x,y)（屏幕坐标）
    pub fn show(&mut self, text: &str, x: i32, y: i32) {
        if text.is_empty() {
            self.hide();
            return;
        }
        let s = self.scale;
        let mut tip = View::leaf(text, self.fg)
            .bg(self.bg)
            .pad(Edges::xy(8.0 * s, 4.0 * s))
            .text_align(Align::Center);
        tip.corner_radius = 5.0 * s;
        if let Some(img) = &self.bg_image {
            tip = tip.bg_image(img.clone());
        }
        if !self.layers.is_empty() {
            tip = tip.layers(self.layers.clone());
        }

        tip.layout(0.0, 0.0, &self.renderer);
        let (w_f, h_f) = tip.measured_size();
        let w = (w_f.ceil() as u32).max(24);
        let h = (h_f.ceil() as u32).max(20);

        self.window.resize(w, h);
        {
            let buf = self.window.buffer_mut();
            let n = (w * h * 4) as usize;
            buf[..n].fill(0);
            tip.paint(buf, w, h, &self.renderer);
        }
        let _ = self.window.update();

        let (px, py) = clamp_to_work_area(x, y, w, h);
        self.window.show(px, py);
        self.visible = true;
    }

    pub fn hide(&mut self) {
        if self.visible {
            self.window.hide();
            self.visible = false;
        }
    }
}

fn dpi_scale() -> f32 {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::Graphics::Gdi::{GetDC, GetDeviceCaps, ReleaseDC, LOGPIXELSY};
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

fn clamp_to_work_area(x: i32, y: i32, w: u32, h: u32) -> (i32, i32) {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        };
        unsafe {
            let pt = POINT { x, y };
            let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(mon, &mut mi).as_bool() {
                let wa = mi.rcWork;
                let (wi, hi) = (w as i32, h as i32);
                let mut nx = x;
                let mut ny = y;
                if nx + wi > wa.right {
                    nx = wa.right - wi;
                }
                if ny + hi > wa.bottom {
                    ny = (y - hi).max(wa.top);
                }
                if nx < wa.left {
                    nx = wa.left;
                }
                if ny < wa.top {
                    ny = wa.top;
                }
                return (nx, ny);
            }
        }
    }
    (x, y)
}
