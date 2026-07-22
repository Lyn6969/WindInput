//! 状态提示气泡：切换中英/标点/全半角/方案时短暂显示当前状态。
//!
//! 与 Go 版本的 showModeIndicator / CmdStatusShow 对齐（简化版）。
//! 统一到 View 盒模型 + DirectWrite：深色半透明圆角底 + 居中白字，约 1 秒后自动隐藏。

use crate::manager::UiEvent;
use crate::text::dwrite::TextRenderer;
use crate::view::{Align, Edges, View, ViewImage, ViewLayer};
use crate::window::LayeredWindow;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Sender;

/// 状态提示气泡的鼠标处理器：左键拖动移动位置，右键请求功能菜单。
/// 抓取偏移模型（同 `input_diag_hud::DragState`）：按下时记录光标−窗口左上偏移，
/// 拖动时用该偏移换算新窗口左上，钳制到工作区后 `SetWindowPos`。
struct StatusTipMouse {
    hwnd: crate::sys::HWND,
    events: Sender<UiEvent>,
    /// 是否正在拖动（`WM_LBUTTONDOWN` → true，`WM_LBUTTONUP` → false）。
    dragging: bool,
    /// 按下时光标屏幕坐标与窗口左上角的偏移，拖动时保持该偏移。
    grab_dx: i32,
    grab_dy: i32,
    /// 阴影左/上扩边（由 `show`/`show_fixed` 每次渲染后同步），供换算内容左上坐标。
    margin: (i32, i32),
    /// 拖动中最近一次落定的窗口左上坐标（`WM_MOUSEMOVE` 写入）。
    drag_pin: Option<(i32, i32)>,
    /// 光标是否在气泡窗口内（`WM_MOUSEMOVE` 置 true / `WM_MOUSELEAVE` 置 false）。
    mouse_over: bool,
    /// 是否已注册过 `WM_MOUSELEAVE` 追踪（一次性，收到 LEAVE 后需重新注册）。
    leave_armed: bool,
    /// 本气泡的右键菜单是否打开中。
    menu_open: bool,
}

impl StatusTipMouse {
    /// 交互进行中：拖动 / 光标悬停其上 / 右键菜单打开。
    /// 临时模式的自动隐藏在此期间必须顺延，否则气泡会在用户正操作它时凭空消失。
    fn interacting(&self) -> bool {
        self.dragging || self.mouse_over || self.menu_open
    }

