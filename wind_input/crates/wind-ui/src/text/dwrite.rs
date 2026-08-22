//! 文本渲染后端（DirectWrite 实现）
//!
//! 与 Go 版本 `wind_input/internal/ui/dwrite_text.go` 对齐。
//!
//! 管线：IDWriteFactory → IDWriteTextFormat/IDWriteTextLayout（测量）
//!      → IDWriteGdiInterop::CreateBitmapRenderTarget（内存 DC 上的 32bpp 顶端向下 DIB）
//!      → 自定义 IDWriteTextRenderer 回调里调 IDWriteBitmapRenderTarget::DrawGlyphRun
//!      → 预乘 alpha 选择性回写到调用方 BGRA 缓冲区。
//!
//! 透明度正确性（修复 GDI 旧实现"黑字被当背景吞掉、抗锯齿丢失"）：
//! 先把目标缓冲区按"不透明"复制进 DIB（GDI 对不透明背景做抗锯齿混合），渲染后
//! 逐像素对比——RGB 未变 = 背景，保留原 alpha；RGB 变了 = 文字像素，按窗口原
//! alpha 预乘（R' = R×A/255），使其成为合法预乘像素，与背景共享同一透明度。

/// 文本度量信息
#[derive(Debug, Clone)]
pub struct TextMetrics {
    pub width: f32,
    pub height: f32,
}

/// 一次文字排版所需的全部字体属性。
///
/// ## 为什么收成结构体
///
/// 这些属性要穿过三个后端（DirectWrite / CoreText / mock）和全部测量与绘制调用点。
/// 散开成位置参数时，每加一项都要改所有签名——重构前的 `draw_text_styled` 已是 11 个参数，
/// 再加行高、斜体就到 13 个，而参数越多，传错顺序时编译器越抓不到（`size`/`weight`
/// 都是数值，换个位置照样编译）。
///
/// 隔壁 wind-ui-rust 走过这条路并留下了教训：字重就是因为"每加一项都要改所有签名"
/// 而**没有进签名**，改走线程局部注入——于是字重成了隐式全局状态，某条路径忘了复位，
/// 后续无关文字就跟着变粗，且只在特定绘制顺序下显形。收成结构体后新增属性只是加一个
/// 字段，签名不动、调用点不动。
///
/// 本仓暂不设 `line_height`：View 引擎还没有行高概念，高度直接取自后端度量。
/// 加字段前先让它在渲染路径里真正生效，否则就是「声明未实现」。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextStyle<'a> {
    /// 字族名。`None` = 用渲染器的全局字体族。
    pub family: Option<&'a str>,
    /// 字号（设备像素，调用方已按 DPI 缩放）。
    pub size: f32,
    /// 字重（400=常规、700=粗）。`0` = 继承渲染器默认，沿用既有约定。
    pub weight: i32,
}

impl<'a> TextStyle<'a> {
    /// 只指定字号，字重与字体族取默认。
    pub fn new(size: f32) -> Self {
        Self {
            family: None,
            size,
            weight: 0,
        }
    }

    /// 换字重（`0` = 继承默认）。
    pub fn with_weight(self, weight: i32) -> Self {
        Self { weight, ..self }
    }

    /// 换字体族（`None`/空串 = 用全局字体族）。
    pub fn with_family(self, family: Option<&'a str>) -> Self {
        Self {
            family: family.filter(|s| !s.trim().is_empty()),
            ..self
        }
    }
}

/// 测量缓存容量上限；超过即整体清空。
///
/// 不做 LRU：候选窗每帧的文本集合高度重复（同一批候选、序号、注释反复测量），
/// 命中率本就极高，淘汰策略的簿记开销换不回收益。整体清空的最坏情况是一帧全 miss，
/// 等价于没有缓存时的行为。
#[cfg_attr(not(windows), allow(dead_code))]
const MEASURE_CACHE_CAP: usize = 4096;

/// 测量缓存键：`(文本, 字号, 字重, 字体族)` 的 64 位哈希。
///
/// 存哈希而非完整键，是为了免掉每次查询都克隆 `String`——测量在热路径上，一帧数十次。
/// 64 位下 4096 条目的碰撞概率约 4.5e-13，可忽略；真碰撞的后果是某段文本用了另一段的
/// 宽度（布局错位），故键必须**覆盖所有影响测量的输入**，漏一项就是系统性错位而非偶发。
///
/// 字号用 `to_bits()` 而非 `as u32`：字号是 DPI 缩放后的浮点（如 14.4/16.8），
/// 取整会让相邻字号撞进同一个键。
///
/// ⚠️ 给 [`TextStyle`] 加字段时**必须同步加进这里**——漏一项就是某段文本静默套用另一段
/// 的宽度。这正是它按整个 `TextStyle` 取参、而非重新罗列各项的原因：字段列表只有一处。
#[cfg_attr(not(windows), allow(dead_code))]
fn measure_key(text: &str, ts: &TextStyle) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    ts.size.to_bits().hash(&mut h);
    ts.weight.hash(&mut h);
    ts.family.hash(&mut h);
    h.finish()
}

/// 扫描 UTF-16 序列，返回私用区（PUA）字符的连续段 `[(起始下标, 码元长度)]`，
/// 下标/长度均以 **UTF-16 码元** 计，可直接用作 `DWRITE_TEXT_RANGE`。
///
/// 三段私用区缺一不可——不同拆字库用的区不同：内置 wubi86 字根在 BMP 私用区
/// （U+E0E1 等），而 986 等第三方码表的字根在补充私用区 A（U+F00FD 等）。
/// 早期只判 BMP 一段，导致后者从不切字体、渲染成方框。
///
/// - BMP 私用区 `U+E000..=U+F8FF`：单码元，`u16` 值即码位。
/// - 补充私用区 A/B `U+F0000..=U+10FFFD`：UTF-16 下是代理对。高位代理恰好占满
///   `0xDB80..=0xDBFF`（`0xDB80..=0xDBBF` → 第 15 平面，`0xDBC0..=0xDBFF` → 第 16
///   平面），不多不少，故判「高位代理落在该段 + 后随合法低位代理」即可，无需还原码位。
///
/// 相邻的 BMP 与补充私用区字符合并进同一段——它们目标字体族相同，合并只减少
/// `SetFontFamilyName` 调用次数，不改变渲染结果。
#[cfg_attr(not(windows), allow(dead_code))]
fn pua_runs(wide: &[u16]) -> Vec<(usize, usize)> {
    /// 单码元即为私用区码位（BMP PUA）。
    fn is_bmp_pua(u: u16) -> bool {
        (0xE000..=0xF8FF).contains(&u)
    }
    /// 补充私用区 A/B 的高位代理段。
    fn is_spua_lead(u: u16) -> bool {
        (0xDB80..=0xDBFF).contains(&u)
    }
    /// 任意低位代理（配对合法性；具体码位无需还原）。
    fn is_trail(u: u16) -> bool {
        (0xDC00..=0xDFFF).contains(&u)
    }

    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    let mut i = 0usize;
    while i < wide.len() {
        // 命中长度：1=BMP 私用区，2=补充私用区代理对，0=非私用区。
        let step = if is_bmp_pua(wide[i]) {
            1
        } else if is_spua_lead(wide[i]) && wide.get(i + 1).is_some_and(|&t| is_trail(t)) {
            2
        } else {
            0
        };
        if step == 0 {
            if let Some(s) = start.take() {
                runs.push((s, i - s));
            }
            i += 1;
        } else {
            start.get_or_insert(i);
            i += step;
        }
    }
    if let Some(s) = start {
        runs.push((s, wide.len() - s));
    }
    runs
}

