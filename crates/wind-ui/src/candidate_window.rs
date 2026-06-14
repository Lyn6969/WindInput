//! 候选窗口：tiny-skia 渲染 + Win32 Layered Window
//!
//! 与 Go 版本 `wind_input/internal/ui/candidate_window.go` 对齐。
//! 使用 tiny-skia 在 BGRA 缓冲区上绘制，然后通过 UpdateLayeredWindow 刷新到屏幕。

use crate::window::LayeredWindow;
use crate::text::dwrite::TextRenderer;
use tracing::{debug, info};

/// 候选词数据
#[derive(Debug, Clone)]
pub struct CandidateItem {
    pub text: String,
    pub code: String,
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
        // 获取系统 DPI 缩放因子
        let dpi_scale = Self::get_dpi_scale();
        // 基础字体大小 24pt，按 DPI 缩放
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
    /// 获取系统 DPI 缩放因子（1.0 = 96 DPI, 1.5 = 144 DPI, 2.0 = 192 DPI）
    fn get_dpi_scale() -> f32 {
        #[cfg(windows)]
        {
            use windows::Win32::Graphics::Gdi::*;
            use windows::Win32::Foundation::HWND;
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
    visible: bool,
    x: i32,
    y: i32,
    /// 文本渲染器
    text_renderer: TextRenderer,
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
            visible: false,
            x: 0,
            y: 0,
            text_renderer,
        })
    }

    pub fn update(&mut self, preedit: &str, candidates: Vec<CandidateItem>, selected: usize) {
        self.preedit = preedit.to_string();
        self.candidates = candidates;
        self.selected = selected;
    }

    pub fn set_position(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
    }

    pub fn show(&mut self) {
        if self.candidates.is_empty() && self.preedit.is_empty() {
            self.hide();
            return;
        }

        let (width, height) = self.calculate_size();
        self.window.resize(width, height);

        tracing::debug!(
            "CandidateWindow::show pos=({},{}), size=({},{}), candidates={}",
            self.x, self.y, width, height, self.candidates.len()
        );

        // 渲染到临时缓冲区
        let buf_size = (width * height * 4) as usize;
        let mut render_buf = vec![0u8; buf_size];

        Self::draw_content_static(
            &mut render_buf,
            width,
            height,
            &self.config,
            &self.preedit,
            &self.candidates,
            self.selected,
            &self.text_renderer,
        );

        // 复制到窗口缓冲区
        self.window.buffer_mut()[..buf_size].copy_from_slice(&render_buf[..buf_size]);

        if let Err(e) = self.window.update() {
            tracing::warn!("CandidateWindow update failed: {}", e);
        }

        self.window.show(self.x, self.y + 20);
        self.visible = true;
    }

    pub fn hide(&mut self) {
        self.window.hide();
        self.visible = false;
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// 候选间距（横向布局中相邻候选 cell 的间隔）
    const CELL_GAP: f32 = 14.0;

    /// 计算横向候选布局：返回每个候选的 (显示标签 "N.文本", 起始 x, cell 宽度)。
    /// 静态以便 calculate_size 与 draw 复用，保证两者尺寸一致。
    /// 注：协调器已按 per_page 切片为单页，此处渲染收到的全部候选。
    fn layout_cells(
        candidates: &[CandidateItem],
        padding_x: f32,
        renderer: &TextRenderer,
    ) -> (Vec<(String, f32, f32)>, f32) {
        let mut cells = Vec::new();
        let mut x = padding_x;
        for (i, c) in candidates.iter().enumerate() {
            let label = format!("{}.{}", i + 1, c.text);
            let w = renderer.measure_text(&label).width;
            cells.push((label, x, w));
            x += w + Self::CELL_GAP;
        }
        // 内容宽度 = 最后一个 cell 右边界（去掉多余 gap）+ 右 padding
        let content_width = if cells.is_empty() {
            padding_x * 2.0
        } else {
            x - Self::CELL_GAP + padding_x
        };
        (cells, content_width)
    }

    fn calculate_size(&self) -> (u32, u32) {
        let line_height =
            self.text_renderer.measure_text("国").height + self.config.item_spacing;

        let preedit_height = if self.preedit.is_empty() { 0.0 } else { line_height };
        let cand_height = if self.candidates.is_empty() { 0.0 } else { line_height };

        let (_, cand_width) = Self::layout_cells(
            &self.candidates,
            self.config.padding_x,
            &self.text_renderer,
        );

        // preedit 行宽（含左右 padding），与候选行宽取最大
        let preedit_width = if self.preedit.is_empty() {
            0.0
        } else {
            self.text_renderer.measure_text(&self.preedit).width + self.config.padding_x * 2.0
        };

        let width = cand_width.max(preedit_width).max(80.0);
        let height = preedit_height + cand_height + self.config.padding_y * 2.0;

        (width.ceil() as u32, height.max(28.0).ceil() as u32)
    }

    /// 静态渲染函数，避免借用冲突
    fn draw_content_static(
        buf: &mut [u8],
        width: u32,
        height: u32,
        config: &CandidateWindowConfig,
        preedit: &str,
        candidates: &[CandidateItem],
        selected: usize,
        text_renderer: &TextRenderer,
    ) {
        let w = width as usize;
        let h = height as usize;

        // 绘制背景
        let bg = config.bg_color;
        for y in 0..h {
            for x in 0..w {
                let idx = (y * w + x) * 4;
                if idx + 3 < buf.len() {
                    buf[idx] = bg[0];
                    buf[idx + 1] = bg[1];
                    buf[idx + 2] = bg[2];
                    buf[idx + 3] = bg[3];
                }
            }
        }

        // 绘制边框
        let border = config.border_color;
        for x in 0..w {
            let idx_top = x * 4;
            let idx_bot = ((h - 1) * w + x) * 4;
            for i in 0..4 {
                if idx_top + i < buf.len() { buf[idx_top + i] = border[i]; }
                if idx_bot + i < buf.len() { buf[idx_bot + i] = border[i]; }
            }
        }
        for y in 0..h {
            let idx_left = y * w * 4;
            let idx_right = (y * w + w - 1) * 4;
            for i in 0..4 {
                if idx_left + i < buf.len() { buf[idx_left + i] = border[i]; }
                if idx_right + i < buf.len() { buf[idx_right + i] = border[i]; }
            }
        }

        // 绘制预编辑文本
        let mut cy = config.padding_y;
        let sample_metrics = text_renderer.measure_text("国");
        let item_height = sample_metrics.height + config.item_spacing;

        if !preedit.is_empty() {
            Self::draw_text_static(buf, w, h, preedit, config.padding_x, cy, text_renderer, config.highlight_color);
            cy += item_height;
            // preedit 与候选之间的分隔线
            let sep_y = (cy - config.item_spacing * 0.5) as usize;
            let sep = config.border_color;
            if sep_y < h {
                for dx in 4..w.saturating_sub(4) {
                    let idx = (sep_y * w + dx) * 4;
                    if idx + 3 < buf.len() {
                        buf[idx] = sep[0];
                        buf[idx + 1] = sep[1];
                        buf[idx + 2] = sep[2];
                        buf[idx + 3] = sep[3];
                    }
                }
            }
        }

        // 绘制候选列表（横向：1.你好  2.尼号  3...）
        let (cells, _) = Self::layout_cells(candidates, config.padding_x, text_renderer);
        let row_top = cy;
        for (i, (label, cell_x, cell_w)) in cells.iter().enumerate() {
            let is_selected = i == selected;

            // 选中项背景块（覆盖该 cell，上下留 2px）
            if is_selected {
                let sel_bg = config.selected_bg;
                let pad = 6.0; // cell 背景左右内边距
                let x0 = (cell_x - pad).max(2.0) as usize;
                let x1 = (cell_x + cell_w + pad).min(w as f32 - 2.0) as usize;
                let y0 = row_top as usize;
                let y1 = (row_top + item_height).min(h as f32) as usize;
                for dy in y0..y1 {
                    for dx in x0..x1 {
                        let idx = (dy * w + dx) * 4;
                        if idx + 3 < buf.len() {
                            buf[idx] = sel_bg[0];
                            buf[idx + 1] = sel_bg[1];
                            buf[idx + 2] = sel_bg[2];
                            buf[idx + 3] = sel_bg[3];
                        }
                    }
                }
            }

            let color = if is_selected {
                config.highlight_color
            } else {
                config.text_color
            };
            Self::draw_text_static(buf, w, h, label, *cell_x, row_top, text_renderer, color);
        }
    }

    /// 绘制文本（使用 TextRenderer 渲染真实文字）
    fn draw_text_static(
        buf: &mut [u8],
        w: usize,
        h: usize,
        text: &str,
        x: f32,
        y: f32,
        renderer: &TextRenderer,
        color: [u8; 4],
    ) {
        if let Err(e) = renderer.draw_text(buf, w as u32, h as u32, x, y, text, color) {
            tracing::warn!("draw_text failed: {}", e);
        }
    }

    pub fn candidates(&self) -> &[CandidateItem] {
        &self.candidates
    }

    pub fn hwnd(&self) -> windows::Win32::Foundation::HWND {
        self.window.hwnd()
    }
}
