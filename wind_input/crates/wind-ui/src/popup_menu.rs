//! 弹出菜单（右键候选菜单 + 功能主菜单）
//!
//! 标准多级级联菜单：父面板常驻，悬停带 ▶ 的项时子菜单作为独立窗口在右侧弹出，可层层展开。
//! 仿 Win32 原生菜单：只在根窗口 SetCapture 一次，捕获后所有鼠标消息以根窗口客户区坐标投递，
//! 再用屏幕坐标对各级面板命中测试。逻辑（结构变更）集中在 MenuState（wnd_proc 侧），
//! 窗口协调（渲染/定位/隐藏多余窗口）在 PopupMenu.tick() 侧。键盘经协调器 MenuKey 转发。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Sender;

use crate::manager::{MenuItemSpec, MenuKind, UiEvent};
use crate::sys::{
    GetCursorPos, HWND, IDC_ARROW, LPARAM, LRESULT, LoadCursorW, POINT, ReleaseCapture, SW_HIDE,
    SetCapture, SetCursor, ShowWindow, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_SETCURSOR,
    WPARAM,
};
use crate::text::dwrite::TextRenderer;
use crate::view::{Align, Edges, Layout, Rect, View};
use crate::window::{LayeredWindow, WindowMouse};
#[cfg(windows)]
use windows::Win32::Foundation::{HANDLE, HGLOBAL};
#[cfg(windows)]
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
#[cfg(windows)]
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
#[cfg(windows)]
use windows::Win32::System::Ole::CF_UNICODETEXT;

const FONT_PX: f32 = 14.0;
const BG: [u8; 4] = [250, 250, 250, 252];
const FG: [u8; 4] = [40, 40, 40, 255];
const DISABLED: [u8; 4] = [175, 175, 178, 255];
const BORDER: [u8; 4] = [205, 205, 208, 230];
const SEP: [u8; 4] = [228, 228, 230, 255];
const HL_BG: [u8; 4] = [225, 236, 252, 255];
/// 高亮项文字色默认值（与基态 FG 同——主题可经 menu_hover_text 覆盖）。
const HL_FG: [u8; 4] = FG;

/// 无选中哨兵
const NONE_SEL: usize = usize::MAX;

/// 一个打开的菜单层级
struct Level {
    /// 本层菜单项（内容源）
    items: Vec<MenuItemSpec>,
    /// 高亮行（== item 下标；NONE_SEL = 无）
    selected: usize,
    /// 屏幕坐标左上角（由 PopupMenu 渲染后写回）
    origin: (i32, i32),
    /// 窗口尺寸
    size: (u32, u32),
    /// 命中矩形 (item 下标, 窗口局部矩形)
    item_rects: Vec<(usize, Rect)>,
}

impl Level {
    fn new(items: Vec<MenuItemSpec>, selected: usize) -> Self {
        Self {
            items,
            selected,
            origin: (0, 0),
            size: (0, 0),
            item_rects: Vec::new(),
        }
    }
}

fn is_separator(it: &MenuItemSpec) -> bool {
    matches!(it.kind, MenuKind::Separator)
}
fn is_submenu(it: &MenuItemSpec) -> bool {
    matches!(it.kind, MenuKind::Submenu)
}
fn selectable(it: &MenuItemSpec) -> bool {
    !is_separator(it) && it.enabled
}

fn first_selectable(items: &[MenuItemSpec]) -> usize {
    items.iter().position(selectable).unwrap_or(NONE_SEL)
}

/// 菜单交互状态（与 wnd_proc 共享）。只做结构变更，dirty 触发 PopupMenu 协调重绘。
struct MenuState {
    /// 打开的层级链（last = 最深/当前活动层）
    levels: Vec<Level>,
    /// 根窗口（捕获窗口）屏幕原点，用于把客户坐标换算回屏幕坐标
    capture_origin: (i32, i32),
    /// 需要重新协调/重绘
    dirty: bool,
    /// 请求关闭
    closed: bool,
    events: Sender<UiEvent>,
}

