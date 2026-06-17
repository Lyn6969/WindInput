//! 主题图片资产解析（各窗口共享）：把 RvImage 的 ref 解析为绝对路径，转成 View 渲染用的
//! ViewImage / ViewLayer。背景图/层的实际绘制由 view.rs 的 paint（线程局部 image_cache）完成。

use crate::view::{ViewImage, ViewLayer};
use wind_theme::{Resolved, RvImage};

/// 把 image ref 解析为可读绝对路径：resources 注册表 → data:/绝对 → 在 asset_dirs（base 链目录）
/// 搜字面文件（如 _base 的 chevron.svg）。
pub fn asset_path(theme: &Resolved, reference: &str) -> Option<String> {
    if reference.is_empty() {
        return None;
    }
    if let Some(p) = theme.resources.get(reference) {
        return Some(p.clone());
    }
    if reference.starts_with("data:") || std::path::Path::new(reference).is_absolute() {
        return Some(reference.to_string());
    }
    for d in &theme.asset_dirs {
        let p = d.join(reference);
        if p.exists() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    Some(reference.to_string())
}

/// RvImage → 渲染用 ViewImage（reference 解析为绝对路径）。
pub fn rv_image(theme: &Resolved, im: Option<&RvImage>) -> Option<ViewImage> {
    let im = im?;
    let path = asset_path(theme, &im.reference)?;
    Some(ViewImage {
        path,
        mode: im.mode.clone(),
        slice: im.slice,
        opacity: im.opacity,
        tint: im.tint,
    })
}

/// RvImage[] → ViewLayer[]：解析路径 + 偏移分流（dp×scale / 百分比）+ 尺寸×scale。
pub fn rv_layers(theme: &Resolved, layers: &[RvImage], scale: f32) -> Vec<ViewLayer> {
    use wind_theme::schema::Dim;
    let split = |d: Option<Dim>| match d {
        Some(Dim::Dp(v)) => (v * scale, 0.0),
        Some(Dim::Px(v)) => (v, 0.0),
        Some(Dim::Pct(v)) => (0.0, v),
        None => (0.0, 0.0),
    };
    layers
        .iter()
        .filter_map(|im| {
            let path = asset_path(theme, &im.reference)?;
            let (off_x, off_x_pct) = split(im.offset_x);
            let (off_y, off_y_pct) = split(im.offset_y);
            Some(ViewLayer {
                path,
                z: im.z,
                anchor: im.anchor.clone(),
                off_x,
                off_y,
                off_x_pct,
                off_y_pct,
                w: if im.w > 0 { im.w as f32 * scale } else { 0.0 },
                h: if im.h > 0 { im.h as f32 * scale } else { 0.0 },
                opacity: im.opacity,
            })
        })
        .collect()
}
