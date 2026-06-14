//! 弹出菜单（右键候选菜单 + 功能主菜单）
//!
//! 与 Go 版本 `wind_input/internal/ui/popup_menu.go` + `unified_menu_build.go` 对齐（简化）。
//! 单窗口下钻式子菜单（栈 + "‹ 返回"）、✓ 勾选态、▶ 子菜单标识；
//! 鼠标本地悬停/点击，键盘经协调器 MenuKey 转发。UI 自管导航，仅把最终动作回送协调器。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Sender;

use crate::manager::{MenuItemSpec, MenuKind, UiEvent};
use crate::text::dwrite::TextRenderer;
use crate::view::{Align, Edges, Layout, Rect, View};
use crate::window::{LayeredWindow, WindowMouse};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, LoadCursorW, SetCursor, ShowWindow, IDC_ARROW, SW_HIDE, WM_LBUTTONDOWN,
    WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_SETCURSOR,
};

const FONT_PX: f32 = 14.0;
const BG: [u8; 4] = [250, 250, 250, 252];
const FG: [u8; 4] = [40, 40, 40, 255];
const DISABLED: [u8; 4] = [175, 175, 178, 255];
const BORDER: [u8; 4] = [205, 205, 208, 230];
const SEP: [u8; 4] = [228, 228, 230, 255];
const HL_BG: [u8; 4] = [225, 236, 252, 255];

/// 一个可渲染/可交互行的动作
enum RowAction {
    Back,
    Separator,
    Leaf(MenuKind),
    Drill(usize), // 进入 当前层 items[idx] 的子菜单
}

/// 渲染行（含勾选/子菜单标识）
struct Row {
    label: String,
    checked: bool,
    has_children: bool,
    enabled: bool,
    action: RowAction,
}

/// 菜单交互状态（与 wnd_proc 共享）。鼠标处理与键盘导航都改这里，dirty 触发重绘。
struct MenuState {
    /// 菜单层级栈（last = 当前层）
    stack: Vec<Vec<MenuItemSpec>>,
    /// 当前层渲染行（render 时由 PopupMenu 计算写入）
    rows: Vec<Row>,
    /// 当前层命中矩形 (行下标, 矩形)
    item_rects: Vec<(usize, Rect)>,
    /// 高亮行
    selected: usize,
    /// 需要重绘
    dirty: bool,
    /// 请求关闭
    closed: bool,
    events: Sender<UiEvent>,
    last_cursor: (i32, i32),
}

impl MenuState {
    fn cur_depth(&self) -> usize {
        self.stack.len()
    }

    /// 移动高亮到下一个可选行（跳过分隔），dir=±1。
    fn move_sel(&mut self, dir: i32) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        let mut i = self.selected as i32;
        for _ in 0..n {
            i = (i + dir).rem_euclid(n as i32);
            let idx = i as usize;
            if !matches!(self.rows[idx].action, RowAction::Separator) && self.rows[idx].enabled {
                self.selected = idx;
                self.dirty = true;
                return;
            }
        }
    }

    fn first_selectable(&self) -> usize {
        self.rows
            .iter()
            .position(|r| !matches!(r.action, RowAction::Separator) && r.enabled)
            .unwrap_or(0)
    }

    /// 激活某行：下钻/返回/触发动作/关闭。
    fn activate(&mut self, row: usize) {
        let Some(r) = self.rows.get(row) else { return };
        if !r.enabled {
            return;
        }
        match &r.action {
            RowAction::Back => self.pop_level(),
            RowAction::Separator => {}
            RowAction::Drill(idx) => {
                if let Some(children) =
                    self.stack.last().and_then(|lvl| lvl.get(*idx)).map(|it| it.children.clone())
                {
                    self.stack.push(children);
                    self.selected = self.first_selectable_for_top();
                    self.dirty = true;
                }
            }
            RowAction::Leaf(kind) => {
                let _ = self.events.send(UiEvent::MenuAction(*kind));
                self.closed = true;
            }
        }
    }

    fn pop_level(&mut self) {
        if self.stack.len() > 1 {
            self.stack.pop();
            self.selected = 0;
            self.dirty = true;
        } else {
            self.closed = true;
            let _ = self.events.send(UiEvent::MenuClose);
        }
    }

    /// 进入当前层 rows[selected] 若为子菜单。
    fn enter_submenu(&mut self) {
        if let Some(r) = self.rows.get(self.selected) {
            if r.has_children {
                let row = self.selected;
                self.activate(row);
            }
        }
    }

    // 子菜单刚 push 后还没 render rows，无法用 rows 求首项；用 stack 顶估计（含返回项偏移 1）。
    fn first_selectable_for_top(&self) -> usize {
        let lvl = match self.stack.last() {
            Some(l) => l,
            None => return 0,
        };
        let back = if self.stack.len() > 1 { 1 } else { 0 };
        for (i, it) in lvl.iter().enumerate() {
            if !matches!(it.kind, MenuKind::Separator) && it.enabled {
                return back + i;
            }
        }
        back
    }

    fn hit(&self, x: f32, y: f32) -> Option<usize> {
        self.item_rects
            .iter()
            .find(|(_, r)| r.contains(x, y))
            .map(|(i, _)| *i)
    }
}

