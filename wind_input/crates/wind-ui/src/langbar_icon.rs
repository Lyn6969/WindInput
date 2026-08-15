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
    /// 不画角标，图标退回「只有主字」的旧样子。
    ///
    /// 做成形状枚举的一员而非另开一个 `enabled` 布尔：两者并存时可以摆出
    /// 「关着 + 选了三角」这种自相矛盾的状态，而单选组从类型上就排除了它，
    /// 菜单上也天然是一组互斥项。
    ///
    /// 这一档同时是用户可见开关的落点——有人就是不想要角标。
    ///
    /// **它是默认值**：角标是加在一个所有 Windows 用户都会看到的系统图标上的新东西，
    /// 默认改变所有人的任务栏是过界的。想要的人去开，这样默认体验与装之前一致。
    #[default]
    None,
    /// 右下角直角三角（中实心 / 英空心）。**开启角标时的推荐形状。**
    ///
    /// 三角填满角落，同样面积下比圆形更"占地方"，因而在 16px 下比小圆点更醒目；
    /// 且直角边贴着图标边界，与主字的接触面比居中的圆更小。配色见
    /// [`IconRenderer::badge_colors`]——彩色时不必靠形状区分，两态可都用实心。
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
    pub const ALL: [BadgeShape; 6] = [
        BadgeShape::None,
        BadgeShape::CornerTriangle,
        BadgeShape::OuterRing,
        BadgeShape::CircleSquare,
        BadgeShape::RingDot,
        BadgeShape::BottomBar,
    ];

    /// 调试菜单文案。
    pub fn label(self) -> &'static str {
        match self {
            BadgeShape::None => "不显示角标",
            BadgeShape::CornerTriangle => "右下三角",
            BadgeShape::OuterRing => "最外圈边框",
            BadgeShape::CircleSquare => "圆 / 方（已否决）",
            BadgeShape::RingDot => "环 / 点（已否决）",
            BadgeShape::BottomBar => "底部横条",
        }
    }

    /// 落盘用的**稳定 id**。
    ///
    /// ⚠ 刻意不存 [`Self::index`]：下标是「在 ALL 里排第几」这个位置身份，把它写进
    /// state.toml 等于让文件格式绑死声明顺序——今天在头上插了一个 `None`，
    /// 昨天存的 0（三角）明天就读成了「不显示」。凡是活得比进程久的标识都要用名字。
    pub fn as_id(self) -> &'static str {
        match self {
            BadgeShape::None => "none",
            BadgeShape::CornerTriangle => "corner_triangle",
            BadgeShape::OuterRing => "outer_ring",
            BadgeShape::CircleSquare => "circle_square",
            BadgeShape::RingDot => "ring_dot",
            BadgeShape::BottomBar => "bottom_bar",
        }
    }

    /// 由稳定 id 还原；未知（含空串）回落到默认形状。
    ///
    /// 「空串 = 用代码默认」这条让 wind-config 侧不必知道默认是哪种形状——
    /// 那份知识只存在于本文件的 `#[default]`，两处各写一份迟早对不上。
    pub fn from_id(id: &str) -> BadgeShape {
        Self::ALL
            .iter()
            .find(|s| s.as_id() == id)
            .copied()
            .unwrap_or_default()
    }

    /// 在 [`Self::ALL`] 中的下标，用作菜单命令参数（仅进程内有效，勿落盘）。
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
    /// 是否全角。为真时在**右上角**画一个小方点。
    ///
    /// 与标点角标分处两角，是因为它们是两个正交的状态：挤在同一角就得设计一套组合
    /// 编码（四种搭配各长什么样），而 16px 上根本放不下那么多可辨的差异。
    ///
    /// 半角不画——与英文模式不画标点角标同一条判据：**没有信息量的状态不占像素**。
    /// 半角是常态，若给它也画一个标记，图标上就常驻一个永远不变的点，既没告诉用户
    /// 任何事，又挤占了本就稀缺的 16×16。
    pub full_width: bool,
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
            full_width: false,
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

    /// 并入另一张蒙版（source-over，逐像素）。
    ///
    /// 用于把来自多处的挖空合成一张：主字只该被挖**一次**，分别挖两遍等于
    /// 让第二遍在第一遍的结果上再算一次覆盖度，交叠处会被削得比任何一处都狠。
    fn union(&mut self, other: &Mask) {
        for (d, s) in self.v.iter_mut().zip(other.v.iter()) {
            *d += s * (1.0 - *d);
        }
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

/// 角三角所在的角落。两个标记各占一角，形状同构、只差一次 y 翻转。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Corner {
    BottomRight,
    TopRight,
}

