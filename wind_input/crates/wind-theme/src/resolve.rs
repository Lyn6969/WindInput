//! 求值：typed `Theme` → `Resolved`（palette + RVNode 树 + resources + behavior）。
//!
//! 与 Go 版 `pkg/theme` 的 `ResolveV3`/`ResolveCandidateViews`/`resolveViewNode` 对齐，但按迁移决策精简：
//! - **不做 derive**（主题须显式给全语义色；见 docs/redesign/theme-migration-plan.md 决策 2）；
//! - **LightDark 解析期坍缩**：求值即 `select(is_dark)→单值`，RVNode 只存终值（决策 3）；
//! - **validate 降级为 warn**：未解析的 `${token}`/缺失 ref 记 `tracing::warn`，不 fail（决策 5）；
//!   配合 `Option` 兜底（缺默认=沿用基态/渲染器内置），外部坏主题不黑屏。

use crate::palette::{Rgba, parse_hex, resolve_palette};
use crate::rvnode::{RvEdges, RvGradient, RvImage, RvNode, RvViews};
use crate::schema::{Ld, Theme, ViewGradient, ViewImage, ViewNode, Views};
use std::collections::HashMap;
use std::path::Path;

const TRANSPARENT: Rgba = [0, 0, 0, 0];

/// 解析后行为配置（引擎基线 ⊕ 主题 behavior）。
#[derive(Clone, Debug)]
pub struct ResolvedBehavior {
    pub font_size: i32,
    pub always_show_pager: bool,
    pub show_page_number: bool,
    pub hide_pager: bool,
    pub vertical_max_width: i32,
}

impl Default for ResolvedBehavior {
    /// 引擎内置基线（与 Go defaultBehavior 一致，零回归）。
    fn default() -> Self {
        Self {
            font_size: 18,
            always_show_pager: false,
            show_page_number: true,
            hide_pager: false,
            vertical_max_width: 600,
        }
    }
}

/// v3 主题求值后的最终形态——渲染层唯一来源（T3 起 wind-ui 消费）。
#[derive(Clone, Debug, Default)]
pub struct Resolved {
    pub is_dark: bool,
    /// 全部已解析颜色 token（顶层语义 + selection/hover + toolbar_* 等功能前缀）。
    pub palette: HashMap<String, Rgba>,
    pub views: RvViews,
    pub behavior: ResolvedBehavior,
    /// 图片资源：名→绝对路径 / data: URI（相对路径已按 theme 目录解析，按 is_dark 选变体）。
    pub resources: HashMap<String, String>,
    /// 资产搜索目录（self + base 链）：用于视图节点里**字面** image ref（如 _base 的 chevron.svg，
    /// 不在 resources 注册表）解析为绝对路径——派生主题继承 base 的图标需到 base 目录查找。
    pub asset_dirs: Vec<std::path::PathBuf>,
}

/// 颜色 token 求值（与 Go resolveColorToken 对齐）：
/// 空/None→None（保留默认）；transparent→全透明；`${name}`→palette 查（缺失 warn）；hex→直解。
fn resolve_color(ld: Option<&Ld>, palette: &HashMap<String, Rgba>, is_dark: bool) -> Option<Rgba> {
    let s = ld?.select(is_dark)?.trim();
    if s.is_empty() {
        return None;
    }
    if s == "transparent" {
        return Some(TRANSPARENT);
    }
    if let Some(name) = s.strip_prefix("${").and_then(|x| x.strip_suffix('}')) {
        let c = palette.get(name).copied();
        if c.is_none() {
            tracing::warn!("主题颜色引用未解析: ${{{}}}（token 不在 colors 表）", name);
        }
        return c;
    }
    match parse_hex(s) {
        Some(c) => Some(c),
        None => {
            tracing::warn!("主题颜色字面值非法: {:?}", s);
            None
        }
    }
}

