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
- 窗口样式追加 `WS_EX_NOACTIVATE`，置顶；初始屏角默认右下（避让全屏检测 `is_foreground_fullscreen`）。
- **可拖动改位置**：鼠标按住拖动即可移动。非激活窗口拖动不会抢走应用 X 的焦点（点击不激活）。拖动经手动 `WM_LBUTTONDOWN` + 移动实现（或 `WM_NCHITTEST` 返回 `HTCAPTION`），需与「复制」区分——见下。
- **不自动隐藏**（区别于 StatusTip 的 ~1s 自隐）；由菜单开关控制显隐。
- 显示内容（多行）：
  - `进程名 (pid)`
  - `禁用态: 是 / 否`
  - `原因: compartment / 密码 / 数字密码 / 无`
  - `InputScope: 0x…（解码位名）`
- 交互：**单击/拖动 = 移动窗口**；**双击 = 复制当前诊断到剪贴板**（满足「用户可上报」，避免与拖动冲突）。
- 每次 `last_input_diag` 更新且 HUD 可见 → 重绘，保证实时。

### D. 抑制策略（P0#1 接线）

> 定性：密码框强制英文**是用户功能，不是诊断功能**。最终归宿是**设置程序**（`wind-setting` 独立仓）里的一个用户可见选项。本次先把引擎侧逻辑接通，并临时挂在「高级」菜单开关上用于测试（见 E 节），后续再迁移到设置界面。

- `handle_focus_gained` 与 compartment 上报处：解码 `input_scope_mask`，命中 IS_PASSWORD / IS_NUMERIC_PASSWORD → 置瞬态 `password_suppress` 标志。
- 效果 = **强制英文、图标不变**（对齐 Go `applyPasswordFieldPolicyNoLock`，参考 Go `handle_lifecycle.go` 约 440 行 IS_PASSWORD=31 / IS_NUMERIC_PASSWORD=63 判定）。实现为：`password_suppress` 为真时，键处理走英文直通，不改 `state.chinese_mode` 的持久值与工具栏图标。
- 离开密码框（下次焦点 mask 清零，`reason=None`）→ 清 `password_suppress`，恢复原中英态。
- 范围澄清：`CompartmentDisabled` 情形 DLL 已放行所有键、引擎收不到，服务端策略仅用于让 HUD 标注「已抑制」；真正「强制英文」生效的是 **InputScope=密码但 compartment 未禁用** 的场景。

### E. 开关（右键菜单「高级」）

不新增 config flag、不新增全局热键。在右键菜单「高级」子菜单（`handle_menu.rs:297 advanced_children`）新增两个可勾选项：

| 菜单项 | MenuCmd | 默认 | 勾选态 | 性质 |
|---|---|---|---|---|
| 输入诊断 HUD | `MenuCmd::ToggleInputDiagnostics` | 关 | HUD 是否可见 | 诊断，长期保留 |
| 密码框强制英文 | `MenuCmd::TogglePasswordSuppress` | 开 | 抑制策略是否启用 | **临时测试入口**，后续迁至设置程序 |

- `run_menu_cmd`（`handle_menu.rs:32`）新增两分支：前者切换 HUD 显隐并触发重绘；后者切换抑制启用标志（关闭时 `password_suppress` 逻辑短路）。
- 两个开关的启用态**存于 coordinator 运行时（会话级，不持久化）**，重启回默认。
- 「密码框强制英文」是用户功能（见 D 节定性），这里的「高级」菜单开关仅为本阶段测试便利；正式版应移到设置程序作为用户可见选项，届时可从「高级」菜单撤下。

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

## 已定决策（本轮评审）

1. 「密码框强制英文」是**用户功能**，非诊断；本次先接通逻辑 + 临时挂「高级」菜单测试，正式版迁至设置程序。
2. HUD **可鼠标拖动**改位置，初始默认右下角；不做屏角配置项。
3. 两个「高级」开关**不持久化**，会话级，重启回默认。

## 未决/评审点

1. 后续把「密码框强制英文」迁到设置程序时的选项文案与位置（属 `wind-setting` 仓，另行处理）。

---

# 增强轮：窗口链 / TSF 实例 / HostRender 运行态

动机：per-app 配置按**进程名**匹配，但 Win10 任务栏搜索这类场景里，一个进程（explorer）承载了
多种完全不同的输入上下文，进程名不足以描述"当前到底在哪儿输入"。要判断 per-app 规则该不该
下沉到窗口级，先得能看见窗口级的事实。

