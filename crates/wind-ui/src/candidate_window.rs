//! 候选窗口：View 盒模型布局 + DirectWrite 文本 + Win32 Layered Window
//!
//! 与 Go 版本 `wind_input/internal/ui/manager_candidate.go` + `viewbox_build.go` 对齐。
//! 用 `crate::view` 的盒模型构建候选树（预编辑行 + 候选行[序号|文本] + 翻页指示），
//! measure/arrange 算出尺寸与每候选的绝对矩形（供鼠标命中），再 paint 到 BGRA 缓冲区。

use crate::text::dwrite::TextRenderer;
use crate::view::{Align, Edges, Layout, Rect, View};
use crate::window::LayeredWindow;

/// 候选词数据
#[derive(Debug, Clone)]
pub struct CandidateItem {
    pub text: String,
    pub code: String,
    /// 序号标签（如 "1" / "a"）；空则按位置自动用数字编号
    pub label: String,
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

    fn get_dpi_scale() -> f32 {
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
    page: usize,
    total_pages: usize,
    visible: bool,
    x: i32,
    y: i32,
    text_renderer: TextRenderer,
    /// arrange 后收集的候选命中矩形：(候选页内下标, 矩形)，供鼠标层使用
    hit_rects: Vec<(i32, Rect)>,
}

impl CandidateWindow {
    pub fn new(config: CandidateWindowConfig) -> Result<Self, String> {
        let window = LayeredWindow::create(None, 400, 200, "WindInputCandidate")?;
        let text_renderer = TextRenderer::new("Microsoft YaHei UI", config.font_size)?;
        Ok(Self {
            window,
            config,
            candidates: Vec::new(),
            preedit: String::new(),
            selected: 0,
            page: 1,
            total_pages: 1,
            visible: false,
            x: 0,
            y: 0,
            text_renderer,
            hit_rects: Vec::new(),
        })
    }

    pub fn update(
        &mut self,
        preedit: &str,
        candidates: Vec<CandidateItem>,
        selected: usize,
        page: usize,
        total_pages: usize,
    ) {
        self.preedit = preedit.to_string();
        self.candidates = candidates;
        self.selected = selected;
        self.page = page.max(1);
        self.total_pages = total_pages.max(1);
    }

    pub fn set_position(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
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

        // 收集候选命中矩形
        self.hit_rects.clear();
        root.collect_hits(&mut self.hit_rects);

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

        self.window.show(self.x, self.y + 20);
        self.visible = true;
    }

    /// 按当前状态构建候选视图树（横向布局）
    fn build_tree(&self) -> View {
        let c = &self.config;
        let s = c.item_spacing.max(2.0);

        let mut root = View::container(Layout::Column)
            .bg(c.bg_color)
            .border(c.border_color, 1.0)
            .radius(c.font_size * 0.25)
            .pad(Edges::xy(c.padding_x, c.padding_y))
            .gap(s);

        // 预编辑行
        if !self.preedit.is_empty() {
            root = root.child(
                View::container(Layout::Row)
                    .child(View::leaf(self.preedit.clone(), c.highlight_color)),
            );
        }

        // 候选行：[序号 文本] cell 横排
        let mut row = View::container(Layout::Row).gap(s * 2.0).cross(Align::Center);
        let item_pad = Edges::xy(s * 1.5, s * 0.5);
        for (i, cand) in self.candidates.iter().enumerate() {
            let marker = if cand.label.is_empty() {
                (i + 1).to_string()
            } else {
                cand.label.clone()
            };
            let is_sel = i == self.selected;
            let txt_color = if is_sel { c.highlight_color } else { c.text_color };

            let mut item = View::container(Layout::Row)
                .cross(Align::Center)
                .gap(s * 0.5)
                .pad(item_pad)
                .radius(c.font_size * 0.18)
                .tag(i as i32)
                .child(View::leaf(marker, c.marker_color()))
                .child(View::leaf(cand.text.clone(), txt_color));
            if is_sel {
                item = item.bg(c.selected_bg);
            }
            row = row.child(item);
        }

        // 翻页指示（多页时）
        if self.total_pages > 1 {
            row = row.child(
                View::leaf(format!("{}/{}", self.page, self.total_pages), c.marker_color())
                    .margin(Edges::xy(s, 0.0)),
            );
        }

        root.child(row)
    }

    pub fn hide(&mut self) {
        self.window.hide();
        self.visible = false;
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
