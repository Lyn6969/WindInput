//! 语言栏图标离屏渲染（Windows TSF 输入指示器那个 16×16 图标）
//!
//! 服务端把当前状态渲染成多档位图写进共享内存，`wind_tsf.dll` 的 `GetIcon` 直接取用。
//! 设计与取舍见 `docs/design/langbar-icon-shared-render.md`。
//!
//! ## 为什么必须分层渲染
//!
//! [`crate::text::dwrite`] 后端假设目标缓冲区是**已含背景的预乘 alpha**：渲染后逐像素
//! 对比，RGB 未变的算背景保留原 alpha，RGB 变了的按缓冲区原 alpha 预乘。
//!
//! 直接后果：**给一个全透明（alpha=0）缓冲画字，文字像素会被按 alpha=0 预乘，
//! 结果全黑透明，什么都看不到。** 所以这里走「黑底画白字 → 取 luminance 当覆盖度」
//! 拿到主字蒙版，角标另行几何绘制拿到第二张蒙版，两张蒙版各自着色后再合成。
//!
//! 顺带的好处是**摆脱了单色限制**：旧的 C++ 实现对整张图共用一个 luminance→alpha，
//! 所以整个图标只能一种颜色；分层之后主字与角标可以各自取色。
//!
//! ## 像素格式
//!
//! 输出 **非预乘** BGRA——Windows 图标的 32bpp DIB 就是这个约定
//! （`CreateIconIndirect` 的 `hbmColor` 里 RGB 不乘 alpha）。

use crate::text::dwrite::TextRenderer;

/// 标点角标要表达的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PunctBadge {
    /// 不画角标（功能关闭，或英文模式下标点不可切换）
    None,
    /// 中文标点
    Chinese,
    /// 英文标点
    English,
}

/// 角标的编码方式。
///
/// 做成枚举而非写死，是因为 16×16 下哪种编码可辨只能真机看——而把渲染搬到服务端后，
/// 换一种编码只需重启服务，不必重新分发 DLL。原型对比见
/// `docs/design/langbar-icon-shared-render.md`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BadgeShape {
    /// 右下角直角三角（中实心 / 英空心）。**当前默认。**
    ///
    /// 三角填满角落，同样面积下比圆形更"占地方"，因而在 16px 下比小圆点更醒目；
    /// 且直角边贴着图标边界，与主字的接触面比居中的圆更小。配色见
    /// [`IconRenderer::badge_colors`]——彩色时不必靠形状区分，两态可都用实心。
    #[default]
    CornerTriangle,
    /// 最外圈边框（中=整圈 / 英=四角断开）。
    ///
    /// 完全不占内部空间，主字一个像素都不用让——这是它相对所有角标方案的根本优势。
    /// 代价是最外圈只有 1~2px，且任务栏图标周围留白很窄，边框容易与相邻图标混淆。
    OuterRing,
    /// 实心圆（中）/ 实心方（英）。
    ///
    /// ⚠ **真机实测否决**：两态都是实心、不依赖细节像素，纸面上 16px 最稳，
    /// 但任务栏上的观感明显不如三角——圆点悬在角落里像个污点，与主字既不贴边也不成整体。
    /// 保留仅供调试菜单对比，不要再选作默认。
    CircleSquare,
    /// 空心环（中）/ 实心点（英）。语义最贴近「。」与「.」。
    ///
    /// ⚠ **真机实测否决**，同 [`Self::CircleSquare`]；且 16px 下环的内腔只剩 1~2px，
    /// 浅底尤其弱。
    RingDot,
    /// 底部横条（中）/ 两点（英）。真实尺寸下区分度最高且不占右下角，
    /// 因此不与「五」「双」这类右下有笔画的字打架；弱点是与标点没有直觉关联。
    BottomBar,
}

impl BadgeShape {
    /// 全部编码方式，顺序即调试菜单里的顺序，也是 [`Self::index`] 的编号依据。
    ///
    /// 单一真相源：菜单项、勾选态还原、`MenuCmd` 的 u8 参数三处都从这里取，
    /// 各写一份的话，加一种形状时漏改任意一处都表现为「点了另一个形状」。
    pub const ALL: [BadgeShape; 5] = [
        BadgeShape::CornerTriangle,
        BadgeShape::OuterRing,
        BadgeShape::CircleSquare,
        BadgeShape::RingDot,
        BadgeShape::BottomBar,
    ];

    /// 调试菜单文案。
    pub fn label(self) -> &'static str {
        match self {
            BadgeShape::CornerTriangle => "右下三角",
            BadgeShape::OuterRing => "最外圈边框",
            BadgeShape::CircleSquare => "圆 / 方（已否决）",
            BadgeShape::RingDot => "环 / 点（已否决）",
            BadgeShape::BottomBar => "底部横条",
        }
    }

    /// 在 [`Self::ALL`] 中的下标，用作菜单命令参数。
    pub fn index(self) -> u8 {
        Self::ALL.iter().position(|&s| s == self).unwrap_or(0) as u8
    }

    /// 由下标还原；越界回落到默认形状（菜单 id 来自另一个进程，不能假定合法）。
    pub fn from_index(i: u8) -> BadgeShape {
        Self::ALL.get(i as usize).copied().unwrap_or_default()
    }
}

/// 一次图标渲染的输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconSpec {
    /// 主字，如「中」「英」「拼」「五」。
    pub label: String,
    /// 标点角标状态。
    pub punct: PunctBadge,
    /// 整体变淡：**只给线程级 KEYBOARD_DISABLED**（输入法整个被禁用，罕见且严重）。
    ///
    /// ⚠ 不要把「焦点不在可编辑控件里」并进来——那是日常状态（点按钮/列表/桌面都会进），
    /// 旧实现试过并入，实测图标频繁变灰、用户无从理解，已改为与密码框一样显「英」。
    pub dimmed: bool,
    /// 动画相位，仅在演示模式下递增（见 [`IconRenderer::demo_animation`]）。
    ///
    /// 放进 spec 而非单独传参，是为了让发布器的"状态未变则跳过"判据自动把它算进去：
    /// 相位一变就是新内容，该重发；相位不变就该跳过。
    pub frame: u32,
}

impl Default for IconSpec {
    fn default() -> Self {
        Self {
            label: "中".to_string(),
            punct: PunctBadge::None,
            dimmed: false,
            frame: 0,
        }
    }
}

/// 单通道覆盖度蒙版（0.0~1.0），最终当 alpha 用。
#[derive(Clone)]
struct Mask {
    n: usize,
    v: Vec<f32>,
}

impl Mask {
    fn new(n: usize) -> Self {
        Self {
            n,
            v: vec![0.0; n * n],
        }
    }

    /// source-over 累加：已有覆盖度不会被后画的削减。
    fn blend(&mut self, x: i32, y: i32, cov: f32) {
        if x < 0 || y < 0 || x as usize >= self.n || y as usize >= self.n {
            return;
        }
        let d = &mut self.v[y as usize * self.n + x as usize];
        *d += cov * (1.0 - *d);
    }