impl MenuState {
    fn screen(&self, x: i32, y: i32) -> (i32, i32) {
        (self.capture_origin.0 + x, self.capture_origin.1 + y)
    }

    /// 屏幕坐标命中：从最深层往外找。返回 (层下标, Some(行) | None=面板空白)。
    fn find_hit(&self, sx: i32, sy: i32) -> Option<(usize, Option<usize>)> {
        for k in (0..self.levels.len()).rev() {
            let lv = &self.levels[k];
            let (ox, oy) = lv.origin;
            let (w, h) = lv.size;
            if sx >= ox && sx < ox + w as i32 && sy >= oy && sy < oy + h as i32 {
                let lx = (sx - ox) as f32;
                let ly = (sy - oy) as f32;
                for (row, r) in &lv.item_rects {
                    if r.contains(lx, ly) {
                        return Some((k, Some(*row)));
                    }
                }
                return Some((k, None));
            }
        }
        None
    }

    /// 压入 levels[k].selected 指向的子菜单。focus=true 时聚焦首个可选项（键盘）。
    fn open_child(&mut self, k: usize, focus: bool) {
        let sel = self.levels[k].selected;
        if sel == NONE_SEL {
            return;
        }
        let children = match self.levels[k].items.get(sel) {
            Some(it) if is_submenu(it) => it.children.clone(),
            _ => return,
        };
        let child_sel = if focus {
            first_selectable(&children)
        } else {
            NONE_SEL
        };
        self.levels.push(Level::new(children, child_sel));
        self.dirty = true;
    }

    /// 鼠标悬停到 (层 k, 行 r)：更新高亮、收起更深层、必要时展开子菜单。
    /// 取消第 k 层高亮（仅当 k 为最深层，避免误关已展开子菜单）。鼠标移出条目时调用。
    fn clear_hover(&mut self, k: usize) {
        if k + 1 == self.levels.len() && self.levels[k].selected != NONE_SEL {
            self.levels[k].selected = NONE_SEL;
            self.dirty = true;
        }
    }

    fn hover(&mut self, k: usize, r: usize) {
        let (ok, sub) = match self.levels[k].items.get(r) {
            Some(it) => (selectable(it), is_submenu(it)),
            None => {
                self.clear_hover(k);
                return;
            }
        };
        // 禁用项/分隔符：不高亮，并清掉残留高亮（鼠标不在可选条目即不应高亮）。
        if !ok {
            self.clear_hover(k);
            return;
        }
        if self.levels[k].selected != r {
            self.levels[k].selected = r;
            self.levels.truncate(k + 1);
            self.dirty = true;
        }
        // 子菜单尚未展开则展开（子层 selected=NONE，避免递归自动深入）
        if sub && self.levels.len() == k + 1 {
            self.open_child(k, false);
        }
    }

    /// 鼠标点击 (层 k, 行 r)。
    fn click(&mut self, k: usize, r: usize) {
        let (ok, sub, kind) = match self.levels[k].items.get(r) {
            Some(it) => (selectable(it), is_submenu(it), it.kind),
            None => return,
        };
        if !ok {
            return;
        }
        if self.levels[k].selected != r {
            self.levels[k].selected = r;
            self.levels.truncate(k + 1);
        }
        if sub {
            if self.levels.len() == k + 1 {
                self.open_child(k, false);
            }
        } else {
            let _ = self.events.send(UiEvent::MenuAction(kind));
            self.closed = true;
        }
    }

    /// 在某层移动高亮（跳过分隔/禁用），dir=±1。
    fn move_sel(&mut self, k: usize, dir: i32) {
        let n = self.levels[k].items.len();
        if n == 0 {
            return;
        }
        let cur = self.levels[k].selected;
        let mut i = if cur == NONE_SEL {
            if dir > 0 { -1 } else { 0 }
        } else {
            cur as i32
        };
        for _ in 0..n {
            i = (i + dir).rem_euclid(n as i32);
            if selectable(&self.levels[k].items[i as usize]) {
                self.levels[k].selected = i as usize;
                self.dirty = true;
                return;
            }
        }
    }

