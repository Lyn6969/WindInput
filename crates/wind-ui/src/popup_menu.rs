//! 弹出菜单（右键候选上下文菜单）
//!
//! 与 Go 版本 `wind_input/internal/ui/popup_menu.go` 对齐（简化版）。
//! 竖排菜单项，DirectWrite 文本，View 盒模型布局。高亮项由协调器的选中态驱动
//! （键盘方向键与鼠标悬停共用），点击/键盘激活与关闭均回送事件给协调器统一处理。
//! SetCapture 捕获鼠标，点击菜单外即关闭。不抢应用焦点。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Sender;

use crate::manager::{MenuItemSpec, MenuKind, UiEvent};
use crate::text::dwrite::TextRenderer;
use crate::view::{Align, Edges, Layout, Rect, View};
use crate::window::{LayeredWindow, WindowMouse};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    LoadCursorW, SetCursor, ShowWindow, IDC_ARROW, SW_HIDE, WM_LBUTTONDOWN, WM_MOUSEMOVE,
    WM_RBUTTONDOWN, WM_SETCURSOR,
};

const FONT_PX: f32 = 14.0;
const BG: [u8; 4] = [250, 250, 250, 252];
const FG: [u8; 4] = [40, 40, 40, 255];
const DISABLED: [u8; 4] = [175, 175, 178, 255];
const BORDER: [u8; 4] = [205, 205, 208, 230];
const SEP: [u8; 4] = [228, 228, 230, 255];
const HL_BG: [u8; 4] = [225, 236, 252, 255];

/// 弹出菜单窗口
pub struct PopupMenu {
    window: LayeredWindow,
    renderer: TextRenderer,
    scale: f32,
    mouse: Rc<RefCell<MenuMouse>>,
    visible: bool,
    items: Vec<MenuItemSpec>,
    selected: usize,
    width: u32,
    height: u32,
}

impl PopupMenu {
    pub fn new(events: Sender<UiEvent>) -> Result<Self, String> {
        let scale = dpi_scale();
        let window = LayeredWindow::create(None, 140, 120, "WindInputPopupMenu")?;
        let renderer = TextRenderer::new("Microsoft YaHei UI", FONT_PX * scale)?;
        let mouse = Rc::new(RefCell::new(MenuMouse {
            item_rects: Vec::new(),
            events,
            last_hover: -1,
            hwnd: window.hwnd(),
        }));
        window.register_mouse(mouse.clone());
        Ok(Self {
            window,
            renderer,
            scale,
            mouse,
            visible: false,
            items: Vec::new(),
            selected: 0,
            width: 0,
            height: 0,
        })
    }

    /// 显示菜单于屏幕坐标 (x,y)，初始高亮 selected。
    pub fn show(&mut self, items: Vec<MenuItemSpec>, x: i32, y: i32, selected: usize) {
        if items.is_empty() {
            return;
        }
        self.items = items;
        self.selected = selected;
        let (w, h) = self.render();
        self.width = w;
        self.height = h;

        let (px, py) = clamp_to_work_area(x, y, w, h);
        self.window.show(px, py);
        self.visible = true;
        unsafe {
            SetCapture(self.window.hwnd());
        }
    }

    /// 更新高亮项（键盘/悬停导航），仅重绘不移位。
    pub fn set_highlight(&mut self, selected: usize) {
        if !self.visible || selected >= self.items.len() {
            return;
        }
        self.selected = selected;
        self.render();
    }

