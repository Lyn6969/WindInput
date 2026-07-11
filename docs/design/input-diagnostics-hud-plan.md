# 输入诊断 HUD + 密码框抑制 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增 DLL→服务端 禁用态上报链路 + 右键菜单可切换的实时 HUD 浮窗，并接通密码框强制英文抑制。

**Architecture:** C++ TSF 在 `OnSetFocus` 与 `KEYBOARD_DISABLED` compartment 变更时上报 `{pid, disabled, reason, input_scope_mask}`；服务端 coordinator 存最新态 `last_input_diag`，据密码位置 `password_suppress`（在 `effective_chinese` 处 `&& !password_suppress` 强制英文），并把最新态经 `ui_tx` 下发给 wind-ui 的 `InputDiagHud` 浮窗。开关走右键菜单「高级」，会话级不持久化。

**Tech Stack:** Rust workspace（wind-ipc / wind-bridge / wind-coordinator / wind-ui）+ C++ TSF（wind_tsf）+ Win32 LayeredWindow / DirectWrite。

设计来源：`docs/design/input-diagnostics-hud.md`。

## Global Constraints

- 提交信息不带 Co-Authored-By，不带 Constraint/Confidence/Tested 等 AI trailer（conventional commit 主题即可）。
- INFO 及以下级别日志不得含用户输入内容/候选词等敏感信息（本特性只记进程名/pid/掩码，允许）。
- Rust 单测命令：`cargo test -p <crate>`；改动后本 crate 测试须绿。
- wire 协议向后兼容：旧 36 字节 `focus_gained` 载荷仍须能解码（新字段缺省 0）。
- 两个「高级」开关**会话级、不持久化**；HUD 默认关、密码框抑制默认开。
- IS_PASSWORD = bit 31，IS_NUMERIC_PASSWORD = bit 63（与 C++ `kScopeBitPassword` / Go 端一致）。

---

## 文件结构

| 文件 | 责任 | 动作 |
|---|---|---|
| `wind-coordinator/src/input_diag.rs` | 诊断纯数据类型 + mask→reason 判定（可单测核心） | 新建 |
| `wind-ipc/src/protocol.rs` | `FocusGainedPayload` 扩展 2 字节 + `InputStateReportPayload` + `CMD_INPUT_STATE_REPORT` | 改 |
| `wind-ipc/src/codec.rs` | `decode_input_state_report` | 改 |
| `wind-bridge/src/handler.rs` | `FocusData` 加 disabled/reason；Handler trait 加 `handle_input_state_report` | 改 |
| `wind-bridge/src/server.rs` `server_unix.rs` | 填新字段 + 分发新命令 | 改 |
| `wind-coordinator/src/coordinator.rs` | `last_input_diag` / `password_suppress` / `effective_chinese` 接线 / HUD 下发 | 改 |
| `wind-coordinator/src/handle_menu.rs` | 「高级」两开关 + `run_menu_cmd` 分支 | 改 |
| `wind-ui/src/manager.rs` | `MenuCmd` 两变体 + `UiCommand::ShowInputDiag/HideInputDiag` + `InputDiagView` | 改 |
| `wind-ui/src/input_diag_hud.rs` | HUD 浮窗（仿 status_tip） | 新建 |
| `wind_tsf/src/TextService.cpp` `IPCClient.cpp` `BinaryProtocol.h` | C++ 上报（focus 扩展 + compartment 上报） | 改 |

依赖顺序：Task1（纯类型）→ Task2（协议）→ Task3（bridge）→ Task4（coordinator 存储+抑制）→ Task5（菜单开关）→ Task6（HUD 窗口）→ Task7（C++ 上报）。Task1/2/4/5 有单测；Task3/6/7 以集成/真机验证为主。

---

## Task 1: 诊断纯数据类型 + mask→reason 判定

**Files:**
- Create: `wind_input/crates/wind-coordinator/src/input_diag.rs`
- Modify: `wind_input/crates/wind-coordinator/src/lib.rs`（`mod input_diag;`）
- Test: 同文件 `#[cfg(test)]`

**Interfaces:**
- Produces:
  - `pub enum InputDiagReason { None, CompartmentDisabled, InputScopePassword, NumericPassword }`（`#[derive(Clone, Copy, Debug, PartialEq, Eq)]`）
  - `pub fn reason_from(disabled: bool, mask: u64) -> InputDiagReason`
  - `pub fn is_password_scope(mask: u64) -> bool`
  - `pub fn reason_label(r: InputDiagReason) -> &'static str`
  - `pub struct InputDiagState { pub pid: u32, pub process_name: String, pub disabled: bool, pub reason: InputDiagReason, pub mask: u64 }`（`#[derive(Clone, Debug, Default)]`；`InputDiagReason` 需 `impl Default`→`None`）

- [ ] **Step 1: 写失败测试**