    fn deepest(&self) -> usize {
        self.levels.len().saturating_sub(1)
    }

    // —— 键盘动作（都作用于最深活动层）——
    fn key_up(&mut self) {
        let k = self.deepest();
        self.move_sel(k, -1);
    }
    fn key_down(&mut self) {
        let k = self.deepest();
        self.move_sel(k, 1);
    }
    fn key_right(&mut self) {
        let k = self.deepest();
        let sel = self.levels[k].selected;
        if sel != NONE_SEL
            && self.levels[k].items.get(sel).map_or(false, is_submenu)
            && self.levels.len() == k + 1
        {
            self.open_child(k, true);
        }
    }
    fn key_left(&mut self) {
        if self.levels.len() > 1 {
            self.levels.pop();
            self.dirty = true;
        } else {
            self.close();
        }
    }
    fn key_enter(&mut self) {
        let k = self.deepest();
        let sel = self.levels[k].selected;
        if sel == NONE_SEL {
            return;
        }
        let Some(it) = self.levels[k].items.get(sel) else {
            return;
        };
        if is_submenu(it) {
            if self.levels.len() == k + 1 {
                self.open_child(k, true);
            }
        } else if selectable(it) {
            let kind = it.kind;
            let _ = self.events.send(UiEvent::MenuAction(kind));
            self.closed = true;
        }
    }

    fn close(&mut self) {
        self.closed = true;
        let _ = self.events.send(UiEvent::MenuClose);
    }
}

/// 弹出菜单窗口（级联，窗口池按需增长）
pub struct PopupMenu {
    windows: Vec<LayeredWindow>,
    renderer: TextRenderer,
    scale: f32,
    state: Rc<RefCell<MenuState>>,
    visible: bool,
    /// 根面板锚点（已钳制到工作区）
    anchor: (i32, i32),
    bg: [u8; 4],
    fg: [u8; 4],
    disabled: [u8; 4],
    border: [u8; 4],
    sep: [u8; 4],
    hl_bg: [u8; 4],
    /// 高亮项文字色（menu_hover_text）：选中/悬停项文字与基态不同。
    hl_fg: [u8; 4],
    /// 菜单容器位图背景 + z 层（jidian menu.root 吃九宫格 panel + 角标水印）。
    bg_image: Option<crate::view::ViewImage>,
    layers: Vec<crate::view::ViewLayer>,
    /// 边框宽 / 圆角（设备像素，从 menu.root 节点 border 读取，px/dp 经 Dim 区分）。
    border_w: f32,
    radius: f32,
    /// 主题配置的软投影（menu.root.shadow）。
    shadow: Option<crate::view::SoftShadow>,
    /// 已应用主题（DPI 变化时按新缩放重解析几何）。
    theme: Option<wind_theme::Resolved>,
}

impl PopupMenu {
    pub fn new(events: Sender<UiEvent>) -> Result<Self, String> {
        let scale = dpi_scale();
        let renderer = TextRenderer::new("Microsoft YaHei UI", FONT_PX * scale)?;
        let state = Rc::new(RefCell::new(MenuState {
            levels: Vec::new(),
            capture_origin: (0, 0),
            dirty: false,
            closed: false,
            events,
        }));
        let mut menu = Self {
            windows: Vec::new(),
            renderer,
            scale,
            state,
            visible: false,
            anchor: (0, 0),
            bg: BG,
            fg: FG,
            disabled: DISABLED,
            border: BORDER,
            sep: SEP,
            hl_bg: HL_BG,
            hl_fg: HL_FG,
            bg_image: None,
            layers: Vec::new(),
            border_w: scale, // 默认 1dp（≈1 设备像素，细边清晰）
            radius: 6.0 * scale,
            shadow: None,
            theme: None,
        };
        // 预创建根窗口并绑定鼠标处理器（捕获后只有根窗口收消息）
        menu.ensure_windows(1)?;
        Ok(menu)
    }

