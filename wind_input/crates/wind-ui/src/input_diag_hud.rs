//! 输入诊断 HUD：非激活置顶浮窗，右键「高级」开关控制显隐，可拖动，双击复制。
//!
//! 复用 [`crate::window::LayeredWindow`]（默认已带 `WS_EX_NOACTIVATE | WS_EX_TOPMOST |
//! WS_EX_TOOLWINDOW | WS_EX_LAYERED`：不进任务栏、不抢焦点、透明渲染）+ [`TextRenderer`] +
//! [`crate::view::View`] 盒模型，仿 `status_tip.rs`/`toast.rs`。MVP 用固定深色半透明底 + 白字，
//! 不接主题。拖动/双击复制在窗口过程（`wnd_proc`）经 [`WindowMouse`] 处理。

use crate::text::dwrite::TextRenderer;
use crate::view::{Align, Edges, Layout, View};
use crate::window::LayeredWindow;

#[derive(Clone, Debug)]
pub struct InputDiagView {
    pub process_name: String,
    pub pid: u32,
    pub disabled: bool,
    pub reason_text: String,
    pub mask: u64,
}

/// 纯格式化：4 行诊断文本（可单测）。
pub fn format_diag_lines(v: &InputDiagView) -> Vec<String> {
    let name = if v.process_name.is_empty() {
        "(未知)"
    } else {
        &v.process_name
    };
    vec![
        format!("{} ({})", name, v.pid),
        format!("禁用态: {}", if v.disabled { "是" } else { "否" }),
        format!("原因: {}", v.reason_text),
        format!("InputScope: 0x{:X}", v.mask),
    ]
}

/// 固定深色半透明底 + 白字（MVP，不接主题）。
const BG: [u8; 4] = [32, 32, 36, 235];
const FG: [u8; 4] = [240, 240, 245, 255];
/// 无主题兜底字号（逻辑像素）。
const FONT_PX: f32 = 14.0;
/// 初始位置距屏幕右下角边距（逻辑像素）。
const MARGIN: i32 = 24;

/// 拖动交互状态（与 `wnd_proc` 共享）：仅在 Windows 有意义，非 Windows 为死代码占位。
struct DragState {
    hwnd: crate::sys::HWND,
    /// 是否正在拖动（`WM_LBUTTONDOWN` → true，`WM_LBUTTONUP` → false）。
    dragging: bool,
    /// 按下时鼠标屏幕坐标与窗口左上角的偏移，拖动时保持该偏移。
    grab_dx: i32,
    grab_dy: i32,
    /// 上次左键按下时间与坐标，用于手动判定双击（窗口类未启用 CS_DBLCLKS）。
    last_down_ms: u64,
    last_down_x: i32,
    last_down_y: i32,
    /// 当前诊断文本（双击复制用），由 `show_or_update` 刷新。
    copy_text: String,
}

impl DragState {
    fn new(hwnd: crate::sys::HWND) -> Self {
        Self {
            hwnd,
            dragging: false,
            grab_dx: 0,
            grab_dy: 0,
            last_down_ms: 0,
            last_down_x: i32::MIN,
            last_down_y: i32::MIN,
            copy_text: String::new(),
        }
    }
}