在 `input_diag.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const IS_PASSWORD: u64 = 1 << 31;
    const IS_NUMERIC_PASSWORD: u64 = 1 << 63;

    #[test]
    fn reason_none_when_clean() {
        assert_eq!(reason_from(false, 0), InputDiagReason::None);
    }

    #[test]
    fn compartment_takes_precedence_over_mask() {
        // disabled=true 一律 CompartmentDisabled，即便 mask 有密码位
        assert_eq!(reason_from(true, IS_PASSWORD), InputDiagReason::CompartmentDisabled);
    }

    #[test]
    fn password_and_numeric_from_mask() {
        assert_eq!(reason_from(false, IS_PASSWORD), InputDiagReason::InputScopePassword);
        assert_eq!(reason_from(false, IS_NUMERIC_PASSWORD), InputDiagReason::NumericPassword);
    }

    #[test]
    fn is_password_scope_covers_both_bits() {
        assert!(is_password_scope(IS_PASSWORD));
        assert!(is_password_scope(IS_NUMERIC_PASSWORD));
        assert!(!is_password_scope(0));
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p wind-coordinator input_diag`
Expected: 编译失败（`input_diag` 模块/符号未定义）。

- [ ] **Step 3: 写最小实现**

`input_diag.rs` 顶部：

```rust
//! 输入诊断纯数据类型 + InputScope 掩码判定（无 I/O，可单测）。

/// InputScope 位：与 C++ kScopeBitPassword / Go 端一致。
const IS_PASSWORD_BIT: u64 = 1 << 31;
const IS_NUMERIC_PASSWORD_BIT: u64 = 1 << 63;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputDiagReason {
    None,
    CompartmentDisabled,
    InputScopePassword,
    NumericPassword,
}

impl Default for InputDiagReason {
    fn default() -> Self {
        InputDiagReason::None
    }
}

/// 判定禁用原因。compartment（DLL 已放行所有键）优先级最高。
pub fn reason_from(disabled: bool, mask: u64) -> InputDiagReason {
    if disabled {
        return InputDiagReason::CompartmentDisabled;
    }
    if mask & IS_NUMERIC_PASSWORD_BIT != 0 {
        return InputDiagReason::NumericPassword;
    }
    if mask & IS_PASSWORD_BIT != 0 {
        return InputDiagReason::InputScopePassword;
    }
    InputDiagReason::None
}

/// mask 是否命中密码/数字密码位（用于抑制策略）。
pub fn is_password_scope(mask: u64) -> bool {
    mask & (IS_PASSWORD_BIT | IS_NUMERIC_PASSWORD_BIT) != 0
}

pub fn reason_label(r: InputDiagReason) -> &'static str {
    match r {
        InputDiagReason::None => "无",
        InputDiagReason::CompartmentDisabled => "compartment",
        InputDiagReason::InputScopePassword => "密码",
        InputDiagReason::NumericPassword => "数字密码",
    }
}

#[derive(Clone, Debug, Default)]
pub struct InputDiagState {
    pub pid: u32,
    pub process_name: String,
    pub disabled: bool,
    pub reason: InputDiagReason,
    pub mask: u64,
}
```

在 `wind-coordinator/src/lib.rs` 加 `mod input_diag;`（若需对外则 `pub mod`；本计划内部使用即可，`pub(crate)` 语义用 `mod`）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p wind-coordinator input_diag`
Expected: PASS（4 tests）。

- [ ] **Step 5: 提交**

```bash
git add wind_input/crates/wind-coordinator/src/input_diag.rs wind_input/crates/wind-coordinator/src/lib.rs
git commit -m "feat(coordinator): 输入诊断数据类型 + InputScope 掩码判定"
```

---

## Task 2: wire 协议扩展 focus_gained + 新增 CMD_INPUT_STATE_REPORT

**Files:**
- Modify: `wind_input/crates/wind-ipc/src/protocol.rs`
- Modify: `wind_input/crates/wind-ipc/src/codec.rs`
- Test: `wind-ipc` 内 `#[cfg(test)]`（protocol.rs 末尾新增）

**Interfaces:**
- Consumes: 现有 `FocusGainedPayload { caret, client_token, input_scope_mask }`（SIZE=36）、`CaretPayload`（SIZE=20）、`IpcHeader`。
- Produces:
  - `FocusGainedPayload` 追加 `pub disabled: u8, pub reason: u8`；`SIZE = 38`；`from_bytes` 向后兼容（buf≥36 即可解，缺字节按 0）。
  - `pub const CMD_INPUT_STATE_REPORT: u16 = 0x0204;`
  - `pub struct InputStateReportPayload { pub pid: u32, pub disabled: u8, pub reason: u8, pub input_scope_mask: u64 }`；`SIZE = 14`；`from_bytes(&[u8]) -> Option<Self>`、`to_bytes() -> [u8; 14]`（LE）。
  - `pub fn decode_input_state_report(payload: &[u8]) -> Result<InputStateReportPayload, CodecError>`（codec.rs）。

- [ ] **Step 1: 写失败测试**

protocol.rs 末尾（或就近 tests 模块）新增：