#[cfg(windows)]
pub use imp::TextRenderer;

/// Windows 实现（DirectWrite）。非 Windows 平台见文件末尾的 mock。
#[cfg(windows)]
mod imp {
    use super::{MEASURE_CACHE_CAP, TextMetrics, TextStyle, measure_key};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::ffi::c_void;

    use windows::Win32::Foundation::{BOOL, COLORREF, DWRITE_E_NOCOLOR, FALSE};
    use windows::Win32::Graphics::DirectWrite::*;
    use windows::Win32::Graphics::Gdi::{DIBSECTION, GetCurrentObject, GetObjectW, OBJ_BITMAP};
    use windows::core::{Interface, PCWSTR, implement};

    /// 五笔字根字体的 DirectWrite 家族名（HeiTiZiGen.ttf 的 name 表家族名）。
    const CHAIZI_FAMILY: &str = "黑体字根";

    /// 拆字字根字体（自定义字体集 + 家族名），用于 PUA 字根字符的级联回退渲染。
    struct ChaiziFont {
        collection: IDWriteFontCollection1,
        family: Vec<u16>,
    }

    /// 渲染表面：尺寸绑定的位图渲染目标 + 其专属字形渲染器回调对象。
    struct Surface {
        target: IDWriteBitmapRenderTarget,
        renderer: IDWriteTextRenderer,
        width: u32,
        height: u32,
    }

    /// 文本渲染器
    pub struct TextRenderer {
        /// 字体族（宽字符，含结尾 0）
        family: Vec<u16>,
        /// 语言区域（宽字符，含结尾 0）
        locale: Vec<u16>,
        /// 基准字号（family 固定）；可按调用传不同字号（序号/注释相对偏移）。
        font_size: f32,
        factory: IDWriteFactory,
        /// 彩色字形拆层接口（Win8.1+）；取不到则退化为单色渲染。
        factory2: Option<IDWriteFactory2>,
        gdi_interop: IDWriteGdiInterop,
        params: IDWriteRenderingParams,
        /// 文本格式缓存：按字号（取整 px）keyed，避免每帧重建 COM 对象。
        formats: RefCell<HashMap<u32, IDWriteTextFormat>>,
        /// 文本测量缓存（键见 `measure_key`）：避免每帧重建 `IDWriteTextLayout`。
        ///
        /// 盒模型对同一段文本会测两到三次（measure 阶段一次、paint 阶段为算对齐再一次、
        /// 有 caret 时再测前半段），上翻布局生效时整棵树还会重建重测。没有这层缓存时，
        /// 每一次都是一个新的 COM 对象 + 一次完整排版。
        measure_cache: RefCell<HashMap<u64, TextMetrics>>,
        /// 当前位图渲染表面（按需重建）
        surface: RefCell<Option<Surface>>,
        /// 拆字字根字体（可选）：设置后对 PUA 码位字符级联回退到该字体渲染。
        chaizi: Option<ChaiziFont>,
    }

    impl TextRenderer {
        /// 创建文本渲染器
        pub fn new(font_family: &str, font_size: f32) -> Result<Self, String> {
            unsafe {
                let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)
                    .map_err(|e| format!("DWriteCreateFactory: {e}"))?;
                let gdi_interop = factory
                    .GetGdiInterop()
                    .map_err(|e| format!("GetGdiInterop: {e}"))?;
                // 默认渲染参数（系统 ClearType 设置）
                let params = factory
                    .CreateRenderingParams()
                    .map_err(|e| format!("CreateRenderingParams: {e}"))?;
                // IDWriteFactory2（Win8.1+）提供彩色字形拆层；取不到则退化为单色。
                let factory2: Option<IDWriteFactory2> = factory.cast().ok();

                let family: Vec<u16> = font_family
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                let locale: Vec<u16> = "zh-cn".encode_utf16().chain(std::iter::once(0)).collect();

                Ok(Self {
                    family,
                    locale,
                    font_size,
                    factory,
                    factory2,
                    gdi_interop,
                    params,
                    formats: RefCell::new(HashMap::new()),
                    measure_cache: RefCell::new(HashMap::new()),
                    surface: RefCell::new(None),
                    chaizi: None,
                })
            }
        }

        /// 加载拆字字根字体（TTF）建自定义字体集，后续渲染中 PUA 码位字符回退到它。
        /// `family` 为方案配置的 DWrite 家族名（空则回退默认 `CHAIZI_FAMILY`）。
        /// 失败返回 Err（不影响普通文本渲染）。
        pub fn set_chaizi_font(&mut self, path: &str, family: &str) -> Result<(), String> {
            unsafe {
                let f3: IDWriteFactory3 = self
                    .factory
                    .cast()
                    .map_err(|e| format!("cast IDWriteFactory3: {e}"))?;
                let path_w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
                let file = f3
                    .CreateFontFileReference(PCWSTR(path_w.as_ptr()), None)
                    .map_err(|e| format!("CreateFontFileReference: {e}"))?;
                let builder: IDWriteFontSetBuilder1 = f3
                    .CreateFontSetBuilder()
                    .map_err(|e| format!("CreateFontSetBuilder: {e}"))?
                    .cast()
                    .map_err(|e| format!("cast IDWriteFontSetBuilder1: {e}"))?;
                builder
                    .AddFontFile(&file)
                    .map_err(|e| format!("AddFontFile: {e}"))?;
                let set = builder
                    .CreateFontSet()
                    .map_err(|e| format!("CreateFontSet: {e}"))?;
                let collection = f3
                    .CreateFontCollectionFromFontSet(&set)
                    .map_err(|e| format!("CreateFontCollectionFromFontSet: {e}"))?;
                let family_name = if family.is_empty() {
                    CHAIZI_FAMILY
                } else {
                    family
                };
                let family: Vec<u16> = family_name
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                self.chaizi = Some(ChaiziFont { collection, family });
                // 字根字体改变了 PUA 字符的字形来源 → 其测量宽度随之改变。不清缓存的话，
                // 切换拆字方案后字根仍按旧字体的宽度布局（表现为字根格错位/重叠）。
                self.measure_cache.borrow_mut().clear();
                Ok(())
            }
        }

        /// 基准字号（View 叶子未显式指定字号时回退）。
        pub fn base_size(&self) -> f32 {
            self.font_size
        }

