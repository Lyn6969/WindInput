//! 工具栏窗口：常驻状态指示器（中英 / 方案 / 标点 / 全半角）。
//!
//! 与 Go 版本 `wind_input/internal/ui/toolbar_window.go` 对齐（简化版）。
//! 横向圆角小条，每格一个状态；中文模式格高亮。固定显示于工作区右下角。
//! 点击切换暂未实现（后续 UI 统一优化阶段补齐拖动 + 命中），当前为展示用。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Sender;

use crate::manager::{ToolbarAction, UiEvent};
use crate::sys::{
    GetCursorPos, GetWindowRect, HWND, HWND_TOPMOST, IDC_ARROW, IDC_SIZEALL, LPARAM, LRESULT,
    LoadCursorW, POINT, RECT, ReleaseCapture, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetCapture,
    SetCursor, SetWindowPos, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSELEAVE, WM_MOUSEMOVE,
    WM_RBUTTONDOWN, WM_SETCURSOR, WPARAM,
};
use crate::text::dwrite::TextRenderer;
use crate::view::Rect;
use crate::window::{LayeredWindow, WindowMouse};

/// 工具栏状态（由协调器推送）
#[derive(Debug, Clone)]
pub struct ToolbarState {
    pub chinese_mode: bool,
    /// 方案友好名（如 "五笔" / "拼音"），与中英状态合并显示在同一格
    pub schema_label: String,
    pub full_width: bool,
    pub chinese_punct: bool,
    /// 简繁转换当前是否启用（格内显示 "繁" 并高亮）
    pub s2t_enabled: bool,
    /// 是否显示简繁格（默认 false；用户开启简繁功能后显示）
    pub s2t_shown: bool,
}

impl Default for ToolbarState {
    fn default() -> Self {
        Self {
            chinese_mode: true,
            schema_label: "五笔".to_string(),
            full_width: false,
            chinese_punct: true,
            s2t_enabled: false,
            s2t_shown: false,
        }
    }
}

/// 一个单元格：文本 + 高亮(激活态，如中文/简繁开) + 淡显(次要状态，如半角/简) + 点击动作
struct Cell {
    text: String,
    highlight: bool,
    dim: bool,
    action: ToolbarAction,
}

// 标点状态图标：外部 SVG 文件（res/icons/）编译期嵌入（include_str!）。
// SVG 仅作 alpha 蒙版（形状黑色填充即可），颜色由工具栏按主题 tint；位置精确、不受字体基线影响。
// 要调整符号样式，直接编辑这两个 svg 文件即可（无需改 Rust 代码）。
/// 全角（中文）标点 。，
const PUNCT_FULL_SVG: &str = include_str!("../res/icons/punct_full.svg");
/// 半角（英文）标点 .,
const PUNCT_HALF_SVG: &str = include_str!("../res/icons/punct_half.svg");
/// 全角宽度：满月（实心圆）—— 对齐微软五笔全/半角月亮状态。
const WIDTH_FULL_SVG: &str = include_str!("../res/icons/width_full.svg");
/// 半角宽度：月牙（弯月）。
const WIDTH_HALF_SVG: &str = include_str!("../res/icons/width_half.svg");

/// 工具栏窗口
pub struct Toolbar {
    window: LayeredWindow,
    renderer: TextRenderer,
    scale: f32,
    visible: bool,
    /// 鼠标处理器（与 window 共享，wnd_proc 经注册表回调）；位置存于其中以便拖动同步
    mouse: Rc<RefCell<ToolbarMouse>>,
    // 主题色（默认浅色，set_theme 加载主题后覆盖）
    bg: [u8; 4],
    fg: [u8; 4],
    hl_bg: [u8; 4],
    hl_fg: [u8; 4],
    sep: [u8; 4],
    grip: [u8; 4],
    settings_icon: [u8; 4],
    hover_bg: [u8; 4],
    /// 最近一次状态（供 hover 变化时本地重绘，无需协调器往返）
    last_state: Option<ToolbarState>,
    /// 已渲染的悬停格下标（-1=无）；tick 检测光标位置变化后据此决定是否重绘
    rendered_hover: i32,
}

impl Toolbar {
    // 视觉常量（逻辑像素，随 DPI 缩放）
    const HEIGHT: f32 = 30.0;
    const GRIP_W: f32 = 12.0;
    const MIN_CELL_W: f32 = 26.0;
    const FONT_PX: f32 = 15.0;

