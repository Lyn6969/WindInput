# HostRender Windows 移植设计（Rust 服务侧）+ macOS W9 接线

- 分支：`worktree-host-render-rust`
- 对照基准（Go 版，同级 `../WindInput-Go`）：
  - `wind_input/internal/bridge/host_render.go` —— HostRenderManager / SharedMemory / NamedEvent
  - `wind_input/internal/bridge/shared_memory.go` —— Windows 共享内存写端（WriteFrame/WriteHide）
  - `wind_input/internal/coordinator/handle_lifecycle.go` —— 生命周期与 composition 终止竞态处理
  - `wind_input/internal/ipc/binary_protocol.go` —— `HostRenderSetupEntry` / `SharedRenderHeader`
  - `docs/archive/startmenu-zorder-solution.md` —— 完整设计史与已知遗留问题
- 相关本仓库文档：
  - `docs/superpowers/specs/2026-06-22-macos-w4-hostrender-design.md`（W4：render_frame 抽取 + macOS forwarder，已实施）
  - `docs/superpowers/plans/2026-07-02-macos-migration-gaps.md`（macOS 缺口清单，Group 1 = W9 接线）

## 1. 背景与目标

Win11 开始菜单 / 任务栏搜索（SearchHost.exe）的 TSF 环境高度受限：普通进程创建的
候选窗口无法盖过其 Band 层级，导致候选框不可见。Go 版通过 **HostRender**（DLL 代理渲染）
解决：服务进程渲染好完整位图 → 全局命名共享内存 → 命名 Event 唤醒宿主进程内的
`wind_tsf.dll` → DLL 用 `CreateWindowInBand` 创建的 Band 窗口 `UpdateLayeredWindow` 上屏。

Rust 重写后该链路的**服务进程侧尚未实现**。本设计目标：

1. 在 Rust 服务进程补齐 Windows HostRender（三种窗口 Candidate/Tooltip/Status 全量对齐 Go）。
2. 保持 **全局 SHM 架构**（用户决策：per-instance SHM 内存代价不可接受），在该架构内
   **根治** Go 版遗留的「单进程多实例致 special 模式退出后候选窗不隐藏」bug。
3. macOS 侧不改架构（W4 的 host-render forwarder 已实施），本轮一并规划 **W9 接线缺口**。
4. `wind_tsf` C++ 端（`HostWindow.cpp`）**零改动**。

## 2. 现状盘点（本仓库已就位 / 缺失）

| 组件 | 状态 |
|---|---|
| C++ `HostWindow.cpp`（CreateWindowInBand、band 降级、渲染线程、鼠标交互） | ✅ 已在仓库，零改动 |
| C++ `TextService::_EnsureHostRenderSetup` + `IPCClient::SendHostRenderRequest` | ✅ 已在仓库（由响应 `STATUS_HOST_RENDER_AVAIL` 位触发） |
| 协议常量 `CMD_HOST_RENDER_REQUEST/SETUP`（0x0501） | ✅ Rust/C++ 两侧均已定义 |
| `SharedRenderHeader` 64B 布局 | ✅ `wind-ipc/src/protocol.rs` 已定义（darwin 在用） |
| POSIX SHM 写端（`shared_memory_posix.rs`） | ✅ 已实施（macOS） |
| 窗口无关光栅化 `render_frame()`（candidate_window.rs） | ✅ W4 已抽取，直接复用 |
| bridge 路由 `CMD_HOST_RENDER_REQUEST`（server.rs:514） | ⚠️ 已路由但 handler 为空桩、**不回 setup payload** |
| `handle_host_render_request/ready`（handler.rs:222/225，coordinator.rs:4026） | ❌ 空桩，签名无 clientID/PID、无返回值 |
| push 状态位 `host_render_avail`（push.rs:169） | ❌ 恒 false（触发链总开关，DLL 永不发起 request） |
| Windows 命名共享内存写端 / 命名 Event | ❌ 缺失 |
| HostRenderManager（白名单/SetupSeq/多实例/清理） | ❌ 缺失 |
| UI 路径 host-render 分流 + 失败回退 | ❌ 缺失 |
| 配置 `compat.host_render_processes` | ✅ schema 已有；⚠️ Rust 默认空（Go 默认 `["SearchHost.exe"]`） |

## 3. 架构（Windows）