```rust
#[cfg(test)]
mod input_diag_wire_tests {
    use super::*;

    #[test]
    fn focus_gained_backward_compat_36_bytes() {
        // 旧 36 字节载荷（无 disabled/reason）仍可解，新字段默认 0
        let mut buf = vec![0u8; 36];
        buf[20..28].copy_from_slice(&7u64.to_le_bytes()); // client_token
        buf[28..36].copy_from_slice(&(1u64 << 31).to_le_bytes()); // input_scope_mask
        let p = FocusGainedPayload::from_bytes(&buf).unwrap();
        assert_eq!(p.client_token, 7);
        assert_eq!(p.input_scope_mask, 1 << 31);
        assert_eq!(p.disabled, 0);
        assert_eq!(p.reason, 0);
    }

    #[test]
    fn focus_gained_reads_new_fields_38_bytes() {
        let mut buf = vec![0u8; 38];
        buf[36] = 1; // disabled
        buf[37] = 2; // reason
        let p = FocusGainedPayload::from_bytes(&buf).unwrap();
        assert_eq!(p.disabled, 1);
        assert_eq!(p.reason, 2);
    }

    #[test]
    fn input_state_report_roundtrip() {
        let r = InputStateReportPayload { pid: 4242, disabled: 1, reason: 1, input_scope_mask: 1 << 31 };
        let bytes = r.to_bytes();
        assert_eq!(bytes.len(), InputStateReportPayload::SIZE);
        let d = InputStateReportPayload::from_bytes(&bytes).unwrap();
        assert_eq!(d.pid, 4242);
        assert_eq!(d.disabled, 1);
        assert_eq!(d.reason, 1);
        assert_eq!(d.input_scope_mask, 1 << 31);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p wind-ipc input_diag_wire`
Expected: 编译失败（`disabled`/`reason`/`InputStateReportPayload` 未定义）。

- [ ] **Step 3: 写最小实现**

protocol.rs 焦点区常量旁新增：

```rust
pub const CMD_INPUT_STATE_REPORT: u16 = 0x0204;
```

`FocusGainedPayload` 结构体加字段并改 SIZE + from_bytes：

```rust
/// 焦点获取载荷 (38 bytes：旧 36 + disabled(1) + reason(1))
#[derive(Clone, Copy, Debug)]
pub struct FocusGainedPayload {
    pub caret: CaretPayload,
    pub client_token: u64,
    pub input_scope_mask: u64,
    pub disabled: u8,
    pub reason: u8,
}

impl FocusGainedPayload {
    pub const SIZE: usize = 38;

    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        // 向后兼容：至少要有旧 36 字节；disabled/reason 缺省 0
        if buf.len() < 36 {
            return None;
        }
        let caret = CaretPayload::from_bytes(&buf[0..20])?;
        let client_token = u64::from_le_bytes(buf[20..28].try_into().ok()?);
        let input_scope_mask = u64::from_le_bytes(buf[28..36].try_into().ok()?);
        let disabled = if buf.len() >= 37 { buf[36] } else { 0 };
        let reason = if buf.len() >= 38 { buf[37] } else { 0 };
        Some(Self { caret, client_token, input_scope_mask, disabled, reason })
    }
}
```

> 若原 `FocusGainedPayload` 有 `to_bytes`，同步补写 38 字节版本（末尾 append disabled/reason）。若无（仅解码用）可略。

新增 `InputStateReportPayload`：

```rust
/// compartment 变更时的最新输入态上报载荷 (14 bytes)
#[derive(Clone, Copy, Debug)]
pub struct InputStateReportPayload {
    pub pid: u32,
    pub disabled: u8,
    pub reason: u8,
    pub input_scope_mask: u64,
}

impl InputStateReportPayload {
    pub const SIZE: usize = 14;

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..4].copy_from_slice(&self.pid.to_le_bytes());
        b[4] = self.disabled;
        b[5] = self.reason;
        b[6..14].copy_from_slice(&self.input_scope_mask.to_le_bytes());
        b
    }

    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE {
            return None;
        }
        Some(Self {
            pid: u32::from_le_bytes(buf[0..4].try_into().ok()?),
            disabled: buf[4],
            reason: buf[5],
            input_scope_mask: u64::from_le_bytes(buf[6..14].try_into().ok()?),
        })
    }
}
```

codec.rs 在 `decode_focus_gained` 旁新增：

```rust
/// 从载荷字节解码 InputStateReportPayload（CMD_INPUT_STATE_REPORT 0x0204）
pub fn decode_input_state_report(payload: &[u8]) -> Result<InputStateReportPayload, CodecError> {
    InputStateReportPayload::from_bytes(payload).ok_or(CodecError::BufferTooShort {
        need: InputStateReportPayload::SIZE,
        got: payload.len(),
    })
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p wind-ipc`
Expected: PASS（含新 3 测试 + 原有全绿）。

- [ ] **Step 5: 提交**

```bash
git add wind_input/crates/wind-ipc/src/protocol.rs wind_input/crates/wind-ipc/src/codec.rs
git commit -m "feat(ipc): focus_gained 载荷加 disabled/reason + CMD_INPUT_STATE_REPORT"
```

---

## Task 3: wind-bridge 传递新字段 + 分发新命令

**Files:**
- Modify: `wind_input/crates/wind-bridge/src/handler.rs`
- Modify: `wind_input/crates/wind-bridge/src/server.rs`（Windows）
- Modify: `wind_input/crates/wind-bridge/src/server_unix.rs`

**Interfaces:**
- Consumes: `FocusGainedPayload.disabled/reason`、`decode_input_state_report`、`CMD_INPUT_STATE_REPORT`。
- Produces:
  - `FocusData` 追加 `pub disabled: bool, pub reason: u8`。
  - Handler trait 新增 `fn handle_input_state_report(&self, pid: u32, disabled: bool, reason: u8, mask: u64) {}`（默认空实现，保持其它 impl 不破）。

- [ ] **Step 1: 扩展 FocusData**

