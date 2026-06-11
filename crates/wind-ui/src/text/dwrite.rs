//! 文本渲染后端（GDI 实现，后续升级到 DirectWrite）
//!
//! 使用 Win32 GDI TextOutW 渲染文本到 BGRA 缓冲区。
//! 支持中文字符，自动创建合适大小的字体。

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use tracing::{debug, error, info, warn};

/// 文本渲染器
pub struct TextRenderer {
    font_family: String,
    font_size: f32,
}

/// 文本度量信息
#[derive(Debug, Clone)]
pub struct TextMetrics {
    pub width: f32,
    pub height: f32,
}

impl TextRenderer {
    /// 创建文本渲染器
    pub fn new(font_family: &str, font_size: f32) -> Result<Self, String> {
        Ok(Self {
            font_family: font_family.to_string(),
            font_size,
        })
    }

    /// 创建 GDI 字体
    fn create_font(&self) -> Result<HFONT, String> {
        let family_wide: Vec<u16> = self.font_family.encode_utf16().chain(std::iter::once(0)).collect();

        // 字体高度（逻辑像素，GDI 会自动处理 DPI 缩放）
        let height = -(self.font_size as i32);

        unsafe {
            let hfont = CreateFontW(
                height,    // nHeight
                0,         // nWidth (自动)
                0,         // nEscapement
                0,         // nOrientation
                400,       // nWeight (FW_NORMAL = 400)
                0,         // bItalic
                0,         // bUnderline
                0,         // bStrikeOut
                1,         // nCharSet (DEFAULT_CHARSET = 1)
                0,         // nOutputPrecision (OUT_DEFAULT_PRECIS = 0)
                0,         // nClipPrecision (CLIP_DEFAULT_PRECIS = 0)
                5,         // nQuality (CLEARTYPE_QUALITY = 5)
                0,         // nPitchAndFamily (DEFAULT_PITCH | FF_DONTCARE = 0)
                windows::core::PCWSTR(family_wide.as_ptr()),
            );

            if hfont.is_invalid() {
                return Err("CreateFontW failed".to_string());
            }

            Ok(hfont)
        }
    }

    /// 测量文本尺寸
    pub fn measure_text(&self, text: &str) -> TextMetrics {
        if text.is_empty() {
            return TextMetrics { width: 0.0, height: self.font_size * 1.2 };
        }

        unsafe {
            let hdc = GetDC(HWND::default());
            let hfont = match self.create_font() {
                Ok(f) => f,
                Err(_) => {
                    ReleaseDC(HWND::default(), hdc);
                    return TextMetrics {
                        width: text.len() as f32 * self.font_size * 0.6,
                        height: self.font_size * 1.2,
                    };
                }
            };

            let old_font: HGDIOBJ = SelectObject(hdc, HGDIOBJ(hfont.0));

            let text_wide: Vec<u16> = text.encode_utf16().collect();
            let mut size = SIZE::default();
            let _ = GetTextExtentPoint32W(hdc, &text_wide, &mut size);

            SelectObject(hdc, old_font);
            let _: BOOL = DeleteObject(HGDIOBJ(hfont.0));
            ReleaseDC(HWND::default(), hdc);

            TextMetrics {
                width: size.cx as f32,
                height: size.cy as f32,
            }
        }
    }

    /// 渲染文本到 BGRA 缓冲区
    ///
    /// - `buf`: 目标 BGRA 缓冲区（已包含背景）
    /// - `buf_width`, `buf_height`: 缓冲区尺寸
    /// - `x`, `y`: 文本绘制位置（逻辑坐标）
    /// - `text`: 要渲染的文本
    /// - `color`: 文本颜色 (B, G, R, A)
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
        if text.is_empty() {
            return Ok(());
        }

        unsafe {
            // 创建内存 DC
            let hdc_screen = GetDC(HWND::default());
            let hdc_mem = CreateCompatibleDC(hdc_screen);

            // 创建 DIB
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: buf_width as i32,
                    biHeight: -(buf_height as i32), // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                ..std::mem::zeroed()
            };

            let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
            let hbitmap = match CreateDIBSection(
                hdc_mem,
                &bmi,
                DIB_RGB_COLORS,
                &mut bits_ptr,
                None,
                0,
            ) {
                Ok(h) => h,
                Err(e) => {
                    DeleteDC(hdc_mem);
                    ReleaseDC(HWND::default(), hdc_screen);
                    return Err(format!("CreateDIBSection failed: {}", e));
                }
            };

            let old_bmp: HGDIOBJ = SelectObject(hdc_mem, HGDIOBJ(hbitmap.0));

            // 清零位图（透明背景）
            let bitmap_size = (buf_width * buf_height * 4) as usize;
            std::ptr::write_bytes(bits_ptr as *mut u8, 0, bitmap_size);

            // 设置文本颜色 (COLORREF = 0x00BBGGRR)
            let text_color_ref = COLORREF(
                (color[0] as u32) | ((color[1] as u32) << 8) | ((color[2] as u32) << 16)
            );
            SetTextColor(hdc_mem, text_color_ref);
            SetBkMode(hdc_mem, TRANSPARENT);

            // 创建并选择字体
            let hfont = match self.create_font() {
                Ok(f) => f,
                Err(e) => {
                    SelectObject(hdc_mem, old_bmp);
                    let _: BOOL = DeleteObject(HGDIOBJ(hbitmap.0));
                    DeleteDC(hdc_mem);
                    ReleaseDC(HWND::default(), hdc_screen);
                    return Err(e);
                }
            };
            let old_font: HGDIOBJ = SelectObject(hdc_mem, HGDIOBJ(hfont.0));

            // 绘制文本
            let text_wide: Vec<u16> = text.encode_utf16().collect();
            let pixel_x = x as i32;
            let pixel_y = y as i32;
            let _ = TextOutW(hdc_mem, pixel_x, pixel_y, &text_wide);

            // 从 DIB 复制像素到目标缓冲区
            let src_buf = std::slice::from_raw_parts(
                bits_ptr as *const u8,
                (buf_width * buf_height * 4) as usize,
            );

            // 合并：GDI TextOutW 不设置 alpha，需要检测 RGB 非零像素
            for i in 0..(buf_width * buf_height) as usize {
                let idx = i * 4;
                let src_b = src_buf[idx];
                let src_g = src_buf[idx + 1];
                let src_r = src_buf[idx + 2];

                // 如果 RGB 有值（文字像素），设置 alpha=255
                if src_b > 0 || src_g > 0 || src_r > 0 {
                    buf[idx] = src_b;
                    buf[idx + 1] = src_g;
                    buf[idx + 2] = src_r;
                    buf[idx + 3] = 255; // 完全不透明
                }
            }

            // 清理
            SelectObject(hdc_mem, old_font);
            let _: BOOL = DeleteObject(HGDIOBJ(hfont.0));
            SelectObject(hdc_mem, old_bmp);
            let _: BOOL = DeleteObject(HGDIOBJ(hbitmap.0));
            DeleteDC(hdc_mem);
            ReleaseDC(HWND::default(), hdc_screen);

            Ok(())
        }
    }
}
