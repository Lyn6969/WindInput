//! 截图工具函数：LayeredWindow buffer → PNG 文件 / 剪贴板。
//!
//! 对齐 Go `wind_input/internal/ui/manager_screenshot.go`。
//!
//! 不依赖 PrintWindow（路径随 windows crate 版本变化）；
//! 直接使用各窗口已渲染到 UpdateLayeredWindow 的 BGRA buffer，
//! 避免引入 GDI 屏幕捕获的剪切区/层级等问题。

use std::path::Path;

/// 返回本地时间戳字符串 YYYYMMDD_HHMMSS，用于截图文件名。
#[cfg(windows)]
pub fn timestamp() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe { GetLocalTime() };
    format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

#[cfg(not(windows))]
pub fn timestamp() -> String {
    "00000000_000000".to_string()
}

/// 裁去四周 alpha ≤ 阈值的透明边缘行/列，返回 (裁剪后 buffer, new_w, new_h)。
/// 用于去除主题阴影/圆角造成的空白边框。全透明时原样返回。
fn trim_transparent(buf: &[u8], w: u32, h: u32) -> (Vec<u8>, u32, u32) {
    const ALPHA_THRESHOLD: u8 = 10;
    if buf.is_empty() || w == 0 || h == 0 {
        return (buf.to_vec(), w, h);
    }
    let stride = (w * 4) as usize;
    let mut top = h;
    let mut bottom = 0u32;
    let mut left = w;
    let mut right = 0u32;

    for y in 0..h {
        for x in 0..w {
            let alpha = buf[y as usize * stride + x as usize * 4 + 3];
            if alpha > ALPHA_THRESHOLD {
                if y < top {
                    top = y;
                }
                if y + 1 > bottom {
                    bottom = y + 1;
                }
                if x < left {
                    left = x;
                }
                if x + 1 > right {
                    right = x + 1;
                }
            }
        }
    }

    if top >= bottom || left >= right {
        return (buf.to_vec(), w, h);
    }

    let new_w = right - left;
    let new_h = bottom - top;
    let mut out = Vec::with_capacity((new_w * new_h * 4) as usize);
    for y in top..bottom {
        let row_start = y as usize * stride + left as usize * 4;
        out.extend_from_slice(&buf[row_start..row_start + new_w as usize * 4]);
    }
    (out, new_w, new_h)
}

/// 反预乘单像素的 RGB 通道：`c' = c × 255 / a`（四舍五入，钳到 255）。
///
/// 窗口 buffer 按 `UpdateLayeredWindow`(`AC_SRC_ALPHA`) 的要求维护**预乘 alpha**，
/// 而 PNG 的 alpha 是**直通**语义。不还原就等于把预乘值当直通值写出去，
/// 半透明像素会被系统性压暗：白底圆角抗锯齿边（a≈128）存的是 128，
/// 看图器按直通合成到白底得 128×0.5+255×0.5=191 的灰边。
/// 仅 `0 < a < 255` 的像素受影响，故症状只出现在圆角/阴影边缘。
#[inline]
fn unpremultiply(px: &mut [u8]) {
    let a = px[3];
    if a == 255 {
        return;
    }
    if a == 0 {
        // 全透明像素的 RGB 无意义，归零避免编码器写出噪声色
        px[0] = 0;
        px[1] = 0;
        px[2] = 0;
        return;
    }
    let a = a as u32;
    for c in &mut px[..3] {
        // 文本选择性回写等路径可能产生 c > a 的非法预乘像素，钳位兜底
        *c = (((*c as u32 * 255) + a / 2) / a).min(255) as u8;
    }
}

/// 将 BGRA 像素缓冲区编码为 PNG 并写入 path（包含自动创建父目录）。
/// 自动裁去四周透明边缘（主题阴影/圆角留白），并把预乘 alpha 还原为 PNG 的直通 alpha。
pub fn save_bgra_to_png(buffer: &[u8], width: u32, height: u32, path: &Path) -> Result<(), String> {
    if buffer.is_empty() || width == 0 || height == 0 {
        return Err("empty buffer or zero size".into());
    }
    let (trimmed, w, h) = trim_transparent(buffer, width, height);
    // BGRA（预乘）→ RGBA（直通）：Win32 DIB 以 B-G-R-A 顺序存储
    let mut rgba = trimmed;
    for chunk in rgba.chunks_exact_mut(4) {
        chunk.swap(0, 2); // B ↔ R
        unpremultiply(chunk);
    }
    let img: image::RgbaImage =
        image::ImageBuffer::from_vec(w, h, rgba).ok_or("ImageBuffer::from_vec failed")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    img.save(path).map_err(|e| format!("PNG encode: {e}"))
}