    fn get(&self, i: usize) -> f32 {
        self.v[i].min(1.0)
    }
}

/// 4×4 超采样画圆/环。`r_in > 0` 即为环。
///
/// 自己做超采样而不用现成的矢量库，是因为这里的图形只有圆和矩形两种，
/// 而在 5px 直径上，抗锯齿质量直接决定两态能否分辨——用得起精确的覆盖度积分。
fn draw_disc(m: &mut Mask, cx: f32, cy: f32, r_out: f32, r_in: f32) {
    const SS: i32 = 4;
    let x0 = (cx - r_out - 1.0).floor() as i32;
    let x1 = (cx + r_out + 1.0).ceil() as i32;
    let y0 = (cy - r_out - 1.0).floor() as i32;
    let y1 = (cy + r_out + 1.0).ceil() as i32;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let mut hit = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
                    if d <= r_out && d >= r_in {
                        hit += 1;
                    }
                }
            }
            if hit > 0 {
                m.blend(x, y, hit as f32 / (SS * SS) as f32);
            }
        }
    }
}

/// 4×4 超采样画轴对齐矩形（半宽 `hw`、半高 `hh`），兼作方点与横条。
fn draw_rect(m: &mut Mask, cx: f32, cy: f32, hw: f32, hh: f32) {
    const SS: i32 = 4;
    let x0 = (cx - hw - 1.0).floor() as i32;
    let x1 = (cx + hw + 1.0).ceil() as i32;
    let y0 = (cy - hh - 1.0).floor() as i32;
    let y1 = (cy + hh + 1.0).ceil() as i32;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let mut hit = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    if (px - cx).abs() <= hw && (py - cy).abs() <= hh {
                        hit += 1;
                    }
                }
            }
            if hit > 0 {
                m.blend(x, y, hit as f32 / (SS * SS) as f32);
            }
        }
    }
}

/// 4×4 超采样画右下角直角三角，直角顶点在 `(s, s)`、直角边长 `leg`。
///
/// 三角能把角落填满，同面积下比圆更醒目；而斜边朝向图标中心，与主字的接触面
/// 又比同样占地的方块小。`hollow` 为真时只留 `th` 厚的边。
fn draw_corner_triangle(m: &mut Mask, s: f32, leg: f32, hollow: bool, th: f32) {
    const SS: i32 = 4;
    let inside = |px: f32, py: f32, l: f32| -> bool {
        px >= s - l && py >= s - l && (px + py) >= (2.0 * s - l)
    };
    let x0 = (s - leg - 1.0).floor() as i32;
    let y0 = (s - leg - 1.0).floor() as i32;
    let x1 = s.ceil() as i32;
    let y1 = s.ceil() as i32;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let mut hit = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    if !inside(px, py, leg) {
                        continue;
                    }
                    // 空心：再挖掉一个内缩的同心三角
                    if hollow && inside(px + th * 1.4, py + th * 1.4, leg - th * 1.4) {
                        continue;
                    }
                    hit += 1;
                }
            }
            if hit > 0 {
                m.blend(x, y, hit as f32 / (SS * SS) as f32);
            }
        }
    }
}

/// 把边界点映射到外圈周长上的归一化位置 `[0,1)`，顺时针：上 → 右 → 下 → 左。
///
/// 用「离哪条边最近」分区（等价于沿对角线切成四块），这样四个角上的像素归属明确，
/// 跑马灯扫过转角时不会出现断点或重叠。
fn perimeter_t(px: f32, py: f32, s: f32) -> f32 {
    let per = 4.0 * s;
    let (d_top, d_bottom, d_left, d_right) = (py, s - py, px, s - px);
    let min = d_top.min(d_bottom).min(d_left).min(d_right);
    if min == d_top {
        px / per
    } else if min == d_right {
        (s + py) / per
    } else if min == d_bottom {
        (2.0 * s + (s - px)) / per
    } else {
        (3.0 * s + (s - py)) / per
    }
}

/// 画最外圈边框。`th` 为厚度；`dashed` 时在四角留缺口。
///
/// 完全不占内部空间是它的全部意义——主字一个像素都不用让。
fn draw_outer_ring(m: &mut Mask, s: f32, th: f32, dashed: bool) {
    const SS: i32 = 4;
    let n = m.n as i32;
    for y in 0..n {
        for x in 0..n {
            let mut hit = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    let edge = px.min(py).min(s - px).min(s - py);
                    if edge > th {
                        continue;
                    }
                    if dashed {
                        // 四角各留一段缺口：把周长四等分后，每段两端各空出 18%
                        let t = perimeter_t(px, py, s) * 4.0;
                        let f = t - t.floor();
                        if !(0.18..=0.82).contains(&f) {
                            continue;
                        }
                    }
                    hit += 1;
                }
            }
            if hit > 0 {
                m.blend(x, y, hit as f32 / (SS * SS) as f32);
            }
        }
    }
}

/// 外圈跑马灯：只点亮周长上 `[phase, phase + len)` 的一段（相位与长度均归一化）。
///
/// 纯演示用，不表达任何状态。
fn draw_ring_marquee(m: &mut Mask, s: f32, th: f32, phase: f32, len: f32) {
    const SS: i32 = 4;
    let n = m.n as i32;
    let phase = phase.rem_euclid(1.0);
    for y in 0..n {
        for x in 0..n {
            let mut hit = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    let edge = px.min(py).min(s - px).min(s - py);
                    if edge > th {
                        continue;
                    }
                    // 相对相位落在 [0, len) 内即点亮；用 rem_euclid 让区间跨越 0 点时仍连续
                    let rel = (perimeter_t(px, py, s) - phase).rem_euclid(1.0);
                    if rel < len {
                        hit += 1;
                    }
                }
            }
            if hit > 0 {
                m.blend(x, y, hit as f32 / (SS * SS) as f32);
            }
        }
    }
}

/// 图标渲染器。持有 [`TextRenderer`]（内含 DirectWrite 工厂与测量缓存），故应长期复用。
pub struct IconRenderer {
    text: TextRenderer,
    /// 角标编码方式。
    pub shape: BadgeShape,
    /// 调试用：在左上角画 N 个点标出这是第几个尺寸档。
    ///
    /// `GetIcon` **没有尺寸参数**——图标多大由我们创建位图时决定，系统拿去后是否二次缩放
    /// 从接口上完全看不出来。开启本项部署一次，就能同时回答「系统挑了哪档」和
    /// 「有没有被缩放」两个问题，这是读代码推不出来的。
    pub size_marks: bool,
    /// 角标配色 `(中文标点, 英文标点)`，BGR。`None` = 与主字同色、跟随任务栏主题。
    ///
    /// 16px 下颜色远比形状好认，用了配色就不必再靠形状区分两态。代价是**不再跟随
    /// 明暗主题**，选色时要自己保证在浅色与深色任务栏上都有足够对比。
    pub badge_colors: Option<([u8; 3], [u8; 3])>,
    /// 演示模式：外圈跑马灯。纯粹展示"服务端渲染 + 定时重发"能做到什么，不表达状态。
    ///
    /// 开启后需要有人按帧推进 [`IconSpec::frame`] 并重新发布，否则画面是静止的——
    /// 渲染端只负责按相位画，不负责驱动时间。
    pub demo_animation: bool,
}