/// 弹出菜单窗口
pub struct PopupMenu {
    window: LayeredWindow,
    renderer: TextRenderer,
    scale: f32,
    state: Rc<RefCell<MenuState>>,
    visible: bool,
    bg: [u8; 4],
    fg: [u8; 4],
    disabled: [u8; 4],
    border: [u8; 4],
    sep: [u8; 4],
    hl_bg: [u8; 4],
}

impl PopupMenu {
    pub fn new(events: Sender<UiEvent>) -> Result<Self, String> {
        let scale = dpi_scale();
        let window = LayeredWindow::create(None, 160, 120, "WindInputPopupMenu")?;
        let renderer = TextRenderer::new("Microsoft YaHei UI", FONT_PX * scale)?;
        let state = Rc::new(RefCell::new(MenuState {
            stack: Vec::new(),
            rows: Vec::new(),
            item_rects: Vec::new(),
            selected: 0,
            dirty: false,
            closed: false,
            events,
            last_cursor: (i32::MIN, i32::MIN),
        }));
        window.register_mouse(state.clone());
        Ok(Self {
            window,
            renderer,
            scale,
            state,
            visible: false,
            bg: BG,
            fg: FG,
            disabled: DISABLED,
            border: BORDER,
            sep: SEP,
            hl_bg: HL_BG,
        })
    }

    /// 应用主题（菜单各色）。
    pub fn set_theme(&mut self, theme: &wind_theme::ResolvedTheme) {
        self.bg = theme.color("menu_bg", BG);
        self.fg = theme.color("menu_text", FG);
        self.disabled = theme.color("menu_disabled", DISABLED);
        self.border = theme.color("menu_border", BORDER);
        self.sep = theme.color("menu_separator", SEP);
        self.hl_bg = theme.color("menu_hover_bg", HL_BG);
    }