    /// 注册一次性 `WM_MOUSELEAVE` 通知（光标移出窗口时收到）。
    fn arm_leave(&mut self) {
        if self.leave_armed {
            return;
        }
        self.leave_armed = true;
        #[cfg(windows)]
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
}

impl crate::window::WindowMouse for StatusTipMouse {
    fn on_message(
        &mut self,
        _hwnd: crate::sys::HWND,
        msg: u32,
        _wparam: crate::sys::WPARAM,
        _lparam: crate::sys::LPARAM,
    ) -> Option<crate::sys::LRESULT> {
        use crate::sys::{
            GetWindowRect, HWND_TOPMOST, IDC_ARROW, IDC_SIZEALL, LRESULT, LoadCursorW, RECT,
            ReleaseCapture, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetCapture, SetCursor,
            SetWindowPos, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSELEAVE, WM_MOUSEMOVE,
            WM_RBUTTONDOWN, WM_SETCURSOR, clamp_to_work_area,
        };
        match msg {
            WM_MOUSELEAVE => {
                // 光标移出气泡：解除"交互中"，临时模式的自动隐藏随后重新计时。
                self.mouse_over = false;
                self.leave_armed = false;
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                let (mx, my) = cursor_screen();
                let (wx, wy) = window_origin(self.hwnd);
                self.grab_dx = mx - wx;
                self.grab_dy = my - wy;
                self.dragging = true;
                unsafe {
                    SetCapture(self.hwnd);
                }
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                // 悬停即视为交互中：光标停在气泡上时不该被自动隐藏抽走。
                self.mouse_over = true;
                self.arm_leave();
                if self.dragging {
                    let (mx, my) = cursor_screen();
                    let nx = mx - self.grab_dx;
                    let ny = my - self.grab_dy;
                    let (w, h) = {
                        let mut r = RECT::default();
                        unsafe {
                            if GetWindowRect(self.hwnd, &mut r).is_ok() {
                                ((r.right - r.left) as u32, (r.bottom - r.top) as u32)
                            } else {
                                (0, 0)
                            }
                        }
                    };
                    let (cx, cy) = clamp_to_work_area(nx, ny, w, h);
                    unsafe {
                        let _ = SetWindowPos(
                            self.hwnd,
                            HWND_TOPMOST,
                            cx,
                            cy,
                            0,
                            0,
                            SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
                        );
                    }
                    self.drag_pin = Some((cx, cy));
                    return Some(LRESULT(0));
                }
                None
            }
            WM_LBUTTONUP => {
                if self.dragging {
                    self.dragging = false;
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    let (wx, wy) = window_origin(self.hwnd);
                    let x = wx + self.margin.0;
                    let y = wy + self.margin.1;
                    let _ = self.events.send(UiEvent::StatusTipMoved { x, y });
                    return Some(LRESULT(0));
                }
                None
            }
            WM_RBUTTONDOWN => {
                let (mx, my) = cursor_screen();
                let _ = self
                    .events
                    .send(UiEvent::RequestStatusMenu { x: mx, y: my });
                Some(LRESULT(0))
            }
            WM_SETCURSOR => {
                unsafe {
                    let cur = if self.dragging {
                        IDC_SIZEALL
                    } else {
                        IDC_ARROW
                    };
                    if let Ok(c) = LoadCursorW(None, cur) {
                        SetCursor(c);
                    }
                }
                Some(LRESULT(1))
            }
            _ => None,
        }
    }
}

/// 取鼠标屏幕坐标（失败回退 (0,0)）。
fn cursor_screen() -> (i32, i32) {
    let mut pt = crate::sys::POINT::default();
    unsafe {
        let _ = crate::sys::GetCursorPos(&mut pt);
    }
    (pt.x, pt.y)
}

/// 取窗口左上角屏幕坐标（失败回退 (0,0)）。
fn window_origin(hwnd: crate::sys::HWND) -> (i32, i32) {
    let mut r = crate::sys::RECT::default();
    unsafe {
        let _ = crate::sys::GetWindowRect(hwnd, &mut r);
    }
    (r.left, r.top)
}

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
    /// 拖动 + 右键菜单处理器（`show`/`show_fixed` 每次渲染后同步其 margin）。
    mouse: Rc<RefCell<StatusTipMouse>>,
}

impl StatusTip {
    /// 无主题时的兜底字号（逻辑像素），与候选窗主题默认一致。
    const DEFAULT_FONT_PX: f32 = 18.0;

