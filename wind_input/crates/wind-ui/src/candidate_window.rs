//! 候选窗口：View 盒模型布局 + DirectWrite 文本 + Win32 Layered Window
//!
//! 与 Go 版本 `wind_input/internal/ui/manager_candidate.go` + `viewbox_build.go` 对齐。
//! 用 `crate::view` 的盒模型构建候选树（预编辑行 + 候选行[序号|文本] + 翻页指示），
//! measure/arrange 算出尺寸与每候选的绝对矩形（供鼠标命中），再 paint 到 BGRA 缓冲区。

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Sender;

use crate::manager::{
    HOVER_PAGE_NEXT as TAG_PAGE_NEXT, HOVER_PAGE_PREV as TAG_PAGE_PREV, MenuAnchor, UiEvent,
};
use crate::sys::{
    GetCursorPos, GetWindowRect, HWND, HWND_TOPMOST, IDC_ARROW, IDC_SIZEALL, LPARAM, LRESULT,
    LoadCursorW, POINT, RECT, ReleaseCapture, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetCapture,
    SetCursor, SetWindowPos, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_RBUTTONDOWN, WM_SETCURSOR, WPARAM, clamp_content_to_monitor,
};
use crate::text::dwrite::TextRenderer;
use crate::view::{Align, Edges, Layout, LeftBar, Rect, View, ViewImage, ViewLayer};
use wind_theme::DEFAULT_ACCENT_BAR_HEIGHT_RATIO;

/// 换行可见符 U+21B5（`↵`）。取编辑器通用约定（VS Code 等显示换行即此符），
/// 而非 Control Pictures 区的 `␊`/`␤`——后者字形是小方框里塞 `LF` 字母，
/// 候选窗字号下几乎认不出，且部分中文字体不覆盖该区段。
const NEWLINE_GLYPH: char = '\u{21B5}';

/// 制表可见符 U+21E5（`⇥`）。与换行分用不同符号：两者在文本里的语义不同，
/// 混用一个符号等于告诉用户「这儿有个空白」却不说是哪种。
const TAB_GLYPH: char = '\u{21E5}';

/// 把候选文本里的换行/制表符替换为可见符号，**仅供显示**。
///
/// `Candidate::text` 本身不动——上屏走的仍是含真换行的原文。两者必须分开：
/// 显示要的是「看得见有个换行」，上屏要的是「真的换行」。
///
/// 不这么做的后果不是「看不见」而是「排版坏掉」：DirectWrite 的 `CreateTextLayout`
/// 把 `\n` 当硬换行，`measure_text_styled` 于是返回 N 倍行高，而 `view.rs` 的盒模型
/// 按内容高布局 → 该候选被撑成多行、整个候选窗变高且行高参差。
///
/// CRLF 只出一个符号（不是两个）——它在用户眼里就是一个换行。
fn visible_whitespace(s: &str) -> Cow<'_, str> {
    if !s.contains(['\n', '\r', '\t']) {
        return Cow::Borrowed(s); // 快路径：绝大多数候选无空白控制符，零分配
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\r' => {
                if it.peek() == Some(&'\n') {
                    it.next();
                }
                out.push(NEWLINE_GLYPH);
            }
            '\n' => out.push(NEWLINE_GLYPH),
            '\t' => out.push(TAB_GLYPH),
            _ => out.push(c),
        }
    }
    Cow::Owned(out)
}
use crate::window::{LayeredWindow, WindowMouse};
use std::time::{Duration, Instant};

/// macOS host-render：`render_frame()` 产出的离屏帧（forwarder 写 SHM + push 用）。
#[cfg(target_os = "macos")]
pub struct RenderedFrame {
    /// 图像屏幕左上角 X（已含软影 margin 回移）
    pub screen_x: i32,
    /// 图像屏幕左上角 Y
    pub screen_y: i32,
    /// 图像宽（设备像素，已乘 scale）
    pub width: u32,
    /// 图像高（设备像素）
    pub height: u32,
    /// 渲染缩放（Retina=2）；.app 按 logical=像素/scale 显示，根治模糊
    pub scale: f32,
    /// 是否含软件高斯阴影（影响 .app 落位/裁切）
    pub software_shadow: bool,
    /// `screen_x/screen_y` 是否为用户固定位置的绝对坐标（而非按光标推算的落点）。
    /// 置位时 `.app` 只做边界钳制，不再套用「下方放不下翻到光标上方」的兜底。
    pub absolute_pos: bool,
    /// 候选命中矩形（窗口缓冲坐标，内容起点 (ml,mt)）：(index, Rect)
    pub hit_rects: Vec<(i32, crate::view::Rect)>,
    /// 预乘 BGRA 像素缓冲（width×height×4）
    pub buf: Vec<u8>,
}

/// Windows host-render：`render_frame()` 产出的离屏帧（写 SHM + Event 触发用）。
#[cfg(windows)]
pub struct RenderedFrame {
    /// 图像屏幕左上角 X（已含软影 margin 回移）
    pub screen_x: i32,
    /// 图像屏幕左上角 Y
    pub screen_y: i32,
    /// 图像宽（设备像素）
    pub width: u32,
    /// 图像高（设备像素）
    pub height: u32,
    /// 渲染缩放（DPI/96.0）
    pub scale: f32,
    /// 是否含软件高斯阴影
    pub software_shadow: bool,
    /// 候选命中矩形（窗口缓冲坐标，内容起点 (ml,mt)）：(index, Rect)
    pub hit_rects: Vec<(i32, crate::view::Rect)>,
    /// 预乘 BGRA 像素缓冲（width×height×4）
    pub buf: Vec<u8>,
}

/// 候选词数据
#[derive(Debug, Clone)]
pub struct CandidateItem {
    pub text: String,
    pub code: String,
    /// 序号标签（如 "1" / "a"）；空则按位置自动用数字编号
    pub label: String,
    /// 悬停反查提示（逐字编码/拼音，多行）；空则用 code 兜底
    pub tooltip: String,
    /// 候选注释（编码后缀/短语提示等），非空时在候选词右侧以注释样式内联显示；空则不显示
    pub comment: String,
    /// 为 true 时完全不渲染序号节点（用于非候选的提示行，如快捷加词预览），
    /// 避免默认主题下出现空的序号圆圈。
    pub no_index: bool,
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    config: CandidateWindowConfig,
    candidates: Vec<CandidateItem>,
    preedit: String,
    /// 编码区插入符位置：`preedit` 内字节偏移（恒在字符边界）。== len 表示光标在末尾。
    preedit_caret: usize,
    /// 模式指示文本（拼/双/快/英/符 或全称）；空=不渲染。
    mode_label: String,
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
    /// 上次内容锚点屏幕坐标 (px, py)：每帧 place_window 算出新位置后与之比较，微移(<4px*scale)则
    /// 保持原位以抑制宿主 caret 抖动（位置保护）。隐藏时清空，下次组合重新落位。
    last_content_pos: Option<(i32, i32)>,
    text_renderer: TextRenderer,
    /// arrange 后收集的候选命中矩形：(候选页内下标, 矩形)，供鼠标层使用
    hit_rects: Vec<(i32, Rect)>,
    /// 鼠标处理器（与 window 共享，wnd_proc 经注册表回调）
    mouse: Rc<RefCell<CandidateMouse>>,
    /// 悬停编码反查气泡
    tooltip: Option<crate::tooltip::Tooltip>,
    /// 已解析主题（RVNode 树 + palette）；默认兜底（空 palette + 渲染器内置色）
    theme: wind_theme::Resolved,
    /// DPI 缩放（主题几何为逻辑像素，渲染时乘此）
    scale: f32,
    /// 竖排布局（候选纵向堆叠）；默认横排。来自 ui.candidate.layout。
    vertical: bool,
    /// 预编辑嵌入模式（编码嵌入候选行首，不显示独立 preedit 条）。
    /// 来自 ui.candidate.preedit_display == "candidate_inline"。
    preedit_embedded: bool,
    /// 候选字号覆盖（>0 时取代主题 behavior.font_size）。来自 ui.candidate.font_size。
    font_size_override: f32,
    /// 候选窗在光标上方时反转候选顺序（仅竖排生效，见 [`CandidateWindow::above_layout`]）。
    /// 来自 ui.candidate.flip_when_above。
    flip_when_above: bool,
    /// 当前锚点是否落在光标上方（定位时计算，随锚点锁定保持）。供 flip 判定。
    placed_above: bool,
    /// 反向事件通道（与 `mouse` / `tooltip` 同源）。窗口自身也要上报状态，见 [`CandidateWindow::report_flip_state`]。
    events: Sender<UiEvent>,
    /// 上一次上报给协调器的「候选是否反转」，用于只在变化时发事件（渲染每帧都会走判定）。
    reported_flip: bool,
    /// 翻页栏显示覆盖（""跟随主题/"hide"/"auto"/"always"）。来自 ui.candidate.pager_bar_display。
    pager_display: String,
    /// 页码文字显示覆盖（""跟随主题/"show"/"hide"）。来自 ui.candidate.page_number_display。
    page_number_display: String,
    /// 候选窗在光标上方时交换编码栏与候选栏位置（编码区整体沉底贴光标）。
    /// 与 flip_when_above 正交：可单独或叠加使用。来自 ui.candidate.swap_preedit_when_above。
    swap_preedit_when_above: bool,
    /// 翻页栏并入编码栏行、右对齐显示（竖排省一行）。仅"非嵌入编码"（有独立编码栏）时生效。
    /// 来自 ui.candidate.pager_in_preedit。
    pager_in_preedit: bool,
    /// 固定位置模式的**内容左上**屏幕坐标；`None`=跟随光标（默认）。
    /// 来自 ui.candidate.position_mode + custom_x/custom_y，每次 UpdateCandidates 同步。
    /// `Some((0, 0))` 是"已开启固定但尚未设定位置"，定位时落到屏幕默认锚点。
    fixed_pos: Option<(i32, i32)>,
}

impl CandidateWindow {
    pub fn new(config: CandidateWindowConfig, events: Sender<UiEvent>) -> Result<Self, String> {
        let window = LayeredWindow::create(None, 400, 200, "WindInputCandidate")?;
        let text_renderer = TextRenderer::new("Microsoft YaHei UI", config.font_size)?;
        let tooltip_events = events.clone();
        let self_events = events.clone();
        let mouse = Rc::new(RefCell::new(CandidateMouse {
            hit_rects: Vec::new(),
            events,
            last_hover: -1,
            last_cursor: (i32::MIN, i32::MIN),
            engaged: false,
            engage_at: None,
            pending_raw: -1,
            engage_delay_ms: 60,
            hwnd: window.hwnd(),
            dragging: false,
            drag_anchor: (0, 0),
            drag_origin: (0, 0),
            drag_pin: None,
            margin: (0, 0, 0, 0),
        }));
        window.register_mouse(mouse.clone());
        Ok(Self {
            window,
            config,
            candidates: Vec::new(),
            preedit: String::new(),
            preedit_caret: 0,
            mode_label: String::new(),
            selected: 0,
            hover: -1,
            page: 1,
            total_pages: 1,
            visible: false,
            x: 0,
            y: 0,
            caret_height: 0,
            caret_valid: false,
            last_content_pos: None,
            text_renderer,
            hit_rects: Vec::new(),
            mouse,
            tooltip: crate::tooltip::Tooltip::new(tooltip_events).ok(),
            theme: wind_theme::Resolved::default(),
            scale: CandidateWindowConfig::get_dpi_scale(),
            vertical: false,
            preedit_embedded: false,
            font_size_override: 0.0,
            flip_when_above: false,
            placed_above: false,
            events: self_events,
            reported_flip: false,
            pager_display: String::new(),
            page_number_display: String::new(),
            swap_preedit_when_above: false,
            pager_in_preedit: false,
            fixed_pos: None,
        })
    }

    /// 应用主题（协调器下发）。同步更新悬停 tooltip 配色。
    /// 设置候选布局方向（true=竖排）。
    pub fn set_vertical(&mut self, vertical: bool) {
        self.vertical = vertical;
    }

    /// 设置预编辑嵌入模式（true=编码嵌入候选行首，不显示独立 preedit 条）。
    pub fn set_preedit_embedded(&mut self, embedded: bool) {
        self.preedit_embedded = embedded;
    }

    /// 设置候选字号覆盖（0=跟随主题）。来自 ui.candidate.font_size。
    pub fn set_font_size_override(&mut self, font_size: f32) {
        self.font_size_override = font_size.max(0.0);
    }

    /// 设置候选字体族（来自 ui.font.family；空=默认 Microsoft YaHei UI）。
    pub fn set_font_family(&mut self, family: &str) {
        let f = if family.trim().is_empty() {
            "Microsoft YaHei UI"
        } else {
            family
        };
        self.text_renderer.set_font_family(f);
    }

    /// 设置"上方时反转候选顺序"。来自 ui.candidate.flip_when_above。
    pub fn set_flip_when_above(&mut self, flip: bool) {
        self.flip_when_above = flip;
    }

    /// 设置"上方时交换编码栏与候选栏位置"。来自 ui.candidate.swap_preedit_when_above。
    pub fn set_swap_preedit_when_above(&mut self, swap: bool) {
        self.swap_preedit_when_above = swap;
    }

    /// 两个「上方专属」开关的最终生效判定（纯函数）：返回 `(反转候选, 交换编码/候选带)`。
    ///
    /// `flip_when_above` **只对竖排成立**：竖排下候选沿光标方向纵向排开，"反转"= 让候选 1
    /// 落到离光标最近的一侧，语义明确；横排下候选是左右并列，反转只是把 1..n 变成 n..1，
    /// 读序被打乱且与窗口在上在下毫无关系（同 `inline_preedit_bottom` 对横排的处置）。
    /// `swap_preedit_when_above` 交换的是编码带与候选带的**上下**位置，横竖排都成立，不受此限。
    ///
    /// 抽成纯函数是为了让「重建视图树的触发条件」与「build_tree 内的实际派生」共用同一份
    /// 判据：否则横排开了 flip 时外层仍会每帧重建一棵与原树完全相同的树，白付 build+layout。
    fn above_layout(above: bool, vertical: bool, flip: bool, swap: bool) -> (bool, bool) {
        (above && vertical && flip, above && swap)
    }

    /// 当前落位下是否有生效的「上方专属」布局 —— 有才需要按 `above=true` 重建视图树。
    fn above_layout_active(&self) -> bool {
        let (flip, swap) = Self::above_layout(
            self.placed_above,
            self.vertical,
            self.flip_when_above,
            self.swap_preedit_when_above,
        );
        flip || swap
    }

