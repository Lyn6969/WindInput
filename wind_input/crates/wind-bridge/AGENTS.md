<!-- Parent: ../../AGENTS.md -->
<!-- Updated: 2026-07-06 -->

# wind-bridge

## Purpose
Named Pipe 服务器 + Push 管道，实现 Rust 服务进程与 C++ wind_tsf TSF DLL 之间的双向 IPC 通信（Windows 专属）。定义 `MessageHandler` trait 供 wind-coordinator 实现，通过 `DeferredHandler` 在初始化期间对 DLL 返回安全默认值，通过 `PushServer` 主动向 DLL 推送状态变更。

## Key Files
| File | Description |
|------|-------------|
| `src/handler.rs` | `MessageHandler` trait：全部命令处理接口（按键/焦点/IME 激活/模式/提交/光标/Host Render/候选鼠标 select·hover·scroll·右键 等）+ 数据类型（`KeyEventData`/`KeyAction`/`StatusUpdateData`/`FocusData`/`CaretData` 等）；`KeyAction::with_composition_placeholder`（非 app_inline preedit 占位） |
| `src/server.rs` | `BridgeServer` + `dispatch_command`：命名管道主循环（每连接独立线程）、全部命令分发逻辑（含 FOCUS_GAINED 两段式、批处理事件展开）、`encode_key_action` 编码；`ClientCtx{conn_id,pid}` 连接级身份（host-render setup/note_focus/cleanup 用） |
| `src/push.rs` | `PushServer`：推送管道服务器（单写者 writer 线程）；三种投递语义——`push_to_active`（幂等广播，状态/配置同步）、`push_commit_to_active`（有副作用的活动客户端精准投递）、`push_to_token`（**按事件源精确投递、无兜底**，activation 状态位用）；`set_client_connected_hook`（客户端 token 握手后回调，host-render 重连补推握手用）；管道名格式锁定测试 |
| `src/deferred.rs` | `DeferredHandler`：初始化前安全代理（键事件返回 PassThrough、模式返回中文默认），`set_ready` 后切换到真实 handler；注意 `handle_key_event_policed` 须显式覆盖 |
| `src/security.rs` | SDDL 安全描述符封装（`D:P(A;;GA;;;WD)(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;AC)S:(ML;;NW;;;LW)`）：允许 AppContainer/UWP/低完整性进程连接管道；host-render 的 SHM/Event 复用同一 SDDL |
| **Host Render（Windows，`cfg(windows)`）** | 服务进程渲染候选/tooltip/status 位图 → 全局命名 SHM → 命名 Event 唤醒宿主内 `wind_tsf.dll` 的 Band 窗口贴图，解决 Win11 开始菜单（SearchHost.exe）候选窗被遮问题。C++ 端零改动。 |
| `src/host_render_windows.rs` | `HostRenderManager`：白名单判定、`setup`(clientID→三 kind SHM/Event)、`write_frame_for_kind`(写全局 SHM + 唤醒同 PID 全部实例)、`hide_kind`(hide 必达)、`cleanup_client`(SetupSeq 守卫)、`active_target`(已 setup 才 Some)、`is_process_whitelisted`。多实例隔离靠帧头 `target_instance_id` + `visible_owner` 单一真源 |
| `src/shared_memory_windows.rs` | `WindowsSharedMemory`：`CreateFileMappingW`（页面文件后备）+ `MapViewOfFile` 写端；`write_frame`/`write_hidden`，sequence 单调递增 |
| `src/named_event.rs` | `NamedEvent`：`CreateEventW`(auto-reset) + `SetEvent`，带 AppContainer SDDL |
| `src/shared_render_frame.rs` | 跨平台纯编帧逻辑（64B `SharedRenderHeader` + BGRA + hit-rect 表）：`encode_frame_into`/`encode_hidden_into`；POSIX 与 Windows 写端共用，Linux 可测 |
| `src/host_render_sink.rs` | `HostRenderSink` trait（macOS forwarder 推帧抽象，`PushServer` 实现之） |
| `src/{server_unix,push_unix,shared_memory_posix}.rs` | macOS/Linux 路径：UDS 请求/推送服务器 + POSIX SHM hostrender 写端（`cfg(unix)`） |

