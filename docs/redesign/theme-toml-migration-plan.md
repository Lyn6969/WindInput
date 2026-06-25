# 主题格式 YAML → TOML 迁移计划

> 真相源：本文件。配套分析见会话；编辑器 schema 见 WindInputThemeEditor `docs/superpowers/specs/2026-06-21-toml-theme-schema-design.md`。
> 约束：硬切换，不做 YAML 向后兼容（与项目「不做向后兼容」一致 + 设计文档明示「全新项目」）。

## 已确认决策
1. 颜色派生**固化**：引擎只读最终色，不重算（与现状一致，palette 本就不派生）。`[colors.derive]` 仅供编辑器，引擎忽略。✅
2. 引擎**补 `border.style`**（solid/dashed/dotted）。✅ schema 解析 + rvnode/resolve 数据通路已就绪；⚠️ **wind-ui 虚线/点线光栅绘制待补**（当前按 solid 渲染）。
3. 编辑器侧硬伤补全（**另一仓库 WindInputThemeEditor**，本计划不含）：prev_image/next_image、shadow.spread_offset、叶子 passthrough。待办。

## 状态：引擎迁移完成（分支 feat/theme-toml-format，未提交）
全部 6 阶段完成；wind-theme 22 测试全绿，workspace 编译通过，受影响 crate fmt 干净。
data/themes + testdata + build/build_debug 暂存均已转 TOML（删除 yaml）。
**遗留**：border.style 虚线光栅（wind-ui）+ 编辑器侧硬伤补全（另一仓库）+ Windows 实机视觉验证。

## 核心策略：写入形态 ≠ 内存形态
TOML 文件是**扁平/简写**的人写形态；引擎加一层 `toml::Value` 归一化，产出与**现有 typed `Theme`（嵌套）完全一致**的规范形态，再 `try_into::<Theme>()`。
→ `resolve.rs` / `rvnode.rs` 渲染器**零改动**；`schema` 结构基本不变（仅 ViewBorder 加 style、Dim/Ld 改吃 toml::Value）。

归一化映射（flat file → canonical nested）：
- 顶层视图表（window/item/…）→ 收进 `views.*`
- 节点 `radius` → `border.radius`；`shape` → `background.shape`
- 节点 `background = "${bg}"`（标量）→ `background = {color="${bg}"}`
- `margin`/`padding`/`slice` 标量/数组简写 → `{top,right,bottom,left}` 全表
- `shadow.offset = [x,y]` → `shadow.offset_x` / `offset_y`
- `toolbar.button.chinese|english` → `toolbar.button.mode.{chinese,english}`
- `toolbar.settings.icon|hole` 标量 → `{color=…}`
- 递归进 selected/hover/disabled、toolbar.grip/button/settings、menu.root/item/separator
- `colors`/`resources`/`behavior`/`meta` 原样

## 阶段

### Stage 1: schema.rs — Dim/Ld 转 toml + border.style + 归一化 transform
**Goal**: `toml::Value`（flat）→ typed `Theme` 的解析路径就绪。
**交付**: Dim/Ld 的 Deserialize 改用 toml::Value；ViewBorder 加 `style`；新增 `normalize_value(toml::Value)->toml::Value`。
**测试**: dim/ld/edges 简写、radius/shape 下沉、shadow offset、toolbar.button.chinese、fill 标量 → 各一例往返到 typed。
**Status**: Complete

### Stage 2: theme.rs — TOML 加载 + merge + 文件名 theme.toml
**Goal**: 加载/合并/校验/meta 全走 TOML。
**交付**: read_to_string `theme.toml`；`toml::from_str::<Value>`；`merge` on toml::Value；load_typed = merge→normalize→try_into。
**测试**: 现有 builtin themes 类型检查、jidian rich features（迁移 testdata 后）。
**Status**: Complete

### Stage 3: palette.rs — colors 改吃 toml::Value
**Goal**: 调色板解析迁到 toml::Value（as_str/as_table），逻辑不变。
**测试**: 现有 palette 单测改 toml。
**Status**: Complete

### Stage 4: 主题文件转换 yaml → toml
**Goal**: data/themes/{_base,default,msime,jidian-classic} + wind-theme/testdata/themes/* 转 TOML。
**交付**: 转换脚本或手转；验证渲染等价。
**Status**: Complete

### Stage 5: 调用方 — coordinator/engine
**Goal**: handle_mode/webdata 文件名 theme.toml；web_theme_preview 的 toml::Value→JSON；validate/import 走 TOML。
**Status**: Complete

### Stage 6: 收尾 — Cargo 依赖、fmt、全绿、Windows 交叉编译
**Status**: Complete
