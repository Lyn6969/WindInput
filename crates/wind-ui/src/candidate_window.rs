//! 候选窗口：tiny-skia 渲染 + Win32 Layered Window
//!
//! 与 Go 版本 `wind_input/internal/ui/candidate_window.go` 对齐。
//! 使用 tiny-skia 在 BGRA 缓冲区上绘制，然后通过 UpdateLayeredWindow 刷新到屏幕。

use crate::window::LayeredWindow;
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
        Self {
            font_size: 16.0,
            per_page: 5,
            bg_color: [255, 255, 255, 245],
            text_color: [51, 51, 51, 255],
            highlight_color: [0, 120, 215, 255],
            border_color: [200, 200, 200, 200],
            selected_bg: [230, 240, 255, 255],
            padding_x: 12.0,
            padding_y: 8.0,
            item_spacing: 4.0,
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
}

impl CandidateWindow {
    pub fn new(config: CandidateWindowConfig) -> Result<Self, String> {
        let window = LayeredWindow::create(None, 400, 200, "WindInputCandidate")?;
        Ok(Self {
            window,
            config,
            candidates: Vec::new(),
            preedit: String::new(),
            selected: 0,
            visible: false,
            x: 0,
            y: 0,
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

    fn calculate_size(&self) -> (u32, u32) {
        let item_height = self.config.font_size + self.config.item_spacing;
        let num_items = self.candidates.len().min(self.config.per_page);

        let preedit_height = if self.preedit.is_empty() {
            0.0
        } else {
            item_height
        };

        let max_text_len = self.candidates.iter()
            .take(self.config.per_page)
            .map(|c| c.text.len())
            .max()
            .unwrap_or(4);

        let char_width = self.config.font_size * 0.6;
        let index_width = self.config.font_size * 1.5;
        let estimated_text_width = max_text_len as f32 * char_width;
        let width = (index_width + estimated_text_width + self.config.padding_x * 2.0).max(80.0);

        let height = preedit_height
            + num_items as f32 * item_height
            + self.config.padding_y * 2.0;

        (width as u32, height.max(30.0) as u32)
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
        let mut cy = config.padding_y as usize;
        let item_height = (config.font_size + config.item_spacing) as usize;

        if !preedit.is_empty() {
            Self::draw_text_static(buf, w, h, preedit, config.padding_x as usize, cy, config.font_size, config.highlight_color);
            cy += item_height;
        }

        // 绘制候选列表
        for (i, candidate) in candidates.iter().take(config.per_page).enumerate() {
            let is_selected = i == selected;

            // 选中项背景
            if is_selected {
                let sel_bg = config.selected_bg;
                for dy in 0..item_height {
                    for dx in 4..w.saturating_sub(4) {
                        let py = cy + dy;
                        let idx = (py * w + dx) * 4;
                        if idx + 3 < buf.len() {
                            buf[idx] = sel_bg[0];
                            buf[idx + 1] = sel_bg[1];
                            buf[idx + 2] = sel_bg[2];
                            buf[idx + 3] = sel_bg[3];
                        }
                    }
                }
            }

            // 序号
            let index_text = format!("{}.", i + 1);
            let index_color = if is_selected {
                config.highlight_color
            } else {
                [150, 150, 150, 255]
            };
            Self::draw_text_static(buf, w, h, &index_text, config.padding_x as usize, cy, config.font_size, index_color);

            // 候选文本
            let text_x = config.padding_x as usize + (config.font_size * 1.5) as usize;
            let text_color = if is_selected {
                config.highlight_color
            } else {
                config.text_color
            };
            Self::draw_text_static(buf, w, h, &candidate.text, text_x, cy, config.font_size, text_color);

            cy += item_height;
        }
    }

    /// 简单文本绘制（每个字符用彩色矩形占位）
    ///
    /// TODO: 替换为真正的字体渲染（DirectWrite/FreeType）
    fn draw_text_static(buf: &mut [u8], w: usize, h: usize, text: &str, x: usize, y: usize, font_size: f32, color: [u8; 4]) {
        let font_h = font_size as usize;
        let char_w = (font_size * 0.55) as usize;
        let mut cx = x;

        for ch in text.chars() {
            let cw = if ch.is_ascii() { char_w } else { (font_size * 0.9) as usize };

            for dy in 0..font_h.min(h.saturating_sub(y)) {
                for dx in 0..cw.min(w.saturating_sub(cx)) {
                    let py = y + dy;
                    let px = cx + dx;
                    if py < h && px < w {
                        let idx = (py * w + px) * 4;
                        if idx + 3 < buf.len() {
                            buf[idx] = color[0];
                            buf[idx + 1] = color[1];
                            buf[idx + 2] = color[2];
                            buf[idx + 3] = color[3];
                        }
                    }
                }
            }

            cx += cw + 1;
            if cx >= w { break; }
        }
    }

    pub fn candidates(&self) -> &[CandidateItem] {
        &self.candidates
    }

    pub fn hwnd(&self) -> windows::Win32::Foundation::HWND {
        self.window.hwnd()
    }
}
