<!-- Parent: ../../AGENTS.md -->
<!-- Updated: 2026-06-29 -->

# wind-ui

## Purpose
输入法所有浮层窗口的渲染与交互层：候选窗、常驻工具栏、级联弹出菜单、状态泡、Toast、悬停 Tooltip。
在独立 UI 线程跑 Win32 消息泵，经 mpsc 通道接收协调器（wind-coordinator）下发的 `UiCommand`、回送鼠标 `UiEvent`。
所有窗口走自带的 `view` 盒模型渲染到 BGRA 缓冲，再经 Layered Window 透明上屏；外观/颜色/几何全部消费 wind-theme 的 `Resolved`。

## Key Files
| File | Description |
|------|-------------|
| `src/lib.rs` | 模块导出 + 仅 re-export `UiManager`；顶部注释定义跨平台可测性三层（必读） |
| `src/manager.rs` | `UiManager` + UI 线程主循环；定义协调器↔UI 协议 `UiCommand`/`UiEvent` 及菜单类型 `MenuItemSpec`/`MenuKind`/`MenuCmd` |
| `src/window.rs` | `LayeredWindow`：Win32 `UpdateLayeredWindow` 封装 + `WindowMouse` 鼠标 trait + 非 Windows mock；wnd_proc 鼠标分发 |
| `src/view.rs` | **实际盒模型引擎**：measure→arrange→paint + 命中矩形提取；圆角/边框/阴影/渐变/九宫格背景图/z 层。各窗口共用 |
| `src/candidate_window.rs` | 候选窗：从候选构建 View 树、布局、绘制、鼠标命中/悬停防抖、翻页；含 `CandidateItem`/`CandidateWindowConfig` |
| `src/toolbar.rs` | 常驻工具栏窗口（中英/方案/标点/全半角），可拖动，命中回送 `ToolbarAction`；横/纵朝向见 `bar_layout` |
| `src/popup_menu.rs` | 级联弹出菜单（右键候选菜单 + 功能主菜单）；多级子菜单、勾选态、键盘导航经协调器 `MenuKey` 转发 |
| `src/status_tip.rs` | 状态提示气泡（切换中英/标点/全半角/方案时短暂或常驻显示） |
| `src/toast.rs` | 一次性通知 Toast（按位置/类型配色，定时自动隐藏） |
| `src/tooltip.rs` | 候选悬停反查气泡（显示编码/拼音） |
| `src/input_diag_hud.rs` | 输入诊断 HUD：四分区文本浮窗（输入态/窗口链/TSF 实例/HostRender），可拖动、双击复制、右键菜单（复制·显示分类·停止刷新·置顶）。定位三档见 `plan_position`——**已定位时走钳制而非原样沿用**，否则内容变长会被屏幕边缘吞掉；拖动不经此路径，故当次说了算、下次更新钳回。`format_diag_lines` 为纯函数（全部展示语义在此可单测） |
| `src/text/dwrite.rs` | DirectWrite 文本测量/渲染（预乘 alpha 回写 BGRA）+ 非 Windows mock（0.6×em 等宽近似） |
| `src/text/backend.rs` | `TextBackend` trait（measure/draw 抽象） |
| `src/theme_assets.rs` | 把 wind-theme 的 `RvImage` ref 解析为绝对路径并转 `ViewImage`/`ViewLayer` |
| `src/image_cache.rs` | 背景图解码/合成缓存（BGRA 预乘，线程局部，跨帧复用） |
| `src/dpi.rs` | 按目标点所在显示器实时取有效 DPI 缩放（多显示器不同缩放动态适配） |
| `src/sys.rs` | 平台基础类型/常量/光标函数 shim（Windows 复用 `windows` crate，其它平台 mock） |
| `src/debounce.rs` | 通用尾沿防抖（悬停高亮/Tooltip/状态泡合并抖动） |
| `src/screenshot.rs` | LayeredWindow BGRA 缓冲 → PNG 文件 / 剪贴板 |