    /// 确保窗口池至少有 n 个窗口（新窗口绑定共享 MenuState 处理器）。
    fn ensure_windows(&mut self, n: usize) -> Result<(), String> {
        while self.windows.len() < n {
            let w = LayeredWindow::create(None, 160, 120, "WindInputPopupMenu")?;
            w.register_mouse(self.state.clone());
            self.windows.push(w);
        }
        Ok(())
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

    /// 应用主题（菜单各色）。
    pub fn set_theme(&mut self, theme: &wind_theme::Resolved) {
        self.theme = Some(theme.clone());
        self.bg = theme.color("menu_bg", BG);
        self.fg = theme.color("menu_text", FG);
        self.disabled = theme.color("menu_disabled", DISABLED);
        self.border = theme.color("menu_border", BORDER);
        self.sep = theme.color("menu_separator", SEP);
        self.hl_bg = theme.color("menu_hover_bg", HL_BG);
        self.hl_fg = theme.color("menu_hover_text", self.fg);
        let s = self.scale;
        if let Some(node) = &theme.views.menu_root {
            self.bg_image = crate::theme_assets::rv_image(theme, node.bg_image.as_ref());
            self.layers = crate::theme_assets::rv_layers(theme, &node.layers, s);
            // 边框色/宽/圆角从 menu.root 节点读取（权威，px/dp 经 Dim 区分）；
            // border_color 默认已带 menu_border token 兜底（resolve build 传入）。
            if let Some(c) = node.border_color {
                self.border = c;
            }
            self.border_w = node
                .border_width
                .map(|d| d.resolve(s, 0.0))
                .unwrap_or(s)
                .max(1.0);
            self.radius = node
                .border_radius
                .map(|d| d.resolve(s, 0.0))
                .unwrap_or(6.0 * s);
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
        } else {
            self.bg_image = None;
            self.layers = Vec::new();
            self.border = theme.color("menu_border", BORDER);
            self.border_w = s;
            self.radius = 6.0 * s;
            self.shadow = None;
        }
    }

    /// 显示菜单（顶层 items）于屏幕坐标 (x,y)。i32::MIN → 取光标位。
    /// `y_bottom`：锚点区域下边界（工具栏底边）；`above=true` 时优先向上展开，
    /// 上方工作区空间不足则改为从 `y_bottom` 向下弹出。
    pub fn show(&mut self, items: Vec<MenuItemSpec>, x: i32, y: i32, y_bottom: i32, above: bool) {
        if items.is_empty() {
            return;
        }
        if self.ensure_windows(1).is_err() {
            return;
        }
        let (ax, mut ay) = if x == i32::MIN || y == i32::MIN {
            let mut p = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut p);
            }
            (p.x, p.y)
        } else {
            (x, y)
        };
        // DPI 动态化：先按显示点所在显示器取缩放，再测量/构建（几何依赖 scale）。
        self.ensure_scale(ax, ay);
        // 先测量根面板尺寸以钳制锚点（选中态不影响尺寸，传无选中即可）。
        let (_root, w, h, _hits) = self.build_view(&items, NONE_SEL);
        // above：菜单底边对齐 (x,y) 向上展开（工具栏菜单用，避免遮挡工具栏）。
        // 若向上展开后顶边低于工作区上边界，则翻转为从 y_bottom 向下弹出。
        if above {
            let tentative = ay - h as i32;
            let has_space = work_area_of(ax, ay)
                .map(|(_, top, _, _)| tentative >= top)
                .unwrap_or(true);
            if has_space {
                ay = tentative;
            } else {
                ay = y_bottom; // 上方空间不足，改为向下弹出
            }
        }
        self.anchor = clamp_to_work_area(ax, ay, w, h);
        {
            let mut st = self.state.borrow_mut();
            // 打开时不预选首项（仿 Win32 原生菜单）：避免首项被高亮“闪一下”；
            // 悬停或方向键导航再点亮（move_sel 从 NONE_SEL 起步落到首个可选项）。
            st.levels = vec![Level::new(items, NONE_SEL)];
            st.dirty = false;
            st.closed = false;
        }
        self.reconcile();
        self.visible = true;
        unsafe {
            SetCapture(self.windows[0].hwnd());
        }
    }

    /// UI 循环每轮调用：脏则协调重绘；请求关闭则隐藏。
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
            self.reconcile();
        }
    }

    /// 键盘转发（协调器在组合期拦截方向键/回车/ESC 后下发）。
    pub fn on_key(&mut self, key: u32) {
        if !self.visible {
            return;
        }
        {
            let mut st = self.state.borrow_mut();
            match key {
                0x26 => st.key_up(),           // Up
                0x28 => st.key_down(),         // Down
                0x27 => st.key_right(),        // Right → 展开子菜单
                0x25 => st.key_left(),         // Left → 收起/返回
                0x1B => st.close(),            // ESC → 关闭
                0x0D | 0x20 => st.key_enter(), // Enter / Space → 激活
                _ => {}
            }
        }
        self.tick();
    }

    /// 协调窗口：按当前 levels 渲染/定位每一级，隐藏多余窗口，写回几何。
    fn reconcile(&mut self) {
        // 快照内容，避免渲染期间持有 state 借用
        let snap: Vec<(Vec<MenuItemSpec>, usize)> = {
            let st = self.state.borrow();
            st.levels
                .iter()
                .map(|l| (l.items.clone(), l.selected))
                .collect()
        };
        if snap.is_empty() {
            return;
        }
        if self.ensure_windows(snap.len()).is_err() {
            return;
        }

        // 软投影四向扩边（与候选窗一致）：内容布局起点移到 (ml,mt)，窗口左上回移，阴影溢出。
        // geom = 内容几何（屏幕坐标，供 place_child 链式定位）；wgeom = 窗口几何（含 margin，
        // 供命中/写回）。命中 item_rects 为窗口相对（含 ml,mt），与 find_hit 的 screen−origin 一致。
        let (ml, mt, mr, mb) = self
            .shadow
            .as_ref()
            .map(|s| s.margins())
            .unwrap_or((0, 0, 0, 0));
        let mut geom: Vec<(i32, i32, u32, u32, Vec<(usize, Rect)>)> =
            Vec::with_capacity(snap.len());
        let mut wgeom: Vec<(i32, i32, u32, u32, Vec<(usize, Rect)>)> =
            Vec::with_capacity(snap.len());
        for k in 0..snap.len() {
            let (items, selected) = &snap[k];
            let (mut root, cw, ch, content_hits) = self.build_view(items, *selected);
            // 内容左上（屏幕）：顶层用 anchor；子级贴父内容右缘 + 高亮项纵向对齐。
            let (cox, coy) = if k == 0 {
                self.anchor
            } else {
                let (pox, poy, pw, ph, prects) = &geom[k - 1];
                let psel = snap[k - 1].1;
                let prect = prects.iter().find(|(i, _)| *i == psel).map(|(_, r)| *r);
                place_child((*pox, *poy), (*pw, *ph), prect, (cw, ch), self.scale)
            };
            let (win_w, win_h) = (cw + ml + mr, ch + mt + mb);
            let (wox, woy) = (cox - ml as i32, coy - mt as i32);
            // 有阴影时把内容重排到 (ml,mt) 并重采窗口相对命中；无阴影时直接用内容命中。
            let whits: Vec<(usize, Rect)> = if ml > 0 || mt > 0 {
                root.layout(ml as f32, mt as f32, &self.renderer);
                let mut raw = Vec::new();
                root.collect_hits(&mut raw);
                raw.iter().map(|(t, r)| (*t as usize, *r)).collect()
            } else {
                content_hits.clone()
            };
            // 绘制到窗口 k（拆分字段借用：renderer 与 windows 是不同字段，可并存）
            {
                let r = &self.renderer;
                let win = &mut self.windows[k];
                win.resize(win_w, win_h);
                let buf = win.buffer_mut();
                let n = (win_w * win_h * 4) as usize;
                buf[..n].fill(0);
                if let Some(s) = &self.shadow {
                    s.paint(
                        buf,
                        win_w,
                        win_h,
                        ml as f32,
                        mt as f32,
                        cw as f32,
                        ch as f32,
                        root.corner_radius,
                    );
                }
                root.paint(buf, win_w, win_h, r);
                let _ = win.update();
                win.show(wox, woy);
            }
            geom.push((cox, coy, cw, ch, content_hits));
            wgeom.push((wox, woy, win_w, win_h, whits));
        }

        // 隐藏多余窗口
        for k in snap.len()..self.windows.len() {
            self.windows[k].hide();
        }

        // 写回几何（窗口坐标：origin/size 含 margin，item_rects 窗口相对）。
        {
            let mut st = self.state.borrow_mut();
            for (k, (ox, oy, w, h, rects)) in wgeom.into_iter().enumerate() {
                if let Some(l) = st.levels.get_mut(k) {
                    l.origin = (ox, oy);
                    l.size = (w, h);
                    l.item_rects = rects;
                }
            }
            st.capture_origin = st.levels.first().map(|l| l.origin).unwrap_or((0, 0));
        }
    }

    /// 构建一层菜单的视图，返回 (根视图, 宽, 高, 命中矩形)。
    fn build_view(
        &self,
        items: &[MenuItemSpec],
        selected: usize,
    ) -> (View, u32, u32, Vec<(usize, Rect)>) {
        let s = self.scale;
        let item_h = (FONT_PX * 1.9 * s).ceil();
        let pad = Edges::xy(12.0 * s, 4.0 * s);

        // 统一项宽 = 勾选列 + 最长标签 + 内边距；子菜单项额外预留 ▸ 列宽（右对齐固定到末端）。
        let arrow_w = self.renderer.measure_text(SUBMENU_ARROW).width;
        let arrow_gap = 12.0 * s; // 标签与 ▸ 之间的最小留白
        // 任一项可勾选时，所有项预留固定勾选列，保证文字左对齐（✓ 与空白等宽）。
        let has_check = items.iter().any(|it| it.checked);
        let check_col = if has_check {
            self.renderer.measure_text(CHECK_MARK).width + 6.0 * s
        } else {
            0.0
        };
        let mut max_label = 0.0f32;
        for it in items {
            if !is_separator(it) {
                let mut w = check_col + self.renderer.measure_text(&it.label).width;
                if is_submenu(it) {
                    w += arrow_gap + arrow_w;
                }
                max_label = max_label.max(w);
            }
        }
        let item_w = (max_label + pad.l + pad.r).max(90.0 * s);

        let mut root = View::container(Layout::Column)
            .bg(self.bg)
            .border(self.border, self.border_w)
            .radius(self.radius)
            .pad(Edges::all(4.0 * s));
        // 主题位图背景 + 层（jidian menu.root 吃九宫格 panel + 角标水印）。
        if let Some(img) = &self.bg_image {
            root = root.bg_image(img.clone());
        }
        if !self.layers.is_empty() {
            root = root.layers(self.layers.clone());
        }

        for (i, it) in items.iter().enumerate() {
            if is_separator(it) {
                root = root.child(
                    View::container(Layout::Row)
                        .fixed_w(item_w)
                        .fixed_h(1.0_f32.max(s))
                        .margin(Edges::xy(0.0, 3.0 * s))
                        .bg(self.sep),
                );
                continue;
            }
            // 高亮项（选中/悬停且可用）文字用 hl_fg，否则基态 fg / 禁用色。
            let is_hl = i == selected && it.enabled;
            let color = if !it.enabled {
                self.disabled
            } else if is_hl {
                self.hl_fg
            } else {
                self.fg
            };
            let mut item = View::container(Layout::Row)
                .fixed_w(item_w)
                .fixed_h(item_h)
                .pad(pad)
                .radius(4.0 * s)
                .cross(Align::Center)
                .tag(i as i32);
            // 勾选列（固定宽）：✓ 仅勾选项显示，但所有项占同宽 → 标签对齐。
            if has_check {
                let mark = if it.checked { CHECK_MARK } else { "" };
                item = item.child(
                    View::leaf(mark, color)
                        .fixed_w(check_col)
                        .text_align(Align::Start),
                );
            }
            item = item.child(View::leaf(it.label.clone(), color));
            // 子菜单 ▸：弹性占位把它推到菜单项右端（与标签解耦，标准菜单观感）。
            if is_submenu(it) {
                item = item
                    .child(View::spacer())
                    .child(View::leaf(SUBMENU_ARROW, color));
            }
            if is_hl {
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
        let item_rects: Vec<(usize, Rect)> = hits.iter().map(|(t, r)| (*t as usize, *r)).collect();

        (root, width, height, item_rects)
    }

    pub fn hide(&mut self) {
        if self.visible {
            unsafe {
                let _ = ReleaseCapture();
            }
            for w in &self.windows {
                unsafe {
                    let _ = ShowWindow(w.hwnd(), SW_HIDE);
                }
            }
            self.visible = false;
            let mut st = self.state.borrow_mut();
            st.levels.clear();
            st.closed = false;
            st.dirty = false;
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// 返回根菜单窗口句柄（截图用；菜单不可见时返回 None）。
    #[cfg(windows)]
    pub fn hwnd(&self) -> Option<windows::Win32::Foundation::HWND> {
        self.windows.first().map(|w| w.hwnd())
    }

    /// 将根菜单当前渲染帧保存为 PNG 文件（截图用）。
    pub fn capture_to_file(&self, path: &std::path::Path) -> Result<(), String> {
        self.windows
            .first()
            .ok_or_else(|| "no menu window".to_string())?
            .capture_to_file(path)
    }
}

/// 勾选标记（占独立固定宽列，保证标签对齐）。
const CHECK_MARK: &str = "✓";

/// 子菜单指示符（固定到菜单项右端）。
const SUBMENU_ARROW: &str = "▸";

impl WindowMouse for MenuState {
    fn on_message(
        &mut self,
        _hwnd: HWND,
        msg: u32,
        _wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        let v = lparam.0 as u32;
        let x = (v & 0xFFFF) as i16 as i32;
        let y = ((v >> 16) & 0xFFFF) as i16 as i32;
        let (sx, sy) = self.screen(x, y);
        match msg {
            WM_MOUSEMOVE => {
                match self.find_hit(sx, sy) {
                    Some((k, Some(r))) => self.hover(k, r),
                    // 面板内非条目处（分隔/空白）→ 取消该层高亮
                    Some((k, None)) => self.clear_hover(k),
                    // 菜单外 → 不强制清（移出窗口保持，避免边缘闪烁）
                    None => {}
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                match self.find_hit(sx, sy) {
                    Some((k, Some(r))) => self.click(k, r),
                    Some((_, None)) => {} // 面板空白处：忽略
                    None => self.close(), // 菜单外 → 关闭
                }
                Some(LRESULT(0))
            }
            WM_RBUTTONDOWN => {
                self.close();
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
#[cfg(windows)]
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

/// 写剪贴板（macOS：经 `pbcopy` 子进程，无需 AppKit/主线程，服务进程即可用；对齐 Go clipboard_darwin）。
#[cfg(target_os = "macos")]
pub fn set_clipboard_text(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = match Command::new("/usr/bin/pbcopy")
        .stdin(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(text.as_bytes());
    }
    let _ = child.wait();
}

/// 写剪贴板（其它非 Windows mock：暂不接入平台剪贴板，空操作）。
#[cfg(all(not(windows), not(target_os = "macos")))]
pub fn set_clipboard_text(_text: &str) {}

/// 读剪贴板文本（CF_UNICODETEXT）。失败/无文本返回空串。
#[cfg(windows)]
pub fn get_clipboard_text() -> String {
    unsafe {
        if OpenClipboard(HWND::default()).is_err() {
            return String::new();
        }
        let mut out = String::new();
        if let Ok(h) = GetClipboardData(CF_UNICODETEXT.0 as u32) {
            let hglobal = HGLOBAL(h.0);
            let ptr = GlobalLock(hglobal) as *const u16;
            if !ptr.is_null() {
                // 读到 NUL 结尾的宽字符串。
                let mut len = 0usize;
                while *ptr.add(len) != 0 {
                    len += 1;
                }
                let slice = std::slice::from_raw_parts(ptr, len);
                out = String::from_utf16_lossy(slice);
                let _ = GlobalUnlock(hglobal);
            }
        }
        let _ = CloseClipboard();
        out
    }
}

/// 读剪贴板文本（macOS：经 `pbpaste` 子进程）。失败/无文本返回空串。
#[cfg(target_os = "macos")]
pub fn get_clipboard_text() -> String {
    use std::process::Command;
    match Command::new("/usr/bin/pbpaste").output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

/// 读剪贴板文本（其它非 Windows mock：返回空串）。
#[cfg(all(not(windows), not(target_os = "macos")))]
pub fn get_clipboard_text() -> String {
    String::new()
}

fn dpi_scale() -> f32 {
    #[cfg(windows)]
    {
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

/// 取某屏幕点所在显示器的工作区矩形 (left, top, right, bottom)。
fn work_area_of(x: i32, y: i32) -> Option<(i32, i32, i32, i32)> {
    #[cfg(windows)]
    {
        use windows::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
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
                return Some((wa.left, wa.top, wa.right, wa.bottom));
            }
        }
    }
    let _ = (x, y);
    None
}

/// 将菜单钳制在光标所在显示器工作区内（右/下溢出贴边）。
fn clamp_to_work_area(x: i32, y: i32, w: u32, h: u32) -> (i32, i32) {
    if let Some((left, top, right, bottom)) = work_area_of(x, y) {
        let (wi, hi) = (w as i32, h as i32);
        let mut nx = x;
        let mut ny = y;
        if nx + wi > right {
            nx = right - wi;
        }
        if ny + hi > bottom {
            ny = (y - hi).max(top);
        }
        if nx < left {
            nx = left;
        }
        if ny < top {
            ny = top;
        }
        return (nx, ny);
    }
    (x, y)
}

/// 子菜单定位：默认贴父面板右侧、纵向对齐父高亮项；右溢出则翻到左侧；最后钳制工作区。
fn place_child(
    parent_origin: (i32, i32),
    parent_size: (u32, u32),
    parent_item: Option<Rect>,
    child_size: (u32, u32),
    scale: f32,
) -> (i32, i32) {
    let (pox, poy) = parent_origin;
    let (pw, _ph) = parent_size;
    let (cw, ch) = (child_size.0 as i32, child_size.1 as i32);
    let overlap = (3.0 * scale) as i32;
    let pad_top = (4.0 * scale) as i32;

    let item_top = parent_item.map(|r| r.y as i32).unwrap_or(0);
    let mut x = pox + pw as i32 - overlap;
    let mut y = poy + item_top - pad_top;

    if let Some((left, top, right, bottom)) = work_area_of(pox, poy) {
        // 右侧放不下 → 翻到父面板左侧
        if x + cw > right {
            x = pox - cw + overlap;
        }
        if x < left {
            x = left;
        }
        if y + ch > bottom {
            y = bottom - ch;
        }
        if y < top {
            y = top;
        }
    }
    (x, y)
}