/// schema ViewImage → 渲染消费 RVImage（不解码位图）。
fn to_rv_image(im: &ViewImage, palette: &HashMap<String, Rgba>, is_dark: bool) -> RvImage {
    let slice_px = |d: Option<crate::schema::Dim>| d.map(|x| x.resolve(1.0, 0.0)).unwrap_or(0.0);
    RvImage {
        reference: im.reference.clone(),
        mode: im.mode.clone(),
        // 九宫切片为源图纹理像素，不缩放：scale=1。
        slice: [
            slice_px(im.slice.top),
            slice_px(im.slice.right),
            slice_px(im.slice.bottom),
            slice_px(im.slice.left),
        ],
        opacity: im.opacity.unwrap_or(1.0),
        z: im.z,
        anchor: im.anchor.clone(),
        offset_x: im.offset.x,
        offset_y: im.offset.y,
        w: im.size.w,
        h: im.size.h,
        tint: resolve_color(im.tint.as_ref(), palette, is_dark),
        disabled_tint: resolve_color(im.disabled_tint.as_ref(), palette, is_dark),
    }
}

/// schema ViewGradient → RVGradient（stop 颜色解析 + 按 pos 升序；无有效 stop 返回 None）。
fn resolve_gradient(
    g: &ViewGradient,
    palette: &HashMap<String, Rgba>,
    is_dark: bool,
) -> Option<RvGradient> {
    let mut stops: Vec<(Rgba, f32)> = g
        .stops
        .iter()
        .filter_map(|s| {
            resolve_color(Some(&s.color), palette, is_dark).map(|c| (c, s.pos.clamp(0.0, 1.0)))
        })
        .collect();
    if stops.is_empty() {
        return None;
    }
    stops.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    Some(RvGradient {
        kind: if g.kind.is_empty() {
            "linear".into()
        } else {
            g.kind.clone()
        },
        angle: g.angle,
        stops,
    })
}

/// 通用 ViewNode → RVNode（窗口无关）。几何直拷（符号 Dim）；颜色 = 默认 ⊕ token 覆盖。
fn resolve_view_node(
    n: &ViewNode,
    palette: &HashMap<String, Rgba>,
    is_dark: bool,
    def_bg: Option<Rgba>,
    def_border: Option<Rgba>,
    def_text: Option<Rgba>,
) -> RvNode {
    let rc = |ld: &Option<Ld>| resolve_color(ld.as_ref(), palette, is_dark);

    let mut out = RvNode {
        margin: RvEdges {
            top: n.margin.top,
            right: n.margin.right,
            bottom: n.margin.bottom,
            left: n.margin.left,
        },
        padding: RvEdges {
            top: n.padding.top,
            right: n.padding.right,
            bottom: n.padding.bottom,
            left: n.padding.left,
        },
        border_radius: n.border.radius,
        border_width: n.border.width,
        border_color: def_border,
        bg_color: def_bg,
        bg_shape: n.background.shape.clone(),
        font_size: n.font_size.unwrap_or(0) as f32,
        font_weight: n.font_weight.unwrap_or(0),
        font_family: n.font_family.clone(),
        text_color: def_text,
        line_spacing: n.line_spacing,
        col_gap: n.col_gap,
        title_gap: n.title_gap,
        prev_char: n.prev_char.clone().unwrap_or_default(),
        next_char: n.next_char.clone().unwrap_or_default(),
        ..Default::default()
    };

    if let Some(c) = rc(&n.background.color) {
        out.bg_color = Some(c);
    }
    if let Some(c) = rc(&n.border.color) {
        out.border_color = Some(c);
    }
    if let Some(c) = rc(&n.color) {
        out.text_color = Some(c);
    }
    if let Some(img) = &n.background.image {
        out.bg_image = Some(to_rv_image(img, palette, is_dark));
    }
    if let Some(g) = &n.background.gradient {
        // resolve_gradient 对空 stops 返回 None，无需外层判空。
        out.bg_gradient = resolve_gradient(g, palette, is_dark);
    }
    if !n.layers.is_empty() {
        out.layers = n
            .layers
            .iter()
            .map(|l| to_rv_image(l, palette, is_dark))
            .collect();
    }
    if let Some(img) = &n.prev_image {
        out.prev_image = Some(to_rv_image(img, palette, is_dark));
    }
    if let Some(img) = &n.next_image {
        out.next_image = Some(to_rv_image(img, palette, is_dark));
    }
    if let Some(sh) = &n.shadow {
        out.shadow_offset_x = sh.offset_x;
        out.shadow_offset_y = sh.offset_y;
        out.shadow_blur = sh.blur;
        out.shadow_spread = sh.spread;
        out.shadow_color = rc(&sh.color);
    }
    out
}