impl IconRenderer {
    /// 字体族与旧 C++ 实现保持一致，避免换渲染端时字形跟着变。
    const FONT_FAMILY: &'static str = "Microsoft YaHei UI";

    /// 主字字重，对齐旧 C++ 实现的 `DWRITE_FONT_WEIGHT_LIGHT`。
    ///
    /// 别用渲染器默认（400）：16px 下常规字重的汉字笔画会挤在一起，真机实测明显偏粗。
    /// 这里既是"看着更好"，也是"与用户习惯的旧图标一致"。
    const FONT_WEIGHT: i32 = 300;

    /// 主字字号 = 图标边长 − 本值，与旧 C++ 实现的 `fontSizeDIP = iconSize - 2` 一致。
    ///
    /// **这是基线，不为新表现让步。** 角标是新增的东西，它的代价由角标自己承担
    /// （靠挖空间隙叠加），而不是把主字整体缩小——早期版本为让位缩到 78%，
    /// 真机对比比旧图标明显小一圈。
    const FONT_SIZE_INSET: f32 = 2.0;

    /// 角标周围挖空的间隙，按图标边长取比例（16px 下约 1.1px）。
    ///
    /// 没有它，角标与主字笔画会糊成一团——第一轮原型的「满格主字 + 角标直接叠加」
    /// 就是这么废掉的。间隙让两者在视觉上分离，主字因而不必缩小。
    const BADGE_GAP: f32 = 0.07;

    /// 外圈厚度（按边长比例）。16px 下约 1.1px——再细就被抗锯齿吃没了。
    const RING_TH: f32 = 0.07;

    /// 跑马灯亮段占周长的比例。
    const MARQUEE_LEN: f32 = 0.28;

    /// 演示动画一圈多少帧。帧率由驱动方决定，这里只定义"转一圈需要几帧"。
    pub const DEMO_FRAMES_PER_CYCLE: u32 = 40;

    /// 默认角标配色 `(中文标点, 英文标点)`，BGR。
    ///
    /// 蓝 `#2288E0` / 橙 `#EE9922`：两者在浅色与深色任务栏上都够亮也够暗，
    /// 且色相相距足够远——16px 下角标只有几个像素，靠色相区分远比靠形状可靠。
    /// 不跟随明暗主题是刻意的，见 [`Self::badge_colors`]。
    pub const DEFAULT_BADGE_COLORS: ([u8; 3], [u8; 3]) = ([0xE0, 0x88, 0x22], [0x22, 0x99, 0xEE]);

    /// 墨迹居中的收敛阈值（像素）。
    ///
    /// 取 0.5 而不是更小，是因为 **0.5px 就是这条渲染管线的物理下限**：
    /// `IDWriteBitmapRenderTarget` 走 GDI 兼容渲染，基线被吸附到整像素，
    /// 亚像素的原点差异根本画不出区别（实测 y=0 与 y=-0.5 输出完全相同）。
    /// 阈值定得比这更小只会让迭代白跑满次数。
    const CENTER_TOL: f32 = 0.5;

    /// 居中最多重画几遍。
    ///
    /// 为什么不能一步到位：原点位移与墨迹位移**不是 1:1**（同上，基线吸附 + 包围盒按
    /// 整像素量），实测原点挪 3px 墨迹只挪 2px。单步牛顿必然欠冲——这正是第一版
    /// 只画两遍却仍偏 1.5px 的原因。三次足够收敛到 ±0.5px。
    const CENTER_MAX_PASSES: usize = 3;

    /// 量墨迹包围盒时的覆盖度门槛。抗锯齿边缘会向外洇出很淡的一圈，
    /// 全算进包围盒会让"边缘更虚的那一侧"显得更宽，反而把中心算偏。
    const INK_THRESHOLD: f32 = 0.10;

    pub fn new(shape: BadgeShape) -> Result<Self, String> {
        // 基准字号仅用于构造，实际每次渲染都按图标尺寸显式指定。
        let text = TextRenderer::new(Self::FONT_FAMILY, 16.0)?;
        Ok(Self {
            text,
            shape,
            size_marks: false,
            badge_colors: Some(Self::DEFAULT_BADGE_COLORS),
            demo_animation: false,
        })
    }

    /// 渲染一个变体，返回 `size_px × size_px` 的**非预乘** BGRA。
    ///
    /// `dark_theme` = 任务栏是暗色（图标应画成浅色）。
    pub fn render(&self, size_px: u16, dark_theme: bool, spec: &IconSpec) -> Vec<u8> {
        let n = size_px as usize;
        let has_badge = spec.punct != PunctBadge::None;

        let s = size_px as f32;
        let glyph = self.render_glyph_mask(size_px, spec);
        // clear 是角标外扩一圈后的形状，用来在主字上"挖"出间隙。
        // 没有它，角标会与主字笔画糊成一团（第一轮原型的方案 C 就是这么废掉的）。
        let (badge, clear) = if has_badge {
            self.render_badge_masks(size_px, spec.punct)
        } else {
            (Mask::new(n), Mask::new(n))
        };

        // 演示动画独立成层：它不表达状态，也不参与挖空，纯粹叠在最上面。
        let marquee = if self.demo_animation {
            let mut m = Mask::new(n);
            let phase = spec.frame as f32 / Self::DEMO_FRAMES_PER_CYCLE as f32;
            draw_ring_marquee(&mut m, s, s * Self::RING_TH, phase, Self::MARQUEE_LEN);
            m
        } else {
            Mask::new(n)
        };

        let fg: u8 = if dark_theme { 255 } else { 0 };
        let fg3 = [fg, fg, fg];
        // 角标可单独配色；未配色时与主字同色，退化为旧的单色行为。
        let badge_col = match (self.badge_colors, spec.punct) {
            (Some((cn, _)), PunctBadge::Chinese) => cn,
            (Some((_, en)), PunctBadge::English) => en,
            _ => fg3,
        };

        let mut out = vec![0u8; n * n * 4];
        for i in 0..n * n {
            // 三层 source-over，自下而上：主字（已挖空）→ 角标 → 演示动画。
            // 挖空必须发生在叠加之前，否则会把角标自己也挖掉。
            let g_a = glyph.get(i) * (1.0 - clear.get(i));
            let b_a = badge.get(i);
            let m_a = marquee.get(i);

            let ab = b_a + g_a * (1.0 - b_a);
            let a = m_a + ab * (1.0 - m_a);

            let mut alpha = (a * 255.0).round().clamp(0.0, 255.0) as u8;
            if spec.dimmed {
                alpha = ((alpha as u32 * 90) / 255) as u8;
            }

            if a > 0.0 {
                // 各层按覆盖度加权求色，再除以合成 alpha 还原成**非预乘**值——
                // 输出给 CreateIconIndirect 的 hbmColor 必须是非预乘的。
                for c in 0..3 {
                    let v = fg3[c] as f32 * g_a * (1.0 - b_a) * (1.0 - m_a)
                        + badge_col[c] as f32 * b_a * (1.0 - m_a)
                        + fg3[c] as f32 * m_a;
                    out[i * 4 + c] = (v / a).round().clamp(0.0, 255.0) as u8;
                }
            }
            out[i * 4 + 3] = alpha; // A（非预乘）
        }
        out
    }