```
coordinator ──UiCommand──▶ wind-ui manager_windows（UiManager）
                               │  host-render 激活时（按目标实例分流）
                               ▼
                     candidate_window::render_frame()      ← W4 已有，复用
                               ▼
                wind-bridge HostRenderManager（新，Windows-only）
                   ├─ 全局 SHM per kind（懒建，3 段：SHM / SHM_TIP / SHM_STS）
                   ├─ 帧头 stamp targetInstanceID
                   └─ SetEvent 唤醒同 PID 全部实例
                               ▼
                wind_tsf HostWindow.cpp（零改动）
                   ├─ 匹配 targetInstanceID → UpdateLayeredWindow 显示
                   └─ 不匹配 / flags 无 VISIBLE → 隐藏
```

### 3.1 命名约定（含 suffix，支持 dev/prod 并行）

对齐 Go 并叠加本仓库的 endpoint suffix 机制（名字经 `CMD_HOST_RENDER_SETUP` 下发，
C++ 端只按名字打开，因此服务端可自由命名）：

- SHM：`Local\WindInput_SHM{suffix}`、`..._TIP`、`..._STS`（全局，per kind 单段，4MB = `MAX_SHARED_RENDER_SIZE`）
- Event：`Local\WindInput_EVT{suffix}_C{clientID}`、`..._TIP`、`..._STS`（per instance 私有）

### 3.2 新增 / 改动模块

| 文件 | 动作 |
|---|---|
| `wind-bridge/src/shared_render_frame.rs`（新，跨平台） | 从 `shared_memory_posix.rs` 抽出 64B header + BGRA + hit-rect 表的**纯编帧逻辑**（`write_frame_into(&mut [u8], …)` / `write_hidden_into`），POSIX 与 Windows 写端共用；Linux 可测 |
| `wind-bridge/src/shared_memory_windows.rs`（新，cfg windows） | `CreateFileMappingW`（页面文件后备）+ `MapViewOfFile` 写端；`write_frame(img, x, y, rects, hover, target_instance_id)` / `write_hidden()`，内部调用公共编帧逻辑；`sequence` 单调递增 |
| `wind-bridge/src/named_event.rs`（新，cfg windows） | `CreateEventW`（auto-reset）/ `SetEvent` / `Close` 封装 |
| `wind-bridge/src/host_render_windows.rs`（新，cfg windows） | `HostRenderManager`（见 §4） |
| `wind-bridge/src/handler.rs` | `handle_host_render_request` 改签名：`fn handle_host_render_setup(&self, client_id: u32, pid: u32) -> Vec<HostRenderSetupEntry>`（darwin/unix 默认实现返回空）；`handle_host_render_ready` 保留语义 |
| `wind-bridge/src/server.rs` | `CMD_HOST_RENDER_REQUEST` 分支：取当前连接 clientID + 宿主 PID → 调 manager `setup` → `encode_host_render_setup(entries)` 作为响应 payload（空 entries 时回 ack，DLL 视为不可用）；连接断开路径挂 `cleanup_client(client_id, expected_seq)` |
| `wind-ipc/src/protocol.rs` + `codec.rs` | `HostRenderSetupEntry { window_kind: u8, max_buffer_size: u32, shm_name, event_name }` + `encode_host_render_setup`（字节布局对照 C++ `BinaryProtocol.h` 解码端，小端、长度前缀 UTF-16/UTF-8 以 C++ 现实现为准）+ 字节断言单测 |
| `wind-bridge/src/push.rs` | `host_render_avail` 位接真实值：由 server 持有的评估回调（焦点进程是否命中白名单）决定；该位翻转会促使 DLL 走 `_EnsureHostRenderSetup` |
| `wind-ui/src/manager_windows.rs` | UpdateCandidates/Tooltip/Status 分流：存在 host-render 目标 → `render_frame()` → `HostRenderManager::write_frame_for_kind(...)`；**失败必须回退本地 LayeredWindow 路径，不得静默 return**（Go 经验 #6）；本地窗口与 host 窗口互斥（走 host 路径时隐藏本地窗口，反之亦然） |
| `wind-coordinator` | host-render 目标评估与生命周期（见 §5）；经 `UiCommand::SetHostRenderTarget(Option<HostRenderTarget{pid, client_id}>)` 通知 UiManager |
| `wind-config` | `compat.host_render_processes` 默认值改为 `["SearchHost.exe"]`（对齐 Go；空列表 = 功能关闭） |