> `src/viewbox/`、`src/renderer.rs`、`src/status.rs`、`src/text/freetype.rs` 当前是**仅含 doc 注释的占位 stub**（无实现），别 import、别往里加逻辑——真实盒模型在 `src/view.rs`。

## For AI Agents

### Working In This Directory
- **线程模型**：`UiManager::new()` 启 `ui-manager` 线程跑 `manager::ui_thread`；协调器持 `Sender<UiCommand>` 下发、`take_event_rx()`（仅一次）取 `Receiver<UiEvent>` 收鼠标事件。**所有窗口/缓存（`thread_local` 鼠标处理表、image cache）只在该 UI 线程存活**，禁止跨线程触碰窗口。
- **渲染管线统一**：每个窗口都 build `view::View` 树 → `measure`/`arrange` → `paint` 到 `LayeredWindow` 的 BGRA buffer → `update()`（`UpdateLayeredWindow`，预乘 alpha BGRA）。文本走 `text::dwrite`。新增窗口类型照此模式，别另起渲染路径。
- **跨平台三层（见 lib.rs，改前必读）**：① 纯 Rust 真实可测——`view` 盒模型布局/形状光栅化、`viewbox`、`debounce`、`image_cache`；② mock 近似——`text::dwrite` 非 Windows 返回等宽近似；③ 仅占位——Layered Window、DirectWrite 字形、剪贴板、消息泵在非 Windows 是空实现。动到 ② ③ 必须 Windows 实测。
- **命令循环要点**：`ui_thread` 大 `match` 消费 `UiCommand`；连续 `UpdateCandidates` 会被合并只渲染最新帧，`HideToolbar`/状态泡走防抖（消除 Alt+Tab、连按切换的闪烁）。加功能 = 加 `UiCommand` 变体 + 加 `match` 分支（缺分支编译过但静默无效）。
- **主题只消费不定义**：颜色/几何/背景图来自 wind-theme 的 `Resolved`/`RvNode`/`RvImage`/`schema::Dim`，经 `SetTheme(Box<Resolved>)` 分发到各窗口 `set_theme`。本 crate 不持有主题语义，别在此硬编码主题色/尺寸。
- **候选窗定位偏移的施加顺序**：主题 `window.position_offset` 在 `place_window`（跟随光标）内部按 **净锚点 → 方位决策 → 施加偏移 → 边界钳制** 的顺序处理。三条不可换位：① 偏移**不能**预先加进锚点——`below_ok`/`above_ok` 拿锚点跟工作区边界比，含偏移会让 `off_y` 越大越容易判成两边都放不下，本该上翻的场景落回下方再被钳到屏幕底、压住光标（真机反馈的「下方正常、上方遮盖」就是它）；② 偏移必须在钳制**之前**施加，否则会把窗口推出工作区且兜不回来；③ 上方用**减号**（`apply_offset_y`），正值恒为「远离光标」。三个调用点（Windows show/macOS render_frame/Windows render_frame）全接；`fixed_pos` 与 `drag_pin` 分支**不叠加**，那是用户显式意图。纯计算抽在 `caret_anchors`/`apply_offset_y`，因为 `place_window` 余下是 `#[cfg(windows)]` 的屏幕钳制、单测覆盖不到。
- **工具栏朝向是 config 不是主题**：`ui.toolbar.vertical` 走 `SetToolbarVertical`（同 `SetToolbarAutoHide` 的下发链）。**纵向恒为横向的转置**——条厚取主题 `[toolbar] height`、格长取 `button_width`，故同一套主题几何在两个朝向下都成立，别为纵向另引一套尺寸（`bar_layout` 的单测就是挡这个的）。绘制一律基于 `bar_layout` 给出的格矩形，横竖只在三处分叉：拖动柄点阵（2×3↔3×2）、分隔线画向、高亮块两个方向的内缩比例。⚠️ `set_vertical` 的重绘必须受 `visible` 门控——`repaint`→`render` 末尾无条件 `show`，对隐藏中的工具栏调用会绕过 `toolbar_gate` 的显示迟滞（`SetTheme` 分支同此约束）。编辑器侧有同源实现 `preview/otherWindows.ts::toolbarLayout` 与 `engineParity` 测试，改这里要同步那边。
- **菜单定位走 `MenuAnchor` + `MenuPlacement`，别加布尔**：锚点是矩形（`x/y/right/bottom`）加三态展开方向——`Below`（光标处右键）、`Above`（横条工具栏，上方装不下则翻到底边下）、`Side`（纵条工具栏：右侧装不下则左侧，纵向按**底边**对齐锚点底边、上方装不下才改顶边对齐）。**哪些边参与定位由 `placement` 决定**，所以构造一律走 `MenuAnchor::at_point`/`above_rect`/`beside_rect`，别手填字段——漏填的边会是 0 而非报错。纯计算在 `popup_menu::place_menu`（有单测），`show` 余下部分非 Windows 覆盖不到。要加第四种方向就加枚举分支，别再退回布尔。
- **一个装饰两处装配 = 迟早不同步**：模式徽标有「横排内嵌预编辑栏」与「竖排独立 chip」两条通路，已抽 `decorate_mode_chip` 共用；候选列表/翻页栏的盒装饰抽 `decorate_box`。往候选窗加这类外观时先找现成闭包，别再复制一份。
- **边框圆角别用 `eff_border` 的第三个返回值**：它在节点未配 `border.radius` 时兜底 `0.0`，直接用会把上游按 `item_radius` 设好的圆角抹平。只取它的色与宽，圆角走节点自身的 `border_radius`（`candidate_window.rs` 内有两处注释标记这个坑）。
- **浮层不抢焦点（不变量）**：wnd_proc 对 `WM_MOUSEACTIVATE`→`MA_NOACTIVATE`、`WM_NCHITTEST`→`HTCLIENT`，点击候选/工具栏不激活窗口、目标应用保持前台；菜单窗无焦点，键盘由协调器 `MenuKey(VK)` 转发。改窗口样式/消息处理时勿破坏这条。