    /// 把「候选项当前是否被反转」上报给协调器（仅在取值变化时发事件）。
    ///
    /// ★ **判据的真相源只能在这一侧**：`placed_above` 要窗口尺寸 + 屏幕工作区才算得出，
    /// `vertical` 还会被模式级强制横/竖排改写 —— 协调器读配置推不出来。故复用
    /// [`CandidateWindow::above_layout`] 这个既有的单一真相源，把它第一个返回值送过去，
    /// 由协调器把 `highlight_up` / `highlight_down` 的走向翻转（视觉方向优先于候选序）。
    ///
    /// 调用点＝每处「`placed_above` 刚刚定完」之后，以及 [`CandidateWindow::hide`]
    /// 清除上翻粘滞时 —— 后者关掉的是「组合结束后状态残留为 true，下一轮首帧渲染前
    /// 用户就按了方向键」的窗口期。
    fn report_flip_state(&mut self) {
        let (flip, _) = Self::above_layout(
            self.placed_above,
            self.vertical,
            self.flip_when_above,
            self.swap_preedit_when_above,
        );
        if flip != self.reported_flip {
            self.reported_flip = flip;
            let _ = self.events.send(UiEvent::CandidateFlipped(flip));
        }
    }

    /// 设置"翻页栏并入编码栏行"。来自 ui.candidate.pager_in_preedit。
    pub fn set_pager_in_preedit(&mut self, on: bool) {
        self.pager_in_preedit = on;
    }

    /// 设置翻页栏显示覆盖。来自 ui.candidate.pager_bar_display。
    pub fn set_pager_display(&mut self, mode: String) {
        self.pager_display = mode;
    }

    /// 设置页码文字显示覆盖。来自 ui.candidate.page_number_display。
    pub fn set_page_number_display(&mut self, mode: String) {
        self.page_number_display = mode;
    }

    /// 是否显示翻页栏（覆盖优先；""跟随主题 behavior）。
    fn pager_visible(&self) -> bool {
        match self.pager_display.as_str() {
            "hide" => false,
            "always" => true,
            "auto" => self.total_pages > 1,
            _ => {
                // 跟随主题：hide_pager 隐藏；always_show_pager 总显示；否则 >1 页显示。
                let b = &self.theme.behavior;
                if b.hide_pager {
                    false
                } else if b.always_show_pager {
                    true
                } else {
                    self.total_pages > 1
                }
            }
        }
    }

    /// 翻页栏可见时是否显示页码文字（覆盖优先；""跟随主题 behavior.show_page_number）。
    fn page_number_visible(&self) -> bool {
        match self.page_number_display.as_str() {
            "show" => true,
            "hide" => false,
            _ => self.theme.behavior.show_page_number,
        }
    }

    /// 设置悬停提示激活延迟（毫秒）。来自 ui.tooltip.delay。
    pub fn set_tooltip_delay(&mut self, delay_ms: i32) {
        self.mouse.borrow_mut().engage_delay_ms = delay_ms.max(0) as u64;
    }

    /// 设置拆字字根字体（TTF 路径 + DWrite 家族名）。**两个渲染器都要收到**。
    ///
    /// 字根字符落在私用区（PUA），系统字体里没有对应字形，须靠渲染器把 PUA 连续段切到字根
    /// 字体集（`text::dwrite` 的 `pua_runs` + `SetFontCollection`，测量与绘制共用同一函数）。
    /// 那套机制本身与控件无关，但**它只对调用过本方法的那个 `TextRenderer` 实例生效**。
    ///
    /// 本方法此前名为 `set_tooltip_chaizi_font`，只转发给 `self.tooltip` —— 于是候选窗自己的
    /// `self.text_renderer` 从未拿到过字根字体。悬停提示里的字根正常、候选行里的字根显示成
    /// 豆腐块或空白，而配置/文件/家族名/日志四项全对，查不出所以然
    /// （同 `project_chaizi_font_pua_plane_gap` 的形态：**四项全对就该查消费端，不是资源本身**）。
    /// 此前无人发现是因为候选行从来没有渲染过字根——注释段接入拆字来源后才第一次有。
    pub fn set_chaizi_font(&mut self, path: &str, family: &str) {
        if let Err(e) = self.text_renderer.set_chaizi_font(path, family) {
            tracing::warn!("候选窗字根字体加载失败: {e}");
        }
        if let Some(t) = self.tooltip.as_mut() {
            t.set_chaizi_font(path, family);
        }
    }

    /// 悬停提示当前（或最近一次）显示的文本内容（右键菜单「复制内容」用）；无实例返回空串。
    pub fn tooltip_text(&self) -> &str {
        self.tooltip.as_ref().map(|t| t.text()).unwrap_or("")
    }

    /// 将悬停提示窗口当前渲染帧保存为 PNG 文件（截图用）。
    pub fn tooltip_capture_to_file(&self, path: &std::path::Path) -> Result<(), String> {
        match self.tooltip.as_ref() {
            Some(t) => t.capture_to_file(path),
            None => Err("tooltip 未初始化".to_string()),
        }
    }

    /// 将悬停 tooltip 当前渲染帧复制到剪贴板。
    pub fn tooltip_capture_to_clipboard(&self) -> Result<(), String> {
        match self.tooltip.as_ref() {
            Some(t) => t.capture_to_clipboard(),
            None => Err("tooltip 未初始化".to_string()),
        }
    }

    /// 悬停提示窗口当前是否可见。
    pub fn tooltip_is_visible(&self) -> bool {
        self.tooltip
            .as_ref()
            .map(|t| t.is_visible())
            .unwrap_or(false)
    }

    /// 设置悬停提示右键菜单打开状态（转发给 Tooltip，见其 set_menu_open 说明）。
    pub fn tooltip_set_menu_open(&mut self, open: bool) {
        if let Some(t) = self.tooltip.as_mut() {
            t.set_menu_open(open);
        }
    }

    pub fn set_theme(&mut self, theme: wind_theme::Resolved) {
        if let Some(tip) = self.tooltip.as_mut() {
            tip.set_theme(&theme);
        }
        self.theme = theme;
    }