/// 将 BGRA 像素缓冲区以 CF_DIB 格式放入剪贴板（可直接粘贴到各类应用）。
///
/// CF_DIB（格式 8）= 全局内存块，内容为 BITMAPINFOHEADER + 像素数据（底到顶行序）。
/// 比 CF_BITMAP（GDI 句柄，格式 2）兼容性更好：微信/浏览器/Office 等均支持 CF_DIB。
/// 自动裁去四周透明边缘再合成白底，去除阴影留白。
#[cfg(windows)]
pub fn copy_bgra_to_clipboard(buffer: &[u8], width: u32, height: u32) -> Result<(), String> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFOHEADER};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

    const CF_DIB: u32 = 8;

    if buffer.is_empty() || width == 0 || height == 0 {
        return Err("empty buffer or zero size".into());
    }

    // 先裁去透明边缘，再合成白底（CF_DIB 无 alpha 通道，否则透明区域会变黑）
    let (trimmed, w, h) = trim_transparent(buffer, width, height);

    // 预乘 BGRA 合成到白色背景：f = premult + (255 - alpha)
    let composited: Vec<u8> = trimmed
        .chunks_exact(4)
        .flat_map(|px| {
            let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
            let inv = 255u8.saturating_sub(a);
            [
                b.saturating_add(inv),
                g.saturating_add(inv),
                r.saturating_add(inv),
                255u8,
            ]
        })
        .collect();

    // CF_DIB 内存布局：BITMAPINFOHEADER（40 字节）+ 像素数据（底到顶行序）
    let header_size = std::mem::size_of::<BITMAPINFOHEADER>();
    let row_bytes = (w * 4) as usize;
    let data_size = row_bytes * h as usize;

    unsafe {
        let hmem = GlobalAlloc(GMEM_MOVEABLE, header_size + data_size)
            .map_err(|e| format!("GlobalAlloc: {e}"))?;
        let ptr = GlobalLock(hmem) as *mut u8;
        if ptr.is_null() {
            return Err("GlobalLock returned null".into());
        }

        // 写入 BITMAPINFOHEADER（positive biHeight = 底到顶，CF_DIB 标准格式）
        let bih = BITMAPINFOHEADER {
            biSize: header_size as u32,
            biWidth: w as i32,
            biHeight: h as i32,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: data_size as u32,
            ..Default::default()
        };
        std::ptr::copy_nonoverlapping(
            &bih as *const BITMAPINFOHEADER as *const u8,
            ptr,
            header_size,
        );

        // 写入像素：composited 是顶到底，CF_DIB 需底到顶，逐行翻转
        let pixels_ptr = ptr.add(header_size);
        for y in 0..h as usize {
            let src_y = h as usize - 1 - y;
            std::ptr::copy_nonoverlapping(
                composited.as_ptr().add(src_y * row_bytes),
                pixels_ptr.add(y * row_bytes),
                row_bytes,
            );
        }
        let _ = GlobalUnlock(hmem);

        OpenClipboard(None).map_err(|e| format!("OpenClipboard: {e}"))?;
        if let Err(e) = EmptyClipboard() {
            let _ = CloseClipboard();
            return Err(format!("EmptyClipboard: {e}"));
        }
        // SetClipboardData 成功 → 系统接管 hmem 所有权
        if let Err(e) = SetClipboardData(CF_DIB, HANDLE(hmem.0)) {
            let _ = CloseClipboard();
            return Err(format!("SetClipboardData: {e}"));
        }
        let _ = CloseClipboard();
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn copy_bgra_to_clipboard(_buffer: &[u8], _width: u32, _height: u32) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在直通 alpha 语义下把像素合成到白底，模拟看图器的显示结果。
    fn over_white(c: u8, a: u8) -> u8 {
        let (c, a) = (c as f32, a as f32 / 255.0);
        (c * a + 255.0 * (1.0 - a)).round() as u8
    }

    #[test]
    fn unpremultiply_restores_white_rounded_edge() {
        // 白底主题圆角抗锯齿边：预乘存 128，还原后应是纯白 255。
        let mut px = [128u8, 128, 128, 128];
        unpremultiply(&mut px);
        assert_eq!(&px[..3], &[255, 255, 255]);
        // 关键判据：合成到白底不再产生灰边（修复前此处得 191）。
        assert_eq!(over_white(px[0], px[3]), 255);
    }

    #[test]
    fn unpremultiply_keeps_opaque_and_zeroes_transparent() {
        let mut opaque = [10u8, 20, 30, 255];
        unpremultiply(&mut opaque);
        assert_eq!(opaque, [10, 20, 30, 255], "不透明像素不应被改动");

        let mut clear = [77u8, 88, 99, 0];
        unpremultiply(&mut clear);
        assert_eq!(clear, [0, 0, 0, 0], "全透明像素 RGB 归零");
    }

    #[test]
    fn unpremultiply_clamps_illegal_pixels() {
        // c > a 的非法预乘像素（文本选择性回写等路径可能产生）不得溢出。
        let mut px = [200u8, 200, 200, 100];
        unpremultiply(&mut px);
        assert_eq!(&px[..3], &[255, 255, 255]);
    }

    #[test]
    fn unpremultiply_preserves_black_shadow() {
        // 阴影是纯黑半透明，反预乘后不变——故此 bug 从不影响阴影，只影响浅色圆角边。
        let mut px = [0u8, 0, 0, 40];
        unpremultiply(&mut px);
        assert_eq!(px, [0, 0, 0, 40]);
    }
}