/// 状态 patch ViewNode → 递归 RVNode（与 Go resolveState 对齐）。
/// nil-gating：仅当显式给了 bg/图/渐变/层/text/border 色/border 宽/字重，或有 palette 默认色，
/// 才算「有覆盖」返回 Some；**不看几何**（状态改几何会致候选框跳动，state_geometry unsupported）。
fn resolve_state(
    node: Option<&ViewNode>,
    palette: &HashMap<String, Rgba>,
    is_dark: bool,
    def_bg: Option<Rgba>,
    def_text: Option<Rgba>,
) -> Option<Box<RvNode>> {
    let rc = |ld: &Option<Ld>| resolve_color(ld.as_ref(), palette, is_dark);
    let mut has = def_bg.is_some() || def_text.is_some();
    if let Some(n) = node {
        let gradient_has = n
            .background
            .gradient
            .as_ref()
            .is_some_and(|g| !g.stops.is_empty());
        if rc(&n.background.color).is_some()
            || n.background.image.is_some()
            || gradient_has
            || !n.layers.is_empty()
            || rc(&n.color).is_some()
            || rc(&n.border.color).is_some()
            || n.border.width.is_some()
            || n.font_weight.is_some()
        {
            has = true;
        }
    }
    if !has {
        return None;
    }
    let default_node;
    let n = match node {
        Some(n) => n,
        None => {
            default_node = ViewNode::default();
            &default_node
        }
    };
    Some(Box::new(resolve_view_node(
        n, palette, is_dark, def_bg, None, def_text,
    )))
}

/// 解析图片路径：data: URI / 绝对路径原样；相对路径拼到 theme 目录。
fn resolve_image_path(p: &str, base_dir: &Path) -> String {
    if p.starts_with("data:") || Path::new(p).is_absolute() {
        return p.to_string();
    }
    base_dir.join(p).to_string_lossy().into_owned()
}

/// 便捷：按名加载主题（base 深合并 + 类型化）并求值为 `Resolved`。
pub fn load_resolved(themes_dir: &Path, name: &str, is_dark: bool) -> anyhow::Result<Resolved> {
    load_resolved_dirs(&[themes_dir.to_path_buf()], name, is_dark)
}

/// 多目录版：在 dirs 中定位主题并求值（base 跨目录、资产按 base 链目录查找）。
pub fn load_resolved_dirs(
    dirs: &[std::path::PathBuf],
    name: &str,
    is_dark: bool,
) -> anyhow::Result<Resolved> {
    let t = crate::theme::load_typed_dirs(dirs, name)?;
    let chain = crate::theme::theme_chain_dirs(dirs, name);
    if chain.is_empty() {
        anyhow::bail!("theme '{}' not found", name);
    }
    Ok(resolve(&t, is_dark, &chain))
}