## 采集内容

| 分区 | 字段 |
|---|---|
| 窗口 | 焦点 HWND + **来源标记** + 类名；顶层 HWND（`GA_ROOT`）+ 类名 + band；前台 HWND + 类名 + 进程 |
| TSF | DocMgr 指针、Context 指针、`focus_session_id`、本次是否换了 DocMgr |
| HostRender | 白名单命中、是否活跃目标、band 窗口实际 band |

不采集**窗口标题**：可能含文档名/网页标题，而类名已足够做判据。

## 三个关键设计

**1. 焦点句柄必须带来源标记（`WND_SRC_*`）。** 三条通路 `ITfContextView::GetWnd()` / 
`GetGUIThreadInfo().hwndFocus` / `GetForegroundWindow()` 给出的不是同一件东西——最后一条甚至
可能不属于本进程。这与 `CARET_SRC_*` 给 caret 坐标分域是同一个教训（那次让 Win32 光标冒充
TSF 插入点，Word 非正文行错位 814px）。而"前台窗口属于别的进程"恰恰是 Win10 搜索框场景的
关键信号，HUD 对此单出一行警告。

**2. 独立命令 + 采集开关，不并入 `focus_gained`。** 后者是宿主 UI 线程上的**同步** IPC 往返
（`focusIpcT0` 计时的那段），首字延迟挂在它身上；采集要查三次类名 + band，塞进去等于给每次
焦点切换加固定开销。故 `CMD_DIAG_SNAPSHOT`（异步）+ `CONFIG_KEY_DIAG_SNAPSHOT` 门控，
关闭时 DLL 一次 Win32 调用都不做。开关随 HUD 显隐推送，**握手时也推**——DLL 每次重连都从
默认值（关）起步，只在切换时推会让重连后的宿主永远不采集，而 SearchHost 这类 transient 宿主
恰恰最常重连。

**3. host-render 三项由服务端现算，且按上报包里的 pid 直查。** 不走 `ActiveCompat` 全局焦点槽
——开始菜单弹出会连带激活兄弟进程污染该槽（`host-render-windows-port.md` §11.2），HUD 若沿用
就会显示"另一个进程的 host 状态"，排查时反被带偏。

## 「没数据」与「数据是 0」

`WindowDiagView::received` 单独存在，就是为了让这两者可分辨。采集开关刚推给 DLL 时还没有任何
快照，此时若照常渲染一排 0，用户会把"尚未采集"读成"band 确实是 0"并据此下结论。未采集时 HUD
只出一行 `(未采集：切换一次输入焦点)`，不渲染任何占位值。

同理，空句柄渲染成 `-` 而非 `0x0`，类名缺失渲染成 `?`。

## 布局契约

`DiagSnapshotHeader`（64 字节）两侧**都是手写序列化**，无编译期约束。仅有的两道闸是
C++ 的 `static_assert(sizeof == 64)` 与 Rust 的 `diag_snapshot_head_layout_is_frozen`
（钉死总长 + 关键字段偏移）。加字段优先吃 `reserved`，别再动偏移量。

变长类名区被截断时只让对应类名退化为空串，不否掉整包——诊断数据缺一格也要能显示其余部分。

## 交互轮：定位自适应 + 右键菜单

分区变多后 HUD 显著变高，随之暴露两个问题。

### 定位三档（`plan_position`）

| 档 | 条件 | 行为 |
|---|---|---|
| `InitialCorner` | 首次显示 | 落屏幕右下角 |
| `ClampCurrent` | 已定位且在屏内 | **以当前实际位置为基准钳回工作区** |
| `ResetCorner` | 已定位但被拖出屏外 | 复位右下角（保证还能被抓回来） |

关键是第二档：此前是"在屏内就原样沿用当前坐标"，于是内容变长后左上角还在屏内、右下角已经
出界，被屏幕边缘吞掉一截。钳制保持左上角尽量贴近原位，只把溢出部分推回来——比整个复位到
右下角温和，用户挑的位置基本还在。

**拖动过程不经过这条路径**（`wnd_proc` 里直接 `SetWindowPos`），所以拖到哪当次就是哪；
下一次内容更新才钳回屏内。这正是「手动移动当次不管、下次更新自动恢复」。

### 右键菜单

