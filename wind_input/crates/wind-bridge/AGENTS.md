<!-- Parent: ../../AGENTS.md -->
<!-- Updated: 2026-06-29 -->

# wind-bridge

## Purpose
Named Pipe 服务器 + Push 管道，实现 Rust 服务进程与 C++ wind_tsf TSF DLL 之间的双向 IPC 通信（Windows 专属）。定义 `MessageHandler` trait 供 wind-coordinator 实现，通过 `DeferredHandler` 在初始化期间对 DLL 返回安全默认值，通过 `PushServer` 主动向 DLL 推送状态变更。

## Key Files
| File | Description |
|------|-------------|
| `src/handler.rs` | `MessageHandler` trait：全部命令处理接口（按键/焦点/IME 激活/模式/提交/光标/Host Render 等）+ 数据类型（`KeyEventData`/`KeyAction`/`StatusUpdateData`/`FocusData`/`CaretData` 等）；`KeyAction::with_composition_placeholder`（非 app_inline preedit 占位） |
| `src/server.rs` | `BridgeServer` + `dispatch_command`：命名管道主循环（每连接独立线程）、全部命令分发逻辑（含 FOCUS_GAINED 两段式、批处理事件展开）、`encode_key_action` 编码 |
| `src/push.rs` | `PushServer`：推送管道服务器（单写者 writer 线程）、`push_to_active`（幂等广播）vs `push_commit_to_active`（有副作用的精准投递）、管道名格式锁定测试 |
| `src/deferred.rs` | `DeferredHandler`：初始化前安全代理（键事件返回 PassThrough、模式返回中文默认），`set_ready` 后切换到真实 handler；注意 `handle_key_event_policed` 须显式覆盖 |
| `src/security.rs` | SDDL 安全描述符封装（`D:P(A;;GA;;;WD)(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;AC)S:(ML;;NW;;;LW)`）：允许 AppContainer/UWP/低完整性进程连接管道 |
| `src/shared_memory.rs` | `SharedMemoryManager`：Host Render 共享内存段名/事件名生成（骨架，mmap 映射尚未实现） |

## For AI Agents

### Working In This Directory
- **新增命令须三处同步**：`handler.rs`（trait 方法定义）→ `server.rs`（`dispatch_command` match 分支）→ `deferred.rs`（`DeferredHandler` 转发实现）。漏掉 `deferred.rs` 会导致服务启动期间该命令被静默丢弃。
- **`DeferredHandler::handle_key_event_policed` 必须显式覆盖**，不得依赖 trait 默认实现（默认只调 `handle_key_event`）。`Coordinator` 重写的 `handle_key_event_policed` 含统计埋点和 preedit 占位后处理，若 `DeferredHandler` 走默认实现，就绪后这些逻辑仍被跳过，导致上屏统计恒为 0。新增类似"trait 默认实现外有额外逻辑"的方法时同理。
- **FOCUS_GAINED 两段式，同步段只做纯内存操作**：`dispatch_command` 同步段只调 `handle_caret_update`（纯字段写入）并立即回 `CMD_MODE_PUSH`（权威 chinese/full 模式，解除 DLL 在 OnSetFocus 内的阻塞，消除首键模式竞态）；`handle_focus_gained`（重型：build_status + push 完整激活状态）延后到 `handle_client` 写出响应后才执行。不得在同步段新增任何阻塞或跨进程调用。
- **同步路径禁止跨进程 Win32/Shell 调用**（铁律，已有事故）：`CmdKeyEvent`/`CmdToggleMode`/`CmdFocusGained` 同步段内，DLL 正在宿主进程 UI 线程上等响应（超时 ~1500ms）。调用 `SHQueryUserNotificationState`、`SendMessage`、`OpenProcess` 等会导致与 explorer/DWM 环形等待，外观为「任务栏/托盘卡顿约 1.5s」。正确做法：事件驱动缓存（ShellHook/WinEventHook 被动收）、或 spawn 独立线程异步执行后立即回 ACK。
- **`push_commit_to_active` vs `push_to_active` 不得互换**：前者有副作用（commit/上屏），必须精准投递给活动 token 匹配的客户端，避免多 DLL 实例各自上屏；后者用于幂等状态同步（激活状态/配置广播），可发给所有连接客户端。
- **推送管道命名格式**：`\\.\pipe\wind_input_push{suffix}`（suffix 如 `_dev`），与 TSF DLL `Globals.h` 一致；`push.rs` 有格式锁定测试 `test_push_pipe_name_suffix_position`。历史上曾因名称格式与 TSF 不一致导致 DLL 连不上 push 管道、热键白名单无法同步，改动时须对齐 TSF 侧常量。
- **`shared_memory.rs` 是未完成骨架**：仅实现 SHM 段名（`Local\WindInput_SHM{suffix}`）和事件名生成，mmap 映射未实现，Host Render 功能不完整。不要基于此文件假设 Host Render 已可用。

### Testing Requirements
- `src/push.rs` 的 `test_push_pipe_name_suffix_position` 无系统依赖，可在任意平台跑（纯字符串格式验证）。
- 涉及命名管道（`BridgeServer::start`、`PushServer::start`）的功能须在 Windows 设备或 CI（Windows runner）验证；`cfg(windows)` 块在非 Windows host 编译不参与。
- 协议变更须同步手工验证 C++ TSF DLL 侧（`wind_tsf/include/BinaryProtocol.h`），bridge 与 DLL 之间无自动 schema 校验。

## Dependencies

### Internal
- `wind-ipc`（IPC 协议编解码：`IpcHeader`/`codec::*`/`protocol::*`/`SharedRenderHeader` 等）

### External
- `tokio`（async runtime，`BridgeServer::start`/`PushServer::start` 为 async fn 入口）
- `bytes`、`tracing`、`anyhow`/`thiserror`
- `windows`（`cfg(windows)` gate：Named Pipe API、`ConvertStringSecurityDescriptorToSecurityDescriptorA`）

## 全局约束
- bridge handler 收到的提交文本/preedit 内容不得出现在 INFO 日志；见根 `AGENTS.md` 日志隐私规则。
- 协议帧格式（命令码/payload 布局）变更须同步 C++ `BinaryProtocol.h`（跨语言镜像约束）。
- 改完跑 `cargo fmt --package wind-bridge`。

<!-- MANUAL: 此行以下为人工补充区，重新生成时保留 -->
