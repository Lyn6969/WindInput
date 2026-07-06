# 智能符号（Smart Symbol）宿主兼容性问题记录

本文记录 2026-07 排查智能符号功能（`input.symbol.smart_mode`，连按同一中文标点转英文）在不同宿主下的兼容性问题、已修复项，以及一个尚未彻底解决、决定搁置的遗留问题。

## 背景：两种实现方案

`input.symbol.smart_method` 枚举（`wind-config/src/config.rs`）：

- `HoldComposition`（默认）：press1 把中文符号放进 TSF 组合态（预上屏），press2 直接替换组合提交英文；超时无 press2 则自动提交中文。整个过程只用 TSF 组合态 API，不发送任何合成按键。
- `DeleteReplace`：press1 直接提交中文符号（正常上屏），press2 时删除光标前 N 个字符、替换为英文。实现在 `wind_tsf/src/TextService.cpp` 的 `CTextService::ReplacePrecedingChars`。

本文主要涉及 `DeleteReplace` 方案在 `ReplacePrecedingChars` 里暴露的一系列宿主兼容性问题。

## 已修复的问题

### 1. Office 下 500ms 后符号重复上屏

`HoldComposition` 超时收口（`OnHoldTimerExpired` → `CommitText`）是从 `SetTimer` 回调发起的 `TF_ES_SYNC` 请求，不在真实按键的同步上下文里。Word 对这类请求的校验比其它宿主严格，会出现"`DoEditSession` 内部其实已经执行完毕，但外层 `RequestEditSession` 的 `hr`/`hrSession` 报告失败"的情况。旧代码要求 `hr`、`hrSession`、`GetSuccess()` 三者都成功才算数，误判失败后继续走 `SendInput` 兜底，在已经正确写入的文字后面又打一遍。

**修复**：`CommitText`、`ReplacePrecedingChars` 改为只信 `GetSuccess()`——它只在 `DoEditSession` 内部真正执行完 `SetText`/`EndComposition` 后才置 `TRUE`，是文档是否已被修改的唯一可信信号（`TF_ES_SYNC` 请求下 `DoEditSession` 必然在 `RequestEditSession` 返回前同步跑完，读取顺序没有竞争）。

### 2. 部分终端/微信下合成按键被自己的钩子二次处理

`ReplacePrecedingChars`、`CommitText`、`InsertText` 的 `SendInput` 兜底注入的退格/字符键，此前没有任何"这是我自己刚合成的按键"标记，会被自己的 `KeyEventSink::OnTestKeyDown` 钩子当成真实用户输入又处理一遍。

**修复**：复用 auto-pair 已有的 `_PushSkipKey`/`_TryConsumeSkipKey` 跳过表机制，新增公开方法 `CKeyEventSink::MarkSyntheticKey(vk)`，三处 `SendInput` 兜底注入前都调用它标记（退格用 `VK_BACK`，Unicode 注入的字符用 `VK_PACKET`）。同时把 `MAX_SKIP_KEYS` 从 16 提到 64（`CommitText`/`InsertText` 可能一次标记一整段文本）。

### 3. 微信 / Windows Terminal 下智能符号完全不触发

真正的阻塞点，比替换执行细节更靠前：`prev_char`（`TextService.cpp` 的 `OnEndEdit` 现读文档得到，光标前一字符）在这类宿主里经常读取失败，恒为 0。服务端（`wind-coordinator/src/handle_punct.rs`）判定 press2 的条件要求 `prev_char` 与武装串末位字符相等，`prev_char=0` 时永远不匹配，于是永远判定"不是 press2"，把第二次按键当全新 press1 处理——服务端根本没有下发过 `ReplaceBackward`。

**修复**：`prev_char == 0` 视为"宿主读不回文档"而非"确定不匹配"，退回只信服务端自己的武装状态（armed + key + timeout，与文档内容无关）判定 press2。改了 `handle_punct.rs` 里 `DeleteReplace` 和 `HoldComposition→fallback` 两个分支。

## 已知但决定不修的问题：TSF 报告成功但实际渲染未生效（Tabby / 微信）

实测发现 Tabby（Electron/Chromium 内核终端）、微信（Qt 内核）这两个宿主自制的 TSFTextStore，对 `CReplaceBackwardEditSession` 的 `ShiftStart`+`SetText` 会**全程报告成功**（`hr`、`hrSession`、`GetSuccess()` 皆 `S_OK`），但实际画面上旧符号没删掉、新符号又插入了一份。同一段代码在 Notepad/Office 等原生编辑控件里结果是对的——不是我们这边 range 算错，是宿主自己的 TSFTextStore 内部模型跟它真实渲染的内容对不上，单靠更严格检查 TSF 返回码无法识别。

