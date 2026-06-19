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
    /// 主题配置的软投影 / 边框 / 圆角（与候选窗一致化）。
    shadow: Option<crate::view::SoftShadow>,
    border: Option<([u8; 4], f32)>,
    radius: Option<f32>,
    /// 已应用主题（DPI 变化时按新缩放重解析几何）。
    theme: Option<wind_theme::Resolved>,
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
            shadow: None,
            border: None,
            radius: None,
            theme: None,
        })
    }

    /// DPI 动态化：按显示点所在显示器实时取缩放，变化则更新字号并按新缩放重解析主题几何。
    fn ensure_scale(&mut self, x: i32, y: i32) {
        let sc = crate::dpi::scale_for_point(x, y);
        if (sc - self.scale).abs() > 0.01 {
            self.scale = sc;
            self.renderer.set_base_size(FONT_PX * sc);
            if let Some(t) = self.theme.clone() {
                self.set_theme(&t);
            }
        }
    }

    /// 应用主题（tooltip 底色/文字色 + 位图背景/层）。
    pub fn set_theme(&mut self, theme: &wind_theme::Resolved) {
        self.theme = Some(theme.clone());
        self.bg = theme.color("tooltip_bg", BG);
        self.fg = theme.color("tooltip_text", FG);
        if let Some(node) = &theme.views.tooltip {
            let s = self.scale;
            self.bg_image = crate::theme_assets::rv_image(theme, node.bg_image.as_ref());
            self.layers = crate::theme_assets::rv_layers(theme, &node.layers, s);
            self.shadow = crate::view::SoftShadow::build(
                node.shadow_offset_x,
                node.shadow_offset_y,
                node.shadow_blur,
                node.shadow_spread,
                node.shadow_spread_offset_x,
                node.shadow_spread_offset_y,
                node.shadow_color,
                s,
            );
            self.border = node.border_color.map(|c| {
                (
                    c,
                    node.border_width
                        .map(|d| d.resolve(s, 0.0))
                        .unwrap_or(s)
                        .max(1.0),
                )
            });
            self.radius = node.border_radius.map(|d| d.resolve(s, 0.0));
        } else {
            self.bg_image = None;
            self.layers = Vec::new();
            self.shadow = None;
            self.border = None;
            self.radius = None;
        }
    }

    /// 显示提示，左上角对齐 (x,y)（屏幕坐标）
    pub fn show(&mut self, text: &str, x: i32, y: i32) {
        if text.is_empty() {
            self.hide();
            return;
        }
        self.ensure_scale(x, y);
        let s = self.scale;
        let mut tip = View::leaf(text, self.fg)
            .bg(self.bg)
            .pad(Edges::xy(8.0 * s, 4.0 * s))
            .text_align(Align::Center);
        if let Some((bc, bw)) = self.border {
            tip = tip.border(bc, bw);
        }
        tip.corner_radius = self.radius.unwrap_or(5.0 * s);
        if let Some(img) = &self.bg_image {
            tip = tip.bg_image(img.clone());
        }
        if !self.layers.is_empty() {
            tip = tip.layers(self.layers.clone());
        }

        // 软投影四向扩边：内容布局起点移到 (ml, mt)，窗口位置左上回移。
        let (ml, mt, mr, mb) = self.shadow.as_ref().map(|sh| sh.margins()).unwrap_or((0, 0, 0, 0));
        tip.layout(ml as f32, mt as f32, &self.renderer);
        let (w_f, h_f) = tip.measured_size();
        let cw = (w_f.ceil() as u32).max(24);
        let ch = (h_f.ceil() as u32).max(20);
        let w = cw + ml + mr;
        let h = ch + mt + mb;

        self.window.resize(w, h);
        {
            let buf = self.window.buffer_mut();
            let n = (w * h * 4) as usize;
            buf[..n].fill(0);
            if let Some(sh) = &self.shadow {
                sh.paint(buf, w, h, ml as f32, mt as f32, cw as f32, ch as f32, tip.corner_radius);
            }
            tip.paint(buf, w, h, &self.renderer);
        }
        let _ = self.window.update();

        // 内容盒按工作区钳位，窗口原点 = 内容锚点 − 左/上 margin（阴影向四周溢出）。
        let (px, py) = clamp_to_work_area(x, y, cw, ch);
        self.window.show(px - ml as i32, py - mt as i32);
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