        /// 仅测试可见：当前测量缓存条目数。
        #[cfg(test)]
        pub fn measure_cache_len(&self) -> usize {
            self.measure_cache.borrow().len()
        }

        /// 更新基准字号（DPI 动态变化时调用）。格式按 px 缓存，无需重建 COM 对象，
        /// 仅改变未显式指定字号的叶子的回退字号。
        pub fn set_base_size(&mut self, size: f32) {
            self.font_size = size;
        }

        /// 切换字体族（ui.font.family 变更时调用）。清空按字号缓存的 TextFormat，使新字体生效。
        ///
        /// 测量缓存同样要清：其键里的字体族为 `None` 时表示"用全局 family"，全局一换，
        /// 这些条目记录的就是旧字体的宽度。
        pub fn set_font_family(&mut self, font_family: &str) {
            self.family = font_family
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            self.formats.borrow_mut().clear();
            self.measure_cache.borrow_mut().clear();
        }

        /// 取得（或创建）给定字号的文本格式（按取整 px 缓存）。
        fn ensure_format(&self, size: f32) -> Result<IDWriteTextFormat, String> {
            let key = size.max(1.0).round() as u32;
            if let Some(f) = self.formats.borrow().get(&key) {
                return Ok(f.clone());
            }
            unsafe {
                let fmt = self
                    .factory
                    .CreateTextFormat(
                        PCWSTR(self.family.as_ptr()),
                        None,
                        DWRITE_FONT_WEIGHT_NORMAL,
                        DWRITE_FONT_STYLE_NORMAL,
                        DWRITE_FONT_STRETCH_NORMAL,
                        key as f32,
                        PCWSTR(self.locale.as_ptr()),
                    )
                    .map_err(|e| format!("CreateTextFormat: {e}"))?;
                self.formats.borrow_mut().insert(key, fmt.clone());
                Ok(fmt)
            }
        }

        /// 为给定文本/字号创建布局对象。
        /// weight>0 且 ≠400 时覆盖字重；family 非空时覆盖字体族（皆作用于全文）。
        fn create_layout(
            &self,
            text: &str,
            size: f32,
            weight: i32,
            family: Option<&str>,
            max_w: f32,
            max_h: f32,
        ) -> Result<IDWriteTextLayout, String> {
            let fmt = self.ensure_format(size)?;
            let wide: Vec<u16> = text.encode_utf16().collect();
            unsafe {
                let layout = self
                    .factory
                    .CreateTextLayout(&wide, &fmt, max_w.max(1.0), max_h.max(1.0))
                    .map_err(|e| format!("CreateTextLayout: {e}"))?;
                // 关闭自动换行：本 View 引擎是单行盒模型，容不下 DirectWrite 自作主张的折行。
                //
                // 测量与绘制传的 max_w 本就不同——测量传 f32::MAX（不换行），绘制传缓冲宽度。
                // 于是文本一旦宽过缓冲，布局按单行高度排、绘制却折成多行，多出来的行直接画到
                // 节点框外，盖住相邻候选。竖排的 behavior.vertical_max_width（出厂默认 0=不限，
                // 用户/主题可显式配正值）或渲染层恒生效的屏幕安全钳制都会把窗口宽度钳掉，
                // 触发这条路径。
                //
                // NO_WRAP 只关**自动**换行，`\n` 硬换行照旧生效（实测：含 \n 的文本仍返回
                // 2 倍行高）——candidate_window 依赖后者做多行候选，不能一起关掉。
                let _ = layout.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
                // 节点级字重/字体族覆盖（作用于全文；下方 chaizi PUA 段会再覆盖字体族）。
                let full = DWRITE_TEXT_RANGE {
                    startPosition: 0,
                    length: wide.len() as u32,
                };
                if weight > 0 && weight != 400 {
                    let _ = layout.SetFontWeight(DWRITE_FONT_WEIGHT(weight), full);
                }
                if let Some(fam) = family.filter(|s| !s.trim().is_empty()) {
                    let famw: Vec<u16> = fam.encode_utf16().chain(std::iter::once(0)).collect();
                    let _ = layout.SetFontFamilyName(PCWSTR(famw.as_ptr()), full);
                }
                // 拆字字根：把私用区（BMP PUA + 补充私用区 A/B）的连续段切到字根字体集，
                // 级联回退渲染字根字符。段划分见 `super::pua_runs`——测量与绘制共用本函数，
                // 故字根段的字体在两条路径上必然一致（否则宽度按主字体缺字宽算，布局出错）。
                if let Some(cf) = &self.chaizi {
                    for (start, len) in super::pua_runs(&wide) {
                        let range = DWRITE_TEXT_RANGE {
                            startPosition: start as u32,
                            length: len as u32,
                        };
                        let _ = layout.SetFontCollection(&cf.collection, range);
                        let _ = layout.SetFontFamilyName(PCWSTR(cf.family.as_ptr()), range);
                    }
                }
                Ok(layout)
            }
        }

        /// 测量文本尺寸（用基准字号）。
        pub fn measure_text(&self, text: &str) -> TextMetrics {
            self.measure_text_sized(text, self.font_size)
        }

        /// 测量文本尺寸（指定字号，其余取默认；宽含尾随空白，高为行高）。
        pub fn measure_text_sized(&self, text: &str, size: f32) -> TextMetrics {
            self.measure(text, &TextStyle::new(size))
        }

        /// 测量文本尺寸。结果按 `measure_key` 缓存。
        pub fn measure(&self, text: &str, ts: &TextStyle) -> TextMetrics {
            if text.is_empty() {
                return TextMetrics {
                    width: 0.0,
                    height: ts.size * 1.2,
                };
            }
            let key = measure_key(text, ts);
            if let Some(m) = self.measure_cache.borrow().get(&key) {
                return m.clone();
            }
            // 排版失败走等宽近似回退，且**不入缓存**：失败多是暂时性的（资源紧张、
            // 字体集正在切换），一旦把回退值固化，这段文本就会一直按错误宽度布局
            // 直到下次整体清空——而清空只在换字体/换字根时发生，可能永远等不到。
            let Some(m) = self.measure_layout(text, ts) else {
                return TextMetrics {
                    width: text.chars().count() as f32 * ts.size * 0.6,
                    height: ts.size * 1.2,
                };
            };
            let mut c = self.measure_cache.borrow_mut();
            if c.len() >= MEASURE_CACHE_CAP {
                c.clear();
            }
            c.insert(key, m.clone());
            m
        }

        /// 走一次 DirectWrite 排版取度量。任一 COM 环节失败返回 `None`
        /// （由 [`TextRenderer::measure`] 决定回退值，并跳过缓存）。
        fn measure_layout(&self, text: &str, ts: &TextStyle) -> Option<TextMetrics> {
            let layout = self
                .create_layout(
                    text,
                    ts.size,
                    ts.weight,
                    ts.family,
                    f32::MAX / 2.0,
                    f32::MAX / 2.0,
                )
                .ok()?;
            unsafe {
                let mut m = DWRITE_TEXT_METRICS::default();
                layout.GetMetrics(&mut m).ok()?;
                let height = if m.height > 0.0 {
                    m.height
                } else {
                    ts.size * 1.2
                };
                Some(TextMetrics {
                    width: m.widthIncludingTrailingWhitespace,
                    height,
                })
            }
        }

