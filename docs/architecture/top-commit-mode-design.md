# 顶码上屏「上屏策略」内部配置（top_commit_mode）设计

状态：设计已定稿，待实现 · 分支 `feat/top-commit-mode` · 2026-07-09

## 1. 背景与动机

### 1.1 当初为什么用「预确认提交」

顶码上屏（top-code commit：码表输入超过满码长且整串无匹配时，顶前 N 码首选、余码续打）在**终端类应用**（Tabby / Electron / 浏览器等 Chromium TSFTextStore 宿主）会出现**余码双重上屏**——输入 `skce…`（顶出「可能」）续打 `yijg`，屏上出现「可能y就是y」这类余码重复。

**根因**（记忆 `project_tsf_desync_analysis` Bug 1，commit `7f616c2`，真机验证通过）：`InsertTextAndStartComposition` 在**单个 EditSession（一次文档锁）**内做了「旧组合 SetText+EndComposition + 新组合 StartComposition+SetText」。Chromium 的 TSFTextStore 按**整锁 diff** 生成 DOM 事件，把新组合的首码并进了 commit（compositionend 带出「可能g」）→ 余码字面双写。

当年的修复照抄某款输入法：用 `_pendingCommitPrefix` 把顶出文字**留在组合态内**（分段显示属性做成「无下划线=像已上屏」），延迟到最终一次 `CommitText` 才真提交。宿主全程只看到 compositionupdate、最后一次 compositionend——diff 式宿主不再把余码并入提交。引擎侧零改动。

失败的死路（勿重走）：①拆两锁 commit+restart（**背靠背、同一拍**）→ tabby 仍双写；②部分 ShiftStart → 仍双写（Chromium 对 ShiftStart 无一流支持）。

### 1.2 预确认方案的新问题（本次动机）

- **下划线观感不一致**（用户反馈）：「无下划线=像已上屏」是靠**分段显示属性**伪装的，很多宿主**不认分段**，把整个组合（含已确认前缀）画上下划线 → 已确认文字看起来没上屏。
- **WPS 智能标点顶屏清空**（上一轮排查确诊）：预确认路径在 WPS 下，`CommitText 结束组合 → 立即 StartComposition 新组合` 被 WPS 只授予**异步**编辑会话（`TF_S_ASYNC`），其 TSFTextStore 把异步 diff 相对「刚结束的旧组合」快照套用，「结束+同位置重开」塌缩成对同一区间的替换 → 抹掉刚提交的文字。

### 1.3 关键新证据：真实输入法用「真提交 + keyup 延迟重开」

用 `temp/ime-event-probe.html` 抓另一款输入法（终端下无双写）的顶码事件流，两组日志一致显示：

```
keydown(触发键)                          ← 顶码触发
  compositionupdate '可能' → input '可能'
  compositionend    '可能'               ← ★ 提交在 keydown 内同步完成
keyup(同一键)                            ← +70~100ms（= keydown→keyup 自然时长）
  compositionstart '' → compositionupdate 'y'  ← ★ 新组合在 keyup 之后才开
```

**结论**：提交发生在触发键的 **keydown** 内，新组合发生在同一键的 **keyup** 之后。这一拍间隔让「提交」和「重开组合」落在**两个独立文档锁 / 两轮消息泵**里，diff 式宿主不合并 → 不双写。那个「~100ms」不是固定延时，而是 **keyup 这个边界**。当年「拆两锁仍双写」之所以失败，正因为是同一拍背靠背、没隔消息泵——**差别不在锁数量，在于中间是否隔了一拍**。

> 注：该证据修正了 `project_tsf_desync_analysis` 里「某输入法顶码根本不提交」的旧结论——那是另一款/另一版输入法的行为。两种行为都存在，本设计采纳「真提交 + keyup 延迟重开」这一被验证可行的路径。

## 2. 目标与非目标

**目标**
- 给**顶码上屏**加一个内部配置 `input.top_commit_mode`，可在两种上屏策略间切换，用于按宿主 A/B 对比：
  - `pre_confirm`（预确认）：当前 `_pendingCommitPrefix` 聚合行为。
  - `direct_commit`（直接提交，**默认**）：真提交 + keyup 延迟重开新组合。

**非目标（本期不做）**
- 智能标点顶屏（`CommitAndHold` 路径，即 WPS 清空那条）——留作后续，复用本期的「commit + keyup 延迟重开」原语。
- 设置界面 UI（wind-setting 独立仓）——本期只落 TOML + 接线。
- per-app 自动分流——只在 `TopCommitMode` 定义处留注释说明后续可接，本期只做全局默认。

## 3. 配置设计

`wind-config/src/config.rs`：

```rust
/// 顶码/顶屏的宿主上屏策略。影响顶码上屏时「已确认文字」如何落到宿主：
/// - PreConfirm：留在 TSF 组合态（_pendingCommitPrefix 聚合），延迟到最终 CommitText 才真提交。
///   diff 式宿主（终端/Chromium）不双写，但部分宿主整段画下划线、WPS 智能标点顶屏会清空。
/// - DirectCommit：顶码时真提交，余码新组合延迟到触发键 keyup 才开（照抄真实输入法时序），
///   靠「隔一拍消息泵」躲开 diff 合并；真提交无下划线歧义、WPS 不清空。
/// TODO(per-app)：后续可按宿主进程名 override（当前仅全局默认）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TopCommitMode {
    PreConfirm,
    #[default]
    DirectCommit,
}
```

挂到 `InputConfig`：

```rust
pub struct InputConfig {
    // …既有字段…
    /// 顶码上屏策略（内部/实验，默认 direct_commit 真提交时序）。
    #[serde(default)]
    pub top_commit_mode: TopCommitMode,
}
```

