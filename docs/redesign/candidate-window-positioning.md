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
| `OnLayoutChange` debounce | `OnLayoutChange` | reflow 完成的权威信号；burst 期间 debounce，等稳定后 flush（50ms，首显延迟的大头） |
| 50ms timer 兜底 | — | 应对完全不发 `OnLayoutChange` 的应用（**比预想的多**，见第 6 层的宿主画像：Word / 记事本都不发） |
| `SendCaretProbe` 试探采样 | `OnLayoutChange` 首帧分支 | 首帧 reflow 期间每次 layout change 采一次坐标（限前 5 次）发给服务端，供 `fast` 档提前放行；DLL 不做判断 |
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
- `arm_pending_first_show_with_timeout(ms)`：置 `pending=true`、自增 token、共享定时器线程排兜底 timer（超时按档位取，见第 6 层）。timer 到点比对 token/pending 仍有效则强制首显（用当前坐标，慢应用降级）。
- `handle_caret_pending`：DLL 握手，若正等待首显则把兜底超时延长到 **600ms**（应对 `OnLayoutChange` burst 慢的应用，如 EverEdit）；`fast` 档拒绝此延长。
- ⚠ **`reset_first_show()` 会 bump token 作废未到期的 timer**。它在每次上屏时都被调用，所以兜底超时
  一旦长于组合寿命，timer 就永远等不到自己到期——这是 `fast` 档必须用短兜底的直接原因（第 6 层判据 3）。
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
| 协调器 | `wind-coordinator/src/coordinator.rs` | `pending_first_show`/`candidate_shown`/`show_authorized`/`composition_start`、`arm_pending_first_show*`/`first_show_fallback_ms`、`reset_first_show`、`notify_ui_update` 门控+坐标基准、`handle_caret_update`、`handle_caret_pending`、`handle_caret_probe`、`handle_commit_request`、`update_active_compat`/`active_compat`/`process_name`、`first_show_was_provisional`/`last_authoritative_caret`/`last_key_interval_ms` |
| 兼容规则 | `wind-config/src/app_compat.rs`、`data/compat.toml` | `AppCompat::load`/`get_rule`（`[[apps]]`：`process`/`caret_use_top`/`first_show_mode`），系统层+用户层覆盖；`FirstShowMode` 枚举、`set_user_first_show_mode`（菜单写盘） |
| 菜单 | `wind-coordinator/src/handle_menu.rs`、`wind-ui/src/manager.rs` | `set_first_show_mode`（写盘→重载→刷新 active_compat）、`MenuCmd::FirstShowMode(u8)`（id 段 `5000..=5999`） |
| IPC | `wind-bridge/src/{handler,server}.rs` | `CaretData{ x,y,height,composition_start_x,composition_start_y }`、`CMD_CARET_PENDING → handle_caret_pending`、`CMD_CARET_PROBE → handle_caret_probe`、`client_token`（高 32 位 = PID） |
| UI | `wind-ui/src/candidate_window.rs` | `place_window`（下方 `caret_y+gap`、上方 `caret_y-height-…`）、`last_content_pos` + 4px 阈值、`placed_above` 粘滞 |

### 第 4 层：compositionStart 组合起点锚定 —— 钉在缓冲头部

**嵌入预编辑模式**（`app_inline`：编码插入宿主、宿主光标随输入右移）下，候选窗若跟随当前光标会一直移到输入缓冲末尾。改用**组合起点坐标**锚定，使候选窗钉在缓冲头部不随输入移动：

- `coordinator.rs` 新增 `composition_start: Mutex<(x, y, valid)>`。
- `handle_caret_update`：组合内首个有效 `compStart` 锁定（`!valid` 时才写），后续即便携带新值也不覆盖——防部分控件 `GetRange` 让起点随输入漂移；`<500px` 校验排除 logical/physical 坐标系不一致。
- `notify_ui_update` 坐标块：`in_app && compStart.valid` → 用 `compStart` 替代当前光标。
- `reset_first_show`（组合结束/隐藏）复位 `valid=false`，下一组合重新锁定。
- 非嵌入模式（preedit 显示在候选窗、宿主光标不动）仍用当前光标。

> 局限：组合期间锁定首个 compStart，故宿主窗口在组合中途移动/滚动时候选窗不跟随（与 Go 一致；组合通常很短，影响小）。

### 第 5 层：应用兼容规则 caret_use_top —— WebView 光标矩形归一化

部分应用（微信 Qt WebView 输入框等）`GetTextExt` 返回的光标 `height` 不稳定：在 `1`/`20px` 间跳变，
导致 `rect.bottom`（= top + height）相差达 ~20px，而 `rect.top` 始终稳定（≤1px，视觉上 ≈ 正文底端）。
若按默认的 `rect.bottom` 定位，候选窗会随 height 跳变上下抖 ~20px。