    pub fn update(
        &mut self,
        preedit: &str,
        preedit_caret: usize,
        mode_label: &str,
        candidates: Vec<CandidateItem>,
        selected: usize,
        hover: i32,
        page: usize,
        total_pages: usize,
    ) {
        self.preedit = preedit.to_string();
        self.preedit_caret = Self::clamp_caret(preedit, preedit_caret);
        self.mode_label = mode_label.to_string();
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

    /// 设置固定位置模式：`Some((x, y))`=固定在该内容左上屏幕坐标，`None`=跟随光标。
    pub fn set_fixed_position(&mut self, pos: Option<(i32, i32)>) {
        self.fixed_pos = pos;
    }

    /// 当前候选窗**内容左上**屏幕坐标（窗口左上 + 阴影扩边）。
    /// 供「定位方式」切到 fixed 时把当前实际位置落盘成 custom_x/custom_y。
    pub fn content_origin(&self) -> (i32, i32) {
        let m = self.mouse.borrow();
        let w = m.window_origin().unwrap_or((0, 0));
        Self::window_to_content(w, m.margin.0, m.margin.1)
    }

    /// 候选页内命中矩形（绝对坐标，相对窗口左上角）
    pub fn hit_rects(&self) -> &[(i32, Rect)] {
        &self.hit_rects
    }

    /// Windows：show 直接复用 render_frame() 渲染结果，blit 到本地 LayeredWindow。
    /// 与 host-render 路径共用单一渲染逻辑，确保几何完全一致。
    #[cfg(windows)]
    pub fn show(&mut self) {
        match self.render_frame() {
            None => {
                self.hide();
            }
            Some(frame) => {
                self.window.resize(frame.width, frame.height);
                {
                    let buf = self.window.buffer_mut();
                    buf[..(frame.width * frame.height * 4) as usize].copy_from_slice(&frame.buf);
                }
                if let Err(e) = self.window.update() {
                    tracing::warn!("CandidateWindow update failed: {}", e);
                }
                // render_frame() 已设 visible=true；screen_x/y 为窗口左上（含阴影偏移）。
                self.window.show(frame.screen_x, frame.screen_y);
                self.update_tooltip(frame.screen_x, frame.screen_y);
            }
        }
    }

    #[cfg(not(windows))]
    pub fn show(&mut self) {
        // mode_label 非空表示已进入临时模式：即使暂无候选/preedit 也要弹窗显示模式标记。
        if self.candidates.is_empty() && self.preedit.is_empty() && self.mode_label.is_empty() {
            self.hide();
            return;
        }

        // DPI 动态化：按光标所在显示器实时取缩放，换显示器后自动按新 DPI 渲染。
        // 几何/字号全部由 self.scale 派生（build_tree 中现算），更新此值即生效。
        let new_scale = crate::dpi::scale_for_point(self.x, self.y);
        if (new_scale - self.scale).abs() > 0.01 {
            self.scale = new_scale;
            self.text_renderer
                .set_base_size((self.theme.behavior.font_size as f32) * new_scale);
        }

        // ── 渲染计时（定位长按翻页卡顿耗时段）──
        let t_start = Instant::now();

        // 构建并测量 View 树
        let mut root = self.build_tree(false);
        let t_build = t_start.elapsed();

        // 窗口投影：高斯软影四向扩边（与 Go shadowMargins 对齐），内容布局起点平移到 (ml, mt)，
        // 窗口显示位置再左上回移 (ml, mt) → 视觉锚点/命中坐标与无阴影时一致，阴影四面溢出。
        let shadow = self.shadow_params();
        let (ml, mt, mr, mb) = match &shadow {
            Some(s) => s.margins(),
            None => (0, 0, 0, 0),
        };

        let t_layout0 = Instant::now();
        root.layout(ml as f32, mt as f32, &self.text_renderer);
        let (w_f, h_f) = root.measured_size();
        let mut content_w = (w_f.ceil() as u32).max(40);
        // 竖排最大宽度（behavior.vertical_max_width 逻辑 px，>0 生效）：钳制窗口宽度上限。
        // 本 View 引擎不支持文本折行，超宽候选在窗口右缘裁切（draw_text 按缓冲宽度裁剪）。
        if self.vertical && !self.candidates.is_empty() {
            let vmax = self.theme.behavior.vertical_max_width;
            if vmax > 0 {
                let vmax_px = ((vmax as f32 * self.scale).ceil() as u32).max(40);
                content_w = content_w.min(vmax_px);
            }
        }
        let content_h = (h_f.ceil() as u32).max(24);
        let width = content_w + ml + mr;
        let height = content_h + mt + mb;
        let t_layout = t_layout0.elapsed();

        // 定位提前到 paint 前（供 flip_when_above 判定）。对齐 Go：每帧据当前光标 + 内容尺寸重算
        // 位置（place_window 负责工作区钳制、上方底边参考、上翻粘滞），再用 4px 阈值抑制微移——
        // 从而 (1) 输入缓冲变化致尺寸变化时按需重定位、不溢出屏幕；(2) 上方显示以底边贴光标为参考；
        // (3) 已显示后宿主 caret 微抖动(<4px)不跳，位置保护；显著变化(换行/reflow 修正)才跟随；
        // (4) 一旦上翻则粘滞上方。新组合首显已由协调器延迟到 reflow 后的权威坐标，故首帧即落在正确处。
        let offset = self.position_offset_px();
        let (px0, py0, above) = Self::place_window(
            self.x,
            self.y,
            self.caret_height,
            content_w,
            content_h,
            self.placed_above,
            offset,
        );
        self.placed_above = above;
        // 位置稳定性：窗口已显示且新位置与上次内容锚点微移(<4px*scale)→保持原位，吞掉 caret 微抖。
        let (px, py) = match self.last_content_pos {
            Some((lx, ly)) if self.visible => {
                let thr = (4.0 * self.scale).round().max(1.0) as i32;
                if (px0 - lx).abs() < thr && (py0 - ly).abs() < thr {
                    (lx, ly)
                } else {
                    (px0, py0)
                }
            }
            _ => (px0, py0),
        };
        self.last_content_pos = Some((px, py));
        // 上方显示且有开关真正生效 → 按上方布局重建候选树（项数/尺寸/位置不变，仅排列翻转）
        self.report_flip_state();
        if self.above_layout_active() {
            root = self.build_tree(true);
            root.layout(ml as f32, mt as f32, &self.text_renderer);
        }

        // 收集候选命中矩形并同步给鼠标处理器（须在 flip 重建后）
        self.hit_rects.clear();
        root.collect_hits(&mut self.hit_rects);
        {
            let mut m = self.mouse.borrow_mut();
            m.hit_rects = self.hit_rects.clone();
            m.last_hover = -1;
        }

        self.window.resize(width, height);

        // 透明清屏 + 绘制
        let t_paint0 = Instant::now();
        {
            let buf = self.window.buffer_mut();
            let buf_size = (width * height * 4) as usize;
            buf[..buf_size].fill(0);
            // 先画投影（在内容下方），再画内容覆盖其上。内容盒左上 = (ml, mt)。
            if let Some(s) = &shadow {
                let radius = self
                    .theme
                    .views
                    .window
                    .border_radius
                    .map(|d| d.resolve(self.scale, 0.0))
                    .unwrap_or(8.0 * self.scale);
                s.paint(
                    buf,
                    width,
                    height,
                    ml as f32,
                    mt as f32,
                    content_w as f32,
                    content_h as f32,
                    radius,
                );
            }
            root.paint(buf, width, height, &self.text_renderer);
        }
        let t_paint = t_paint0.elapsed();

        let t_blit0 = Instant::now();
        if let Err(e) = self.window.update() {
            tracing::warn!("CandidateWindow update failed: {}", e);
        }
        let t_blit = t_blit0.elapsed();

        tracing::debug!(
            "render[{}x{} n={}]: build={:?} layout={:?} paint={:?} blit={:?} | total={:?}",
            width,
            height,
            self.candidates.len(),
            t_build,
            t_layout,
            t_paint,
            t_blit,
            t_start.elapsed()
        );

        // 位置 (px, py) 已在 paint 前计算并锚定（组合期锁定后复用，避免漂移）。
        // 窗口实际左上 = 内容锚点 − 左/上 margin，使内容仍落在锚点处，阴影向四周溢出。
        self.window.show(px - ml as i32, py - mt as i32);
        self.visible = true;
        let t_tip0 = Instant::now();
        // 命中矩形是窗口缓冲坐标（内容起点在 (ml,mt)）；tooltip 定位须用窗口屏幕原点。
        self.update_tooltip(px - ml as i32, py - mt as i32);
        let t_tip = t_tip0.elapsed();
        if t_tip.as_micros() > 200 {
            tracing::debug!("render tooltip={:?}", t_tip);
        }
    }

    /// macOS host-render：把当前候选状态渲染到一块独立 BGRA 缓冲区并返回，
    /// 不依赖 Win32 Layered Window（forwarder 取此 buffer 写 POSIX SHM + push 给 .app）。
    /// 镜像 `show()` 的 build/layout/place/flip/paint 流程，但 paint 到自有 Vec。
    /// 返回 None 表示应隐藏候选窗（无候选 / 无 preedit / 无模式标记）。
    #[cfg(target_os = "macos")]
    pub fn render_frame(&mut self) -> Option<RenderedFrame> {
        if self.candidates.is_empty() && self.preedit.is_empty() && self.mode_label.is_empty() {
            return None;
        }

        // DPI：按光标所在屏取缩放（Retina=2），几何/字号由 self.scale 派生。
        // 固定位置模式按**固定点**取——固定位置可能落在与光标不同缩放的另一块屏上，
        // 按光标算会让窗口用错误的 DPI 渲染（字号忽大忽小）。与 Windows 分支同一判据。
        let (dpi_x, dpi_y) = match self.fixed_pos {
            Some(f) if f != (0, 0) => f,
            _ => (self.x, self.y),
        };
        let new_scale = crate::dpi::scale_for_point(dpi_x, dpi_y);
        if (new_scale - self.scale).abs() > 0.01 {
            self.scale = new_scale;
            self.text_renderer
                .set_base_size((self.theme.behavior.font_size as f32) * new_scale);
        }

        let mut root = self.build_tree(false);
        // macOS 改用系统原生 NSPanel 阴影（.app 端 panel.hasShadow + invalidateShadow，自动沿
        // 圆角内容形状投影），位图内【不画软件阴影】：margin 归零 → 定位精确贴 caret、无偏移，
        // 观感也更 native。software_shadow=false → Swift 侧启用系统窗口阴影。
        let shadow: Option<crate::view::SoftShadow> = None;
        let (ml, mt, mr, mb) = match &shadow {
            Some(s) => s.margins(),
            None => (0, 0, 0, 0),
        };
        root.layout(ml as f32, mt as f32, &self.text_renderer);
        let (w_f, h_f) = root.measured_size();
        let mut content_w = (w_f.ceil() as u32).max(40);
        if self.vertical && !self.candidates.is_empty() {
            let vmax = self.theme.behavior.vertical_max_width;
            if vmax > 0 {
                let vmax_px = ((vmax as f32 * self.scale).ceil() as u32).max(40);
                content_w = content_w.min(vmax_px);
            }
        }
        let content_h = (h_f.ceil() as u32).max(24);
        let width = content_w + ml + mr;
        let height = content_h + mt + mb;

        // ── 定位：fixed_pos（用户固定位置）> place_window（跟随光标）──
        //
        // Windows 那边还有第三级 `drag_pin`（本次组合内冻结拖动落位）。macOS 的拖动发生在
        // `.app` 侧的 NSPanel 上，服务进程这边的 mouse handler 是不产生事件的 mock，
        // `drag_pin` 恒为 None——会话内的落位冻结由 `.app` 自己记（见 CandidatePanel.dragPin）。
        let (px, py, absolute) = match self.fixed_pos {
            Some(f) => {
                // 固定位置下窗口不随光标上下移动，"上翻"随之失去意义：placed_above 必须归 false，
                // 否则 flip_when_above / swap_preedit_when_above 会让内容在一个不动的窗口里倒序。
                self.placed_above = false;
                self.last_content_pos = None;
                let (wx, wy) = Self::place_fixed(
                    f,
                    self.x,
                    self.y,
                    width,
                    height,
                    (ml as i32, mt as i32, mr as i32, mb as i32),
                );
                // place_fixed 返回**窗口**左上（含阴影扩边）；macOS 不画软阴影，扩边恒 0，
                // 故这里窗口左上就是内容左上，与下面跟随光标分支的 (px, py) 同义。
                (wx, wy, true)
            }
            None => {
                // place_window 约定 caret_y 为光标【底端】（下方锚点=底端+gap），但 macOS .app 的
                // caretRectToWire 发的是光标行【顶端】(top-left)，故这里把行高补上传光标底端 =
                // self.y + self.caret_height，候选窗才落在光标行下方、不遮挡输入。
                let offset = self.position_offset_px();
                let (px0, py0, above) = Self::place_window(
                    self.x,
                    self.y + self.caret_height,
                    self.caret_height,
                    content_w,
                    content_h,
                    self.placed_above,
                    offset,
                );
                self.placed_above = above;
                let (px, py) = match self.last_content_pos {
                    Some((lx, ly)) if self.visible => {
                        let thr = (4.0 * self.scale).round().max(1.0) as i32;
                        if (px0 - lx).abs() < thr && (py0 - ly).abs() < thr {
                            (lx, ly)
                        } else {
                            (px0, py0)
                        }
                    }
                    _ => (px0, py0),
                };
                self.last_content_pos = Some((px, py));
                (px, py, false)
            }
        };
        self.report_flip_state();
        if self.above_layout_active() {
            root = self.build_tree(true);
            root.layout(ml as f32, mt as f32, &self.text_renderer);
        }

        // 命中矩形（窗口缓冲坐标，内容起点 (ml,mt)），同步给鼠标处理器。
        self.hit_rects.clear();
        root.collect_hits(&mut self.hit_rects);
        {
            let mut m = self.mouse.borrow_mut();
            m.hit_rects = self.hit_rects.clone();
            m.last_hover = -1;
        }

        // 绘制到独立 BGRA 缓冲（透明清屏 → 软影 → 内容）。
        let mut buf = vec![0u8; (width * height * 4) as usize];
        if let Some(s) = &shadow {
            let radius = self
                .theme
                .views
                .window
                .border_radius
                .map(|d| d.resolve(self.scale, 0.0))
                .unwrap_or(8.0 * self.scale);
            s.paint(
                &mut buf,
                width,
                height,
                ml as f32,
                mt as f32,
                content_w as f32,
                content_h as f32,
                radius,
            );
        }
        root.paint(&mut buf, width, height, &self.text_renderer);

        self.visible = true;
        // 关键单位换算：ml/mt/blur 都是 device px（含 scale），而 screen_x/y 是 .app 使用的【逻辑点】
        // （图像按 scale 显示）。故补偿前必须 /scale 换回逻辑点，否则 Retina(×2) 下会多减一截 →
        // 候选窗偏左、偏上盖住 caret（正是截图现象）。no-shadow(margin=0)→0，无变化。
        let scale = self.scale.max(1.0);
        let ml_l = (ml as f32 / scale).round() as i32;
        let mt_l = (mt as f32 / scale).round() as i32;
        // 软阴影上边距是完整 3σ 高斯尾(多为透明)；内容紧贴 caret 时上方约一个 blur 高的【可见浓阴影】
        // 会盖住 caret。故内容较锚点再下移「可见浓阴影」高度(≈blur，换算逻辑点)，使浓阴影恰落 caret
        // 下方、透明尾不碍观感。above(上翻)不额外下移；固定位置分支 placed_above 恒为 false，
        // 但那条路上窗口本就不贴 caret，阴影补偿有没有都不影响观感。
        let clear_l = if self.placed_above {
            0
        } else {
            (shadow.as_ref().map(|s| s.blur).unwrap_or(0.0) / scale).round() as i32
        };
        Some(RenderedFrame {
            // 图像含四向 margin(device)，其屏幕左上(逻辑点) = 内容锚点 − 左/上 margin/scale
            //（纵向再 +可见浓阴影下移，让阴影落在 caret 下方）。
            screen_x: px - ml_l,
            screen_y: py - mt_l + clear_l,
            width,
            height,
            scale: self.scale,
            software_shadow: shadow.is_some(),
            absolute_pos: absolute,
            hit_rects: self.hit_rects.clone(),
            buf,
        })
    }

    /// Windows host-render：把当前候选状态渲染到一块独立 BGRA 缓冲区并返回，
    /// 供 host-render 管理器写 SHM + 触发 Event，DLL 侧 HostWindow 读取并显示。
    /// 镜像 `show()` 的 build/layout/place/collect_hits/paint 流程，但 paint 到自有 Vec，
    /// 不调 Win32 LayeredWindow API（写 SHM 路径不需要本地窗口）。
    /// 返回 None 表示应隐藏候选窗（无候选 / 无 preedit / 无模式标记）。
    #[cfg(windows)]
    pub fn render_frame(&mut self) -> Option<RenderedFrame> {
        if self.candidates.is_empty() && self.preedit.is_empty() && self.mode_label.is_empty() {
            return None;
        }

        // DPI 探测点：固定位置模式按**固定点**取缩放，而不是光标点——固定位置可能落在
        // 与光标不同缩放的另一块屏上，按光标算会让窗口用错误的 DPI 渲染（字号忽大忽小）。
        let (dpi_x, dpi_y) = match self.fixed_pos {
            Some(f) if f != (0, 0) => f,
            _ => (self.x, self.y),
        };
        let new_scale = crate::dpi::scale_for_point(dpi_x, dpi_y);
        if (new_scale - self.scale).abs() > 0.01 {
            self.scale = new_scale;
            self.text_renderer
                .set_base_size((self.theme.behavior.font_size as f32) * new_scale);
        }

        let mut root = self.build_tree(false);
        let shadow = self.shadow_params();
        let (ml, mt, mr, mb) = match &shadow {
            Some(s) => s.margins(),
            None => (0, 0, 0, 0),
        };
        root.layout(ml as f32, mt as f32, &self.text_renderer);
        let (w_f, h_f) = root.measured_size();
        let mut content_w = (w_f.ceil() as u32).max(40);
        if self.vertical && !self.candidates.is_empty() {
            let vmax = self.theme.behavior.vertical_max_width;
            if vmax > 0 {
                let vmax_px = ((vmax as f32 * self.scale).ceil() as u32).max(40);
                content_w = content_w.min(vmax_px);
            }
        }
        let content_h = (h_f.ceil() as u32).max(24);
        let width = content_w + ml + mr;
        let height = content_h + mt + mb;

        // ── 定位：三级优先级 drag_pin > fixed_pos > place_window(跟随光标) ──
        // 用户已手动拖动过本次组合的候选窗 → 冻结落位：位置固定、上/下方排列也不再翻转，
        // 否则窗口停在原处但内容突然倒序，视觉上会"自己变了个样"。
        let drag_pin = self.mouse.borrow().drag_pin;
        let mut screen_xy: Option<(i32, i32)> = match (drag_pin, self.fixed_pos) {
            // 拖动落定：placed_above 保持不变（同上，避免窗口不动而内容倒序）。
            (Some(p), _) => Some(p),
            // 固定位置模式：窗口不再随光标上下移动，"上翻"随之失去意义 —— placed_above
            // 必须归 false，否则 flip_when_above / swap_preedit_when_above 会让内容
            // 在一个位置固定的窗口里莫名倒序。
            (None, Some(f)) => {
                self.placed_above = false;
                self.last_content_pos = None;
                Some(Self::place_fixed(
                    f,
                    self.x,
                    self.y,
                    width,
                    height,
                    (ml as i32, mt as i32, mr as i32, mb as i32),
                ))
            }
            (None, None) => None,
        };
        if screen_xy.is_none() {
            // Windows 的 self.y 已是光标底端（与 show() 语义一致），直接传入。
            // 主题位置偏移只在这条「跟随光标」分支叠加——上面两个分支是用户显式定位。
            let offset = self.position_offset_px();
            let (px0, py0, above) = Self::place_window(
                self.x,
                self.y,
                self.caret_height,
                content_w,
                content_h,
                self.placed_above,
                offset,
            );
            self.placed_above = above;
            let (px, py) = match self.last_content_pos {
                Some((lx, ly)) if self.visible => {
                    let thr = (4.0 * self.scale).round().max(1.0) as i32;
                    if (px0 - lx).abs() < thr && (py0 - ly).abs() < thr {
                        (lx, ly)
                    } else {
                        (px0, py0)
                    }
                }
                _ => (px0, py0),
            };
            self.last_content_pos = Some((px, py));
            screen_xy = Some((px - ml as i32, py - mt as i32));
        }
        self.report_flip_state();
        if self.above_layout_active() {
            root = self.build_tree(true);
            root.layout(ml as f32, mt as f32, &self.text_renderer);
        }

        self.hit_rects.clear();
        root.collect_hits(&mut self.hit_rects);
        {
            let mut m = self.mouse.borrow_mut();
            m.hit_rects = self.hit_rects.clone();
            m.last_hover = -1;
        }

        let mut buf = vec![0u8; (width * height * 4) as usize];
        if let Some(s) = &shadow {
            let radius = self
                .theme
                .views
                .window
                .border_radius
                .map(|d| d.resolve(self.scale, 0.0))
                .unwrap_or(8.0 * self.scale);
            s.paint(
                &mut buf,
                width,
                height,
                ml as f32,
                mt as f32,
                content_w as f32,
                content_h as f32,
                radius,
            );
        }
        root.paint(&mut buf, width, height, &self.text_renderer);

        self.visible = true;
        // 上方三级定位分支穷尽，此处必为 Some；debug 下断言，release 兜底到光标处而非
        // (0,0)——真出现逻辑漏洞时窗口至少还在光标附近，不会莫名飞到屏幕左上角。
        debug_assert!(screen_xy.is_some(), "定位三分支必有其一赋值 screen_xy");
        let (screen_x, screen_y) = screen_xy.unwrap_or((self.x, self.y));
        // 鼠标层记录阴影扩边，两处要用：
        // 1. 拖动落定时把窗口左上换算回**内容左上**再上报落盘，否则每次「拖动→保存→
        //    重显」都会多减一次阴影，窗口逐次漂移；
        // 2. 拖动中按内容矩形钳制，让可见内容能真正贴到屏幕边缘（含任务栏）。
        self.mouse.borrow_mut().margin = (ml as i32, mt as i32, mr as i32, mb as i32);
        Some(RenderedFrame {
            screen_x,
            screen_y,
            width,
            height,
            scale: self.scale,
            software_shadow: shadow.is_some(),
            hit_rects: self.hit_rects.clone(),
            buf,
        })
    }

    /// 悬停时在该候选下方显示其编码（反查）；无悬停或无编码则隐藏。
    /// `(wx, wy)` 为候选窗口屏幕原点（命中矩形坐标的基准）。
    /// 横排：tooltip 在候选行下方（不足时上翻）。
    /// 竖排：tooltip 在候选窗右侧（不足时左侧），纵向对齐悬停候选行，避免遮挡下方候选。
    fn update_tooltip(&mut self, wx: i32, wy: i32) {
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
                    if self.vertical {
                        // 竖排：以悬停候选项自身宽度为锚点（hit rect 已含阴影偏移，wx+r.x 即屏幕坐标）。
                        // tooltip 显示在候选项右侧，空间不足时改左侧，不遮挡下方候选。
                        tip.show_beside(
                            &code,
                            wx + r.x as i32,         // 候选项左边界
                            wx + (r.x + r.w) as i32, // 候选项右边界
                            wy + r.y as i32,
                            wy + (r.y + r.h) as i32,
                        );
                    } else {
                        tip.show(
                            &code,
                            wx + r.x as i32,
                            wy + r.y as i32,
                            wy + (r.y + r.h) as i32,
                        );
                    }
                }
                None => tip.hide(),
            }
        }
    }

    /// host-render 专用：渲染当前悬停 tooltip 到 BGRA buffer。
    /// `(wx, wy)` 为候选窗口屏幕原点（与 render_frame 的 screen_x/y 一致）。
    /// 返回 `(bgra, w, h, screen_x, screen_y, software_shadow)`；无悬停/无文本返回 None。
    #[cfg(windows)]
    pub fn render_tooltip_frame(
        &mut self,
        wx: i32,
        wy: i32,
    ) -> Option<(Vec<u8>, u32, u32, i32, i32, bool)> {
        let hover = self.hover;
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

        let tip = self.tooltip.as_mut()?;
        match info {
            Some((code, r)) => {
                if self.vertical {
                    tip.render_frame_beside(
                        &code,
                        wx + r.x as i32,
                        wx + (r.x + r.w) as i32,
                        wy + r.y as i32,
                        wy + (r.y + r.h) as i32,
                    )
                } else {
                    tip.render_frame(
                        &code,
                        wx + r.x as i32,
                        wy + r.y as i32,
                        wy + (r.y + r.h) as i32,
                    )
                }
            }
            None => None,
        }
    }

    /// **内容**左上 → **窗口**左上（减去软阴影扩边）。
    ///
    /// 落盘的 custom_x/y 记的是内容左上（用户视觉上看到的窗口边缘），而 Win32 定位用的是
    /// 窗口左上。本函数与 `window_to_content` 必须严格互逆，否则每轮
    /// 「拖动 → 落盘 → 重新显示」都会多减一次阴影，候选窗逐次向左上漂移。
    ///
    /// 纯算术、无平台依赖，故【不加 cfg 门控】：调用方 `content_origin` 是未门控的 pub fn，
    /// 若把本对函数限定为 windows-only，非 Windows 目标就会 E0599 找不到函数——这类不对称
    /// 在 Windows 本地开发时永远显现不出来，只有 CI 的 macOS 目标才炸。
    /// Linux（既非 windows 也非 macos，仅供跑测试）下 `place_fixed` 无调用者，故显式
    /// allow(dead_code)：它与 `window_to_content` 共用互逆契约和 round-trip 测试，必须成对存在。
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    fn content_to_window(content: (i32, i32), ml: u32, mt: u32) -> (i32, i32) {
        (content.0 - ml as i32, content.1 - mt as i32)
    }

    /// **窗口**左上 → **内容**左上（加回软阴影扩边）。`content_to_window` 的逆。
    fn window_to_content(window: (i32, i32), ml: i32, mt: i32) -> (i32, i32) {
        (window.0 + ml, window.1 + mt)
    }

    /// 固定位置模式的窗口落点（返回窗口左上屏幕坐标，含阴影扩边）。
    ///
    /// - `fixed` 是**内容**左上坐标，减去阴影扩边 `(ml, mt)` 才是窗口左上。
    /// - `(0, 0)` 视作"已开启固定但尚未设定位置"（用户还没拖过），落到默认锚点。
    /// - 钳制按**内容**矩形、且钳到整块屏幕（含任务栏）：`custom_x/y` 是绝对屏幕坐标，
    ///   用户换分辨率或拔掉副屏后会指向不可见区域，候选窗就此"消失"且无法用鼠标拖回来；
    ///   但按含阴影的窗口矩形去钳会让内容离屏幕边还有一整个阴影宽就被拦下，见
    ///   [`clamp_content_to_monitor`]。
    ///
    /// macOS 同样走这里（`.app` 的 NSPanel 只是照搬算好的坐标）：那边不画软阴影，扩边恒 0，
    /// 且 `clamp_content_to_monitor` 在非 Windows 是恒等函数——真正的屏幕钳制由 `.app` 用
    /// `NSScreen.visibleFrame` 做，服务进程这边查不到 macOS 的显示器几何。
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    fn place_fixed(
        fixed: (i32, i32),
        caret_x: i32,
        caret_y: i32,
        width: u32,
        height: u32,
        margin: (i32, i32, i32, i32),
    ) -> (i32, i32) {
        let (ml, mt, _, _) = margin;
        let (wx, wy) = if fixed == (0, 0) {
            let content_w = (width as i32 - ml - margin.2).max(1) as u32;
            let (cx, cy) = Self::default_fixed_anchor(caret_x, caret_y, content_w);
            (cx - ml, cy - mt)
        } else {
            Self::content_to_window(fixed, ml as u32, mt as u32)
        };
        clamp_content_to_monitor(wx, wy, width, height, margin)
    }

    /// 固定模式尚未设定位置时的默认锚点（**内容**左上屏幕坐标）。
    ///
    /// 取**光标所在**显示器的工作区（用户在哪块屏打字就在哪块屏出现），水平居中、
    /// 垂直落在约 3/4 高度处：贴底会和任务栏/其他浮动 UI 挤在一起，居中则挡住正文。
    /// 内容高度不参与——顶端定在 3/4 处即可，超出底部由调用方的钳制拉回。
    ///
    /// 这里用 `rcWork` 而非 `rcMonitor`：自动落位应避开任务栏，只有用户**手动拖动**
    /// 才允许压到任务栏上。
    ///
    /// 非 Windows 退化为「就落在光标处」：服务进程查不到 macOS 的显示器几何（`.app` 才有
    /// `NSScreen`），而这只影响「刚打开固定位置、还没拖过」这一瞬——窗口出现在光标旁，
    /// 用户拖一次就定下来了。与其为此加一轮 IPC 往返，不如让首帧落在最不意外的地方。
    #[cfg_attr(not(windows), allow(unused_variables))]
    fn default_fixed_anchor(caret_x: i32, caret_y: i32, width: u32) -> (i32, i32) {
        #[cfg(windows)]
        {
            use windows::Win32::Foundation::POINT;
            use windows::Win32::Graphics::Gdi::{
                GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
            };
            unsafe {
                let pt = POINT {
                    x: caret_x,
                    y: caret_y,
                };
                let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
                let mut mi = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if GetMonitorInfoW(mon, &mut mi).as_bool() {
                    let wa = mi.rcWork;
                    let x = wa.left + ((wa.right - wa.left) - width as i32) / 2;
                    let y = wa.top + ((wa.bottom - wa.top) * 3) / 4;
                    return (x, y);
                }
            }
        }
        (caret_x, caret_y)
    }

    /// 据冻结光标锚点与当前内容尺寸计算候选窗位置，钳制在光标所在显示器工作区内。
    /// 规则：
    /// - 默认显示在光标下方（+gap）；下方空间不足则上翻到光标上方。
    /// - 上方显示以"窗口底边贴近光标顶端"为参考：`above_y = caret_top - h - gap`，
    ///   底边 = caret_top - gap 与高度无关 → 候选变少（h 减小）时顶边下移、底边不动，不会离光标变远。
    /// - `sticky_above`=true（当前已在上方）时优先保持上方，仅当上方也放不下才回落下方，
    ///   避免候选数量变化时上下抖动（需求 4）。
    /// - 左右溢出则贴边（横向右方空间不足时允许左移，属位置保护的例外）。
    ///
    /// 返回 (x, y, above)：above=true 表示窗口被上翻到光标上方（供 flip_when_above 判定）。
    ///
    /// 仅「跟随光标」定位方式走这里；固定位置见 [`Self::place_fixed`]。
    // 非 Windows 下窗口钳制为空实现，caret_h/w/h 及 x/y 的可变性仅 Windows 分支需要。
    #[cfg_attr(not(windows), allow(unused_variables, unused_mut))]
    /// 候选窗相对光标的**锚点**计算（纯函数，不含屏幕钳制）：返回 `(x, below_y, above_y)`。
    ///
    /// `caret_y` 为光标**底端**。下方锚点 = 底端 + gap；上方锚点以窗口底边贴光标顶端为参考。
    /// 主题偏移 `off_y` 语义恒为「远离光标」：下方 **+**、上方 **−**。写成同号会让上翻时
    /// 偏移把窗口压向光标，与配置意图相反——这是本函数存在的唯一理由（抽出来才测得到，
    /// `place_window` 余下部分是 `#[cfg(windows)]` 的屏幕钳制，依赖真实显示器）。
    fn caret_anchors(
        caret_x: i32,
        caret_y: i32,
        caret_h: i32,
        window_h: i32,
        off_x: i32,
    ) -> (i32, i32, i32) {
        let gap = 2;
        let below_y = caret_y + gap;
        let above_y = caret_y - caret_h.max(0) - window_h - gap;
        (caret_x + off_x, below_y, above_y)
    }

    /// 按最终方位施加主题 Y 偏移：`off_y` 正值**恒为远离光标**——在下方就向下、
    /// 在上方就向上。
    ///
    /// ⚠️ 必须在**方位已定之后**调用，不能把偏移预先加进锚点：
    /// `below_ok`/`above_ok` 是拿锚点跟工作区边界比出来的，锚点含偏移会让 off_y 越大
    /// 两个条件越难成立 —— 本该上翻的场景被判成「上方也放不下」，落回下方分支再被钳到
    /// `wa.bottom - hi`（贴屏幕底），窗口就压在光标上了。偏移只该改变距离，不该改变
    /// 「往上还是往下」这个决策。
    fn apply_offset_y(y: i32, above: bool, off_y: i32) -> i32 {
        if above { y - off_y } else { y + off_y }
    }

    /// 跟随光标定位。`offset` 为主题 `window.position_offset`（设备像素，已 ×scale）。
    ///
    /// ⚠️ 偏移**必须在此函数内、锚点计算处注入**，不能在调用方给返回值加：
    /// 下方那段 `rcWork` 越界兜底要在偏移之后跑，否则偏移能把窗口推出屏幕且再也钳不回来。
    /// 上方锚点用**减号**——`off_y` 正值语义恒为「远离光标」，上翻时是向上推。
    fn place_window(
        caret_x: i32,
        caret_y: i32,
        caret_h: i32,
        w: u32,
        h: u32,
        sticky_above: bool,
        offset: (i32, i32),
    ) -> (i32, i32, bool) {
        let (wi, hi) = (w as i32, h as i32);
        // 净锚点（不含 Y 偏移）：方位决策必须基于它，见 apply_offset_y 的说明。
        let (ax, below_y, above_y) = Self::caret_anchors(caret_x, caret_y, caret_h, hi, offset.0);
        let (mut x, mut y) = (ax, Self::apply_offset_y(below_y, false, offset.1));
        let mut above = false;
        #[cfg(windows)]
        {
            use windows::Win32::Foundation::POINT;
            use windows::Win32::Graphics::Gdi::{
                GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
            };
            unsafe {
                let pt = POINT {
                    x: caret_x,
                    y: caret_y,
                };
                let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
                let mut mi = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if GetMonitorInfoW(mon, &mut mi).as_bool() {
                    let wa = mi.rcWork;
                    // 可行性判定用**净锚点**（不含主题偏移）：偏移只该改变距离，
                    // 不该左右「往上还是往下」——含偏移会让 off_y 越大越容易判成两边都
                    // 放不下，落回下方再被钳到屏幕底、压住光标。
                    let below_ok = below_y + hi <= wa.bottom;
                    let above_ok = above_y >= wa.top;
                    // 垂直方位决策：
                    // - 已在上方（sticky）→ 只要上方放得下就保持上方；上方放不下才回落下方（需求 4）。
                    // - 否则默认下方，仅当下方放不下且上方放得下才上翻。
                    above = if sticky_above {
                        above_ok
                    } else {
                        !below_ok && above_ok
                    };
                    // 方位定后再施加偏移：上方向上、下方向下，均为远离光标。
                    y = Self::apply_offset_y(
                        if above { above_y } else { below_y },
                        above,
                        offset.1,
                    );
                    // 方位定后的越界兜底：贴住对应屏幕边。
                    // 偏移可能把窗口顶出工作区，这里兜回来——所以偏移必须在钳制之前施加。
                    if above {
                        if y < wa.top {
                            y = wa.top;
                        }
                    } else if y + hi > wa.bottom {
                        y = wa.bottom - hi;
                        if y < wa.top {
                            y = wa.top;
                        }
                    }
                    // 左右钳制（右溢出左移；左溢出贴左）。
                    if x + wi > wa.right {
                        x = wa.right - wi;
                    }
                    if x < wa.left {
                        x = wa.left;
                    }
                }
            }
        }
        (x, y, above)
    }

    /// 窗口投影参数（共享 SoftShadow；读 RvViews 顶层 shadow_* 字段）。
    fn shadow_params(&self) -> Option<crate::view::SoftShadow> {
        let v = &self.theme.views;
        crate::view::SoftShadow::build(
            v.shadow_offset_x,
            v.shadow_offset_y,
            v.shadow_blur,
            v.shadow_spread,
            v.shadow_spread_offset_x,
            v.shadow_spread_offset_y,
            v.shadow_color,
            self.scale,
        )
    }

    /// 把 image ref 解析为可读绝对路径（委托共享 theme_assets）。
    fn asset_path(&self, reference: &str) -> Option<String> {
        crate::theme_assets::asset_path(&self.theme, reference)
    }

    /// RvImage → 渲染用 ViewImage（委托共享 theme_assets）。
    fn rv_image(&self, im: Option<&wind_theme::RvImage>) -> Option<ViewImage> {
        crate::theme_assets::rv_image(&self.theme, im)
    }

    /// footer 翻页箭头图标（SVG + tint）。无 prev/next_image 时 None（回退文字箭头）。
    /// enabled→tint（如 ${accent}）；disabled→disabled_tint（如 ${text_hint}），缺则回退 tint。
    fn arrow_icon(&self, im: Option<&wind_theme::RvImage>, enabled: bool) -> Option<ViewImage> {
        let im = im?;
        let path = self.asset_path(&im.reference)?;
        let tint = if enabled {
            im.tint
        } else {
            im.disabled_tint.or(im.tint)
        };
        Some(ViewImage {
            path,
            // stretch：图标栅格化到其所在小节点尺寸（footer_fs，= 主题图标尺寸），
            // 该小节点再居中于更大的触摸盒内，从而"图标尺寸不变、操作区放大"。
            mode: "stretch".into(),
            slice: [0.0; 4],
            opacity: 1.0,
            tint,
        })
    }

    /// RvImage[] → ViewLayer[]（委托共享 theme_assets）。
    fn rv_layers(&self, layers: &[wind_theme::RvImage]) -> Vec<ViewLayer> {
        crate::theme_assets::rv_layers(&self.theme, layers, self.scale)
    }

    /// 主题 `window.position_offset` → 设备像素 (x, y)。未配=(0,0)，与旧行为一致。
    ///
    /// 供边缘带装饰的主题拉开候选窗与光标的观感距离。**只喂给 place_window**——
    /// 固定位置与用户拖动是显式意图，再叠加会让窗口莫名偏离用户放的地方，
    /// 且会破坏拖动落盘的 window↔content 换算互逆性（见 place_fixed 一侧的坐标约定）。
    fn position_offset_px(&self) -> (i32, i32) {
        let v = &self.theme.views;
        let px = |d: Option<wind_theme::schema::Dim>| {
            d.map(|x| x.resolve(self.scale, 0.0)).unwrap_or(0.0).round() as i32
        };
        (px(v.window_offset_x), px(v.window_offset_y))
    }

    /// RvGradient → 渲染用 ViewGradient（stop 颜色直通 [R,G,B,A]）。
    fn rv_gradient(&self, g: Option<&wind_theme::RvGradient>) -> Option<crate::view::ViewGradient> {
        let g = g?;
        if g.stops.is_empty() {
            return None;
        }
        Some(crate::view::ViewGradient {
            radial: g.kind == "radial",
            angle: g.angle,
            stops: g.stops.clone(),
        })
    }

    /// 按当前状态构建候选视图树（横向布局）。
    /// T3：从 RVNode 树（`Resolved.views`）取色/几何，颜色 None→兜底（与旧 ResolvedTheme 默认等值，零回归）。
    /// 构建候选 View 树。`above=true`（窗口上翻到光标上方）时按三个正交开关调整布局：
    /// 反转候选（flip_when_above，仅竖排）、交换编码/候选区（swap_preedit_when_above）、翻页栏并入编码栏
    /// （pager_in_preedit，与 above 无关的常驻行为）。任何情形下每项 tag/序号标签/选中判定仍用
    /// 原始索引 i，确保命中/选中映射不受排列变化影响。
    /// 把插入符位置夹到合法字符边界。协调器发来的偏移与它自己的 preedit 同源、本应恒合法，
    /// 但 UI 与协调器是跨线程的两份状态（命令队列还会合并/丢弃），一旦错位 `split_at` 会
    /// **panic 掉整个 UI 线程**。故在此收口：越界截到末尾，落在字符中间则退到前一个边界。
    fn clamp_caret(text: &str, caret: usize) -> usize {
        let mut c = caret.min(text.len());
        while c > 0 && !text.is_char_boundary(c) {
            c -= 1;
        }
        c
    }

    /// 编码栏文本节点（含插入符）。
    ///
    /// 插入符走 `View::caret_at` 的覆盖层语义：文本作为**整串**一次整形，宽度与光标位置无关，
    /// 故移动光标不会让编码位移。曾把文本拆成「前半 + 竖线 + 后半」三节点，因
    /// `measure(a+b) != measure(a)+measure(b)`（字距在拆分边界丢失 + 每段各自舍入）、
    /// 且拆分点随光标而变，导致整串宽度抖动、字符跟着晃。
    fn preedit_view(&self, fs: f32) -> View {
        let v = &self.theme.views;
        View::leaf(
            self.preedit.clone(),
            v.preedit_bar.text_color.unwrap_or([100, 100, 100, 255]),
        )
        .font_size(fs)
        .font_weight(v.preedit_bar.font_weight)
        .font_family(v.preedit_bar.font_family.clone())
        // 竖线宽度随 DPI 缩放，但至少 1px（否则高分屏下会消失）。
        .caret_at(self.preedit_caret, self.scale.max(1.0))
    }

    fn build_tree(&self, above: bool) -> View {
        use wind_theme::rvnode::{RvEdges, RvNode};
        use wind_theme::schema::Dim;
        let t = &self.theme;
        let v = &t.views;
        // above=true（窗口被上翻到光标上方）时，据两个正交开关派生上方专属行为：
        //   flip_cands = 反转候选项排列顺序（ui.candidate.flip_when_above，仅竖排有意义）
        //   swap_bands = 交换编码区↔候选区上下位置（ui.candidate.swap_preedit_when_above，编码沉底贴光标）
        // 二者可单独或叠加：叠加时候选块内部反转 + 编码沉底 = 候选1 紧挨编码栏。
        // 生效判据见 above_layout（与外层「要不要重建」共用同一份，避免两处各判一套）。
        let (flip_cands, swap_bands) = Self::above_layout(
            above,
            self.vertical,
            self.flip_when_above,
            self.swap_preedit_when_above,
        );
        // 翻页栏并入编码栏行（右对齐）：需开关开启 + 有独立编码栏（非嵌入）+ 翻页栏本身可见。
        // 满足时翻页栏随编码区一同装配（swap 时自动跟随沉底）；否则回退候选行尾/竖排底部独立行。
        let pager_will_inline = self.pager_in_preedit
            && !self.preedit.is_empty()
            && !self.preedit_embedded
            && self.pager_visible();
        let s = self.scale;
        // 字号：base = 用户覆盖(ui.candidate.font_size>0) 否则主题 behavior.font_size（默认 18）× DPI；
        // 序号/注释/预编辑按各节点 font_size（相对主字号的有符号逻辑偏移）调整。
        let base_logical = if self.font_size_override > 0.0 {
            self.font_size_override
        } else {
            t.behavior.font_size as f32
        };
        let base_fs = base_logical * s;
        let node_fs = |n: &RvNode| (base_fs + n.font_size * s).max(6.0 * s);
        let index_fs = node_fs(&v.index);
        let text_fs = node_fs(&v.text);
        let preedit_fs = node_fs(&v.preedit_bar);

        // 颜色：None→兜底。
        let col = |o: Option<[u8; 4]>, d: [u8; 4]| o.unwrap_or(d);
        // 单个 Dim→设备像素（dp×scale）；None→def_logical×scale。
        let dim = |o: Option<Dim>, def_logical: f32| {
            o.map(|x| x.resolve(s, 0.0)).unwrap_or(def_logical * s)
        };
        // RvEdges 四边内边距→设备像素 Edges；逐边 None→对应 def_logical×scale。
        let edges_or = |e: &RvEdges, d: [f32; 4]| Edges {
            t: e.top.map(|x| x.resolve(s, 0.0)).unwrap_or(d[0] * s),
            r: e.right.map(|x| x.resolve(s, 0.0)).unwrap_or(d[1] * s),
            b: e.bottom.map(|x| x.resolve(s, 0.0)).unwrap_or(d[2] * s),
            l: e.left.map(|x| x.resolve(s, 0.0)).unwrap_or(d[3] * s),
        };
        // 状态 patch 取色（selected/hover 的 bg/text，None patch 或缺色→兜底）。
        let patch_bg =
            |p: &Option<Box<RvNode>>, d: [u8; 4]| p.as_ref().and_then(|n| n.bg_color).unwrap_or(d);
        // 状态文字色（与 Go effectiveNode 对齐）：选中优先于悬停；选中/悬停 patch 未给文字色
        // → 回退基态色（不跨态借色）。index/text/comment 同一套消费。
        let eff_text = |node: &RvNode, base: [u8; 4], sel: bool, hov: bool| -> [u8; 4] {
            let st = if sel {
                node.selected.as_deref()
            } else if hov {
                node.hover.as_deref()
            } else {
                None
            };
            st.and_then(|n| n.text_color).unwrap_or(base)
        };
        // 有效字重（与 eff_text 同构）：节点/item 的状态 patch 字重优先，回退节点/item 基态；0=继承默认。
        // item 参与是因为主题常把"选中加粗"配在 [item.selected].font_weight（如 jidian），需作用到候选文本。
        let eff_weight = |node: &RvNode, item: &RvNode, sel: bool, hov: bool| -> i32 {
            let state = |n: &RvNode| -> Option<i32> {
                let st = if sel {
                    n.selected.as_deref()
                } else if hov {
                    n.hover.as_deref()
                } else {
                    None
                };
                st.map(|s| s.font_weight).filter(|w| *w != 0)
            };
            state(node)
                .or_else(|| state(item))
                .or_else(|| (node.font_weight != 0).then_some(node.font_weight))
                .or_else(|| (item.font_weight != 0).then_some(item.font_weight))
                .unwrap_or(0)
        };
        // 有效边框（base ⊕ selected/hover patch，对齐 Go effectiveNode）：返回设备像素
        // (颜色, 线宽, 圆角)。仅当节点/状态给了 border 色才绘制（如 svgtest text.selected.border）。
        let eff_border = |node: &RvNode, sel: bool, hov: bool| -> Option<([u8; 4], f32, f32)> {
            let st = if sel {
                node.selected.as_deref()
            } else if hov {
                node.hover.as_deref()
            } else {
                None
            };
            let color = st.and_then(|n| n.border_color).or(node.border_color)?;
            let width = st
                .and_then(|n| n.border_width)
                .or(node.border_width)
                .map(|d| d.resolve(s, 0.0))
                .unwrap_or(s)
                .max(1.0);
            let radius = st
                .and_then(|n| n.border_radius)
                .or(node.border_radius)
                .map(|d| d.resolve(s, 0.0))
                .unwrap_or(0.0);
            Some((color, width, radius))
        };
        // 有效背景色：状态 patch ?? 基态。与 eff_border 同构。
        // 独立于 patch_bg（那个只看状态、且带硬兜底），用于「基态也要能配底色」的节点。
        let eff_bg = |node: &RvNode, sel: bool, hov: bool| -> Option<[u8; 4]> {
            let st = if sel {
                node.selected.as_deref()
            } else if hov {
                node.hover.as_deref()
            } else {
                None
            };
            st.and_then(|n| n.bg_color).or(node.bg_color)
        };

        // 通用盒装饰：底色 / 背景图 / 渐变 / z 层覆盖图 / 边框 + 圆角。
        // 每项都是 Option —— 主题未配则原样返回，几何与观感零变化。供此前只有文字色、
        // 连背景边框都没接线的容器节点（candidate_list / footer_bar）复用。
        // 圆角只在显式配了 border_radius 时设置：View 默认 0，不设即方角，与原行为一致。
        let decorate_box = |view: View, node: &RvNode| -> View {
            let mut view = view;
            if let Some(c) = node.bg_color {
                view = view.bg(c);
            }
            if let Some(vi) = self.rv_image(node.bg_image.as_ref()) {
                view = view.bg_image(vi);
            }
            if let Some(g) = self.rv_gradient(node.bg_gradient.as_ref()) {
                view = view.bg_gradient(g);
            }
            let layers = self.rv_layers(&node.layers);
            if !layers.is_empty() {
                view = view.layers(layers);
            }
            if let Some(r) = node.border_radius {
                view = view.radius(r.resolve(s, 0.0));
            }
            if let Some(bc) = node.border_color {
                view = view.border(bc, dim(node.border_width, 0.0).max(1.0));
            }
            view
        };

        // 模式徽标的盒装饰（底色 / 边框 / 圆角 / 内边距）。
        // **横排内嵌与竖排独立 chip 是两条通路**，抽成闭包共用——本仓「同一功能两处装配、
        // 只改一处」是高发 bug 形态。
        // 门控：有底色**或**有边框才渲染成徽标盒（原先只看底色，那样「只要边框的空心徽标」
        // 表达不出来）；两者皆无时保持纯文字，零回归。
        // 圆角一律取 mode_label.border_radius ?? item.border_radius —— **不能用 eff_border
        // 返回的第三个值**，它在未配 border.radius 时兜底 0.0，会把徽标圆角抹平（同 :1824 的坑）。
        let decorate_mode_chip = |chip: View| -> View {
            let border = eff_border(&v.mode_label, false, false);
            if v.mode_label.bg_color.is_none() && border.is_none() {
                return chip;
            }
            let mut chip = chip
                .radius(dim(
                    v.mode_label.border_radius.or(v.item.border_radius),
                    4.0,
                ))
                .pad(edges_or(&v.mode_label.padding, [1.0, 6.0, 1.0, 6.0]));
            if let Some(bg) = v.mode_label.bg_color {
                chip = chip.bg(bg);
            }
            if let Some((bc, bw, _)) = border {
                chip = chip.border(bc, bw);
            }
            chip
        };

        let mut root = View::container(Layout::Column)
            .bg(col(v.window.bg_color, [255, 255, 255, 255]))
            .border(
                col(v.window.border_color, [200, 200, 200, 200]),
                dim(v.window.border_width, 1.0).max(1.0),
            )
            .radius(dim(v.window.border_radius, 8.0))
            .pad(edges_or(&v.window.padding, [6.0, 8.0, 6.0, 8.0]))
            // band 间距（预编辑条↔候选列表↔翻页）= 主题 candidate_list.band_gap，未配→0（与 Go 一致）。
            .gap(dim(v.window_gap, 0.0));
        // 窗口背景图（九宫格/拉伸位图皮肤，如 jidian 的 panel）。
        if let Some(vi) = self.rv_image(v.window.bg_image.as_ref()) {
            root = root.bg_image(vi);
        }
        // 窗口背景渐变（叠在底色上、背景图下）。
        if let Some(g) = self.rv_gradient(v.window.bg_gradient.as_ref()) {
            root = root.bg_gradient(g);
        }
        // 窗口 z 层覆盖图（如 jidian 右下角 mark 水印）。
        let win_layers = self.rv_layers(&v.window.layers);
        if !win_layers.is_empty() {
            root = root.layers(win_layers);
        }

        // 预编辑行（主题背景带 + 文本色）。完整渲染 preedit_bar 自身背景：底色 + 位图背景 +
        // z 层 + 边框（此前只画底色，位图主题的 preedit 背景/边框丢失）。
        // 嵌入模式（preedit_embedded）下不画独立条——编码作为候选行首单元内联（见下方 list 构建）。
        // 【延迟装配】编码栏与分隔线不在此直接 push root，先存入变量；末尾装配段据 swap_bands 决定
        // 放窗口顶部（正常）或底部（编码沉底贴光标），并据 pager_will_inline 追加翻页栏到栏行右端。
        let mut preedit_band: Option<View> = None;
        let mut preedit_sep: Option<View> = None;
        if !self.preedit.is_empty() && !self.preedit_embedded {
            let mut band = View::container(Layout::Row)
                .cross(Align::Center)
                .bg(col(v.preedit_bar.bg_color, [240, 240, 240, 255]))
                // 圆角：preedit_bar 自己的优先，未配才跟随 item（历史行为——此前只读
                // item.border_radius，调候选项圆角会连带改预编辑栏）。`.or()` 保回退链，
                // 老主题零变化。
                .radius(dim(
                    v.preedit_bar.border_radius.or(v.item.border_radius),
                    4.0,
                ))
                .pad(edges_or(&v.preedit_bar.padding, [3.0, 8.0, 3.0, 8.0]))
                .margin(edges_or(&v.preedit_bar.margin, [0.0; 4]))
                .child(self.preedit_view(preedit_fs));
            if let Some(vi) = self.rv_image(v.preedit_bar.bg_image.as_ref()) {
                band = band.bg_image(vi);
            }
            if let Some(g) = self.rv_gradient(v.preedit_bar.bg_gradient.as_ref()) {
                band = band.bg_gradient(g);
            }
            let band_layers = self.rv_layers(&v.preedit_bar.layers);
            if !band_layers.is_empty() {
                band = band.layers(band_layers);
            }
            if let Some(bc) = v.preedit_bar.border_color {
                band = band.border(bc, dim(v.preedit_bar.border_width, 0.0).max(1.0));
            }
            // 右对齐区（mode_label / 并入的翻页栏）：band 跨轴撑满窗口内容宽 + spacer 吸收中间空白，
            // 才能把右侧内容顶到栏行末尾。二者任一存在即启用（排布：[编码] spacer [mode_label] [翻页栏]）。
            let need_right_align = !self.mode_label.is_empty() || pager_will_inline;
            if need_right_align {
                band = band.fill_cross().child(View::spacer());
            }
            // 模式标记：右对齐到栏行末尾（在翻页栏之左）；字号取主题 mode_label 配置。
            if !self.mode_label.is_empty() {
                let ml_fs = node_fs(&v.mode_label);
                let chip = decorate_mode_chip(
                    View::leaf(
                        self.mode_label.clone(),
                        col(v.mode_label.text_color, [120, 120, 128, 255]),
                    )
                    .font_size(ml_fs)
                    .font_weight(v.mode_label.font_weight)
                    .font_family(v.mode_label.font_family.clone())
                    .margin(Edges {
                        l: 12.0 * s,
                        ..Edges::default()
                    }),
                );
                band = band.child(chip);
            }
            // 翻页栏并入（pager_will_inline）：在末尾装配段追加到此 band 末尾（spacer 右侧）。
            // 清风设计主题（定义 separator 色）：预编辑行全宽 + 底部极淡分隔线（与候选区分）；
            // 普通/第三方主题（无 separator）保持原行为（内容宽、无分隔线），尽量减少影响。
            let sep_col = t.color("separator", [0, 0, 0, 0]);
            let preedit_designed = sep_col[3] > 0;
            if preedit_designed && !need_right_align {
                band = band.fill_cross();
            }
            preedit_band = Some(band);
            if preedit_designed {
                preedit_sep = Some(
                    View::container(Layout::Row)
                        .bg(sep_col)
                        .fixed_h((1.0 * s).max(1.0))
                        .fill_cross()
                        .margin(Edges {
                            b: 4.0 * s,
                            ..Edges::default()
                        }),
                );
            }
        }

        // 候选项颜色（基态）。状态色（选中/悬停）逐项经 eff_text 计算。
        let text_color = col(v.text.text_color, [30, 30, 30, 255]);
        let sel_bg = patch_bg(&v.item.selected, [230, 240, 255, 255]);
        let hover_bg = patch_bg(&v.item.hover, [238, 242, 247, 255]);
        let index_color = col(v.index.text_color, [66, 133, 244, 255]);
        let comment_color = col(v.comment.text_color, [150, 150, 150, 255]);
        let comment_fs = node_fs(&v.comment);
        let index_circle = v.index.bg_shape == "circle";
        let index_circle_bg = col(v.index.bg_color, [66, 133, 244, 255]);
        let item_pad = edges_or(&v.item.padding, [7.0, 10.0, 7.0, 8.0]);
        let item_radius = dim(v.item.border_radius, 4.0);
        // 候选间距全由主题决定（与 Go 对齐，以主题配置为准）：
        // 横排候选框间隙 = max(item_spacing − item 左右内边距, 0)——内边距本身已提供视觉间隔，
        // 扣除避免"间距 + 内边距"叠加（旧逻辑用 config×2 凭空多加一段，致 msime 元素间隔不一致）。
        // 竖排行距 = candidate_list.row_gap。主题未配 → 0（候选框相邻，靠 padding 分隔）。
        let item_spacing = v.item_spacing.map(|d| d.resolve(s, 0.0)).unwrap_or(0.0);
        let box_gap = (item_spacing - item_pad.l - item_pad.r).max(0.0);
        let row_gap_v = dim(v.row_gap, 0.0);
        // 选中候选左侧强调条（仅主题启用时，如 msime/jidian）。
        // height_ratio 判零回退：RvViews 若未经 resolve 填充（Default 构造）该字段是 0.0，
        // 直接乘会把条高算成 0 → 钳到 2px 细线，观感是强调条消失。
        let accent_bar = v.accent_bar_enabled.then(|| LeftBar {
            color: col(v.accent_bar.bg_color, [66, 133, 244, 255]),
            width: dim(v.accent_bar_width, 3.0),
            height_ratio: if v.accent_bar_height_ratio > 0.0 {
                v.accent_bar_height_ratio
            } else {
                DEFAULT_ACCENT_BAR_HEIGHT_RATIO
            },
            offset: dim(v.accent_bar_offset, 0.0),
        });

        // 候选列表：横排=Row（cell 并列）；竖排=Column（候选纵向堆叠）。
        // 无候选时（仅提示徽标/preedit，无候选可纵向堆叠）强制用 Row：徽标 + 等高占位
        // 并排成单行，高度 = 一个候选行，与横排一致；否则竖排下徽标行与占位行会纵向
        // 堆叠致提示窗口过高（网址/临拼/临英刚进入时尤甚）。preedit/徽标分隔方向亦随之。
        let list_vertical = self.vertical && !self.candidates.is_empty();
        // candidate_list 此前只贡献 gap/row_gap/band_gap 三个间距，背景与边框从不接线，
        // 「候选区与编码栏用不同底色分区」这类设计表达不出来。装饰全 Option，未配零回归。
        let mut list = decorate_box(
            if list_vertical {
                // fill_cross 让候选列撑满 root 宽度（= max(最宽候选, 翻页栏)），
                // 再配合各候选 item 的 fill_cross，所有候选高亮宽度统一。
                View::container(Layout::Column).gap(row_gap_v).fill_cross()
            } else {
                View::container(Layout::Row)
                    .gap(box_gap)
                    .cross(Align::Center)
            }
            // 候选区内边距（默认全 0=零回归）。footer_bar 不加这项——它的 padding
            // 已被翻页箭头消费（fpad），容器再加一层会双重生效。
            .pad(edges_or(&v.candidate_list.padding, [0.0; 4])),
            &v.candidate_list,
        );
        // 内联编码的"沉底"（swap_preedit_when_above 在首单元内联下的落点）：
        // 内联模式没有独立编码栏可参与末尾那段 band/list 交换装配，编码是 list 的首个子节点。
        // 横排下首单元 = 行首（最左），与上下无关，开关本就无意义 → 不参与；
        // 竖排下首单元 = 列首（最上），语义上等同于"编码栏在上"，此时开关必须生效 →
        // 把内联编码（及紧随其后的模式标记，二者共同构成独立栏模式下的"编码栏"内容）
        // 延迟到候选项之后再挂，得到与独立栏模式一致的"编码沉底贴光标"表现。
        let inline_preedit_bottom =
            swap_bands && list_vertical && self.preedit_embedded && !self.preedit.is_empty();
        // 沉底时延迟装配的内联节点，按原相对顺序在候选项之后追加。
        let mut inline_tail: Vec<View> = Vec::new();
        // 嵌入模式：编码作为候选行首单元内联（无独立背景，与候选间留白分隔），对齐 Go buildEmbeddedPreedit。
        // 横排右留白、竖排下留白（沉底时翻到上留白，否则会紧贴末个候选）。
        if self.preedit_embedded && !self.preedit.is_empty() {
            // 左缩进与独立条模式一致：取 preedit_bar.padding.left，避免编码贴窗口左缘。
            let pe_left = edges_or(&v.preedit_bar.padding, [3.0, 8.0, 3.0, 8.0]).l;
            let sep = if list_vertical {
                let gap = 6.0 * s;
                Edges {
                    l: pe_left,
                    t: if inline_preedit_bottom { gap } else { 0.0 },
                    b: if inline_preedit_bottom { 0.0 } else { gap },
                    ..Edges::default()
                }
            } else {
                Edges {
                    l: pe_left,
                    r: 16.0 * s,
                    ..Edges::default()
                }
            };
            let node = self.preedit_view(preedit_fs).margin(sep);
            if inline_preedit_bottom {
                inline_tail.push(node);
            } else {
                list = list.child(node);
            }
        }
        // 模式标记（拼/双/快/英/符 或全称）：紧随输入缓冲之后、候选之前内联显示。
        // 仅"无独立 preedit 栏"时在此（内联编码 candidate_inline / 内嵌应用 app_inline /
        // 栏模式但暂无 preedit）；有 preedit 栏时已置于栏行最后（见上）。
        // 颜色/背景由主题 mode_label 节点配置，与普通候选区分。横排右留白、竖排下留白。
        let preedit_bar_shown = !self.preedit.is_empty() && !self.preedit_embedded;
        if !self.mode_label.is_empty() && !preedit_bar_shown {
            let ml_fs = node_fs(&v.mode_label);
            let sep = if list_vertical {
                let gap = 6.0 * s;
                Edges {
                    t: if inline_preedit_bottom { gap } else { 0.0 },
                    b: if inline_preedit_bottom { 0.0 } else { gap },
                    ..Edges::default()
                }
            } else {
                Edges {
                    r: 12.0 * s,
                    ..Edges::default()
                }
            };
            // 主题为 mode_label 配了底色或边框 → 渲染为小徽标（圆角 + 内边距）以更醒目。
            // 与横排内嵌通路共用 decorate_mode_chip，两处装配保持一致。
            let chip = decorate_mode_chip(
                View::leaf(
                    self.mode_label.clone(),
                    col(v.mode_label.text_color, [120, 120, 128, 255]),
                )
                .font_size(ml_fs)
                .font_weight(v.mode_label.font_weight)
                .font_family(v.mode_label.font_family.clone())
                .margin(sep),
            );
            if inline_preedit_bottom {
                inline_tail.push(chip);
            } else {
                list = list.child(chip);
            }
        }
        // 无候选但有提示（模式徽标 / preedit）时：补一个与正常候选行等高的透明占位行，
        // 使提示窗口（如网址模式、临拼/临英刚进入）高度与有候选时及普通候选窗一致，
        // 避免窗口忽高忽低。占位行内边距/字号与候选行一致 → 测得同高，内容透明不可见。
        if self.candidates.is_empty() {
            list = list.child(
                View::container(Layout::Row)
                    .cross(Align::Center)
                    .pad(item_pad)
                    .child(
                        View::leaf(" ".to_string(), [0, 0, 0, 0]).font_size(text_fs.max(index_fs)),
                    ),
            );
        }
        // 逆序仅改排列顺序；i 仍是原始索引（tag/标签/选中据此）
        let mut order: Vec<(usize, &CandidateItem)> = self.candidates.iter().enumerate().collect();
        if flip_cands {
            order.reverse();
        }
        for (i, cand) in order {
            let is_sel = i == self.selected;
            let is_hover = self.hover >= 0 && self.hover as usize == i;
            // 状态文字色（选中/悬停各自的色，回退基态）。
            let txt_color = eff_text(&v.text, text_color, is_sel, is_hover);
            let cmt_color = eff_text(&v.comment, comment_color, is_sel, is_hover);

            // 行内间距改由各子节点 margin 承载（与前端盒模型一致）：text.margin.left 默认 4dp
            // 作为序号↔文字间距；不再用容器 gap 借 text.margin.left。
            let mut item = View::container(Layout::Row)
                .cross(Align::Center)
                .pad(item_pad)
                .margin(edges_or(&v.item.margin, [0.0; 4]))
                .radius(item_radius)
                .tag(i as i32);
            // 竖排时撑满列宽（等于最宽候选的宽度），高亮背景宽度统一，与 Go 行为一致。
            if list_vertical {
                item = item.fill_cross();
            }
            // 序号节点：no_index 项（如快捷加词提示行）完全跳过，避免空圆圈占位。
            if !cand.no_index {
                let marker = if cand.label.is_empty() {
                    (i + 1).to_string()
                } else {
                    cand.label.clone()
                };
                let idx_color = eff_text(&v.index, index_color, is_sel, is_hover);
                // 圆圈样式 → 方形节点 + 真圆背景 + 居中数字。
                let mut idx_leaf = View::leaf(marker, idx_color)
                    .font_size(index_fs)
                    .font_weight(eff_weight(&v.index, &v.item, is_sel, is_hover))
                    .font_family(v.index.font_family.clone())
                    .pad(edges_or(&v.index.padding, [0.0; 4]))
                    .margin(edges_or(&v.index.margin, [0.0; 4]));
                if index_circle {
                    let d = (index_fs * 1.5).round();
                    idx_leaf = idx_leaf
                        .circle_bg(index_circle_bg)
                        .fixed_w(d)
                        .fixed_h(d)
                        .text_align(Align::Center);
                }
                if let Some((bc, bw, br)) = eff_border(&v.index, is_sel, is_hover) {
                    idx_leaf = idx_leaf.border(bc, bw).radius(br);
                }
                item = item.child(idx_leaf);
            }
            // no_index 行无序号节点：文字左边距归零，顶格不留序号间距（消除占位）。
            let text_margin = if cand.no_index {
                Edges::default()
            } else {
                edges_or(&v.text.margin, [0.0, 0.0, 0.0, 4.0])
            };
            let mut tleaf = View::leaf(visible_whitespace(&cand.text).into_owned(), txt_color)
                .font_size(text_fs)
                .font_weight(eff_weight(&v.text, &v.item, is_sel, is_hover))
                .font_family(v.text.font_family.clone())
                .pad(edges_or(&v.text.padding, [0.0; 4]))
                .margin(text_margin);
            // 文字叶子的背景（底色/图/渐变）：此前只画边框，配了背景一律不生效，
            // 「文字药丸」这类样式做不出来。圆角由下面的 eff_border 一并给。
            if let Some(c) = eff_bg(&v.text, is_sel, is_hover) {
                tleaf = tleaf.bg(c);
            }
            if let Some(vi) = self.rv_image(v.text.bg_image.as_ref()) {
                tleaf = tleaf.bg_image(vi);
            }
            if let Some(g) = self.rv_gradient(v.text.bg_gradient.as_ref()) {
                tleaf = tleaf.bg_gradient(g);
            }
            if let Some((bc, bw, br)) = eff_border(&v.text, is_sel, is_hover) {
                tleaf = tleaf.border(bc, bw).radius(br);
            }
            item = item.child(tleaf);
            // 注释（编码后缀/短语提示）：非空时在候选词右侧以注释样式内联显示。
            // 内/外边距完整消费：comment.padding 四边 + comment.margin 四边（左默认 6dp 兜底间距）。
            if !cand.comment.is_empty() {
                let mut cleaf = View::leaf(cand.comment.clone(), cmt_color)
                    .font_size(comment_fs)
                    .font_weight(eff_weight(&v.comment, &v.item, is_sel, is_hover))
                    .font_family(v.comment.font_family.clone())
                    .pad(edges_or(&v.comment.padding, [0.0; 4]))
                    .margin(edges_or(&v.comment.margin, [0.0, 0.0, 0.0, 6.0]));
                // 注释叶子背景同 text（「注释气泡」样式）。
                if let Some(c) = eff_bg(&v.comment, is_sel, is_hover) {
                    cleaf = cleaf.bg(c);
                }
                if let Some(vi) = self.rv_image(v.comment.bg_image.as_ref()) {
                    cleaf = cleaf.bg_image(vi);
                }
                if let Some(g) = self.rv_gradient(v.comment.bg_gradient.as_ref()) {
                    cleaf = cleaf.bg_gradient(g);
                }
                if let Some((bc, bw, br)) = eff_border(&v.comment, is_sel, is_hover) {
                    cleaf = cleaf.border(bc, bw).radius(br);
                }
                item = item.child(cleaf);
            }
            // 候选项基态底色：此前只在选中/悬停时调 .bg()，`[item] background = "…"` 从不生效，
            // 而同级的背景图/渐变基态是读的（见下），三者本该同级。选中/悬停底色随后覆盖。
            if let Some(c) = v.item.bg_color {
                item = item.bg(c);
            }
            // 选中底色优先于悬停底色（两者独立：选中=空格上屏目标，悬停=鼠标提示）
            if is_sel {
                item = item.bg(sel_bg);
                if let Some(bar) = accent_bar {
                    item = item.left_bar(bar);
                }
            } else if is_hover {
                item = item.bg(hover_bg);
            }
            // 候选项背景图：选中态优先用 selected patch 的图（如 jidian 的 sel.png），否则用 base。
            let item_img = if is_sel {
                self.rv_image(v.item.selected.as_ref().and_then(|n| n.bg_image.as_ref()))
                    .or_else(|| self.rv_image(v.item.bg_image.as_ref()))
            } else {
                self.rv_image(v.item.bg_image.as_ref())
            };
            if let Some(vi) = item_img {
                item = item.bg_image(vi);
            }
            // 候选项背景渐变：选中态优先用 selected patch 的渐变，否则用 base。
            let item_grad = if is_sel {
                self.rv_gradient(
                    v.item
                        .selected
                        .as_ref()
                        .and_then(|n| n.bg_gradient.as_ref()),
                )
                .or_else(|| self.rv_gradient(v.item.bg_gradient.as_ref()))
            } else {
                self.rv_gradient(v.item.bg_gradient.as_ref())
            };
            if let Some(g) = item_grad {
                item = item.bg_gradient(g);
            }
            // 候选项边框（含 selected/hover 换色换宽）。eff_border 此前只用在
            // index/text/comment 三个叶子上，item 容器自己从没画过边框——
            // 主题里写的 [item] border / [item.selected] border 一律不生效。
            // 只取色与宽：圆角沿用上面按 item_radius 设好的值，避免 eff_border 的
            // 0.0 兜底把未配 border.radius 的候选项圆角抹平。
            if let Some((bc, bw, _)) = eff_border(&v.item, is_sel, is_hover) {
                item = item.border(bc, bw);
            }
            list = list.child(item);
        }
        // 内联编码沉底（见 inline_preedit_bottom）：候选项装配完毕后按原顺序追加编码与模式标记。
        // 竖排未并入的翻页栏另在末尾装配段随 swap 翻到顶部，故此处追加即为窗口最底部。
        for node in inline_tail {
            list = list.child(node);
        }

        // 翻页器（多页时）：‹ p/t › —— 箭头可点击翻页，带悬停高亮 + 禁用态
        // mut：末尾装配段据归属（并入编码栏 / 候选行尾 / 竖排底部）用 take() 转移所有权。
        let mut pager = if self.pager_visible() {
            let disabled = t.color("text_hint", [180, 180, 185, 255]);
            let marker_c = t.color("text_dim", [140, 140, 145, 255]);
            let accent = col(v.accent_bar.bg_color, [66, 133, 244, 255]);
            // 文字箭头启用色：优先 footer_bar.text_color，回退 accent。
            let arrow_on = col(v.footer_bar.text_color, accent);
            let footer_fs = node_fs(&v.footer_bar);
            let prev_on = self.page > 1;
            let next_on = self.page < self.total_pages;
            // 固定矩形触摸区（对齐 Go）：宽 = 字号 + 左右 padding，高 = 候选行高，内容居中。
            // 命中区 = 整个矩形（与图标实际像素范围解耦），悬停在该矩形内即触发圆角高亮。
            let fpad = edges_or(&v.footer_bar.padding, [0.0, 6.0, 0.0, 6.0]);
            let arrow_w = footer_fs + fpad.l + fpad.r;
            // 触摸区高度：独立行=候选行高；并入编码栏(pager_will_inline)=编码文字高(自适应)，
            // 使翻页栏不撑高编码栏（消除有/无翻页栏时的抖动）并与编码在栏内垂直居中。
            let row_h = if pager_will_inline {
                preedit_fs.max(footer_fs)
            } else {
                text_fs + item_pad.t + item_pad.b
            };
            // 翻页箭头：主题配了 prev/next_image（如 _base 的 chevron SVG + tint）则用图标，否则回退文字 ‹ ›。
            let prev_icon = self.arrow_icon(v.footer_bar.prev_image.as_ref(), prev_on);
            let next_icon = self.arrow_icon(v.footer_bar.next_image.as_ref(), next_on);
            // 图标保持主题尺寸（footer_fs 方形），水平居中靠对称内边距撑到 arrow_w；
            // 垂直靠 cross(Center) 居中于 row_h。触摸盒 = arrow_w × row_h，与图标像素范围解耦。
            let icon_pad_x = ((arrow_w - footer_fs) * 0.5).max(0.0);
            let arrow =
                |icon: Option<ViewImage>, txt: &str, tag: i32, enabled: bool, hovered: bool| {
                    let mut node = match icon {
                        Some(vi) => View::container(Layout::Row)
                            .fixed_h(row_h)
                            .pad(Edges::xy(icon_pad_x, 0.0))
                            .child(
                                View::container(Layout::Row)
                                    .fixed_w(footer_fs)
                                    .fixed_h(footer_fs)
                                    .bg_image(vi),
                            ),
                        // 文字箭头启用色：优先 footer_bar.text_color（清风主题设为 text_hint → 细小淡 ‹›），
                        // 未配置则回退 accent（旧主题保持原样）。
                        None => View::leaf(txt, if enabled { arrow_on } else { disabled })
                            .font_size(footer_fs)
                            .fixed_w(arrow_w)
                            .fixed_h(row_h)
                            .text_align(Align::Center),
                    };
                    node = node.radius(item_radius).cross(Align::Center);
                    if enabled {
                        node = node.tag(tag); // 仅启用项参与命中
                        if hovered {
                            node = node.bg(hover_bg); // 圆角悬停高亮覆盖整个按钮矩形
                        }
                    }
                    node
                };
            Some(
                decorate_box(
                    View::container(Layout::Row)
                        .cross(Align::Center)
                        .margin(edges_or(&v.footer_bar.margin, [0.0, 0.0, 0.0, 8.0])),
                    &v.footer_bar,
                )
                .child(arrow(
                    prev_icon,
                    "‹",
                    TAG_PAGE_PREV,
                    prev_on,
                    self.hover == TAG_PAGE_PREV,
                ))
                .child(
                    View::leaf(
                        if self.page_number_visible() {
                            format!("{}/{}", self.page, self.total_pages)
                        } else {
                            String::new()
                        },
                        // 页码颜色用主题 footer_bar.color（如 svgtest 的亮红/暗粉），缺则回退 text_dim。
                        col(v.footer_bar.text_color, marker_c),
                    )
                    .font_size(footer_fs)
                    .font_weight(v.footer_bar.font_weight)
                    .font_family(v.footer_bar.font_family.clone()),
                )
                .child(arrow(
                    next_icon,
                    "›",
                    TAG_PAGE_NEXT,
                    next_on,
                    self.hover == TAG_PAGE_NEXT,
                )),
            )
        } else {
            None
        };

        // ── 装配 ──
        // 翻页栏归属（按优先级三选一）：
        //   1) pager_will_inline → 追加到编码栏 band 右端（随编码区一同装配，swap 时自动沉底贴光标）
        //   2) 横排 → 并入候选行尾（原行为）
        //   3) 竖排 → 候选区底部独立行（原行为）
        if pager_will_inline {
            if let (Some(band), Some(p)) = (preedit_band.take(), pager.take()) {
                preedit_band = Some(band.child(p));
            }
        } else if !self.vertical {
            if let Some(p) = pager.take() {
                list = list.child(p);
            }
        }
        // 竖排未并入的翻页栏：候选区独立行（与 list 同属候选区，随 swap 一起移动）。
        // 独立行按主题 behavior.pager_align（left/center/right，默认 center）水平对齐：包一层
        // fill_cross 的 Row 用 spacer 顶位。inline 情形已在编码栏内右对齐，不经此。
        let vertical_bottom_pager = if self.vertical {
            pager.take().map(|p| {
                let row = View::container(Layout::Row)
                    .fill_cross()
                    .cross(Align::Center);
                match self.theme.behavior.pager_align.as_str() {
                    "left" => row.child(p),
                    "right" => row.child(View::spacer()).child(p),
                    _ => row.child(View::spacer()).child(p).child(View::spacer()),
                }
            })
        } else {
            None
        };

        // 编码区（band + 分隔线）与候选区（list + 竖排底部翻页栏）按 swap_bands 决定上下顺序：
        //   false（正常）：编码区在上、候选区在下 → [band][sep][候选区]
        //   true （上翻）：候选区在上、编码区在下 → [候选区][sep][band]（编码沉底贴光标）
        // 分隔线始终位于编码栏"朝向候选区"的一侧，语义（分隔编码/候选）保持不变。
        if swap_bands {
            // 完整上下镜像：正常 [编码][候选][翻页] → 镜像 [翻页][候选][编码]。
            // 竖排底部翻页栏一并翻到顶部，否则会夹在候选与编码之间，视觉不自然。
            if let Some(p) = vertical_bottom_pager {
                root = root.child(p);
            }
            root = root.child(list);
            if let Some(sep) = preedit_sep {
                root = root.child(sep);
            }
            if let Some(band) = preedit_band {
                root = root.child(band);
            }
        } else {
            if let Some(band) = preedit_band {
                root = root.child(band);
            }
            if let Some(sep) = preedit_sep {
                root = root.child(sep);
            }
            root = root.child(list);
            if let Some(p) = vertical_bottom_pager {
                root = root.child(p);
            }
        }
        root
    }

    pub fn hide(&mut self) {
        self.window.hide();
        self.visible = false;
        self.last_content_pos = None; // 组合结束，下次显示重新落位
        self.placed_above = false; // 清除上方粘滞，下次组合按下方默认重新判定
        self.report_flip_state(); // 粘滞一清，反转随之失效，须立刻让协调器跟上
        {
            let mut m = self.mouse.borrow_mut();
            m.reset_hover();
            // 拖动位置只在"本次组合"内有效：组合结束即失效，下次输入恢复跟随光标。
            // 注意 hide_local_window_only()（host-render 分流）刻意不走这里，落位状态须保留。
            m.reset_drag();
        }
        if let Some(t) = self.tooltip.as_mut() {
            t.hide();
        }
    }

    /// host-render 分流专用：仅隐藏本地 Win32 窗口与 tooltip 窗口，
    /// 不清除 visible / last_content_pos / placed_above 落位状态。
    /// render_frame() 已维护这些状态，host 模式内容确实可见，保持 visible=true 更正确。
    #[cfg(windows)]
    pub fn hide_local_window_only(&mut self) {
        self.window.hide();
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

    /// 当前鼠标悬停项（页内下标；翻页器 tag 或 -1=无）。host 分流写帧时作 rendered_hover_index。
    #[cfg(windows)]
    pub fn hover(&self) -> i32 {
        self.hover
    }

    pub fn candidates(&self) -> &[CandidateItem] {
        &self.candidates
    }

    pub fn hwnd(&self) -> HWND {
        self.window.hwnd()
    }

    /// 将当前渲染帧保存为 PNG 文件（截图用）。
    pub fn capture_to_file(&self, path: &std::path::Path) -> Result<(), String> {
        self.window.capture_to_file(path)
    }

    /// 将当前渲染帧复制到剪贴板（截图用）。
    pub fn capture_to_clipboard(&self) -> Result<(), String> {
        self.window.capture_to_clipboard()
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
    /// 窗口级闸门：是否已"激活"（用户已真实移动鼠标过本窗口）。
    /// 激活后每次 hover 立即响应，不再有逐项延迟。
    engaged: bool,
    /// 首次真实移动后的激活时刻；到期即激活（窗口级一次性防抖）。
    engage_at: Option<Instant>,
    /// 最近一次命中目标（激活瞬间据此发出首个悬停）。
    pending_raw: i32,
    /// 悬停激活延迟（毫秒）。来自 ui.tooltip.delay；默认 60。
    engage_delay_ms: u64,
    /// 本窗口句柄（拖动时 SetCapture / SetWindowPos 用）
    hwnd: HWND,
    /// 是否正在拖动（左键按在空白区并保持）
    dragging: bool,
    /// 拖动起点：光标屏幕坐标
    drag_anchor: (i32, i32),
    /// 拖动起点：窗口左上屏幕坐标
    drag_origin: (i32, i32),
    /// 拖动落定位置（窗口左上屏幕坐标）。`Some` 即"本次组合已被用户手动摆放"，
    /// 此后该组合内的每帧渲染都固定用它，不再跟随光标；`hide()`（组合结束）清空。
    drag_pin: Option<(i32, i32)>,
    /// 当前阴影扩边 (left, top, right, bottom)，每帧渲染后由 `render_frame` 同步。
    /// 窗口左上 + (left, top) = **内容**左上，即落盘用的坐标系；四个分量一起用于
    /// 按内容矩形做拖动钳制。
    margin: (i32, i32, i32, i32),
}

impl CandidateMouse {
    /// 由 UI 循环每轮调用：未激活时检查激活闸门到期，激活瞬间补发当前悬停。
    fn flush(&mut self) {
        if self.engaged {
            return; // 已激活：悬停在 on_message 内即时发出
        }
        if let Some(at) = self.engage_at
            && Instant::now() >= at
        {
            self.engaged = true;
            self.engage_at = None;
            if self.pending_raw != self.last_hover {
                self.last_hover = self.pending_raw;
                let _ = self.events.send(UiEvent::Hover(self.pending_raw));
            }
        }
    }

    /// 重置悬停状态（窗口隐藏 / 新组合）。
    /// 以当前物理光标位作基线，使内容刷新引起的伪移动被门控，仅真实移动才激活。
    fn reset_hover(&mut self) {
        self.last_hover = -1;
        self.engaged = false;
        self.engage_at = None;
        self.pending_raw = -1;
        let (sx, sy) = unsafe {
            let mut p = POINT::default();
            let _ = GetCursorPos(&mut p);
            (p.x, p.y)
        };
        self.last_cursor = (sx, sy);
    }
}

impl CandidateMouse {
    /// 当前窗口左上屏幕坐标；取不到时 None（非 Windows mock 恒 None 语义下的兜底由调用点给）。
    fn window_origin(&self) -> Option<(i32, i32)> {
        let mut r = RECT::default();
        unsafe {
            if GetWindowRect(self.hwnd, &mut r).is_ok() {
                return Some((r.left, r.top));
            }
        }
        None
    }

    /// 当前窗口尺寸（含阴影扩边）；用于拖动时的工作区钳制。
    fn window_size(&self) -> (u32, u32) {
        let mut r = RECT::default();
        unsafe {
            if GetWindowRect(self.hwnd, &mut r).is_ok() {
                return ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
            }
        }
        (0, 0)
    }

    /// 清除拖动落定位置：下次显示恢复跟随光标自动定位。
    fn reset_drag(&mut self) {
        self.dragging = false;
        self.drag_pin = None;
    }

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
                    _ => {
                        // 空白区（编码栏/内边距，非候选非翻页键）→ 起拖，整窗跟随光标。
                        // 与工具栏同构：记录光标与窗口起点，SetCapture 保证移出窗口后仍收到消息。
                        let mut p = POINT::default();
                        unsafe {
                            let _ = GetCursorPos(&mut p);
                        }
                        self.drag_anchor = (p.x, p.y);
                        self.drag_origin = self.window_origin().unwrap_or((p.x, p.y));
                        self.dragging = true;
                        unsafe {
                            SetCapture(self.hwnd);
                        }
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
                    // 以真实窗口位置落定，避免累积误差
                    if let Some(pos) = self.window_origin() {
                        self.drag_pin = Some(pos);
                        // 上报**内容左上**（窗口左上 + 阴影扩边）：固定位置模式下协调器把它
                        // 落盘成 custom_x/custom_y；跟随模式下协调器直接忽略——那里的拖动
                        // 只是"临时挪开"，下次组合仍回到光标旁。
                        let (cx, cy) =
                            CandidateWindow::window_to_content(pos, self.margin.0, self.margin.1);
                        let _ = self
                            .events
                            .send(UiEvent::CandidateWindowMoved { x: cx, y: cy });
                    }
                }
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE if self.dragging => {
                let mut p = POINT::default();
                unsafe {
                    let _ = GetCursorPos(&mut p);
                }
                let nx = self.drag_origin.0 + (p.x - self.drag_anchor.0);
                let ny = self.drag_origin.1 + (p.y - self.drag_anchor.1);
                let (w, h) = self.window_size();
                // 按**内容**矩形钳制、且钳到整块屏幕：软阴影可溢出屏幕，候选窗也允许
                // 摆到任务栏上方。用 clamp_to_work_area 会让内容离屏幕边还差一个阴影
                // 宽（blur=8 的主题约 29px）就拖不动了。
                let (cx, cy) = clamp_content_to_monitor(nx, ny, w, h, self.margin);
                // 拖动中即写 drag_pin：候选内容若在拖动期间刷新，渲染路径才不会把窗口拽回光标处。
                self.drag_pin = Some((cx, cy));
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
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                // 物理移动门控：内容变化（打字换候选/窗口刷新）也会产生 WM_MOUSEMOVE，
                // 但此时物理光标屏幕坐标不变 → 忽略，避免静止鼠标下方候选变化引起闪烁。
                let (sx, sy) = unsafe {
                    let mut p = POINT::default();
                    let _ = GetCursorPos(&mut p);
                    (p.x, p.y)
                };
                if (sx, sy) == self.last_cursor {
                    return Some(LRESULT(0));
                }
                self.last_cursor = (sx, sy);
                let (x, y) = mouse_pos(lparam);
                let raw = self.hit(x, y);
                self.pending_raw = raw;
                if self.engaged {
                    // 已激活：即时高亮/显示 tooltip，无逐项延迟
                    if raw != self.last_hover {
                        self.last_hover = raw;
                        let _ = self.events.send(UiEvent::Hover(raw));
                    }
                } else if self.engage_at.is_none() {
                    // 首次真实移动：启动窗口级激活闸门（仅一次，~60ms）
                    self.engage_at =
                        Some(Instant::now() + Duration::from_millis(self.engage_delay_ms));
                }
                Some(LRESULT(0))
            }
            WM_RBUTTONDOWN => {
                let (x, y) = mouse_pos(lparam);
                let i = self.hit(x, y);
                // 用屏幕光标坐标定位菜单
                let (sx, sy) = unsafe {
                    let mut p = POINT::default();
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
                    // 空白处 → 功能主菜单（光标处向下弹）
                    let _ = self
                        .events
                        .send(UiEvent::RequestMainMenu(MenuAnchor::at_point(sx, sy)));
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

#[cfg(test)]
mod tests {
    use super::{CandidateWindow, visible_whitespace};
    use std::borrow::Cow;

    /// 主题位置偏移的方向语义：正值恒为「远离光标」——下方向下、上方向上。
    ///
    /// 上方若照抄下方的加号，上翻时偏移会把窗口**压向**光标，与配置意图相反。
    /// 这类符号错误从现象很难反推（只有触发上翻的场景才露馅），故直接锁死。
    #[test]
    fn position_offset_always_pushes_away_from_caret() {
        // 下方：正值下移
        assert_eq!(CandidateWindow::apply_offset_y(300, false, 12), 312);
        // 上方：正值上移（同样是远离，不是压向光标）
        assert_eq!(CandidateWindow::apply_offset_y(300, true, 12), 288);
        // 零偏移两侧都是恒等
        assert_eq!(CandidateWindow::apply_offset_y(300, false, 0), 300);
        assert_eq!(CandidateWindow::apply_offset_y(300, true, 0), 300);
        // 负偏移语义对称（把窗口拉近光标）
        assert_eq!(CandidateWindow::apply_offset_y(300, false, -6), 294);
        assert_eq!(CandidateWindow::apply_offset_y(300, true, -6), 306);
    }

    /// **净锚点不含 Y 偏移**——这是「偏移不左右上下翻转决策」的地基。
    ///
    /// `below_ok`/`above_ok` 拿锚点跟工作区边界比：锚点一旦含偏移，off_y 越大两个条件
    /// 越难成立，本该上翻的场景会被判成「上方也放不下」，落回下方再被钳到屏幕底、
    /// 压住光标——表现就是「下方正常、上方遮盖」。故锁死锚点与 off_y 无关。
    #[test]
    fn caret_anchors_are_independent_of_y_offset() {
        let (caret_x, caret_y, caret_h, win_h) = (100, 200, 20, 80);
        let a = CandidateWindow::caret_anchors(caret_x, caret_y, caret_h, win_h, 0);
        // 净锚点即旧行为：下方=光标底端+gap，上方=底边贴光标顶端
        assert_eq!(a.1, caret_y + 2, "下方 = 光标底端 + gap");
        assert_eq!(a.2, caret_y - caret_h - win_h - 2, "上方 = 底边贴光标顶端");
        // x 偏移仍在锚点里（水平方向没有翻转决策，不受影响）
        let b = CandidateWindow::caret_anchors(caret_x, caret_y, caret_h, win_h, 5);
        assert_eq!(b.0 - a.0, 5, "x 偏移右移");
        assert_eq!((b.1, b.2), (a.1, a.2), "x 偏移不该动垂直锚点");
    }

    /// 「上方反转候选」只对竖排成立，横排必须原样保持 1..n。
    ///
    /// 横排候选是左右并列，"反转"跟窗口在光标上方还是下方没有任何关系，只会把读序
    /// 倒过来（1 跑到最右）。两个开关正交，故一并锁死 swap 不受排列方向影响。
    #[test]
    fn flip_when_above_applies_to_vertical_only() {
        // (above, vertical, flip, swap) -> (flip_cands, swap_bands)
        let f = CandidateWindow::above_layout;
        assert_eq!(
            f(true, true, true, false),
            (true, false),
            "竖排上方：反转生效"
        );
        assert_eq!(
            f(true, false, true, false),
            (false, false),
            "横排上方：反转不得生效"
        );
        assert_eq!(
            f(false, true, true, false),
            (false, false),
            "下方：一律不生效"
        );
        // swap 交换的是上下两条带，横竖排都成立
        assert_eq!(
            f(true, false, false, true),
            (false, true),
            "横排上方：交换仍生效"
        );
        assert_eq!(f(true, true, false, true), (false, true));
        // 叠加：竖排两个都开 → 两个都生效；横排只剩 swap
        assert_eq!(f(true, true, true, true), (true, true));
        assert_eq!(f(true, false, true, true), (false, true));
    }

    /// 插入符位置一律夹到合法字符边界——`split_at` 落在字符中间会 panic 掉 UI 线程。
    #[test]
    fn clamp_caret_never_splits_a_char() {
        assert_eq!(CandidateWindow::clamp_caret("nihao", 3), 3);
        assert_eq!(CandidateWindow::clamp_caret("nihao", 5), 5, "末尾合法");
        assert_eq!(CandidateWindow::clamp_caret("nihao", 99), 5, "越界截到末尾");
        assert_eq!(CandidateWindow::clamp_caret("", 4), 0, "空串恒 0");
        // 「你」占 3 字节：落在 1/2 时退回 0，落在 3 是边界
        assert_eq!(CandidateWindow::clamp_caret("你hao", 1), 0);
        assert_eq!(CandidateWindow::clamp_caret("你hao", 2), 0);
        assert_eq!(CandidateWindow::clamp_caret("你hao", 3), 3);
        assert_eq!(CandidateWindow::clamp_caret("你hao", 4), 4);
    }

    /// clamp 后切分恒安全（本测试的意义在于：若 clamp 失效，split_at 会 panic）。
    #[test]
    fn clamped_caret_is_safe_to_split() {
        for text in ["", "nihao", "你好hao", "·ni'hao"] {
            for caret in 0..=text.len() + 2 {
                let c = CandidateWindow::clamp_caret(text, caret);
                let (head, tail) = text.split_at(c);
                assert_eq!(format!("{head}{tail}"), text);
            }
        }
    }

    /// 候选文本的空白控制符须显示为可见符号，且**不得改动其他字符**。
    ///
    /// 这只是显示投影——`Candidate::text` 原文不动，上屏走的仍是真换行（见
    /// [`visible_whitespace`] 的文档）。故本测试只锁「显示成什么样」，
    /// 不能被误读成「上屏文本也变了」。
    #[test]
    fn whitespace_becomes_visible_glyphs() {
        // 无控制符：原样借用，零分配
        assert!(matches!(visible_whitespace("你好"), Cow::Borrowed("你好")));

        assert_eq!(visible_whitespace("甲\n乙"), "甲↵乙");
        assert_eq!(visible_whitespace("丙\t丁"), "丙⇥丁");
        // CRLF 是**一个**换行，只出一个符号
        assert_eq!(visible_whitespace("戊\r\n己"), "戊↵己");
        // 孤立 CR 同样算换行
        assert_eq!(visible_whitespace("庚\r辛"), "庚↵辛");
        // 混合 + 连续
        assert_eq!(visible_whitespace("a\n\tb"), "a↵⇥b");
        // 普通空格不动——它本来就看得见，替换只会平添噪声
        assert_eq!(visible_whitespace("甲 乙"), "甲 乙");
    }

    /// 固定位置的两个坐标系必须严格互逆。
    ///
    /// 落盘的 custom_x/y 是**内容**左上，Win32 定位用的是**窗口**左上（含软阴影扩边）：
    /// 固定定位走 content→window，拖动落定上报走 window→content。若两者不互逆，
    /// 每轮「拖动 → 落盘 → 重新显示」都会多减一次阴影，候选窗每次重显都往左上爬一点——
    /// 这种漂移从现象极难反推，故在此直接锁死。
    #[cfg(windows)]
    #[test]
    fn content_window_coord_roundtrip_is_lossless() {
        // 覆盖有/无阴影、负坐标（副屏在主屏左侧时屏幕坐标为负）
        for (content, ml, mt) in [
            ((100, 200), 12u32, 10u32),
            ((0, 0), 0, 0),
            ((-1920, 40), 8, 8),
            ((3, 7), 1, 0),
        ] {
            let w = CandidateWindow::content_to_window(content, ml, mt);
            let back = CandidateWindow::window_to_content(w, ml as i32, mt as i32);
            assert_eq!(back, content, "content={content:?} margin=({ml},{mt})");
        }
    }

    /// 阴影扩边确实被减掉了——若 content_to_window 退化成恒等函数，
    /// 上面的 round-trip 仍会通过，但固定位置会整体偏移一个阴影的量。
    #[cfg(windows)]
    #[test]
    fn content_to_window_subtracts_the_shadow_margin() {
        assert_eq!(
            CandidateWindow::content_to_window((100, 200), 12, 10),
            (88, 190)
        );
    }
}
