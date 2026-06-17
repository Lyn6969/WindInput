//! 精简 View 盒模型（measure → arrange → paint + 命中矩形提取）
//!
//! 与 Go 版本 `wind_input/internal/ui/viewbox*.go` 的核心子集对齐：
//! row/column 布局、padding/margin、背景/圆角/边框、固定尺寸、交叉轴对齐、
//! 文本叶子。布局分三步——measure 自底向上算尺寸，arrange 自顶向下定坐标，
//! paint 递归绘制；arrange 后每个带 tag 的节点都有绝对矩形，供鼠标命中复用。
//!
//! 不含 Go 的渐变 / 九宫格图 / 阴影模糊 / z 分层等重特性（后续按需扩展）。

use crate::text::dwrite::TextRenderer;
use std::cell::RefCell;
use tiny_skia::{
    Color, FillRule, FilterQuality, Paint, PathBuilder, Pattern, PixmapMut, SpreadMode, Transform,
};

thread_local! {
    /// 背景图解码/填充缓存（UI 单线程，跨帧复用，避免每帧解码）。
    static IMAGE_CACHE: RefCell<crate::image_cache::ImageCache> =
        RefCell::new(crate::image_cache::ImageCache::new());
}

/// 背景填充图（已解析路径 + 模式）。slice 为源图四边切片像素 [上,右,下,左]。
#[derive(Clone, Debug)]
pub struct ViewImage {
    pub path: String,
    pub mode: String,
    pub slice: [f32; 4],
    pub opacity: f32,
}

/// z 层级覆盖图（按 anchor 九宫定位 + offset + size 绘于 host 内）。
#[derive(Clone, Debug)]
pub struct ViewLayer {
    pub path: String,
    pub z: i32,
    pub anchor: String,
    /// dp 偏移（已 ×scale，px）。
    pub off_x: f32,
    pub off_y: f32,
    /// 百分比偏移（相对 host 宽/高；paint 期换算）。与 dp 偏移叠加。
    pub off_x_pct: f32,
    pub off_y_pct: f32,
    /// 目标尺寸 px（0=原图尺寸）。
    pub w: f32,
    pub h: f32,
    pub opacity: f32,
}

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
    /// 文本字号（设备像素）；None=用渲染器基准字号。序号/注释按相对偏移设具体值。
    pub font_size: Option<f32>,
    pub text_align: Align,
    /// 左侧强调条 (颜色, 宽度 px)：在节点左缘内绘制竖条（选中候选用）；不占布局空间（落在左内边距内）。
    pub left_bar: Option<([u8; 4], f32)>,
    /// 背景填充图（叠在底色之上，裁到圆角内）。
    pub bg_image: Option<ViewImage>,
    /// z 层级覆盖图（z<0 在内容下、z>0 在内容上）。
    pub layers: Vec<ViewLayer>,
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
            font_size: None,
            text_align: Align::Start,
            left_bar: None,
            bg_image: None,
            layers: Vec::new(),
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
    pub fn font_size(mut self, px: f32) -> Self {
        self.font_size = Some(px);
        self
    }
    pub fn left_bar(mut self, color: [u8; 4], width: f32) -> Self {
        self.left_bar = Some((color, width));
        self
    }
    pub fn bg_image(mut self, img: ViewImage) -> Self {
        self.bg_image = Some(img);
        self
    }
    pub fn layers(mut self, layers: Vec<ViewLayer>) -> Self {
        self.layers = layers;
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
    pub fn fixed_w(mut self, w: f32) -> Self {
        self.fixed_w = Some(w);
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
            let m = tr.measure_text_sized(t, self.font_size.unwrap_or(tr.base_size()));
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
        // 背景填充图（叠在底色上，裁到圆角内）。
        if let Some(img) = &self.bg_image {
            paint_bg_image(buf, buf_w, buf_h, r, self.corner_radius, img);
        }
        // 左侧强调条（选中候选）：在左内边距内画竖条，高 = 内容高的 60%，垂直居中。不占布局。
        if let Some((color, bw)) = self.left_bar {
            let bh = (r.h * 0.6).max(2.0);
            let by = r.y + (r.h - bh) * 0.5;
            fill_rounded(buf, buf_w, buf_h, r.x, by, bw, bh, color, bw * 0.5);
        }
        // z<0 覆盖图（在内容下方）。
        for layer in self.layers.iter().filter(|l| l.z < 0) {
            paint_layer(buf, buf_w, buf_h, r, layer);
        }
        // 文本
        if let Some(t) = &self.text {
            let size = self.font_size.unwrap_or(tr.base_size());
            let m = tr.measure_text_sized(t, size);
            let cx0 = r.x + self.padding.l;
            let content_w = r.w - self.padding.w();
            let content_h = r.h - self.padding.h();
            let tx = match self.text_align {
                Align::Start => cx0,
                Align::Center => cx0 + (content_w - m.width) * 0.5,
                Align::End => cx0 + content_w - m.width,
            };
            let ty = r.y + self.padding.t + (content_h - m.height) * 0.5;
            let _ = tr.draw_text_sized(
                buf,
                buf_w,
                buf_h,
                tx.max(r.x),
                ty.max(r.y),
                t,
                size,
                self.text_color,
            );
        }
        // 子节点
        for c in &self.children {
            c.paint(buf, buf_w, buf_h, tr);
        }
        // z>=0 覆盖图（在内容上方）。
        for layer in self.layers.iter().filter(|l| l.z >= 0) {
            paint_layer(buf, buf_w, buf_h, r, layer);
        }
    }
}

