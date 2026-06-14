//! 候选窗口：View 盒模型布局 + DirectWrite 文本 + Win32 Layered Window
//!
//! 与 Go 版本 `wind_input/internal/ui/manager_candidate.go` + `viewbox_build.go` 对齐。
//! 用 `crate::view` 的盒模型构建候选树（预编辑行 + 候选行[序号|文本] + 翻页指示），
//! measure/arrange 算出尺寸与每候选的绝对矩形（供鼠标命中），再 paint 到 BGRA 缓冲区。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Sender;

use crate::debounce::Debouncer;
use crate::manager::{UiEvent, HOVER_PAGE_NEXT as TAG_PAGE_NEXT, HOVER_PAGE_PREV as TAG_PAGE_PREV};
use crate::text::dwrite::TextRenderer;
use crate::view::{Align, Edges, Layout, Rect, View};
use crate::window::{LayeredWindow, WindowMouse};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, LoadCursorW, SetCursor, IDC_ARROW, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_RBUTTONDOWN, WM_SETCURSOR,
};

/// 候选词数据
#[derive(Debug, Clone)]
pub struct CandidateItem {
    pub text: String,
    pub code: String,
    /// 序号标签（如 "1" / "a"）；空则按位置自动用数字编号
    pub label: String,
    /// 悬停反查提示（逐字编码/拼音，多行）；空则用 code 兜底
    pub tooltip: String,
}

/// 候选窗口配置
pub struct CandidateWindowConfig {
    pub font_size: f32,
    pub per_page: usize,
    pub bg_color: [u8; 4],
    pub text_color: [u8; 4],
    pub highlight_color: [u8; 4],
    pub border_color: [u8; 4],
    pub selected_bg: [u8; 4],
    /// 鼠标悬停底色（比选中底色更淡，区分两种状态）
    pub hover_bg: [u8; 4],
    pub padding_x: f32,
    pub padding_y: f32,
    pub item_spacing: f32,
}

impl Default for CandidateWindowConfig {
    fn default() -> Self {
        let dpi_scale = Self::get_dpi_scale();
        let base_font_size = 24.0;
        let font_size = base_font_size * dpi_scale;

        Self {
            font_size,
            per_page: 5,
            bg_color: [255, 255, 255, 245],
            text_color: [51, 51, 51, 255],
            highlight_color: [0, 120, 215, 255],
            border_color: [200, 200, 200, 200],
            selected_bg: [230, 240, 255, 255],
            hover_bg: [238, 242, 247, 255],
            padding_x: 12.0 * dpi_scale,
            padding_y: 8.0 * dpi_scale,
            item_spacing: 4.0 * dpi_scale,
        }
    }
}

impl CandidateWindowConfig {
    /// 序号标签颜色（比正文淡）
    fn marker_color(&self) -> [u8; 4] {
        [140, 140, 145, 255]
    }

    pub(crate) fn get_dpi_scale() -> f32 {
        #[cfg(windows)]
        {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::Graphics::Gdi::*;
            unsafe {
                let hdc = GetDC(HWND::default());
                let dpi = GetDeviceCaps(hdc, LOGPIXELSY);
                ReleaseDC(HWND::default(), hdc);
                dpi as f32 / 96.0
            }
        }
        #[cfg(not(windows))]
        {
            1.0
        }
    }
}

/// 候选窗口
pub struct CandidateWindow {
    window: LayeredWindow,
    config: CandidateWindowConfig,
    candidates: Vec<CandidateItem>,
    preedit: String,
    selected: usize,
    /// 鼠标悬停项（页内下标），-1 表示无；与 selected 独立渲染
    hover: i32,
    page: usize,
    total_pages: usize,
    visible: bool,
    x: i32,
    y: i32,
    /// 光标高度（上翻定位用）
    caret_height: i32,
    /// 当前光标坐标是否有效
    caret_valid: bool,
    /// 组合期间锚定位置（首次显示时按光标算定，之后保持不动）；隐藏时清空
    anchor: Option<(i32, i32)>,
    /// 锚点是否已按有效坐标锁定（false=临时位置，待有效坐标到达后重锚）
    anchor_locked: bool,
    text_renderer: TextRenderer,
    /// arrange 后收集的候选命中矩形：(候选页内下标, 矩形)，供鼠标层使用
    hit_rects: Vec<(i32, Rect)>,
    /// 鼠标处理器（与 window 共享，wnd_proc 经注册表回调）
    mouse: Rc<RefCell<CandidateMouse>>,
    /// 悬停编码反查气泡
    tooltip: Option<crate::tooltip::Tooltip>,
    /// 已解析主题（颜色/几何）；默认兜底清风蓝
    theme: wind_theme::ResolvedTheme,
    /// DPI 缩放（主题几何为逻辑像素，渲染时乘此）
    scale: f32,
}

