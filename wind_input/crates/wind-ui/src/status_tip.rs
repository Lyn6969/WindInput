//! 状态提示气泡：切换中英/标点/全半角/方案时短暂显示当前状态。
//!
//! 与 Go 版本的 showModeIndicator / CmdStatusShow 对齐（简化版）。
//! 统一到 View 盒模型 + DirectWrite：深色半透明圆角底 + 居中白字，约 1 秒后自动隐藏。

use crate::text::dwrite::TextRenderer;
use crate::view::{Align, Edges, View, ViewImage, ViewLayer};
use crate::window::LayeredWindow;

/// 状态提示气泡窗口
pub struct StatusTip {
    window: LayeredWindow,
    renderer: TextRenderer,
    scale: f32,
    bg: [u8; 4],
    fg: [u8; 4],
    /// 主题位图背景（如 jidian status 的九宫格 panel）+ z 层水印。
    bg_image: Option<ViewImage>,
    layers: Vec<ViewLayer>,
    /// 主题配置的软投影 / 边框 / 圆角（与候选窗一致化）。
    shadow: Option<crate::view::SoftShadow>,
    border: Option<([u8; 4], f32)>,
    radius: Option<f32>,
    /// 已应用主题（DPI 变化时按新缩放重解析几何）。
    theme: Option<wind_theme::Resolved>,
}

impl StatusTip {
    /// 基准字号（逻辑像素）。
    const FONT_PX: f32 = 22.0;

    pub fn new() -> Result<Self, String> {
        let scale = Self::dpi_scale();
        let window = LayeredWindow::create(None, 200, 80, "WindInputStatusTip")?;
        let renderer = TextRenderer::new("Microsoft YaHei UI", Self::FONT_PX * scale)?;
        Ok(Self {
            window,
            renderer,
            scale,
            bg: [40, 40, 40, 235],
            fg: [245, 245, 245, 255],
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
            self.renderer.set_base_size(Self::FONT_PX * sc);
            if let Some(t) = self.theme.clone() {
                self.set_theme(&t);
            }
        }
    }

    /// 应用主题（状态气泡底色/文字色 + 位图背景/层）。
    pub fn set_theme(&mut self, theme: &wind_theme::Resolved) {
        self.theme = Some(theme.clone());
        self.bg = theme.color("status_bg", self.bg);
        self.fg = theme.color("status_text", self.fg);
        if let Some(node) = &theme.views.status {
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

    /// 显示提示文本，居中于 (cx, cy) 上方
    pub fn show(&mut self, text: &str, cx: i32, cy: i32) {
        self.ensure_scale(cx, cy);
        let s = self.scale;
        // 单个 View 叶子即气泡：背景 + 圆角 + 内边距 + 居中文字
        let mut tip = View::leaf(text, self.fg)
            .bg(self.bg)
            .pad(Edges::xy(18.0 * s, 10.0 * s))
            .text_align(Align::Center);
        // 边框（主题配了才描）。
        if let Some((bc, bw)) = self.border {
            tip = tip.border(bc, bw);
        }
        // 圆角：主题配置优先，否则随高度估算（字高 + 内边距）。
        tip.corner_radius = self
            .radius
            .unwrap_or((self.renderer.measure_text("国").height + 20.0 * s) * 0.28);
        // 主题位图背景 + 层（jidian status 吃九宫格 panel + 角标水印）。
        if let Some(img) = &self.bg_image {
            tip = tip.bg_image(img.clone());
        }
        if !self.layers.is_empty() {
            tip = tip.layers(self.layers.clone());
        }

        // 软投影四向扩边：内容布局起点移到 (ml, mt)，窗口位置左上回移，阴影四面溢出。
        let (ml, mt, mr, mb) = self.shadow.as_ref().map(|sh| sh.margins()).unwrap_or((0, 0, 0, 0));
        tip.layout(ml as f32, mt as f32, &self.renderer);
        let (w_f, h_f) = tip.measured_size();
        let cw = (w_f.ceil() as u32).max(48);
        let ch = (h_f.ceil() as u32).max(36);
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
        if let Err(e) = self.window.update() {
            tracing::warn!("StatusTip update failed: {}", e);
        }

        // 显示在 caret 上方居中（内容锚点 − 左/上 margin，阴影向四周溢出）。
        let cx0 = (cx - (cw as i32) / 2).max(0);
        let cy0 = (cy - (ch as i32) - 8).max(0);
        self.window.show(cx0 - ml as i32, cy0 - mt as i32);
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