    /// 主字蒙版：黑底画白字，取 max(R,G,B) 当覆盖度。
    ///
    /// 见模块文档——不能直接在透明缓冲上画字，那样文字会被按 alpha=0 预乘成全透明。
    ///
    /// ## 为什么要画两遍
    ///
    /// **版面盒 ≠ 墨迹盒。** `measure` 返回的是 DirectWrite 的行盒：宽含字符的左右边距、
    /// 高是 ascent + descent + lineGap。把行盒摆正中，字形在盒内本就不居中（CJK 字面
    /// 在 em 框里偏上，行间距又只加在下方），于是整个字肉眼可见地偏上偏左——加了最外圈
    /// 边框作参照后这一点特别明显，这正是本次要修的。
    ///
    /// 修法是先照旧画一遍，**从画出来的蒙版量真实墨迹的包围盒**，再按差量重画一遍。
    /// 不用别的办法的理由：DirectWrite 虽有 `GetOverhangMetrics`，但它给的是相对行盒的
    /// 溢出量、仍受行盒定义影响；而"字形实际点亮了哪些像素"才是我们要对齐的东西，
    /// 直接量输出是唯一不依赖任何度量约定的口径，换字体换字号都不会失准。
    ///
    /// 代价是每个变体多画一次（十个变体 ≈ 20 次小面积排版），只在状态变化时发生，可忽略。
    fn render_glyph_mask(&self, size_px: u16, spec: &IconSpec) -> Mask {
        let s = size_px as f32;

        // 字号**与角标有无完全无关**，恒等于旧 C++ 实现的取值。
        //
        // 两次踩坑都在这一行：先是为给角标让位把字缩到 78%（比旧图标明显小一圈），
        // 又因为按 has_badge 分档，导致英文态（无角标）走满格、中文态走 78%，
        // 每次中英切换字号肉眼可见地跳。图标统共一个字，它的尺寸就是基线本身。
        let font_size = s - Self::FONT_SIZE_INSET;
        let style = crate::text::dwrite::TextStyle::new(font_size).with_weight(Self::FONT_WEIGHT);

        // 第一遍按行盒粗定位。测量必须与绘制同一个 TextStyle——字重影响字宽，
        // 用 measure_text_sized（不带字重）测出来的宽度会与实际绘制不符。
        let m = self.text.measure(&spec.label, &style);
        let x0 = ((s - m.width) * 0.5).max(0.0);
        let y0 = ((s - m.height) * 0.5).max(0.0);
        let mut mask = self.draw_glyph_at(size_px, &style, &spec.label, x0, y0);

        // 逐次按墨迹残差校正。无墨迹（非 Windows 的 mock 后端）时 delta 为 None，直接跳过。
        //
        // 不必担心把字挤出画布：这里求的是"让墨迹盒正中"的位移，它同时也是让溢出最小的
        // 位移——原本装得下的必然仍装得下，原本装不下的也只会更好。
        let (mut ox, mut oy) = (0.0f32, 0.0f32);
        let mut err = Self::center_err(&mask, s);
        for _ in 0..Self::CENTER_MAX_PASSES {
            let Some((dx, dy)) = Self::ink_center_delta(&mask, s) else {
                break;
            };
            if dx.abs() <= Self::CENTER_TOL && dy.abs() <= Self::CENTER_TOL {
                break;
            }
            let (nx, ny) = (ox + dx, oy + dy);
            let next = self.draw_glyph_at(size_px, &style, &spec.label, x0 + nx, y0 + ny);
            let next_err = Self::center_err(&next, s);
            // 不再改善就收手并保留上一版。吸附使残差呈阶梯状，硬追下去会在两个
            // 相邻整像素位置之间来回跳，跑满次数还回到更差的那一边。
            if next_err >= err {
                break;
            }
            mask = next;
            err = next_err;
            (ox, oy) = (nx, ny);
        }

        if self.size_marks {
            Self::draw_size_marks(&mut mask, size_px);
        }
        mask
    }

    /// 在指定原点画一次主字，返回覆盖度蒙版。
    fn draw_glyph_at(
        &self,
        size_px: u16,
        style: &crate::text::dwrite::TextStyle,
        label: &str,
        x: f32,
        y: f32,
    ) -> Mask {
        let n = size_px as usize;
        // 黑色不透明底：GDI 需要不透明背景才能正确抗锯齿混合
        let mut buf = vec![0u8; n * n * 4];
        for px in buf.chunks_exact_mut(4) {
            px[3] = 255;
        }
        let _ = self.text.draw(
            &mut buf,
            size_px as u32,
            size_px as u32,
            x,
            y,
            label,
            style,
            [255, 255, 255, 255], // 白字（BGRA）
        );

        let mut mask = Mask::new(n);
        for i in 0..n * n {
            let b = buf[i * 4];
            let g = buf[i * 4 + 1];
            let r = buf[i * 4 + 2];
            // max 而非平均：保留抗锯齿边缘的过渡，与旧 C++ 实现同口径
            mask.v[i] = r.max(g).max(b) as f32 / 255.0;
        }
        mask
    }

    /// 偏心程度，用于比较两次尝试谁更居中。无墨迹时视为无穷差。
    fn center_err(m: &Mask, s: f32) -> f32 {
        Self::ink_center_delta(m, s).map_or(f32::INFINITY, |(dx, dy)| dx.abs().max(dy.abs()))
    }

