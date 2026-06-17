//! 背景图解码与填充缓存（九宫格/拉伸/平铺/center）。
//!
//! 与 Go 版 `internal/ui/viewbox_image_resolver.go` 对齐（精简）。线程局部使用（UI 单线程）。
//! 源图解码后保留 unpremult RGBA 供采样；按 (path, mode, slice, dest_w, dest_h) 缓存合成后的
//! 目标位图（tiny-skia Pixmap，**BGRA 序 + 预乘**，可直接作 Pattern 填到 BGRA 缓冲）。

use std::collections::HashMap;
use tiny_skia::{Pixmap, PremultipliedColorU8};

#[inline]
fn transparent() -> PremultipliedColorU8 {
    PremultipliedColorU8::from_rgba(0, 0, 0, 0).unwrap()
}

/// 合成单像素为 BGRA 预乘（R/B 交换以适配 BGRA 缓冲）。
/// tint[3]>0：把图当 alpha mask、用 tint 色填充；否则按 premult 标志直通（svg 已预乘 / 位图未预乘）。
#[inline]
fn compose(r: u8, g: u8, b: u8, a: u8, tint: [u8; 4], premult: bool) -> PremultipliedColorU8 {
    if tint[3] > 0 {
        let ta = ((a as u16 * tint[3] as u16) / 255) as u8;
        let p = |c: u8| ((c as u16 * ta as u16) / 255) as u8;
        PremultipliedColorU8::from_rgba(p(tint[2]), p(tint[1]), p(tint[0]), ta).unwrap_or_else(transparent)
    } else if premult {
        PremultipliedColorU8::from_rgba(b, g, r, a).unwrap_or_else(transparent)
    } else {
        let p = |c: u8| ((c as u16 * a as u16) / 255) as u8;
        PremultipliedColorU8::from_rgba(p(b), p(g), p(r), a).unwrap_or_else(transparent)
    }
}

/// 栅格化 SVG 到 w×h，返回预乘 RGBA 字节（resvg 输出）。
fn rasterize_svg(path: &str, w: u32, h: u32) -> Option<Vec<u8>> {
    let data = std::fs::read(path).ok()?;
    let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    let mut pm = resvg::tiny_skia::Pixmap::new(w, h)?;
    let sx = w as f32 / size.width().max(1.0);
    let sy = h as f32 / size.height().max(1.0);
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(sx, sy),
        &mut pm.as_mut(),
    );
    Some(pm.data().to_vec())
}

/// 填充模式码：0=stretch（默认）1=nine_slice 2=tile 3=center。
pub fn mode_code(mode: &str) -> u8 {
    match mode {
        "nine_slice" => 1,
        "tile" => 2,
        "center" => 3,
        _ => 0, // stretch
    }
}

/// 解码后的源图（unpremult RGBA8，row-major）。
struct Src {
    w: u32,
    h: u32,
    rgba: Vec<u8>,
}

type FillKey = (String, u8, [u32; 4], u32, u32, [u8; 4]);

#[derive(Default)]
pub struct ImageCache {
    src: HashMap<String, Option<Src>>,
    fills: HashMap<FillKey, Option<Pixmap>>,
}