走 `tooltip.rs` 同款链路：UI 侧只报告"用户在哪儿右键了"（`UiEvent::RequestInputDiagMenu`），
菜单树与动作分发都归协调器。菜单项：复制全部内容 / 显示分类（4 个分区勾选）/ 停止刷新 /
窗口置顶 / 关闭 HUD。勾选态直接读运行时状态，保证与实际行为不会脱节。

**冻结落在推送层而非渲染层**：数据照常进 `last_*_diag`，只是不往屏幕送，故解冻后立即有最新值
（若在 UI 侧丢弃，解冻后要等下一次焦点事件才恢复）。

⚠️ **冻结只挡数据变化引起的刷新，不挡用户自己的操作**。切分区/切置顶/切冻结一律走
`push_input_diag_hud(force=true)`——否则点了菜单屏幕毫无变化，而那与"菜单坏了"在用户眼里
完全一样。冻结本身也在 HUD 首行标注 `⏸ 已停止刷新`：冻结而不标注，用户会拿旧快照当现状读。

三处「沉默即误解」的兜底，与前一轮的 `received` 同源：分区全关时给一行 `(所有分区已隐藏：
右键 →「显示分类」)` 而不是空窗口；「未采集」提示只在依赖快照的分区至少开一个时出现（全关
的人是主动不看，再提醒就成了噪音）。

### 置顶开关：z 序必须分「切换」与「稳态」两档（`plan_zorder`）

真机复现「关掉置顶仍压着记事本」。根因是 `LayeredWindow::show()` 历来**无条件**
`SetWindowPos(HWND_TOPMOST)` 且不带 `SWP_NOZORDER`——于是每次刷新的顺序是
「插回置顶组 → 降到非置顶组顶部」，窗口永远浮在所有普通窗口之上，且每次数据更新都重来一遍。

修法：`show_z(x, y, ShowZOrder)` 三档。

| `applied` → `want` | 档 | 理由 |
|---|---|---|
| 任意 → 置顶 | `Topmost` | 每次重申（其间可能有别的窗口抢到更高 z 位） |
| 非「已非置顶」 → 非置顶 | `NoTopmost` | 真正移出置顶组，清 `WS_EX_TOPMOST` |
| 已非置顶 → 非置顶 | **`Keep`** | 放手不管，别的窗口被激活时自然盖过来 |

第三档是修复的核心。另外在刚移出置顶组时补一次「插到当前前台窗口之后」，让效果立刻可见
——否则窗口停在非置顶组顶部，要先去点一下记事本才看得出变化，而在那之前用户已经认定开关坏了。
⚠️ 前台窗口自身是置顶窗口时跳过这一步：`SetWindowPos` 的语义会让插到 topmost 之后的窗口
也变成 topmost，刚清掉的样式会被当场戴回来。

### 快照来源进程必须自报（真机暴露）

首份 Win10 真机数据里三个 HWND 全是 `0x101AA`，而首行进程写着 `explorer.exe(7172)`、
前台窗口却查出属于 `searchapp.exe(8704)`——一个 HWND 不可能同时属于两个进程。

根因：`last_input_diag`（随 focus_gained / compartment 变更走）与 `last_window_diag`
（随 `CMD_DIAG_SNAPSHOT` 走）是**两个独立的槽，可被不同进程的上报各自覆盖**。多进程宿主
下 HUD 并排显示两份数据，就隐含承诺了它们同源，读者会脑补出一个不存在的完整画面。

修法：`WindowDiagView` 带上 `pid` / `process_name`，窗口分区显示 `来源: X(pid)`；与首行
进程不同时改显 `⚠ 本节来自 X(pid)，非上方进程`。

连带修正一处误报：`foreground_is_other_process` 的比较基准此前取首行 pid，应取**快照
自己的 pid**——真机那份数据里快照与前台本属同一个 searchapp，却因输入态那半停在 explorer
而报出「前台窗口属于其他进程」。

> 同型隐患：任何把两个独立更新的槽并排展示的界面都有这个问题。并排即承诺同源。

### 两个开关的逃生口

非置顶会让 HUD 沉到宿主窗口之下、右键菜单点不到，于是**没法再打开置顶**；冻结着关掉 HUD、
下次打开仍是旧快照，看起来就是「HUD 坏了不刷新」。两者都是开关把自己的逃生口关上了。

复位点选在「重新打开 HUD」：那本就是用户表达「重来一次」的动作。分区显示**不**复位——它是
纯显示偏好，且全关时 HUD 会留一行可右键的提示，不封死。