impl CandidateWindow {
    pub fn new(config: CandidateWindowConfig, events: Sender<UiEvent>) -> Result<Self, String> {
        let window = LayeredWindow::create(None, 400, 200, "WindInputCandidate")?;
        let text_renderer = TextRenderer::new("Microsoft YaHei UI", config.font_size)?;
        let mouse = Rc::new(RefCell::new(CandidateMouse {
            hit_rects: Vec::new(),
            events,
            last_hover: -1,
            last_cursor: (i32::MIN, i32::MIN),
            hover_debounce: Debouncer::new(120),
        }));
        window.register_mouse(mouse.clone());
        Ok(Self {
            window,
            config,
            candidates: Vec::new(),
            preedit: String::new(),
            selected: 0,
            hover: -1,
            page: 1,
            total_pages: 1,
            visible: false,
            x: 0,
            y: 0,
            caret_height: 0,
            caret_valid: false,
            anchor: None,
            anchor_locked: false,
            text_renderer,
            hit_rects: Vec::new(),
            mouse,
            tooltip: crate::tooltip::Tooltip::new().ok(),
            theme: wind_theme::ResolvedTheme::default(),
            scale: CandidateWindowConfig::get_dpi_scale(),
        })
    }

    /// 应用主题（协调器下发）。
    pub fn set_theme(&mut self, theme: wind_theme::ResolvedTheme) {
        self.theme = theme;
    }

    pub fn update(
        &mut self,
        preedit: &str,
        candidates: Vec<CandidateItem>,
        selected: usize,
        hover: i32,
        page: usize,
        total_pages: usize,
    ) {
        self.preedit = preedit.to_string();
        self.candidates = candidates;
        self.selected = selected;
        self.hover = hover;
        self.page = page.max(1);
        self.total_pages = total_pages.max(1);
    }

    pub fn set_position(&mut self, x: i32, y: i32, caret_height: i32, caret_valid: bool) {
        self.x = x;
        self.y = y;
        self.caret_height = caret_height;
        self.caret_valid = caret_valid;
    }

    /// 候选页内命中矩形（绝对坐标，相对窗口左上角）
    pub fn hit_rects(&self) -> &[(i32, Rect)] {
        &self.hit_rects
    }

    pub fn show(&mut self) {
        if self.candidates.is_empty() && self.preedit.is_empty() {
            self.hide();
            return;
        }

        // 构建并测量 View 树
        let mut root = self.build_tree();
        root.layout(0.0, 0.0, &self.text_renderer);
        let (w_f, h_f) = root.measured_size();
        let width = (w_f.ceil() as u32).max(40);
        let height = (h_f.ceil() as u32).max(24);

        // 收集候选命中矩形并同步给鼠标处理器
        self.hit_rects.clear();
        root.collect_hits(&mut self.hit_rects);
        {
            let mut m = self.mouse.borrow_mut();
            m.hit_rects = self.hit_rects.clone();
            m.last_hover = -1;
        }

        self.window.resize(width, height);

        // 透明清屏 + 绘制
        {
            let buf = self.window.buffer_mut();
            let buf_size = (width * height * 4) as usize;
            buf[..buf_size].fill(0);
            root.paint(buf, width, height, &self.text_renderer);
        }

        tracing::debug!(
            "CandidateWindow::show pos=({},{}), size=({},{}), candidates={}, page={}/{}",
            self.x, self.y, width, height, self.candidates.len(), self.page, self.total_pages
        );

        if let Err(e) = self.window.update() {
            tracing::warn!("CandidateWindow update failed: {}", e);
        }

        // 位置锚定：组合期间固定——锚点一旦按有效坐标锁定，打字/悬停/翻页刷新都复用，
        // 避免窗口随光标/刷新漂移。首次连接尚无有效坐标时，锚点为"临时"，
        // 待有效坐标到达再重锚（避免卡在左上角不恢复）。
        let keep = self.visible && self.anchor_locked && self.anchor.is_some();
        let (px, py) = if keep {
            self.anchor.unwrap()
        } else {
            let p = Self::clamp_to_work_area(self.x, self.y, self.caret_height, width, height);
            self.anchor = Some(p);
            self.anchor_locked = self.caret_valid; // 仅有效坐标才锁定
            p
        };
        self.window.show(px, py);
        self.visible = true;
        self.update_tooltip(px, py);
    }