    // 默认浅色配色（主题加载后由 set_theme 覆盖，以下为无主题时的兜底值）
    const BG: [u8; 4] = [255, 255, 255, 245];      // 白色半透明底
    const FG: [u8; 4] = [72, 72, 78, 255];           // 正常文字深灰
    const HL_BG: [u8; 4] = [66, 133, 244, 255];      // 高亮蓝（中文模式 / 简繁启用）
    const HL_FG: [u8; 4] = [255, 255, 255, 255];
    const SEP: [u8; 4] = [214, 214, 220, 255];       // 浅灰分隔线
    const GRIP: [u8; 4] = [186, 186, 194, 255];      // 拖动点
    const SETTINGS_ICON: [u8; 4] = [140, 140, 148, 255]; // 设置图标（比普通文字更淡）
    const HOVER_BG: [u8; 4] = [0, 0, 0, 13];         // 鼠标悬停高亮（极淡，~5% 黑）

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
            hover_idx: -1,
            dirty: false,
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
            settings_icon: Self::SETTINGS_ICON,
            hover_bg: Self::HOVER_BG,
            last_state: None,
            rendered_hover: -1,
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
        self.settings_icon = theme.color("toolbar_settings_icon", self.settings_icon);
        self.hover_bg = theme.color("toolbar_hover", self.hover_bg);
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

    /// 根据状态构建单元格序列。
    /// 布局：拖动条 | 中英状态（含方案名）| 符号 | 全半角 | [简繁] | 设置图标
    fn cells(state: &ToolbarState) -> Vec<Cell> {
        // 中英状态：方案已统一并入此格，仅显示中/英（方案切换走右键菜单）。
        let mode_text = if state.chinese_mode { "中" } else { "英" };

        let mut cells = vec![
            Cell {
                text: mode_text.to_string(),
                highlight: state.chinese_mode,
                dim: false,
                action: ToolbarAction::ToggleMode,
            },
            // 标点格：文本留空，渲染时按全/半角矢量绘制句号+逗号（不依赖字体字形定位）。
            Cell {
                text: String::new(),
                highlight: false,
                dim: false,
                action: ToolbarAction::TogglePunct,
            },
            // 全/半角格：文本留空，渲染时按状态画月亮 SVG（满月=全角 / 弯月=半角，对齐微软五笔）。
            Cell {
                text: String::new(),
                highlight: false,
                dim: false,
                action: ToolbarAction::ToggleWidth,
            },
        ];

        // 简繁格：默认不显示（s2t_shown=false），用户开启简繁功能后显示。
        if state.s2t_shown {
            cells.push(Cell {
                text: if state.s2t_enabled { "繁" } else { "简" }.to_string(),
                highlight: state.s2t_enabled,
                dim: !state.s2t_enabled,
                action: ToolbarAction::ToggleS2t,
            });
        }

        // 设置（始终显示在末尾）：文本留空，渲染时画矢量齿轮（不依赖字体字形）。
        cells.push(Cell {
            text: String::new(),
            highlight: false,
            dim: false,
            action: ToolbarAction::OpenSettings,
        });

        cells
    }

    /// DPI 动态化：按工具栏当前位置所在显示器实时取缩放（拖到别的显示器后自动适配）。
    /// 工具栏仅颜色随主题、几何随 scale 现算，故只需更新 scale 与字号。
    fn ensure_scale(&mut self) {
        let pos = self.mouse.borrow().pos.unwrap_or((0, 0));
        let sc = crate::dpi::scale_for_point(pos.0, pos.1);
        if (sc - self.scale).abs() > 0.01 {
            self.scale = sc;
            self.renderer.set_base_size(Self::FONT_PX * sc);
        }
    }

    /// 更新状态并重绘（首次会计算位置并显示）。缓存状态以便 hover 变化时本地重绘。
    pub fn update(&mut self, state: &ToolbarState) {
        self.last_state = Some(state.clone());
        let hover = self.rendered_hover;
        self.render(state, hover);
    }