/// 求值入口：typed `Theme` + is_dark + 资产目录链（self 在前，base 在后）→ `Resolved`。
/// `asset_dirs[0]` = self 主题目录（resources 相对路径基准）；全部用于字面 image ref 查找。
pub fn resolve(theme: &Theme, is_dark: bool, asset_dirs: &[std::path::PathBuf]) -> Resolved {
    let self_dir = asset_dirs
        .first()
        .cloned()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let theme_dir = self_dir.as_path();
    // 1. palette（不 derive；resolve_palette 已忽略 derive 块）。
    let palette = resolve_palette(theme.colors.as_ref(), is_dark);
    // 2. views（无 views 块 → 全默认 RvViews，仅 palette 取色，零回归）。
    let views = match &theme.views {
        Some(v) => resolve_views(v, &palette, is_dark),
        None => RvViews::default(),
    };
    // 3. behavior（基线 ⊕ 主题）。
    let behavior = merge_behavior(theme);
    // 4. resources（按 is_dark 选变体 + 相对路径解析）。
    let resources = theme
        .resources
        .iter()
        .filter_map(|(name, ref_ld)| {
            ref_ld
                .select(is_dark)
                .map(|p| (name.clone(), resolve_image_path(p, theme_dir)))
        })
        .collect();

    Resolved {
        is_dark,
        palette,
        views,
        behavior,
        resources,
        asset_dirs: asset_dirs.to_vec(),
    }
}

fn merge_behavior(theme: &Theme) -> ResolvedBehavior {
    let mut b = ResolvedBehavior::default();
    if let Some(ov) = &theme.behavior {
        if let Some(v) = ov.font_size {
            b.font_size = v;
        }
        if let Some(v) = ov.always_show_pager {
            b.always_show_pager = v;
        }
        if let Some(v) = ov.show_page_number {
            b.show_page_number = v;
        }
        if let Some(v) = ov.hide_pager {
            b.hide_pager = v;
        }
        if let Some(v) = ov.vertical_max_width {
            b.vertical_max_width = v;
        }
    }
    b
}

/// 候选窗各节点 + 其它窗口 + 列表级几何求值（与 Go ResolveCandidateViews + other_views 对齐）。
fn resolve_views(v: &Views, palette: &HashMap<String, Rgba>, is_dark: bool) -> RvViews {
    let tk = |name: &str| palette.get(name).copied();
    let build = |n: &ViewNode, bg: Option<Rgba>, border: Option<Rgba>, text: Option<Rgba>| {
        resolve_view_node(n, palette, is_dark, bg, border, text)
    };

    let mut rv = RvViews {
        window: build(&v.window, tk("bg"), tk("border"), None),
        preedit_bar: build(&v.preedit_bar, tk("surface"), None, tk("text_dim")),
        candidate_list: build(&v.candidate_list, None, None, None),
        item: build(&v.item, None, None, None),
        index: build(&v.index, tk("accent"), None, tk("on_accent")),
        text: build(&v.text, None, None, tk("text")),
        comment: build(&v.comment, None, None, tk("text_hint")),
        accent_bar: build(&v.accent_bar, tk("accent"), None, None),
        footer_bar: build(&v.footer_bar, None, None, None),
        mode_label: build(&v.mode_label, None, None, tk("text_hint")),
        shadow_color: tk("shadow"),
        ..Default::default()
    };

    // item/text/index/comment 状态 patch。
    rv.item.selected = resolve_state(
        v.item.selected.as_deref(),
        palette,
        is_dark,
        tk("selection"),
        tk("selection_text"),
    );
    rv.item.hover = resolve_state(v.item.hover.as_deref(), palette, is_dark, tk("hover"), None);
    rv.item.disabled = resolve_state(v.item.disabled.as_deref(), palette, is_dark, None, None);
    rv.text.selected = resolve_state(
        v.text.selected.as_deref(),
        palette,
        is_dark,
        None,
        tk("selection_text"),
    );
    rv.text.hover = resolve_state(v.text.hover.as_deref(), palette, is_dark, None, None);
    rv.index.selected = resolve_state(v.index.selected.as_deref(), palette, is_dark, None, None);
    rv.index.hover = resolve_state(v.index.hover.as_deref(), palette, is_dark, None, None);
    rv.comment.selected =
        resolve_state(v.comment.selected.as_deref(), palette, is_dark, None, None);
    rv.comment.hover = resolve_state(v.comment.hover.as_deref(), palette, is_dark, None, None);

    // 列表级几何（candidate_list 节点）。
    rv.item_spacing = v.candidate_list.gap;
    rv.window_gap = v.candidate_list.band_gap;
    rv.row_gap = v.candidate_list.row_gap;
    // window 投影 → 顶层 shadow 字段。
    if let Some(sh) = &v.window.shadow {
        rv.shadow_offset_x = sh.offset_x;
        rv.shadow_offset_y = sh.offset_y;
        rv.shadow_blur = sh.blur;
        rv.shadow_spread = sh.spread;
        if let Some(c) = resolve_color(sh.color.as_ref(), palette, is_dark) {
            rv.shadow_color = Some(c);
        }
    }
    // accent_bar 启用 + 几何。
    rv.accent_bar_enabled = v.accent_bar.enabled.unwrap_or(false);
    rv.accent_bar_width = v.accent_bar.width;
    rv.accent_bar_offset = v.accent_bar.offset;
    if let Some(r) = v.accent_bar.height_ratio {
        rv.accent_bar_height_ratio = r;
    }

    // 其它窗口（status/tooltip/toast）：各注入自己 palette 默认色（T5 再补 toolbar/menu）。
    rv.status = v
        .status
        .as_ref()
        .map(|n| build(n, tk("status_bg"), None, tk("status_text")));
    rv.tooltip = v
        .tooltip
        .as_ref()
        .map(|n| build(n, tk("tooltip_bg"), None, tk("tooltip_text")));
    rv.toast = v
        .toast
        .as_ref()
        .map(|n| build(n, tk("toast_bg"), None, tk("toast_text")));
    // 菜单容器（menu.root）：背景图/层来源；颜色默认 menu_bg/menu_border/menu_text。
    rv.menu_root = v
        .menu
        .as_ref()
        .map(|m| build(&m.root, tk("menu_bg"), tk("menu_border"), tk("menu_text")));

    rv
}

