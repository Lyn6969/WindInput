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

    /// 加载拆字字根字体（PUA 字根字符渲染）。失败仅日志，不影响普通提示。
    pub fn set_chaizi_font(&mut self, path: &str) {
        if let Err(e) = self.renderer.set_chaizi_font(path) {
            tracing::warn!("加载拆字字根字体失败 ({path}): {e}");
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

    /// 渲染文本到窗口缓冲，返回内容尺寸和阴影 margin。
    /// 返回 `(cw, ch, ml, mt, mr, mb)`；失败返回 None（text 为空时调用方已拦截）。
    fn render_to_window(&mut self, text: &str) -> (u32, u32, u32, u32, u32, u32) {
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

        let (ml, mt, mr, mb) = self
            .shadow
            .as_ref()
            .map(|sh| sh.margins())
            .unwrap_or((0, 0, 0, 0));
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
        let _ = self.window.update();
        (cw, ch, ml, mt, mr, mb)
    }

    /// 横排模式：在候选行下方显示提示，下方不足时上翻到候选行上方。
    /// `anchor_top`/`anchor_bottom` 为候选行的屏幕上/下边界。
    pub fn show(&mut self, text: &str, x: i32, anchor_top: i32, anchor_bottom: i32) {
        if text.is_empty() {
            self.hide();
            return;
        }
        self.ensure_scale(x, anchor_bottom);
        let (cw, ch, ml, mt, ..) = self.render_to_window(text);
        // 内容盒按工作区钳位（下方优先，不足上翻到候选行上方）；窗口原点 = 内容锚点 − 左/上 margin。
        let (px, py) = clamp_to_work_area(x, anchor_top, anchor_bottom, cw, ch);
        self.window.show(px - ml as i32, py - mt as i32);
        self.visible = true;
    }

    /// 竖排模式：在候选窗右侧显示提示，右侧空间不足时改显示在左侧。
    /// `win_left`/`win_right` 为候选窗左右边界（含阴影）屏幕坐标。
    /// `row_top`/`row_bottom` 为悬停候选行的屏幕上/下边界，tooltip 纵向对齐候选行。
    pub fn show_beside(
        &mut self,
        text: &str,
        win_left: i32,
        win_right: i32,
        row_top: i32,
        row_bottom: i32,
    ) {
        if text.is_empty() {
            self.hide();
            return;
        }
        self.ensure_scale(win_right, row_top);
        let (cw, ch, ml, mt, ..) = self.render_to_window(text);
        let (px, py) = clamp_beside(win_left, win_right, row_top, row_bottom, cw, ch);
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
        use windows::Win32::Graphics::Gdi::{GetDC, GetDeviceCaps, LOGPIXELSY, ReleaseDC};
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

/// 竖排模式：tooltip 显示在候选窗**右侧**（空间不足时改左侧），纵向对齐悬停候选行。
/// `win_left`/`win_right` 为候选窗左右边界（含阴影）；`row_top`/`row_bottom` 为候选行上下边界。
#[cfg_attr(not(windows), allow(unused_variables))]
fn clamp_beside(
    win_left: i32,
    win_right: i32,
    row_top: i32,
    _row_bottom: i32,
    w: u32,
    h: u32,
) -> (i32, i32) {
    let gap = 4;
    let (mut px, mut py) = (win_right + gap, row_top);
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
        };
        unsafe {
            let pt = POINT {
                x: win_right,
                y: row_top,
            };
            let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(mon, &mut mi).as_bool() {
                let wa = mi.rcWork;
                let (wi, hi) = (w as i32, h as i32);
                // 右侧放不下则改左侧
                if px + wi > wa.right {
                    px = win_left - gap - wi;
                }
                // 左侧也越界则贴左边
                if px < wa.left {
                    px = wa.left;
                }
                // 纵向：对齐候选行顶，下方越界时上移
                if py + hi > wa.bottom {
                    py = wa.bottom - hi;
                }
                if py < wa.top {
                    py = wa.top;
                }
                return (px, py);
            }
        }
    }
    (px, py)
}

/// 钳位 tooltip 到工作区：默认候选行下方（anchor_bottom + gap）；下方放不下则上翻到候选行
/// **上方**（anchor_top − gap − h，让出整行高度避免遮挡候选）；左右越界贴边。
#[cfg_attr(not(windows), allow(unused_variables))]
fn clamp_to_work_area(x: i32, anchor_top: i32, anchor_bottom: i32, w: u32, h: u32) -> (i32, i32) {
    let gap = 2;
    let (mut nx, mut ny) = (x, anchor_bottom + gap);
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
        };
        unsafe {
            let pt = POINT {
                x,
                y: anchor_bottom,
            };
            let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(mon, &mut mi).as_bool() {
                let wa = mi.rcWork;
                let (wi, hi) = (w as i32, h as i32);
                // 下方放不下 → 上翻到候选行上方（让出整行高度，不遮候选）
                if ny + hi > wa.bottom {
                    ny = (anchor_top - gap - hi).max(wa.top);
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
                return (nx, ny);
            }
        }
    }
    (nx, ny)
}