`handler.rs` 结构体末尾加字段：

```rust
    pub input_scope_mask: u64,
    pub disabled: bool,
    pub reason: u8,
}
```

Handler trait（`handler.rs` 内 `pub trait Handler`）新增默认方法：

```rust
    /// compartment 禁用态变更（不换焦点）上报。默认空实现。
    fn handle_input_state_report(&self, _pid: u32, _disabled: bool, _reason: u8, _mask: u64) {}
```

- [ ] **Step 2: server.rs 填字段 + 分发**

`server.rs` 的 `FocusData { … }` 构造补：

```rust
                input_scope_mask: fg.input_scope_mask,
                disabled: fg.disabled != 0,
                reason: fg.reason,
            };
```

在 `if cmd == CMD_FOCUS_GAINED …` 分支之后，仿其结构新增：

```rust
        if cmd == CMD_INPUT_STATE_REPORT
            && let Ok(r) = decode_input_state_report(payload)
        {
            handler.handle_input_state_report(r.pid, r.disabled != 0, r.reason, r.input_scope_mask);
        }
```

`server_unix.rs` 同步补 `FocusData` 的三字段（disabled/reason 缺省来自 payload；Unix 端不接 CMD_INPUT_STATE_REPORT，仅保证编译）。

> 需要 `use wind_ipc::{decode_input_state_report, protocol::CMD_INPUT_STATE_REPORT};`（照现有 import 风格）。

- [ ] **Step 3: 编译验证**

Run: `cargo build -p wind-bridge`
Expected: 通过（无未用告警可忽略）。

- [ ] **Step 4: 提交**

```bash
git add wind_input/crates/wind-bridge/src/handler.rs wind_input/crates/wind-bridge/src/server.rs wind_input/crates/wind-bridge/src/server_unix.rs
git commit -m "feat(bridge): FocusData 传 disabled/reason + 分发 CMD_INPUT_STATE_REPORT"
```

---

## Task 4: coordinator 存储 last_input_diag + 密码框抑制接线

**Files:**
- Modify: `wind_input/crates/wind-coordinator/src/coordinator.rs`
- Test: coordinator.rs `#[cfg(test)]`（复用已有 headless 构造 `Coordinator::build*` 测试辅助）

**Interfaces:**
- Consumes: Task1 `input_diag::{InputDiagState, InputDiagReason, reason_from, is_password_scope}`；Task3 `Handler::handle_input_state_report`；`FocusData.disabled/reason`。
- Produces（`impl Coordinator`）：
  - 字段：`last_input_diag: Mutex<InputDiagState>`、`password_suppress: AtomicBool`、`password_suppress_enabled: AtomicBool`(默认 true)、`input_diag_hud_visible: AtomicBool`(默认 false)。
  - `pub(crate) fn effective_chinese(&self, s: &InputState) -> bool`。
  - `pub(crate) fn apply_input_diag(&self, pid: u32, disabled: bool, reason: u8, mask: u64)`。
  - `pub(crate) fn password_suppress_enabled(&self) -> bool` / `input_diag_hud_visible(&self) -> bool`。

- [ ] **Step 1: 写失败测试**

用已有 headless 构造（`Coordinator::build_for_test` / `build_headless` 之一，照 coordinator.rs 现有测试用法）：

```rust
#[test]
fn password_scope_sets_suppress_and_state() {
    let c = test_coordinator(); // 复用本文件已有的测试构造 helper
    c.apply_input_diag(1234, false, /*reason*/2, 1 << 31);
    assert!(c.password_suppress.load(std::sync::atomic::Ordering::Relaxed));
    let d = c.last_input_diag.lock().unwrap();
    assert_eq!(d.reason, InputDiagReason::InputScopePassword);
    assert_eq!(d.pid, 1234);
}

#[test]
fn suppress_cleared_when_mask_clears() {
    let c = test_coordinator();
    c.apply_input_diag(1, false, 2, 1 << 31);
    c.apply_input_diag(1, false, 0, 0);
    assert!(!c.password_suppress.load(std::sync::atomic::Ordering::Relaxed));
}

#[test]
fn disabled_policy_no_suppress_when_off() {
    let c = test_coordinator();
    c.password_suppress_enabled.store(false, std::sync::atomic::Ordering::Relaxed);
    c.apply_input_diag(1, false, 2, 1 << 31);
    assert!(!c.password_suppress.load(std::sync::atomic::Ordering::Relaxed));
}
```

> 若本文件尚无 `test_coordinator()` helper，用现有测试里实际的构造方式替换（如 `Coordinator::build_headless(...)`）；不要新造与既有不一致的构造。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p wind-coordinator apply_input_diag`（或按测试名）
Expected: 编译失败（字段/方法未定义）。

- [ ] **Step 3: 写最小实现**

`Coordinator` 结构体加字段（初始化处 `Coordinator::build` 内一并补默认）：

```rust
    pub(crate) last_input_diag: std::sync::Mutex<crate::input_diag::InputDiagState>,
    pub(crate) password_suppress: std::sync::atomic::AtomicBool,
    pub(crate) password_suppress_enabled: std::sync::atomic::AtomicBool,
    pub(crate) input_diag_hud_visible: std::sync::atomic::AtomicBool,