/// 当前毫秒时间戳（单调够用；仅用于双击间隔判定）。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl crate::window::WindowMouse for DragState {
    fn on_message(
        &mut self,
        _hwnd: crate::sys::HWND,
        msg: u32,
        _wparam: crate::sys::WPARAM,
        _lparam: crate::sys::LPARAM,
    ) -> Option<crate::sys::LRESULT> {
        use crate::sys::{
            HWND_TOPMOST, LRESULT, ReleaseCapture, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
            SetCapture, SetWindowPos, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
        };
        match msg {
            WM_LBUTTONDOWN => {
                // 记录抓取偏移（鼠标屏幕坐标 − 窗口左上角），并 SetCapture 以持续收到移动。
                let (mx, my) = cursor_screen();
                let (wx, wy) = window_origin(self.hwnd);
                self.grab_dx = mx - wx;
                self.grab_dy = my - wy;
                self.dragging = true;
                unsafe {
                    SetCapture(self.hwnd);
                }
                // 手动双击判定：与上次按下间隔 < 400ms 且位移 < 6px → 视为双击，复制文本。
                let t = now_ms();
                let dbl = t.saturating_sub(self.last_down_ms) < 400
                    && (mx - self.last_down_x).abs() < 6
                    && (my - self.last_down_y).abs() < 6;
                if dbl {
                    self.dragging = false;
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    if !self.copy_text.is_empty() {
                        crate::popup_menu::set_clipboard_text(&self.copy_text);
                    }
                    self.last_down_ms = 0; // 消费本次双击，避免三击连锁
                } else {
                    self.last_down_ms = t;
                    self.last_down_x = mx;
                    self.last_down_y = my;
                }
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                if self.dragging {
                    let (mx, my) = cursor_screen();
                    let nx = mx - self.grab_dx;
                    let ny = my - self.grab_dy;
                    unsafe {
                        let _ = SetWindowPos(
                            self.hwnd,
                            HWND_TOPMOST,
                            nx,
                            ny,
                            0,
                            0,
                            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
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
                    return Some(LRESULT(0));
                }
                None
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

/// 输入诊断 HUD 窗口
pub struct InputDiagHud {
    window: LayeredWindow,
    renderer: TextRenderer,
    scale: f32,
    /// 拖动/双击状态（注册进 `wnd_proc`）。show_or_update 刷新其 copy_text。
    state: std::rc::Rc<std::cell::RefCell<DragState>>,
    /// 当前窗口左上角屏幕坐标（首次为右下角；拖动后由 wnd_proc 移动，此值仅作 show 定位兜底）。
    pos: (i32, i32),
    /// 是否已定位过（避免每次 update 都重置到右下角，尊重用户拖动）。
    positioned: bool,
}

impl InputDiagHud {
    pub fn new() -> Result<Self, String> {
        let scale = dpi_scale();
        let window = LayeredWindow::create(None, 240, 120, "WindInputDiagHud")?;
        let renderer = TextRenderer::new("Microsoft YaHei UI", FONT_PX * scale)?;
        let state = std::rc::Rc::new(std::cell::RefCell::new(DragState::new(window.hwnd())));
        window.register_mouse(state.clone());
        let pos = initial_bottom_right(240, 120, scale);
        Ok(Self {
            window,
            renderer,
            scale,
            state,
            pos,
            positioned: false,
        })
    }

    /// 渲染 4 行诊断文本并显示/更新窗口（首次落右下角，之后保持当前位置尊重拖动）。
    pub fn show_or_update(&mut self, v: &InputDiagView) {
        let lines = format_diag_lines(v);
        // 双击复制文本：整块以换行连接。
        self.state.borrow_mut().copy_text = lines.join("\n");

        let s = self.scale;
        let mut col = View::container(Layout::Column)
            .bg(BG)
            .pad(Edges::xy(12.0 * s, 9.0 * s))
            .gap(4.0 * s);
        col.corner_radius = 8.0 * s;
        for line in &lines {
            col = col.child(View::leaf(line.clone(), FG).text_align(Align::Start));
        }
        col.layout(0.0, 0.0, &self.renderer);
        let (w_f, h_f) = col.measured_size();
        let w = (w_f.ceil() as u32).max(80);
        let h = (h_f.ceil() as u32).max(40);
        self.window.resize(w, h);
        {
            let buf = self.window.buffer_mut();
            buf.fill(0);
        }
        {
            let (bw, bh) = self.window.size();
            let buf = self.window.buffer_mut();
            col.paint(buf, bw, bh, &self.renderer);
        }
        if let Err(e) = self.window.update() {
            tracing::warn!("InputDiagHud update failed: {}", e);
        }
        // 首次定位到右下角；之后不覆盖（尊重用户拖动后的位置）。
        if !self.positioned {
            self.pos = initial_bottom_right(w, h, self.scale);
            self.positioned = true;
        }
        self.window.show(self.pos.0, self.pos.1);
    }

    pub fn hide(&mut self) {
        self.window.hide();
    }
}

/// 系统 DPI 缩放因子（仿 status_tip.rs）。
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

/// 主屏右下角初始坐标 = 屏幕尺寸 − 窗口尺寸 − 边距（边距按 DPI 缩放）。
#[cfg_attr(not(windows), allow(unused_variables))]
fn initial_bottom_right(w: u32, h: u32, scale: f32) -> (i32, i32) {
    let margin = (MARGIN as f32 * scale).round() as i32;
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
        };
        unsafe {
            let sw = GetSystemMetrics(SM_CXSCREEN);
            let sh = GetSystemMetrics(SM_CYSCREEN);
            let x = (sw - w as i32 - margin).max(0);
            let y = (sh - h as i32 - margin).max(0);
            (x, y)
        }
    }
    #[cfg(not(windows))]
    {
        (margin, margin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn format_lines_shape() {
        let v = InputDiagView {
            process_name: "chrome.exe".into(),
            pid: 4242,
            disabled: true,
            reason_text: "compartment".into(),
            mask: 1 << 31,
        };
        let lines = format_diag_lines(&v);
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("chrome.exe"));
        assert!(lines[0].contains("4242"));
        assert!(lines[1].contains("是")); // 禁用态: 是
        assert!(lines[2].contains("compartment"));
        assert!(lines[3].contains("0x")); // mask 十六进制
    }
}