- 默认 `DirectCommit` = 真提交时序，躲开 diff 合并与整段下划线；需回退旧行为显式设 `pre_confirm`。
- serde `snake_case` → TOML 值 `pre_confirm` / `direct_commit`。

## 4. 数据流（direct_commit 下的顶码）

分叉点在 `coordinator.rs` 顶码分支（现 `:3580` 返回 `KeyAction::InsertText { has_new_composition: true, … }`）：

```
读 self.rt().config.input.top_commit_mode：
  PreConfirm  → 维持现状：InsertText{ new_composition: 余码, has_new_composition: true }
                → encode_commit_text(has_new_composition=true)
                → ResponseType::CommitText{ restartComposition=true }
                → C++ InsertTextAndStartComposition（prefix 聚合）
  DirectCommit → 新动作：CommitThenDeferComposition{ commit_text: 顶出文本, deferred_composition: 余码, timeout_ms }
                → 新 ResponseType
                → C++ 真提交 + keyup 延迟重开
```

新增：
- `KeyAction::CommitThenDeferComposition { commit_text: String, deferred_composition: String, timeout_ms: u32 }`（wind-coordinator）。
- 对应 IPC 响应类型 + 编解码（`wind-bridge/src/server.rs` 的 `encode_key_action`、`wind-ipc` codec、C++ `IPCClient.cpp` 解析、`ResponseType`）。
- `timeout_ms` = 兜底定时器时长（见 §5）。用**独立常量** `kDeferredCompositionFallbackMs = 150`（与 smart symbol 的 `smart_timeout_ms` 语义无关，不复用）；服务端把该常量随动作下发，C++ 直接用。

余码为空（顶码后无剩余码）时不发本动作，退化为普通 `CommitText`（无需重开组合）。

## 5. C++ 执行机制（keyup 延迟重开）

`wind_tsf`：

1. **收到 `CommitThenDeferComposition`**（在触发键的 keydown 处理链内）：
   - 立即 `CommitText(commit_text)`：`CCommitTextEditSession` 对当前组合 SetText(commit_text)+EndComposition，文档落定顶出文本（对齐 probe 的 compositionend@keydown）。此时 `_pendingCommitPrefix` 在 direct_commit 全程为空，`full == commit_text`。
   - 暂存 `_pendingDeferredComp = 余码` + `_deferredTriggerVk = 触发键 vk`；启动兜底定时器（`SetTimer` ~150ms）。
   - `_isComposing` 暂置 FALSE（组合已结束），待重开时再置 TRUE。
2. **触发键 keyup**（`KeyEventSink::OnKeyUp` / `OnTestKeyUp`）：若 vk 命中 `_deferredTriggerVk` 且暂存非空 → `StartComposition + UpdateComposition(余码)`（对齐 compositionstart@keyup），清暂存 + KillTimer。
3. **兜底定时器到期**（keyup 未达：长按自动重复 / 失焦 / 键被吞）→ 同样开余码组合，清暂存。**keyup 与定时器先到者触发、后到者作废**（用暂存是否已清空判定）。
4. **快打/失焦/透传防御**：暂存期间若来新键（keydown 进入服务）、`PassThrough`、失焦 `EndComposition`/`OnCompositionTerminated` → 先**立即 flush**（开出待重开的余码组合）再处理新事件，避免余码卡住或与后续字符错序。参照现有 `FlushHoldCompositionIfActive` 的挂接点布防。

新增状态（`TextService.h`）：`std::wstring _pendingDeferredComp; UINT _deferredTriggerVk; UINT_PTR _hDeferredTimer;` + `StartDeferredComposition()` / `CancelDeferredComposition()` / `FlushDeferredIfActive()`，与 HoldComposition 计时器状态并列、互不干扰。

## 6. 边界情况

| 场景 | 处理 |
|---|---|
| 余码为空 | 不发新动作，普通 CommitText |
| 触发键 keyup 缺席（长按重复/失焦/吞键） | 兜底定时器补开余码组合 |
| keyup 与定时器竞态 | 先到者开组合并清暂存，后到者见暂存空即 no-op |
| 暂存期间来新按键（快打） | 先 flush 余码组合，再处理新键（防错序） |
| 暂存期间失焦/宿主强杀组合 | flush 或按 EndComposition 语义收口；余码组合已在服务端 buffer，flush 后正常续打 |
| `pre_confirm`（回退旧行为） | 完全走现有路径，行为不变，零回归 |

## 7. 测试计划

- **Rust 单测**（wind-coordinator）：
  - `top_commit_mode=direct_commit` 时顶码返回 `CommitThenDeferComposition{commit_text, deferred_composition=余码}`。
  - `top_commit_mode=pre_confirm` 时顶码维持 `InsertText{has_new_composition}`。
  - 余码为空时退化为普通 CommitText。
  - config serde round-trip（`pre_confirm`/`direct_commit`）+ 缺省默认 `direct_commit`。
- **C++**：本机 MSVC `/Zs` 语法检查（TextService.cpp / KeyEventSink.cpp）。
- **真机 A/B**（两模式各测）：终端(Tabby/WT) 不双写 · WPS 顶屏不清空 · 记事本正常 · 已确认文字无下划线歧义。

## 8. 未来工作

- 智能标点顶屏（`CommitAndHold`）复用本原语：把「CommitText + 立即 HoldComposition」改为「CommitText + keyup 延迟重开 held 符号」，根治 WPS 清空。
- per-app 自动分流：按宿主进程名选 `top_commit_mode`（如 WPS/Office→direct_commit、终端→按对比结果定），接入统一 app_rules 机制。
- direct_commit 已设为默认；后续视真机反馈逐步下线 pre_confirm 分段显示属性伪装逻辑。