```

`build` 初始化：

```rust
            last_input_diag: std::sync::Mutex::new(Default::default()),
            password_suppress: std::sync::atomic::AtomicBool::new(false),
            password_suppress_enabled: std::sync::atomic::AtomicBool::new(true),
            input_diag_hud_visible: std::sync::atomic::AtomicBool::new(false),
```

核心方法：

```rust
    pub(crate) fn apply_input_diag(&self, pid: u32, disabled: bool, reason_byte: u8, mask: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let reason = crate::input_diag::reason_from(disabled, mask);
        let name = if pid != 0 { self.cached_proc_name((pid as u64) << 32) } else { String::new() };
        // 抑制：命中密码位 且 开关开 → 强制英文
        let suppress = crate::input_diag::is_password_scope(mask)
            && self.password_suppress_enabled.load(Relaxed);
        self.password_suppress.store(suppress, Relaxed);
        {
            let mut d = self.last_input_diag.lock().unwrap_or_else(|e| e.into_inner());
            *d = crate::input_diag::InputDiagState {
                pid,
                process_name: name,
                disabled,
                reason,
                mask,
            };
        }
        self.push_input_diag_hud_if_visible();
    }

    pub(crate) fn effective_chinese(&self, s: &InputState) -> bool {
        s.chinese_mode
            && !s.caps_lock
            && !self.password_suppress.load(std::sync::atomic::Ordering::Relaxed)
    }
```

`handle_focus_gained`（coordinator.rs:3977）在末尾 `Some(status)` 前加：

```rust
        let pid = (data.client_token >> 32) as u32;
        self.apply_input_diag(pid, data.disabled, data.reason, data.input_scope_mask);
```

实现 Handler 上报回调（coordinator 的 `impl Handler`）：

```rust
    fn handle_input_state_report(&self, pid: u32, disabled: bool, reason: u8, mask: u64) {
        self.apply_input_diag(pid, disabled, reason, mask);
    }
```

抑制消费——把三处 `chinese_mode && !caps_lock`（coordinator.rs:2270 / 2556 / 2895）替换为 `self.effective_chinese(&state)`（就地对应的 state 变量名）。逐处确认上下文能拿到 `&state`；拿不到的用 `s.chinese_mode && !s.caps_lock && !self.password_suppress.load(Relaxed)` 内联。

> `push_input_diag_hud_if_visible` 在 Task 6 定义；本 Task 先留一个空私有方法占位（`fn push_input_diag_hud_if_visible(&self) {}`），Task 6 填实现，避免跨 Task 编译断裂。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p wind-coordinator`
Expected: PASS（新 3 测试 + 原有全绿）。特别核对既有依赖 `chinese_mode && !caps_lock` 的测试未回归。

- [ ] **Step 5: 提交**

```bash
git add wind_input/crates/wind-coordinator/src/coordinator.rs
git commit -m "feat(coordinator): last_input_diag 存储 + 密码框强制英文抑制"
```

---

## Task 5: 右键菜单「高级」两个开关

**Files:**
- Modify: `wind_input/crates/wind-ui/src/manager.rs`（MenuCmd + to_menu_id）
- Modify: `wind_input/crates/wind-coordinator/src/handle_menu.rs`
- Test: coordinator.rs / handle_menu 相关 `#[cfg(test)]`（切换语义单测）

**Interfaces:**
- Consumes: Task4 的 `input_diag_hud_visible` / `password_suppress_enabled`。
- Produces:
  - `MenuCmd::ToggleInputDiagnostics`、`MenuCmd::TogglePasswordSuppress`；`to_menu_id` 各给 100-199 区间未用 id（如 `ToggleInputDiagnostics => 120`、`TogglePasswordSuppress => 121`——先读 `to_menu_id` 现有 MenuCmd 映射确认不冲突）。
  - coordinator：`pub(crate) fn toggle_input_diag_hud(&self)`、`pub(crate) fn toggle_password_suppress(&self)`。

- [ ] **Step 1: 写失败测试（切换语义）**

coordinator.rs tests：

```rust
#[test]
fn toggle_hud_flips_visibility() {
    use std::sync::atomic::Ordering::Relaxed;
    let c = test_coordinator();
    assert!(!c.input_diag_hud_visible.load(Relaxed));
    c.toggle_input_diag_hud();
    assert!(c.input_diag_hud_visible.load(Relaxed));
    c.toggle_input_diag_hud();
    assert!(!c.input_diag_hud_visible.load(Relaxed));
}

#[test]
fn toggle_password_suppress_flips_enabled() {
    use std::sync::atomic::Ordering::Relaxed;
    let c = test_coordinator();
    assert!(c.password_suppress_enabled.load(Relaxed)); // 默认开
    c.toggle_password_suppress();
    assert!(!c.password_suppress_enabled.load(Relaxed));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p wind-coordinator toggle_`
Expected: 编译失败（方法未定义）。

- [ ] **Step 3: 实现**

manager.rs 的 `MenuCmd` 末尾加：

```rust
    /// 切换输入诊断 HUD 显隐（高级菜单）
    ToggleInputDiagnostics,
    /// 切换密码框强制英文（高级菜单，临时测试入口）
    TogglePasswordSuppress,
```

`to_menu_id` 的 MenuCmd 分支加两条（用确认未冲突的 id）：

```rust
            MenuCmd::ToggleInputDiagnostics => 120,
            MenuCmd::TogglePasswordSuppress => 121,
```