        /// 确保位图渲染表面至少为给定尺寸（只增长不重建：翻页时窗口宽度抖动，
        /// 复用最大表面可避免每帧重建 COM 渲染目标）。DIB 实际可比窗口大，
        /// draw_text 用窗口尺寸裁剪、用 DIBSECTION 的真实 stride 索引，故安全。
        fn ensure_surface(&self, w: u32, h: u32) -> Result<(), String> {
            let (cur_w, cur_h) = self
                .surface
                .borrow()
                .as_ref()
                .map_or((0, 0), |s| (s.width, s.height));
            if cur_w >= w && cur_h >= h {
                return Ok(());
            }
            let nw = w.max(cur_w);
            let nh = h.max(cur_h);
            unsafe {
                let target = self
                    .gdi_interop
                    .CreateBitmapRenderTarget(None, nw, nh)
                    .map_err(|e| format!("CreateBitmapRenderTarget: {e}"))?;
                target
                    .SetPixelsPerDip(1.0)
                    .map_err(|e| format!("SetPixelsPerDip: {e}"))?;
                let renderer: IDWriteTextRenderer = GlyphRenderer {
                    target: target.clone(),
                    params: self.params.clone(),
                    factory2: self.factory2.clone(),
                }
                .into();
                *self.surface.borrow_mut() = Some(Surface {
                    target,
                    renderer,
                    width: nw,
                    height: nh,
                });
            }
            Ok(())
        }

        /// 渲染文本到 BGRA 缓冲区（用基准字号）。
        ///
        /// - `buf`: 目标 BGRA 缓冲区（已含背景，预乘 alpha）
        /// - `buf_width`/`buf_height`: 缓冲区尺寸
        /// - `x`/`y`: 文本左上角（像素坐标）
        /// - `color`: 文本颜色 [R, G, B, A]（`A` 为文字自身不透明度，见
        ///   [`TextRenderer::draw`] 步骤 3 的二次混合）
        #[allow(clippy::too_many_arguments)]
        pub fn draw_text(
            &self,
            buf: &mut [u8],
            buf_width: u32,
            buf_height: u32,
            x: f32,
            y: f32,
            text: &str,
            color: [u8; 4],
        ) -> Result<(), String> {
            self.draw_text_sized(
                buf,
                buf_width,
                buf_height,
                x,
                y,
                text,
                self.font_size,
                color,
            )
        }

        /// 绘制文本（指定字号，其余取默认）。
        #[allow(clippy::too_many_arguments)]
        pub fn draw_text_sized(
            &self,
            buf: &mut [u8],
            buf_width: u32,
            buf_height: u32,
            x: f32,
            y: f32,
            text: &str,
            size: f32,
            color: [u8; 4],
        ) -> Result<(), String> {
            self.draw(
                buf,
                buf_width,
                buf_height,
                x,
                y,
                text,
                &TextStyle::new(size),
                color,
            )
        }

