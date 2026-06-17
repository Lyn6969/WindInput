//! 工具栏窗口：常驻状态指示器（中英 / 方案 / 标点 / 全半角）。
//!
//! 与 Go 版本 `wind_input/internal/ui/toolbar_window.go` 对齐（简化版）。
//! 横向圆角小条，每格一个状态；中文模式格高亮。固定显示于工作区右下角。
//! 点击切换暂未实现（后续 UI 统一优化阶段补齐拖动 + 命中），当前为展示用。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Sender;

use crate::manager::{ToolbarAction, UiEvent};
use crate::text::dwrite::TextRenderer;
use crate::view::Rect;
use crate::window::{LayeredWindow, WindowMouse};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowRect, LoadCursorW, SetCursor, SetWindowPos, HWND_TOPMOST, IDC_ARROW,
    IDC_SIZEALL, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_SETCURSOR,
};

/// 工具栏状态（由协调器推送）
#[derive(Debug, Clone)]
pub struct ToolbarState {
    pub chinese_mode: bool,
    /// 方案友好名（如 "五笔" / "拼音"）
    pub schema_label: String,
    pub full_width: bool,
    pub chinese_punct: bool,
}

impl Default for ToolbarState {
    fn default() -> Self {
        Self {
            chinese_mode: true,
            schema_label: "五笔".to_string(),
            full_width: false,
            chinese_punct: true,
        }
    }
}

/// 一个单元格：文本 + 是否高亮（中文模式格）+ 点击动作
struct Cell {
    text: String,
    highlight: bool,
    action: ToolbarAction,
}

/// 工具栏窗口
pub struct Toolbar {
    window: LayeredWindow,
    renderer: TextRenderer,
    scale: f32,
    visible: bool,
    /// 鼠标处理器（与 window 共享，wnd_proc 经注册表回调）；位置存于其中以便拖动同步
    mouse: Rc<RefCell<ToolbarMouse>>,
    // 主题色（默认深灰，set_theme 覆盖）
    bg: [u8; 4],
    fg: [u8; 4],
    hl_bg: [u8; 4],
    hl_fg: [u8; 4],
    sep: [u8; 4],
    grip: [u8; 4],
}

impl Toolbar {
    // 视觉常量（逻辑像素，随 DPI 缩放）
    const HEIGHT: f32 = 30.0;
    const GRIP_W: f32 = 12.0;
    const CELL_PAD_X: f32 = 9.0;
    const MIN_CELL_W: f32 = 26.0;
    const FONT_PX: f32 = 15.0;

    const BG: [u8; 4] = [44, 44, 46, 240]; // 深灰圆角底
    const FG: [u8; 4] = [235, 235, 238, 255]; // 普通文字
    const HL_BG: [u8; 4] = [66, 133, 244, 255]; // 中文模式高亮蓝
    const HL_FG: [u8; 4] = [255, 255, 255, 255];
    const SEP: [u8; 4] = [70, 70, 74, 255]; // 分隔线
    const GRIP: [u8; 4] = [120, 120, 124, 255];

    pub fn new(events: Sender<UiEvent>) -> Result<Self, String> {
        let scale = Self::dpi_scale();
        let window = LayeredWindow::create(None, 160, 40, "WindInputToolbar")?;
        let renderer = TextRenderer::new("Microsoft YaHei UI", Self::FONT_PX * scale)?;
        let hwnd = window.hwnd();
        let mouse = Rc::new(RefCell::new(ToolbarMouse {
            hits: Vec::new(),
            events,
            hwnd,
            pos: None,
            dragging: false,
            anchor: (0, 0),
            origin: (0, 0),
        }));
        window.register_mouse(mouse.clone());
        Ok(Self {
            window,
            renderer,
            scale,
            visible: false,
            mouse,
            bg: Self::BG,
            fg: Self::FG,
            hl_bg: Self::HL_BG,
            hl_fg: Self::HL_FG,
            sep: Self::SEP,
            grip: Self::GRIP,
        })
    }