impl Resolved {
    /// 便捷：window 背景色（测试/调用方常用）。
    pub fn window_bg(&self) -> Option<Rgba> {
        self.views.window.bg_color
    }
    /// 按名取 palette 色（供工具栏/菜单等扁平取色，与旧 ResolvedTheme.color 同义）。
    pub fn color(&self, name: &str, default: Rgba) -> Rgba {
        self.palette.get(name).copied().unwrap_or(default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Dim;

    fn themes_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/themes")
    }

    fn load(name: &str, is_dark: bool) -> Resolved {
        load_resolved_dirs(&[themes_dir()], name, is_dark).unwrap()
    }

    #[test]
    fn test_default_theme_resolve() {
        let r = load("default", false);
        // window bg = bg light = 白
        assert_eq!(r.window_bg(), Some([255, 255, 255, 255]));
        // default override：序号圆圈 = accent 底 + on_accent 白字
        assert_eq!(r.views.index.bg_color, Some([0x42, 0x85, 0xF4, 255]));
        assert_eq!(r.views.index.text_color, Some([255, 255, 255, 255]));
        // 行为：default 单页不显翻页区、显示页码
        assert!(!r.behavior.always_show_pager);
        assert!(r.behavior.show_page_number);
        assert_eq!(r.behavior.font_size, 18);
        // 暗色 bg 不同
        let d = load("default", true);
        assert_eq!(d.window_bg(), Some([0x2D, 0x2D, 0x2D, 255]));
    }

    #[test]
    fn test_jidian_classic_rvnode() {
        let r = load("jidian-classic", false);
        // window 九宫格背景图
        let bg = r.views.window.bg_image.as_ref().expect("window bg image");
        assert_eq!(bg.reference, "panel");
        assert_eq!(bg.mode, "nine_slice");
        assert_eq!(bg.slice, [8.0, 8.0, 8.0, 8.0]);
        // window 投影 blur 进顶层 shadow
        assert_eq!(r.views.shadow_blur, Some(Dim::Dp(8.0)));
        // layers 水印 z=1 + 透明度
        assert_eq!(r.views.window.layers.len(), 1);
        let mark = &r.views.window.layers[0];
        assert_eq!(mark.reference, "mark");
        assert_eq!(mark.z, 1);
        assert_eq!(mark.opacity, 0.9);
        assert_eq!(mark.offset_x, Some(Dim::Dp(-8.0)));
        // item 选中态 patch：背景图 + 字重（几何不进）
        let sel = r.views.item.selected.as_ref().expect("item.selected patch");
        assert_eq!(sel.font_weight, 600);
        assert!(sel.bg_image.is_some());
        // accent_bar 几何
        assert_eq!(r.views.accent_bar_width, Some(Dim::Dp(3.0)));
        // resources 解析为绝对路径（按 is_dark 选 light 变体 panel.png）
        let panel = r.resources.get("panel").expect("panel resource");
        assert!(panel.ends_with("panel.png"), "got {panel}");
        assert!(Path::new(panel).is_absolute(), "应为绝对路径: {panel}");
        // 暗色选 dark 变体
        let d = load("jidian-classic", true);
        assert!(
            d.resources
                .get("panel")
                .unwrap()
                .ends_with("panel-dark.png")
        );
        // 其它窗口也吃九宫格
        let status = r.views.status.as_ref().expect("status rvnode");
        assert!(status.bg_image.is_some());
        assert_eq!(status.layers.len(), 1);
    }

    #[test]
    fn test_read_meta_and_multidir_base() {
        let dir = themes_dir();
        let dirs = vec![dir.clone()];
        // 显示名 / order 取自主题自身 meta（不 base 合并）。
        let m = crate::theme::read_meta(&dirs, "jidian-classic").expect("jidian meta");
        assert_eq!(m.name, "极点经典(位图)");
        assert_eq!(m.order, 2);
        let dm = crate::theme::read_meta(&dirs, "default").expect("default meta");
        assert_eq!(dm.name, "默认主题");
        // 多目录加载：jidian `base: _base` 跨目录解析正常（resources/views 出来）。
        let r = load_resolved_dirs(&dirs, "jidian-classic", false).expect("resolve jidian");
        assert!(r.views.window.bg_image.is_some(), "应解析出 _base+jidian 合并后的 window 背景图");
        assert!(r.resources.contains_key("panel"));

        // 资产链：default 继承 _base，链中应含 _base 目录（footer chevron.svg 在 _base）。
        let d = load_resolved_dirs(&dirs, "default", false).expect("resolve default");
        assert!(
            d.asset_dirs.iter().any(|p| p.ends_with("_base")),
            "default 资产链应含 _base 目录"
        );
        // footer 翻页箭头继承自 _base（chevron SVG + tint）。
        let prev = d.views.footer_bar.prev_image.as_ref().expect("footer prev_image");
        assert_eq!(prev.reference, "chevron_prev.svg");
        assert!(prev.tint.is_some(), "chevron 应有 tint(${{accent}})");
    }

    #[test]
    fn test_state_gating_drops_geometry_only_patch() {
        // hover 只改 padding（几何）应被视为空 patch 丢弃；改色则保留。
        let palette = HashMap::new();
        let geo_only = ViewNode {
            padding: crate::schema::ViewEdges {
                top: Some(Dim::Dp(99.0)),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(
            resolve_state(Some(&geo_only), &palette, false, None, None).is_none(),
            "纯几何 patch 应丢弃"
        );
        let color_patch = ViewNode {
            color: Some(Ld::Scalar("#FF0000".into())),
            ..Default::default()
        };
        assert!(
            resolve_state(Some(&color_patch), &palette, false, None, None).is_some(),
            "改色 patch 应保留"
        );
    }
}