coordinator（handle_menu.rs 的 `impl Coordinator`）加切换方法：

```rust
    pub(crate) fn toggle_input_diag_hud(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let now = !self.input_diag_hud_visible.load(Relaxed);
        self.input_diag_hud_visible.store(now, Relaxed);
        if now {
            self.push_input_diag_hud_if_visible();
        } else {
            let _ = self.ui_tx.send(wind_ui::manager::UiCommand::HideInputDiag);
        }
    }

    pub(crate) fn toggle_password_suppress(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let now = !self.password_suppress_enabled.load(Relaxed);
        self.password_suppress_enabled.store(now, Relaxed);
        // 关闭抑制时立即解除当前生效的强制英文
        if !now {
            self.password_suppress.store(false, Relaxed);
        }
    }
```

`run_menu_cmd`（handle_menu.rs:32）加两分支：

```rust
            MenuCmd::ToggleInputDiagnostics => self.toggle_input_diag_hud(),
            MenuCmd::TogglePasswordSuppress => self.toggle_password_suppress(),
```

`build_main_menu_items` 的 `advanced_children`（handle_menu.rs:297）追加两项（含勾选态）：

```rust
            M::separator(),
            M::leaf(
                "输入诊断 HUD",
                cmd(MenuCmd::ToggleInputDiagnostics),
                true,
                self.input_diag_hud_visible.load(std::sync::atomic::Ordering::Relaxed),
            ),
            M::leaf(
                "密码框强制英文",
                cmd(MenuCmd::TogglePasswordSuppress),
                true,
                self.password_suppress_enabled.load(std::sync::atomic::Ordering::Relaxed),
            ),
```

> `UiCommand::HideInputDiag` 在 Task 6 定义。为不阻塞本 Task 编译，可在 Task 6 之前先在 manager.rs 加占位空变体 `HideInputDiag`（无字段），Task 6 补 `ShowInputDiag`。建议实现顺序：先做 Task 6 的 manager.rs 变体声明，再回 Task 5 接线；或将本 Task 的 `ui_tx.send(...HideInputDiag)` 一行与 Task 6 合并提交。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p wind-coordinator toggle_ && cargo build -p wind-ui`
Expected: PASS + 编译通过。

- [ ] **Step 5: 提交**

```bash
git add wind_input/crates/wind-ui/src/manager.rs wind_input/crates/wind-coordinator/src/handle_menu.rs
git commit -m "feat(menu): 高级子菜单加输入诊断 HUD + 密码框强制英文开关"
```

---

## Task 6: HUD 浮窗 InputDiagHud + UiCommand 接线

**Files:**
- Modify: `wind_input/crates/wind-ui/src/manager.rs`（`UiCommand::ShowInputDiag/HideInputDiag` + `InputDiagView`；dispatch）
- Create: `wind_input/crates/wind-ui/src/input_diag_hud.rs`
- Modify: `wind_input/crates/wind-ui/src/lib.rs`（`mod input_diag_hud;`）
- Modify: `wind_input/crates/wind-coordinator/src/coordinator.rs`（`push_input_diag_hud_if_visible` 实体）
- Test: `input_diag_hud.rs` 内纯格式化函数单测

**Interfaces:**
- Consumes: `ui_tx: Sender<UiCommand>`；Task4 `last_input_diag` / `input_diag_hud_visible`；`LayeredWindow`、`TextRenderer`、`View`（仿 status_tip.rs）。
- Produces:
  - `pub struct InputDiagView { pub process_name: String, pub pid: u32, pub disabled: bool, pub reason_text: String, pub mask: u64 }`
  - `UiCommand::ShowInputDiag(InputDiagView)`、`UiCommand::HideInputDiag`
  - `pub fn format_diag_lines(v: &InputDiagView) -> Vec<String>`（纯函数，可单测）
  - `InputDiagHud`（窗口对象，`new/show/hide/update`）

- [ ] **Step 1: 写失败测试（纯格式化）**

`input_diag_hud.rs`：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn format_lines_shape() {
        let v = InputDiagView {
            process_name: "chrome.exe".into(),
            pid: 4242,
            disabled: true,
            reason_text: "compartment".into(),
            mask: 1 << 31,
        };
        let lines = format_diag_lines(&v);
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("chrome.exe"));
        assert!(lines[0].contains("4242"));
        assert!(lines[1].contains("是")); // 禁用态: 是
        assert!(lines[2].contains("compartment"));
        assert!(lines[3].contains("0x")); // mask 十六进制
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p wind-ui format_lines`
Expected: 编译失败。

- [ ] **Step 3: 实现纯逻辑 + 窗口**

manager.rs 加 `UiCommand` 变体与视图：

```rust
    /// 显示/更新输入诊断 HUD（右键「高级」开）
    ShowInputDiag(crate::input_diag_hud::InputDiagView),
    /// 隐藏输入诊断 HUD
    HideInputDiag,
```

`input_diag_hud.rs`（纯逻辑 + 窗口骨架，窗口部分仿 `status_tip.rs`）：