    /// 应用主题（工具栏各色，跟随语义）。
    pub fn set_theme(&mut self, theme: &wind_theme::Resolved) {
        self.bg = theme.color("toolbar_background", self.bg);
        self.fg = theme.color("toolbar_full_width_off_text", self.fg);
        self.hl_bg = theme.color("toolbar_mode_chinese_bg", self.hl_bg);
        self.hl_fg = theme.color("toolbar_mode_text", self.hl_fg);
        self.sep = theme.color("toolbar_border", self.sep);
        self.grip = theme.color("toolbar_grip", self.grip);
    }

    /// 设置工具栏位置（启动恢复持久化位置）；钳制到工作区内。
    pub fn set_pos(&mut self, x: i32, y: i32) {
        let (w, h) = self.window.size();
        let (cx, cy) = clamp_to_work_area(x, y, w, h);
        self.mouse.borrow_mut().pos = Some((cx, cy));
        if self.visible {
            self.window.show(cx, cy);
        }
    }

    /// 根据状态构建单元格序列
    fn cells(state: &ToolbarState) -> Vec<Cell> {
        let mode = if state.chinese_mode { "中" } else { "英" };
        let schema = state
            .schema_label
            .chars()
            .next()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".to_string());
        let punct = if state.chinese_punct { "，。" } else { ",." };
        let width = if state.full_width { "全" } else { "半" };
        vec![
            Cell { text: mode.to_string(), highlight: state.chinese_mode, action: ToolbarAction::ToggleMode },
            Cell { text: schema, highlight: false, action: ToolbarAction::SwitchEngine },
            Cell { text: punct.to_string(), highlight: false, action: ToolbarAction::TogglePunct },
            Cell { text: width.to_string(), highlight: false, action: ToolbarAction::ToggleWidth },
        ]
    }

    /// 更新状态并重绘（首次会计算位置并显示）
    pub fn update(&mut self, state: &ToolbarState) {
        let s = self.scale;
        let height = (Self::HEIGHT * s).ceil();
        let grip_w = (Self::GRIP_W * s).ceil();
        let pad_x = Self::CELL_PAD_X * s;
        let min_cell = Self::MIN_CELL_W * s;

        let cells = Self::cells(state);

        // 逐格量宽
        let mut cell_widths = Vec::with_capacity(cells.len());
        for c in &cells {
            let m = self.renderer.measure_text(&c.text);
            cell_widths.push((m.width + pad_x * 2.0).max(min_cell).ceil());
        }
        let total_w: f32 = grip_w + cell_widths.iter().sum::<f32>();
        let w = total_w.ceil() as u32;
        let h = height as u32;

        self.window.resize(w, h);
        let buf_size = (w * h * 4) as usize;
        {
            let buf = self.window.buffer_mut();
            buf[..buf_size].fill(0);
            let radius = (h as f32 * 0.22) as u32;
            fill_rounded(buf, w, h, 0, 0, w, h, self.bg, radius);
            // 拖动柄点阵（视觉对齐 Go，暂不响应拖动）
            draw_grip(buf, w, h, grip_w as u32, self.grip, s);
        }

        // 逐格绘制 + 记录命中矩形
        let mut x = grip_w;
        let font_h = self.renderer.measure_text("中").height;
        let mut hits: Vec<(ToolbarAction, Rect)> = Vec::with_capacity(cells.len());
        for (i, c) in cells.iter().enumerate() {
            let cw = cell_widths[i];
            hits.push((c.action, Rect { x, y: 0.0, w: cw, h: h as f32 }));
            // 分隔线（首格前不画）
            if i > 0 {
                draw_vsep(self.window.buffer_mut(), w, h, x as u32, self.sep, s);
            }
            // 高亮底（中文模式格）
            if c.highlight {
                let inset = (4.0 * s) as u32;
                let hx = x as u32 + inset / 2;
                let hy = inset;
                let hw = (cw as u32).saturating_sub(inset);
                let hh = h.saturating_sub(inset * 2);
                let hr = (hh as f32 * 0.3) as u32;
                fill_rounded(self.window.buffer_mut(), w, h, hx, hy, hw, hh, self.hl_bg, hr);
            }
            // 居中文字
            let m = self.renderer.measure_text(&c.text);
            let tx = x + (cw - m.width) * 0.5;
            let ty = (h as f32 - font_h) * 0.5;
            let fg = if c.highlight { self.hl_fg } else { self.fg };
            let _ = self
                .renderer
                .draw_text(self.window.buffer_mut(), w, h, tx.max(x), ty.max(0.0), &c.text, fg);
            x += cw;
        }
        if let Err(e) = self.window.update() {
            tracing::warn!("Toolbar update failed: {}", e);
        }

        // 位置：优先用持久化/拖动后的位置；首次落在工作区右下角（避开任务栏）。
        // 钳制到当前显示器工作区内——避免切换显示器/远程后旧坐标落在屏外不可见。
        let (px, py) = {
            let mut m = self.mouse.borrow_mut();
            m.hits = hits; // 同步命中矩形给鼠标处理器
            let raw = m.pos.unwrap_or_else(|| Self::corner_position(w, h));
            let clamped = clamp_to_work_area(raw.0, raw.1, w, h);
            m.pos = Some(clamped);
            clamped
        };
        self.window.show(px, py);
        self.visible = true;
    }

