//! 精简 View 盒模型（measure → arrange → paint + 命中矩形提取）
//!
//! 与 Go 版本 `wind_input/internal/ui/viewbox*.go` 的核心子集对齐：
//! row/column 布局、padding/margin、背景/圆角/边框、固定尺寸、交叉轴对齐、
//! 文本叶子。布局分三步——measure 自底向上算尺寸，arrange 自顶向下定坐标，
//! paint 递归绘制；arrange 后每个带 tag 的节点都有绝对矩形，供鼠标命中复用。
//!
//! 不含 Go 的渐变 / 九宫格图 / 阴影模糊 / z 分层等重特性（后续按需扩展）。

use crate::text::dwrite::TextRenderer;

/// 四边内/外边距
#[derive(Clone, Copy, Default)]
pub struct Edges {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl Edges {
    pub fn all(v: f32) -> Self {
        Self { l: v, t: v, r: v, b: v }
    }
    pub fn xy(x: f32, y: f32) -> Self {
        Self { l: x, t: y, r: x, b: y }
    }
    fn w(&self) -> f32 {
        self.l + self.r
    }
    fn h(&self) -> f32 {
        self.t + self.b
    }
}

/// 主轴方向
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Row,
    Column,
}

/// 对齐方式（交叉轴 / 文本水平）
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Start,
    Center,
    End,
}

/// 绝对矩形（arrange 后填充）
#[derive(Clone, Copy, Default, Debug)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    /// 点是否落在矩形内
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
    }
}

/// 一个视图节点（容器或文本叶子）
pub struct View {
    pub layout: Layout,
    pub margin: Edges,
    pub padding: Edges,
    pub gap: f32,
    pub cross_align: Align,
    pub fixed_w: Option<f32>,
    pub fixed_h: Option<f32>,
    pub bg: Option<[u8; 4]>,
    pub corner_radius: f32,
    /// 边框 (颜色, 宽度)
    pub border: Option<([u8; 4], f32)>,
    pub text: Option<String>,
    pub text_color: [u8; 4],
    pub text_align: Align,
    pub children: Vec<View>,
    /// 命中标识：>=0 参与命中收集（如候选下标 / 按钮 id），<0 忽略
    pub tag: i32,
    // 计算结果
    mw: f32,
    mh: f32,
    rect: Rect,
}

impl Default for View {
    fn default() -> Self {
        Self {
            layout: Layout::Row,
            margin: Edges::default(),
            padding: Edges::default(),
            gap: 0.0,
            cross_align: Align::Start,
            fixed_w: None,
            fixed_h: None,
            bg: None,
            corner_radius: 0.0,
            border: None,
            text: None,
            text_color: [0, 0, 0, 255],
            text_align: Align::Start,
            children: Vec::new(),
            tag: -1,
            mw: 0.0,
            mh: 0.0,
            rect: Rect::default(),
        }
    }
}

impl View {
    /// 文本叶子
    pub fn leaf(text: impl Into<String>, color: [u8; 4]) -> Self {
        Self {
            text: Some(text.into()),
            text_color: color,
            ..Default::default()
        }
    }

    /// 容器
    pub fn container(layout: Layout) -> Self {
        Self {
            layout,
            ..Default::default()
        }
    }

    // —— 链式构建辅助 ——
    pub fn pad(mut self, e: Edges) -> Self {
        self.padding = e;
        self
    }
    pub fn margin(mut self, e: Edges) -> Self {
        self.margin = e;
        self
    }
    pub fn gap(mut self, g: f32) -> Self {
        self.gap = g;
        self
    }
    pub fn cross(mut self, a: Align) -> Self {
        self.cross_align = a;
        self
    }
    pub fn bg(mut self, c: [u8; 4]) -> Self {
        self.bg = Some(c);
        self
    }
    pub fn radius(mut self, r: f32) -> Self {
        self.corner_radius = r;
        self
    }
    pub fn border(mut self, c: [u8; 4], w: f32) -> Self {
        self.border = Some((c, w));
        self
    }
    pub fn text_align(mut self, a: Align) -> Self {
        self.text_align = a;
        self
    }
    pub fn tag(mut self, t: i32) -> Self {
        self.tag = t;
        self
    }
    pub fn fixed_h(mut self, h: f32) -> Self {
        self.fixed_h = Some(h);
        self
    }
    pub fn child(mut self, c: View) -> Self {
        self.children.push(c);
        self
    }

    fn margin_box(&self) -> (f32, f32) {
        (self.mw + self.margin.w(), self.mh + self.margin.h())
    }

    /// 一次完整布局：自底向上测量，再自顶向下定位（根左上角 = (x,y)）。
    pub fn layout(&mut self, x: f32, y: f32, tr: &TextRenderer) {
        self.measure(tr);
        self.arrange(x, y);
    }

