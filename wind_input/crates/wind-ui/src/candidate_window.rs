//! 候选窗口：View 盒模型布局 + DirectWrite 文本 + Win32 Layered Window
//!
//! 与 Go 版本 `wind_input/internal/ui/manager_candidate.go` + `viewbox_build.go` 对齐。
//! 用 `crate::view` 的盒模型构建候选树（预编辑行 + 候选行[序号|文本] + 翻页指示），
//! measure/arrange 算出尺寸与每候选的绝对矩形（供鼠标命中），再 paint 到 BGRA 缓冲区。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Sender;

use crate::manager::{HOVER_PAGE_NEXT as TAG_PAGE_NEXT, HOVER_PAGE_PREV as TAG_PAGE_PREV, UiEvent};
use crate::sys::{
    GetCursorPos, HWND, IDC_ARROW, LPARAM, LRESULT, LoadCursorW, POINT, SetCursor, WM_LBUTTONDOWN,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_SETCURSOR, WPARAM,
};
use crate::text::dwrite::TextRenderer;
use crate::view::{Align, Edges, Layout, Rect, View, ViewImage, ViewLayer};
use crate::window::{LayeredWindow, WindowMouse};
use std::time::{Duration, Instant};

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
    /// 候选窗在光标上方时反转候选顺序。来自 ui.candidate.flip_when_above。
    flip_when_above: bool,
    /// 当前锚点是否落在光标上方（定位时计算，随锚点锁定保持）。供 flip 判定。
    placed_above: bool,
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
            engaged: false,
            engage_at: None,
            pending_raw: -1,
            engage_delay_ms: 60,
        }));
        window.register_mouse(mouse.clone());
        Ok(Self {
            window,
            config,
            candidates: Vec::new(),
            preedit: String::new(),
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
            anchor: None,
            anchor_locked: false,
            text_renderer,
            hit_rects: Vec::new(),
            mouse,
            tooltip: crate::tooltip::Tooltip::new().ok(),
            theme: wind_theme::Resolved::default(),
            scale: CandidateWindowConfig::get_dpi_scale(),
            vertical: false,
            preedit_embedded: false,
            font_size_override: 0.0,
            flip_when_above: false,
            placed_above: false,
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

    /// 设置"上方时反转候选顺序"。来自 ui.candidate.flip_when_above。
    pub fn set_flip_when_above(&mut self, flip: bool) {
        self.flip_when_above = flip;
    }

    /// 设置悬停提示激活延迟（毫秒）。来自 ui.tooltip.delay。
    pub fn set_tooltip_delay(&mut self, delay_ms: i32) {
        self.mouse.borrow_mut().engage_delay_ms = delay_ms.max(0) as u64;
    }

    /// 设置拆字字根字体路径（PUA 字根字符渲染）。转发给悬停提示窗。
    pub fn set_tooltip_chaizi_font(&mut self, path: &str) {
        if let Some(t) = self.tooltip.as_mut() {
            t.set_chaizi_font(path);
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
        mode_label: &str,
        candidates: Vec<CandidateItem>,
        selected: usize,
        hover: i32,
        page: usize,
        total_pages: usize,
    ) {
        self.preedit = preedit.to_string();
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

    /// 候选页内命中矩形（绝对坐标，相对窗口左上角）
    pub fn hit_rects(&self) -> &[(i32, Rect)] {
        &self.hit_rects
    }

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
        let content_w = (w_f.ceil() as u32).max(40);
        let content_h = (h_f.ceil() as u32).max(24);
        let width = content_w + ml + mr;
        let height = content_h + mt + mb;
        let t_layout = t_layout0.elapsed();

        // 定位提前到 paint 前（供 flip_when_above 判定）：组合期锚点锁定后复用，
        // 仅首次/重锚时按工作区钳制并记录是否上翻（placed_above）。
        let keep = self.visible && self.anchor_locked && self.anchor.is_some();
        let (px, py) = if keep {
            self.anchor.unwrap()
        } else {
            let (x, y, above) =
                Self::clamp_to_work_area(self.x, self.y, self.caret_height, content_w, content_h);
            self.anchor = Some((x, y));
            self.anchor_locked = self.caret_valid; // 仅有效坐标才锁定
            self.placed_above = above;
            (x, y)
        };
        // 上方显示且启用 → 逆序重建候选树（项数/尺寸/位置不变，仅排列翻转）
        if self.flip_when_above && self.placed_above {
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
        self.update_tooltip(px, py);
        let t_tip = t_tip0.elapsed();
        if t_tip.as_micros() > 200 {
            tracing::debug!("render tooltip={:?}", t_tip);
        }
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
                Some((code, r)) => tip.show(&code, px + r.x as i32, py + (r.y + r.h) as i32 + 2),
                None => tip.hide(),
            }
        }
    }

    /// 将候选窗钳制在光标所在显示器的工作区内：
    /// 默认显示在光标下方（+gap）；下方空间不足则上翻到光标上方；
    /// 左右溢出则贴边。避免窗口跑到屏幕外。
    // 非 Windows 下窗口钳制为空实现，caret_h/w/h 及 x/y 的可变性仅 Windows 分支需要。
    /// 返回 (x, y, above)：above=true 表示窗口被上翻到光标上方（供 flip_when_above 判定）。
    #[cfg_attr(not(windows), allow(unused_variables, unused_mut))]
    fn clamp_to_work_area(
        caret_x: i32,
        caret_y: i32,
        caret_h: i32,
        w: u32,
        h: u32,
    ) -> (i32, i32, bool) {
        let gap = 2;
        // caret_y 为光标底端（与 Go 一致）：默认显示在其下方，仅留 gap
        let (mut x, mut y) = (caret_x, caret_y + gap);
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
                    let (wi, hi) = (w as i32, h as i32);
                    // 下方放不下 → 上翻到光标上方（光标顶端 = caret_y - caret_h）
                    if y + hi > wa.bottom {
                        let above_y = caret_y - caret_h.max(0) - hi - gap;
                        if above_y >= wa.top {
                            y = above_y;
                            above = true;
                        } else {
                            y = wa.bottom - hi;
                        }
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

    /// 按当前状态构建候选视图树（横向布局）。
    /// T3：从 RVNode 树（`Resolved.views`）取色/几何，颜色 None→兜底（与旧 ResolvedTheme 默认等值，零回归）。
    /// 构建候选 View 树。`flip=true` 时候选按相反顺序排列（上方显示场景），
    /// 但每项的 tag/序号标签/选中判定仍用原始索引 i，确保命中/选中映射不变。
    fn build_tree(&self, flip: bool) -> View {
        use wind_theme::rvnode::{RvEdges, RvNode};
        use wind_theme::schema::Dim;
        let t = &self.theme;
        let v = &t.views;
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
        // 窗口 z 层覆盖图（如 jidian 右下角 mark 水印）。
        let win_layers = self.rv_layers(&v.window.layers);
        if !win_layers.is_empty() {
            root = root.layers(win_layers);
        }

        // 预编辑行（主题背景带 + 文本色）。完整渲染 preedit_bar 自身背景：底色 + 位图背景 +
        // z 层 + 边框（此前只画底色，位图主题的 preedit 背景/边框丢失）。
        // 嵌入模式（preedit_embedded）下不画独立条——编码作为候选行首单元内联（见下方 list 构建）。
        if !self.preedit.is_empty() && !self.preedit_embedded {
            let mut band = View::container(Layout::Row)
                .bg(col(v.preedit_bar.bg_color, [240, 240, 240, 255]))
                .radius(dim(v.item.border_radius, 4.0))
                .pad(edges_or(&v.preedit_bar.padding, [3.0, 8.0, 3.0, 8.0]))
                .margin(edges_or(&v.preedit_bar.margin, [0.0; 4]))
                .child(
                    View::leaf(
                        self.preedit.clone(),
                        col(v.preedit_bar.text_color, [100, 100, 100, 255]),
                    )
                    .font_size(preedit_fs),
                );
            if let Some(vi) = self.rv_image(v.preedit_bar.bg_image.as_ref()) {
                band = band.bg_image(vi);
            }
            let band_layers = self.rv_layers(&v.preedit_bar.layers);
            if !band_layers.is_empty() {
                band = band.layers(band_layers);
            }
            if let Some(bc) = v.preedit_bar.border_color {
                band = band.border(bc, dim(v.preedit_bar.border_width, 0.0).max(1.0));
            }
            // 模式标记：单行 preedit 栏模式下右对齐到栏行末尾。
            // band 跨轴撑满窗口内容宽 + spacer 吸收中间空白 → 标记贴右；字号取主题 mode_label 配置。
            if !self.mode_label.is_empty() {
                let ml_fs = node_fs(&v.mode_label);
                let mut chip = View::leaf(
                    self.mode_label.clone(),
                    col(v.mode_label.text_color, [120, 120, 128, 255]),
                )
                .font_size(ml_fs)
                .margin(Edges {
                    l: 12.0 * s,
                    ..Edges::default()
                });
                if let Some(bg) = v.mode_label.bg_color {
                    chip = chip
                        .bg(bg)
                        .radius(dim(v.item.border_radius, 4.0))
                        .pad(edges_or(&v.mode_label.padding, [1.0, 6.0, 1.0, 6.0]));
                }
                band = band.fill_cross().child(View::spacer()).child(chip);
            }
            root = root.child(band);
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
        let accent_bar = v.accent_bar_enabled.then(|| {
            (
                col(v.accent_bar.bg_color, [66, 133, 244, 255]),
                dim(v.accent_bar_width, 3.0),
            )
        });

        // 候选列表：横排=Row（cell 并列）；竖排=Column（候选纵向堆叠）。
        let mut list = if self.vertical {
            View::container(Layout::Column).gap(row_gap_v)
        } else {
            View::container(Layout::Row)
                .gap(box_gap)
                .cross(Align::Center)
        };
        // 嵌入模式：编码作为候选行首单元内联（无独立背景，与候选间留白分隔），对齐 Go buildEmbeddedPreedit。
        // 横排右留白、竖排下留白。
        if self.preedit_embedded && !self.preedit.is_empty() {
            let sep = if self.vertical {
                Edges {
                    b: 6.0 * s,
                    ..Edges::default()
                }
            } else {
                Edges {
                    r: 16.0 * s,
                    ..Edges::default()
                }
            };
            list = list.child(
                View::leaf(
                    self.preedit.clone(),
                    col(v.preedit_bar.text_color, [100, 100, 100, 255]),
                )
                .font_size(preedit_fs)
                .margin(sep),
            );
        }
        // 模式标记（拼/双/快/英/符 或全称）：紧随输入缓冲之后、候选之前内联显示。
        // 仅"无独立 preedit 栏"时在此（内联编码 candidate_inline / 内嵌应用 app_inline /
        // 栏模式但暂无 preedit）；有 preedit 栏时已置于栏行最后（见上）。
        // 颜色/背景由主题 mode_label 节点配置，与普通候选区分。横排右留白、竖排下留白。
        let preedit_bar_shown = !self.preedit.is_empty() && !self.preedit_embedded;
        if !self.mode_label.is_empty() && !preedit_bar_shown {
            let ml_fs = node_fs(&v.mode_label);
            let sep = if self.vertical {
                Edges {
                    b: 6.0 * s,
                    ..Edges::default()
                }
            } else {
                Edges {
                    r: 12.0 * s,
                    ..Edges::default()
                }
            };
            let mut chip = View::leaf(
                self.mode_label.clone(),
                col(v.mode_label.text_color, [120, 120, 128, 255]),
            )
            .font_size(ml_fs)
            .margin(sep);
            // 主题为 mode_label 配了底色 → 渲染为小徽标（圆角 + 内边距）以更醒目。
            if let Some(bg) = v.mode_label.bg_color {
                chip = chip
                    .bg(bg)
                    .radius(dim(v.item.border_radius, 4.0))
                    .pad(edges_or(&v.mode_label.padding, [1.0, 6.0, 1.0, 6.0]));
            }
            list = list.child(chip);
        }
        // 逆序仅改排列顺序；i 仍是原始索引（tag/标签/选中据此）
        let mut order: Vec<(usize, &CandidateItem)> = self.candidates.iter().enumerate().collect();
        if flip {
            order.reverse();
        }
        for (i, cand) in order {
            let marker = if cand.label.is_empty() {
                (i + 1).to_string()
            } else {
                cand.label.clone()
            };
            let is_sel = i == self.selected;
            let is_hover = self.hover >= 0 && self.hover as usize == i;
            // 状态文字色（选中/悬停各自的色，回退基态）。
            let txt_color = eff_text(&v.text, text_color, is_sel, is_hover);
            let idx_color = eff_text(&v.index, index_color, is_sel, is_hover);
            let cmt_color = eff_text(&v.comment, comment_color, is_sel, is_hover);

            // 序号：圆圈样式 → 方形节点 + 真圆背景 + 居中数字。
            let mut idx_leaf = View::leaf(marker, idx_color)
                .font_size(index_fs)
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

            // 行内间距改由各子节点 margin 承载（与前端盒模型一致）：text.margin.left 默认 4dp
            // 作为序号↔文字间距；不再用容器 gap 借 text.margin.left。
            let mut item = View::container(Layout::Row)
                .cross(Align::Center)
                .pad(item_pad)
                .margin(edges_or(&v.item.margin, [0.0; 4]))
                .radius(item_radius)
                .tag(i as i32)
                .child(idx_leaf)
                .child(
                    View::leaf(cand.text.clone(), txt_color)
                        .font_size(text_fs)
                        .pad(edges_or(&v.text.padding, [0.0; 4]))
                        .margin(edges_or(&v.text.margin, [0.0, 0.0, 0.0, 4.0])),
                );
            // 注释（编码后缀/短语提示）：非空时在候选词右侧以注释样式内联显示。
            // 内/外边距完整消费：comment.padding 四边 + comment.margin 四边（左默认 6dp 兜底间距）。
            if !cand.comment.is_empty() {
                item = item.child(
                    View::leaf(cand.comment.clone(), cmt_color)
                        .font_size(comment_fs)
                        .pad(edges_or(&v.comment.padding, [0.0; 4]))
                        .margin(edges_or(&v.comment.margin, [0.0, 0.0, 0.0, 6.0])),
                );
            }
            // 选中底色优先于悬停底色（两者独立：选中=空格上屏目标，悬停=鼠标提示）
            if is_sel {
                item = item.bg(sel_bg);
                if let Some((c, w)) = accent_bar {
                    item = item.left_bar(c, w);
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
            list = list.child(item);
        }

        // 翻页器（多页时）：‹ p/t › —— 箭头可点击翻页，带悬停高亮 + 禁用态
        let pager = if self.total_pages > 1 {
            let disabled = t.color("text_hint", [180, 180, 185, 255]);
            let marker_c = t.color("text_dim", [140, 140, 145, 255]);
            let accent = col(v.accent_bar.bg_color, [66, 133, 244, 255]);
            let footer_fs = node_fs(&v.footer_bar);
            let prev_on = self.page > 1;
            let next_on = self.page < self.total_pages;
            // 固定矩形触摸区（对齐 Go）：宽 = 字号 + 左右 padding，高 = 候选行高，内容居中。
            // 命中区 = 整个矩形（与图标实际像素范围解耦），悬停在该矩形内即触发圆角高亮。
            let fpad = edges_or(&v.footer_bar.padding, [0.0, 6.0, 0.0, 6.0]);
            let arrow_w = footer_fs + fpad.l + fpad.r;
            let row_h = text_fs + item_pad.t + item_pad.b;
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
                        None => View::leaf(txt, if enabled { accent } else { disabled })
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
                View::container(Layout::Row)
                    .cross(Align::Center)
                    .margin(edges_or(&v.footer_bar.margin, [0.0, 0.0, 0.0, 8.0]))
                    .child(arrow(
                        prev_icon,
                        "‹",
                        TAG_PAGE_PREV,
                        prev_on,
                        self.hover == TAG_PAGE_PREV,
                    ))
                    .child(
                        View::leaf(format!("{}/{}", self.page, self.total_pages), marker_c)
                            .font_size(footer_fs),
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

        // 装配：横排把翻页器并入候选行尾；竖排把候选列表 + 翻页器纵向堆入窗口。
        if self.vertical {
            root = root.child(list);
            if let Some(p) = pager {
                root = root.child(p);
            }
        } else {
            if let Some(p) = pager {
                list = list.child(p);
            }
            root = root.child(list);
        }
        root
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

    pub fn hwnd(&self) -> HWND {
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
    /// 窗口级闸门：是否已"激活"（用户已真实移动鼠标过本窗口）。
    /// 激活后每次 hover 立即响应，不再有逐项延迟。
    engaged: bool,
    /// 首次真实移动后的激活时刻；到期即激活（窗口级一次性防抖）。
    engage_at: Option<Instant>,
    /// 最近一次命中目标（激活瞬间据此发出首个悬停）。
    pending_raw: i32,
    /// 悬停激活延迟（毫秒）。来自 ui.tooltip.delay；默认 60。
    engage_delay_ms: u64,
}

impl CandidateMouse {
    /// 由 UI 循环每轮调用：未激活时检查激活闸门到期，激活瞬间补发当前悬停。
    fn flush(&mut self) {
        if self.engaged {
            return; // 已激活：悬停在 on_message 内即时发出
        }
        if let Some(at) = self.engage_at {
            if Instant::now() >= at {
                self.engaged = true;
                self.engage_at = None;
                if self.pending_raw != self.last_hover {
                    self.last_hover = self.pending_raw;
                    let _ = self.events.send(UiEvent::Hover(self.pending_raw));
                }
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
