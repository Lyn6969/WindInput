# 主题系统 Go→Rust 迁移计划

## 背景与定调

Go `pkg/theme`（7224 行 / 50 文件，已是 V3-D 重构态）→ Rust `wind-theme`（迁移前 493 行，仅候选窗扁平字段）。

**关键前提（已与用户确认）**：主题是**开放生态**——除内置 `_base`/`default`/`msime` 外，还有
编辑器（WindInputThemeEditor）生成、Hub 分发的富主题（如 `jidian-classic`：九宫格背景图 + layers
水印 + 状态背景图，铺到所有窗口）。因此渲染面必须覆盖 Go 的完整能力，不能按"3 个纯色主题"裁剪。

**架构调整（最大）**：当前 Rust 丢掉了 Go 的 `RVNode` 渲染消费树，改成扁平 `ResolvedTheme`，撑不住
富主题。要补回 `ViewNode 树 → resolve → RVNode 树 → wind-ui 绘制` 两层模型，但做得比 Go 干净。

## 已定设计决策

1. **capability.go 不进引擎**。编辑器发 yaml、经 setting 中转、不直连服务；能力声明是"编辑器显示哪些
   控件"的元数据，归编辑器/setting 侧。引擎职责是**真实渲染所有能力**。保留为独立静态文件（后置）。
2. **derive 引擎不做**。主题必须显式给全语义色（如现 `_base`）。"只填 primary 生成整套"的便利交给
   编辑器预览后生成完整 yaml 落盘。引擎遇 `derive:` 键忽略。
3. **LightDark 降为解析期坍缩**。加载时已知 is_dark，求值即 `select(is_dark)→单值`，RVNode 只存终值。
   去掉泛型类型在渲染层的存在；**泛化到图片 ref/tint**（resources 的 `{light,dark}` 变体）。
4. **Dimension 坍缩成单 enum** `Dim{Dp(f32),Px(f32),Pct(f32)}` + `resolve(scale,host)->f32`。保留百分比。
5. **validate 降级为 warn 不 fail**。外部主题不可信，坏 ref/typo 要暴露；但配合 `unwrap_or(default)`
   兜底，警告而非拒绝，避免坏主题黑屏。
6. **state_geometry 保留 unsupported 约束**。selected/hover patch 只合并色/图/边框/字体，不合并几何
   （padding/margin/font_size），防候选框跳动。
7. **base 深合并保持 Value 层**（现 Rust 已实现）——比 Go 的 typed deepMergeTheme 简单，先合并后 typed。

## 分阶段（每阶段可编译 + 测试通过 + 交叉编译）

### T1：类型化 ViewNode schema（wind-theme，无渲染）
**目标**：`schema.rs` 定义 `Dim`/`ColorRef`/`ResourceRef`/`ViewEdges`/`ViewBorder`/`ViewFill`/
`ViewImage`/`ViewGradient`/`ViewShadowSpec`/`ViewNode`/`Views`/`ToolbarViews`/`MenuViews`/`Meta`/
`Behavior`/`Resources`，serde_yaml derive。复用现有 Value 层深合并，再 `from_value` 成 typed `Theme`。
**成功标准**：`_base`/`default`/`msime`/`jidian-classic` 四个主题都能解析成 typed `Theme` 不报错。
**状态**：✅ 完成（`schema.rs` + `theme::load_typed`；7 单测全绿，含 jidian 富特性解析：九宫格/阴影 blur/layers 水印/item 选中态背景图+字重/accent_bar/其它窗口）