    pub fn new(events: Sender<UiEvent>) -> Result<Self, String> {
        let scale = Self::dpi_scale();
        let window = LayeredWindow::create(None, 200, 80, "WindInputStatusTip")?;
        let mouse = Rc::new(RefCell::new(StatusTipMouse {
            hwnd: window.hwnd(),
            events,
            dragging: false,
            grab_dx: 0,
            grab_dy: 0,
            margin: (0, 0),
            drag_pin: None,
            mouse_over: false,
            leave_armed: false,
            menu_open: false,
        }));
        window.register_mouse(mouse.clone());
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
            mouse,
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

    /// 渲染气泡到 BGRA Vec（离屏化，不依赖 LayeredWindow）。
    /// 返回 `(bgra, w, h, cw, ch, ml, mt, has_shadow)`。
    fn render_bubble_to_bgra(
        &mut self,
        text: &str,
    ) -> (Vec<u8>, u32, u32, u32, u32, u32, u32, bool) {
        let s = self.scale;
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
        let mut buf = vec![0u8; (w * h * 4) as usize];
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
        (buf, w, h, cw, ch, ml, mt, self.shadow.is_some())
    }

    /// 渲染气泡到窗口缓冲并 update。返回 (内容宽 cw, 内容高 ch, 左 margin ml, 上 margin mt)。
    /// 供 show(跟随光标) 与 show_fixed(固定坐标) 复用，只在定位上分叉。
    fn render_bubble(&mut self, text: &str) -> (u32, u32, u32, u32) {
        let (buf, w, h, cw, ch, ml, mt, _) = self.render_bubble_to_bgra(text);
        self.window.resize(w, h);
        {
            let wbuf = self.window.buffer_mut();
            wbuf[..(w * h * 4) as usize].copy_from_slice(&buf);
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
        self.mouse.borrow_mut().margin = (ml as i32, mt as i32);
        // 拖动中：跳过重新定位，避免状态刷新把窗口拽回去（拖动本身已用 SetWindowPos 定位）。
        let m = self.mouse.borrow();
        if m.dragging && m.drag_pin.is_some() {
            return;
        }
        drop(m);
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
        self.mouse.borrow_mut().margin = (ml as i32, mt as i32);
        // 拖动中：跳过重新定位，避免状态刷新把窗口拽回去（拖动本身已用 SetWindowPos 定位）。
        let m = self.mouse.borrow();
        if m.dragging && m.drag_pin.is_some() {
            return;
        }
        drop(m);
        // 内容锚点 (fx,fy) − 左/上 margin，阴影向四周溢出。
        self.window.show(fx - ml as i32, fy - mt as i32);
    }

    /// 将当前渲染帧保存为 PNG 文件（截图用）。
    pub fn capture_to_file(&self, path: &std::path::Path) -> Result<(), String> {
        self.window.capture_to_file(path)
    }

    /// 将当前渲染帧复制到剪贴板（截图用）。
    pub fn capture_to_clipboard(&self) -> Result<(), String> {
        self.window.capture_to_clipboard()
    }

    /// 用户是否正在与气泡交互（拖动 / 悬停 / 右键菜单打开）。
    /// 临时模式的自动隐藏须在此期间顺延——否则用户正拖着它、或菜单还开着，气泡就消失了。
    pub fn interacting(&self) -> bool {
        self.mouse.borrow().interacting()
    }

    /// 标记本气泡的右键菜单开/关（打开期间抑制自动隐藏）。
    pub fn set_menu_open(&self, open: bool) {
        self.mouse.borrow_mut().menu_open = open;
    }

    /// 当前气泡**内容左上**屏幕坐标（窗口左上 + 阴影扩边）。
    /// 供「固定位置」开关把当前实际位置落盘成 custom_x/custom_y。
    pub fn content_origin(&self) -> (i32, i32) {
        let m = self.mouse.borrow();
        let (wx, wy) = window_origin(m.hwnd);
        (wx + m.margin.0, wy + m.margin.1)
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

    /// host-render：将状态气泡渲染到 BGRA buffer 并计算屏幕坐标（光标下方/上方）。
    /// 返回 `(bgra, w, h, screen_x, screen_y, software_shadow)`；text 为空返回 None。
    #[cfg(windows)]
    pub fn render_frame(
        &mut self,
        text: &str,
        cx: i32,
        cy: i32,
        caret_h: i32,
        off_x: i32,
        off_y: i32,
    ) -> Option<(Vec<u8>, u32, u32, i32, i32, bool)> {
        if text.is_empty() {
            return None;
        }
        self.ensure_scale(cx, cy);
        let s = self.scale;
        let (buf, w, h, cw, ch, ml, mt, has_shadow) = self.render_bubble_to_bgra(text);
        let gap = (4.0 * s).round() as i32;
        let x = cx - (cw as i32) / 2 + off_x;
        let y = cy + gap + off_y;
        let (px, py) = clamp_below_or_above(x, y, cw, ch, cy, caret_h, gap);
        Some((buf, w, h, px - ml as i32, py - mt as i32, has_shadow))
    }

    /// host-render：固定坐标模式，直接用 (fx, fy) 作内容左上屏幕坐标。
    /// 返回 `(bgra, w, h, screen_x, screen_y, software_shadow)`；text 为空返回 None。
    #[cfg(windows)]
    pub fn render_frame_fixed(
        &mut self,
        text: &str,
        fx: i32,
        fy: i32,
    ) -> Option<(Vec<u8>, u32, u32, i32, i32, bool)> {
        if text.is_empty() {
            return None;
        }
        self.ensure_scale(fx, fy);
        let (buf, w, h, _cw, _ch, ml, mt, has_shadow) = self.render_bubble_to_bgra(text);
        Some((buf, w, h, fx - ml as i32, fy - mt as i32, has_shadow))
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