        /// 绘制文本。
        #[allow(clippy::too_many_arguments)]
        pub fn draw(
            &self,
            buf: &mut [u8],
            buf_width: u32,
            buf_height: u32,
            x: f32,
            y: f32,
            text: &str,
            ts: &TextStyle,
            color: [u8; 4],
        ) -> Result<(), String> {
            if text.is_empty() || buf_width == 0 || buf_height == 0 {
                return Ok(());
            }
            let w = buf_width as usize;
            let h = buf_height as usize;
            if buf.len() < w * h * 4 {
                return Err("buffer too small".into());
            }

            self.ensure_surface(buf_width, buf_height)?;
            let surface = self.surface.borrow();
            let surface = surface.as_ref().ok_or("no surface")?;

            unsafe {
                // 取内存 DC 中 DIB 的像素指针与行距。
                let memdc = surface.target.GetMemoryDC();
                let hbmp = GetCurrentObject(memdc, OBJ_BITMAP);
                let mut ds = DIBSECTION::default();
                let n = GetObjectW(
                    hbmp,
                    std::mem::size_of::<DIBSECTION>() as i32,
                    Some(&mut ds as *mut _ as *mut c_void),
                );
                if n == 0 || ds.dsBm.bmBits.is_null() {
                    return Err("GetObjectW(DIBSECTION) failed".into());
                }
                let stride = ds.dsBm.bmWidthBytes as usize; // 32bpp 顶端向下，bmBits 指向首（顶）行
                let bits = ds.dsBm.bmBits as *mut u8;
                let dib = std::slice::from_raw_parts_mut(bits, stride * h);

                // 颜色经 clientDrawingContext 透传给字形回调。
                // 入参 color 约定为 [R,G,B,A]；COLORREF = 0x00BBGGRR。
                let colorref: u32 =
                    (color[0] as u32) | ((color[1] as u32) << 8) | ((color[2] as u32) << 16);
                let layout = self.create_layout(
                    text,
                    ts.size,
                    ts.weight,
                    ts.family,
                    buf_width as f32,
                    buf_height as f32,
                )?;

                // 关键优化：用文本度量算出包围盒，后续两遍逐像素操作只在盒内进行
                // （原实现每次绘制都遍历整窗，单帧十余次 × 整窗 → paint 高达 ~100ms）。
                // ClearType/抗锯齿可能轻微外溢，留 2px 余量。
                let mut tm = DWRITE_TEXT_METRICS::default();
                let _ = layout.GetMetrics(&mut tm);
                const MARGIN: f32 = 2.0;
                let cx0 = (x + tm.left - MARGIN).floor().max(0.0) as usize;
                let cy0 = (y + tm.top - MARGIN).floor().max(0.0) as usize;
                let cx1 = (((x + tm.left + tm.widthIncludingTrailingWhitespace + MARGIN).ceil())
                    .max(0.0) as usize)
                    .min(w);
                let cy1 = (((y + tm.top + tm.height + MARGIN).ceil()).max(0.0) as usize).min(h);
                if cx0 >= cx1 || cy0 >= cy1 {
                    return Ok(());
                }

                // 1) 背景按不透明复制进 DIB（仅包围盒；盒外 DIB 残留不会被读取）。
                for row in cy0..cy1 {
                    let src = row * w * 4;
                    let dst = row * stride;
                    for col in cx0..cx1 {
                        let s = src + col * 4;
                        let d = dst + col * 4;
                        dib[d] = buf[s];
                        dib[d + 1] = buf[s + 1];
                        dib[d + 2] = buf[s + 2];
                        dib[d + 3] = 255;
                    }
                }

                // 2) 渲染文本（绝对坐标 x,y，不受 DIB 实际尺寸影响）。
                layout
                    .Draw(
                        Some(&colorref as *const u32 as *const c_void),
                        &surface.renderer,
                        x,
                        y,
                    )
                    .map_err(|e| format!("TextLayout::Draw: {e}"))?;

                // 3) 选择性预乘回写：RGB 变动的像素视为文字，按窗口原 alpha 预乘（仅包围盒）。
                //
                // 文字自身的 alpha（`color[3]`）在这一步才混进来，而非交给 DirectWrite：
                // `BitmapRenderTarget::DrawGlyphRun` 只接受不含 alpha 的 COLORREF，半透明
                // 文字色根本传不进去。DirectWrite 已把**字形覆盖率**（含抗锯齿/ClearType）
                // 算进 (nr,ng,nb)——那是"文字色完全不透明"时的合成结果；此处再按 fa 与原
                // 背景混一次，等效于把 fa 乘进有效覆盖率。
                //
                // fa=255 时 mix 退化为 n 本身，逐像素等同旧逻辑 → 不透明文字零回归。
                let fa = color[3] as u32;
                // 背景侧取 buf 的现有预乘值当直通用——与步骤 1 拷进 DIB 的口径一致，
                // 两处必须同源，否则半透明背景上的文字会与 DirectWrite 的混合基准错位。
                let mix = |n: u8, b: u8| ((n as u32 * fa + b as u32 * (255 - fa)) / 255) as u8;
                for row in cy0..cy1 {
                    let sbase = row * w * 4;
                    let dbase = row * stride;
                    for col in cx0..cx1 {
                        let s = sbase + col * 4;
                        let d = dbase + col * 4;
                        let nb = dib[d];
                        let ng = dib[d + 1];
                        let nr = dib[d + 2];
                        if nb == buf[s] && ng == buf[s + 1] && nr == buf[s + 2] {
                            continue; // 背景未变
                        }
                        let a = buf[s + 3] as u32;
                        // 先按 fa 混合（读原 buf 值），再按窗口 alpha 预乘写回——顺序承重：
                        // mix 的背景侧必须是尚未被本像素写覆盖的原值。
                        let fb = mix(nb, buf[s]);
                        let fg = mix(ng, buf[s + 1]);
                        let fr = mix(nr, buf[s + 2]);
                        buf[s] = (fb as u32 * a / 255) as u8;
                        buf[s + 1] = (fg as u32 * a / 255) as u8;
                        buf[s + 2] = (fr as u32 * a / 255) as u8;
                        // alpha 保持窗口原值
                    }
                }
            }
            Ok(())
        }
    }

    /// DWRITE_COLOR_F（0..1 各通道）→ GDI COLORREF（0x00BBGGRR）。
    /// BitmapRenderTarget.DrawGlyphRun 只接受不含 alpha 的 COLORREF；彩色 emoji 层通常 a=1.0，可接受。
    fn color_f_to_colorref(c: DWRITE_COLOR_F) -> COLORREF {
        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
        COLORREF((q(c.b) << 16) | (q(c.g) << 8) | q(c.r))
    }

    /// 自定义字形渲染器：优先把字形拆成彩色层逐层着色（emoji），否则以文字色单色绘制。
    /// 颜色不存于对象内，而是每次 Draw 经 clientDrawingContext 透传，避免可变状态。
    #[implement(IDWriteTextRenderer)]
    struct GlyphRenderer {
        target: IDWriteBitmapRenderTarget,
        params: IDWriteRenderingParams,
        /// 彩色字形拆层接口（Win8.1+）；None 时仅单色绘制。
        factory2: Option<IDWriteFactory2>,
    }

    #[allow(non_snake_case)]
    impl IDWritePixelSnapping_Impl for GlyphRenderer_Impl {
        fn IsPixelSnappingDisabled(&self, _ctx: *const c_void) -> windows::core::Result<BOOL> {
            Ok(FALSE)
        }

        fn GetCurrentTransform(
            &self,
            _ctx: *const c_void,
            transform: *mut DWRITE_MATRIX,
        ) -> windows::core::Result<()> {
            // 单位矩阵
            unsafe {
                if !transform.is_null() {
                    *transform = DWRITE_MATRIX {
                        m11: 1.0,
                        m12: 0.0,
                        m21: 0.0,
                        m22: 1.0,
                        dx: 0.0,
                        dy: 0.0,
                    };
                }
            }
            Ok(())
        }

        fn GetPixelsPerDip(&self, _ctx: *const c_void) -> windows::core::Result<f32> {
            Ok(1.0)
        }
    }

    #[allow(non_snake_case)]
    impl IDWriteTextRenderer_Impl for GlyphRenderer_Impl {
        fn DrawGlyphRun(
            &self,
            ctx: *const c_void,
            baseline_x: f32,
            baseline_y: f32,
            measuring_mode: DWRITE_MEASURING_MODE,
            glyph_run: *const DWRITE_GLYPH_RUN,
            desc: *const DWRITE_GLYPH_RUN_DESCRIPTION,
            _effect: Option<&windows::core::IUnknown>,
        ) -> windows::core::Result<()> {
            let colorref = if ctx.is_null() {
                0u32
            } else {
                unsafe { *(ctx as *const u32) }
            };

            // 优先：把字形拆成彩色层（COLR/CPAL，如 emoji）逐层着色叠加。
            // 字体无彩色数据时 TranslateColorGlyphRun 返回 DWRITE_E_NOCOLOR，落到下方单色路径。
            if let Some(f2) = &self.factory2 {
                let desc_opt = if desc.is_null() { None } else { Some(desc) };
                let enumr = unsafe {
                    f2.TranslateColorGlyphRun(
                        baseline_x,
                        baseline_y,
                        glyph_run,
                        desc_opt,
                        measuring_mode,
                        None, // 无世界变换（位图已按物理像素 1:1）
                        0,    // 默认调色板
                    )
                };
                match enumr {
                    Ok(en) => {
                        unsafe {
                            // 逐层绘制；枚举出错则中止彩色路径（已绘层保留）。
                            while let Ok(more) = en.MoveNext() {
                                if !more.as_bool() {
                                    break;
                                }
                                let Ok(run_ptr) = en.GetCurrentRun() else {
                                    break;
                                };
                                if run_ptr.is_null() {
                                    break;
                                }
                                let run = &*run_ptr;
                                // paletteIndex == 0xFFFF 为规范哨兵：该层用文字前景色。
                                let color = if run.paletteIndex == 0xFFFF {
                                    COLORREF(colorref)
                                } else {
                                    color_f_to_colorref(run.runColor)
                                };
                                let _ = self.target.DrawGlyphRun(
                                    run.baselineOriginX,
                                    run.baselineOriginY,
                                    measuring_mode,
                                    &run.glyphRun,
                                    &self.params,
                                    color,
                                    None,
                                );
                            }
                        }
                        return Ok(());
                    }
                    Err(e) if e.code() == DWRITE_E_NOCOLOR => {} // 无彩色数据：走单色
                    Err(_) => {}                                 // 其它失败：保守走单色
                }
            }

            // 单色：用文字颜色直接在已拷入真实背景的位图上抗锯齿混合。
            unsafe {
                self.target.DrawGlyphRun(
                    baseline_x,
                    baseline_y,
                    measuring_mode,
                    glyph_run,
                    &self.params,
                    COLORREF(colorref),
                    None,
                )?;
            }
            Ok(())
        }

        fn DrawUnderline(
            &self,
            _ctx: *const c_void,
            _x: f32,
            _y: f32,
            _underline: *const DWRITE_UNDERLINE,
            _effect: Option<&windows::core::IUnknown>,
        ) -> windows::core::Result<()> {
            Ok(())
        }

        fn DrawStrikethrough(
            &self,
            _ctx: *const c_void,
            _x: f32,
            _y: f32,
            _strikethrough: *const DWRITE_STRIKETHROUGH,
            _effect: Option<&windows::core::IUnknown>,
        ) -> windows::core::Result<()> {
            Ok(())
        }

        fn DrawInlineObject(
            &self,
            _ctx: *const c_void,
            _x: f32,
            _y: f32,
            _obj: Option<&IDWriteInlineObject>,
            _sideways: BOOL,
            _rtl: BOOL,
            _effect: Option<&windows::core::IUnknown>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
    }
} // mod imp (windows)

