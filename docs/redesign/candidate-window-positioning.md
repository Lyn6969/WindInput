# 候选窗坐标时序与定位设计

> 记录候选窗"在哪里显示"这件事背后的 Windows TSF 复杂性与防抖/防错位机制。
> Go 版本（`../WindInput/wind_input`）经长期实测稳定，Rust 版本据此移植。
> 涉及 DLL（`wind_tsf`）、协调器（`wind-coordinator`）、UI（`wind-ui`）三层。

## 1. 问题现象

候选窗应紧贴输入光标显示。但在某些应用（尤其终端类如 **tabby**、WebView、WPS、Excel）会出现：

- **错位**：输入完一个字/词上屏后，立即输入下一个，候选窗出现在偏离光标约"一个刚上屏内容宽度"的位置。
- **抖动**：候选窗出现时先在一个位置闪现、再跳到另一处；或输入过程中随宿主上报的光标微抖动而抖。

"不是所有应用都出现"是关键线索——根因是**宿主上报光标坐标的时序**，而非定位算法本身。

## 2. 根因：Windows TSF 的坐标采集时序

候选窗位置来自宿主光标矩形，经 `ITfContextView::GetTextExt` 获取（屏幕坐标）。问题在于：

1. **reflow 滞后**：新 composition 刚 `StartComposition` 时，宿主尚未完成文本重排（reflow）。此刻 `GetTextExt` 返回的是**reflow 前的旧坐标**（上一组合/上屏前的位置）。错位量 ≈ 刚上屏内容宽度。
2. **退化矩形**：reflow 未完成时部分宿主返回 `height==0` 的退化矩形，坐标完全不可靠。
3. **坐标微抖**：WPS 等在首次与后续 `GetTextExt` 间返回相差 1~2px 的光标高度/位置；微信等 WebView 的 `height` 在 `1`/`20` 间跳变，使 `rect.bottom` 相差达 20px（但 `rect.top` 稳定）。
4. **坐标系不一致**：个别控件 `GetRange` 让组合起点 anchor 随输入漂移；或返回 logical/physical 混用的越界坐标。

> Windows 输入法栈（TSF + CUAS + 各框架自绘）没有统一的"光标已稳定"信号，只能靠一组经验性 hack 逼近。以下机制都是为对抗上述时序而生。

## 3. DLL 层（wind_tsf）的应对

见 `wind_tsf/src/TextService.cpp`：

| 机制 | 位置 | 作用 |
|------|------|------|
| `_compositionJustStarted` | `StartComposition` 后置位 | 标记"刚启动、reflow 未完成"，推迟首次坐标发送 |
| 推迟 + `SendCaretPending` | `SendCaretPositionUpdate` | justStarted 期间不立即发坐标，先发"坐标待定"握手 |
| `OnLayoutChange` debounce | `OnLayoutChange` | reflow 完成的权威信号；burst 期间 debounce，等稳定后 flush |
| 50ms timer 兜底 | — | 应对完全不发 `OnLayoutChange` 的应用（某些 CUAS 路径） |
| `height==0` / 越界过滤 | `GetCaretPositionFromTSF` / `_CacheCaretPosition` | 退化矩形、`IsScreenPointOutsideForegroundWindow` 越界坐标不缓存、不上报 |
| `SendCaretUpdate(x,y,h,compStartX,compStartY)` | reflow 后 | 发送权威坐标 + 组合起点坐标 |

要点：**DLL 保证每个新组合"先发 CaretPending、reflow 后再发权威 CaretUpdate(height>0)"**。协调器据此实现"延迟首显"。

## 4. 协调器 + UI 层的三层机制（对齐 Go）

Go 版本用三层独立机制覆盖不同失效模式，Rust 同构移植：

### 第 1 层：延迟首次显示（pendingFirstShow）—— 根治错位

**新组合首帧不立即显示候选窗**，而是等 reflow 后的权威坐标（或兜底超时）再首显。从根本上避免在 reflow 前的陈旧坐标处显示，因此既无错位也无"先显示再跳"。

状态机（Rust：`wind-coordinator/src/coordinator.rs`）：

- 字段：`pending_first_show: Mutex<bool>`、`pending_first_show_token: Mutex<u64>`、`candidate_shown: Mutex<bool>`、`show_authorized: AtomicBool`。
- `notify_ui_update` 门控：过了"无内容/candwin 隐藏"守卫后，若 `!show_authorized && !candidate_shown`（首帧且非授权）→ `arm_pending_first_show()` 并 `return`（不下发）。
- `arm_pending_first_show_with_timeout(ms)`：置 `pending=true`、自增 token、`thread::spawn` 兜底 timer（默认 **150ms**）。timer 到点比对 token/pending 仍有效则强制首显（用当前坐标，慢应用降级）。
- `handle_caret_pending`：DLL 握手，若正等待首显则把兜底超时延长到 **600ms**（应对 `OnLayoutChange` burst 慢的应用，如 EverEdit）。
- `handle_caret_update`：`height==0` 直接跳过；权威坐标到达且 `was_pending` → `show_authorized=true` 后 `notify_ui_update`（首显落在正确坐标）。
- 下发 `UpdateCandidates` 后置 `candidate_shown=true`；`reset_first_show()` 复位首显状态并作废 timer，调用点：`notify_ui_hide`、`notify_ui_update` 的隐藏分支、`handle_commit_request`（上屏）、**顶码上屏**（`top_code_commit`：部分上屏 + 余码续组合，宿主光标已前移，余码候选窗须重新延迟到新坐标并重锁组合起点）。

