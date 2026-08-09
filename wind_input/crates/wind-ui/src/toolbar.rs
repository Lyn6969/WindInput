//! 工具栏窗口：常驻状态指示器（中英 / 方案 / 标点 / 全半角）。
//!
//! 与 Go 版本 `wind_input/internal/ui/toolbar_window.go` 对齐（简化版）。
//! 横向圆角小条，每格一个状态；中文模式格高亮。固定显示于工作区右下角。
//! 点击切换暂未实现（后续 UI 统一优化阶段补齐拖动 + 命中），当前为展示用。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Sender;

use crate::auto_hide::{AutoHide, AutoHideAction};
use crate::manager::{MenuAnchor, ToolbarAction, UiEvent};
use crate::sys::{
    GetCursorPos, GetWindowRect, HWND, HWND_TOPMOST, IDC_ARROW, IDC_SIZEALL, LPARAM, LRESULT,
    LoadCursorW, POINT, RECT, ReleaseCapture, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetCapture,
    SetCursor, SetWindowPos, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSELEAVE, WM_MOUSEMOVE,
    WM_RBUTTONDOWN, WM_SETCURSOR, WPARAM, clamp_to_work_area,
};
use crate::text::dwrite::TextRenderer;
use crate::view::Rect;
use crate::window::{LayeredWindow, WindowMouse};
use wind_theme::schema::Dim;

/// 工具栏状态（由协调器推送）
#[derive(Debug, Clone)]
pub struct ToolbarState {
    pub chinese_mode: bool,
    /// 有效显示标签：中文模式取方案 icon_label（如 "拼"/"五"），无则 "中"；
    /// 英文小写为 "英"，大写锁定为 "A"（由协调器预计算后填入）。
    pub icon_label: String,
    pub caps_lock: bool,
    pub full_width: bool,
    pub chinese_punct: bool,
    /// 简繁转换当前是否启用（格内显示 "繁" 并高亮）
    pub s2t_enabled: bool,
    /// 是否显示简繁格（默认 false；用户开启简繁功能后显示）
    pub s2t_shown: bool,
    /// 密码框强制英文生效中：仅影响**呈现**（模式格显 "英" 且不高亮）。
    ///
    /// 独立于 `icon_label` 而非直接改写它：后者是「当前方案标签」的单一语义，且会经
    /// StatusUpdate 下发写入 TSF 的 `_inputTypeLabel`（持久值）。把这种随焦点来去的
    /// 临时态烧进标签，离开密码框时就得指望下一次状态推送把它改回来，漏一次图标即
    /// 长期卡在 "英"。
    pub password_suppress: bool,
}