// macOS：真字形渲染走 CoreText（text/coretext.rs），re-export 为本模块的 TextRenderer。
#[cfg(target_os = "macos")]
pub use crate::text::coretext::TextRenderer;

// Linux 等其余非 Windows 平台：保留 mock 桩（仅供编译/测试，无真实字形）。
#[cfg(all(not(windows), not(target_os = "macos")))]
pub use imp::TextRenderer;

/// 非 Windows/非 macOS mock：测量用等宽近似（字符数 × 字号 × 0.6），绘制为空操作。
/// 让候选窗/工具栏/菜单等布局逻辑能在 Linux 上编译与跑测试。
#[cfg(all(not(windows), not(target_os = "macos")))]
mod imp {
    use super::{TextMetrics, TextStyle};

    pub struct TextRenderer {
        font_size: f32,
    }

    impl TextRenderer {
        pub fn new(_font_family: &str, font_size: f32) -> Result<Self, String> {
            Ok(Self { font_size })
        }

        pub fn base_size(&self) -> f32 {
            self.font_size
        }

        pub fn set_base_size(&mut self, size: f32) {
            self.font_size = size;
        }

        pub fn set_font_family(&mut self, _font_family: &str) {}

        pub fn set_chaizi_font(&mut self, _path: &str, _family: &str) -> Result<(), String> {
            Ok(())
        }

        pub fn measure_text(&self, text: &str) -> TextMetrics {
            self.measure_text_sized(text, self.font_size)
        }

        pub fn measure_text_sized(&self, text: &str, size: f32) -> TextMetrics {
            if text.is_empty() {
                return TextMetrics {
                    width: 0.0,
                    height: size * 1.2,
                };
            }
            TextMetrics {
                width: text.chars().count() as f32 * size * 0.6,
                height: size * 1.2,
            }
        }

        /// mock：字重/字体族不影响等宽近似测量，只取字号。
        pub fn measure(&self, text: &str, ts: &TextStyle) -> TextMetrics {
            self.measure_text_sized(text, ts.size)
        }

        #[allow(clippy::too_many_arguments)]
        pub fn draw_text(
            &self,
            _buf: &mut [u8],
            _buf_width: u32,
            _buf_height: u32,
            _x: f32,
            _y: f32,
            _text: &str,
            _color: [u8; 4],
        ) -> Result<(), String> {
            Ok(())
        }

        #[allow(clippy::too_many_arguments)]
        pub fn draw_text_sized(
            &self,
            _buf: &mut [u8],
            _buf_width: u32,
            _buf_height: u32,
            _x: f32,
            _y: f32,
            _text: &str,
            _size: f32,
            _color: [u8; 4],
        ) -> Result<(), String> {
            Ok(())
        }

        /// mock：绘制空操作（样式忽略）。
        #[allow(clippy::too_many_arguments)]
        pub fn draw(
            &self,
            _buf: &mut [u8],
            _buf_width: u32,
            _buf_height: u32,
            _x: f32,
            _y: f32,
            _text: &str,
            _ts: &TextStyle,
            _color: [u8; 4],
        ) -> Result<(), String> {
            Ok(())
        }
    }
}

// 换行语义：关自动换行、留硬换行。两条缺一不可——只验前者会让多行候选静默退化成单行，
// 只验后者则放任溢出继续。需要真实 DirectWrite，gate 到 Windows。
#[cfg(all(test, windows))]
mod wrapping_tests {
    use super::{TextRenderer, TextStyle};

    fn tr() -> TextRenderer {
        TextRenderer::new("Microsoft YaHei UI", 16.0).expect("建 TextRenderer")
    }

    /// 自动换行必须关闭：宽过缓冲的文本只画一行（超出部分裁掉），不得折行。
    ///
    /// 折行的后果不是「看不全」而是「画到别处」——多出来的行落在节点框外，盖住相邻候选。
    /// 竖排 `vertical_max_width`（用户/主题显式配置正值时）或渲染层恒生效的屏幕安全钳制
    /// 都会钳窗口宽度，触发这条路径（出厂默认值为 0=不限，此处直接构造窄缓冲区复现）。
    #[test]
    fn long_text_clips_instead_of_wrapping() {
        let r = tr();
        let ts = TextStyle::new(16.0);
        let line_h = r.measure("中", &ts).height;

        const BW: u32 = 60; // 远窄于文本宽度
        const BH: u32 = 120;
        let mut buf = vec![255u8; (BW * BH * 4) as usize];
        r.draw(
            &mut buf,
            BW,
            BH,
            0.0,
            0.0,
            "这是一个很长的候选词条",
            &ts,
            [0, 0, 0, 255],
        )
        .expect("draw");

        let bottom = (0..BH as i32)
            .rfind(|&y| (0..BW as i32).any(|x| buf[((y * BW as i32 + x) * 4 + 2) as usize] < 128))
            .unwrap_or(-1);
        assert!(
            bottom >= 0,
            "字形应当被画出来，否则本用例测的是「什么都没画」"
        );
        assert!(
            (bottom as f32) <= line_h,
            "宽过缓冲的文本应裁切在单行内（底边 {bottom} ≤ 行高 {line_h:.0}），\
             实测溢出说明自动换行又被打开了"
        );
    }

    /// `\n` 硬换行必须保留——candidate_window 依赖它做多行候选。
    /// 这是上一条修复的边界：NO_WRAP 只该关自动换行，一起关掉硬换行就是过度修复。
    #[test]
    fn hard_newline_still_breaks_lines() {
        let r = tr();
        let ts = TextStyle::new(16.0);
        let one = r.measure("中文", &ts);
        let two = r.measure("中文\n第二行", &ts);
        assert!(
            two.height > one.height * 1.5,
            "含 \\n 的文本应约为两倍行高（实得 {:.1} vs 单行 {:.1}）",
            two.height,
            one.height
        );
    }
}

