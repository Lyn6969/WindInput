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
    /// 基准字号（逻辑像素）：跟随主题 behavior.font_size（+ status 节点偏移）。
    base_logical: f32,
}

impl StatusTip {
    /// 无主题时的兜底字号（逻辑像素），与候选窗主题默认一致。
    const DEFAULT_FONT_PX: f32 = 18.0;

    pub fn new() -> Result<Self, String> {
        let scale = Self::dpi_scale();
        let window = LayeredWindow::create(None, 200, 80, "WindInputStatusTip")?;
        let renderer = TextRenderer::new("Microsoft YaHei UI", Self::DEFAULT_FONT_PX * scale)?;
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
            base_logical: Self::DEFAULT_FONT_PX,
        })
    }

    /// DPI 动态化：按显示点所在显示器实时取缩放，变化则更新字号并按新缩放重解析主题几何。
    fn ensure_scale(&mut self, x: i32, y: i32) {
        let sc = crate::dpi::scale_for_point(x, y);
        if (sc - self.scale).abs() > 0.01 {
            self.scale = sc;
            self.renderer.set_base_size(self.base_logical * sc);
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
        // 尺寸跟随主题：基准 = behavior.font_size（+ status 节点相对偏移），弃用硬编码。
        let node_off = theme
            .views
            .status
            .as_ref()
            .map(|n| n.font_size)
            .unwrap_or(0.0);
        self.base_logical = (theme.behavior.font_size as f32 + node_off).max(8.0);
        self.renderer.set_base_size(self.base_logical * self.scale);
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

    /// 渲染气泡到窗口缓冲并 update。返回 (内容宽 cw, 内容高 ch, 左 margin ml, 上 margin mt)。
    /// 供 show(跟随光标) 与 show_fixed(固定坐标) 复用，只在定位上分叉。
    fn render_bubble(&mut self, text: &str) -> (u32, u32, u32, u32) {
        let s = self.scale;
        // 单个 View 叶子即气泡：背景 + 圆角 + 内边距 + 居中文字。
        let mut tip = View::leaf(text, self.fg)
            .bg(self.bg)
            .pad(Edges::xy(10.0 * s, 5.0 * s))
            .text_align(Align::Center);
        if let Some((bc, bw)) = self.border {
            tip = tip.border(bc, bw);
        }
        tip.corner_radius = self
            .radius
            .unwrap_or((self.renderer.measure_text("国").height + 10.0 * s) * 0.3);
        if let Some(img) = &self.bg_image {
            tip = tip.bg_image(img.clone());
        }
        if !self.layers.is_empty() {
            tip = tip.layers(self.layers.clone());
        }
        // 软投影四向扩边：内容布局起点移到 (ml, mt)，窗口位置左上回移，阴影四面溢出。
        let (ml, mt, mr, mb) = self
            .shadow
            .as_ref()
            .map(|sh| sh.margins())
            .unwrap_or((0, 0, 0, 0));
        tip.layout(ml as f32, mt as f32, &self.renderer);
        let (w_f, h_f) = tip.measured_size();
        let cw = (w_f.ceil() as u32).max(32);
        let ch = (h_f.ceil() as u32).max(24);
        let w = cw + ml + mr;
        let h = ch + mt + mb;
        self.window.resize(w, h);
        {
            let buf = self.window.buffer_mut();
            let n = (w * h * 4) as usize;
            buf[..n].fill(0);
            if let Some(sh) = &self.shadow {
                sh.paint(
                    buf,
                    w,
                    h,
                    ml as f32,
                    mt as f32,
                    cw as f32,
                    ch as f32,
                    tip.corner_radius,
                );
            }
            tip.paint(buf, w, h, &self.renderer);
        }
        if let Err(e) = self.window.update() {
            tracing::warn!("StatusTip update failed: {}", e);
        }
        (cw, ch, ml, mt)
    }

    /// 显示提示文本：水平居中于光标、默认在光标下方（下方不足则上翻），加用户偏移。
    /// `cy` 为光标底端，`caret_h` 为光标高度（上翻定位用）。
    pub fn show(&mut self, text: &str, cx: i32, cy: i32, caret_h: i32, off_x: i32, off_y: i32) {
        self.ensure_scale(cx, cy);
        let s = self.scale;
        let (cw, ch, ml, mt) = self.render_bubble(text);
        // 水平居中于光标、默认光标下方（下方不足上翻），叠加用户偏移；按工作区钳位。
        let gap = (4.0 * s).round() as i32;
        let x = cx - (cw as i32) / 2 + off_x;
        let y = cy + gap + off_y;
        let (px, py) = clamp_below_or_above(x, y, cw, ch, cy, caret_h, gap);
        // 内容锚点 − 左/上 margin，阴影向四周溢出。
        self.window.show(px - ml as i32, py - mt as i32);
    }

    /// 固定坐标显示（position_mode=fixed）：(fx,fy) 为内容左上屏幕坐标，不随光标。
    pub fn show_fixed(&mut self, text: &str, fx: i32, fy: i32) {
        self.ensure_scale(fx, fy);
        let (_cw, _ch, ml, mt) = self.render_bubble(text);
        // 内容锚点 (fx,fy) − 左/上 margin，阴影向四周溢出。
        self.window.show(fx - ml as i32, fy - mt as i32);
    }

    /// 将当前渲染帧保存为 PNG 文件（截图用）。
    pub fn capture_to_file(&self, path: &std::path::Path) -> Result<(), String> {
        self.window.capture_to_file(path)
    }

    /// 窗口当前是否可见（查询 Win32 IsWindowVisible）。
    pub fn is_visible(&self) -> bool {
        #[cfg(windows)]
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(self.window.hwnd()).as_bool()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// 返回状态提示窗口句柄（截图用）。
    #[cfg(windows)]
    pub fn hwnd(&self) -> windows::Win32::Foundation::HWND {
        self.window.hwnd()
    }

    pub fn hide(&self) {
        self.window.hide();
    }
}

/// 把气泡钳制在光标所在显示器工作区：默认 (x, y_below)；下方放不下则上翻到光标上方
/// （光标顶端 = caret_y - caret_h）；左右越界贴边。返回内容盒左上屏幕坐标。
#[cfg_attr(not(windows), allow(unused_variables))]
fn clamp_below_or_above(
    x: i32,
    y_below: i32,
    w: u32,
    h: u32,
    caret_y: i32,
    caret_h: i32,
    gap: i32,
) -> (i32, i32) {
    let (mut nx, mut ny) = (x, y_below);
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
        };
        unsafe {
            let pt = POINT { x, y: caret_y };
            let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(mon, &mut mi).as_bool() {
                let wa = mi.rcWork;
                let (wi, hi) = (w as i32, h as i32);
                // 下方放不下 → 上翻到光标上方
                if ny + hi > wa.bottom {
                    let above = caret_y - caret_h.max(0) - hi - gap;
                    ny = if above >= wa.top {
                        above
                    } else {
                        wa.bottom - hi
                    };
                }
                if nx + wi > wa.right {
                    nx = wa.right - wi;
                }
                if nx < wa.left {
                    nx = wa.left;
                }
                if ny < wa.top {
                    ny = wa.top;
                }
            }
        }
    }
    (nx.max(0), ny.max(0))
}

impl StatusTip {
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
                if dpi > 0 { dpi as f32 / 96.0 } else { 1.0 }
            }
        }
        #[cfg(not(windows))]
        {
            1.0
        }
    }
}