### Testing Requirements
- 传递依赖 `windows` crate（仅 `cfg(windows)` 启用），非 Windows 平台以 mock 编译。`cargo test -p wind-ui` 在 host 能跑，但**只有纯 Rust 部分（`view` 盒模型/形状、`image_cache`、`debounce`）是真实验证**；文本测量是 mock 近似，Layered Window/DirectWrite/剪贴板/消息泵是空实现，其测试仅校 mock 的 API 契约。
- 真实窗口透明渲染、鼠标命中、DirectWrite 字形、截图/剪贴板等必须 **Windows 设备或 CI 实测**，host 单测覆盖不到。

## Dependencies

### Internal
- `wind-theme` — 唯一活跃消费的本仓 crate：`Resolved`/`RvNode`/`RvImage`/`RvGradient`/`schema::Dim`，提供窗口外观。
- `wind-candidate`、`wind-bridge` — Cargo.toml 已声明，当前源码未直接引用（预留/待接线）。

### External
- `tiny-skia` — 纯 Rust 2D 光栅化（盒模型形状/渐变/背景图填充）。
- `windows` / `windows-core`（仅 Windows）— Win32 窗口、GDI、DirectWrite、剪贴板、Shell。
- `resvg` / `image` — SVG 栅格化与位图解码（主题图标/背景）。
- `tracing`、`anyhow`、`thiserror`、`tokio`。

## 全局约束
- 改完跑 `cargo fmt`。
- 日志 INFO 级不得含用户输入/候选/词库内容——候选数量/坐标可记，候选文本仅 `debug!`（参见 `manager.rs` 现有用法）。

<!-- MANUAL: 此行以下为人工补充区，重新生成时保留 -->