    /// 显示菜单（顶层 items）于屏幕坐标 (x,y)。i32::MIN → 取光标位。
    pub fn show(&mut self, items: Vec<MenuItemSpec>, x: i32, y: i32) {
        if items.is_empty() {
            return;
        }
        {
            let mut st = self.state.borrow_mut();
            st.stack = vec![items];
            st.selected = st.first_selectable_for_top();
            st.dirty = false;
            st.closed = false;
            st.last_cursor = (i32::MIN, i32::MIN);
        }
        let (w, h) = self.render();

        let (ax, ay) = if x == i32::MIN || y == i32::MIN {
            let mut p = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut p);
            }
            (p.x, p.y)
        } else {
            (x, y)
        };
        let (px, py) = clamp_to_work_area(ax, ay, w, h);
        self.window.show(px, py);
        self.visible = true;
        unsafe {
            SetCapture(self.window.hwnd());
        }
    }

    /// UI 循环每轮调用：脏则重绘；请求关闭则隐藏。
    pub fn tick(&mut self) {
        if !self.visible {
            return;
        }
        let (dirty, closed) = {
            let st = self.state.borrow();
            (st.dirty, st.closed)
        };
        if closed {
            self.hide();
            return;
        }
        if dirty {
            self.state.borrow_mut().dirty = false;
            self.render();
        }
    }

    /// 键盘转发（协调器在组合期拦截方向键/回车/ESC 后下发）。
    pub fn on_key(&mut self, key: u32) {
        if !self.visible {
            return;
        }
        match key {
            0x26 => self.state.borrow_mut().move_sel(-1), // Up
            0x28 => self.state.borrow_mut().move_sel(1),  // Down
            0x27 => self.state.borrow_mut().enter_submenu(), // Right
            0x25 | 0x1B => {
                // Left / ESC：返回上层或关闭
                self.state.borrow_mut().pop_level();
            }
            0x0D | 0x20 => {
                // Enter / Space：激活当前
                let sel = self.state.borrow().selected;
                self.state.borrow_mut().activate(sel);
            }
            _ => {}
        }
        self.tick();
    }

    /// 渲染当前层，返回 (宽,高)。计算 rows + item_rects 写回 state。
    fn render(&mut self) -> (u32, u32) {
        let s = self.scale;
        let item_h = (FONT_PX * 1.9 * s).ceil();
        let pad = Edges::xy(12.0 * s, 4.0 * s);

        // 计算行
        let rows = self.compute_rows();

        // 统一项宽 = 最长行 + 内边距
        let mut max_label = 0.0f32;
        for r in &rows {
            if !matches!(r.action, RowAction::Separator) {
                let w = self.renderer.measure_text(&self.row_text(r)).width;
                max_label = max_label.max(w);
            }
        }
        let item_w = (max_label + pad.l + pad.r).max(90.0 * s);

        let selected = self.state.borrow().selected;
        let mut root = View::container(Layout::Column)
            .bg(self.bg)
            .border(self.border, 1.0)
            .radius(6.0 * s)
            .pad(Edges::all(4.0 * s));

        for (i, r) in rows.iter().enumerate() {
            if matches!(r.action, RowAction::Separator) {
                root = root.child(
                    View::container(Layout::Row)
                        .fixed_w(item_w)
                        .fixed_h(1.0_f32.max(s))
                        .margin(Edges::xy(0.0, 3.0 * s))
                        .bg(self.sep),
                );
                continue;
            }
            let color = if r.enabled { self.fg } else { self.disabled };
            let mut item = View::container(Layout::Row)
                .fixed_w(item_w)
                .fixed_h(item_h)
                .pad(pad)
                .radius(4.0 * s)
                .cross(Align::Center)
                .tag(i as i32)
                .child(View::leaf(self.row_text(r), color));
            if i == selected && r.enabled {
                item = item.bg(self.hl_bg);
            }
            root = root.child(item);
        }

        root.layout(0.0, 0.0, &self.renderer);
        let (w_f, h_f) = root.measured_size();
        let width = (w_f.ceil() as u32).max(80);
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

        // 写回 state
        {
            let mut st = self.state.borrow_mut();
            st.item_rects = hits.iter().map(|(t, r)| (*t as usize, *r)).collect();
            st.rows = rows;
        }
        (width, height)
    }

    /// 行显示文本：勾选前缀 + 标签 + 子菜单箭头。
    fn row_text(&self, r: &Row) -> String {
        let mark = if r.checked { "✓ " } else { "   " };
        let arrow = if r.has_children { "  ▶" } else { "" };
        format!("{}{}{}", mark, r.label, arrow)
    }

    /// 由当前层 items 计算行（含 "‹ 返回"）。
    fn compute_rows(&self) -> Vec<Row> {
        let st = self.state.borrow();
        let lvl = match st.stack.last() {
            Some(l) => l,
            None => return Vec::new(),
        };
        let mut rows = Vec::new();
        if st.stack.len() > 1 {
            rows.push(Row {
                label: "‹ 返回".into(),
                checked: false,
                has_children: false,
                enabled: true,
                action: RowAction::Back,
            });
        }
        for (i, it) in lvl.iter().enumerate() {
            match it.kind {
                MenuKind::Separator => rows.push(Row {
                    label: String::new(),
                    checked: false,
                    has_children: false,
                    enabled: false,
                    action: RowAction::Separator,
                }),
                MenuKind::Submenu => rows.push(Row {
                    label: it.label.clone(),
                    checked: it.checked,
                    has_children: true,
                    enabled: it.enabled,
                    action: RowAction::Drill(i),
                }),
                kind => rows.push(Row {
                    label: it.label.clone(),
                    checked: it.checked,
                    has_children: false,
                    enabled: it.enabled,
                    action: RowAction::Leaf(kind),
                }),
            }
        }
        rows
    }

    pub fn hide(&mut self) {
        if self.visible {
            unsafe {
                let _ = ReleaseCapture();
                let _ = ShowWindow(self.window.hwnd(), SW_HIDE);
            }
            self.visible = false;
            let mut st = self.state.borrow_mut();
            st.stack.clear();
            st.rows.clear();
            st.item_rects.clear();
            st.closed = false;
            st.dirty = false;
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }
}

impl WindowMouse for MenuState {
    fn on_message(
        &mut self,
        _hwnd: HWND,
        msg: u32,
        _wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        let v = lparam.0 as u32;
        let x = (v & 0xFFFF) as i16 as f32;
        let y = ((v >> 16) & 0xFFFF) as i16 as f32;
        match msg {
            WM_MOUSEMOVE => {
                if let Some(i) = self.hit(x, y) {
                    if self.selected != i && self.rows.get(i).map_or(false, |r| r.enabled) {
                        self.selected = i;
                        self.dirty = true;
                    }
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                match self.hit(x, y) {
                    Some(i) => self.activate(i),
                    None => {
                        // 点击菜单外 → 关闭
                        self.closed = true;
                        let _ = self.events.send(UiEvent::MenuClose);
                    }
                }
                Some(LRESULT(0))
            }
            WM_RBUTTONDOWN => {
                self.closed = true;
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
