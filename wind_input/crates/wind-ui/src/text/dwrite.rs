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

#[cfg(windows)]
pub use imp::TextRenderer;

/// Windows 实现（DirectWrite）。非 Windows 平台见文件末尾的 mock。
#[cfg(windows)]
mod imp {
    use super::TextMetrics;
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
                Ok(())
            }
        }

        /// 基准字号（View 叶子未显式指定字号时回退）。
        pub fn base_size(&self) -> f32 {
            self.font_size
        }

        /// 更新基准字号（DPI 动态变化时调用）。格式按 px 缓存，无需重建 COM 对象，
        /// 仅改变未显式指定字号的叶子的回退字号。
        pub fn set_base_size(&mut self, size: f32) {
            self.font_size = size;
        }

        /// 切换字体族（ui.font.family 变更时调用）。清空按字号缓存的 TextFormat，使新字体生效。
        pub fn set_font_family(&mut self, font_family: &str) {
            self.family = font_family
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            self.formats.borrow_mut().clear();
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
                // 拆字字根：对 PUA 码位（U+E000..=U+F8FF，皆 BMP 单码元）的连续段
                // 切到黑体字根字体集，级联回退渲染字根字符。
                if let Some(cf) = &self.chaizi {
                    let mut i = 0usize;
                    while i < wide.len() {
                        if (0xE000..=0xF8FF).contains(&wide[i]) {
                            let start = i;
                            while i < wide.len() && (0xE000..=0xF8FF).contains(&wide[i]) {
                                i += 1;
                            }
                            let range = DWRITE_TEXT_RANGE {
                                startPosition: start as u32,
                                length: (i - start) as u32,
                            };
                            let _ = layout.SetFontCollection(&cf.collection, range);
                            let _ = layout.SetFontFamilyName(PCWSTR(cf.family.as_ptr()), range);
                        } else {
                            i += 1;
                        }
                    }
                }
                Ok(layout)
            }
        }

        /// 测量文本尺寸（用基准字号）。
        pub fn measure_text(&self, text: &str) -> TextMetrics {
            self.measure_text_sized(text, self.font_size)
        }

        /// 测量文本尺寸（指定字号；宽含尾随空白，高为行高）。
        pub fn measure_text_sized(&self, text: &str, size: f32) -> TextMetrics {
            self.measure_text_styled(text, size, 0, None)
        }

        /// 测量文本尺寸（指定字号 + 字重 + 字体族覆盖）。
        pub fn measure_text_styled(
            &self,
            text: &str,
            size: f32,
            weight: i32,
            family: Option<&str>,
        ) -> TextMetrics {
            if text.is_empty() {
                return TextMetrics {
                    width: 0.0,
                    height: size * 1.2,
                };
            }
            let layout = match self.create_layout(
                text,
                size,
                weight,
                family,
                f32::MAX / 2.0,
                f32::MAX / 2.0,
            ) {
                Ok(l) => l,
                Err(_) => {
                    return TextMetrics {
                        width: text.chars().count() as f32 * size * 0.6,
                        height: size * 1.2,
                    };
                }
            };
            unsafe {
                let mut m = DWRITE_TEXT_METRICS::default();
                if layout.GetMetrics(&mut m).is_err() {
                    return TextMetrics {
                        width: text.chars().count() as f32 * size * 0.6,
                        height: size * 1.2,
                    };
                }
                let height = if m.height > 0.0 { m.height } else { size * 1.2 };
                TextMetrics {
                    width: m.widthIncludingTrailingWhitespace,
                    height,
                }
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

        /// 渲染文本到 BGRA 缓冲区。
        ///
        /// - `buf`: 目标 BGRA 缓冲区（已含背景，预乘 alpha）
        /// - `buf_width`/`buf_height`: 缓冲区尺寸
        /// - `x`/`y`: 文本左上角（像素坐标）
        /// - `color`: 文本颜色 [B, G, R, A]
        /// 绘制文本（用基准字号）。
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

        /// 绘制文本（指定字号）。
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
            self.draw_text_styled(buf, buf_width, buf_height, x, y, text, size, 0, None, color)
        }

        /// 绘制文本（指定字号 + 字重 + 字体族覆盖）。
        #[allow(clippy::too_many_arguments)]
        pub fn draw_text_styled(
            &self,
            buf: &mut [u8],
            buf_width: u32,
            buf_height: u32,
            x: f32,
            y: f32,
            text: &str,
            size: f32,
            weight: i32,
            family: Option<&str>,
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
                    size,
                    weight,
                    family,
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
                        buf[s] = (nb as u32 * a / 255) as u8;
                        buf[s + 1] = (ng as u32 * a / 255) as u8;
                        buf[s + 2] = (nr as u32 * a / 255) as u8;
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
    use super::TextMetrics;

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

        /// mock：字重/字体族不影响等宽近似测量，委托 sized。
        pub fn measure_text_styled(
            &self,
            text: &str,
            size: f32,
            _weight: i32,
            _family: Option<&str>,
        ) -> TextMetrics {
            self.measure_text_sized(text, size)
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

        /// mock：绘制空操作（字重/字体族忽略）。
        #[allow(clippy::too_many_arguments)]
        pub fn draw_text_styled(
            &self,
            _buf: &mut [u8],
            _buf_width: u32,
            _buf_height: u32,
            _x: f32,
            _y: f32,
            _text: &str,
            _size: f32,
            _weight: i32,
            _family: Option<&str>,
            _color: [u8; 4],
        ) -> Result<(), String> {
            Ok(())
        }
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