```rust
//! 输入诊断 HUD：非激活置顶浮窗，右键「高级」开关控制显隐，可拖动，双击复制。

use crate::window::LayeredWindow;
use crate::text::dwrite::TextRenderer;

#[derive(Clone, Debug)]
pub struct InputDiagView {
    pub process_name: String,
    pub pid: u32,
    pub disabled: bool,
    pub reason_text: String,
    pub mask: u64,
}

/// 纯格式化：4 行诊断文本（可单测）。
pub fn format_diag_lines(v: &InputDiagView) -> Vec<String> {
    let name = if v.process_name.is_empty() { "(未知)" } else { &v.process_name };
    vec![
        format!("{} ({})", name, v.pid),
        format!("禁用态: {}", if v.disabled { "是" } else { "否" }),
        format!("原因: {}", v.reason_text),
        format!("InputScope: 0x{:X}", v.mask),
    ]
}

pub struct InputDiagHud {
    window: LayeredWindow,
    renderer: TextRenderer,
    // …仿 status_tip.rs：scale/bg/fg/theme 等
}

impl InputDiagHud {
    pub fn new() -> Result<Self, String> {
        // 仿 StatusTip::new，窗口类名 "WindInputDiagHud"；创建后追加 WS_EX_NOACTIVATE。
        // 详见下方「窗口实现要点」。
        todo!("窗口构造：仿 status_tip.rs，见要点")
    }
    pub fn show_or_update(&mut self, v: &InputDiagView) { /* 渲染 format_diag_lines(v) 并 show */ }
    pub fn hide(&mut self) { /* self.window.hide() */ }
}
```

**窗口实现要点（非单测，真机验证）：**
1. 构造仿 `status_tip.rs::StatusTip::new`：`LayeredWindow::create(None, W, H, "WindInputDiagHud")` + `TextRenderer`。
2. 创建后对窗口句柄 `GetWindowLongPtr/SetWindowLongPtr` 追加 `WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_TOOLWINDOW`（不进任务栏、不抢焦点）。
3. 初始位置右下角（`GetSystemMetrics(SM_CXSCREEN/CYSCREEN)` - 尺寸 - 边距）。
4. 拖动：窗口过程处理 `WM_LBUTTONDOWN` → 记录起点并 `SetCapture`，`WM_MOUSEMOVE`（按下时）`SetWindowPos` 跟随，`WM_LBUTTONUP` → `ReleaseCapture`。因窗口 `WS_EX_NOACTIVATE`，点击不激活、不夺焦。
5. 双击 `WM_LBUTTONDBLCLK` → 把 `format_diag_lines` 结果 `\n` 连接写剪贴板（复用 `popup_menu::try_set_clipboard_text`）。
6. 渲染：4 行文本 + 深色半透明圆角底，字号/主题解析仿 `status_tip.rs`（可后续接主题，MVP 用其默认 bg/fg）。

manager.rs 的 UiCommand dispatch（UiManager 处理循环，仿 `ShowStatusTip`/`HideStatusTip` 分支）：

```rust
    UiCommand::ShowInputDiag(v) => {
        // 惰性创建 self.input_diag_hud，然后 show_or_update(&v)
    }
    UiCommand::HideInputDiag => {
        if let Some(h) = self.input_diag_hud.as_mut() { h.hide(); }
    }
```

coordinator（coordinator.rs）把 Task4 占位的空方法替换为实体：

```rust
    fn push_input_diag_hud_if_visible(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        if !self.input_diag_hud_visible.load(Relaxed) {
            return;
        }
        let d = self.last_input_diag.lock().unwrap_or_else(|e| e.into_inner());
        let view = wind_ui::manager::InputDiagView {
            process_name: d.process_name.clone(),
            pid: d.pid,
            disabled: d.disabled,
            reason_text: crate::input_diag::reason_label(d.reason).to_string(),
            mask: d.mask,
        };
        let _ = self.ui_tx.send(wind_ui::manager::UiCommand::ShowInputDiag(view));
    }
```

