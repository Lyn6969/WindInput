//! 工具栏窗口：常驻状态指示器（中英 / 方案 / 标点 / 全半角）。
//!
//! 与 Go 版本 `wind_input/internal/ui/toolbar_window.go` 对齐（简化版）。
//! 横向圆角小条，每格一个状态；中文模式格高亮。固定显示于工作区右下角。
//! 点击切换暂未实现（后续 UI 统一优化阶段补齐拖动 + 命中），当前为展示用。

use crate::text::dwrite::TextRenderer;
use crate::window::LayeredWindow;

/// 工具栏状态（由协调器推送）
#[derive(Debug, Clone)]
pub struct ToolbarState {
    pub chinese_mode: bool,
    /// 方案友好名（如 "五笔" / "拼音"）
    pub schema_label: String,
    pub full_width: bool,
    pub chinese_punct: bool,
}

impl Default for ToolbarState {
    fn default() -> Self {
        Self {
            chinese_mode: true,
            schema_label: "五笔".to_string(),
            full_width: false,
            chinese_punct: true,
        }
    }
}

/// 一个单元格：文本 + 是否高亮（中文模式格）
struct Cell {
    text: String,
    highlight: bool,
}

/// 工具栏窗口
pub struct Toolbar {
    window: LayeredWindow,
    renderer: TextRenderer,
    scale: f32,
    /// 是否已计算固定位置
    pos: Option<(i32, i32)>,
    visible: bool,
}

impl Toolbar {
    // 视觉常量（逻辑像素，随 DPI 缩放）
    const HEIGHT: f32 = 30.0;
    const GRIP_W: f32 = 12.0;
    const CELL_PAD_X: f32 = 9.0;
    const MIN_CELL_W: f32 = 26.0;
    const FONT_PX: f32 = 15.0;

    const BG: [u8; 4] = [44, 44, 46, 240]; // 深灰圆角底
    const FG: [u8; 4] = [235, 235, 238, 255]; // 普通文字
    const HL_BG: [u8; 4] = [66, 133, 244, 255]; // 中文模式高亮蓝
    const HL_FG: [u8; 4] = [255, 255, 255, 255];
    const SEP: [u8; 4] = [70, 70, 74, 255]; // 分隔线
    const GRIP: [u8; 4] = [120, 120, 124, 255];

    pub fn new() -> Result<Self, String> {
        let scale = Self::dpi_scale();
        let window = LayeredWindow::create(None, 160, 40, "WindInputToolbar")?;
        let renderer = TextRenderer::new("Microsoft YaHei UI", Self::FONT_PX * scale)?;
        Ok(Self {
            window,
            renderer,
            scale,
            pos: None,
            visible: false,
        })
    }

    /// 根据状态构建单元格序列
    fn cells(state: &ToolbarState) -> Vec<Cell> {
        let mode = if state.chinese_mode { "中" } else { "英" };
        let schema = state
            .schema_label
            .chars()
            .next()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".to_string());
        let punct = if state.chinese_punct { "，。" } else { ",." };
        let width = if state.full_width { "全" } else { "半" };
        vec![
            Cell { text: mode.to_string(), highlight: state.chinese_mode },
            Cell { text: schema, highlight: false },
            Cell { text: punct.to_string(), highlight: false },
            Cell { text: width.to_string(), highlight: false },
        ]
    }

    /// 更新状态并重绘（首次会计算位置并显示）
    pub fn update(&mut self, state: &ToolbarState) {
        let s = self.scale;
        let height = (Self::HEIGHT * s).ceil();
        let grip_w = (Self::GRIP_W * s).ceil();
        let pad_x = Self::CELL_PAD_X * s;
        let min_cell = Self::MIN_CELL_W * s;

        let cells = Self::cells(state);

        // 逐格量宽
        let mut cell_widths = Vec::with_capacity(cells.len());
        for c in &cells {
            let m = self.renderer.measure_text(&c.text);
            cell_widths.push((m.width + pad_x * 2.0).max(min_cell).ceil());
        }
        let total_w: f32 = grip_w + cell_widths.iter().sum::<f32>();
        let w = total_w.ceil() as u32;
        let h = height as u32;

        self.window.resize(w, h);
        let buf_size = (w * h * 4) as usize;
        {
            let buf = self.window.buffer_mut();
            buf[..buf_size].fill(0);
            let radius = (h as f32 * 0.22) as u32;
            fill_rounded(buf, w, h, 0, 0, w, h, Self::BG, radius);
            // 拖动柄点阵（视觉对齐 Go，暂不响应拖动）
            draw_grip(buf, w, h, grip_w as u32, Self::GRIP, s);
        }

        // 逐格绘制
        let mut x = grip_w;
        let font_h = self.renderer.measure_text("中").height;
        for (i, c) in cells.iter().enumerate() {
            let cw = cell_widths[i];
            // 分隔线（首格前不画）
            if i > 0 {
                draw_vsep(self.window.buffer_mut(), w, h, x as u32, Self::SEP, s);
            }
            // 高亮底（中文模式格）
            if c.highlight {
                let inset = (4.0 * s) as u32;
                let hx = x as u32 + inset / 2;
                let hy = inset;
                let hw = (cw as u32).saturating_sub(inset);
                let hh = h.saturating_sub(inset * 2);
                let hr = (hh as f32 * 0.3) as u32;
                fill_rounded(self.window.buffer_mut(), w, h, hx, hy, hw, hh, Self::HL_BG, hr);
            }
            // 居中文字
            let m = self.renderer.measure_text(&c.text);
            let tx = x + (cw - m.width) * 0.5;
            let ty = (h as f32 - font_h) * 0.5;
            let fg = if c.highlight { Self::HL_FG } else { Self::FG };
            let _ = self
                .renderer
                .draw_text(self.window.buffer_mut(), w, h, tx.max(x), ty.max(0.0), &c.text, fg);
            x += cw;
        }

        if let Err(e) = self.window.update() {
            tracing::warn!("Toolbar update failed: {}", e);
        }

        // 固定位置：工作区右下角（避开任务栏）
        let (px, py) = *self.pos.get_or_insert_with(|| Self::corner_position(w, h));
        self.window.show(px, py);
        self.visible = true;
    }