一度尝试过"默认全局改用真实合成按键（不信任 TSF 成功与否）"，但这会在 EverEdit 之类宿主上撞到下面这个新问题，且用户按住 Shift 连续输入多个符号时无法排队处理（见下）。**最终决定：`ReplacePrecedingChars` 默认仍然优先走 TSF 同步 range 替换**（`kTryTsfRangeReplace = true`，`TextService.cpp`），Tabby/微信这个问题暂不处理；如果以后确认必须解决，思路是按宿主进程名特判，只在已知问题宿主上切到合成按键路径，而不是全局切换。

## 遗留问题（本次搁置）：EverEdit 下 Shift 类标点删改失败

### 现象

EverEdit 编辑器里，`DeleteReplace` 的 TSF range 替换**稳定真实失败**（`hrSession=0x80004005` / `E_FAIL`，每次必现，不是间歇性谎报），代码正确地回退到 `SendInput`（退格 × count + Unicode 注入新文本，已加 `MarkSyntheticKey`）。

- 不带 Shift 的标点（如句号"。"→"."）：SendInput 兜底表现正常。
- 需要 Shift 组合键才能打出的标点（如 Shift+"-" 打出中文破折号"——"）：删改失败，旧符号没删掉，新符号又插入了一份（表现为重复上屏）。

日志实证（同一 EverEdit 会话）：

```
mods=0x0（句号，无 Shift）→ TSF failed → SendInput fallback → 结果正确
mods=0x11（Shift + "-"）    → TSF failed → SendInput fallback → 结果错误
```

### 根因

`SendInput` 注入 `VK_BACK` 时，Windows 会把当前**物理**按住的修饰键状态和注入的按键叠加——只要 Shift 还按着，宿主收到的实际上是"Shift+Backspace"而不是普通退格。EverEdit 把这个组合键解读成了别的操作（不是删除单个字符），于是旧符号删不掉；新文本走 `KEYEVENTF_UNICODE` 直接注入 Unicode 码点，不受修饰键影响，正常插入——两者叠加就是"重复上屏"。

### 已排除的两种修法

1. **注入前临时松开 Shift，退格后再恢复 Shift**：这是最直觉的修法，但代码里 `_SimulatePairKey` 相关注释早就记录过并放弃了这条路——松开/恢复 Shift 会让 OS 产生新的 Shift 按下/抬起事件，可能污染中英文切换的判定状态机（`_pendingKeyUpKey` 等）。本次复核确认了具体原因：`OnTestKeyDown`/`OnTestKeyUp` 会检查"自生成按键跳过表"（`_TryConsumeSkipKey`），但**真正维护中英切换状态机的 `OnKeyDown`/`OnKeyUp` 完全不检查这张表**，即使把合成的 Shift 按键标记为跳过也拦不住状态机被污染。要根治得把 skip 检查也接入 `OnKeyDown`/`OnKeyUp`——这两个函数是所有按键处理的公共入口，改动面和回归风险都不小，需要仔细回归热键、中英切换等功能。

2. **延后到修饰键释放后再执行替换**（仿照 `_SimulatePairKey` 的 `_pendingPairAction` 延迟队列）：已实现过一版又撤销。问题是用户可能按住 Shift 连续输入多个符号（例如连按多次 Shift+"-"），每次替换都相对"当前光标位置"操作；如果排队等到 Shift 最终松开才批量执行，前面几次替换执行时的光标位置假设已经过时，容易执行到错误的位置——这不是简单排队能解决的时序问题，且用户明确反馈了会"卡住"。

### 后续方向（未实施，供下次参考）

- 方向 A：把 skip 检查接入 `OnKeyDown`/`OnKeyUp`，让"松开/恢复修饰键"技术真正安全，根治所有 Shift 类合成按键场景（不限于智能符号）。工作量最大，但最通用。
- 方向 B：检测到"TSF range 替换真失败 + 修饰键按住"时，直接放弃这次智能符号转换（保留中文符号原样，不报错也不重复），把功能收窄成"无 Shift 才支持"，改动小但功能有缺口。
- 方向 C：调研 EverEdit 具体把 Shift+Backspace 解读成了什么操作，看是否有更精确的绕过方式（例如换一种删除机制，如 `WM_CHAR` 直接注入退格字符 `\b` 而非 `VK_BACK` 虚拟键，规避虚拟键+修饰键叠加的问题——未验证是否可行）。

## 相关代码位置

- `wind_input/crates/wind-coordinator/src/handle_punct.rs`：`try_smart_symbol_replace`（press1/press2 状态机、`prev_char` 判定）
- `wind_tsf/src/TextService.cpp`：`CommitText`、`ReplacePrecedingChars`（含 `kTryTsfRangeReplace` 开关）、`CReplaceBackwardEditSession`（含诊断用 readback 日志）
- `wind_tsf/src/KeyEventSink.cpp` / `include/KeyEventSink.h`：`MarkSyntheticKey`、`_PushSkipKey`/`_TryConsumeSkipKey`、`_SimulatePairKey`（Shift 合成按键相关历史教训）