    /// 实际渲染（hover_idx=当前悬停格下标，-1 无）。update 与 tick 均经此单点渲染。
    fn render(&mut self, state: &ToolbarState, hover_idx: i32) {
        self.ensure_scale();
        let s = self.scale;
        let height = (Self::HEIGHT * s).ceil();
        let grip_w = (Self::GRIP_W * s).ceil();
        let min_cell = Self::MIN_CELL_W * s;

        let cells = Self::cells(state);

        // 所有状态格统一方形（宽=高，下限 min_cell）：标点/简繁等图标与文字均居中于等宽方格，
        // 状态切换不改变单元格宽度，工具栏整体宽度稳定不抖动。
        let cell_w = height.max(min_cell);
        let cell_widths: Vec<f32> = cells.iter().map(|_| cell_w).collect();
        let total_w: f32 = grip_w + cell_w * cells.len() as f32;
        let w = total_w.ceil() as u32;
        let h = height as u32;

        self.window.resize(w, h);
        let buf_size = (w * h * 4) as usize;
        {
            let buf = self.window.buffer_mut();
            buf[..buf_size].fill(0);
            let radius = (h as f32 * 0.30) as u32;
            fill_rounded(buf, w, h, 0, 0, w, h, self.bg, radius);
            // 细边框（与背景同弧度），增强浅色背景下的轮廓（对齐设计稿胶囊外框）。
            crate::view::fill_ring(
                buf,
                w,
                h,
                0.0,
                0.0,
                w as f32,
                h as f32,
                self.sep,
                radius as f32,
                (1.0 * s).max(1.0),
            );
            // 拖动柄点阵
            draw_grip(buf, w, h, grip_w as u32, self.grip, s);
        }

        // 逐格绘制 + 记录命中矩形
        let mut x = grip_w;
        let font_h = self.renderer.measure_text("中").height;
        let mut hits: Vec<(ToolbarAction, Rect)> = Vec::with_capacity(cells.len());
        for (i, c) in cells.iter().enumerate() {
            let cw = cell_widths[i];
            hits.push((
                c.action,
                Rect {
                    x,
                    y: 0.0,
                    w: cw,
                    h: h as f32,
                },
            ));
            // 分隔线：仅「拖动柄之后」(首格前) 与「设置图标之前」绘制（对齐设计稿，状态格之间不画）。
            let is_settings = matches!(c.action, ToolbarAction::OpenSettings);
            if i == 0 || is_settings {
                draw_vsep(self.window.buffer_mut(), w, h, x as u32, self.sep, s);
            }
            // 高亮底（激活态：中文模式格 / 简繁启用）+ 悬停底（鼠标移入，非激活格才画，避免叠加）。
            let cell_bg = if c.highlight {
                Some(self.hl_bg)
            } else if (i as i32) == hover_idx {
                Some(self.hover_bg)
            } else {
                None
            };
            if let Some(bgc) = cell_bg {
                let inset = (4.0 * s) as u32;
                let hx = x as u32 + inset / 2;
                let hy = inset;
                let hw = (cw as u32).saturating_sub(inset);
                let hh = h.saturating_sub(inset * 2);
                let hr = (hh as f32 * 0.3) as u32;
                fill_rounded(self.window.buffer_mut(), w, h, hx, hy, hw, hh, bgc, hr);
            }
            if is_settings {
                // 设置：矢量齿轮，单元格内精确居中（不依赖字体度量 → 与文字格完全对齐）。
                // 中心孔用工具栏底色（不透明）→ 视觉镂空。
                let gcx = x + cw * 0.5;
                let gcy = h as f32 * 0.5;
                let gear_r = (font_h * 0.42).max(5.0);
                let hole = [self.bg[0], self.bg[1], self.bg[2], 255];
                crate::view::fill_gear(
                    self.window.buffer_mut(),
                    w,
                    h,
                    gcx,
                    gcy,
                    gear_r,
                    self.settings_icon,
                    hole,
                );
            } else if matches!(c.action, ToolbarAction::TogglePunct | ToolbarAction::ToggleWidth) {
                // 标点 / 全半角：按状态渲染内联 SVG 图标，主题色 tint，居中于方格。
                let svg = match (c.action, state.chinese_punct, state.full_width) {
                    (ToolbarAction::TogglePunct, true, _) => PUNCT_FULL_SVG,
                    (ToolbarAction::TogglePunct, false, _) => PUNCT_HALF_SVG,
                    (ToolbarAction::ToggleWidth, _, true) => WIDTH_FULL_SVG,
                    _ => WIDTH_HALF_SVG,
                };
                let size = (h as f32).min(cw);
                let dx = x + (cw - size) * 0.5;
                let dy = (h as f32 - size) * 0.5;
                crate::view::draw_svg_icon(self.window.buffer_mut(), w, h, svg, dx, dy, size, self.fg);
            } else {
                // 居中文字
                let m = self.renderer.measure_text(&c.text);
                let tx = x + (cw - m.width) * 0.5;
                let ty = (h as f32 - font_h) * 0.5;
                let fg = if c.highlight {
                    self.hl_fg
                } else if c.dim {
                    dim_color(self.fg)
                } else {
                    self.fg
                };
                let _ = self.renderer.draw_text(
                    self.window.buffer_mut(),
                    w,
                    h,
                    tx.max(x),
                    ty.max(0.0),
                    &c.text,
                    fg,
                );
            }
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
        self.rendered_hover = hover_idx;
    }

    /// UI 循环每轮调用：消费鼠标处理器的悬停脏标记（由 WM_MOUSEMOVE/WM_MOUSELEAVE 事件置位），
    /// 仅在悬停格变化时本地重绘（无需协调器往返、不轮询光标）。与菜单 dirty→tick 重绘模式一致。
    pub fn tick(&mut self) {
        if !self.visible {
            return;
        }
        let (dirty, hov) = {
            let m = self.mouse.borrow();
            (m.dirty, m.hover_idx)
        };
        if !dirty {
            return;
        }
        self.mouse.borrow_mut().dirty = false;
        if hov != self.rendered_hover {
            if let Some(state) = self.last_state.clone() {
                self.render(&state, hov);
            } else {
                self.rendered_hover = hov;
            }
        }
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
        self.rendered_hover = -1; // 重新显示时按光标位置重算悬停
    }

    /// 工作区右下角位置（避开任务栏），右/下各留 12px 边距
    #[cfg_attr(not(windows), allow(unused_variables))]
    fn corner_position(w: u32, h: u32) -> (i32, i32) {
        #[cfg(windows)]
        {
            use windows::Win32::Foundation::RECT;
            use windows::Win32::UI::WindowsAndMessaging::{
                SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
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
                if dpi > 0 { dpi as f32 / 96.0 } else { 1.0 }
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
    /// 当前悬停格下标（-1=无）；由 WM_MOUSEMOVE/WM_MOUSELEAVE 事件更新
    hover_idx: i32,
    /// 悬停态有变更、待 Toolbar::tick 重绘
    dirty: bool,
}

impl ToolbarMouse {
    fn cell_at(&self, x: f32, y: f32) -> Option<ToolbarAction> {
        self.hits
            .iter()
            .find(|(_, r)| r.contains(x, y))
            .map(|(a, _)| *a)
    }

    /// 命中格下标（-1=无）。用于悬停高亮。
    fn hover_at(&self, x: f32, y: f32) -> i32 {
        self.hits
            .iter()
            .position(|(_, r)| r.contains(x, y))
            .map(|i| i as i32)
            .unwrap_or(-1)
    }

    /// 注册一次性 WM_MOUSELEAVE 通知（光标移出窗口时收到），以便清除悬停。
    fn arm_leave(&self) {
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
                    if self.hover_idx != -1 {
                        self.hover_idx = -1; // 拖动中不显示悬停
                        self.dirty = true;
                    }
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
                } else {
                    // 非拖动：更新悬停格（变化置脏，由 Toolbar::tick 重绘）+ 注册移出通知。
                    let hov = self.hover_at(cx, cy);
                    if hov != self.hover_idx {
                        self.hover_idx = hov;
                        self.dirty = true;
                    }
                    self.arm_leave();
                }
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE => {
                // 光标移出工具栏 → 清除悬停高亮。
                if self.hover_idx != -1 {
                    self.hover_idx = -1;
                    self.dirty = true;
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
                    if matches!(action, ToolbarAction::OpenSettings) {
                        // 设置键 = 弹出功能主菜单（工具栏上方，避免遮挡）。
                        let (mx, my) = self.pos.unwrap_or((0, 0));
                        let _ = self.events.send(UiEvent::RequestMainMenu {
                            x: mx,
                            y: my,
                            above: true,
                        });
                    } else {
                        // 其它单元格：按下未拖动 → 抬起时触发切换
                        let _ = self.events.send(UiEvent::Toolbar(action));
                    }
                }
                Some(LRESULT(0))
            }
            WM_RBUTTONDOWN => {
                // 右键工具栏 → 功能主菜单，在工具栏上方弹出（避免遮挡工具栏）。
                let (mx, my) = self.pos.unwrap_or_else(|| {
                    let mut p = POINT::default();
                    unsafe {
                        let _ = GetCursorPos(&mut p);
                    }
                    (p.x, p.y)
                });
                let _ = self.events.send(UiEvent::RequestMainMenu {
                    x: mx,
                    y: my,
                    above: true,
                });
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

/// 在缓冲区子区域 (x,y,w,h) 内填充圆角矩形
/// 将 (x,y,w,h) 钳制到所在（或最近）显示器工作区内，保证完整可见。
/// 用于切换显示器 / 远程连接后旧坐标落到屏外时拉回。
// 非 Windows 下无显示器工作区查询，w/h 仅 Windows 分支使用。
#[cfg_attr(not(windows), allow(unused_variables))]
fn clamp_to_work_area(x: i32, y: i32, w: u32, h: u32) -> (i32, i32) {
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

/// 次要状态文字淡显：alpha 降至 ~65%（半角/未启用简繁等次要态比正常文字更弱）。
fn dim_color(c: [u8; 4]) -> [u8; 4] {
    [c[0], c[1], c[2], (c[3] as f32 * 0.65) as u8]
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