时序（上屏后立即输入下一个）：
```
上屏 commit → reset_first_show（candidate_shown=false, 作废 timer）
首键 → notify_ui_update：首帧未授权 → arm（pending=true, 150ms timer），不显示
DLL CaretPending → handle_caret_pending：延长到 600ms
DLL reflow 后 CaretUpdate(height>0, C_new) → was_pending → 授权首显 → 候选窗出现在 C_new
（从不在旧坐标 C_old 显示，故无错位、无跳动）
```

### 第 2 层：3px caret 移动过滤 —— 已显示后防抖

候选窗已显示后，`handle_caret_update` 收到的坐标若与上次相差 `≤3px`（且非首显）→ 跳过 reshow，吞掉宿主 caret 微调（如 WPS 的 2px 偏移）。显著变化（换行/reflow 修正）才 reshow。

### 第 3 层：4px 位置阈值 —— 渲染落位防抖

UI 层（`wind-ui/src/candidate_window.rs`）**每帧据当前光标 + 内容尺寸重算窗口位置**（`place_window`），再与上次内容锚点比较：`<4px*scale` 微移则保持原位（`last_content_pos`）。这是位置保护的最后一道，吞掉穿过前两层的残余微抖。

> 注意：UI 层**不再锁定锚点**（早期 Rust 实现曾用 anchor 锁定，导致首帧锁死在陈旧坐标、reflow 坐标被忽略——正是 bug 之源）。改为"每帧重算 + 阈值过滤"，与 Go 一致。

### place_window 的定位规则（满足若干交互需求）

`place_window(caret_x, caret_y, caret_h, w, h, sticky_above)`（`candidate_window.rs`）：

- 默认显示在光标下方（`caret_y + gap`）；下方空间不足则上翻到光标上方。
- **上方显示以"窗口底边贴光标顶端"为参考**：`above_y = caret_top - h - gap`，底边与高度无关 → 候选变少时顶边下移、底边不动，不会离光标变远。
- **上翻粘滞（sticky_above）**：一旦上翻，候选数量变化也保持上方，仅当上方也放不下才回落（`placed_above` 跨帧维持，隐藏时复位）。
- 左右溢出贴边（横向右方不足时左移）。
- 尺寸变化每帧重算 → 不溢出屏幕。

## 5. 关键文件 / 函数索引

| 层 | 文件 | 关键点 |
|----|------|--------|
| DLL | `wind_tsf/src/TextService.cpp` | `_compositionJustStarted`、`SendCaretPositionUpdate`、`OnLayoutChange`、`SendCaretUpdate` |
| 协调器 | `wind-coordinator/src/coordinator.rs` | `pending_first_show`/`candidate_shown`/`show_authorized`/`composition_start`、`arm_pending_first_show*`、`reset_first_show`、`notify_ui_update` 门控+坐标基准、`handle_caret_update`、`handle_caret_pending`、`handle_commit_request` |
| IPC | `wind-bridge/src/{handler,server}.rs` | `CaretData{ x,y,height,composition_start_x,composition_start_y }`、`CMD_CARET_PENDING → handle_caret_pending` |
| UI | `wind-ui/src/candidate_window.rs` | `place_window`、`last_content_pos` + 4px 阈值、`placed_above` 粘滞 |

### 第 4 层：compositionStart 组合起点锚定 —— 钉在缓冲头部

**嵌入预编辑模式**（`app_inline`：编码插入宿主、宿主光标随输入右移）下，候选窗若跟随当前光标会一直移到输入缓冲末尾。改用**组合起点坐标**锚定，使候选窗钉在缓冲头部不随输入移动：

- `coordinator.rs` 新增 `composition_start: Mutex<(x, y, valid)>`。
- `handle_caret_update`：组合内首个有效 `compStart` 锁定（`!valid` 时才写），后续即便携带新值也不覆盖——防部分控件 `GetRange` 让起点随输入漂移；`<500px` 校验排除 logical/physical 坐标系不一致。
- `notify_ui_update` 坐标块：`in_app && compStart.valid` → 用 `compStart` 替代当前光标。
- `reset_first_show`（组合结束/隐藏）复位 `valid=false`，下一组合重新锁定。
- 非嵌入模式（preedit 显示在候选窗、宿主光标不动）仍用当前光标。

> 局限：组合期间锁定首个 compStart，故宿主窗口在组合中途移动/滚动时候选窗不跟随（与 Go 一致；组合通常很短，影响小）。

## 6. 已知降级与未移植项

- **慢应用兜底**：若 reflow 坐标在超时（150ms/600ms）内未到达，兜底 timer 用按键前的当前坐标首显——可能短暂错位后被后续 reflow 坐标 reshow 纠正。属可接受降级。
- **pendingReplay（跨焦点 buffer 重放）**：Go 对 Excel 单元格/编辑栏切换等有专门的 replay 路径，Rust 暂未引入。

## 7. 调参

- `arm_pending_first_show` 默认超时 **150ms**、握手延长 **600ms**（`coordinator.rs`）。
- 第 2 层 caret 过滤阈值 **3px**（`handle_caret_update`）。
- 第 3 层位置阈值 **4px × DPI scale**（`candidate_window.rs`）。

数值取自 Go 实测经验。调小→更跟手但更易抖；调大→更稳但大幅移动可能滞后。