    pub fn show(&mut self) {
        if let Some((x, y)) = self.pos {
            self.window.show(x, y);
            self.visible = true;
        }
    }

    pub fn hide(&mut self) {
        self.window.hide();
        self.visible = false;
    }

    /// 工作区右下角位置（避开任务栏），右/下各留 12px 边距
    fn corner_position(w: u32, h: u32) -> (i32, i32) {
        #[cfg(windows)]
        {
            use windows::Win32::Foundation::RECT;
            use windows::Win32::UI::WindowsAndMessaging::{
                SystemParametersInfoW, SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
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
}

/// 在缓冲区子区域 (x,y,w,h) 内填充圆角矩形
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
    let r = radius as i32;
    let (x0, y0) = (x as i32, y as i32);
    let (wi, hi) = (w as i32, h as i32);
    for ry in 0..hi {
        for rx in 0..wi {
            if !corner_inside(rx, ry, wi, hi, r) {
                continue;
            }
            let px = x0 + rx;
            let py = y0 + ry;
            if px < 0 || py < 0 || px >= buf_w as i32 || py >= buf_h as i32 {
                continue;
            }
            let idx = ((py * buf_w as i32 + px) * 4) as usize;
            if idx + 3 < buf.len() {
                buf[idx] = color[0];
                buf[idx + 1] = color[1];
                buf[idx + 2] = color[2];
                buf[idx + 3] = color[3];
            }
        }
    }
}

fn corner_inside(x: i32, y: i32, w: i32, h: i32, r: i32) -> bool {
    if r <= 0 {
        return true;
    }
    let corners = [
        (r, r, x < r && y < r),
        (w - 1 - r, r, x > w - 1 - r && y < r),
        (r, h - 1 - r, x < r && y > h - 1 - r),
        (w - 1 - r, h - 1 - r, x > w - 1 - r && y > h - 1 - r),
    ];
    for (cx, cy, in_quadrant) in corners {
        if in_quadrant {
            let dx = (x - cx) as f32;
            let dy = (y - cy) as f32;
            return dx * dx + dy * dy <= (r * r) as f32;
        }
    }
    true
}

/// 竖直分隔线（在 x 处，上下各内缩 6px）
fn draw_vsep(buf: &mut [u8], buf_w: u32, buf_h: u32, x: u32, color: [u8; 4], scale: f32) {
    let inset = (6.0 * scale) as u32;
    let y0 = inset;
    let y1 = buf_h.saturating_sub(inset);
    if x >= buf_w {
        return;
    }
    for y in y0..y1 {
        let idx = ((y * buf_w + x) * 4) as usize;
        if idx + 3 < buf.len() {
            buf[idx] = color[0];
            buf[idx + 1] = color[1];
            buf[idx + 2] = color[2];
            buf[idx + 3] = color[3];
        }
    }
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
    let r2 = r * r;
    let x0 = (cx - r).floor() as i32;
    let x1 = (cx + r).ceil() as i32;
    let y0 = (cy - r).floor() as i32;
    let y1 = (cy + r).ceil() as i32;
    for py in y0..y1 {
        for px in x0..x1 {
            if px < 0 || py < 0 || px >= buf_w as i32 || py >= buf_h as i32 {
                continue;
            }
            let ddx = px as f32 + 0.5 - cx;
            let ddy = py as f32 + 0.5 - cy;
            if ddx * ddx + ddy * ddy <= r2 {
                let idx = ((py * buf_w as i32 + px) * 4) as usize;
                if idx + 3 < buf.len() {
                    buf[idx] = color[0];
                    buf[idx + 1] = color[1];
                    buf[idx + 2] = color[2];
                    buf[idx + 3] = color[3];
                }
            }
        }
    }
}