    /// 求「把墨迹包围盒摆到 `s×s` 正中」所需的位移。无墨迹时返回 `None`。
    fn ink_center_delta(m: &Mask, s: f32) -> Option<(f32, f32)> {
        let n = m.n;
        let (mut x0, mut x1, mut y0, mut y1) = (usize::MAX, 0usize, usize::MAX, 0usize);
        for y in 0..n {
            for x in 0..n {
                if m.v[y * n + x] < Self::INK_THRESHOLD {
                    continue;
                }
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
        }
        if x0 == usize::MAX {
            return None;
        }
        // 包围盒按**像素边界**取值：第 x1 个像素的右边界是 x1+1。
        let cx = (x0 + x1 + 1) as f32 * 0.5;
        let cy = (y0 + y1 + 1) as f32 * 0.5;
        Some((s * 0.5 - cx, s * 0.5 - cy))
    }

    /// 角标蒙版与配套的「挖空」蒙版。
    ///
    /// 返回 `(badge, clear)`：`badge` 是角标本身，`clear` 是同一形状外扩
    /// [`Self::BADGE_GAP`] 后的版本。合成时先用 `clear` 从主字里挖掉一圈再叠 `badge`，
    /// 两者之间因而始终留有间隙，主字不必为角标缩小。
    fn render_badge_masks(&self, size_px: u16, punct: PunctBadge) -> (Mask, Mask) {
        let gap = size_px as f32 * Self::BADGE_GAP;
        (
            self.draw_badge(size_px, punct, 0.0),
            self.draw_badge(size_px, punct, gap),
        )
    }

    /// 按当前形状画角标；`expand > 0` 时整体外扩，用于生成挖空蒙版。
    fn draw_badge(&self, size_px: u16, punct: PunctBadge, expand: f32) -> Mask {
        let n = size_px as usize;
        let s = size_px as f32;
        let mut m = Mask::new(n);
        let cn = punct == PunctBadge::Chinese;
        // 挖空蒙版里所有中空形状都要退化为实心：环心若露出主字笔画，
        // 那一小截孤立的笔画比不挖还脏。
        let solid = expand > 0.0;

        match self.shape {
            BadgeShape::OuterRing => {
                // 外圈在最外围，与居中的主字几乎不重叠，故**不挖空**：
                // 挖了反而会削掉主字外缘一整圈，让字凭空变小。
                if expand > 0.0 {
                    return m;
                }
                draw_outer_ring(&mut m, s, s * Self::RING_TH, !cn);
            }
            BadgeShape::CornerTriangle => {
                // 直角边贴着图标边界，斜边朝向中心——同样占地下与主字的接触面比方块小。
                //
                // 0.34 是真机调过的：0.42 时三角在 16px 上压得太重，抢了主字的视觉重心
                // （用户原话「需要改小一点」）。再往下到 0.28 就开始糊成一个色块，
                // 直角三角的形状特征消失、与圆点无异。
                let leg = s * 0.34 + expand;
                let th = s * 0.09;
                // 空心只在**没有配色**时用来区分两态。配了色就一律实心：
                // 预览实测 16px 下空心三角只剩一条 1px 的细边，英文态几乎看不见，
                // 而颜色本身已经把两态分开了，形状不必再兼这份职责。
                //
                // 挖空蒙版恒实心，否则空心内腔会漏出主字笔画。
                let hollow = !cn && self.badge_colors.is_none() && !solid;
                draw_corner_triangle(&mut m, s, leg, hollow, th);
            }
            BadgeShape::BottomBar => {
                // 底部标记不占右下角，与主字笔画不打架
                let y = s - s * 0.11;
                let th = s * 0.055;
                if cn {
                    draw_rect(&mut m, s * 0.5, y, s * 0.32 + expand, th + expand);
                } else {
                    let r = th * 1.35;
                    draw_disc(&mut m, s * 0.31, y, r + expand, 0.0);
                    draw_disc(&mut m, s * 0.69, y, r + expand, 0.0);
                }
            }
            // 显式列出而非用 `_ =>` 通配：新增形状时编译器会在这里报不穷尽，
            // 强制你为它选一种画法，而不是默默掉进圆/环的分支画出个莫名其妙的东西。
            BadgeShape::CircleSquare | BadgeShape::RingDot => {
                // 右下角角标：基准半径取边长的 17%，16px 下直径约 5.4px
                let r = s * 0.17;
                let cx = s - r - s * 0.04;
                let cy = s - r - s * 0.04;
                if self.shape == BadgeShape::CircleSquare {
                    if cn {
                        draw_disc(&mut m, cx, cy, r * 0.92 + expand, 0.0);
                    } else {
                        draw_rect(&mut m, cx, cy, r * 0.74 + expand, r * 0.74 + expand);
                    }
                } else if cn {
                    let inner = if solid { 0.0 } else { r * 0.42 };
                    draw_disc(&mut m, cx, cy, r + expand, inner);
                } else {
                    draw_disc(&mut m, cx, cy, r * 0.72 + expand, 0.0);
                }
            }
        }
        m
    }

    /// 在左上角画 N 个点标出尺寸档下标（调试用，见 [`Self::size_marks`]）。
    fn draw_size_marks(m: &mut Mask, size_px: u16) {
        let idx = wind_ipc::protocol::ICON_SIZES
            .iter()
            .position(|&s| s == size_px)
            .unwrap_or(0);
        let r = (size_px as f32 * 0.05).max(0.6);
        for k in 0..=idx {
            let cx = r + 0.5 + k as f32 * (r * 2.0 + 1.0);
            draw_disc(m, cx, r + 0.5, r, 0.0);
        }
    }
}

/// 把当前状态渲染成全部变体并投送到共享内存。
///
/// 服务进程持有一个，状态变化时调 [`Self::publish`]。DLL 侧的通知走既有 push 通道
/// （`push_state_update` → `OnUpdate(TF_LBI_ICON)`），本类不负责通知。
#[cfg(windows)]
pub struct LangBarIconPublisher {
    renderer: IconRenderer,
    shm: wind_bridge::icon_shm_windows::IconShm,
    /// 上次发布的状态。图标更新是用户操作级频率，但状态推送比它频繁得多
    /// （焦点切换等也会推），没必要每次都重渲十张位图。
    last: Option<IconSpec>,
}

#[cfg(windows)]
impl LangBarIconPublisher {
    /// `suffix` 取 `wind_config::variant::pipe_suffix()`（`""` / `"_dev"`）。
    pub fn new(suffix: &str, shape: BadgeShape) -> Result<Self, String> {
        let renderer = IconRenderer::new(shape)?;
        let shm = wind_bridge::icon_shm_windows::IconShm::create(suffix)
            .map_err(|e| format!("创建图标共享内存失败: {e}"))?;
        Ok(Self {
            renderer,
            shm,
            last: None,
        })
    }

    /// 调试开关：在各档位图上烧尺寸标记，用于真机确认系统实际取用了哪一档。
    pub fn set_size_marks(&mut self, on: bool) {
        if self.renderer.size_marks != on {
            self.renderer.size_marks = on;
            self.last = None; // 呈现变了，下次必须重发
        }
    }

    pub fn size_marks(&self) -> bool {
        self.renderer.size_marks
    }

    /// 换角标编码方式。改这个不需要重新分发 DLL——这正是把渲染搬到服务端的收益。
    pub fn set_shape(&mut self, shape: BadgeShape) {
        if self.renderer.shape != shape {
            self.renderer.shape = shape;
            self.last = None;
        }
    }

    pub fn shape(&self) -> BadgeShape {
        self.renderer.shape
    }

    /// 角标彩色 / 与主字同色。关掉即退化为跟随明暗主题的单色行为。
    pub fn set_colored(&mut self, on: bool) {
        let next = on.then_some(IconRenderer::DEFAULT_BADGE_COLORS);
        if self.renderer.badge_colors != next {
            self.renderer.badge_colors = next;
            self.last = None;
        }
    }