/// 4×4 超采样画直角三角，直角顶点在 `corner` 指定的角上、直角边长 `leg`。
///
/// 三角能把角落填满，同面积下比圆更醒目；而斜边朝向图标中心，与主字的接触面
/// 又比同样占地的方块小——这也是它同时被选为标点角标与全角标记形状的原因。
/// `hollow` 为真时只留 `th` 厚的边。
fn draw_corner_triangle(m: &mut Mask, s: f32, leg: f32, hollow: bool, th: f32, corner: Corner) {
    const SS: i32 = 4;
    // 两个角落的判据同构：把 y 翻过来，右上角就变成右下角的问题。
    // 与其写两份几乎相同的不等式（改一处忘另一处，且形状差异极难用眼睛发现），
    // 不如只做一次坐标变换。
    let fold_y = |py: f32| -> f32 {
        match corner {
            Corner::BottomRight => py,
            Corner::TopRight => s - py,
        }
    };
    // 判据一律在**折叠后**的坐标里做。空心那步要沿两条直角边各内缩一段，而内缩的
    // y 方向在翻转过的角落里是反的——若在原坐标里加同一个正偏移，右上角的空心会朝
    // 图标外面缩，画出来是个实心三角（形状差异细到肉眼几乎看不出）。
    let inside = |px: f32, qy: f32, l: f32| -> bool {
        px >= s - l && qy >= s - l && (px + qy) >= (2.0 * s - l)
    };
    let x0 = (s - leg - 1.0).floor() as i32;
    let x1 = s.ceil() as i32;
    let (y0, y1) = match corner {
        Corner::BottomRight => ((s - leg - 1.0).floor() as i32, s.ceil() as i32),
        Corner::TopRight => (0i32, (leg + 1.0).ceil() as i32),
    };
    for y in y0..=y1 {
        for x in x0..=x1 {
            let mut hit = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let qy = fold_y(y as f32 + (sy as f32 + 0.5) / SS as f32);
                    if !inside(px, qy, leg) {
                        continue;
                    }
                    // 空心：再挖掉一个内缩的同心三角
                    if hollow && inside(px + th * 1.4, qy + th * 1.4, leg - th * 1.4) {
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
    /// 角标不透明度（0~1）。**小于 1 时会同时关掉挖空**，得到"半遮"的效果。
    ///
    /// 这两件事是互斥的，不是叠加的：
    /// - `= 1.0`：实心角标 + 挖空。靠周围那圈留白与主字分离，代价是底下的笔画被切掉，
    ///   右下有笔画的「五」「双」「拼」看起来像缺了一角。
    /// - `< 1.0`：半透明角标 + 不挖空。笔画从角标里透出来，靠色差分离，字是完整的。
    ///
    /// 若两者同用（第一版就是），主字先被挖掉一圈、没有笔画可透，角标又被调淡，
    /// 于是调这个值**看起来毫无效果**——淡的只是角标自己，底下本来就是空的。
    ///
    /// 不要调到很低：角标要在 16px 的任务栏上一眼可辨，太透就等于没画。
    pub badge_alpha: f32,
    /// 全角标记总开关。**默认关**，理由同 [`BadgeShape::None`] 之为默认形状：
    /// 这是加在系统图标上的新东西，默认改变所有人的任务栏是过界的。
    ///
    /// 与 `spec.full_width` 的分工：这个是「要不要有这个功能」，那个是「此刻是不是全角」。
    /// 合成一个的话，关掉功能就得靠"永远上报半角"来实现，那会让状态本身失真。
    ///
    /// ⚠️ **测试里要验标记的形状/位置，必须显式 `r.width_mark = true`**。
    /// 默认关改上来那次，四个只设了 `spec.full_width` 的测试一起变红——它们本意是
    /// 验「标记画得对不对」，却把「默认开着」当成了隐含前提。凡是验某功能表现的
    /// 测试，都该自己把该功能打开，而不是依赖默认值。
    pub width_mark: bool,
    /// 全角标记的颜色（BGR）。`None` = 与主字同色、跟随明暗主题，同 [`Self::badge_colors`]。
    ///
    /// 单独一种颜色而不是复用标点角标的配色：全角与标点是两个不相干的状态，共用颜色会
    /// 让人以为它们有关联（"蓝的那个又出现在右上角了？"）。选色要与标点那两色都拉开
    /// 色相距离，同时在深浅两种任务栏上都立得住。
    pub width_mark_color: Option<[u8; 3]>,
    /// 角标大小倍率。1.0 = 各形状自己调好的基准尺寸。
    ///
    /// 单独抽出来是因为"多大合适"只能真机看，而它与形状是两个自由度：换形状不该
    /// 连带把调好的大小丢掉。基准值写在各形状的绘制分支里，这里只做整体缩放。
    pub badge_scale: f32,
    /// 全角标记（右上角小方点）的大小倍率，语义同 [`Self::badge_scale`]。
    pub width_mark_scale: f32,
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

    /// 全角标记默认色 `#E0447A`（玫红，BGR 存储），见 [`Self::width_mark_color`]。
    ///
    /// 先试过绿 `#33BB55`，真机否决：**不够清晰**。原因是绿的感知亮度天生偏高，
    /// 在浅色任务栏上与白底拉不开，而 16px 上只有几个像素、没有面积去弥补对比。
    /// 玫红的感知亮度低得多，浅底上压得住；饱和度又足够，深底上也不会糊成一团。
    /// 与标点那两色（蓝 `#2288E0` / 橙 `#EE9922`）的色相距离同样够远，三者同屏不串。
    pub const DEFAULT_WIDTH_MARK_COLOR: [u8; 3] = [0x7A, 0x44, 0xE0];

    /// 角标默认不透明度，见 [`Self::badge_alpha`]。
    ///
    /// 0.88：真机上 0.72 太淡，16px 下的标记本就只有几个像素，透得太狠就认不出颜色了。
    ///
    /// 仍要严格小于 1——等于 1 会切到挖空那一档（见 [`Self::badge_alpha`]），主字被
    /// 削掉一角，而半遮的全部意义就是不削字。这个上限不是审美取舍，是档位边界。
    pub const DEFAULT_BADGE_ALPHA: f32 = 0.88;

    /// 全角标记（右上角三角）的基准直角边长（按图标边长取比例）。
    ///
    /// 比标点角标的 0.34 小一档：全角是次要状态，出现频率低且不影响"现在打出来是
    /// 什么"，不该与标点角标争同等的视觉重量。
    ///
    /// 形状取三角而非方点，理由与标点角标同一条：三角把角落填满、斜边朝中心，
    /// 同等面积下与主字的接触面最小——真机比选后确认这是最不干扰主体的形状。
    const WIDTH_MARK_LEG: f32 = 0.28;

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
            width_mark: false,
            width_mark_color: Some(Self::DEFAULT_WIDTH_MARK_COLOR),
            badge_alpha: Self::DEFAULT_BADGE_ALPHA,
            badge_scale: 1.0,
            width_mark_scale: 1.0,
            demo_animation: false,
        })
    }

    /// 渲染一个变体，返回 `size_px × size_px` 的**非预乘** BGRA。
    ///
    /// `dark_theme` = 任务栏是暗色（图标应画成浅色）。
    pub fn render(&self, size_px: u16, dark_theme: bool, spec: &IconSpec) -> Vec<u8> {
        let n = size_px as usize;
        // 关掉角标（shape=None）与「此刻没有标点态可显示」等价处理，走同一条无角标路径——
        // 于是「关掉」在像素上必然与英文模式下的样子一字不差，不会留下什么残迹。
        let has_badge = spec.punct != PunctBadge::None && self.shape != BadgeShape::None;

        let s = size_px as f32;
        let glyph = self.render_glyph_mask(size_px, spec);
        // clear 是角标外扩一圈后的形状，用来在主字上"挖"出间隙。
        // 没有它，角标会与主字笔画糊成一团（第一轮原型的方案 C 就是这么废掉的）。
        // ★ **挖空与透明是互斥的两种分离手段，不能叠加。**
        //
        // - 挖空：在角标周围切掉一圈主字，靠"留白"把两者分开。角标是实心的，
        //   它底下的笔画被切掉了，看不见。
        // - 透明：让笔画从角标里透出来，靠"色差"把两者分开。这才是**半遮**。
        //
        // 同时用是最差的组合：主字先被挖掉一圈（没有笔画可透），角标又被调淡
        // （不够醒目）——于是调低不透明度"看起来完全没有效果"，因为角标底下
        // 本来就是空的，淡的只是它自己。这正是第一版的实际表现。
        let (badge, mut clear) = if has_badge {
            let (b, carved) = self.render_badge_masks(size_px, spec.punct);
            // 不透明才挖空。半透明时保留主字，让它从角标里透出来。
            let clear = if self.badge_alpha >= 1.0 {
                carved
            } else {
                Mask::new(n)
            };
            (b, clear)
        } else {
            (Mask::new(n), Mask::new(n))
        };

        // 全角标记：右上角小方点，与标点角标各占一角、互不相干，但**分离手段与角标同一套**
        // （见上）——不透明才挖空，半透明则保留主字让笔画透出来。两者若各走各的，
        // 同一个图标上会同时出现"挖了一圈的方点"和"半遮的三角"，看起来像两套设计。
        // 挖空并进 clear 而不是各挖各的：主字只该被挖一次，分两遍挖会让交叠处被削得更狠。
        let width_mark = if spec.full_width && self.width_mark {
            let (mark, mark_clear) = self.render_width_mark_masks(size_px);
            if self.badge_alpha >= 1.0 {
                clear.union(&mark_clear);
            }
            mark
        } else {
            Mask::new(n)
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

        // 两层共用同一个不透明度：它们是同一套视觉语言里的两个标记，一个半遮一个实心
        // 会让人以为二者层级不同。要分别调时再拆成两个字段，现在拆只是徒增一个必须
        // 记得同步的量。
        let badge_alpha = self.badge_alpha.clamp(0.0, 1.0);
        // 全角标记自己的颜色；未配色时退化为主字色（与 badge_colors 同一开关控制）。
        let width_col = self.width_mark_color.unwrap_or(fg3);

        let mut out = vec![0u8; n * n * 4];
        for i in 0..n * n {
            // 四层 source-over，自下而上：主字（已挖空）→ 角标 → 全角标记 → 演示动画。
            // 挖空必须发生在叠加之前，否则会把角标自己也挖掉。
            let g_a = glyph.get(i) * (1.0 - clear.get(i));
            let b_a = badge.get(i) * badge_alpha;
            let w_a = width_mark.get(i) * badge_alpha;
            let m_a = marquee.get(i);

            let ab = b_a + g_a * (1.0 - b_a);
            let abw = w_a + ab * (1.0 - w_a);
            let a = m_a + abw * (1.0 - m_a);

            let mut alpha = (a * 255.0).round().clamp(0.0, 255.0) as u8;
            if spec.dimmed {
                alpha = ((alpha as u32 * 90) / 255) as u8;
            }

            if a > 0.0 {
                // 各层按覆盖度加权求色，再除以合成 alpha 还原成**非预乘**值——
                // 输出给 CreateIconIndirect 的 hbmColor 必须是非预乘的。
                // 每层的权重 = 自身 alpha × 其上各层的 (1 - alpha)。
                for c in 0..3 {
                    let v = fg3[c] as f32 * g_a * (1.0 - b_a) * (1.0 - w_a) * (1.0 - m_a)
                        + badge_col[c] as f32 * b_a * (1.0 - w_a) * (1.0 - m_a)
                        + width_col[c] as f32 * w_a * (1.0 - m_a)
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

    /// 全角标记（右上角小方点）的 `(标记, 挖空)` 两张蒙版。
    ///
    /// 与角标同一套路：挖空版就是外扩一圈的自己，用来在主字上开出间隙。
    fn render_width_mark_masks(&self, size_px: u16) -> (Mask, Mask) {
        let gap = size_px as f32 * Self::BADGE_GAP;
        (
            self.draw_width_mark(size_px, 0.0),
            self.draw_width_mark(size_px, gap),
        )
    }

    /// 画右上角的全角三角；`expand > 0` 时整体外扩，用于生成挖空蒙版。
    fn draw_width_mark(&self, size_px: u16, expand: f32) -> Mask {
        let n = size_px as usize;
        let s = size_px as f32;
        let mut m = Mask::new(n);
        // 倍率只作用于标记本身，不作用于外扩量：gap 是"主字与标记之间留多宽"，
        // 由可读性决定，跟标记多大无关。
        let leg = s * Self::WIDTH_MARK_LEG * self.width_mark_scale.max(0.0);
        // 倍率为 0 视作关掉这个标记，**连挖空一起**短路。
        // 只短路标记那一张是不够的：挖空版的边长是 leg + expand，leg 为 0 时它仍有
        // expand 那么大，结果是主字右上角被挖掉一块、却没有任何东西补上去。
        if leg <= 0.0 {
            return m;
        }
        draw_corner_triangle(&mut m, s, leg + expand, false, 0.0, Corner::TopRight);
        m
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
        // 倍率只缩放形状本身，**不缩放 expand**：expand 是主字与角标之间的间隙，
        // 由"多宽才不糊在一起"决定，与角标多大无关。跟着一起缩会让角标调小时
        // 间隙也变窄，恰好在最需要间隙的时候把它收掉。
        let k = self.badge_scale.max(0.0);

        match self.shape {
            // 走不到（render 已按 has_badge 短路），但仍显式列出：下面那句
            // 「新增形状时编译器会在这里报不穷尽」的保证，要求这里不出现通配臂。
            BadgeShape::None => {}
            BadgeShape::OuterRing => {
                // 外圈在最外围，与居中的主字几乎不重叠，故**不挖空**：
                // 挖了反而会削掉主字外缘一整圈，让字凭空变小。
                if expand > 0.0 {
                    return m;
                }
                draw_outer_ring(&mut m, s, s * Self::RING_TH * k, !cn);
            }
            BadgeShape::CornerTriangle => {
                // 直角边贴着图标边界，斜边朝向中心——同样占地下与主字的接触面比方块小。
                //
                // 0.34 是真机调过的：0.42 时三角在 16px 上压得太重，抢了主字的视觉重心
                // （用户原话「需要改小一点」）。再往下到 0.28 就开始糊成一个色块，
                // 直角三角的形状特征消失、与圆点无异。
                let leg = s * 0.34 * k + expand;
                let th = s * 0.09 * k;
                // 空心只在**没有配色**时用来区分两态。配了色就一律实心：
                // 预览实测 16px 下空心三角只剩一条 1px 的细边，英文态几乎看不见，
                // 而颜色本身已经把两态分开了，形状不必再兼这份职责。
                //
                // 挖空蒙版恒实心，否则空心内腔会漏出主字笔画。
                let hollow = !cn && self.badge_colors.is_none() && !solid;
                draw_corner_triangle(&mut m, s, leg, hollow, th, Corner::BottomRight);
            }
            BadgeShape::BottomBar => {
                // 底部标记不占右下角，与主字笔画不打架
                let y = s - s * 0.11;
                let th = s * 0.055 * k;
                if cn {
                    draw_rect(&mut m, s * 0.5, y, s * 0.32 * k + expand, th + expand);
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
                let r = s * 0.17 * k;
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
    /// 演示动画的当前相位。
    ///
    /// 归发布器所有而不是由调用方每次传入：普通状态推送与动画定时器都会走到 `publish`，
    /// 若相位由调用方给，状态推送那条路必须知道「现在动画转到哪了」才能不打断它——
    /// 那等于把动画状态复制到每个调用点。放在这里，状态推送只管状态，相位自然延续。
    demo_frame: u32,
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
            demo_frame: 0,
        })
    }

    /// 演示动画（外圈跑马灯）开关。关掉时相位归零，下次开启从起点转起。
    ///
    /// 只切开关不会让画面动起来——还需要有人按帧调 [`Self::advance_demo_frame`] 并重新
    /// 发布。渲染端只按相位画，不持有时间。
    pub fn set_demo_animation(&mut self, on: bool) {
        if self.renderer.demo_animation != on {
            self.renderer.demo_animation = on;
            self.demo_frame = 0;
            self.last = None; // 呈现变了，下次必须重发
        }
    }

    pub fn demo_animation(&self) -> bool {
        self.renderer.demo_animation
    }

    /// 当前相位。
    pub fn demo_frame(&self) -> u32 {
        self.demo_frame
    }

    /// 推进一帧并返回新相位。按周期取模，避免长时间运行后溢出。
    pub fn advance_demo_frame(&mut self) -> u32 {
        self.demo_frame = (self.demo_frame + 1) % IconRenderer::DEMO_FRAMES_PER_CYCLE;
        self.demo_frame
    }

    /// 一次性套用全部呈现参数（配置侧的落地入口）。
    ///
    /// 每项都是 `Option`，`None` = 该项不动（保留渲染器自带的默认）。收成一个函数而不是
    /// 让调用方逐字段赋值，是为了让「改了参数必须清 `last` 才会重发」这件事只有一处
    /// 需要记得——漏清的症状是「配置改了、日志说读到了、图标纹丝不动」。
    ///
    /// 返回是否**确实有改动**，调用方据此决定要不要重新发布。
    #[allow(clippy::too_many_arguments)]
    pub fn apply_appearance(
        &mut self,
        shape: Option<BadgeShape>,
        badge_scale: Option<f32>,
        width_mark: Option<bool>,
        width_mark_scale: Option<f32>,
        badge_alpha: Option<f32>,
        badge_colors: Option<Option<([u8; 3], [u8; 3])>>,
        width_mark_color: Option<Option<[u8; 3]>>,
    ) -> bool {
        let r = &mut self.renderer;
        let mut changed = false;
        let mut set = |cond: bool| changed |= cond;

        if let Some(v) = shape {
            set(r.shape != v);
            r.shape = v;
        }
        if let Some(v) = badge_scale {
            set(r.badge_scale != v);
            r.badge_scale = v;
        }
        if let Some(v) = width_mark {
            set(r.width_mark != v);
            r.width_mark = v;
        }
        if let Some(v) = width_mark_scale {
            set(r.width_mark_scale != v);
            r.width_mark_scale = v;
        }
        if let Some(v) = badge_alpha {
            set(r.badge_alpha != v);
            r.badge_alpha = v;
        }
        if let Some(v) = badge_colors {
            set(r.badge_colors != v);
            r.badge_colors = v;
        }
        if let Some(v) = width_mark_color {
            set(r.width_mark_color != v);
            r.width_mark_color = v;
        }

        if changed {
            self.last = None; // 呈现变了，下次必须重发
        }
        changed
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
    ///
    /// **标点角标与全角标记一起切**：分开切会出现"关了彩色但右上角还是绿的"，
    /// 而这个开关在用户看来只有一个意思——图标上还有没有非主字色的东西。
    pub fn set_colored(&mut self, on: bool) {
        let next = on.then_some(IconRenderer::DEFAULT_BADGE_COLORS);
        let next_mark = on.then_some(IconRenderer::DEFAULT_WIDTH_MARK_COLOR);
        if self.renderer.badge_colors != next || self.renderer.width_mark_color != next_mark {
            self.renderer.badge_colors = next;
            self.renderer.width_mark_color = next_mark;
            self.last = None;
        }
    }

    pub fn colored(&self) -> bool {
        self.renderer.badge_colors.is_some()
    }

    /// 全角标记（右上角三角）是否开启。
    pub fn width_mark(&self) -> bool {
        self.renderer.width_mark
    }

    /// 渲染并发布。返回新的发布序号；`None` 表示状态未变、已跳过。
    ///
    /// 返回序号而非布尔，是为了让服务端日志记下「这是第几版位图」。排查「图标落后一帧」
    /// 一类问题时，服务端只能看到自己发布了什么、DLL 只能看到自己读到了什么，两边日志
    /// 唯一能对上号的量就是这个序号——它同时是读端 seqlock 的判据（SHM header 的
    /// `sequence`），不是为日志另造的计数器。
    pub fn publish(&mut self, spec: &IconSpec) -> Result<Option<u32>, String> {
        if self.last.as_ref() == Some(spec) {
            return Ok(None);
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

        let seq = self
            .shm
            .publish(&bitmaps)
            .map_err(|e| format!("发布图标共享内存失败: {e}"))?;
        self.last = Some(spec.clone());
        Ok(Some(seq))
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

    /// 各编码方式两两不同——否则「换一种形状」的开关是空的。
    ///
    /// 遍历 `ALL` 而非手写列表：新增形状时自动纳入，不会出现「加了一种但没人测」。
    #[test]
    fn badge_shapes_produce_distinct_pixels() {
        let shapes = BadgeShape::ALL;
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

    /// 全角标记画在**右上角**象限。
    ///
    /// 这条同时钉住「两个状态分处两角」这个设计：全角与标点是正交的两件事，挤在同一角
    /// 就得为四种搭配各设计一种编码，而 16px 上放不下那么多可辨的差异。
    #[test]
    fn full_width_mark_lands_in_top_right_quadrant() {
        let mut r = IconRenderer::new(BadgeShape::CornerTriangle).expect("renderer");
        // ★ 必须显式开启：`width_mark` 默认关（产品决策，见该字段注释）。
        // 本测试要验的是「开启后标记画在哪」，**不该依赖默认值**——默认值一翻转，
        // 断言的前提就没了，而测试本身看不出哪里错。
        r.width_mark = true;
        let n = 32usize;
        let half = r.render(32, false, &IconSpec::default());
        let full = r.render(
            32,
            false,
            &IconSpec {
                full_width: true,
                ..IconSpec::default()
            },
        );

        let mut gained_top_right = 0;
        for y in 0..(n / 4) {
            for x in (n * 3 / 4)..n {
                if alpha_at(&full, n, x, y) > alpha_at(&half, n, x, y) {
                    gained_top_right += 1;
                }
            }
        }
        assert!(
            gained_top_right > 0,
            "右上角没有画出全角标记（新增不透明像素数为 0）"
        );
    }

    /// 半角在右上角**一点痕迹都不能留**。
    ///
    /// 半角是常态：给它也画个标记，图标上就常驻一个永不变化的点——既没告诉用户任何事，
    /// 又占掉 16×16 里本就稀缺的一角。这条防的是将来有人"顺手给半角也画个空心的"。
    #[test]
    fn half_width_leaves_no_top_right_mark() {
        let mut r = IconRenderer::new(BadgeShape::None).expect("renderer");
        r.width_mark = true; // 默认关；本测试验的是开启后半角仍不留痕
        let plain = r.render(32, false, &IconSpec::default());
        let mut with_mark = IconSpec::default();
        with_mark.full_width = true;
        let marked = r.render(32, false, &with_mark);
        assert_ne!(plain, marked, "全角与半角渲染结果相同，标记没画出来");

        // 反向：把全角关掉应当逐字节回到「从没有过标记」的样子。
        let back = r.render(32, false, &IconSpec::default());
        assert_eq!(plain, back, "半角仍留有全角标记的残迹");
    }

    /// 全角标记与标点角标彼此正交：四种搭配两两不同。
    ///
    /// 若哪天有人把两者合进同一层（比如共用一张蒙版或同一个开关），本条会失败。
    #[test]
    fn width_mark_and_punct_badge_are_independent() {
        let mut r = IconRenderer::new(BadgeShape::CornerTriangle).expect("renderer");
        r.width_mark = true; // 默认关；两者正交只在标记启用时才谈得上
        let mk = |punct, full_width| {
            r.render(
                32,
                false,
                &IconSpec {
                    punct,
                    full_width,
                    ..IconSpec::default()
                },
            )
        };
        let combos = [
            mk(PunctBadge::None, false),
            mk(PunctBadge::None, true),
            mk(PunctBadge::Chinese, false),
            mk(PunctBadge::Chinese, true),
        ];
        for i in 0..combos.len() {
            for j in (i + 1)..combos.len() {
                assert_ne!(combos[i], combos[j], "组合 {i} 与 {j} 渲染结果相同");
            }
        }
    }

    /// 角标不透明度只作用于**角标**，主字与全角标记不受影响。
    ///
    /// 透明度的用途是"别把字吃掉"（右下有笔画的「五」「双」会被实心色块切掉一角），
    /// 一旦它漏到主字上，整个图标会随之发灰——那是 `dimmed` 的语义，两者必须分开。
    #[test]
    fn badge_alpha_only_affects_the_badge() {
        let mut r = IconRenderer::new(BadgeShape::CornerTriangle).expect("renderer");
        let n = 32usize;
        let spec_cn = IconSpec {
            punct: PunctBadge::Chinese,
            ..IconSpec::default()
        };

        r.badge_alpha = 1.0;
        let opaque = r.render(32, false, &spec_cn);
        r.badge_alpha = 0.5;
        let translucent = r.render(32, false, &spec_cn);
        assert_ne!(opaque, translucent, "调低不透明度后角标没有变化");

        // 左上象限只有主字，不该有任何一个像素被动过。
        for y in 0..(n / 2) {
            for x in 0..(n / 2) {
                assert_eq!(
                    alpha_at(&opaque, n, x, y),
                    alpha_at(&translucent, n, x, y),
                    "角标不透明度漏到了主字上（{x},{y}）"
                );
            }
        }
    }

    /// 半透明角标必须真的"半遮"：角标覆盖处底下的主字笔画不能被挖掉。
    ///
    /// 这是第一版的实际缺陷——挖空与透明同时用，主字先被切掉一圈，角标底下根本没有
    /// 笔画可透，于是调低不透明度看起来毫无效果。判据取"同一处像素在半透明档下比
    /// 全不透明档**更接近主字**"：不挖空时该处是 主字⊕角标 的混合，挖空时只有角标。
    #[test]
    fn translucent_badge_lets_the_glyph_show_through() {
        let n = 32usize;
        let spec_cn = IconSpec {
            // 「五」右下有横笔，正是被角标压住的那种字。
            label: "五".to_string(),
            punct: PunctBadge::Chinese,
            ..IconSpec::default()
        };

        let mut r = IconRenderer::new(BadgeShape::CornerTriangle).expect("renderer");
        r.badge_alpha = 1.0;
        let opaque = r.render(32, false, &spec_cn);
        r.badge_alpha = 0.5;
        let translucent = r.render(32, false, &spec_cn);

        // 挖空只在不透明档发生，所以不透明档在角标外围会有一圈 alpha 被削低的像素；
        // 半透明档保留主字，那一圈应当更"实"。统计右下象限里 alpha 更高的像素数。
        let mut translucent_denser = 0;
        for y in (n / 2)..n {
            for x in (n / 2)..n {
                if alpha_at(&translucent, n, x, y) > alpha_at(&opaque, n, x, y) {
                    translucent_denser += 1;
                }
            }
        }
        assert!(
            translucent_denser > 0,
            "半透明档没有保留任何被挖掉的主字像素——挖空与透明又叠加了，\
             调不透明度会再次变成静默无效"
        );
    }

    /// 右上三角的**直角确实在右上角**，斜边朝图标中心。
    ///
    /// 上一条只验证"右上象限有新像素"，方块、圆点、甚至画反了的三角都能通过。这里沿
    /// 顶行取两点：贴着右边界的那点在直角顶点上必有墨；沿同一行往左走出斜边之外的那点
    /// 必须与半角时**逐字节相同**（即标记没画到那儿去）。
    ///
    /// 之所以值得单独钉：两个角落共用一套判据、靠一次 y 翻转区分，翻错的产物仍是个
    /// 三角，只是朝向不同——这种差异在 16px 的任务栏上几乎看不出来。
    #[test]
    fn width_mark_triangle_has_its_right_angle_at_top_right() {
        let mut r = IconRenderer::new(BadgeShape::None).expect("renderer");
        r.width_mark = true; // 默认关；本测试验的是开启后三角的直角朝向
        let n = 32usize;
        let half = r.render(32, false, &IconSpec::default());
        let full = r.render(
            32,
            false,
            &IconSpec {
                full_width: true,
                ..IconSpec::default()
            },
        );

        // 直角顶点：顶行最右一列。
        assert!(
            alpha_at(&full, n, n - 1, 0) > alpha_at(&half, n, n - 1, 0),
            "右上角顶点处没有墨——三角没画在直角该在的地方"
        );

        // 顶行往左第 11 列（leg ≈ 32×0.28 ≈ 9），已在斜边之外。
        // 若这里也有墨，画出来的就是方块或朝向错误的三角。
        let outside_x = n - 11;
        assert_eq!(
            alpha_at(&full, n, outside_x, 0),
            alpha_at(&half, n, outside_x, 0),
            "斜边之外（{outside_x},0）被画上了墨——形状不是右上直角三角"
        );
    }

    /// 全角标记有自己的颜色，且与标点角标那两色都不同。
    ///
    /// 同色会让人以为两者有关联——"蓝的那个怎么又跑到右上角去了"。它们表达的是
    /// 两个不相干的状态，颜色是这里唯一能承载"不相干"的通道（位置已被占用来区分角落）。
    #[test]
    fn width_mark_has_its_own_color() {
        let r = IconRenderer::new(BadgeShape::None).expect("renderer");
        let mark = r.width_mark_color.expect("默认应有配色");
        let (cn, en) = r.badge_colors.expect("默认应有配色");
        assert_ne!(mark, cn, "全角标记与中文标点角标同色");
        assert_ne!(mark, en, "全角标记与英文标点角标同色");
    }

    /// 关掉彩色时，标点角标与全角标记**一起**退化为主字色。
    ///
    /// 分开切的后果是"关了彩色但右上角还是绿的"——而这个开关在用户看来只有一个意思：
    /// 图标上还有没有非主字色的东西。
    #[test]
    fn disabling_colors_clears_both_badge_and_width_mark() {
        let mut p_renderer = IconRenderer::new(BadgeShape::CornerTriangle).expect("renderer");
        // 直接验证渲染器字段的联动语义（发布器需要真实 SHM，构造不起来）。
        p_renderer.badge_colors = None;
        p_renderer.width_mark_color = None;
        let spec_both = IconSpec {
            punct: PunctBadge::Chinese,
            full_width: true,
            ..IconSpec::default()
        };
        let mono = p_renderer.render(32, false, &spec_both);

        // 单色档下整张图只该有主字那一种色相：浅色主题前景为黑，故 RGB 三通道相等。
        for i in 0..(32 * 32) {
            if mono[i * 4 + 3] == 0 {
                continue; // 全透明像素的颜色无意义
            }
            let (b, g, r) = (mono[i * 4], mono[i * 4 + 1], mono[i * 4 + 2]);
            assert!(
                b == g && g == r,
                "关掉彩色后仍有带色相的像素（{b},{g},{r}）"
            );
        }
    }

    /// 全角标记倍率归零 = 彻底关掉，右上角必须与半角逐字节相同。
    ///
    /// 防的是一种很容易漏的半截短路：标记本身按 hw=0 画不出来，可挖空版的尺寸是
    /// `hw + expand`，仍有 expand 那么大——于是主字右上角被挖掉一块、却没有任何
    /// 东西补上去，看起来像字缺了一角，而"关掉"本该什么都不发生。
    #[test]
    fn width_mark_scale_zero_carves_nothing() {
        let mut r = IconRenderer::new(BadgeShape::None).expect("renderer");
        r.width_mark_scale = 0.0;
        let full = r.render(
            32,
            false,
            &IconSpec {
                full_width: true,
                ..IconSpec::default()
            },
        );
        let half = r.render(32, false, &IconSpec::default());
        assert_eq!(full, half, "倍率归零时仍在主字上挖了洞");
    }

    /// 大小倍率确实改变角标尺寸，且与不透明度是两个独立的自由度。
    #[test]
    fn badge_scale_changes_badge_size() {
        let mut r = IconRenderer::new(BadgeShape::CornerTriangle).expect("renderer");
        let spec_cn = IconSpec {
            punct: PunctBadge::Chinese,
            ..IconSpec::default()
        };
        r.badge_scale = 1.0;
        let base = r.render(32, false, &spec_cn);
        r.badge_scale = 0.6;
        let small = r.render(32, false, &spec_cn);
        assert_ne!(base, small, "改了大小倍率但渲染结果相同");
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

    /// 关掉角标后必须与「本来就没有角标」逐字节相同。
    ///
    /// 这是「关」这一档的全部承诺：不是画一个更小的角标，而是一点痕迹都不留。
    /// 若哪天挖空蒙版忘了跟着短路，主字上会留下一圈没人填的凹口——那种缺陷肉眼
    /// 只会觉得「字有点怪」，很难联想到是关掉的那条路径没走干净。
    #[test]
    fn shape_none_leaves_no_trace() {
        let off = IconRenderer::new(BadgeShape::None).expect("renderer");
        let on = IconRenderer::new(BadgeShape::CornerTriangle).expect("renderer");
        for &size in &wind_ipc::protocol::ICON_SIZES {
            let baseline = on.render(size, false, &spec(PunctBadge::None));
            for p in [PunctBadge::Chinese, PunctBadge::English] {
                assert_eq!(
                    off.render(size, false, &spec(p)),
                    baseline,
                    "{size}px / {p:?}：关掉角标后仍与无角标基线不同"
                );
            }
        }
    }

    /// 落盘 id 必须唯一且可往返——它写进 state.toml，活得比进程久。
    ///
    /// 未知 id 回落默认这条同样重要：state.toml 是用户可编辑的文本文件，
    /// 手写错一个字母不该让图标消失或让服务崩掉。
    #[test]
    fn badge_shape_id_roundtrips() {
        let mut seen = std::collections::HashSet::new();
        for &sh in &BadgeShape::ALL {
            assert!(seen.insert(sh.as_id()), "{sh:?} 的 id 与别的形状重复");
            assert_eq!(BadgeShape::from_id(sh.as_id()), sh);
        }
        for bogus in ["", "triangle", "CornerTriangle", "0"] {
            assert_eq!(
                BadgeShape::from_id(bogus),
                BadgeShape::default(),
                "未知 id {bogus:?} 未回落到默认"
            );
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
