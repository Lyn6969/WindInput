# 输入诊断 HUD + 密码框抑制 设计

日期：2026-07-11
状态：设计已定，待实现（本文档不含代码）

## 背景与目标

部分用户反馈「某些应用完全无法输入」。经代码定位，根因在 C++ TSF 层的禁用门闸：

- `wind_tsf/src/KeyEventSink.cpp` 三处 `if (_pTextService->IsKeyboardDisabled()) return S_OK;`（约 186 / 1095 / 1251 行）——该状态为真时 DLL **放行所有按键、完全不拦截**，表现即「打不了字」。
- 该状态 `_bKeyboardDisabled` 由 `GUID_COMPARTMENT_KEYBOARD_DISABLED` compartment sink 驱动（`TextService.cpp` 的 `_InitKeyboardDisabledCompartment`，GUID 见 `kGuidCompartmentKeyboardDisabled`）。Chromium 系密码框会置位，但**某些应用可能异常置位**，是主要可疑点。
- 该禁用状态**目前只活在 DLL 内部，从未上报给 Rust 服务端**，无法观测。
- 另一路信号 `input_scope_mask`（含 IS_PASSWORD=bit31 / IS_NUMERIC_PASSWORD=bit63）虽由 `focus_gained` 发到服务端（`wind-bridge/src/handler.rs:157` 的 `FocusData.input_scope_mask`），但 `coordinator.rs:3977 handle_focus_gained` **完全未读取**——即 Go 对齐项 P0#1「数据到位、消费缺失」。

本设计一次交付两件事：

1. **观测**：新增 DLL→服务端 禁用态上报链路 + 热键无关的**右键菜单可切换的实时 HUD 浮窗**，让用户/支持人员在问题应用现场看到进程名、禁用态、原因、InputScope。
2. **抑制**：`handle_focus_gained` 消费 `input_scope_mask`，密码框强制英文（对齐 Go `applyPasswordFieldPolicyNoLock`）。

### 焦点悖论与解法

出问题的是应用 X，但查看诊断会抢走 X 的焦点。解法：

- HUD 是**非激活置顶浮窗**（`WS_EX_NOACTIVATE` + topmost），显示时不抢焦点。
- HUD 是**持续开关**（一次开启后常显），并**跟随后续焦点切换实时刷新**——用户在「高级」菜单里开一次，再回到应用 X，HUD 即显示 X 的真实禁用态。

## 非目标（YAGNI）

- 不做滚动事件日志 / 一键导出历史（本次用实时 HUD，现场目视；HUD 内提供「复制当前诊断」即可）。
- 不新增调试用的全局热键，不新增 config 开关来启用 HUD——统一走右键菜单「高级」。
- 不复刻 Go 的 `perf`/pprof、备份、低内存治理等（属其它 P 级项）。
- 不改 macOS 路径（本设计聚焦 Windows TSF；`server_unix.rs` 的 `FocusData` 构造同步补字段即可编译，行为不涉及）。

## 架构

三段：数据链路（C++→Rust）→ 状态存储（coordinator）→ 两个消费者（HUD 渲染 + 抑制策略）。

### A. 数据链路（C++ → Rust）

上报时机（`wind_tsf`）：

1. `CTextService::OnSetFocus`（已上报 `focus_gained`，**扩展载荷**再带禁用态字段）。
2. `GUID_COMPARTMENT_KEYBOARD_DISABLED` compartment sink 触发时（**新增**一条独立上报，覆盖「不换焦点但 compartment 变更」，如网页内密码框获焦）。

上报载荷（新增/扩展）：

| 字段 | 含义 |
|---|---|
| `pid` | 焦点进程 pid（已有，经 client_token 高 32 位） |
| `disabled: bool` | `_bKeyboardDisabled` 当前值 |
| `reason: u8` | 0=None / 1=CompartmentDisabled / 2=InputScopePassword / 3=NumericPassword |
| `input_scope_mask: u64` | 已有字段 |

`reason` 判定（DLL 侧）：compartment 置位 → `CompartmentDisabled`；否则看 mask 的 IS_PASSWORD(31) / IS_NUMERIC_PASSWORD(63) 位；都无 → `None`。

协议落点：

- 扩展 `focus_gained` 编解码，新增 `disabled`/`reason` 两字段（`input_scope_mask` 已有）。
- 新增独立命令 `CMD_INPUT_STATE_REPORT`（`wind-ipc` 协议常量 + `wind-bridge` 解码），仅用于 compartment 变更时的最新态上报。

### B. 状态存储（coordinator）

- `Coordinator` 新增 `last_input_diag: Mutex<InputDiagState>`。
- `InputDiagState { pid, process_name, disabled, reason, input_scope_mask, updated_at }`。
- `handle_focus_gained` 与 `CMD_INPUT_STATE_REPORT` 处理均写入该状态；`process_name` 复用已有 `pid_names` 缓存 / `cached_proc_name`（`coordinator.rs:506/1115`），避免重复 `OpenProcess`。
- 写入后若 HUD 可见，触发一次 HUD 重绘推送。

### C. HUD 浮窗（wind-ui）