// ———————————————— 像素绘制工具 ————————————————

/// 贝塞尔逼近圆弧的控制点比例（kappa）。
const KAPPA: f32 = 0.552_284_75;

/// 在缓冲区子区域填充圆角矩形：tiny-skia 抗锯齿填充 + 源覆盖混合。
/// `color` 约定为直通 [R,G,B,A]；缓冲区按预乘 BGRA 维护（供 UpdateLayeredWindow）。
///
/// 关键技巧：把 BGRA 缓冲当作 tiny-skia 的"RGBA" Pixmap 直接渲染（零拷贝），
/// 传色时交换 R/B（Color 取 [B,G,R,A]）。预乘 alpha 合成逐通道对称，故输出即合法 BGRA。
/// 绘制背景填充图：从线程局部缓存取目标尺寸填充位图（BGRA 预乘），以 Pattern 填到圆角路径内。
fn paint_bg_image(buf: &mut [u8], buf_w: u32, buf_h: u32, r: Rect, radius: f32, img: &ViewImage) {
    let x = r.x.round();
    let y = r.y.round();
    let rw = r.w.round().max(1.0);
    let rh = r.h.round().max(1.0);
    let slice = [
        img.slice[0].round().max(0.0) as u32,
        img.slice[1].round().max(0.0) as u32,
        img.slice[2].round().max(0.0) as u32,
        img.slice[3].round().max(0.0) as u32,
    ];
    let mode = crate::image_cache::mode_code(&img.mode);
    let Some(path) = round_rect_path(x, y, rw, rh, radius.round().max(0.0)) else {
        return;
    };
    IMAGE_CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        let Some(fill) = cache.fill(&img.path, mode, slice, rw as u32, rh as u32) else {
            return;
        };
        let Some(mut pm) = PixmapMut::from_bytes(buf, buf_w, buf_h) else {
            return;
        };
        // 填充位图已是目标尺寸（mode 缩放完成），Pattern 仅平移到 rect → 无需再缩放（Nearest 即可）。
        let shader = Pattern::new(
            fill.as_ref(),
            SpreadMode::Pad,
            FilterQuality::Nearest,
            img.opacity.clamp(0.0, 1.0),
            Transform::from_translate(x, y),
        );
        let paint = Paint {
            shader,
            anti_alias: true,
            ..Default::default()
        };
        pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    });
}