## 4. HostRenderManager 设计

```rust
pub struct HostRenderManager {
    shms: HashMap<HostWindowKind, SharedMemoryWindows>, // 全局 per kind，懒建
    clients: HashMap<u32, ClientState>,                  // clientID → 状态
    setup_seq: u64,                                      // 单调递增
    visible_owner: HashMap<HostWindowKind, (u32 /*pid*/, u32 /*client_id*/)>, // §6 关键
    whitelist: Vec<String>,                              // 进程名模式，配置热更新
}
struct ClientState {
    pid: u32,
    setup_seq: u64,
    events: HashMap<HostWindowKind, NamedEvent>, // per instance 私有
}
```

公开 API（语义逐条对齐 Go `host_render.go`）：

- `is_process_whitelisted(pid) -> bool`：`QueryFullProcessImageNameW` 取进程名 →
  小写 → 通配符匹配（`*`/`?`，等价 Go `filepath.Match`）。
- `setup(client_id, pid) -> Vec<HostRenderSetupEntry>`：白名单校验 → 重建该 client
  旧 events → 三 kind 懒建全局 SHM + 新建私有 Event → `setup_seq += 1` 记入
  ClientState → 返回 entries。
- `write_frame_for_kind(kind, pid, target_client_id, frame) -> Result<()>`：写全局 SHM
  （stamp `target_instance_id = target_client_id`）→ 登记 `visible_owner[kind]` →
  SetEvent 唤醒**该 PID 全部实例**。
- `hide_kind(kind)`：见 §6，「hide 必达」入口。
- `cleanup_client(client_id, expected_seq)`：`expected_seq != 0 && state.setup_seq != expected_seq`
  时跳过（SetupSeq 守卫，防同 PID 断线重连竞态误删新状态，Go 经验 #5）；否则关 events、
  清 visible_owner 中属于该 client 的登记（清理前先走一次 hide）。

锁策略：单 `Mutex` 内只做状态变更与 SHM memcpy；`SetEvent` 与进程名查询移出锁外，
避免阻塞 bridge 线程。

## 5. coordinator 生命周期（移植 Go 关键经验）

| Go 经验 | Rust 落点 |
|---|---|
| hostRenderFunc 绑定进程级，焦点抖动不得清除；showUI 前重评估 | coordinator 在**每次候选显示前**调 `update_host_render_target()`：焦点 PID 命中白名单且该 client 已 setup → `SetHostRenderTarget(Some)`；否则 `None`。FocusLost 不主动清目标 |
| SearchHost composition 不工作（`TF_E_NOLOCK`、立即终止） | host-render 目标激活时：跳过依赖 TSF composition 的逻辑；composition 终止竞态窗口由 100ms 放宽至 500ms（对照 `handle_lifecycle.go:559-572`） |
| pendingFirstShow 首帧延迟 | host-render 激活时跳过候选窗首次定位 debounce，直接显示 |
| 同进程 band 变化（开始菜单 band=6 ↔ 任务栏搜索 band=13） | C++ 端 `UpdateBand` 已处理，服务端无需感知；保留 |
| WriteFrame 失败回退 | manager_windows 分流处失败 → 清目标 + 走本地窗口 + WARN 日志（不含用户输入内容） |

## 6. 多实例 bug 根治（全局 SHM 之内）

**原则：hide 必达 + 单一可见真源。**

Go 版 bug（special 退出候选窗不隐藏）的类别根因：hide 动作依赖当时的 host-render
评估结果——评估态在 hide 前发生变化时，hide 走了本地窗口路径，SHM 里残留可见帧，
Band 窗口无人熄灯。

Rust 设计将 hide 与评估解耦：

1. `visible_owner[kind]` 是「哪个 (pid, client) 的 Band 窗口当前可见」的唯一真源，
   只在 `write_frame_for_kind` 成功时登记。
2. **所有**隐藏路径（HideCandidates/HideTooltip/HideStatus、special 退出、焦点切换、
   client 断线清理、Shutdown）统一收敛到 `hide_kind(kind)`：
   - 若 `visible_owner[kind]` 存在 → 写 hide 帧（`flags` 清 VISIBLE 且
     `target_instance_id = 0`，恒不匹配任何实例 → 广播隐藏）→ SetEvent 唤醒该 PID
     **全部**实例 → 清 owner。
   - 不存在 → no-op。
   - 该调用**不检查**当前 host-render 目标/白名单/评估状态。
