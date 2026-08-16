//! 候选悬停提示气泡（编码反查）：悬停候选时显示其编码/拼音，教学如何输入。
//!
//! 与 Go 版本 `wind_input/internal/ui/tooltip.go` 对齐（简化版）。
//! 深色圆角小气泡 + DirectWrite 文本。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::Sender;

use crate::manager::UiEvent;
use crate::sys::{
    GetCursorPos, HWND, LPARAM, LRESULT, POINT, WM_MOUSELEAVE, WM_MOUSEMOVE, WM_RBUTTONDOWN, WPARAM,
};
use crate::text::dwrite::TextRenderer;
use crate::view::{Align, Edges, View, ViewImage, ViewLayer};
use crate::window::{LayeredWindow, WindowMouse};

/// 鼠标跟踪器：检测鼠标是否悬停在 tooltip 上（WM_MOUSELEAVE 触发时直接隐藏窗口）；
/// 右键弹出反查菜单（复制内容/截图此窗口）。
struct TooltipMouse {
    /// 仅 Windows 读取（TrackMouseEvent / ShowWindow）；其它平台无 Win32 消息泵。
    #[cfg_attr(not(windows), allow(dead_code))]
    hwnd: HWND,
    mouse_over: Rc<Cell<bool>>,
    tracking: bool,
    /// 回送协调器的鼠标事件通道（右键请求菜单）。
    events: Sender<UiEvent>,
    /// 菜单打开期间抑制 WM_MOUSELEAVE 自动隐藏：右键弹出菜单后鼠标会移到菜单窗口上，
    /// 触发 WM_MOUSELEAVE，若不抑制 tooltip 会当场消失，菜单就指向一个已不存在的窗口。
    suppress_hide: Rc<Cell<bool>>,
}

impl TooltipMouse {
    #[cfg(windows)]
    fn arm_leave(&self) {
        unsafe {
            use windows::Win32::UI::Input::KeyboardAndMouse::{
                TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
            };
            let mut t = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: self.hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut t);
        }
    }
    #[cfg(not(windows))]
    fn arm_leave(&self) {}
}

impl WindowMouse for TooltipMouse {
    fn on_message(
        &mut self,
        _hwnd: HWND,
        msg: u32,
        _wparam: WPARAM,
        _lparam: LPARAM,
    ) -> Option<LRESULT> {
        match msg {
            WM_MOUSEMOVE => {
                self.mouse_over.set(true);
                if !self.tracking {
                    self.tracking = true;
                    self.arm_leave();
                }
                None
            }
            WM_MOUSELEAVE => {
                self.mouse_over.set(false);
                self.tracking = false;
                // 鼠标离开时直接隐藏（对齐 Go TooltipWindow WM_MOUSELEAVE 行为）；
                // 菜单打开期间抑制——鼠标离开是移向菜单窗口，不是真正离开。
                if !self.suppress_hide.get() {
                    #[cfg(windows)]
                    unsafe {
                        use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};
                        let _ = ShowWindow(self.hwnd, SW_HIDE);
                    }
                }
                None
            }
            WM_RBUTTONDOWN => {
                self.suppress_hide.set(true);
                let (sx, sy) = unsafe {
                    let mut p = POINT::default();
                    let _ = GetCursorPos(&mut p);
                    (p.x, p.y)
                };
                let _ = self
                    .events
                    .send(UiEvent::RequestTooltipMenu { x: sx, y: sy });
                None
            }
            _ => None,
        }
    }
}

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
    /// 鼠标是否正悬停在 tooltip 上（由 TooltipMouse 更新）。
    /// hide() 遇到此标志时推迟隐藏，待 WM_MOUSELEAVE 自动触发后真正隐藏。
    mouse_over: Rc<Cell<bool>>,
    /// 右键菜单是否打开中（与 TooltipMouse 共享，供 set_menu_open 写入）。
    suppress_hide: Rc<Cell<bool>>,
    /// 当前显示的文本内容（供右键菜单「复制内容」使用）。
    text: String,
}