> `InputDiagView` 从 `input_diag_hud` re-export 到 `manager`（`pub use crate::input_diag_hud::InputDiagView;`），使 coordinator 用 `wind_ui::manager::InputDiagView` 一致。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p wind-ui format_lines && cargo build -p wind-ui && cargo build -p wind-coordinator`
Expected: 纯逻辑测试 PASS + 编译通过（`todo!` 在非测试路径不触发编译错误；若窗口构造未完成会 panic，仅真机运行时相关，单测不走该路径）。

> 实现窗口体（替换 `todo!`）后，`cargo build -p wind-service`（或主服务包）确保集成编译。

- [ ] **Step 5: 提交**

```bash
git add wind_input/crates/wind-ui/src/input_diag_hud.rs wind_input/crates/wind-ui/src/manager.rs wind_input/crates/wind-ui/src/lib.rs wind_input/crates/wind-coordinator/src/coordinator.rs
git commit -m "feat(ui): 输入诊断 HUD 浮窗（非激活可拖动，双击复制）"
```

---

## Task 7: C++ TSF 上报（focus 扩展 + compartment 上报）

**Files:**
- Modify: `wind_tsf/src/BinaryProtocol.h`（`CMD_INPUT_STATE_REPORT 0x0204`；focus_gained 编码 +2 字节）
- Modify: `wind_tsf/src/IPCClient.cpp` / `.h`（`SendFocusGained` 补 disabled/reason；新增 `SendInputStateReport`）
- Modify: `wind_tsf/src/TextService.cpp`（`OnSetFocus` 上报补字段；`KEYBOARD_DISABLED` compartment sink 回调调用 `SendInputStateReport`）

**Interfaces:**
- Consumes: 已有 `_QueryInputScopeMask()`、`_IsFocusKeyboardDisabled()`、`_bKeyboardDisabled`、compartment sink（`_dwKeyboardDisabledSinkCookie`）。
- Produces（wire，与 Rust Task2 对齐）：
  - focus_gained 载荷尾部 append `disabled(1B) + reason(1B)`（总 38B）。
  - `CMD_INPUT_STATE_REPORT = 0x0204`，载荷 `pid(u32 LE) + disabled(1) + reason(1) + input_scope_mask(u64 LE)`（14B）。

- [ ] **Step 1: 协议常量 + reason 计算 helper**

`BinaryProtocol.h` 加 `constexpr uint16_t CMD_INPUT_STATE_REPORT = 0x0204;`。

TextService.cpp 加一个内联 helper（与 Rust `reason_from` 同语义）：

```cpp
// reason: 0 None / 1 CompartmentDisabled / 2 InputScopePassword / 3 NumericPassword
static uint8_t ComputeInputReason(bool disabled, uint64_t mask) {
    if (disabled) return 1;
    if (mask & (1ull << 63)) return 3; // IS_NUMERIC_PASSWORD
    if (mask & (1ull << 31)) return 2; // IS_PASSWORD
    return 0;
}
```

- [ ] **Step 2: focus_gained 编码补 2 字节**

`IPCClient.cpp` 的 `SendFocusGained`（或等价编码处）在原 36 字节末尾 append `disabled`、`reason` 两字节；签名加 `bool disabled, uint8_t reason`。`TextService.cpp::OnSetFocus` 调用点：先算 `mask = _QueryInputScopeMask()`，`disabled = _IsFocusKeyboardDisabled()`，`reason = ComputeInputReason(disabled, mask)`，传入。

- [ ] **Step 3: compartment 变更上报**

新增 `IPCClient::SendInputStateReport(uint32_t pid, bool disabled, uint8_t reason, uint64_t mask)`，按 14 字节 LE 编码，命令 `CMD_INPUT_STATE_REPORT`（异步 push，仿现有 async 上报）。

在 `KEYBOARD_DISABLED` compartment sink 回调（`TextService.cpp` 更新 `_bKeyboardDisabled` 处）之后调用：取当前前台/焦点 pid、`disabled = _bKeyboardDisabled`、`mask = _QueryInputScopeMask()`、`reason = ComputeInputReason(...)`，`SendInputStateReport(...)`。

- [ ] **Step 4: 构建 DLL**

Run: 项目既有 C++ 构建（`scripts/dev.ps1` 或 CMake 目标；参照 AGENTS.md）。
Expected: 编译链接通过。

> 注意 memory：Rust 侧 `clang-19` 未链接会阻断集成构建；C++ 单独走 cl/CMake。构建命令以本机既有脚本为准。

- [ ] **Step 5: 提交**

```bash
git add wind_tsf/src/BinaryProtocol.h wind_tsf/src/IPCClient.cpp wind_tsf/src/IPCClient.h wind_tsf/src/TextService.cpp
git commit -m "feat(tsf): 上报禁用态（focus 扩展 disabled/reason + compartment 变更上报）"
```

---

## 真机手测清单（Task 7 后，整体验证）

- [ ] Chromium 密码框：HUD 显示 `原因: compartment`，键放行；离开恢复。
- [ ] 普通文本框：HUD `原因: 无`，中文正常。
- [ ] 复现「无法输入」应用：HUD 现场显示禁用态与原因，判定「应用侧 vs 我方」。
- [ ] 网页内点密码框（不换窗）：compartment 上报即时刷新 HUD（验证 CMD_INPUT_STATE_REPORT 链路）。
- [ ] InputScope=密码但未 compartment 禁用：`密码框强制英文`=开 → 强制英文；「高级」关闭该开关 → 不再强制。
- [ ] HUD 拖动改位置不抢焦点；双击复制诊断文本到剪贴板。
- [ ] 「高级」菜单两项勾选态随开关正确反映。

## 未纳入本计划（后续）

- 把「密码框强制英文」从「高级」菜单迁至设置程序（`wind-setting` 独立仓）作为用户可见选项。
- HUD 主题化 / 位置持久化（当前会话级、固定初始右下）。

---

## Self-Review

**Spec coverage：** 设计 A（数据链路）→Task2/3/7；B（状态存储）→Task4；C（HUD）→Task6；D（抑制）→Task4；E（菜单开关）→Task5；测试节→各 Task 单测 + 手测清单。全覆盖。

**Placeholder scan：** 窗口体与 C++ 编码用「实现要点」列出确切 Win32 调用与字节布局，非空泛占位；`todo!` 仅限窗口构造真机路径且单测不触达，已标注。

**Type consistency：** `InputDiagReason`/`reason_from`/`is_password_scope`/`reason_label`（Task1）在 Task4/6 一致引用；`InputDiagView`/`format_diag_lines`（Task6）与 coordinator 下发一致；wire 字段顺序 focus(+disabled,+reason) 与 report(pid,disabled,reason,mask) 在 Rust Task2 与 C++ Task7 双向对齐；`effective_chinese` 命名在 Task4 定义、抑制消费处引用一致。