    /// 悬停时在该候选下方显示其编码（反查）；无悬停或无编码则隐藏。
    fn update_tooltip(&mut self, px: i32, py: i32) {
        let hover = self.hover;
        // 仅候选项（非翻页器 tag）显示反查提示
        let info = if (0..TAG_PAGE_PREV).contains(&hover) {
            let code = self
                .candidates
                .get(hover as usize)
                .map(|c| c.tooltip.clone())
                .unwrap_or_default();
            self.hit_rects
                .iter()
                .find(|(t, _)| *t == hover)
                .map(|(_, r)| *r)
                .filter(|_| !code.is_empty())
                .map(|r| (code, r))
        } else {
            None
        };
        if let Some(tip) = self.tooltip.as_mut() {
            match info {
                Some((code, r)) => {
                    tip.show(&code, px + r.x as i32, py + (r.y + r.h) as i32 + 2)
                }
                None => tip.hide(),
            }
        }
    }

    /// 将候选窗钳制在光标所在显示器的工作区内：
    /// 默认显示在光标下方（+gap）；下方空间不足则上翻到光标上方；
    /// 左右溢出则贴边。避免窗口跑到屏幕外。
    fn clamp_to_work_area(caret_x: i32, caret_y: i32, caret_h: i32, w: u32, h: u32) -> (i32, i32) {
        let gap = 2;
        // caret_y 为光标底端（与 Go 一致）：默认显示在其下方，仅留 gap
        let (mut x, mut y) = (caret_x, caret_y + gap);
        #[cfg(windows)]
        {
            use windows::Win32::Foundation::POINT;
            use windows::Win32::Graphics::Gdi::{
                GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
            };
            unsafe {
                let pt = POINT { x: caret_x, y: caret_y };
                let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
                let mut mi = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if GetMonitorInfoW(mon, &mut mi).as_bool() {
                    let wa = mi.rcWork;
                    let (wi, hi) = (w as i32, h as i32);
                    // 下方放不下 → 上翻到光标上方（光标顶端 = caret_y - caret_h）
                    if y + hi > wa.bottom {
                        let above = caret_y - caret_h.max(0) - hi - gap;
                        y = if above >= wa.top { above } else { wa.bottom - hi };
                    }
                    // 左右钳制
                    if x + wi > wa.right {
                        x = wa.right - wi;
                    }
                    if x < wa.left {
                        x = wa.left;
                    }
                    // 垂直兜底
                    if y < wa.top {
                        y = wa.top;
                    }
                }
            }
        }
        (x, y)
    }