impl ImageCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// 解码源图（缓存；失败缓存 None 避免反复重试）。
    fn decode(&mut self, path: &str) -> Option<&Src> {
        if !self.src.contains_key(path) {
            let decoded = image::open(path).ok().map(|img| {
                let rgba = img.to_rgba8();
                Src {
                    w: rgba.width(),
                    h: rgba.height(),
                    rgba: rgba.into_raw(),
                }
            });
            if decoded.is_none() {
                tracing::warn!("主题背景图解码失败: {}", path);
            }
            self.src.insert(path.to_string(), decoded);
        }
        self.src.get(path).and_then(|o| o.as_ref())
    }

    /// 源图原始尺寸（解码后；用于 layer size=0 时取原尺寸）。
    pub fn src_size(&mut self, path: &str) -> Option<(u32, u32)> {
        self.decode(path).map(|s| (s.w, s.h))
    }

    /// 取（或构建）目标尺寸填充位图（BGRA 序 + 预乘）。
    /// tint=[0,0,0,0] 表示不染色；非零时把图当 alpha mask、用 tint 色填充（单色 SVG/图标随主题变色）。
    pub fn fill(
        &mut self,
        path: &str,
        mode: u8,
        slice: [u32; 4],
        w: u32,
        h: u32,
        tint: [u8; 4],
    ) -> Option<&Pixmap> {
        let key = (path.to_string(), mode, slice, w, h, tint);
        if !self.fills.contains_key(&key) {
            let built = self.build_fill(path, mode, slice, w, h, tint);
            self.fills.insert(key.clone(), built);
        }
        self.fills.get(&key).and_then(|o| o.as_ref())
    }

    fn build_fill(
        &mut self,
        path: &str,
        mode: u8,
        slice: [u32; 4],
        w: u32,
        h: u32,
        tint: [u8; 4],
    ) -> Option<Pixmap> {
        if w == 0 || h == 0 {
            return None;
        }
        let mut pm = Pixmap::new(w, h)?;
        if path.to_ascii_lowercase().ends_with(".svg") {
            // SVG：按目标尺寸栅格化（resvg 输出预乘 RGBA），逐像素 tint/直通 + R/B 交换。
            let rgba = rasterize_svg(path, w, h)?;
            let px = pm.pixels_mut();
            for (i, p) in px.iter_mut().enumerate() {
                let b = i * 4;
                *p = compose(rgba[b], rgba[b + 1], rgba[b + 2], rgba[b + 3], tint, true);
            }
            return Some(pm);
        }
        // 位图：image 解码（未预乘）→ 按模式采样 → tint/预乘 + R/B 交换。
        let src = self.decode(path)?;
        let (sw, sh, data) = (src.w, src.h, &src.rgba);
        if sw == 0 || sh == 0 {
            return None;
        }
        let px = pm.pixels_mut();
        for dy in 0..h {
            for dx in 0..w {
                let Some((sx, sy)) = map_src(mode, slice, sw, sh, w, h, dx, dy) else {
                    continue; // 透明（Pixmap::new 已清零）
                };
                let si = ((sy * sw + sx) * 4) as usize;
                px[(dy * w + dx) as usize] =
                    compose(data[si], data[si + 1], data[si + 2], data[si + 3], tint, false);
            }
        }
        Some(pm)
    }
}

/// 目标像素 (dx,dy) → 源像素坐标；None=该处透明（仅 center 越界）。
#[allow(clippy::too_many_arguments)]
fn map_src(
    mode: u8,
    slice: [u32; 4],
    sw: u32,
    sh: u32,
    w: u32,
    h: u32,
    dx: u32,
    dy: u32,
) -> Option<(u32, u32)> {
    match mode {
        1 => {
            // nine_slice：slice = [上,右,下,左]
            let sx = nine_axis(dx, w, sw, slice[3], slice[1])?;
            let sy = nine_axis(dy, h, sh, slice[0], slice[2])?;
            Some((sx, sy))
        }
        2 => Some((dx % sw, dy % sh)), // tile
        3 => {
            // center：源居中，越界透明
            let offx = (w as i64 - sw as i64) / 2;
            let offy = (h as i64 - sh as i64) / 2;
            let sx = dx as i64 - offx;
            let sy = dy as i64 - offy;
            if sx < 0 || sx >= sw as i64 || sy < 0 || sy >= sh as i64 {
                return None;
            }
            Some((sx as u32, sy as u32))
        }
        _ => {
            // stretch：等比映射到源
            let sx = (dx as u64 * sw as u64 / w as u64).min(sw as u64 - 1) as u32;
            let sy = (dy as u64 * sh as u64 / h as u64).min(sh as u64 - 1) as u32;
            Some((sx, sy))
        }
    }
}

/// 九宫格单轴映射：起/末 `s0`/`s1` 像素 1:1，中段拉伸。
fn nine_axis(d: u32, dlen: u32, slen: u32, s0: u32, s1: u32) -> Option<u32> {
    // 切片过大时退化为整体拉伸，避免中段为负。
    if s0 + s1 >= slen || s0 + s1 >= dlen {
        return Some((d as u64 * slen as u64 / dlen as u64).min(slen as u64 - 1) as u32);
    }
    if d < s0 {
        Some(d) // 起始固定段
    } else if d >= dlen - s1 {
        Some(slen - (dlen - d)) // 末尾固定段（对齐到源末尾）
    } else {
        // 中段拉伸：dest [s0, dlen-s1) → src [s0, slen-s1)
        let dmid = d - s0;
        let dmid_len = dlen - s0 - s1;
        let smid_len = slen - s0 - s1;
        Some((s0 + (dmid as u64 * smid_len as u64 / dmid_len as u64) as u32).min(slen - 1))
    }
}
