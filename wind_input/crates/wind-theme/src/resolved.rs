//! 已解析主题：把合并后的 YAML + 调色板解析为 UI 直接可用的具体色值与几何。
//!
//! 与 Go 版本 `wind_input/pkg/theme/resolved.go` 对齐（候选窗子集，逐步扩展到其它窗口）。

use crate::palette::{Rgba, color_token, resolve_palette};
use crate::theme;
use serde_yaml::Value;
use std::collections::HashMap;
use std::path::Path;

/// 四边边距/内边距（像素，未经 DPI 缩放）
#[derive(Clone, Copy, Debug, Default)]
pub struct Pad {
    pub t: f32,
    pub r: f32,
    pub b: f32,
    pub l: f32,
}

/// 已解析主题（候选窗 + 调色板；其它窗口经 palette 取色）
#[derive(Clone, Debug)]
pub struct ResolvedTheme {
    pub is_dark: bool,
    pub palette: HashMap<String, Rgba>,
    // 窗口
    pub win_bg: Rgba,
    pub win_border: Rgba,
    pub win_radius: f32,
    pub win_border_width: f32,
    pub win_pad: Pad,
    // 预编辑栏
    pub preedit_bg: Rgba,
    pub preedit_color: Rgba,
    pub preedit_pad: Pad,
    // 候选项
    pub item_pad: Pad,
    pub item_radius: f32,
    pub sel_bg: Rgba,
    pub sel_text: Rgba,
    pub hover_bg: Rgba,
    // 序号
    pub index_color: Rgba,
    pub index_font_offset: i32,
    pub index_circle: bool,
    pub index_circle_bg: Rgba,
    // 候选文本
    pub text_color: Rgba,
    pub text_margin_l: f32,
    // 注释
    pub comment_color: Rgba,
    pub comment_font_offset: i32,
    // 翻页器
    pub footer_font_offset: i32,
    // 强调条
    pub accent_bar: Rgba,
    // 行为
    pub always_show_pager: bool,
    pub show_page_number: bool,
}

impl Default for ResolvedTheme {
    /// 兜底主题（清风蓝，等价于此前硬编码外观），主题加载失败时使用。
    fn default() -> Self {
        Self {
            is_dark: false,
            palette: HashMap::new(),
            win_bg: [255, 255, 255, 245],
            win_border: [200, 200, 200, 200],
            win_radius: 8.0,
            win_border_width: 1.0,
            win_pad: Pad {
                t: 6.0,
                r: 8.0,
                b: 6.0,
                l: 8.0,
            },
            preedit_bg: [240, 240, 240, 255],
            preedit_color: [100, 100, 100, 255],
            preedit_pad: Pad {
                t: 3.0,
                r: 8.0,
                b: 3.0,
                l: 8.0,
            },
            item_pad: Pad {
                t: 7.0,
                r: 10.0,
                b: 7.0,
                l: 8.0,
            },
            item_radius: 4.0,
            sel_bg: [230, 240, 255, 255],
            sel_text: [30, 30, 30, 255],
            hover_bg: [238, 242, 247, 255],
            index_color: [66, 133, 244, 255],
            index_font_offset: -2,
            index_circle: false,
            index_circle_bg: [66, 133, 244, 255],
            text_color: [30, 30, 30, 255],
            text_margin_l: 4.0,
            comment_color: [150, 150, 150, 255],
            comment_font_offset: -4,
            footer_font_offset: -4,
            accent_bar: [66, 133, 244, 255],
            always_show_pager: false,
            show_page_number: true,
        }
    }
}