impl Tooltip {
    pub fn new(events: Sender<UiEvent>) -> Result<Self, String> {
        let scale = dpi_scale();
        let window = LayeredWindow::create(None, 120, 40, "WindInputTooltip")?;
        let renderer = TextRenderer::new("Microsoft YaHei UI", FONT_PX * scale)?;
        let mouse_over = Rc::new(Cell::new(false));
        let suppress_hide = Rc::new(Cell::new(false));
        // 注册鼠标跟踪：鼠标进入 tooltip 时保持可见；WM_MOUSELEAVE 触发时自动隐藏；右键弹出菜单。
        window.register_mouse(Rc::new(RefCell::new(TooltipMouse {
            hwnd: window.hwnd(),
            mouse_over: mouse_over.clone(),
            tracking: false,
            events,
            suppress_hide: suppress_hide.clone(),
        })));
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
            mouse_over,
            suppress_hide,
            text: String::new(),
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

    /// 加载拆字字根字体（PUA 字根字符渲染）。`family` 为 DWrite 家族名。失败仅日志，不影响普通提示。
    pub fn set_chaizi_font(&mut self, path: &str, family: &str) {
        if let Err(e) = self.renderer.set_chaizi_font(path, family) {
            tracing::warn!("加载拆字字根字体失败 ({path}): {e}");
        }
    }

    /// 应用主题（tooltip 底色/文字色 + 位图背景/层）。
    pub fn set_theme(&mut self, theme: &wind_theme::Resolved) {
        self.theme = Some(theme.clone());
        // palette 兜底 → tooltip 节点覆盖（节点色已在 resolve 阶段合成 palette 默认）。
        self.bg = theme.color("tooltip_bg", BG);
        self.fg = theme.color("tooltip_text", FG);
        if let Some(node) = &theme.views.tooltip {
            if let Some(c) = node.bg_color {
                self.bg = c;
            }
            if let Some(c) = node.text_color {
                self.fg = c;
            }
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

    /// 渲染到 BGRA Vec（离屏化，不依赖 LayeredWindow）。
    /// 返回 `(bgra, w, h, cw, ch, ml, mt, mr, mb, has_shadow)`。
    fn render_to_bgra(
        &mut self,
        text: &str,
    ) -> (Vec<u8>, u32, u32, u32, u32, u32, u32, u32, u32, bool) {
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
        let n = (w * h * 4) as usize;
        let mut buf = vec![0u8; n];
        if let Some(sh) = &self.shadow {
            sh.paint(
                &mut buf,
                w,
                h,
                ml as f32,
                mt as f32,
                cw as f32,
                ch as f32,
                tip.corner_radius,
            );
        }
        tip.paint(&mut buf, w, h, &self.renderer);
        let has_shadow = self.shadow.is_some();
        (buf, w, h, cw, ch, ml, mt, mr, mb, has_shadow)
    }

    /// 渲染文本到窗口缓冲，返回内容尺寸和阴影 margin。
    /// 返回 `(cw, ch, ml, mt, mr, mb)`；失败返回 None（text 为空时调用方已拦截）。
    fn render_to_window(&mut self, text: &str) -> (u32, u32, u32, u32, u32, u32) {
        let (buf, w, h, cw, ch, ml, mt, mr, mb, _) = self.render_to_bgra(text);
        self.window.resize(w, h);
        {
            let wbuf = self.window.buffer_mut();
            wbuf[..(w * h * 4) as usize].copy_from_slice(&buf);
        }
        let _ = self.window.update();
        (cw, ch, ml, mt, mr, mb)
    }

    /// 横排模式：在候选行下方显示提示，下方不足时上翻到候选行上方。
    /// `anchor_top`/`anchor_bottom` 为候选行的屏幕上/下边界。
    pub fn show(&mut self, text: &str, x: i32, anchor_top: i32, anchor_bottom: i32) {
        self.text = text.to_string();
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
        self.text = text.to_string();
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
        if self.mouse_over.get() {
            // 鼠标正悬停在 tooltip 上，不立即隐藏；WM_MOUSELEAVE 触发后 TooltipMouse 会自动隐藏窗口。
            return;
        }
        if self.visible {
            self.window.hide();
            self.visible = false;
        }
    }

    /// 当前显示（或最近一次显示）的文本内容（右键菜单「复制内容」用）。
    pub fn text(&self) -> &str {
        &self.text
    }

    /// 将当前渲染帧保存为 PNG 文件（截图用）。
    pub fn capture_to_file(&self, path: &std::path::Path) -> Result<(), String> {
        self.window.capture_to_file(path)
    }

    /// 将当前渲染帧复制到剪贴板（截图用）。
    pub fn capture_to_clipboard(&self) -> Result<(), String> {
        self.window.capture_to_clipboard()
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

    /// 设置右键菜单打开状态：开启时抑制 WM_MOUSELEAVE 自动隐藏；关闭时若鼠标已不在
    /// tooltip 上则立即隐藏（避免菜单关掉后 tooltip 永久赖着不走）。
    pub fn set_menu_open(&mut self, open: bool) {
        self.suppress_hide.set(open);
        if !open && !self.mouse_over.get() {
            self.hide();
        }
    }

    /// 横排 host-render：渲染到 BGRA buffer + 计算屏幕坐标，不操作 LayeredWindow。
    /// 返回 `(bgra, w, h, screen_x, screen_y, software_shadow)`；text 为空返回 None。
    #[cfg(windows)]
    pub fn render_frame(
        &mut self,
        text: &str,
        x: i32,
        anchor_top: i32,
        anchor_bottom: i32,
    ) -> Option<(Vec<u8>, u32, u32, i32, i32, bool)> {
        if text.is_empty() {
            return None;
        }
        self.ensure_scale(x, anchor_bottom);
        let (buf, w, h, cw, ch, ml, mt, _mr, _mb, has_shadow) = self.render_to_bgra(text);
        let (px, py) = clamp_to_work_area(x, anchor_top, anchor_bottom, cw, ch);
        Some((buf, w, h, px - ml as i32, py - mt as i32, has_shadow))
    }

    /// 竖排 host-render：渲染到 BGRA buffer + 计算候选窗右侧/左侧坐标，不操作 LayeredWindow。
    #[cfg(windows)]
    pub fn render_frame_beside(
        &mut self,
        text: &str,
        win_left: i32,
        win_right: i32,
        row_top: i32,
        row_bottom: i32,
    ) -> Option<(Vec<u8>, u32, u32, i32, i32, bool)> {
        if text.is_empty() {
            return None;
        }
        self.ensure_scale(win_right, row_top);
        let (buf, w, h, cw, ch, ml, mt, _mr, _mb, has_shadow) = self.render_to_bgra(text);
        let (px, py) = clamp_beside(win_left, win_right, row_top, row_bottom, cw, ch);
        Some((buf, w, h, px - ml as i32, py - mt as i32, has_shadow))
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
#[cfg_attr(not(windows), allow(unused_variables, unused_mut))]
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
#[cfg_attr(not(windows), allow(unused_variables, unused_mut))]
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