/// 绘制 z 层覆盖图：按 anchor 九宫定位 + offset（dp + 百分比）置于 host 内，stretch 到目标尺寸 + opacity。
fn paint_layer(buf: &mut [u8], buf_w: u32, buf_h: u32, host: Rect, layer: &ViewLayer) {
    IMAGE_CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        // 目标尺寸：指定则用之，否则用原图尺寸。
        let (lw, lh) = if layer.w > 0.0 && layer.h > 0.0 {
            (layer.w.round().max(1.0), layer.h.round().max(1.0))
        } else {
            let Some((sw, sh)) = cache.src_size(&layer.path) else {
                return;
            };
            (sw as f32, sh as f32)
        };
        // anchor 九宫基位（host 内）+ offset（dp px + 百分比相对 host 宽/高）。
        let (ax, ay) = anchor_pos(&layer.anchor, host, lw, lh);
        let lx = (ax + layer.off_x + layer.off_x_pct / 100.0 * host.w).round();
        let ly = (ay + layer.off_y + layer.off_y_pct / 100.0 * host.h).round();
        let Some(fill) = cache.fill(&layer.path, crate::image_cache::mode_code("stretch"), [0; 4], lw as u32, lh as u32)
        else {
            return;
        };
        let Some(path) = round_rect_path(lx, ly, lw, lh, 0.0) else {
            return;
        };
        let Some(mut pm) = PixmapMut::from_bytes(buf, buf_w, buf_h) else {
            return;
        };
        let shader = Pattern::new(
            fill.as_ref(),
            SpreadMode::Pad,
            FilterQuality::Nearest,
            layer.opacity.clamp(0.0, 1.0),
            Transform::from_translate(lx, ly),
        );
        let paint = Paint {
            shader,
            anti_alias: true,
            ..Default::default()
        };
        pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    });
}

/// anchor 九宫定位：返回覆盖图左上角在 host 内的基准坐标（未含 offset）。
fn anchor_pos(anchor: &str, host: Rect, lw: f32, lh: f32) -> (f32, f32) {
    let ax = if anchor.contains("left") {
        host.x
    } else if anchor.contains("right") {
        host.x + host.w - lw
    } else {
        host.x + (host.w - lw) * 0.5
    };
    let ay = if anchor.contains("top") {
        host.y
    } else if anchor.contains("bottom") {
        host.y + host.h - lh
    } else {
        host.y + (host.h - lh) * 0.5
    };
    (ax, ay)
}

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
    if color[3] == 0 {
        return;
    }
    // 位置/尺寸对齐像素网格（这些盒子本就像素对齐），半径保留浮点供 AA。
    let x = x.round();
    let y = y.round();
    let w = w.round();
    let h = h.round();
    if w <= 0.0 || h <= 0.0 || buf_w == 0 || buf_h == 0 {
        return;
    }
    let Some(path) = round_rect_path(x, y, w, h, radius.round().max(0.0)) else {
        return;
    };
    let Some(mut pm) = PixmapMut::from_bytes(buf, buf_w, buf_h) else {
        return;
    };
    let mut paint = Paint::default();
    paint.anti_alias = true;
    // BGRA 缓冲被当作 RGBA：换 R/B，输出即正确的预乘 BGRA。
    paint.set_color(Color::from_rgba8(color[2], color[1], color[0], color[3]));
    pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
}

/// 填充实心圆（tiny-skia 抗锯齿）。`color` 为 [R,G,B,A]，缓冲预乘 BGRA（同 fill_rounded 换 R/B）。
pub fn fill_circle(buf: &mut [u8], buf_w: u32, buf_h: u32, cx: f32, cy: f32, r: f32, color: [u8; 4]) {
    if color[3] == 0 || r <= 0.0 || buf_w == 0 || buf_h == 0 {
        return;
    }
    let mut pb = PathBuilder::new();
    pb.push_circle(cx, cy, r);
    let Some(path) = pb.finish() else {
        return;
    };
    let Some(mut pm) = PixmapMut::from_bytes(buf, buf_w, buf_h) else {
        return;
    };
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color(Color::from_rgba8(color[2], color[1], color[0], color[3]));
    pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
}

/// 构造圆角矩形路径（radius 自动钳制到 min(w,h)/2；为 0 时退化为直角矩形）。
fn round_rect_path(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Option<tiny_skia::Path> {
    let r = radius.min(w * 0.5).min(h * 0.5).max(0.0);
    let mut pb = PathBuilder::new();
    if r <= 0.0 {
        pb.push_rect(tiny_skia::Rect::from_xywh(x, y, w, h)?);
    } else {
        let (l, t, rt, b) = (x, y, x + w, y + h);
        let k = r * KAPPA;
        pb.move_to(l + r, t);
        pb.line_to(rt - r, t);
        pb.cubic_to(rt - r + k, t, rt, t + r - k, rt, t + r);
        pb.line_to(rt, b - r);
        pb.cubic_to(rt, b - r + k, rt - r + k, b, rt - r, b);
        pb.line_to(l + r, b);
        pb.cubic_to(l + r - k, b, l, b - r + k, l, b - r);
        pb.line_to(l, t + r);
        pb.cubic_to(l, t + r - k, l + r - k, t, l + r, t);
        pb.close();
    }
    pb.finish()
}