按进程名匹配的兼容规则（`compat.toml` 的 `[[apps]]`，对齐 Go `pkg/config/compat.go`）解决：

- **规则加载**：`wind-config/src/app_compat.rs` 的 `AppCompat::load(data_dir, user_dir)`——系统层
  `{data}/compat.toml` + 用户层 `{user_config}/compat.toml` 覆盖；`get_rule(process)` 不区分大小写。
  系统预置见 `data/compat.toml`（`Weixin.exe → caret_use_top = true`）。
- **进程识别**：协调器 `update_active_compat(client_token)` 从 `client_token` 高 32 位取 PID
  （`pid = token >> 32`，复用既有 token 编码，无需改 IPC 协议），经 `process_name(pid)`
  （`OpenProcess` + `QueryFullProcessImageNameW`，对齐 Go `bridge.GetProcessName`）解析进程名，
  查规则后把 `(pid, caret_use_top)` 缓存进 `active_compat`；按 pid 缓存避免每帧 `OpenProcess`。
  接入点：`handle_focus_gained`（FOCUS_GAINED 重型后置段，不在 DLL 同步阻塞路径上）、`handle_ime_activated`。
- **坐标变换**（`handle_caret_update` 顶部，对齐 Go `HandleCaretUpdate`）：命中规则时
  `Y -= rawH`（bottom → 稳定的 top）、组合起点 Y 同步上移。

> **height 必须保留真实行高，不能压成 1**（与 Go 的差异点）：wind-ui 的**下方**公式 `below_y = caret_y + gap`
> 不读 height，故下方紧贴只靠稳定的 top；但**上方**公式 `above_y = caret_y - height - hi - gap` 用
> `caret_top = caret_y - height` 推算正文顶端。若 height=1，正文顶端被当成 `top-1`（≈正文底端），
> 上方候选窗会整条压住正文/光标。故变换保留 `height = rawH.max(CARET_USE_TOP_MIN_LINE_H=18)`：
> 真实行高让上方正确避让正文，退化帧（rawH=1）落到下限兜底。偏大只是上方多留空隙，偏小才遮挡——宁大勿小。

### 第 6 层：首显档位 FirstShowMode —— 用延迟换准确的三档取舍

第 1 层根治了错位，但代价是**首显恒定延迟 85~95ms**（C++ `OnLayoutChange` 的 50ms debounce 占大头，
每次 burst 事件都重置它）。连打时组合本身只活几十毫秒，候选窗往往来不及出现就被下一次上屏
`reset_first_show()` 掀掉，表现为「迟钝」。且延迟无法靠单纯调小超时解决——超时短了就退回错位。

出路是承认**这是取舍而非 bug**，按宿主分档。`compat.toml` 的 `first_show_mode`（`wait`/`fast`/`instant`，
`FirstShowMode` 枚举）逐进程选择：

| 档位 | 菜单名 | 首帧行为 | 适用 |
|------|--------|----------|------|
| `wait`（默认） | 等待光标稳定 | 第 1 层原样：等权威坐标或 150ms 兜底 | 未验证过的宿主，保守 |
| `fast` | 快速显示 | 采信试探坐标 / 连打直接放行 / 短兜底，三条判据见下 | 发 `OnLayoutChange` 且 reflow 快的宿主 |
| `instant` | 立即显示候选窗 | 完全不等，用按键前的坐标（走 `notify_ui_update` 逃生口） | 组合期极短、或根本不上报组合坐标的宿主 |

`fast` 档的三条判据（`coordinator.rs::handle_caret_probe` / `first_show_fallback_ms`）：

1. **试探采样 + 「≠ 上一轮权威坐标」**：DLL 在首帧 reflow 期间每次 `OnLayoutChange` 取一次坐标发
   `CMD_CARET_PROBE`（限前 5 次）。协调器判断该坐标是否已不等于上一轮权威坐标——不等即说明宿主已
   reflow，本帧可信，立即首显。这一条把 EverEdit 的首显从 ~90ms 压到 ~3ms、WPS 到 ~11ms。
2. **连打快路径**（`fast_typing_window_ms`，默认 100ms）：相邻两次按键间隔小于该值时，跳过第 1 条的
   比对直接采信首条采样。依据是连打时光标沿同一行顺序前移、不发生重排，坐标本就八九不离十，而这种
   节奏下用户对「跟手」的敏感度远高于十几像素的偏差。
3. **短兜底**（`fast_first_show_fallback_ms`，默认 25ms）：等不到试探/权威坐标就用现有坐标先显示。
   **这一条不可省**——见下面的宿主画像：不发 `OnLayoutChange` 的宿主拿不到任何试探坐标，若沿用 `wait`
   档的 150ms，兜底 timer 会在组合结束时被 `reset_first_show()` 作废而**永远不会到期**，`fast` 就静默
   退化成了 `wait`。同理 `handle_caret_pending` 的 600ms 延长对 `fast` 档刻意不生效。