    /// 渲染当前 items + 高亮，返回 (宽,高)。命中矩形同步给鼠标处理器。
    fn render(&mut self) -> (u32, u32) {
        let s = self.scale;
        let item_h = (FONT_PX * 1.9 * s).ceil();
        let pad = Edges::xy(12.0 * s, 4.0 * s);

        let mut max_label = 0.0f32;
        for it in &self.items {
            if !matches!(it.kind, MenuKind::Separator) {
                max_label = max_label.max(self.renderer.measure_text(&it.label).width);
            }
        }
        let item_w = (max_label + pad.l + pad.r).max(80.0 * s);

        let mut root = View::container(Layout::Column)
            .bg(BG)
            .border(BORDER, 1.0)
            .radius(6.0 * s)
            .pad(Edges::all(4.0 * s));

        for (i, it) in self.items.iter().enumerate() {
            if matches!(it.kind, MenuKind::Separator) {
                root = root.child(
                    View::container(Layout::Row)
                        .fixed_w(item_w)
                        .fixed_h(1.0_f32.max(s))
                        .margin(Edges::xy(0.0, 3.0 * s))
                        .bg(SEP),
                );
                continue;
            }
            let color = if it.enabled { FG } else { DISABLED };
            let mut item = View::container(Layout::Row)
                .fixed_w(item_w)
                .fixed_h(item_h)
                .pad(pad)
                .radius(4.0 * s)
                .cross(Align::Center)
                .child(View::leaf(it.label.clone(), color));
            if it.enabled {
                item = item.tag(i as i32);
            }
            if i == self.selected && it.enabled {
                item = item.bg(HL_BG);
            }
            root = root.child(item);
        }

        root.layout(0.0, 0.0, &self.renderer);
        let (w_f, h_f) = root.measured_size();
        let width = (w_f.ceil() as u32).max(60);
        let height = (h_f.ceil() as u32).max(24);

        let mut hits = Vec::new();
        root.collect_hits(&mut hits);

        self.window.resize(width, height);
        {
            let buf = self.window.buffer_mut();
            let n = (width * height * 4) as usize;
            buf[..n].fill(0);
            root.paint(buf, width, height, &self.renderer);
        }
        let _ = self.window.update();

        self.mouse.borrow_mut().item_rects = hits;
        (width, height)
    }

    pub fn hide(&mut self) {
        if self.visible {
            unsafe {
                let _ = ReleaseCapture();
                let _ = ShowWindow(self.window.hwnd(), SW_HIDE);
            }
            self.visible = false;
            self.mouse.borrow_mut().last_hover = -1;
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }
}

/// 菜单鼠标处理器：悬停/点击/外部点击均回送事件给协调器。
struct MenuMouse {
    item_rects: Vec<(i32, Rect)>,
    events: Sender<UiEvent>,
    last_hover: i32,
    hwnd: HWND,
}

impl MenuMouse {
    fn hit(&self, x: f32, y: f32) -> i32 {
        for (tag, r) in &self.item_rects {
            if r.contains(x, y) {
                return *tag;
            }
        }
        -1
    }

    fn close_window(&self) {
        unsafe {
            let _ = ReleaseCapture();
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }
}

impl WindowMouse for MenuMouse {
    fn on_message(
        &mut self,
        _hwnd: HWND,
        msg: u32,
        _wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        match msg {
            WM_MOUSEMOVE => {
                let (x, y) = pos(lparam);
                let i = self.hit(x, y);
                if i != self.last_hover {
                    self.last_hover = i;
                    let _ = self.events.send(UiEvent::MenuHover(i));
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                let (x, y) = pos(lparam);
                let i = self.hit(x, y);
                self.close_window();
                if i >= 0 {
                    let _ = self.events.send(UiEvent::MenuActivate(i as usize));
                } else {
                    let _ = self.events.send(UiEvent::MenuClose);
                }
                Some(LRESULT(0))
            }
            WM_RBUTTONDOWN => {
                self.close_window();
                let _ = self.events.send(UiEvent::MenuClose);
                Some(LRESULT(0))
            }
            WM_SETCURSOR => {
                unsafe {
                    if let Ok(c) = LoadCursorW(None, IDC_ARROW) {
                        SetCursor(c);
                    }
                }
                Some(LRESULT(1))
            }
            _ => None,
        }
    }
}

fn pos(lparam: LPARAM) -> (f32, f32) {
    let v = lparam.0 as u32;
    ((v & 0xFFFF) as i16 as f32, ((v >> 16) & 0xFFFF) as i16 as f32)
}

/// 写剪贴板（CF_UNICODETEXT）
pub fn set_clipboard_text(text: &str) {
    if text.is_empty() {
        return;
    }
    unsafe {
        if OpenClipboard(HWND::default()).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = wide.len() * std::mem::size_of::<u16>();
        if let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, bytes) {
            let ptr = GlobalLock(hmem) as *mut u16;
            if !ptr.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
                let _ = GlobalUnlock(hmem);
                let _ = SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(hmem.0));
            }
        }
        let _ = CloseClipboard();
    }
}

fn dpi_scale() -> f32 {
    #[cfg(windows)]
    {
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

/// 将菜单钳制在光标所在显示器工作区内（右/下溢出贴边）。
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