impl Default for ToolbarState {
    fn default() -> Self {
        Self {
            chinese_mode: true,
            icon_label: "中".to_string(),
            caps_lock: false,
            full_width: false,
            chinese_punct: true,
            s2t_enabled: false,
            s2t_shown: false,
            password_suppress: false,
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
/// 设置齿轮图标。
const SETTING_SVG: &str = include_str!("../res/icons/setting.svg");

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
    /// 自动隐藏状态机（默认关闭；SetToolbarAutoHide 命令配置）
    auto_hide: AutoHide,
    // 工具栏几何（主题 [toolbar] 描述；None→render 内置默认，见常量 HEIGHT/GRIP_W 等）。
    tb_height: Option<Dim>,
    tb_grip_width: Option<Dim>,
    tb_button_width: Option<Dim>,
    tb_button_padding: Option<Dim>,
    tb_button_radius: Option<Dim>,
    /// 整条外框圆角 / 线宽（[toolbar] border.radius / .width）。None→内置派生值。
    tb_border_radius: Option<Dim>,
    tb_border_width: Option<Dim>,
    /// 纵向排列（ui.toolbar.vertical，非主题——见 `bar_layout`）。
    vertical: bool,
    /// 待落的「某显示器右下角」请求：`(工作区右边界, 下边界)`，由 `render` 消费。
    ///
    /// 存边界而不是直接算坐标，是因为落点要减去工具栏自身尺寸，而**尺寸在 `render` 之前
    /// 不可信**——窗口以 `create(160, 40)` 的占位尺寸起步，`set_vertical` 在隐藏期间又
    /// 不重排（不出图），于是首次渲染前 `window.size()` 既不是横条真值也不是纵条真值。
    pending_corner: Option<(i32, i32)>,
}

/// 整条工具栏的几何：窗口尺寸 + 每格矩形（设备像素，相对窗口左上角）。
struct BarLayout {
    w: f32,
    h: f32,
    /// 拖动柄占据的区域（横条=左端竖条，纵条=顶端横条）。
    grip: Rect,
    cells: Vec<Rect>,
}

/// 按朝向铺开整条工具栏。**纵向恒为横向的转置**：`thickness`（主题 `[toolbar] height`）
/// 在横条里是条高、在纵条里是条宽；`cell`（`button_width`）在横条里是格宽、纵条里是格高。
/// 于是同一套主题几何在两个朝向下都成立，不必为纵向另配一套尺寸。
///
/// 抽成纯函数是为了可单测：`render` 的其余部分要拿 DirectWrite 测文字、要提交 Layered
/// Window，在非 Windows 上是 mock/空实现，覆盖不到。
fn bar_layout(vertical: bool, thickness: f32, grip_len: f32, cell: f32, n: usize) -> BarLayout {
    let long = grip_len + cell * n as f32;
    let (w, h) = if vertical {
        (thickness, long)
    } else {
        (long, thickness)
    };
    let grip = if vertical {
        Rect {
            x: 0.0,
            y: 0.0,
            w: thickness,
            h: grip_len,
        }
    } else {
        Rect {
            x: 0.0,
            y: 0.0,
            w: grip_len,
            h: thickness,
        }
    };
    let cells = (0..n)
        .map(|i| {
            let off = grip_len + cell * i as f32;
            if vertical {
                Rect {
                    x: 0.0,
                    y: off,
                    w: thickness,
                    h: cell,
                }
            } else {
                Rect {
                    x: off,
                    y: 0.0,
                    w: cell,
                    h: thickness,
                }
            }
        })
        .collect();
    BarLayout { w, h, grip, cells }
}

impl Toolbar {
    // 几何默认值（逻辑像素，随 DPI 缩放）。主题 [toolbar] 未描述时的兜底；
    // 与 _base/theme.toml [toolbar] 保持一致，改这里也应同步 _base（反之亦然）。
    const HEIGHT: f32 = 30.0;
    const GRIP_W: f32 = 12.0;
    const BUTTON_W: f32 = 30.0; // 每格（按钮槽）宽度，字面值；配 button_width 覆盖
    const BUTTON_PAD: f32 = 4.0; // 激活/悬停格高亮内缩；配 button_padding 覆盖
    const FONT_PX: f32 = 15.0;

    // 默认浅色配色（主题加载后由 set_theme 覆盖，以下为无主题时的兜底值）
    const BG: [u8; 4] = [255, 255, 255, 245]; // 白色半透明底
    const FG: [u8; 4] = [72, 72, 78, 255]; // 正常文字深灰
    const HL_BG: [u8; 4] = [66, 133, 244, 255]; // 高亮蓝（中文模式 / 简繁启用）
    const HL_FG: [u8; 4] = [255, 255, 255, 255];
    const SEP: [u8; 4] = [214, 214, 220, 255]; // 浅灰分隔线
    const GRIP: [u8; 4] = [186, 186, 194, 255]; // 拖动点
    const SETTINGS_ICON: [u8; 4] = [140, 140, 148, 255]; // 设置图标（比普通文字更淡）
    const HOVER_BG: [u8; 4] = [0, 0, 0, 13]; // 鼠标悬停高亮（极淡，~5% 黑）

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
            cursor_inside: false,
            size: (0, 0),
            vertical: false,
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
            auto_hide: AutoHide::new(),
            tb_height: None,
            tb_grip_width: None,
            tb_button_width: None,
            tb_button_padding: None,
            tb_button_radius: None,
            tb_border_radius: None,
            tb_border_width: None,
            vertical: false,
            pending_corner: None,
        })
    }

    /// 应用主题（工具栏各色，跟随语义）。
    pub fn set_theme(&mut self, theme: &wind_theme::Resolved) {
        // 背景色/边框色：[toolbar] 节点值优先（resolve 已合成 palette 默认），
        // 未配才落回 token —— 与其它窗口一致。
        self.bg = theme.color("toolbar_background", self.bg);
        self.fg = theme.color("toolbar_full_width_off_text", self.fg);
        self.hl_bg = theme.color("toolbar_mode_chinese_bg", self.hl_bg);
        self.hl_fg = theme.color("toolbar_mode_text", self.hl_fg);
        self.sep = theme.color("toolbar_border", self.sep);
        self.grip = theme.color("toolbar_grip", self.grip);
        self.settings_icon = theme.color("toolbar_settings_icon", self.settings_icon);
        self.hover_bg = theme.color("toolbar_hover", self.hover_bg);
        // 几何：从解析后的 [toolbar] 读取（None→render 用内置默认，行为不变）。
        let v = &theme.views;
        self.tb_height = v.toolbar_height;
        self.tb_grip_width = v.toolbar_grip_width;
        self.tb_button_width = v.toolbar_button_width;
        self.tb_button_padding = v.toolbar_button_padding;
        self.tb_button_radius = v.toolbar_button_radius;
        self.tb_border_radius = v.toolbar_border_radius;
        self.tb_border_width = v.toolbar_border_width;
        // [toolbar] 节点色覆盖上面的 token 兜底。
        if let Some(c) = v.toolbar_bg_color {
            self.bg = c;
        }
        if let Some(c) = v.toolbar_border_color {
            self.sep = c;
        }
    }

    /// 配置纵向排列（`ui.toolbar.vertical`，经 SetToolbarVertical 下发）。
    ///
    /// 换向会改窗口尺寸，故可见时立即用缓存状态重绘——否则要等下一次状态推送（切中英等）
    /// 才换向，设置页里改完看着像没生效。
    ///
    /// ⚠️ 重绘必须受 `visible` 门控：`repaint`→`render` 末尾无条件 `show`，对隐藏中的
    /// 工具栏调用会把它显形，绕过 `toolbar_gate` 的显示迟滞（同 `SetTheme` 分支的约束）。
    /// 隐藏期间换向不必出图——朝向已存好，而所有重新显示的路径都经 `update`→`render`
    /// 重算尺寸，不会留下旧朝向的残帧。
    pub fn set_vertical(&mut self, vertical: bool) {
        if self.vertical == vertical {
            return;
        }
        self.vertical = vertical;
        if self.visible {
            self.repaint();
        }
    }

    /// 配置自动隐藏（启动/配置重载时经 SetToolbarAutoHide 下发）。
    /// 淡出中关闭开关 → 恢复不透明；开启且当前可见 → 立即起表。
    pub fn set_auto_hide(&mut self, enabled: bool, delay_ms: u64) {
        if self.auto_hide.configure(enabled, delay_ms) {
            if let Err(e) = self.window.update_with_alpha(255) {
                tracing::warn!("Toolbar restore alpha: {}", e);
            }
        }
        if enabled && self.visible {
            // 淡出中重新配置：先恢复不透明再重新计时（configure(true) 不返回 was_fading）。
            if let Err(e) = self.window.update_with_alpha(255) {
                tracing::warn!("Toolbar restore alpha: {}", e);
            }
            self.auto_hide.on_shown(std::time::Instant::now());
        }
    }

    /// 设置工具栏位置（启动恢复持久化位置 / 运行期跟随焦点换屏）。
    ///
    /// **隐藏期间不钳制**，原样存下交给 `render`：钳制要拿工具栏尺寸比对工作区边界，而
    /// 首次渲染前 `window.size()` 还是 `create` 时的占位值 160×40。启动序列恰好命中这一点
    /// ——位置在 `init_toolbar_pos` 下发，朝向要到其后的 `apply_ui_config` 才下发，且
    /// `set_vertical` 隐藏期间不重排，于是纵条会被按 160 宽钳制：贴右保存的坐标被判越界，
    /// 拉回 `工作区右边界 - 160`，重启后凭空左移 100+px（y 方向 40 高不越界，故只横向偏）。
    /// `render` 里那次钳制用的是刚 `resize` 出来的真实尺寸，是唯一可信的时机。
    ///
    /// 不做 alpha 恢复与计时重置：协调器的换屏下发**紧接着**就是 `UpdateToolbar`，
    /// 而 `render` 末尾以 alpha=255 提交并 `on_shown`，两件事都在那里发生。
    ///
    /// ⚠️ `window.show` 受 `visible` 门控：对隐藏中的工具栏只记坐标不显形，否则会绕过
    /// `toolbar_gate` 的显示迟滞（同 `set_vertical` 的约束）。
    pub fn set_pos(&mut self, x: i32, y: i32) {
        // 显式位置优先于待落的角落请求，否则 render 会拿 pending 覆盖掉这次设定。
        self.pending_corner = None;
        if self.visible {
            let (w, h) = self.window.size();
            let (cx, cy) = clamp_to_work_area(x, y, w, h);
            self.mouse.borrow_mut().pos = Some((cx, cy));
            self.window.show(cx, cy);
        } else {
            self.mouse.borrow_mut().pos = Some((x, y));
        }
    }

    /// 移到指定显示器工作区的右下角——焦点切到一块**从未拖过工具栏**的屏时用。
    ///
    /// 由协调器传工作区右/下边界、这边算落点，而不是协调器直接算坐标下发：右下角要减去
    /// 工具栏自身的 w/h，而尺寸只有 UI 侧知道（随主题/朝向/DPI 变）。留边同 `corner_position`。
    ///
    /// 隐藏期间同样把计算推迟到 `render`（理由见 `set_pos`）——此处若按占位尺寸算，
    /// 纵条会落在偏左偏下 100+px 的地方。
    pub fn set_corner(&mut self, work_right: i32, work_bottom: i32) {
        if self.visible {
            let (w, h) = self.window.size();
            let (x, y) = Self::corner_in_work_area(work_right, work_bottom, w, h);
            self.set_pos(x, y);
        } else {
            self.pending_corner = Some((work_right, work_bottom));
        }
    }

    /// 根据状态构建单元格序列。
    /// 布局：拖动条 | 中英状态（含方案名）| 符号 | 全半角 | [简繁] | 设置图标
    fn cells(state: &ToolbarState) -> Vec<Cell> {
        // 有效中文：中文模式且大写锁定未开（对齐 Go effectiveChinese = chineseMode && !capsLockOn）。
        // 密码框强制英文时同样不算「有效中文」——此刻键已全部透传给宿主，高亮着中文格
        // 会与实际行为相反。⚠ 这是纯呈现判断，输入闸在 coordinator 的 password_suppress
        // 分支，两者各管各的，勿把本行的结论回灌给任何状态。
        let effective_chinese = state.chinese_mode && !state.caps_lock && !state.password_suppress;
        // 显示标签由协调器预计算存入 icon_label；此处直接使用。
        // 密码框例外：覆盖为 "英"，与该状态下的实际输入行为一致（见 password_suppress 注释）。
        let mode_text: &str = if state.password_suppress {
            "英"
        } else {
            &state.icon_label
        };

        let mut cells = vec![
            Cell {
                text: mode_text.to_string(),
                highlight: effective_chinese,
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

    /// 用缓存状态原地重绘（主题切换后刷新外观，无需重新传状态）。
    pub fn repaint(&mut self) {
        if let Some(state) = self.last_state.clone() {
            let hover = self.rendered_hover;
            self.render(&state, hover);
        }
    }

    /// 实际渲染（hover_idx=当前悬停格下标，-1 无）。update 与 tick 均经此单点渲染。
    fn render(&mut self, state: &ToolbarState, hover_idx: i32) {
        self.ensure_scale();
        let s = self.scale;
        // Dim→设备像素（dp×scale）；None→def_logical×scale（同候选窗 dim 闭包）。
        let dim = |o: Option<Dim>, def_logical: f32| {
            o.map(|x| x.resolve(s, 0.0)).unwrap_or(def_logical * s)
        };
        // 纵向下这两个值转 90°：thickness 成条宽、grip_len 成顶端拖动区高度。
        let thickness = dim(self.tb_height, Self::HEIGHT).ceil();
        let grip_len = dim(self.tb_grip_width, Self::GRIP_W).ceil();

        let cells = Self::cells(state);
        // 英文模式下标点固定显示半角，无需看 chinese_punct。
        let effective_chinese = state.chinese_mode && !state.caps_lock;

        // 每格等长（默认 30dp≈方形）：标点/简繁等图标与文字均居中于等长格，
        // 状态切换不改变格尺寸，工具栏整体长度稳定不抖动。主题可配 button_width 覆盖。
        let cell_len = dim(self.tb_button_width, Self::BUTTON_W);
        let layout = bar_layout(self.vertical, thickness, grip_len, cell_len, cells.len());
        let w = layout.w.ceil() as u32;
        let h = layout.h.ceil() as u32;

        self.window.resize(w, h);
        let buf_size = (w * h * 4) as usize;
        {
            let buf = self.window.buffer_mut();
            buf[..buf_size].fill(0);
            // 整条圆角：主题 [toolbar] border.radius 优先，未配则 = 条高×0.30（原派生行为，
            // 胶囊外形）。配 0 即直角——硬边缘风格靠这条实现。
            let radius = self
                .tb_border_radius
                .map(|d| d.resolve(s, 0.0))
                .unwrap_or(h as f32 * 0.30) as u32;
            fill_rounded(buf, w, h, 0, 0, w, h, self.bg, radius);
            // 细边框（与背景同弧度），增强浅色背景下的轮廓（对齐设计稿胶囊外框）。
            // 线宽：主题 border.width 优先，未配落 1dp（原字面量）。
            let border_w = self
                .tb_border_width
                .map(|d| d.resolve(s, 0.0))
                .unwrap_or(1.0 * s)
                .max(1.0);
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
                border_w,
            );
            // 拖动柄点阵
            draw_grip(buf, w, h, &layout.grip, self.vertical, self.grip, s);
        }

        // 逐格绘制 + 记录命中矩形
        let font_h = self.renderer.measure_text("中").height;
        let mut hits: Vec<(ToolbarAction, Rect)> = Vec::with_capacity(cells.len());
        for (i, c) in cells.iter().enumerate() {
            let r = layout.cells[i];
            hits.push((c.action, r));
            // 分隔线：仅「拖动柄之后」(首格前) 与「设置图标之前」绘制（对齐设计稿，状态格之间不画）。
            let is_settings = matches!(c.action, ToolbarAction::OpenSettings);
            if i == 0 || is_settings {
                // 画在格的**起始边**上：横条取左缘 x、纵条取上缘 y。
                let pos = if self.vertical { r.y } else { r.x };
                draw_sep(
                    self.window.buffer_mut(),
                    w,
                    h,
                    pos as u32,
                    self.vertical,
                    self.sep,
                    s,
                );
            }
            // 激活格（中文模式）画主题色底 + 高亮文字；悬停格画极淡底。
            // hl_bg 成对配合 hl_fg（如 msime 白字蓝底），缺底色时白字在亮色工具栏上不可见。
            let cell_bg = if c.highlight {
                Some(self.hl_bg)
            } else if (i as i32) == hover_idx {
                Some(self.hover_bg)
            } else {
                None
            };
            if let Some(bgc) = cell_bg {
                let inset = dim(self.tb_button_padding, Self::BUTTON_PAD) as u32;
                // 长轴方向两端各缩 inset/2、厚度方向两端各缩 inset（横条既有比例，纵条转置
                // 施加）——高亮块因此在两个朝向下都是"沿条身更瘦"的胶囊，而非贴边方块。
                let (hx, hy, hw, hh) = if self.vertical {
                    (
                        r.x as u32 + inset,
                        r.y as u32 + inset / 2,
                        (r.w as u32).saturating_sub(inset * 2),
                        (r.h as u32).saturating_sub(inset),
                    )
                } else {
                    (
                        r.x as u32 + inset / 2,
                        r.y as u32 + inset,
                        (r.w as u32).saturating_sub(inset),
                        (r.h as u32).saturating_sub(inset * 2),
                    )
                };
                // 高亮格圆角：主题 button_radius 优先，否则 = 内**短边**×0.3。横条下短边
                // 恒是内高（厚度方向缩得更多），故与原「内高×0.3」等值，纵条则自动转置。
                let hr = self
                    .tb_button_radius
                    .map(|d| d.resolve(s, 0.0))
                    .unwrap_or(hw.min(hh) as f32 * 0.3) as u32;
                fill_rounded(self.window.buffer_mut(), w, h, hx, hy, hw, hh, bgc, hr);
            }
            if is_settings {
                let size = font_h * 0.80;
                let dx = r.x + (r.w - size) * 0.5;
                let dy = r.y + (r.h - size) * 0.5;
                crate::view::draw_svg_icon(
                    self.window.buffer_mut(),
                    w,
                    h,
                    SETTING_SVG,
                    dx,
                    dy,
                    size,
                    self.settings_icon,
                );
            } else if matches!(
                c.action,
                ToolbarAction::TogglePunct | ToolbarAction::ToggleWidth
            ) {
                // 标点 / 全半角：按状态渲染内联 SVG 图标，主题色 tint，居中于方格。
                // 英文模式下标点固定半角（不可切换），无论 chinese_punct 状态如何。
                let svg = match (
                    c.action,
                    effective_chinese && state.chinese_punct,
                    state.full_width,
                ) {
                    (ToolbarAction::TogglePunct, true, _) => PUNCT_FULL_SVG,
                    (ToolbarAction::TogglePunct, false, _) => PUNCT_HALF_SVG,
                    (ToolbarAction::ToggleWidth, _, true) => WIDTH_FULL_SVG,
                    _ => WIDTH_HALF_SVG,
                };
                let size = font_h * 0.80;
                let dx = r.x + (r.w - size) * 0.5;
                let dy = r.y + (r.h - size) * 0.5;
                crate::view::draw_svg_icon(
                    self.window.buffer_mut(),
                    w,
                    h,
                    svg,
                    dx,
                    dy,
                    size,
                    self.fg,
                );
            } else {
                // 居中文字
                let m = self.renderer.measure_text(&c.text);
                let tx = r.x + (r.w - m.width) * 0.5;
                let ty = r.y + (r.h - font_h) * 0.5;
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
                    tx.max(r.x),
                    ty.max(r.y),
                    &c.text,
                    fg,
                );
            }
        }
        if let Err(e) = self.window.update() {
            tracing::warn!("Toolbar update failed: {}", e);
        }

        // 位置：优先用持久化/拖动后的位置；首次落在工作区右下角（避开任务栏）。
        // 钳制到当前显示器工作区内——避免切换显示器/远程后旧坐标落在屏外不可见。
        //
        // 一切依赖尺寸的落点计算都收口在这里：上面刚按当前朝向/主题/DPI 排完版并 resize，
        // `w`/`h` 此刻才是真值。`set_pos`/`set_corner` 在隐藏期间只登记意图、不算坐标，
        // 就是为了不在 `window.size()` 仍是占位值 160×40 时下判断（见 `set_pos` 文档）。
        let (px, py) = {
            let mut m = self.mouse.borrow_mut();
            m.hits = hits; // 同步命中矩形给鼠标处理器
            // 菜单锚点要用的尺寸/朝向，与命中矩形同源同刻更新——分开更新迟早错位。
            m.size = (w, h);
            m.vertical = self.vertical;
            let raw = match self.pending_corner.take() {
                Some((work_right, work_bottom)) => {
                    Self::corner_in_work_area(work_right, work_bottom, w, h)
                }
                None => m.pos.unwrap_or_else(|| Self::corner_position(w, h)),
            };
            let clamped = clamp_to_work_area(raw.0, raw.1, w, h);
            m.pos = Some(clamped);
            clamped
        };
        self.window.show(px, py);
        self.visible = true;
        self.rendered_hover = hover_idx;
        // 任何显示/状态刷新（render 是所有显示路径的单点）都重置自动隐藏计时。
        // window.update() 以 alpha=255 提交，天然恢复不透明。
        self.auto_hide.on_shown(std::time::Instant::now());
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
        if dirty {
            self.mouse.borrow_mut().dirty = false;
            if hov != self.rendered_hover {
                if let Some(state) = self.last_state.clone() {
                    self.render(&state, hov);
                } else {
                    self.rendered_hover = hov;
                }
            }
        }
        // 自动隐藏推进。快速路径：未启用/无活动计时时 is_active()=false 直接跳过，
        // 不取 Instant::now()、无系统调用（开关关闭时零开销的硬约束）。
        if self.auto_hide.is_active() {
            let (inside, dragging) = {
                let m = self.mouse.borrow();
                (m.cursor_inside, m.dragging)
            };
            let now = std::time::Instant::now();
            match self.auto_hide.tick_at(now, inside, dragging) {
                AutoHideAction::None => {}
                AutoHideAction::Fade(a) => {
                    if let Err(e) = self.window.update_with_alpha(a) {
                        tracing::warn!("Toolbar fade: {}", e);
                    }
                }
                AutoHideAction::Restore => {
                    if let Err(e) = self.window.update_with_alpha(255) {
                        tracing::warn!("Toolbar fade restore: {}", e);
                    }
                }
                AutoHideAction::Hide => self.hide(),
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
            // 显示路径同 render()：重置自动隐藏计时（未启用时 on_shown 为 no-op）。
            self.auto_hide.on_shown(std::time::Instant::now());
        }
    }

    /// 当前是否可见（`show` 置 true，`hide` 置 false）。
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// 将当前渲染帧保存为 PNG 文件（截图用）。
    pub fn capture_to_file(&self, path: &std::path::Path) -> Result<(), String> {
        self.window.capture_to_file(path)
    }

    /// 返回工具栏窗口句柄（截图用）。
    #[cfg(windows)]
    pub fn hwnd(&self) -> windows::Win32::Foundation::HWND {
        self.window.hwnd()
    }

    pub fn hide(&mut self) {
        self.window.hide();
        self.visible = false;
        self.rendered_hover = -1; // 重新显示时按光标位置重算悬停
        self.auto_hide.on_hidden();
    }

    /// 给定工作区右/下边界，算工具栏右下角落点（右/下各留 12px 边距）。
    ///
    /// 纯几何、无系统调用，故可被任意显示器复用——`corner_position` 喂主屏，
    /// `set_corner` 喂焦点所在屏。`max(0)` 的下限只在单屏（工作区从 0 起）时有意义，
    /// 副屏的工作区左/上边界可为负，钳到 0 会把工具栏推回主屏；真正的越界回收交给
    /// `set_pos` 里的 `clamp_to_work_area`（它按落点解析显示器，不预设原点）。
    fn corner_in_work_area(work_right: i32, work_bottom: i32, w: u32, h: u32) -> (i32, i32) {
        const MARGIN: i32 = 12;
        (
            work_right - w as i32 - MARGIN,
            work_bottom - h as i32 - MARGIN,
        )
    }

    /// 主显示器工作区右下角位置（避开任务栏）——工具栏**首次**显示且无任何记忆位置时的落点。
    /// `SPI_GETWORKAREA` 取的恒是主屏；跟随焦点换屏走 `set_corner`，不经过这里。
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
                    let (x, y) = Self::corner_in_work_area(rect.right, rect.bottom, w, h);
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
    /// 光标是否在工具栏窗口内（WM_MOUSEMOVE 置 true / WM_MOUSELEAVE 置 false）；
    /// 自动隐藏顺延判据——不能用 hover_idx（拖动柄区为 -1 但光标仍在窗内）。
    cursor_inside: bool,
    /// 最近一次渲染的窗口尺寸与朝向，由 `render` 同步（与 `hits` 同一处）。
    /// 菜单锚点据此计算——比现取 `GetWindowRect` 准（渲染刚定的尺寸，无需等窗口生效）
    /// 且无系统调用。`render` 必先于任何鼠标事件发生，故不存在 (0,0) 被用到的时机。
    size: (u32, u32),
    vertical: bool,
}

impl ToolbarMouse {
    /// 工具栏当前占据的屏幕矩形 `(left, top, right, bottom)`。
    fn rect(&self) -> (i32, i32, i32, i32) {
        let (x, y) = self.pos.unwrap_or((0, 0));
        (x, y, x + self.size.0 as i32, y + self.size.1 as i32)
    }

    /// 主菜单锚点：横条向上弹（避免压住工具栏），纵条向侧面弹——竖条上仍向上弹会让
    /// 菜单飘到条顶之上老远，与点击位置差出整条的高度。
    fn menu_anchor(&self) -> MenuAnchor {
        let (l, t, r, b) = self.rect();
        if self.vertical {
            MenuAnchor::beside_rect(l, t, r, b)
        } else {
            MenuAnchor::above_rect(l, t, b)
        }
    }

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
                self.cursor_inside = true;
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
                self.cursor_inside = false;
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
                        // 设置键 = 弹出功能主菜单（贴着工具栏，避免遮挡它）。
                        let _ = self
                            .events
                            .send(UiEvent::RequestMainMenu(self.menu_anchor()));
                    } else {
                        // 其它单元格：按下未拖动 → 抬起时触发切换
                        let _ = self.events.send(UiEvent::Toolbar(action));
                    }
                }
                Some(LRESULT(0))
            }
            WM_RBUTTONDOWN => {
                // 右键工具栏 → 功能主菜单，贴着工具栏弹出（避免遮挡工具栏）。
                let _ = self
                    .events
                    .send(UiEvent::RequestMainMenu(self.menu_anchor()));
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

/// 格间分隔线：横条画竖线（`pos` 是 x），纵条画横线（`pos` 是 y）；线两端各内缩 6px。
fn draw_sep(
    buf: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    pos: u32,
    vertical: bool,
    color: [u8; 4],
    scale: f32,
) {
    let inset = (6.0 * scale) as u32;
    // across = 线要横跨的那条边（横条跨条高、纵条跨条宽）。
    let across = if vertical { buf_h } else { buf_w };
    let span = if vertical { buf_w } else { buf_h };
    let a0 = inset;
    let a1 = span.saturating_sub(inset);
    if pos >= across || a1 <= a0 {
        return;
    }
    let (x, y, w, h) = if vertical {
        (a0 as f32, pos as f32, (a1 - a0) as f32, 1.0)
    } else {
        (pos as f32, a0 as f32, 1.0, (a1 - a0) as f32)
    };
    // 1px 线 = 直角矩形（tiny-skia），与其它形状统一
    crate::view::fill_rounded(buf, buf_w, buf_h, x, y, w, h, color, 0.0);
}

/// 拖动柄点阵，居中于 `grip` 区。横条 2 列×3 行、纵条转置为 3 列×2 行——点阵的长边
/// 始终垂直于条身，看着才像「抓手」而不是顺着条身的一道装饰线。
fn draw_grip(
    buf: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    grip: &Rect,
    vertical: bool,
    color: [u8; 4],
    scale: f32,
) {
    let dot = (2.0 * scale).max(1.0);
    let gap = 4.0 * scale;
    let cx = grip.x + grip.w / 2.0;
    let cy = grip.y + grip.h / 2.0;
    let (cols, rows) = if vertical { (3, 2) } else { (2, 3) };
    let x0 = cx - (cols - 1) as f32 * gap / 2.0;
    let y0 = cy - (rows - 1) as f32 * gap / 2.0;
    for row in 0..rows {
        for col in 0..cols {
            let x = x0 + col as f32 * gap;
            let y = y0 + row as f32 * gap;
            fill_dot(buf, buf_w, buf_h, x, y, dot / 2.0, color);
        }
    }
}

fn fill_dot(buf: &mut [u8], buf_w: u32, buf_h: u32, cx: f32, cy: f32, r: f32, color: [u8; 4]) {
    // 抗锯齿圆点（tiny-skia），与其它形状统一
    crate::view::fill_circle(buf, buf_w, buf_h, cx, cy, r, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    // 典型值：主题 [toolbar] 默认（条厚 30dp、拖动柄 12dp、格 30dp）× scale 1.0，
    // 4 格（模式/标点/全半角/设置）。
    const THICK: f32 = 30.0;
    const GRIP: f32 = 12.0;
    const CELL: f32 = 30.0;
    const N: usize = 4;

    /// 横条：回归基线。重构成 `bar_layout` 之前，render 内联算的就是这组值——
    /// 这条测试的作用是「纵向功能没有偷偷改掉横向的既有排布」。
    #[test]
    fn horizontal_layout_matches_legacy_geometry() {
        let l = bar_layout(false, THICK, GRIP, CELL, N);
        assert_eq!((l.w, l.h), (12.0 + 30.0 * 4.0, 30.0));
        assert_eq!(
            (l.grip.x, l.grip.y, l.grip.w, l.grip.h),
            (0.0, 0.0, 12.0, 30.0)
        );
        // 格自拖动柄之后起，沿 x 依次排开，各格占满条厚。
        for (i, c) in l.cells.iter().enumerate() {
            assert_eq!(c.x, 12.0 + 30.0 * i as f32, "第 {i} 格 x");
            assert_eq!((c.y, c.w, c.h), (0.0, 30.0, 30.0), "第 {i} 格");
        }
    }

    /// 右下角落点：从工作区右/下边界各退去工具栏尺寸再留 12px 边距。
    #[test]
    fn corner_backs_off_from_work_area_edges() {
        // 1920×1080 主屏，任务栏 40px：工作区右下 (1920, 1040)；工具栏 132×30。
        let (x, y) = Toolbar::corner_in_work_area(1920, 1040, 132, 30);
        assert_eq!((x, y), (1920 - 132 - 12, 1040 - 30 - 12));
    }

    /// 副屏在主屏**左侧**时工作区坐标为负，落点必须跟着为负。
    ///
    /// 这正是不能在此处 `max(0)` 的理由：钳到 0 会把工具栏推回主屏，表现为「切到左边那块屏
    /// 工具栏没跟过去」。越界回收由 `set_pos` 里的 `clamp_to_work_area` 负责——它按落点
    /// 反查显示器，不预设桌面原点在 (0,0)。
    #[test]
    fn corner_allows_negative_coords_on_left_side_monitor() {
        // 左侧副屏：虚拟桌面 x ∈ [-1920, 0)，工作区右下 (0, 1080)。
        let (x, y) = Toolbar::corner_in_work_area(0, 1080, 132, 30);
        assert_eq!(x, -144, "落点应留在左侧副屏上（负坐标）");
        assert_eq!(y, 1038);
    }

    /// 落点是**尺寸的函数**——同一块屏上横条与纵条的右下角必然落在不同位置。
    ///
    /// 这条测试把「算落点时尺寸必须已是真值」钉死。窗口以 `create(160, 40)` 的占位尺寸
    /// 起步，`set_vertical` 在隐藏期间不重排，故首次 `render` 之前 `window.size()` 两种
    /// 朝向的真值都不是。启动序列恰好在那个窗口里恢复位置，用占位尺寸算/钳的结果就是
    /// 重启后凭空左移——量级见末尾断言。修法是 `set_pos`/`set_corner` 隐藏期间只登记
    /// 意图，落点与钳制统一由 `render` 用刚 `resize` 出的尺寸计算。
    #[test]
    fn corner_depends_on_bar_orientation() {
        // 1920×1080 屏、底部 40px 任务栏 → 工作区右下 (1920, 1040)。
        // 默认几何：横条 132×30，转置后纵条 30×132。
        let horizontal = Toolbar::corner_in_work_area(1920, 1040, 132, 30);
        let vertical = Toolbar::corner_in_work_area(1920, 1040, 30, 132);
        assert_eq!(vertical.0 - horizontal.0, 102, "纵条更窄，落点更靠右");
        assert_eq!(horizontal.1 - vertical.1, 102, "纵条更高，落点更靠上");

        // 占位尺寸算出的落点比纵条真值靠左 130px——「重启后位置左移」的正是这个量。
        // 若哪天改了 create 的占位尺寸，这条会红，提醒回来确认推迟计算仍然成立。
        let placeholder = Toolbar::corner_in_work_area(1920, 1040, 160, 40);
        assert_eq!(vertical.0 - placeholder.0, 130);
    }

    /// 纵条：整条与横条互为转置——宽高对调、格沿 y 排开、各格占满条宽。
    #[test]
    fn vertical_layout_is_transpose_of_horizontal() {
        let h = bar_layout(false, THICK, GRIP, CELL, N);
        let v = bar_layout(true, THICK, GRIP, CELL, N);
        assert_eq!((v.w, v.h), (h.h, h.w), "整条宽高对调");
        assert_eq!((v.grip.w, v.grip.h), (h.grip.h, h.grip.w), "拖动柄区对调");
        assert_eq!(v.cells.len(), h.cells.len());
        for (i, (cv, ch)) in v.cells.iter().zip(h.cells.iter()).enumerate() {
            assert_eq!((cv.x, cv.y), (ch.y, ch.x), "第 {i} 格坐标对调");
            assert_eq!((cv.w, cv.h), (ch.h, ch.w), "第 {i} 格尺寸对调");
        }
    }

    /// 主题几何在两个朝向下同源：条厚恒取 `height`、格长恒取 `button_width`。
    /// 若哪天有人为纵向另引一套尺寸，这条会红——那正是要挡的改动。
    #[test]
    fn vertical_reuses_same_theme_dimensions() {
        let v = bar_layout(true, THICK, GRIP, CELL, N);
        assert_eq!(v.w, THICK, "纵条宽 = 主题 height");
        for c in &v.cells {
            assert_eq!(c.h, CELL, "纵条每格高 = 主题 button_width");
            assert_eq!(c.w, THICK, "纵条每格宽 = 条宽");
        }
        assert_eq!(v.h, GRIP + CELL * N as f32, "纵条总高 = 拖动柄 + 各格");
    }

    /// 格数随简繁格增减（`cells()` 的既有行为），布局须跟着长短，不能越界。
    #[test]
    fn layout_tracks_cell_count() {
        let four = bar_layout(true, THICK, GRIP, CELL, 4);
        let five = bar_layout(true, THICK, GRIP, CELL, 5);
        assert_eq!(five.h - four.h, CELL, "多一格恰长一格");
        assert_eq!(five.cells.len(), 5);
        // 末格不得超出整条（渲染越界会被静默裁掉，看着像"最后一格没画出来"）。
        let last = five.cells.last().unwrap();
        assert!(
            last.y + last.h <= five.h,
            "末格越界：{} > {}",
            last.y + last.h,
            five.h
        );
    }

    /// 缩放只改绝对值、不改结构：dp→设备像素由调用方（render 的 dim 闭包）算好再传入。
    #[test]
    fn layout_scales_uniformly() {
        let one = bar_layout(true, THICK, GRIP, CELL, N);
        let two = bar_layout(true, THICK * 2.0, GRIP * 2.0, CELL * 2.0, N);
        assert_eq!((two.w, two.h), (one.w * 2.0, one.h * 2.0));
    }
}