    fn measure(&mut self, tr: &TextRenderer) {
        let (cw, ch) = if let Some(t) = &self.text {
            let m = tr.measure_text(t);
            (m.width, m.height)
        } else {
            let mut main = 0.0f32;
            let mut cross = 0.0f32;
            let n = self.children.len();
            for c in &mut self.children {
                c.measure(tr);
                let (mw, mh) = c.margin_box();
                match self.layout {
                    Layout::Row => {
                        main += mw;
                        cross = cross.max(mh);
                    }
                    Layout::Column => {
                        main += mh;
                        cross = cross.max(mw);
                    }
                }
            }
            if n > 1 {
                main += self.gap * (n - 1) as f32;
            }
            match self.layout {
                Layout::Row => (main, cross),
                Layout::Column => (cross, main),
            }
        };
        let mut w = cw + self.padding.w();
        let mut h = ch + self.padding.h();
        if let Some(fw) = self.fixed_w {
            w = fw;
        }
        if let Some(fh) = self.fixed_h {
            h = fh;
        }
        self.mw = w;
        self.mh = h;
    }

    fn arrange(&mut self, x: f32, y: f32) {
        self.rect = Rect {
            x,
            y,
            w: self.mw,
            h: self.mh,
        };
        if self.children.is_empty() {
            return;
        }
        let cx0 = x + self.padding.l;
        let cy0 = y + self.padding.t;
        let content_w = self.mw - self.padding.w();
        let content_h = self.mh - self.padding.h();

        match self.layout {
            Layout::Row => {
                let mut cx = cx0;
                for c in &mut self.children {
                    let (cmw, cmh) = c.margin_box();
                    let cy = match self.cross_align {
                        Align::Start => cy0,
                        Align::Center => cy0 + (content_h - cmh) * 0.5,
                        Align::End => cy0 + content_h - cmh,
                    };
                    c.arrange(cx + c.margin.l, cy + c.margin.t);
                    cx += cmw + self.gap;
                }
            }
            Layout::Column => {
                let mut cy = cy0;
                for c in &mut self.children {
                    let (cmw, cmh) = c.margin_box();
                    let cx = match self.cross_align {
                        Align::Start => cx0,
                        Align::Center => cx0 + (content_w - cmw) * 0.5,
                        Align::End => cx0 + content_w - cmw,
                    };
                    c.arrange(cx + c.margin.l, cy + c.margin.t);
                    cy += cmh + self.gap;
                }
            }
        }
    }

    /// 测得尺寸（measure 后有效）
    pub fn measured_size(&self) -> (f32, f32) {
        (self.mw, self.mh)
    }

    /// 收集所有 tag>=0 节点的绝对矩形 → (tag, rect)
    pub fn collect_hits(&self, out: &mut Vec<(i32, Rect)>) {
        if self.tag >= 0 {
            out.push((self.tag, self.rect));
        }
        for c in &self.children {
            c.collect_hits(out);
        }
    }

    /// 递归绘制到 BGRA 缓冲区
    pub fn paint(&self, buf: &mut [u8], buf_w: u32, buf_h: u32, tr: &TextRenderer) {
        let r = self.rect;
        // 背景 + 边框
        match (self.bg, self.border) {
            (Some(bg), Some((bc, bw))) => {
                fill_rounded(buf, buf_w, buf_h, r.x, r.y, r.w, r.h, bc, self.corner_radius);
                let inr = (self.corner_radius - bw).max(0.0);
                fill_rounded(
                    buf,
                    buf_w,
                    buf_h,
                    r.x + bw,
                    r.y + bw,
                    (r.w - bw * 2.0).max(0.0),
                    (r.h - bw * 2.0).max(0.0),
                    bg,
                    inr,
                );
            }
            (Some(bg), None) => {
                fill_rounded(buf, buf_w, buf_h, r.x, r.y, r.w, r.h, bg, self.corner_radius);
            }
            (None, Some((bc, bw))) => {
                fill_rounded(buf, buf_w, buf_h, r.x, r.y, r.w, r.h, bc, self.corner_radius);
                let inr = (self.corner_radius - bw).max(0.0);
                // 仅描边：内部不填（保留已有背景）——此处简化为不支持纯描边镂空，
                // 调用方需要透明镂空边框时应配合外层背景。当前候选窗边框始终配 bg。
                let _ = inr;
            }
            (None, None) => {}
        }
        // 文本
        if let Some(t) = &self.text {
            let m = tr.measure_text(t);
            let cx0 = r.x + self.padding.l;
            let content_w = r.w - self.padding.w();
            let content_h = r.h - self.padding.h();
            let tx = match self.text_align {
                Align::Start => cx0,
                Align::Center => cx0 + (content_w - m.width) * 0.5,
                Align::End => cx0 + content_w - m.width,
            };
            let ty = r.y + self.padding.t + (content_h - m.height) * 0.5;
            let _ = tr.draw_text(buf, buf_w, buf_h, tx.max(r.x), ty.max(r.y), t, self.text_color);
        }
        // 子节点
        for c in &self.children {
            c.paint(buf, buf_w, buf_h, tr);
        }
    }
}

// ———————————————— 像素绘制工具 ————————————————

/// 在缓冲区子区域填充圆角矩形（覆盖写入，预乘 alpha 由颜色自带）
pub fn fill_rounded(
    buf: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: [u8; 4],
    radius: f32,
) {
    let x0 = x.round() as i32;
    let y0 = y.round() as i32;
    let wi = w.round() as i32;
    let hi = h.round() as i32;
    let r = radius.round() as i32;
    if wi <= 0 || hi <= 0 {
        return;
    }
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