## For AI Agents

### Working In This Directory
- **新增命令须三处同步**：`handler.rs`（trait 方法定义）→ `server.rs`（`dispatch_command` match 分支）→ `deferred.rs`（`DeferredHandler` 转发实现）。漏掉 `deferred.rs` 会导致服务启动期间该命令被静默丢弃。
- **`DeferredHandler::handle_key_event_policed` 必须显式覆盖**，不得依赖 trait 默认实现（默认只调 `handle_key_event`）。`Coordinator` 重写的 `handle_key_event_policed` 含统计埋点和 preedit 占位后处理，若 `DeferredHandler` 走默认实现，就绪后这些逻辑仍被跳过，导致上屏统计恒为 0。新增类似"trait 默认实现外有额外逻辑"的方法时同理。
- **FOCUS_GAINED 两段式，同步段只做纯内存操作**：`dispatch_command` 同步段只调 `handle_caret_update`（纯字段写入）并立即回 `CMD_MODE_PUSH`（权威 chinese/full 模式，解除 DLL 在 OnSetFocus 内的阻塞，消除首键模式竞态）；`handle_focus_gained`（重型：build_status + push 完整激活状态）延后到 `handle_client` 写出响应后才执行。不得在同步段新增任何阻塞或跨进程调用。
- **同步路径禁止跨进程 Win32/Shell 调用**（铁律，已有事故）：`CmdKeyEvent`/`CmdToggleMode`/`CmdFocusGained` 同步段内，DLL 正在宿主进程 UI 线程上等响应（超时 ~1500ms）。调用 `SHQueryUserNotificationState`、`SendMessage`、`OpenProcess` 等会导致与 explorer/DWM 环形等待，外观为「任务栏/托盘卡顿约 1.5s」。正确做法：事件驱动缓存（ShellHook/WinEventHook 被动收）、或 spawn 独立线程异步执行后立即回 ACK。
- **三种 push 投递语义不得互换**：`push_commit_to_active`（有副作用如 commit/上屏，精准投给活动 token，避免多 DLL 实例各自上屏）、`push_to_active`（幂等状态/配置广播，发所有连接客户端）、`push_to_token`（**按事件源精确投递、命中失败即丢弃不兜底**）。**按事件源计算的帧必须走 `push_to_token`**：`activation status` 的 `hostRenderAvail` 位按触发者 PID 算，若用广播，开始菜单弹出时 StartMenuExperienceHost 等兄弟进程的激活推送（avail=0）会被 SearchHost 收到，触发 DLL Band 窗口销毁重建死循环（真机踩坑）。
- **推送管道命名格式**：`\\.\pipe\wind_input_push{suffix}`（suffix 如 `_dev`），与 TSF DLL `Globals.h` 一致；`push.rs` 有格式锁定测试 `test_push_pipe_name_suffix_position`。历史上曾因名称格式与 TSF 不一致导致 DLL 连不上 push 管道、热键白名单无法同步，改动时须对齐 TSF 侧常量。