- 新增 `InputDiagHud`，仿 `StatusTip`（`wind-ui/src/status_tip.rs`）：`LayeredWindow` + `TextRenderer`（View 盒模型 + DirectWrite），深色半透明圆角底。
- 窗口样式追加 `WS_EX_NOACTIVATE`，置顶；固定屏角（默认右下，避让全屏检测 `is_foreground_fullscreen`）。
- **不自动隐藏**（区别于 StatusTip 的 ~1s 自隐）；由菜单开关控制显隐。
- 显示内容（多行）：
  - `进程名 (pid)`
  - `禁用态: 是 / 否`
  - `原因: compartment / 密码 / 数字密码 / 无`
  - `InputScope: 0x…（解码位名）`
  - 底部提示：`点击复制当前诊断`（写剪贴板，满足「用户可上报」）
- 每次 `last_input_diag` 更新且 HUD 可见 → 重绘，保证实时。

### D. 抑制策略（P0#1 接线）

- `handle_focus_gained` 与 compartment 上报处：解码 `input_scope_mask`，命中 IS_PASSWORD / IS_NUMERIC_PASSWORD → 置瞬态 `password_suppress` 标志。
- 效果 = **强制英文、图标不变**（对齐 Go `applyPasswordFieldPolicyNoLock`，参考 Go `handle_lifecycle.go` 约 440 行 IS_PASSWORD=31 / IS_NUMERIC_PASSWORD=63 判定）。实现为：`password_suppress` 为真时，键处理走英文直通，不改 `state.chinese_mode` 的持久值与工具栏图标。
- 离开密码框（下次焦点 mask 清零，`reason=None`）→ 清 `password_suppress`，恢复原中英态。
- 范围澄清：`CompartmentDisabled` 情形 DLL 已放行所有键、引擎收不到，服务端策略仅用于让 HUD 标注「已抑制」；真正「强制英文」生效的是 **InputScope=密码但 compartment 未禁用** 的场景。

### E. 开关（右键菜单「高级」）

不新增 config flag、不新增全局热键。在右键菜单「高级」子菜单（`handle_menu.rs:297 advanced_children`）新增两个可勾选项：

| 菜单项 | MenuCmd | 默认 | 勾选态 |
|---|---|---|---|
| 输入诊断 HUD | `MenuCmd::ToggleInputDiagnostics` | 关 | HUD 是否可见 |
| 密码框强制英文 | `MenuCmd::TogglePasswordSuppress` | 开 | 抑制策略是否启用 |

- `run_menu_cmd`（`handle_menu.rs:32`）新增两分支：前者切换 HUD 显隐并触发重绘；后者切换抑制启用标志（关闭时 `password_suppress` 逻辑短路，作为「误伤」安全阀）。
- 两个开关的启用态存于 coordinator 运行时（会话级；是否持久化到 config 由实现阶段决定，倾向不持久化——诊断/安全阀性质，重启回默认）。

> 决策备注：「密码框强制英文」开关是否要加，源于此前对抑制误伤的担忧。放进「高级」菜单而非 config，契合「开关走右键菜单」的约定，同时保留安全阀。若认为多余可在评审时去掉，抑制则恒开。

## 数据流

```
应用X TSF context
  │ OnSetFocus / compartment 变更
  ▼
wind_tsf: 判定 disabled + reason + mask
  │ focus_gained(扩展) 或 CMD_INPUT_STATE_REPORT
  ▼
wind-bridge 解码 → FocusData / InputStateReport
  ▼
coordinator: 写 last_input_diag ──┬─► HUD 可见? → 重绘 InputDiagHud
                                   └─► 命中密码位 & 抑制启用? → 置 password_suppress
```

## 涉及文件（预估）

- `wind_tsf/src/TextService.cpp`：compartment sink 回调新增上报；`OnSetFocus` 上报扩展 disabled/reason。
- `wind_tsf/src/IPCClient.cpp` + `BinaryProtocol.h`：focus_gained 载荷扩展 + `CMD_INPUT_STATE_REPORT`。
- `wind-ipc`：协议常量 `CMD_INPUT_STATE_REPORT` + 编解码。
- `wind-bridge/src/handler.rs` `server.rs` `server_unix.rs`：`FocusData` 扩展 + 新命令分发。
- `wind-coordinator/src/coordinator.rs`：`last_input_diag`、上报处理、`handle_focus_gained` 消费 mask、`password_suppress`。
- `wind-coordinator/src/handle_menu.rs`：两个「高级」菜单项 + `run_menu_cmd` 分支。
- `wind-ui`：新增 `InputDiagHud`（仿 `status_tip.rs`）+ `manager.rs` 的 `MenuCmd` 两个变体与 UI 指令。

## 测试

Rust 单测：

- mask 解码 → `reason` 判定（None / Password / NumericPassword）。
- `password_suppress` 置位/恢复：密码框 mask → 置位；mask 清零 → 复位。
- 抑制启用时 force_english 生效、菜单关闭抑制时不生效。
- 菜单开关切换 HUD 可见标志 + 抑制启用标志。

真机手测清单（C++ 时序，无法单测）：

- Chromium 密码框：HUD 显示 `原因: compartment`，键放行。
- 普通文本框标注 `原因: 无`，中文正常。
- 复现「无法输入」应用：HUD 现场显示禁用态与原因，判定「应用侧 vs 我方」。
- 网页内点密码框（不换窗）：compartment 上报即时刷新 HUD。
- 密码框强制英文生效 + 离开恢复原中英态；「高级」关闭抑制后不再强制。

## 未决/评审点

1. 「密码框强制英文」菜单开关是否保留（见 E 决策备注）。
2. HUD 屏角位置是否需可配（暂定固定右下）。
3. 两个开关是否持久化到 config（暂定不持久化）。