    /// 按当前状态构建候选视图树（横向布局）
    fn build_tree(&self) -> View {
        let t = &self.theme;
        let s = self.scale;
        let gap = self.config.item_spacing.max(2.0);
        let fs = self.config.font_size;
        // 主题四边内边距（逻辑像素）→ 设备像素
        let edges = |p: &wind_theme::Pad| Edges {
            l: p.l * s,
            t: p.t * s,
            r: p.r * s,
            b: p.b * s,
        };

        let mut root = View::container(Layout::Column)
            .bg(t.win_bg)
            .border(t.win_border, (t.win_border_width * s).max(1.0))
            .radius(t.win_radius * s)
            .pad(edges(&t.win_pad))
            .gap(gap);

        // 预编辑行（主题背景带 + 文本色）
        if !self.preedit.is_empty() {
            root = root.child(
                View::container(Layout::Row)
                    .bg(t.preedit_bg)
                    .radius(t.item_radius * s)
                    .pad(edges(&t.preedit_pad))
                    .child(View::leaf(self.preedit.clone(), t.preedit_color)),
            );
        }

        // 候选行：[序号 文本] cell 横排
        let mut row = View::container(Layout::Row).gap(gap * 2.0).cross(Align::Center);
        for (i, cand) in self.candidates.iter().enumerate() {
            let marker = if cand.label.is_empty() {
                (i + 1).to_string()
            } else {
                cand.label.clone()
            };
            let is_sel = i == self.selected;
            let is_hover = self.hover >= 0 && self.hover as usize == i;
            let txt_color = if is_sel { t.sel_text } else { t.text_color };

            // 序号：圆圈样式 → 带底色 + 大圆角（药丸近似圆）
            let mut idx_leaf = View::leaf(marker, t.index_color);
            if t.index_circle {
                idx_leaf = idx_leaf
                    .bg(t.index_circle_bg)
                    .radius(fs * 0.5)
                    .pad(Edges::xy(s * 4.0, s * 1.0));
            }

            let mut item = View::container(Layout::Row)
                .cross(Align::Center)
                .gap(t.text_margin_l * s)
                .pad(edges(&t.item_pad))
                .radius(t.item_radius * s)
                .tag(i as i32)
                .child(idx_leaf)
                .child(View::leaf(cand.text.clone(), txt_color));
            // 选中底色优先于悬停底色（两者独立：选中=空格上屏目标，悬停=鼠标提示）
            if is_sel {
                item = item.bg(t.sel_bg);
            } else if is_hover {
                item = item.bg(t.hover_bg);
            }
            row = row.child(item);
        }

        // 翻页器（多页时）：‹ p/t › —— 箭头可点击翻页，带悬停高亮 + 禁用态
        if self.total_pages > 1 {
            let disabled = t.color("text_hint", [180, 180, 185, 255]);
            let marker_c = t.color("text_dim", [140, 140, 145, 255]);
            let arrow = |txt: &str, tag: i32, enabled: bool, hovered: bool| {
                let color = if enabled { t.accent_bar } else { disabled };
                let mut v = View::leaf(txt, color)
                    .pad(Edges::xy(gap * 1.2, gap * 0.5))
                    .radius(t.item_radius * s)
                    .cross(Align::Center);
                if enabled {
                    v = v.tag(tag); // 仅启用项参与命中
                    if hovered {
                        v = v.bg(t.hover_bg);
                    }
                }
                v
            };
            let prev_on = self.page > 1;
            let next_on = self.page < self.total_pages;
            row = row
                .child(
                    arrow("‹", TAG_PAGE_PREV, prev_on, self.hover == TAG_PAGE_PREV)
                        .margin(Edges::xy(gap, 0.0)),
                )
                .child(View::leaf(
                    format!("{}/{}", self.page, self.total_pages),
                    marker_c,
                ))
                .child(arrow("›", TAG_PAGE_NEXT, next_on, self.hover == TAG_PAGE_NEXT));
        }

        root.child(row)
    }

    pub fn hide(&mut self) {
        self.window.hide();
        self.visible = false;
        self.anchor = None; // 组合结束，下次显示重新锚定
        self.anchor_locked = false;
        self.mouse.borrow_mut().reset_hover();
        if let Some(t) = self.tooltip.as_mut() {
            t.hide();
        }
    }