    pub fn show(&mut self) {
        let pos = self.mouse.borrow().pos;
        if let Some((x, y)) = pos {
            let (w, h) = self.window.size();
            let (cx, cy) = clamp_to_work_area(x, y, w, h);
            self.mouse.borrow_mut().pos = Some((cx, cy));
            self.window.show(cx, cy);
            self.visible = true;
        }
    }

    pub fn hide(&mut self) {
        self.window.hide();
        self.visible = false;
    }

    /// 工作区右下角位置（避开任务栏），右/下各留 12px 边距
    fn corner_position(w: u32, h: u32) -> (i32, i32) {
        #[cfg(windows)]
        {
            use windows::Win32::Foundation::RECT;
            use windows::Win32::UI::WindowsAndMessaging::{
                SystemParametersInfoW, SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
            };
            unsafe {
                let mut rect = RECT::default();
                let ok = SystemParametersInfoW(
                    SPI_GETWORKAREA,
                    0,
                    Some(&mut rect as *mut _ as *mut std::ffi::c_void),
                    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                );
                if ok.is_ok() && rect.right > rect.left {
                    let margin = 12;
                    let x = rect.right - w as i32 - margin;
                    let y = rect.bottom - h as i32 - margin;
                    return (x.max(0), y.max(0));
                }
            }
        }
        (200, 200)
    }

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

/// 工具栏鼠标处理器：点击单元格切换；非单元格区（拖动柄）按下拖动整条工具栏。
pub struct ToolbarMouse {
    hits: Vec<(ToolbarAction, Rect)>,
    events: Sender<UiEvent>,
    hwnd: HWND,
    /// 当前位置（屏幕坐标）；None = 尚未定位
    pos: Option<(i32, i32)>,
    dragging: bool,
    /// 拖动起点：光标屏幕坐标
    anchor: (i32, i32),
    /// 拖动起点：窗口屏幕坐标
    origin: (i32, i32),
}

impl ToolbarMouse {
    fn cell_at(&self, x: f32, y: f32) -> Option<ToolbarAction> {
        self.hits
            .iter()
            .find(|(_, r)| r.contains(x, y))
            .map(|(a, _)| *a)
    }
}

impl WindowMouse for ToolbarMouse {
    fn on_message(
        &mut self,
        _hwnd: HWND,
        msg: u32,
        _wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        let v = lparam.0 as u32;
        let cx = (v & 0xFFFF) as i16 as f32;
        let cy = ((v >> 16) & 0xFFFF) as i16 as f32;
        match msg {
            WM_LBUTTONDOWN => {
                if self.cell_at(cx, cy).is_none() {
                    // 非单元格（拖动柄区）→ 开始拖动
                    let mut p = POINT::default();
                    unsafe {
                        let _ = GetCursorPos(&mut p);
                    }
                    self.anchor = (p.x, p.y);
                    self.origin = self.pos.unwrap_or((p.x, p.y));
                    self.dragging = true;
                    unsafe {
                        SetCapture(self.hwnd);
                    }
                }
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                if self.dragging {
                    let mut p = POINT::default();
                    unsafe {
                        let _ = GetCursorPos(&mut p);
                    }
                    let nx = self.origin.0 + (p.x - self.anchor.0);
                    let ny = self.origin.1 + (p.y - self.anchor.1);
                    // 钳制到（最近显示器的）工作区，防止拖出桌面/拖入任务栏。
                    // 多显示器下 MonitorFromPoint(NEAREST) 会随光标过界切到目标显示器。
                    let (w, h) = unsafe {
                        let mut r = RECT::default();
                        if GetWindowRect(self.hwnd, &mut r).is_ok() {
                            ((r.right - r.left) as u32, (r.bottom - r.top) as u32)
                        } else {
                            (0, 0)
                        }
                    };
                    let (cx, cy) = clamp_to_work_area(nx, ny, w, h);
                    self.pos = Some((cx, cy));
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
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                if self.dragging {
                    self.dragging = false;
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    // 取实际窗口位置回报，供持久化
                    let mut r = RECT::default();
                    let (x, y) = unsafe {
                        if GetWindowRect(self.hwnd, &mut r).is_ok() {
                            (r.left, r.top)
                        } else {
                            self.pos.unwrap_or((0, 0))
                        }
                    };
                    self.pos = Some((x, y));
                    let _ = self.events.send(UiEvent::ToolbarMoved { x, y });
                } else if let Some(action) = self.cell_at(cx, cy) {
                    // 单元格：按下未拖动 → 抬起时触发切换
                    let _ = self.events.send(UiEvent::Toolbar(action));
                }
                Some(LRESULT(0))
            }
            WM_RBUTTONDOWN => {
                // 右键工具栏 → 功能主菜单（屏幕光标定位）
                let mut p = POINT::default();
                unsafe {
                    let _ = GetCursorPos(&mut p);
                }
                let _ = self.events.send(UiEvent::RequestMainMenu { x: p.x, y: p.y });
                Some(LRESULT(0))
            }
            WM_SETCURSOR => {
                unsafe {
                    let cur = if self.dragging { IDC_SIZEALL } else { IDC_ARROW };
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

/// 在缓冲区子区域 (x,y,w,h) 内填充圆角矩形
/// 将 (x,y,w,h) 钳制到所在（或最近）显示器工作区内，保证完整可见。
/// 用于切换显示器 / 远程连接后旧坐标落到屏外时拉回。
fn clamp_to_work_area(x: i32, y: i32, w: u32, h: u32) -> (i32, i32) {
    #[cfg(windows)]
    {
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
                    ny = wa.bottom - hi;
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

/// 圆角填充：复用 view 的抗锯齿 + 预乘混合实现，保持各窗口圆角一致。
fn fill_rounded(
    buf: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    color: [u8; 4],
    radius: u32,
) {
    crate::view::fill_rounded(
        buf,
        buf_w,
        buf_h,
        x as f32,
        y as f32,
        w as f32,
        h as f32,
        color,
        radius as f32,
    );
}

/// 竖直分隔线（在 x 处，上下各内缩 6px）
fn draw_vsep(buf: &mut [u8], buf_w: u32, buf_h: u32, x: u32, color: [u8; 4], scale: f32) {
    let inset = (6.0 * scale) as u32;
    let y0 = inset;
    let y1 = buf_h.saturating_sub(inset);
    if x >= buf_w || y1 <= y0 {
        return;
    }
    // 1px 竖线 = 直角矩形（tiny-skia），与其它形状统一
    crate::view::fill_rounded(
        buf,
        buf_w,
        buf_h,
        x as f32,
        y0 as f32,
        1.0,
        (y1 - y0) as f32,
        color,
        0.0,
    );
}

/// 左侧拖动柄：2×3 点阵
fn draw_grip(buf: &mut [u8], buf_w: u32, buf_h: u32, grip_w: u32, color: [u8; 4], scale: f32) {
    let dot = (2.0 * scale).max(1.0);
    let gap = 4.0 * scale;
    let cx = grip_w as f32 / 2.0;
    let cy = buf_h as f32 / 2.0;
    let start_y = cy - gap;
    for row in 0..3 {
        let y = start_y + row as f32 * gap;
        for col in 0..2 {
            let dx = cx - gap / 2.0 + col as f32 * gap;
            fill_dot(buf, buf_w, buf_h, dx, y, dot / 2.0, color);
        }
    }
}

fn fill_dot(buf: &mut [u8], buf_w: u32, buf_h: u32, cx: f32, cy: f32, r: f32, color: [u8; 4]) {
    // 抗锯齿圆点（tiny-skia），与其它形状统一
    crate::view::fill_circle(buf, buf_w, buf_h, cx, cy, r, color);
}