### T2：resolve → RVNode（wind-theme）
**目标**：`resolve.rs` 把 typed `Theme` 求值为 `Resolved{ palette, views: RvViews(RvNode 树), resources, behavior }`。
token 递归求值 + 环检测（扩 palette.rs）；light/dark 坍缩（颜色+ref+tint）；Dim 保持符号态。
derive 忽略；validate warn 级。**保留 `ResolvedTheme` 扁平 facade**（从 Resolved 投影），让 wind-ui
迁移期零改动继续编译。
**成功标准**：jidian-classic 解析出含 BgImage/Layers/状态 patch 的 RvNode；旧 ResolvedTheme 测试仍绿。
**状态**：✅ 完成（`rvnode.rs` RVNode 树 + `resolve.rs` 求值；10 单测全绿、clippy 干净、windows-gnu 交叉编译 0 错误）。
要点：不 derive；LightDark 解析期坍缩（颜色+resources ref+tint）；validate 降级为 tracing::warn；
状态 patch nil-gating 不看几何；window 投影/accent_bar/列表几何归位；status/tooltip/toast 注入各自 palette 默认。
**未做（留后续阶段）**：toolbar/menu 复杂结构（T5 其它窗口）；ResolvedTheme 扁平 facade 暂保持独立旧实现
（wind-ui 仍消费旧 ResolvedTheme，T3 再切换到 Resolved/RvNode，届时删旧实现）。

### T3：候选窗消费 RVNode（wind-ui）
**目标**：candidate_window/viewbox 从 RvNode 取几何/色/字体（替换扁平字段读法）。
**成功标准**：候选窗外观与迁移前一致（default/msime 零回归）。
**状态**：✅ 完成（提交 `cefbaf9`）。Resolved 端到端贯通：coordinator.push_theme→load_resolved→SetTheme
(Box<Resolved>)→5 窗口；候选窗 build_tree 全读 RvViews/RvNode + 兜底色（与旧默认等值）；Resolved 加
Default/load_resolved、RvNode 加 bg_shape（序号圆形）。windows-gnu 交叉编译 Finished。
**未做**：旧 `resolved.rs`(ResolvedTheme/ThemeManager) 暂留（仅自身测试引用），待后续 T6/清理删除。

### T3.5：候选窗渲染完整度（拉近与 Go 外观，提交 `e16b277`）✅
诊断：Rust 与 Go 候选窗差异**在渲染层非主题数据**。本阶段补：
- **字号接主题**：base = `behavior.font_size`（默认 18，Go 同）× DPI，取代硬编码 24（之前大 33%）。
- **每节点字号偏移**：序号/注释/预编辑/翻页器按各 RvNode.font_size 渲染（序号/翻页更小）。
- **选中强调条**：主题启用（accent_bar.enabled，msime/jidian）时选中项左缘画竖条。
- 基建：`TextRenderer`(dwrite) 改为按调用传字号（size-keyed format 缓存，加 `*_sized`/`base_size`）；
  `View` 加 `font_size`/`left_bar`；`RvViews` 加 `accent_bar_enabled`。
**仍未做（已知差异）**：comment 内联（需 coordinator→UI 数据管线）、窗口阴影（需窗口缓冲扩边）、
真圆序号（现药丸近似）、竖排/多列布局、背景图/layers（=T4）。

### T4：图片管线 nine_slice + layers + SVG tint（wind-ui 解码缓存 + wind-theme RVImage）
**目标**：`bgimage.rs`/svg tint 落地；wind-ui 按 ref 解码缓存位图，九宫格/拉伸/平铺/center + z 层 +
tint mask（resvg 已是依赖）。footer chevron SVG tint 走此路径。
**成功标准**：jidian-classic 九宫格背景 + 右下角水印层在候选窗正确渲染。
**状态**：未开始

### T5：其它窗口走 RVNode（status/tooltip/menu/toast/toolbar）
**目标**：other_views 等价——各窗口注入自己 palette 语义色，复用 RvNode 解析 + 图片管线。
**成功标准**：jidian-classic 的 status/tooltip/menu/toast 也吃九宫格背景 + 水印。
**状态**：未开始

### T6：收尾（gradient + 百分比 offset + behavior.font_size + golden 测试）
**目标**：渐变栅格化；layers 百分比偏移；behavior 补 font_size；用 Rust 自有 3 主题建 golden 测试。
**成功标准**：全能力覆盖；golden 快照稳定。
**状态**：未开始

### 后置（独立于渲染管线）
- capability manifest 静态文件（供编辑器/setting 消费）。