    /// UI 循环每轮调用：推进悬停防抖（稳定后才发出 Hover 事件）。
    pub fn tick(&self) {
        self.mouse.borrow_mut().flush();
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn candidates(&self) -> &[CandidateItem] {
        &self.candidates
    }

    pub fn hwnd(&self) -> windows::Win32::Foundation::HWND {
        self.window.hwnd()
    }
}

/// 候选窗鼠标处理器：命中候选→选词，悬停→高亮，滚轮→翻页。
/// 命中矩形为窗口本地坐标（绘制于 0,0），与 WM_* 的 client 坐标一致。
pub struct CandidateMouse {
    hit_rects: Vec<(i32, Rect)>,
    events: Sender<UiEvent>,
    /// 已生效（已发出）的悬停目标，去重用
    last_hover: i32,
    /// 上次物理光标屏幕坐标——过滤内容变化引起的伪 WM_MOUSEMOVE
    last_cursor: (i32, i32),
    /// 悬停防抖：稳定后才发出（避免打字/快速划过的高亮+tooltip 闪烁）
    hover_debounce: Debouncer<i32>,
}

impl CandidateMouse {
    /// 由 UI 循环每轮调用：到期则发出去抖后的悬停目标。
    fn flush(&mut self) {
        if let Some(t) = self.hover_debounce.poll() {
            if t != self.last_hover {
                self.last_hover = t;
                let _ = self.events.send(UiEvent::Hover(t));
            }
        }
    }

    /// 重置悬停状态（窗口隐藏 / 新组合）。
    fn reset_hover(&mut self) {
        self.hover_debounce.cancel();
        self.last_hover = -1;
        self.last_cursor = (i32::MIN, i32::MIN);
    }
}

impl CandidateMouse {
    fn hit(&self, x: f32, y: f32) -> i32 {
        for (tag, r) in &self.hit_rects {
            if r.contains(x, y) {
                return *tag;
            }
        }
        -1
    }
}

/// 从 lParam 解出 client 坐标（低/高 16 位有符号）
fn mouse_pos(lparam: LPARAM) -> (f32, f32) {
    let v = lparam.0 as u32;
    let x = (v & 0xFFFF) as i16 as f32;
    let y = ((v >> 16) & 0xFFFF) as i16 as f32;
    (x, y)
}

impl WindowMouse for CandidateMouse {
    fn on_message(
        &mut self,
        _hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        match msg {
            WM_LBUTTONDOWN => {
                let (x, y) = mouse_pos(lparam);
                let i = self.hit(x, y);
                match i {
                    TAG_PAGE_PREV => {
                        let _ = self.events.send(UiEvent::Page(-1));
                    }
                    TAG_PAGE_NEXT => {
                        let _ = self.events.send(UiEvent::Page(1));
                    }
                    i if i >= 0 => {
                        let _ = self.events.send(UiEvent::CandidateSelect(i as usize));
                    }
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                // 物理移动门控：内容变化（打字换候选/窗口刷新）也会产生 WM_MOUSEMOVE，
                // 但此时物理光标屏幕坐标不变 → 忽略，避免静止鼠标下方候选变化引起闪烁。
                let (sx, sy) = unsafe {
                    let mut p = windows::Win32::Foundation::POINT::default();
                    let _ = GetCursorPos(&mut p);
                    (p.x, p.y)
                };
                if (sx, sy) == self.last_cursor {
                    return Some(LRESULT(0));
                }
                self.last_cursor = (sx, sy);
                let (x, y) = mouse_pos(lparam);
                // 命中目标经防抖：稳定 ~120ms 后才高亮/显示 tooltip
                let raw = self.hit(x, y);
                self.hover_debounce.trigger(raw);
                Some(LRESULT(0))
            }
            WM_RBUTTONDOWN => {
                let (x, y) = mouse_pos(lparam);
                let i = self.hit(x, y);
                // 用屏幕光标坐标定位菜单
                let (sx, sy) = unsafe {
                    let mut p = windows::Win32::Foundation::POINT::default();
                    let _ = GetCursorPos(&mut p);
                    (p.x, p.y)
                };
                if i >= 0 {
                    // 命中候选 → 词条菜单
                    let _ = self.events.send(UiEvent::RequestCandidateMenu {
                        page_local: i as usize,
                        x: sx,
                        y: sy,
                    });
                } else {
                    // 空白处 → 功能主菜单
                    let _ = self.events.send(UiEvent::RequestMainMenu { x: sx, y: sy });
                }
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                // 高 16 位为有符号滚动量：上滚(>0)→上一页，下滚(<0)→下一页
                let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
                let dir = if delta > 0 { -1 } else { 1 };
                let _ = self.events.send(UiEvent::Page(dir));
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