// 文字色 alpha 的**像素级**验证：混合公式改的是逐像素算术，只靠逻辑推导不算验过。
// 需要真实 DirectWrite 出字形，故 gate 到 Windows。
#[cfg(all(test, windows))]
mod alpha_text_tests {
    use super::{TextRenderer, TextStyle};

    const W: u32 = 48;
    const H: u32 = 48;

    /// 不透明白底的 BGRA 预乘缓冲（A=255 时预乘即直通）。
    fn white_buf() -> Vec<u8> {
        vec![255u8; (W * H * 4) as usize]
    }

    /// 缓冲中最暗的 R 通道值。取最暗而非固定坐标——字形的确切落点随字体/hinting 变，
    /// 但"块体最暗处"这个判据与位置无关。
    fn darkest_r(buf: &[u8]) -> u8 {
        buf.chunks_exact(4).map(|p| p[2]).min().unwrap_or(255)
    }

    /// 在白底画一个全块字符（█ U+2588，覆盖率≈1），返回缓冲。
    fn draw_block(alpha: u8) -> Vec<u8> {
        let r = TextRenderer::new("微软雅黑", 32.0).expect("建 TextRenderer");
        let mut buf = white_buf();
        r.draw(
            &mut buf,
            W,
            H,
            0.0,
            0.0,
            "\u{2588}",
            &TextStyle::new(32.0),
            [0, 0, 0, alpha],
        )
        .expect("draw");
        buf
    }

    /// alpha=255：全块应压到近黑——同时确认字形真的画出来了（否则下一条测了个寂寞）。
    #[test]
    fn opaque_text_is_near_black() {
        let d = darkest_r(&draw_block(255));
        assert!(d < 64, "不透明黑块中心应近黑，实得 {d}");
    }

    /// alpha=128：同一个全块应落在中灰，而非近黑。
    ///
    /// 这是修复前后的分水岭——旧实现丢弃 `color[3]`，此处会与上一条同样得到近黑。
    /// 区间放宽到 96..=176 以容纳字体覆盖率与 ClearType 的差异；判据是"明显不是黑"。
    #[test]
    fn half_alpha_text_blends_to_midtone() {
        let d = darkest_r(&draw_block(128));
        assert!(
            (96..=176).contains(&d),
            "50% alpha 黑块应混成中灰（96..=176），实得 {d}——落在近黑说明 alpha 又被丢了"
        );
    }

    /// 单调性：alpha 越低，字越淡。比固定区间更稳，不受字体覆盖率影响。
    #[test]
    fn lower_alpha_yields_lighter_text() {
        let opaque = darkest_r(&draw_block(255));
        let half = darkest_r(&draw_block(128));
        let faint = darkest_r(&draw_block(48));
        assert!(
            opaque < half && half < faint,
            "alpha 越低字应越淡，实得 255→{opaque} 128→{half} 48→{faint}"
        );
    }
}

// 测量缓存的**接线**测试：键函数再正确，没接进 `TextRenderer::measure` 也是白搭，
// 而 `measure_key_tests` 直接调键函数，接线断了它照样全绿。这里从公开的测量入口进，
// 用缓存条目数确认它真的被查过、被写过。
//
// 需要真实 DirectWrite（`TextRenderer::new` 建 COM 工厂），故 gate 到 Windows；
// 键本身的正确性由跨平台的 `measure_key_tests` 在 Linux CI 上守。
#[cfg(all(test, windows))]
mod measure_cache_tests {
    use super::{TextRenderer, TextStyle};

    fn tr() -> TextRenderer {
        TextRenderer::new("微软雅黑", 14.0).expect("建 DirectWrite TextRenderer")
    }

    /// 默认样式 + 指定字号。
    fn ts(size: f32) -> TextStyle<'static> {
        TextStyle::new(size)
    }

    /// 测量结果入缓存，重复测量命中而不新增条目。
    #[test]
    fn repeated_measure_hits_cache() {
        let r = tr();
        assert_eq!(r.measure_cache_len(), 0, "起手应为空");
        let a = r.measure("你好", &ts(14.0));
        assert_eq!(r.measure_cache_len(), 1, "首次测量应入缓存");
        let b = r.measure("你好", &ts(14.0));
        assert_eq!(r.measure_cache_len(), 1, "重复测量应命中，不得新增");
        assert_eq!(a.width, b.width, "命中值须与首次一致");
        assert_eq!(a.height, b.height);
    }

    /// 空串走的是提前返回，不该占用缓存条目。
    #[test]
    fn empty_text_does_not_populate_cache() {
        let r = tr();
        let _ = r.measure("", &ts(14.0));
        assert_eq!(r.measure_cache_len(), 0);
    }

    /// 换字体族清空缓存——键里 `None` 表示"用全局 family"，全局一换这些条目就失效了。
    #[test]
    fn set_font_family_clears_cache() {
        let mut r = tr();
        let _ = r.measure("你好", &ts(14.0));
        assert_eq!(r.measure_cache_len(), 1);
        r.set_font_family("宋体");
        assert_eq!(r.measure_cache_len(), 0, "换字体族须清空测量缓存");
    }

    /// 不同字号各占一条（键含字号），且两者宽度确有差异——顺带证明缓存没把它们混为一谈。
    #[test]
    fn distinct_sizes_are_cached_separately() {
        let r = tr();
        let small = r.measure("你好", &ts(12.0));
        let large = r.measure("你好", &ts(24.0));
        assert_eq!(r.measure_cache_len(), 2, "两种字号应各占一条");
        assert!(
            large.width > small.width,
            "24px 应宽于 12px（得 {} vs {}）",
            large.width,
            small.width
        );
    }
}

// 测量缓存键的跨平台测试（`measure_key` 不依赖 DirectWrite，与 `pua_runs` 同样不限平台）。
//
// 这里测的是**键的区分度**而非缓存命中：键漏掉任何一项影响测量的输入，后果都是某段文本
// 静默套用另一段的宽度——布局错位，且因为是缓存命中路径，重现条件依赖于测量顺序，极难定位。
#[cfg(test)]
mod measure_key_tests {
    use super::{TextStyle, measure_key};