impl ResolvedTheme {
    /// 加载并解析主题：themes_dir/<name>/theme.yaml（含 base 继承）。
    pub fn load(themes_dir: &Path, name: &str, is_dark: bool) -> anyhow::Result<Self> {
        let merged = theme::load_merged(themes_dir, name, 0)?;
        let palette = resolve_palette(merged.get("colors"), is_dark);
        let views = merged.get("views");
        let beh = merged.get("behavior");
        let d = Self::default();

        let col = |path: &[&str], default: Rgba| -> Rgba {
            nav(views, path)
                .and_then(|v| color_token(v, &palette, is_dark))
                .unwrap_or(default)
        };
        let shape_circle = nav(views, &["index", "background", "shape"])
            .and_then(|v| v.as_str())
            .map(|s| s == "circle")
            .unwrap_or(d.index_circle);

        Ok(Self {
            is_dark,
            win_bg: col(&["window", "background", "color"], d.win_bg),
            win_border: col(&["window", "border", "color"], d.win_border),
            win_radius: num(views, &["window", "border", "radius"], d.win_radius),
            win_border_width: num(views, &["window", "border", "width"], d.win_border_width),
            win_pad: pad(views, &["window", "padding"], d.win_pad),
            preedit_bg: col(&["preedit_bar", "background", "color"], d.preedit_bg),
            preedit_color: col(&["preedit_bar", "color"], d.preedit_color),
            preedit_pad: pad(views, &["preedit_bar", "padding"], d.preedit_pad),
            item_pad: pad(views, &["item", "padding"], d.item_pad),
            item_radius: num(views, &["item", "border", "radius"], d.item_radius),
            sel_bg: col(&["item", "selected", "background", "color"], d.sel_bg),
            sel_text: col(&["item", "selected", "color"], d.sel_text),
            hover_bg: col(&["item", "hover", "background", "color"], d.hover_bg),
            index_color: col(&["index", "color"], d.index_color),
            index_font_offset: num(views, &["index", "font_size"], d.index_font_offset as f32)
                as i32,
            index_circle: shape_circle,
            index_circle_bg: col(&["index", "background", "color"], d.index_circle_bg),
            text_color: col(&["text", "color"], d.text_color),
            text_margin_l: num(views, &["text", "margin", "left"], d.text_margin_l),
            comment_color: col(&["comment", "color"], d.comment_color),
            comment_font_offset: num(
                views,
                &["comment", "font_size"],
                d.comment_font_offset as f32,
            ) as i32,
            footer_font_offset: num(
                views,
                &["footer_bar", "font_size"],
                d.footer_font_offset as f32,
            ) as i32,
            accent_bar: col(&["accent_bar", "background", "color"], d.accent_bar),
            always_show_pager: vbool(beh, "always_show_pager", d.always_show_pager),
            show_page_number: vbool(beh, "show_page_number", d.show_page_number),
            palette,
        })
    }

    /// 按调色板名取色（供工具栏/菜单/tooltip 等使用）。
    pub fn color(&self, name: &str, default: Rgba) -> Rgba {
        self.palette.get(name).copied().unwrap_or(default)
    }
}

/// 按路径导航嵌套 Value。
fn nav<'a>(root: Option<&'a Value>, path: &[&str]) -> Option<&'a Value> {
    let mut cur = root?;
    for key in path {
        cur = cur.get(*key)?;
    }
    Some(cur)
}

/// 取数值（支持 "8" / 8 / "1px"）。
fn num(views: Option<&Value>, path: &[&str], default: f32) -> f32 {
    match nav(views, path) {
        Some(Value::Number(n)) => n.as_f64().map(|f| f as f32).unwrap_or(default),
        Some(Value::String(s)) => s
            .trim()
            .trim_end_matches("px")
            .trim()
            .parse::<f32>()
            .unwrap_or(default),
        _ => default,
    }
}

/// 取四边内边距 {top,right,bottom,left}；显式给了节点但缺某边按 0。
fn pad(views: Option<&Value>, path: &[&str], default: Pad) -> Pad {
    let node = match nav(views, path) {
        Some(v) => v,
        None => return default,
    };
    let one = |k: &str| match node.get(k) {
        Some(Value::Number(n)) => n.as_f64().map(|f| f as f32).unwrap_or(0.0),
        _ => 0.0,
    };
    Pad {
        t: one("top"),
        r: one("right"),
        b: one("bottom"),
        l: one("left"),
    }
}

fn vbool(beh: Option<&Value>, key: &str, default: bool) -> bool {
    beh.and_then(|b| b.get(key))
        .and_then(|v| v.as_bool())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_default_theme() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build_debug/data/themes");
        if !dir.join("default/theme.yaml").exists() {
            return; // 无数据则跳过
        }
        let t = ResolvedTheme::load(&dir, "default", false).expect("load default");
        // default override：序号圆圈（accent 底 + on_accent 白字）
        assert!(t.index_circle, "default 主题序号应为圆圈");
        assert_eq!(t.index_color, [255, 255, 255, 255]); // on_accent 白
        assert_eq!(t.index_circle_bg, [0x42, 0x85, 0xF4, 255]); // accent 清风蓝
        assert_eq!(t.win_bg, [255, 255, 255, 255]); // bg light
        assert!(t.win_radius > 0.0);
        // accent_bar 取自 _base = accent
        assert_eq!(t.accent_bar, [0x42, 0x85, 0xF4, 255]);
        // 暗色：bg 应不同
        let dark = ResolvedTheme::load(&dir, "default", true).unwrap();
        assert_eq!(dark.win_bg, [0x2D, 0x2D, 0x2D, 255]);
    }
}