    pub fn colored(&self) -> bool {
        self.renderer.badge_colors.is_some()
    }

    /// 渲染并发布。返回 `true` 表示确实写了新内容，`false` 表示状态未变已跳过。
    pub fn publish(&mut self, spec: &IconSpec) -> Result<bool, String> {
        if self.last.as_ref() == Some(spec) {
            return Ok(false);
        }

        // 用变体表驱动渲染，而不是另写一遍嵌套循环——两处循环各写一遍时，
        // 一旦变体表的顺序或档位变了而这里没跟上，图标就会张冠李戴
        // （某个尺寸档显示另一档的内容），且不会有任何报错。
        let table = wind_ipc::protocol::icon_variant_table();
        let mut bitmaps = Vec::with_capacity(table.len());
        for v in &table {
            let dark = v.theme == wind_ipc::protocol::ICON_THEME_DARK;
            bitmaps.push(self.renderer.render(v.size_px, dark, spec));
        }

        self.shm
            .publish(&bitmaps)
            .map_err(|e| format!("发布图标共享内存失败: {e}"))?;
        self.last = Some(spec.clone());
        Ok(true)
    }

    /// SHM 名（日志与排查用）。
    pub fn shm_name(&self) -> &str {
        self.shm.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 取某像素的 alpha（输出是非预乘 BGRA）。
    fn alpha_at(buf: &[u8], n: usize, x: usize, y: usize) -> u8 {
        buf[(y * n + x) * 4 + 3]
    }

    fn spec(punct: PunctBadge) -> IconSpec {
        IconSpec {
            punct,
            ..IconSpec::default()
        }
    }

    /// 每个尺寸档都要输出恰好 size×size×4 字节——SHM 变体表按这个长度切片，
    /// 少一个字节就会让后续所有变体错位。
    #[test]
    fn output_length_matches_every_declared_size() {
        let r = IconRenderer::new(BadgeShape::CircleSquare).expect("renderer");
        for &size in &wind_ipc::protocol::ICON_SIZES {
            let buf = r.render(size, false, &spec(PunctBadge::Chinese));
            assert_eq!(
                buf.len(),
                wind_ipc::protocol::icon_variant_bytes(size),
                "尺寸档 {size} 输出长度不符"
            );
        }
    }

    /// 主字尺寸不得随角标有无变化。
    ///
    /// 真机回归：早期版本按 has_badge 分了两档字号（有角标 78%、无角标满格），
    /// 而英文态恰好没有标点角标，于是每次中英切换字号都肉眼可见地跳一下。
    ///
    /// 只在 Windows 上跑：其它平台文本后端是 mock，画不出主字。
    #[cfg(windows)]
    #[test]
    fn glyph_size_does_not_change_with_badge() {
        let r = IconRenderer::new(BadgeShape::CircleSquare).expect("renderer");
        const N: usize = 32;

        // 主字的垂直跨度。只扫左侧 55% 的列，避开右下角的角标。
        let glyph_height = |punct: PunctBadge| -> usize {
            let buf = r.render(N as u16, false, &spec(punct));
            let mut top: Option<usize> = None;
            let mut bottom = 0usize;
            for y in 0..N {
                let inked = (0..(N * 55 / 100)).any(|x| buf[(y * N + x) * 4 + 3] > 0);
                if inked {
                    top.get_or_insert(y);
                    bottom = y;
                }
            }
            top.map_or(0, |t| bottom - t + 1)
        };

        let without = glyph_height(PunctBadge::None);
        let with = glyph_height(PunctBadge::Chinese);
        assert!(without > 0, "主字根本没画出来");
        assert_eq!(
            without, with,
            "主字高度随角标变化了——字号又按 has_badge 分档了？"
        );
    }

    /// 中文标点与英文标点必须画出**不同**的像素，否则角标形同虚设。
    ///
    /// 这条是整个功能的存在意义所在：曾经的实现里 `_bChinesePunct` 一路传到了 DLL、
    /// 也参与了重绘判据，唯独没进绘制——图标每次都重画，画出来的东西却一模一样。
    #[test]
    fn chinese_and_english_badges_differ() {
        let r = IconRenderer::new(BadgeShape::CircleSquare).expect("renderer");
        for &size in &wind_ipc::protocol::ICON_SIZES {
            let cn = r.render(size, false, &spec(PunctBadge::Chinese));
            let en = r.render(size, false, &spec(PunctBadge::English));
            assert_ne!(cn, en, "尺寸档 {size} 的中/英标点角标画出来是一样的");
        }
    }

    /// 三种编码方式两两不同——否则「换一种形状」的开关是空的。
    #[test]
    fn badge_shapes_produce_distinct_pixels() {
        let shapes = [
            BadgeShape::CircleSquare,
            BadgeShape::RingDot,
            BadgeShape::BottomBar,
            BadgeShape::CornerTriangle,
            BadgeShape::OuterRing,
        ];
        let mut rendered = Vec::new();
        for sh in shapes {
            let r = IconRenderer::new(sh).expect("renderer");
            rendered.push(r.render(24, false, &spec(PunctBadge::Chinese)));
        }
        for i in 0..rendered.len() {
            for j in (i + 1)..rendered.len() {
                assert_ne!(rendered[i], rendered[j], "形状 {i} 与 {j} 渲染结果相同");
            }
        }
    }

    /// 角标画在右下角象限，不能跑到主字中心去。
    #[test]
    fn corner_badge_lands_in_bottom_right_quadrant() {
        let r = IconRenderer::new(BadgeShape::CircleSquare).expect("renderer");
        let n = 32usize;
        let none = r.render(32, false, &spec(PunctBadge::None));
        let cn = r.render(32, false, &spec(PunctBadge::Chinese));

        // 右下角必须出现新的不透明像素
        let mut gained_bottom_right = 0;
        for y in (n * 3 / 4)..n {
            for x in (n * 3 / 4)..n {
                if alpha_at(&cn, n, x, y) > alpha_at(&none, n, x, y) {
                    gained_bottom_right += 1;
                }
            }
        }
        assert!(
            gained_bottom_right > 0,
            "右下角没有画出角标（新增不透明像素数为 0）"
        );
    }

    /// 变淡只压低 alpha，不改变颜色通道——旧实现里变淡与"显英文"是两种不同的表达，
    /// 混在一起会让「输入法被禁用」和「当前位置不可输入」看起来一样。
    #[test]
    fn dimmed_only_lowers_alpha() {
        let r = IconRenderer::new(BadgeShape::CircleSquare).expect("renderer");
        let normal = r.render(24, false, &spec(PunctBadge::Chinese));
        let dim = r.render(
            24,
            false,
            &IconSpec {
                dimmed: true,
                ..spec(PunctBadge::Chinese)
            },
        );
        assert_eq!(normal.len(), dim.len());
        for i in (0..normal.len()).step_by(4) {
            assert_eq!(normal[i], dim[i], "B 通道被改动");
            assert_eq!(normal[i + 1], dim[i + 1], "G 通道被改动");
            assert_eq!(normal[i + 2], dim[i + 2], "R 通道被改动");
            assert!(dim[i + 3] <= normal[i + 3], "变淡反而提高了 alpha");
        }
    }

    /// 暗色主题下图标画成浅色，亮色主题下画成深色。
    ///
    /// 只检查**有覆盖**的像素：多色合成只在 alpha>0 处写颜色，全透明像素的 RGB 留 0。
    /// 这对 32bpp alpha 图标无影响（系统按 alpha 取舍，RGB 被忽略），
    /// 但断言若不加这道过滤就会把"透明处没填前景色"误报成主题失效。
    ///
    /// **必须关掉配色**：彩色角标按设计就不跟随主题（见 `badge_colors`），
    /// 开着配色时角标像素两个主题下相同，本断言测的是主字那条单色通路。
    /// 角标不随主题变化本身另有 [`badge_colors_are_theme_independent`] 把关。
    #[test]
    fn theme_flips_foreground_channels() {
        let mut r = IconRenderer::new(BadgeShape::CircleSquare).expect("renderer");
        r.badge_colors = None;
        let light = r.render(24, false, &spec(PunctBadge::Chinese));
        let dark = r.render(24, true, &spec(PunctBadge::Chinese));
        let mut inked = 0;
        for i in (0..light.len()).step_by(4) {
            // alpha 与主题无关，两者必须逐像素相等
            assert_eq!(light[i + 3], dark[i + 3], "主题不应改变覆盖度");
            if light[i + 3] == 0 {
                continue;
            }
            inked += 1;
            assert_eq!(light[i], 0, "亮色主题应画深色前景");
            assert_eq!(dark[i], 255, "暗色主题应画浅色前景");
        }
        assert!(inked > 0, "整张图都是透明的，断言等于没跑");
    }

    /// 外圈方案不得侵占主字：它的全部价值就在于"一个像素都不用主字让"。
    ///
    /// 若哪天给它也加上挖空，主字外缘会被削掉一整圈、凭空变小，而这在小图标上
    /// 很难一眼看出是"被挖了"还是"字本来就小"。
    #[cfg(windows)]
    #[test]
    fn outer_ring_does_not_carve_into_glyph() {
        let r = IconRenderer::new(BadgeShape::OuterRing).expect("renderer");
        const N: usize = 32;
        let plain = r.render(N as u16, false, &spec(PunctBadge::None));
        let ringed = r.render(N as u16, false, &spec(PunctBadge::Chinese));

        // 只看内部区域（去掉最外 3 圈，那是外圈自己的地盘），主字应逐像素不变
        for y in 3..(N - 3) {
            for x in 3..(N - 3) {
                let i = (y * N + x) * 4 + 3;
                assert_eq!(plain[i], ringed[i], "外圈把主字内部像素改了 @({x},{y})");
            }
        }
    }

    /// 配色开启时角标不随主题变化——这是配色方案的**已知代价**，不是缺陷。
    ///
    /// 写成测试而不只写注释：一旦哪天有人"顺手"把角标也接上主题前景色，
    /// 选色时"在浅底与深底上都够醒目"这个前提就被悄悄换掉了，而画面上不易察觉。
    #[test]
    fn badge_colors_are_theme_independent() {
        let r = IconRenderer::new(BadgeShape::CornerTriangle).expect("renderer");
        // 无角标那版只用来划出「角标独占像素」，覆盖度与主题无关，取一份即可
        let none_l = r.render(24, false, &spec(PunctBadge::None));
        let cn_l = r.render(24, false, &spec(PunctBadge::Chinese));
        let cn_d = r.render(24, true, &spec(PunctBadge::Chinese));
        // 只看「加了角标才出现覆盖」的像素，绕开主字（主字是跟随主题的单色）
        let mut checked = 0;
        for i in (0..cn_l.len()).step_by(4) {
            if cn_l[i + 3] == 0 || none_l[i + 3] > 0 {
                continue;
            }
            checked += 1;
            assert_eq!(
                cn_l[i..i + 3],
                cn_d[i..i + 3],
                "角标颜色随主题变了——配色的前提是两个主题共用一组颜色"
            );
        }
        assert!(checked > 0, "没有找到角标独占像素，断言等于没跑");
    }

    /// 主字墨迹必须落在图标正中。
    ///
    /// 回归的是「按行盒居中」那版：行盒高含 lineGap 且只加在下方，CJK 字面在 em 框里
    /// 又偏上，两者叠加使字整体偏上——加了最外圈边框后一眼可见。
    ///
    /// 容差 0.75px：GDI 兼容渲染把基线吸附到整像素，可达位置本就是 1px 一档，
    /// 加上包围盒按整像素量的半像素误差，0.5 已是物理下限。再紧就是在测噪声。
    #[cfg(windows)]
    #[test]
    fn glyph_ink_is_centered() {
        let r = IconRenderer::new(BadgeShape::CornerTriangle).expect("renderer");
        for label in ["中", "英", "拼", "五"] {
            for &size in &wind_ipc::protocol::ICON_SIZES {
                let s = size as f32;
                let mask = r.render_glyph_mask(
                    size,
                    &IconSpec {
                        label: label.to_string(),
                        ..spec(PunctBadge::None)
                    },
                );
                let (dx, dy) =
                    IconRenderer::ink_center_delta(&mask, s).expect("主字没画出来，无法量墨迹");
                assert!(
                    dx.abs() <= 0.75 && dy.abs() <= 0.75,
                    "「{label}」在 {size}px 下未居中：残余位移 ({dx:.2}, {dy:.2})"
                );
            }
        }
    }

    /// 形状下标往返必须自洽——菜单命令只传一个 u8，映射错位就是「点了另一个形状」。
    #[test]
    fn badge_shape_index_roundtrips() {
        for (i, sh) in BadgeShape::ALL.iter().enumerate() {
            assert_eq!(sh.index() as usize, i, "{sh:?} 的下标与 ALL 中的位置不符");
            assert_eq!(BadgeShape::from_index(i as u8), *sh);
        }
        // 越界回落到默认，不 panic：id 由另一个进程回传，不能假定合法
        assert_eq!(
            BadgeShape::from_index(BadgeShape::ALL.len() as u8),
            BadgeShape::default()
        );
    }

    /// 演示动画：相位不同必须画出不同像素，否则"动画"是静止的。
    #[test]
    fn demo_animation_frames_differ() {
        let mut r = IconRenderer::new(BadgeShape::CircleSquare).expect("renderer");
        r.demo_animation = true;
        let at = |frame: u32| {
            r.render(
                24,
                false,
                &IconSpec {
                    frame,
                    ..spec(PunctBadge::Chinese)
                },
            )
        };
        // 取相隔四分之一周期的两帧，跑马灯应转过约 90°
        let quarter = IconRenderer::DEMO_FRAMES_PER_CYCLE / 4;
        assert_ne!(at(0), at(quarter), "动画两帧完全相同——相位没生效");
        // 整周期回到原点
        assert_eq!(
            at(0),
            at(IconRenderer::DEMO_FRAMES_PER_CYCLE),
            "转满一圈没有回到起始帧"
        );
    }

    /// 关闭演示动画时，相位不得影响画面——否则状态推送会被无谓的重发刷屏。
    #[test]
    fn frame_is_ignored_when_demo_off() {
        let r = IconRenderer::new(BadgeShape::CircleSquare).expect("renderer");
        let a = r.render(24, false, &spec(PunctBadge::Chinese));
        let b = r.render(
            24,
            false,
            &IconSpec {
                frame: 7,
                ..spec(PunctBadge::Chinese)
            },
        );
        assert_eq!(a, b, "演示动画关闭时相位仍改变了画面");
    }

    /// 手动预览工具：把各形状渲染成对比图，供肉眼比选后再决定部署哪种。
    ///
    /// 部署一次要 UAC 提权并重启输入法，逐个形状试成本太高；而这些参数
    /// （形状、配色、间隙、字重）恰恰只能靠看。默认 `#[ignore]`，不进常规测试。
    ///
    /// ```text
    /// cargo test -p wind-ui --lib dump_preview -- --ignored --nocapture
    /// ```
    /// 输出目录由 `WIND_ICON_PREVIEW_DIR` 指定，缺省为系统临时目录。
    #[cfg(windows)]
    #[test]
    #[ignore = "手动预览工具，不参与常规测试"]
    fn dump_preview() {
        use image::{Rgba, RgbaImage};

        let dir = std::env::var("WIND_ICON_PREVIEW_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        std::fs::create_dir_all(&dir).expect("创建输出目录");

        const ZOOM: u32 = 9;
        const PAD: u32 = 10;
        let sizes: [u16; 2] = [16, 24];
        let shapes = [
            ("1_corner_triangle", BadgeShape::CornerTriangle),
            ("2_outer_ring", BadgeShape::OuterRing),
            ("3_circle_square", BadgeShape::CircleSquare),
            ("4_ring_dot", BadgeShape::RingDot),
            ("5_bottom_bar", BadgeShape::BottomBar),
        ];
        // 取实际出货的那组配色，别在预览里另写一份——预览与真机不同色时，
        // 肉眼比选出来的结论根本不适用于装机后的样子。
        let colors = IconRenderer::DEFAULT_BADGE_COLORS;

        // 把一个变体贴到画布上（BGRA→RGBA，最近邻放大），底色模拟任务栏
        let blit = |img: &mut RgbaImage, px: &[u8], n: u32, ox: u32, oy: u32, dark: bool| {
            let bg = if dark { 0x20u8 } else { 0xF3u8 };
            for y in 0..n * ZOOM {
                for x in 0..n * ZOOM {
                    let i = ((y / ZOOM) * n + (x / ZOOM)) as usize * 4;
                    let (b, g, r, a) = (px[i], px[i + 1], px[i + 2], px[i + 3] as u32);
                    let mix = |c: u8| ((c as u32 * a + bg as u32 * (255 - a)) / 255) as u8;
                    img.put_pixel(ox + x, oy + y, Rgba([mix(r), mix(g), mix(b), 255]));
                }
            }
        };

        // ── 图一：形状对比。每行一种形状；列 = {16,24}px × {中,英} × {浅,深} ──
        let row_h = 24 * ZOOM + PAD;
        let width = PAD
            + sizes
                .iter()
                .map(|s| 4 * (*s as u32 * ZOOM + PAD))
                .sum::<u32>();
        let mut img = RgbaImage::from_pixel(
            width,
            PAD + shapes.len() as u32 * row_h,
            Rgba([255, 255, 255, 255]),
        );
        for (ri, (name, shape)) in shapes.iter().enumerate() {
            let mut r = IconRenderer::new(*shape).expect("renderer");
            r.badge_colors = Some(colors);
            let y = PAD + ri as u32 * row_h;
            let mut x = PAD;
            for &size in &sizes {
                let n = size as u32;
                for col in 0..4 {
                    let cn = col % 2 == 0;
                    let dark = col >= 2;
                    let punct = if cn {
                        PunctBadge::Chinese
                    } else {
                        PunctBadge::English
                    };
                    let px = r.render(size, dark, &spec(punct));
                    blit(&mut img, &px, n, x, y + (24 * ZOOM - n * ZOOM) / 2, dark);
                    x += n * ZOOM + PAD;
                }
            }
            println!("row {ri}: {name}");
        }
        let p = dir.join("icon_shapes.png");
        img.save(&p).expect("保存 icon_shapes.png");
        println!("wrote {}", p.display());
        println!("cols: [16px] 中/浅 英/浅 中/深 英/深 | [24px] 同上");

        // ── 图二：演示动画一圈的帧序列 ──
        let mut r = IconRenderer::new(BadgeShape::CornerTriangle).expect("renderer");
        r.badge_colors = Some(colors);
        r.demo_animation = true;
        let frames = 8u32;
        let step = IconRenderer::DEMO_FRAMES_PER_CYCLE / frames;
        let n = 24u32;
        let mut anim = RgbaImage::from_pixel(
            PAD + frames * (n * ZOOM + PAD),
            PAD * 2 + 2 * (n * ZOOM + PAD),
            Rgba([255, 255, 255, 255]),
        );
        for f in 0..frames {
            let s = IconSpec {
                frame: f * step,
                ..spec(PunctBadge::Chinese)
            };
            for (ri, dark) in [false, true].into_iter().enumerate() {
                let px = r.render(n as u16, dark, &s);
                blit(
                    &mut anim,
                    &px,
                    n,
                    PAD + f * (n * ZOOM + PAD),
                    PAD + ri as u32 * (n * ZOOM + PAD),
                    dark,
                );
            }
        }
        let p = dir.join("icon_anim.png");
        anim.save(&p).expect("保存 icon_anim.png");
        println!(
            "wrote {} （上排浅底、下排深底，左→右为一圈的 8 帧）",
            p.display()
        );
    }

    /// 尺寸档标记开启时，各档左上角画的点数不同——这是真机验证"系统用了哪档"的依据。
    #[test]
    fn size_marks_differ_per_size_tier() {
        let mut r = IconRenderer::new(BadgeShape::CircleSquare).expect("renderer");
        r.size_marks = true;
        let a = r.render(16, false, &spec(PunctBadge::None));
        let b = r.render(16, false, &spec(PunctBadge::None));
        assert_eq!(a, b, "同一档两次渲染应完全一致");

        // 不同档之间左上角的点数不同（此处只验证渲染不 panic 且长度正确，
        // 点数差异靠真机肉眼判读——这正是这个标记存在的理由）
        for &size in &wind_ipc::protocol::ICON_SIZES {
            let buf = r.render(size, false, &spec(PunctBadge::None));
            assert_eq!(buf.len(), wind_ipc::protocol::icon_variant_bytes(size));
        }
    }
}