    /// 字号 14、字重 400、指定字体族的基准样式。
    fn base_style() -> TextStyle<'static> {
        TextStyle::new(14.0)
            .with_weight(400)
            .with_family(Some("微软雅黑"))
    }

    /// 同一组输入恒得同一个键（缓存能命中的前提）。
    #[test]
    fn same_inputs_yield_same_key() {
        assert_eq!(
            measure_key("你好", &base_style()),
            measure_key("你好", &base_style())
        );
    }

    /// 四项输入各自独立参与键——逐项只改一个，键都必须变。
    #[test]
    fn each_input_affects_key() {
        let s = base_style();
        let base = measure_key("你好", &s);
        assert_ne!(base, measure_key("你好啊", &s), "文本");
        assert_ne!(
            base,
            measure_key("你好", &TextStyle { size: 16.0, ..s }),
            "字号"
        );
        assert_ne!(base, measure_key("你好", &s.with_weight(700)), "字重");
        assert_ne!(
            base,
            measure_key("你好", &s.with_family(Some("宋体"))),
            "字体族"
        );
    }

    /// 字号必须按 `to_bits()` 精确入键，不能取整。
    ///
    /// 字号是 DPI 缩放后的浮点：125% 下 12px→15.0、13px→16.25，150% 下 14px→21.0。
    /// 若按 `as u32`/`round()` 入键，16.25 与 16.8 会撞进同一条缓存——注释与正文只差
    /// 一两个像素时恰好落进这个区间，表现为某一档 DPI 下注释宽度突然用了正文的值。
    #[test]
    fn fractional_sizes_do_not_collide() {
        assert_ne!(
            measure_key("你好", &TextStyle::new(16.25)),
            measure_key("你好", &TextStyle::new(16.8)),
            "同一整数区间内的两个字号不得共用缓存键"
        );
    }

    /// `None`（用全局字体族）与显式指定不是一回事：`set_font_family` 只会让前者失效。
    /// 两者若共用键，换字体后显式指定的条目会被连带清掉（性能损失，无正确性问题），
    /// 更糟的是反过来——全局族的条目被显式族的值命中，直接就是错误宽度。
    #[test]
    fn none_family_differs_from_explicit() {
        let s = TextStyle::new(14.0);
        assert_ne!(
            measure_key("你好", &s),
            measure_key("你好", &s.with_family(Some("微软雅黑"))),
        );
    }

    /// 空串字体族经 `with_family` 归一成 `None`——统一在构造处收口，免得各调用点
    /// 各自过滤，漏一处就多出一条与 `None` 等价却不同键的缓存。
    #[test]
    fn empty_family_normalizes_to_none() {
        let s = TextStyle::new(14.0);
        assert_eq!(
            measure_key("你好", &s),
            measure_key("你好", &s.with_family(Some(""))),
            "空串字体族应归一为 None"
        );
        assert_eq!(s.with_family(Some("  ")).family, None, "纯空白也应归一");
    }
}

// 私用区分段的跨平台测试（`pua_runs` 不依赖 DirectWrite，故不限平台，Windows 本机
// `cargo test` 也覆盖）。真实数据取自两份拆字库的首行，避免自造码位掩盖区间边界错误。
#[cfg(test)]
mod pua_runs_tests {
    use super::pua_runs;

    fn runs(s: &str) -> Vec<(usize, usize)> {
        pua_runs(&s.encode_utf16().collect::<Vec<u16>>())
    }

    /// 内置 wubi86 拆字库："的" → U+E0E1 U+E124 U+E147 U+E13D（BMP 私用区，单码元）。
    #[test]
    fn bmp_pua_run_is_detected() {
        assert_eq!(runs("\u{E0E1}\u{E124}\u{E147}\u{E13D}"), vec![(0, 4)]);
    }

    /// 986 拆字库："的" → U+F00FD U+F00F7 U+F013C（补充私用区 A，各占 2 码元）。
    /// 修复前这一段完全不命中，字根落回主字体渲染成方框。
    #[test]
    fn spua_a_run_is_detected() {
        assert_eq!(runs("\u{F00FD}\u{F00F7}\u{F013C}"), vec![(0, 6)]);
    }

    /// 补充私用区 B（第 16 平面）同样纳入。
    #[test]
    fn spua_b_run_is_detected() {
        assert_eq!(runs("\u{100000}\u{10FFFD}"), vec![(0, 4)]);
    }

    /// 非私用区的**代理对不得命中**——CJK 扩展 B（U+20000）等生僻字若被误切到字根
    /// 字体集，反而会变成方框。这是判据不能只看"是不是代理对"的原因。
    #[test]
    fn non_pua_supplementary_chars_are_excluded() {
        assert!(runs("\u{20000}\u{2A6DF}\u{1F600}").is_empty());
    }

    /// 混排：汉字 + 字根 + 编码，段起止按 UTF-16 码元定位（非字符数）。
    /// "的" 1 码元 + "：" 1 码元 → 字根段从下标 2 起、占 6 码元。
    #[test]
    fn mixed_text_run_offsets_are_utf16_units() {
        assert_eq!(runs("的：\u{F00FD}\u{F00F7}\u{F013C} rqy"), vec![(2, 6)]);
    }

    /// 多段：被普通字符隔开的字根分别成段。
    #[test]
    fn separate_runs_are_not_merged_across_plain_text() {
        assert_eq!(runs("\u{E0E1}中\u{F00FD}"), vec![(0, 1), (2, 2)]);
    }

    /// BMP 与补充私用区相邻时合并为一段（目标字体族相同，合并不改变渲染）。
    #[test]
    fn adjacent_bmp_and_supplementary_pua_merge() {
        assert_eq!(runs("\u{E0E1}\u{F00FD}"), vec![(0, 3)]);
    }

    /// 孤立高位代理（非法 UTF-16，可能来自截断的外部数据）不得命中、不得越界 panic。
    #[test]
    fn lone_lead_surrogate_is_ignored() {
        assert!(pua_runs(&[0xDB80]).is_empty());
        assert!(pua_runs(&[0xDB80, 0x4E2D]).is_empty());
    }

    #[test]
    fn empty_and_plain_text_yield_no_runs() {
        assert!(runs("").is_empty());
        assert!(runs("中文 abc 123").is_empty());
    }
}

// 非 Windows mock 文本渲染器的冒烟测试：验证 mock 的等宽近似测量契约
// （字符数 × 字号 × 0.6）与 draw_text 空操作。
// 边界：真实字形宽度/渲染由 Windows + DirectWrite 决定，**不在此覆盖，须 Windows 实测**。
#[cfg(all(test, not(windows), not(target_os = "macos")))]
mod tests {
    use super::TextRenderer;

    #[test]
    fn mock_measure_empty_is_zero_width() {
        let tr = TextRenderer::new("any", 20.0).unwrap();
        let m = tr.measure_text("");
        assert_eq!(m.width, 0.0);
        assert!(m.height > 0.0);
    }

    #[test]
    fn mock_measure_scales_with_char_count() {
        let tr = TextRenderer::new("any", 20.0).unwrap();
        let one = tr.measure_text("中").width;
        let three = tr.measure_text("中文字").width;
        assert!(three > one);
        assert!((three - one * 3.0).abs() < 1e-3);
    }

    #[test]
    fn mock_draw_text_is_ok() {
        let tr = TextRenderer::new("any", 16.0).unwrap();
        let mut buf = vec![0u8; 8 * 8 * 4];
        assert!(
            tr.draw_text(&mut buf, 8, 8, 0.0, 0.0, "x", [0, 0, 0, 255])
                .is_ok()
        );
    }
}
