//! 精简 View 盒模型（measure → arrange → paint + 命中矩形提取）
//!
//! 与 Go 版本 `wind_input/internal/ui/viewbox*.go` 的核心子集对齐：
//! row/column 布局、padding/margin、背景/圆角/边框、固定尺寸、交叉轴对齐、
//! 文本叶子。布局分三步——measure 自底向上算尺寸，arrange 自顶向下定坐标，
//! paint 递归绘制；arrange 后每个带 tag 的节点都有绝对矩形，供鼠标命中复用。
//!
//! 九宫格图 / 阴影模糊 / z 分层 / 渐变背景均已支持。

use crate::text::dwrite::TextRenderer;
use std::cell::RefCell;
use tiny_skia::{
    Color, FillRule, FilterQuality, GradientStop, LinearGradient, Paint, PathBuilder, Pattern,
    PixmapMut, PixmapPaint, Point, RadialGradient, SpreadMode, Transform,
};
use wind_theme::schema::Dim;

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
    /// 单色染色（None=图原样）；非 None 时把图当 alpha mask、用此色填充（单色 SVG/图标随主题变色）。
    pub tint: Option<[u8; 4]>,
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

/// 背景渐变（叠在底色之上、背景图之下，裁到圆角内）。
/// linear 按 angle 方向铺色；radial 以节点中心为圆心。stops 为 (RGBA 直通, pos∈[0,1])。
#[derive(Clone, Debug)]
pub struct ViewGradient {
    pub radial: bool,
    /// 线性角度（度）：0=左→右，顺时针增大。radial 时忽略。
    pub angle: f32,
    pub stops: Vec<([u8; 4], f32)>,
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
        Self {
            l: v,
            t: v,
            r: v,
            b: v,
        }
    }
    pub fn xy(x: f32, y: f32) -> Self {
        Self {
            l: x,
            t: y,
            r: x,
            b: y,
        }
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
    /// 文本字重（400/500/700…）；None/0=继承渲染器默认（NORMAL）。
    pub font_weight: Option<i32>,
    /// 文本字体族覆盖；None/空=用渲染器全局字体族。
    pub font_family: Option<String>,
    pub text_align: Align,
    /// 左侧强调条 (颜色, 宽度 px)：在节点左缘内绘制竖条（选中候选用）；不占布局空间（落在左内边距内）。
    pub left_bar: Option<([u8; 4], f32)>,
    /// 圆形背景色：在节点中心画真圆（直径=min(w,h)）；序号圆圈用，替代圆角矩形药丸近似。
    pub circle_bg: Option<[u8; 4]>,
    /// 背景填充图（叠在底色之上，裁到圆角内）。
    pub bg_image: Option<ViewImage>,
    /// 背景渐变（叠在底色之上、背景图之下，裁到圆角内）。
    pub bg_gradient: Option<ViewGradient>,
    /// z 层级覆盖图（z<0 在内容下、z>0 在内容上）。
    pub layers: Vec<ViewLayer>,
    pub children: Vec<View>,
    /// 弹性占位：主轴方向吸收容器剩余空间（用于把后续子节点推到末端，如菜单 ▸ 右对齐）。
    pub grow: bool,
    /// 跨轴填充：在父容器排布时把本节点撑满父内容的跨轴尺寸（Column→宽度），
    /// 供其内部 spacer 实现右对齐（如 preedit 栏让模式标记贴右）。
    pub fill_cross: bool,
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
            font_weight: None,
            font_family: None,
            text_align: Align::Start,
            left_bar: None,
            circle_bg: None,
            bg_image: None,
            bg_gradient: None,
            layers: Vec::new(),
            children: Vec::new(),
            grow: false,
            fill_cross: false,
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

    /// 弹性占位：主轴吸收剩余空间，把其后的兄弟节点推到容器末端。
    pub fn spacer() -> Self {
        Self {
            grow: true,
            ..Default::default()
        }
    }

    /// 标记本节点为弹性（主轴吸收剩余空间）。
    pub fn grow(mut self) -> Self {
        self.grow = true;
        self
    }

    /// 标记本节点跨轴填充父容器（Column→撑满宽度），供内部 spacer 右对齐。
    pub fn fill_cross(mut self) -> Self {
        self.fill_cross = true;
        self
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
    /// 设字重（>0 生效；0/None 继承默认 NORMAL）。
    pub fn font_weight(mut self, w: i32) -> Self {
        self.font_weight = if w > 0 { Some(w) } else { None };
        self
    }
    /// 设字体族覆盖（非空生效）。
    pub fn font_family(mut self, f: Option<String>) -> Self {
        self.font_family = f.filter(|s| !s.trim().is_empty());
        self
    }
    pub fn bg_gradient(mut self, g: ViewGradient) -> Self {
        self.bg_gradient = Some(g);
        self
    }
    pub fn left_bar(mut self, color: [u8; 4], width: f32) -> Self {
        self.left_bar = Some((color, width));
        self
    }
    pub fn circle_bg(mut self, color: [u8; 4]) -> Self {
        self.circle_bg = Some(color);
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
            let m = tr.measure_text_styled(
                t,
                self.font_size.unwrap_or(tr.base_size()),
                self.font_weight.unwrap_or(0),
                self.font_family.as_deref(),
            );
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

        let n = self.children.len();
        let gap_total = if n > 1 {
            self.gap * (n - 1) as f32
        } else {
            0.0
        };
        let growers = self.children.iter().filter(|c| c.grow).count();

        match self.layout {
            Layout::Row => {
                // 弹性分配：主轴剩余空间均摊给 grow 子节点（撑大其 mw）。
                if growers > 0 {
                    let used: f32 =
                        self.children.iter().map(|c| c.margin_box().0).sum::<f32>() + gap_total;
                    let extra = (content_w - used).max(0.0) / growers as f32;
                    for c in self.children.iter_mut().filter(|c| c.grow) {
                        c.mw += extra;
                    }
                }
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
                if growers > 0 {
                    let used: f32 =
                        self.children.iter().map(|c| c.margin_box().1).sum::<f32>() + gap_total;
                    let extra = (content_h - used).max(0.0) / growers as f32;
                    for c in self.children.iter_mut().filter(|c| c.grow) {
                        c.mh += extra;
                    }
                }
                // 跨轴填充：fill_cross 子节点宽度撑满列内容宽（供其内部 spacer 右对齐）。
                for c in self.children.iter_mut().filter(|c| c.fill_cross) {
                    c.mw = (content_w - c.margin.w()).max(c.mw);
                }
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
        // 背景 + 边框：先铺底色（满圆角矩形），再画 even-odd 描边环覆盖外缘 bw 宽。
        // 边框作为干净描边环绘制（粗细恒为 bw、内外各一条 AA），不再用内/外两次填充
        // （旧法 AA 在边框/底色交界处双重混合致软边、且无法画镂空边框）。
        if let Some(bg) = self.bg {
            fill_rounded(
                buf,
                buf_w,
                buf_h,
                r.x,
                r.y,
                r.w,
                r.h,
                bg,
                self.corner_radius,
            );
        }
        // 背景渐变（叠在底色上、背景图下，裁到圆角内）。
        if let Some(g) = &self.bg_gradient {
            paint_bg_gradient(buf, buf_w, buf_h, r, self.corner_radius, g);
        }
        if let Some((bc, bw)) = self.border {
            fill_ring(
                buf,
                buf_w,
                buf_h,
                r.x,
                r.y,
                r.w,
                r.h,
                bc,
                self.corner_radius,
                bw,
            );
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
        // 圆形背景（序号圆圈）：节点中心真圆，直径 = min(w,h)。
        if let Some(color) = self.circle_bg {
            let cx = r.x + r.w * 0.5;
            let cy = r.y + r.h * 0.5;
            fill_circle(buf, buf_w, buf_h, cx, cy, r.w.min(r.h) * 0.5, color);
        }
        // z<0 覆盖图（在内容下方）。
        for layer in self.layers.iter().filter(|l| l.z < 0) {
            paint_layer(buf, buf_w, buf_h, r, layer);
        }
        // 文本
        if let Some(t) = &self.text {
            let size = self.font_size.unwrap_or(tr.base_size());
            let weight = self.font_weight.unwrap_or(0);
            let family = self.font_family.as_deref();
            let m = tr.measure_text_styled(t, size, weight, family);
            let cx0 = r.x + self.padding.l;
            let content_w = r.w - self.padding.w();
            let content_h = r.h - self.padding.h();
            let tx = match self.text_align {
                Align::Start => cx0,
                Align::Center => cx0 + (content_w - m.width) * 0.5,
                Align::End => cx0 + content_w - m.width,
            };
            let ty = r.y + self.padding.t + (content_h - m.height) * 0.5;
            let _ = tr.draw_text_styled(
                buf,
                buf_w,
                buf_h,
                tx.max(r.x),
                ty.max(r.y),
                t,
                size,
                weight,
                family,
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
/// 绘制背景渐变：以 tiny-skia 线性/径向着色器填到圆角路径内（叠在底色之上、背景图之下）。
fn paint_bg_gradient(
    buf: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    r: Rect,
    radius: f32,
    g: &ViewGradient,
) {
    if g.stops.is_empty() {
        return;
    }
    let x = r.x.round();
    let y = r.y.round();
    let rw = r.w.round().max(1.0);
    let rh = r.h.round().max(1.0);
    // 单色/退化 → 纯色填充（tiny-skia 渐变需 ≥2 停靠点）。
    if g.stops.len() < 2 {
        fill_rounded(buf, buf_w, buf_h, r.x, r.y, r.w, r.h, g.stops[0].0, radius);
        return;
    }
    let Some(path) = round_rect_path(x, y, rw, rh, radius.round().max(0.0)) else {
        return;
    };
    // (B,G,R,A) 交换：缓冲按 BGRA 维护，tiny-skia 当 RGBA 渲染（同 fill_rounded 约定）。
    let to_color = |c: &[u8; 4]| Color::from_rgba8(c[2], c[1], c[0], c[3]);
    // pos 规整：钳到 [0,1] 且单调不减（tiny-skia 要求递增停靠点）。
    let mut last = 0.0f32;
    let stops: Vec<GradientStop> = g
        .stops
        .iter()
        .map(|(c, p)| {
            let pos = p.clamp(0.0, 1.0).max(last);
            last = pos;
            GradientStop::new(pos, to_color(c))
        })
        .collect();
    let cx = x + rw * 0.5;
    let cy = y + rh * 0.5;
    let shader = if g.radial {
        let radius_len = 0.5 * (rw * rw + rh * rh).sqrt();
        RadialGradient::new(
            Point::from_xy(cx, cy),
            Point::from_xy(cx, cy),
            radius_len.max(1.0),
            stops,
            SpreadMode::Pad,
            Transform::identity(),
        )
    } else {
        // angle: 0=左→右，顺时针；端点沿方向投影覆盖整盒。
        let rad = g.angle.to_radians();
        let dx = rad.cos();
        let dy = rad.sin();
        let hl = (rw * dx).abs() * 0.5 + (rh * dy).abs() * 0.5;
        let p0 = Point::from_xy(cx - dx * hl, cy - dy * hl);
        let p1 = Point::from_xy(cx + dx * hl, cy + dy * hl);
        LinearGradient::new(p0, p1, stops, SpreadMode::Pad, Transform::identity())
    };
    let Some(shader) = shader else {
        return;
    };
    let Some(mut pm) = PixmapMut::from_bytes(buf, buf_w, buf_h) else {
        return;
    };
    let paint = Paint {
        shader,
        anti_alias: true,
        ..Default::default()
    };
    pm.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

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
    let tint = img.tint.unwrap_or([0, 0, 0, 0]);
    let Some(path) = round_rect_path(x, y, rw, rh, radius.round().max(0.0)) else {
        return;
    };
    IMAGE_CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        let Some(fill) = cache.fill(&img.path, mode, slice, rw as u32, rh as u32, tint) else {
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
        pm.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
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
        let Some(fill) = cache.fill(
            &layer.path,
            crate::image_cache::mode_code("stretch"),
            [0; 4],
            lw as u32,
            lh as u32,
            [0, 0, 0, 0],
        ) else {
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
        pm.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
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
    pm.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

/// 填充实心圆（tiny-skia 抗锯齿）。`color` 为 [R,G,B,A]，缓冲预乘 BGRA（同 fill_rounded 换 R/B）。
pub fn fill_circle(
    buf: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    cx: f32,
    cy: f32,
    r: f32,
    color: [u8; 4],
) {
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
    pm.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

/// 绘制齿轮图标（工具栏设置按钮）：8 齿平顶 cog 多边形（`color` 齿轮体）+ 中心孔（`hole` 色）。
/// 纯矢量绘制，不依赖字体度量 → 在单元格内精确居中、与文字格对齐。
/// `(cx,cy)` 中心，`r` 外径（齿尖半径）。color/hole 为 [R,G,B,A]，缓冲预乘 BGRA（同 fill_rounded 换 R/B）。
pub fn fill_gear(
    buf: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    cx: f32,
    cy: f32,
    r: f32,
    color: [u8; 4],
    hole: [u8; 4],
) {
    if color[3] == 0 || r <= 0.0 || buf_w == 0 || buf_h == 0 {
        return;
    }
    let teeth = 8usize;
    let r_in = r * 0.72; // 齿根半径
    let seg = std::f32::consts::TAU / teeth as f32;
    let tip = seg * 0.28; // 齿尖半角（平顶齿）
    let mut pb = PathBuilder::new();
    for i in 0..teeth {
        let c = i as f32 * seg;
        // 每齿 4 顶点：根-起 → 尖-起 → 尖-止 → 根-止，形成平顶梯形齿。
        let verts = [
            (r_in, c - seg * 0.5),
            (r, c - tip),
            (r, c + tip),
            (r_in, c + seg * 0.5),
        ];
        for (j, (rad, ang)) in verts.iter().enumerate() {
            let x = cx + rad * ang.cos();
            let y = cy + rad * ang.sin();
            if i == 0 && j == 0 {
                pb.move_to(x, y);
            } else {
                pb.line_to(x, y);
            }
        }
    }
    pb.close();
    if let Some(path) = pb.finish() {
        if let Some(mut pm) = PixmapMut::from_bytes(buf, buf_w, buf_h) {
            let mut paint = Paint::default();
            paint.anti_alias = true;
            paint.set_color(Color::from_rgba8(color[2], color[1], color[0], color[3]));
            pm.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }
    // 中心孔：在齿轮体上叠画一个小圆（hole 色，通常取工具栏底色 → 视觉镂空）。
    fill_circle(buf, buf_w, buf_h, cx, cy, r * 0.34, hole);
}

/// 绘制内联 SVG 图标到 BGRA 缓冲（工具栏图标用）：按 `color` 单色 tint（SVG 仅作 alpha 蒙版），
/// 栅格化到 `size`×`size` 后以 src-over 合成到 `(dx,dy)`（左上角）。不依赖字体度量 → 位置精确、形状灵活。
pub fn draw_svg_icon(
    buf: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    svg: &str,
    dx: f32,
    dy: f32,
    size: f32,
    color: [u8; 4],
) {
    if color[3] == 0 {
        return;
    }
    let s = size.round().max(1.0) as u32;
    let Some(icon) = crate::image_cache::rasterize_svg_str_tinted(svg, s, s, color) else {
        return;
    };
    let Some(mut pm) = PixmapMut::from_bytes(buf, buf_w, buf_h) else {
        return;
    };
    pm.draw_pixmap(
        dx.round() as i32,
        dy.round() as i32,
        icon.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
}

/// 高斯软投影（对齐 Go paintBlurredShadow）：在临时缓冲画 spread 扩张的圆角矩形，
/// alpha 通道做 3 次方框模糊逼近高斯，着色后预乘 src-over 合成到主 BGRA 缓冲。
/// (box_x, box_y, box_w, box_h) 为内容盒在主缓冲中的几何（不含 spread/offset）；
/// off_x/off_y 为阴影总偏移（基础 + 扩散额外偏移之和）。
#[allow(clippy::too_many_arguments)]
pub fn paint_blur_shadow(
    buf: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    box_x: f32,
    box_y: f32,
    box_w: f32,
    box_h: f32,
    radius: f32,
    blur: f32,
    spread: f32,
    off_x: f32,
    off_y: f32,
    color: [u8; 4],
) {
    if color[3] == 0 || buf_w == 0 || buf_h == 0 {
        return;
    }
    // 扩散后阴影盒（内容盒 ±spread，再加总偏移）
    let bw = box_w + 2.0 * spread;
    let bh = box_h + 2.0 * spread;
    let bx = box_x + off_x - spread;
    let by = box_y + off_y - spread;
    if bw <= 0.0 || bh <= 0.0 {
        return;
    }
    // 3 次方框模糊级联 sigma ≈ sqrt(blur*(blur+2))，3-sigma 需约 3×sigma px 衰减到透明。
    let sigma = (blur * (blur + 2.0)).max(0.0).sqrt();
    let pad = (3.0 * sigma).ceil() as i32 + 2;
    let tmp_w = bw.ceil() as i32 + 2 * pad;
    let tmp_h = bh.ceil() as i32 + 2 * pad;
    if tmp_w < 1 || tmp_h < 1 {
        return;
    }
    // 临时盒内阴影左上（保留亚像素偏移维持 AA）
    let local_x = pad as f32 + (bx - bx.floor());
    let local_y = pad as f32 + (by - by.floor());

    let mut tmp = vec![0u8; (tmp_w * tmp_h * 4) as usize];
    {
        let Some(mut pm) = PixmapMut::from_bytes(&mut tmp, tmp_w as u32, tmp_h as u32) else {
            return;
        };
        let Some(path) = round_rect_path(local_x, local_y, bw, bh, radius.max(0.0)) else {
            return;
        };
        let mut paint = Paint::default();
        paint.anti_alias = true;
        paint.set_color(Color::from_rgba8(0, 0, 0, 255));
        pm.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    // 提取 alpha 通道 → 3× 方框模糊
    let n = (tmp_w * tmp_h) as usize;
    let mut alpha = vec![0u8; n];
    for (i, a) in alpha.iter_mut().enumerate() {
        *a = tmp[i * 4 + 3];
    }
    let r = blur.round() as i32;
    if r > 0 {
        for _ in 0..3 {
            box_blur_alpha(&mut alpha, tmp_w, tmp_h, r);
        }
    }

    // 着色 + 预乘 src-over 合成到主缓冲（主缓冲为 BGRA：0=B,1=G,2=R,3=A；color 为 [R,G,B,A]）
    let dst_x0 = bx.floor() as i32 - pad;
    let dst_y0 = by.floor() as i32 - pad;
    let (cr, cg, cb, ca) = (
        color[0] as u32,
        color[1] as u32,
        color[2] as u32,
        color[3] as u32,
    );
    for ty in 0..tmp_h {
        for tx in 0..tmp_w {
            let ma = alpha[(ty * tmp_w + tx) as usize] as u32;
            if ma == 0 {
                continue;
            }
            let fa = ma * ca / 255; // 最终 alpha
            if fa == 0 {
                continue;
            }
            let dx = dst_x0 + tx;
            let dy = dst_y0 + ty;
            if dx < 0 || dx >= buf_w as i32 || dy < 0 || dy >= buf_h as i32 {
                continue;
            }
            let off = ((dy * buf_w as i32 + dx) * 4) as usize;
            let inv = 255 - fa;
            let sb = cb * fa / 255;
            let sg = cg * fa / 255;
            let sr = cr * fa / 255;
            buf[off] = ((sb * 255 + buf[off] as u32 * inv) / 255) as u8;
            buf[off + 1] = ((sg * 255 + buf[off + 1] as u32 * inv) / 255) as u8;
            buf[off + 2] = ((sr * 255 + buf[off + 2] as u32 * inv) / 255) as u8;
            buf[off + 3] = ((fa * 255 + buf[off + 3] as u32 * inv) / 255) as u8;
        }
    }
}

/// 窗口软投影参数（设备像素，已 ×scale）。模糊扩散层总偏移 = 基础 offset + 扩散额外偏移。
/// 候选窗与其它窗口（status/tooltip/toast）共享：四向扩边 + 高斯软影绘制一处实现。
pub struct SoftShadow {
    pub ox: f32,
    pub oy: f32,
    pub blur: f32,
    pub spread: f32,
    pub sox: f32,
    pub soy: f32,
    pub color: [u8; 4],
}

impl SoftShadow {
    /// 从节点 shadow_* 字段（Option<Dim> + 颜色）构建并 ×scale。
    /// 无色/全透明/零模糊零扩散零偏移 → None（不画投影）。
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        offset_x: Option<Dim>,
        offset_y: Option<Dim>,
        blur: Option<Dim>,
        spread: Option<Dim>,
        spread_off_x: Option<Dim>,
        spread_off_y: Option<Dim>,
        color: Option<[u8; 4]>,
        scale: f32,
    ) -> Option<SoftShadow> {
        let color = color?;
        if color[3] == 0 {
            return None;
        }
        let signed = |d: Option<Dim>| d.map(|x| x.resolve(scale, 0.0)).unwrap_or(0.0);
        let nonneg = |d: Option<Dim>| signed(d).max(0.0);
        let sh = SoftShadow {
            ox: signed(offset_x),
            oy: signed(offset_y),
            blur: nonneg(blur),
            spread: nonneg(spread),
            sox: signed(spread_off_x),
            soy: signed(spread_off_y),
            color,
        };
        if sh.blur <= 0.0 && sh.spread <= 0.0 && sh.off_x() == 0.0 && sh.off_y() == 0.0 {
            return None;
        }
        Some(sh)
    }

    /// 模糊扩散层 X 方向总偏移（基础 + 扩散额外）。
    pub fn off_x(&self) -> f32 {
        self.ox + self.sox
    }
    /// 模糊扩散层 Y 方向总偏移。
    pub fn off_y(&self) -> f32 {
        self.oy + self.soy
    }

    /// 四向缓冲扩边 (left, top, right, bottom)（与 Go shadowMargins 对齐）。
    pub fn margins(&self) -> (u32, u32, u32, u32) {
        let sigma = (self.blur * (self.blur + 2.0)).max(0.0).sqrt();
        let base = (3.0 * sigma).ceil() + 2.0 + self.spread;
        let (ox, oy) = (self.off_x(), self.off_y());
        (
            (base + (-ox).max(0.0)).ceil() as u32,
            (base + (-oy).max(0.0)).ceil() as u32,
            (base + ox.max(0.0)).ceil() as u32,
            (base + oy.max(0.0)).ceil() as u32,
        )
    }

    /// 在主缓冲画软影。(bx,by) 为内容盒左上（不含 offset/spread），(bw,bh) 内容盒尺寸，radius 圆角。
    #[allow(clippy::too_many_arguments)]
    pub fn paint(
        &self,
        buf: &mut [u8],
        buf_w: u32,
        buf_h: u32,
        bx: f32,
        by: f32,
        bw: f32,
        bh: f32,
        radius: f32,
    ) {
        paint_blur_shadow(
            buf,
            buf_w,
            buf_h,
            bx,
            by,
            bw,
            bh,
            radius,
            self.blur,
            self.spread,
            self.off_x(),
            self.off_y(),
            self.color,
        );
    }
}

/// 对 alpha 缓冲做一次可分离方框模糊（水平 + 垂直），边界取延伸（clamp）。三次调用逼近高斯。
fn box_blur_alpha(a: &mut [u8], w: i32, h: i32, r: i32) {
    if r <= 0 || w <= 0 || h <= 0 {
        return;
    }
    let win = (2 * r + 1) as u32;
    let mut tmp = vec![0u8; a.len()];
    // 水平
    for y in 0..h {
        let row = (y * w) as usize;
        let mut sum: u32 = 0;
        for k in -r..=r {
            let xi = k.clamp(0, w - 1) as usize;
            sum += a[row + xi] as u32;
        }
        for x in 0..w {
            tmp[row + x as usize] = (sum / win) as u8;
            let x_in = (x + r + 1).clamp(0, w - 1) as usize;
            let x_out = (x - r).clamp(0, w - 1) as usize;
            sum += a[row + x_in] as u32;
            sum -= a[row + x_out] as u32;
        }
    }
    // 垂直
    for x in 0..w {
        let xi = x as usize;
        let mut sum: u32 = 0;
        for k in -r..=r {
            let yi = k.clamp(0, h - 1);
            sum += tmp[(yi * w) as usize + xi] as u32;
        }
        for y in 0..h {
            a[(y * w) as usize + xi] = (sum / win) as u8;
            let y_in = (y + r + 1).clamp(0, h - 1);
            let y_out = (y - r).clamp(0, h - 1);
            sum += tmp[(y_in * w) as usize + xi] as u32;
            sum -= tmp[(y_out * w) as usize + xi] as u32;
        }
    }
}

/// 向 PathBuilder 追加一个圆角矩形子路径（radius 自动钳制到 min(w,h)/2；为 0 时退化为直角矩形）。
fn push_round_rect(pb: &mut PathBuilder, x: f32, y: f32, w: f32, h: f32, radius: f32) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let r = radius.min(w * 0.5).min(h * 0.5).max(0.0);
    if r <= 0.0 {
        if let Some(rect) = tiny_skia::Rect::from_xywh(x, y, w, h) {
            pb.push_rect(rect);
        }
        return;
    }
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

/// 构造圆角矩形路径（radius 自动钳制到 min(w,h)/2；为 0 时退化为直角矩形）。
fn round_rect_path(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    push_round_rect(&mut pb, x, y, w, h, radius);
    pb.finish()
}

/// 圆角矩形描边环（外圈 − 内圈，even-odd 单次填充）：粗细恒为 bw、内外各一条干净 AA，
/// 对齐 Go 的边框画法（避免中心描边 AA 渗色致粗细不均）。透明内部也适用。
/// color 为 [R,G,B,A]，缓冲预乘 BGRA（同 fill_rounded 换 R/B）。
pub fn fill_ring(
    buf: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: [u8; 4],
    radius: f32,
    bw: f32,
) {
    if color[3] == 0 || bw <= 0.0 || buf_w == 0 || buf_h == 0 {
        return;
    }
    let x = x.round();
    let y = y.round();
    let w = w.round();
    let h = h.round();
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let radius = radius.round().max(0.0);
    let mut pb = PathBuilder::new();
    push_round_rect(&mut pb, x, y, w, h, radius); // 外圈
    push_round_rect(
        &mut pb,
        x + bw,
        y + bw,
        w - 2.0 * bw,
        h - 2.0 * bw,
        (radius - bw).max(0.0),
    ); // 内圈（even-odd 挖空）
    let Some(path) = pb.finish() else {
        return;
    };
    let Some(mut pm) = PixmapMut::from_bytes(buf, buf_w, buf_h) else {
        return;
    };
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color(Color::from_rgba8(color[2], color[1], color[0], color[3]));
    pm.fill_path(
        &path,
        &paint,
        FillRule::EvenOdd,
        Transform::identity(),
        None,
    );
}

// ———————————————— 测试 ————————————————
//
// 测试边界说明：本模块的盒模型布局（measure/arrange/collect_hits）与形状绘制
// （fill_rounded/fill_circle/fill_ring，基于纯 Rust 的 tiny-skia）在 **所有平台
// 行为一致**，Linux 上的测试结果对 Windows 同样有效。唯一的跨平台差异是**文本
// 测量**：Windows 用 DirectWrite，非 Windows 用 mock 近似（字符数 × 字号 × 0.6）。
// 因此凡断言**具体文本尺寸/含文本布局数值**的用例都 gate 到 `not(windows)`，
// 以 mock 的确定尺寸做精确断言；纯几何与形状用例则跨平台运行。

/// 几何与形状绘制测试（跨平台真实：tiny-skia 纯 Rust 光栅化，不依赖文本后端）。
#[cfg(test)]
mod geom_tests {
    use super::*;

    #[test]
    fn rect_contains_includes_edges() {
        let r = Rect {
            x: 10.0,
            y: 10.0,
            w: 20.0,
            h: 20.0,
        };
        assert!(r.contains(10.0, 10.0)); // 左上角
        assert!(r.contains(30.0, 30.0)); // 右下角（x+w, y+h 含边界）
        assert!(r.contains(20.0, 20.0)); // 内部
        assert!(!r.contains(9.9, 20.0)); // 左外
        assert!(!r.contains(30.1, 20.0)); // 右外
        assert!(!r.contains(20.0, 9.9)); // 上外
    }

    #[test]
    fn edges_helpers() {
        let a = Edges::all(5.0);
        assert_eq!((a.l, a.t, a.r, a.b), (5.0, 5.0, 5.0, 5.0));
        assert_eq!(a.w(), 10.0);
        assert_eq!(a.h(), 10.0);
        let xy = Edges::xy(3.0, 4.0);
        assert_eq!((xy.l, xy.t, xy.r, xy.b), (3.0, 4.0, 3.0, 4.0));
        assert_eq!(xy.w(), 6.0);
        assert_eq!(xy.h(), 8.0);
    }

    #[test]
    fn fill_rounded_writes_alpha() {
        let mut buf = vec![0u8; 10 * 10 * 4];
        fill_rounded(
            &mut buf,
            10,
            10,
            0.0,
            0.0,
            10.0,
            10.0,
            [255, 0, 0, 255],
            0.0,
        );
        let center = (5 * 10 + 5) * 4;
        assert!(buf[center + 3] > 0, "中心像素 alpha 应被写入");
    }

    #[test]
    fn fill_rounded_transparent_color_is_noop() {
        let mut buf = vec![0u8; 4 * 4 * 4];
        fill_rounded(&mut buf, 4, 4, 0.0, 0.0, 4.0, 4.0, [255, 0, 0, 0], 0.0);
        assert!(buf.iter().all(|&b| b == 0), "alpha=0 不应写入任何像素");
    }

    #[test]
    fn fill_circle_writes_center_not_corner() {
        let mut buf = vec![0u8; 20 * 20 * 4];
        fill_circle(&mut buf, 20, 20, 10.0, 10.0, 8.0, [0, 255, 0, 255]);
        assert!(buf[(10 * 20 + 10) * 4 + 3] > 0, "圆心应被填充");
        assert_eq!(buf[(0 * 20 + 0) * 4 + 3], 0, "角落在圆外，不应被填充");
    }

    #[test]
    fn gradient_linear_writes_pixels() {
        let mut buf = vec![0u8; 16 * 16 * 4];
        let g = ViewGradient {
            radial: false,
            angle: 0.0,
            stops: vec![([255, 0, 0, 255], 0.0), ([0, 0, 255, 255], 1.0)],
        };
        let r = Rect {
            x: 0.0,
            y: 0.0,
            w: 16.0,
            h: 16.0,
        };
        paint_bg_gradient(&mut buf, 16, 16, r, 0.0, &g);
        assert!(buf[(8 * 16 + 8) * 4 + 3] > 0, "渐变中心 alpha 应被写入");
    }

    #[test]
    fn gradient_single_stop_falls_back_to_solid() {
        let mut buf = vec![0u8; 8 * 8 * 4];
        let g = ViewGradient {
            radial: false,
            angle: 0.0,
            stops: vec![([0, 255, 0, 255], 0.5)],
        };
        let r = Rect {
            x: 0.0,
            y: 0.0,
            w: 8.0,
            h: 8.0,
        };
        paint_bg_gradient(&mut buf, 8, 8, r, 0.0, &g);
        assert!(buf[(4 * 8 + 4) * 4 + 3] > 0, "单停靠点退化为纯色填充");
    }

    #[test]
    fn fill_ring_hollow_center() {
        let mut buf = vec![0u8; 20 * 20 * 4];
        fill_ring(
            &mut buf,
            20,
            20,
            0.0,
            0.0,
            20.0,
            20.0,
            [0, 0, 255, 255],
            0.0,
            2.0,
        );
        // 边框环：靠边像素被描边，正中心（挖空）保持透明
        assert!(buf[(0 * 20 + 10) * 4 + 3] > 0, "上边框应被描边");
        assert_eq!(buf[(10 * 20 + 10) * 4 + 3], 0, "环中心应镂空透明");
    }
}

/// 盒模型布局测试。断言含文本尺寸，依赖非 Windows 的 mock 文本测量
/// （`measure_text_sized` = 字符数 × 字号 × 0.6，行高 = 字号 × 1.2），故 gate 到 not(windows)。
#[cfg(all(test, not(windows)))]
mod layout_tests {
    use super::*;
    use crate::text::dwrite::TextRenderer;

    fn tr() -> TextRenderer {
        TextRenderer::new("test", 20.0).unwrap()
    }

    /// 构造一个固定尺寸的叶容器（无文本，尺寸跨平台确定）。
    fn fixed(w: f32, h: f32, tag: i32) -> View {
        View::container(Layout::Row).fixed_w(w).fixed_h(h).tag(tag)
    }

    /// 取某 tag 节点 arrange 后的绝对矩形。
    fn hit(root: &View, tag: i32) -> Rect {
        let mut out = Vec::new();
        root.collect_hits(&mut out);
        out.into_iter()
            .find(|(t, _)| *t == tag)
            .unwrap_or_else(|| panic!("tag {tag} 未命中"))
            .1
    }

    #[test]
    fn measure_row_sums_main_axis_with_gap() {
        let mut v = View::container(Layout::Row)
            .gap(10.0)
            .child(fixed(50.0, 20.0, -1))
            .child(fixed(50.0, 20.0, -1));
        v.layout(0.0, 0.0, &tr());
        assert_eq!(v.measured_size(), (110.0, 20.0)); // 50+50+gap10, 交叉轴取 max
    }

    #[test]
    fn measure_column_sums_main_axis_with_gap() {
        let mut v = View::container(Layout::Column)
            .gap(10.0)
            .child(fixed(50.0, 20.0, -1))
            .child(fixed(50.0, 20.0, -1));
        v.layout(0.0, 0.0, &tr());
        assert_eq!(v.measured_size(), (50.0, 50.0)); // 宽取 max50, 高 20+20+gap10
    }

    #[test]
    fn measure_adds_padding() {
        let mut v = View::container(Layout::Row)
            .pad(Edges::all(8.0))
            .child(fixed(50.0, 20.0, -1));
        v.layout(0.0, 0.0, &tr());
        assert_eq!(v.measured_size(), (66.0, 36.0)); // +16 两侧 padding
    }

    #[test]
    fn fixed_size_overrides_content() {
        let mut v = View::container(Layout::Row)
            .fixed_w(200.0)
            .fixed_h(30.0)
            .child(fixed(999.0, 999.0, -1));
        v.layout(0.0, 0.0, &tr());
        assert_eq!(v.measured_size(), (200.0, 30.0));
    }

    #[test]
    fn leaf_text_measured_via_mock() {
        let mut v = View::leaf("abc", [0, 0, 0, 255]).font_size(20.0);
        v.layout(0.0, 0.0, &tr());
        // mock: 3 字符 × 20 × 0.6 = 36 宽；20 × 1.2 = 24 高
        assert_eq!(v.measured_size(), (36.0, 24.0));
    }

    #[test]
    fn arrange_cross_center_offsets_child() {
        let mut root = View::container(Layout::Row)
            .cross(Align::Center)
            .fixed_h(40.0)
            .child(fixed(50.0, 20.0, 0));
        root.layout(0.0, 0.0, &tr());
        let r = hit(&root, 0);
        assert_eq!(r.x, 0.0);
        assert_eq!(r.y, 10.0); // (40-20)/2 居中
    }

    #[test]
    fn arrange_grow_spacer_absorbs_remaining() {
        let mut root = View::container(Layout::Row)
            .fixed_w(200.0)
            .child(fixed(50.0, 10.0, 0))
            .child(View::spacer())
            .child(fixed(50.0, 10.0, 2));
        root.layout(0.0, 0.0, &tr());
        assert_eq!(hit(&root, 0).x, 0.0);
        assert_eq!(hit(&root, 2).x, 150.0); // spacer 吸收 200-100=100，把末项推到 150
    }

    #[test]
    fn arrange_fill_cross_stretches_width() {
        let mut root = View::container(Layout::Column).fixed_w(100.0).child(
            View::container(Layout::Column)
                .fixed_h(20.0)
                .fill_cross()
                .tag(0),
        );
        root.layout(0.0, 0.0, &tr());
        assert_eq!(hit(&root, 0).w, 100.0); // 跨轴撑满列内容宽
    }

    #[test]
    fn arrange_applies_child_margin() {
        let mut root = View::container(Layout::Row).child(fixed(10.0, 10.0, 0).margin(Edges {
            l: 5.0,
            t: 3.0,
            r: 0.0,
            b: 0.0,
        }));
        root.layout(0.0, 0.0, &tr());
        let r = hit(&root, 0);
        assert_eq!((r.x, r.y), (5.0, 3.0)); // margin 偏移子节点原点
    }

    #[test]
    fn collect_hits_depth_first_skips_untagged() {
        let mut root = View::container(Layout::Column)
            .child(fixed(10.0, 10.0, 0))
            .child(fixed(10.0, 10.0, 1).child(fixed(5.0, 5.0, 2)));
        root.layout(0.0, 0.0, &tr());
        let mut out = Vec::new();
        root.collect_hits(&mut out);
        let tags: Vec<i32> = out.iter().map(|(t, _)| *t).collect();
        assert_eq!(tags, vec![0, 1, 2]); // 先序遍历；root 默认 tag=-1 不收集
    }
}