3. manager_windows 的隐藏命令处理 = 本地窗口 hide + `hide_kind` 双发（幂等，各自
   no-op 短路），保证两条渲染路径无论当时走的哪条，隐藏都必达。

C++ 端零改动：`target_instance_id` 不匹配即隐藏是现有行为，`0` 是天然广播值
（clientID 从 1 起）。

## 7. macOS 部分（W9 接线，不改架构）

按 `docs/superpowers/plans/2026-07-02-macos-migration-gaps.md` Group 1 执行，此处只
锁定归属与顺序，任务细节以该文档为准：

1. **1a 统一菜单树下发**：`build_unified_menu_tree()` 纯构建函数 +
   `encode_menu_show` + server macOS 分支回菜单树（替代 ack）。
2. **1b 菜单命令 id 映射**：统一菜单 id ↔ `handle_menu_command` 可解析值核对。
3. **1c hover/点选/右键**：coordinator 覆写 `handle_candidate_hover`（置 hover_index +
   重渲染推帧）；`encode_candidate_menu_flags` 按候选下发禁用位；点选链路真机验证。
4. **darwin `CMD_HOST_RENDER_SETUP` 握手时机闭环**：现状 Swift 端按固定
   `endpoint::shm_name(suffix)` 打开 SHM 且已工作 → 正式决策为「darwin 无需 setup
   握手」，在协议注释中写明 0x0501 为 Windows 专用，关闭 W4 遗留待办。

## 8. 配置

```toml
[compat]
host_render_processes = ["SearchHost.exe"]  # 进程名白名单，支持 * / ? 通配，空 = 关闭
```

- 默认值从空改为 `["SearchHost.exe"]`（对齐 Go；用户已有配置不受影响，L1 默认层变更）。
- 热更新：配置变更 → manager 白名单原子替换；已 setup 的 client 不回收（下一次
  焦点评估自然生效）。

## 9. 测试策略

1. **Linux 可测（纯逻辑）**：
   - `shared_render_frame` 编帧字节断言（header 偏移/魔数/序列递增/hide 帧 flags 与
     target=0）；
   - `encode_host_render_setup` 字节布局断言（对照 C++ 解码端偏移）；
   - HostRenderManager 状态机单测（SHM/Event 以 trait 注入 mock）：setup 重入、
     SetupSeq 守卫跳过旧清理、visible_owner 登记/转移、hide 幂等、白名单通配匹配。
2. **Windows 单测**：真实 SHM/Event 创建 + 第二进程句柄可 Open；写帧后按 header
   逐字段回读。
3. **真机验证清单**：
   - 开始菜单搜索（band=6）输入中文，候选可见、翻页、鼠标点选/悬停/滚轮；
   - 任务栏搜索（band=13 + owner）同上；
   - 同 PID 双记事本窗口交替输入，恰好一个候选框可见、切换无残留；
   - **special 模式（quick_symbols，ForceVertical）选字上屏后候选窗隐藏**（原 bug 回归）；
   - 白名单外应用不受影响（本地 LayeredWindow 路径零回归）；
   - 服务重启 / DLL 断线重连后自愈（SetupSeq 路径）；
   - dev/prod suffix 并行不串线。

## 10. 风险

| 风险 | 缓解 |
|---|---|
| `encode_host_render_setup` 字节布局与 C++ 既有解码端不符 | 实现前先读 `IPCClient.cpp` 的 setup 解码代码，以 C++ 为准写断言测试 |
| handler trait 改签名波及 unix/deferred 实现 | 默认实现返回空 Vec，darwin/linux 零行为变化；CI 三平台把关 |
| `host_render_avail` 位翻转时机与 DLL `_EnsureHostRenderSetup` 的交互竞态 | 对照 Go server 的置位逻辑逐行核对；真机断线重连验证 |
| manager_windows 分流触碰 Windows 主渲染路径 | 无 host-render 目标时走原路径零分支副作用；白名单默认仅 SearchHost.exe，爆炸半径受控 |
| 多实例 hide 广播误伤「另一实例正要显示」 | 显示帧总在 hide 之后由目标实例的新 Event 触发重画（sequence 递增），最终状态收敛于最后一帧 |