#### 宿主画像（实测，AutoHotkey `d`+空格 循环 50 轮）

| 宿主 | 组合期发 `OnLayoutChange` | 组合坐标到达延迟 | 说明 |
|------|--------------------------|------------------|------|
| EverEdit | 会，burst 3~4 次 | **3~10ms**（试探） | `fast` 档最理想的宿主 |
| WPS | 会 | ~7ms（试探，前两次仍是旧坐标） | 需靠判据 1 跳过旧坐标帧 |
| **Word** | **50 轮 0 次** | **60~190ms** | 坐标只能靠 C++ 50ms timer + 异步 `GetTextExt`，而 Word 的 edit session 排队极慢 |
| 记事本 | 几乎不发（仅首轮 1 次） | 拿不到 | 组合期间完全不上报坐标 |

> **教训**：`OnLayoutChange` 不是普遍可依赖的信号。任何「等某个宿主回调」的策略都必须自带
> **短于组合寿命**的兜底，否则在不发该回调的宿主上会静默退化成上一档，而日志里只表现为
> 「一直在等」——没有任何一行报错。

#### 首显容差：抖动来自校正动作，不是坐标偏差

`fast`/`instant` 用的是非权威坐标，随后真权威坐标到达时若照第 2 层的 3px 判据必然触发 reshow，
表现为「显示后跳一下」——**而抖动的观感恰恰来自这个校正动作，不是十几像素的偏差本身**。
故引入 `first_show_settle_ratio`（默认 **0.8**）：本轮首显用过非权威坐标时（`first_show_was_provisional`），
该轮**第一次**权威坐标只要偏差在 `行高 × 0.8` 以内就不校正。换行/重排的偏差通常 ≥2 个行高，远超
此阈值，仍会正常校正。多数商业输入法也是这么处理的。

置位该标志的三条路径：`instant` 逃生口、`fast` 的试探采信、以及兜底 timer 到期首显（用的都是旧坐标）。

#### 入口

- **右键菜单**：高级 → 「候选窗首显（<进程名>）」子菜单单选，写用户层 `compat.toml` 并热重载整表
  （`handle_menu.rs::set_first_show_mode`，三步：写盘 → 重载 → 刷新 `active_compat`）。
- **配置**：`compat.toml` 的 `first_show_mode`；三个内部选项在 `config.toml` 的 `[ui.candidate]` 下，
  不进设置页（`first_show_settle_ratio` / `fast_typing_window_ms` / `fast_first_show_fallback_ms`）。

## 7. 已知降级与未移植项

- **慢应用兜底**：若 reflow 坐标在超时内未到达，兜底 timer 用按键前的当前坐标首显——可能短暂错位后被
  后续 reflow 坐标 reshow 纠正（`fast`/`instant` 下受 0.8 行高容差保护，多数不会真的 reshow）。属可接受降级。
- **`fast` 档在极端连打下仍可能不显示**：Word 在 AutoHotkey `sleep 60` 节奏下组合只活 27~57ms，
  25ms 兜底加 IPC 往返约 30ms，最短的那批赶不上。真人打字节奏（>100ms）不受影响。若要进一步压，
  只能调小 `fast_first_show_fallback_ms`（代价是更常用到旧坐标）。
- **`fast` 档对宿主的依赖未消除**：判据 1、2 都要求宿主发 `OnLayoutChange`；不发的宿主实际是靠判据 3
  退化成 `instant` 在工作，「快速」二字对它们名不副实。彻底解法需要一条不依赖该回调的坐标来源。
- **pendingReplay（跨焦点 buffer 重放）**：Go 对 Excel 单元格/编辑栏切换等有专门的 replay 路径，Rust 暂未引入。

## 8. 调参

- `arm_pending_first_show` 超时：`wait`/`instant` 档 **150ms**、握手延长 **600ms**；
  `fast` 档 `ui.candidate.fast_first_show_fallback_ms`（默认 **25ms**，且拒绝 600ms 延长）。
- 连打判定窗口 `ui.candidate.fast_typing_window_ms`（默认 **100ms**，0 = 关闭该快路径）。
- 首显容差 `ui.candidate.first_show_settle_ratio`（默认 **0.8** × 行高，0 = 关闭）。
- 第 2 层 caret 过滤阈值 **3px**（`handle_caret_update`；容差生效时取两者较大值）。
- 第 3 层位置阈值 **4px × DPI scale**（`candidate_window.rs`）。

前三项取自本仓 Windows 实测（见上方宿主画像），后两项取自 Go 实测经验。
调小→更跟手但更易抖；调大→更稳但大幅移动可能滞后。