### Host Render（已完整实现 + 真机验证，改动前必读）
- **架构**：全局 SHM per kind（Candidate/Tooltip/Status 三段，非 per-instance——用户拍板内存代价不可接受），帧头 `target_instance_id` 区分实例，`SetEvent` 唤醒同 PID 全部实例，只有匹配者显示。C++ `wind_tsf/src/HostWindow.cpp` 是权威解码端，**零改动**——协议/值域一律以它为准。
- **hide 必达 + `visible_owner` 单一真源**：所有隐藏路径（HideCandidates/HideTooltip/HideStatus/special 退出/断线 cleanup/Shutdown）统一收敛到 `hide_kind`，它只看 `visible_owner[kind]`、不查白名单/评估态；hide 帧 `target_instance_id=0`（clientID 从 1 起，0 恒不匹配 → 广播隐藏）。这是 Go 版「special 退出候选窗不隐藏」多实例 bug 的根治点，勿退化。
- **翻页/hover 三值域必须区分**（真机踩坑：翻页点击无效）：① Rust 内部 tag `HOVER_PAGE_PREV/NEXT = 100000/100001`；② SHM hit-rect 表 & `CMD_CANDIDATE_SELECT` 上行 = `-1 上页 / -2 下页`；③ `CMD_CANDIDATE_HOVER` 上行（因 hover 需独立的「无」）= `-1 无 / -2 上页 / -3 下页`。写帧时（`wind-ui/manager.rs`）必须把内部 tag 重映射为 SHM 值域，正数 tag 会被 C++ `_HitTest` 当候选索引 → `mouse_select(100000)` 被丢弃。
- **鼠标命令一律 i32**：`CMD_CANDIDATE_SELECT/HOVER/SCROLL`（0x020D/0x020E/0x0211）payload 是 i32，负值有语义。DLL 走 `SendAsync` 不读响应，dispatch 臂须 `if is_async { None }`（回 ack 会污染管道）。
- **0x0211 平台双语义**：Windows=`CMD_CANDIDATE_SCROLL`（Go/C++ 原始）；darwin=`CMD_FRONT_CONTEXT`（macOS 移植期误复用）。dispatch 已按 `cfg` 分臂，两端上行方向平台互斥；未来同平台需共用须迁移 FRONT_CONTEXT 并同步 Swift。
- **重连补推握手**（真机踩坑：服务重启后概率停留普通渲染）：SearchHost 这类 locked/transient DocMgr 宿主重连后**不发 focus_gained（DLL 跳过）也不重发 IME_ACTIVATED** → 永无 activation push → DLL 挂死 SHM 不重新 setup。解法 = `set_client_connected_hook` 在 push token 握手后回调，白名单 pid 定向补推 activation（avail=1）触发 C++ `_EnsureHostRenderSetup(forceRefresh)`。
- **键事件刷新焦点**：同类 transient 宿主二次聚焦时 focus_gained/IME_ACTIVATED 都缺席，`CMD_KEY_EVENT` 分发时 `note_focus` 是唯一可靠焦点信号，否则 `active_target` 滞留旧进程致回退本地渲染。
- **avail 位按事件源 PID**：`push_activation_status(client_token)` 用 `client_token >> 32`（PID）查白名单，不用全局焦点槽（会被兄弟进程污染）。
- **写帧失败必回退本地窗口**，不得静默丢帧（Go 经验）；无 host-render 目标时本地 LayeredWindow 路径零分支副作用。

### Testing Requirements
- `src/push.rs` 的 `test_push_pipe_name_suffix_position` 无系统依赖，可在任意平台跑（纯字符串格式验证）。
- 涉及命名管道（`BridgeServer::start`、`PushServer::start`）的功能须在 Windows 设备或 CI（Windows runner）验证；`cfg(windows)` 块在非 Windows host 编译不参与。
- 协议变更须同步手工验证 C++ TSF DLL 侧（`wind_tsf/include/BinaryProtocol.h`），bridge 与 DLL 之间无自动 schema 校验。

## Dependencies

### Internal
- `wind-ipc`（IPC 协议编解码：`IpcHeader`/`codec::*`/`protocol::*`/`SharedRenderHeader` 等）

### External
- `bytes`、`tracing`、`anyhow`/`thiserror`
- **无 async runtime**：IPC 为同步线程模型（每连接独立线程），`BridgeServer::start`/`PushServer::start` 均为同步 fn（tokio 已于 2026-07 移除，勿再引入）
- `windows`（`cfg(windows)` gate：Named Pipe API、`ConvertStringSecurityDescriptorToSecurityDescriptorA`）

## 全局约束
- bridge handler 收到的提交文本/preedit 内容不得出现在 INFO 日志；见根 `AGENTS.md` 日志隐私规则。
- 协议帧格式（命令码/payload 布局）变更须同步 C++ `BinaryProtocol.h`（跨语言镜像约束）。
- 改完跑 `cargo fmt --package wind-bridge`。

<!-- MANUAL: 此行以下为人工补充区，重新生成时保留 -->
