# wind_macos

macOS IMKit `.app` 工程。与 Win 端 `wind_tsf/` DLL 对位，与跨平台 Rust 服务
（`wind_input/`，cargo workspace）通过 Unix Domain Socket 通信。

`.app` 只是 IMKit 壳：渲染、定位、上屏决策的真身都在 Rust 服务里。对应关系见
`scripts/mac/dev.sh` 头部注释（Win 的 `m1`=TSF DLL ↔ mac 的 `m1`=`.app`，
Win 的 `m2`=核心 exe ↔ mac 的 `m2`=Rust 服务）。

## 当前阶段

协议通路、IMKit 骨架、候选/菜单/提示的原生呈现均已就位：

- 协议层 `WindInputKit`（BinaryCodec + BridgeClient + PushClient + ProtocolTypes）
- IMKit `.app`：InputController（composition / commit / 焦点 / 密码框 / 自愈重连）
  + KeyHandler（NSEvent → Win VK）
- 呈现层：候选框 NSPanel（SHM 取帧）、统一菜单、菜单栏模式指示器、tooltip、
  状态气泡、Toast、命令直通车按键合成
- IMK server 注册 + 自身 `--register-input-source` 子命令（镜像 Squirrel/RIME 路径）
- 单元测试 `Tests/WindInputKitTests/`（`swift test`）

### 与 Windows 的功能差距（待补）

Rust 核心跨平台，引擎/词库/候选/词频类改动 macOS 自动受益；差距集中在**宿主层**。
`wind-ui` 的 `UiCommand` 有 42 个变体，`manager_macos.rs` 的 Forwarder 目前接了 24 个。
未接的：

| UiCommand | 影响 | 备注 |
|---|---|---|
| `ShowInputDiag` / `HideInputDiag` / `CopyInputDiagText` | 输入诊断 HUD 整套缺失 | 设计见 `docs/design/input-diagnostics-hud.md` |
| `TakeScreenshot` / `ScreenshotCandidateToClipboard` / `ScreenshotStatusTip` / `ScreenshotTooltip` / `CopyTooltipText` | 浮层截图与复制内容缺失 | Win 侧走 GDI 取窗口位图；mac 需从 SHM 帧或 NSPanel 取 |
| `ReportCandidatePos` / `ReportStatusTipPos` | 候选窗 / 状态气泡的拖动与位置固定缺失 | 拖动在 .app 侧，落点须经上行帧回报给服务持久化（协议要新增码位） |
| `ShowCandidateMenu` / `HideMenu` / `MenuKey` | 候选右键菜单的键盘导航缺失 | 菜单树本身已走 `CmdMenuShow` + `UnifiedMenuBuilder` |
| `SetToolbarPos` / `SetToolbarAutoHide` | 语义部分 N/A（mac 用菜单栏指示器，无浮动工具栏） | 但配置项当前无处落地 |
| `SetHostRender` | Windows 专有（宿主进程内 Band 窗口） | mac 无对应概念 |

已接：`RegisterGlobalHotkeys`（见下）、`OpenPath` / `OpenApp`（`/usr/bin/open`，
`.app` 包走 `open -a … --args`）。

未接的变体落在 `Forwarder::handle` 的 `other =>` 兜底臂，只打一条 debug 日志。
**新接一个变体时同步更新本表**。

### 全局热键（Carbon）

实现在 `wind-ui/src/global_hotkey_macos.rs`，选 Carbon `RegisterEventHotKey` 而非
CGEventTap / NSEvent 全局监听：后两者要「辅助功能」授权（ad-hoc 重部署 cdhash 一变就
失效），而全局热键的语义恰恰是「本输入法没激活时也得生效」，授权掉了就是静默失灵。

**线程约定不可省**：热键事件只投递到**主线程**的 Carbon 事件队列。因此服务在 macOS 上
主线程跑 `run_main_loop()`（CFRunLoop），「重启服务」的等待挪到辅助线程、收到信号后
`stop_main_loop()`；forwarder 线程调 `apply()` 时只入队 + 唤醒，真正的注册/撤销一律在
主线程的 perform 回调里做。把注册直接放到 forwarder 线程能编过也不报错，但 Carbon Event
Manager 是主线程亲和的，症状会是热键**时灵时不灵**。

修饰键按**物理键直译**：Alt→Option、Win→Command，不做「Ctrl 自动换 Command」的翻译
——否则设置界面显示的与实际生效的不是一回事。VK→CGKeyCode 复用 `wind_keys::key_inject::
vk_to_cgkeycode`（与按键注入同源，禁止另起一张表）。表里没有的 VK（OEM 符号键等）跳过
并 warn，**不能**当成 keycode 0（那会注册出一个按 `a` 就触发的热键）。

另有两处协议侧的既有约束，不是"漏做"而是设计所限：

- **`CMD_CANDIDATE_SCROLL`（滚轮翻页）macOS 用不了**：0x0211 码位平台双语义，
  macOS 上行方向已被 `CMD_FRONT_CONTEXT` 占用（见 `wind-ipc/src/protocol.rs` 该处注释）。
  要做滚轮翻页得复用 `CMD_CANDIDATE_SELECT` 的负 index 约定（-1 上页 / -2 下页），
  或把 `FRONT_CONTEXT` 迁到空闲码位并同步 Swift 端。
- **`CMD_INPUT_STATS`（输入统计）无上报端**：Win 侧由 TSF DLL 在英文模式下上报，
  mac 的 `.app` 未采集，故英文输入不计入统计。

## 安装与 TIS 注册

`.app` 工程层的必要条件（缺一项就注册不上或切不过去）：

- bundleID 含 `.inputmethod.` 字符串（Apple 第一步 filter，不含直接 skip）
- Info.plist 全字段（ComponentInputModeDict + ts* + TISInputSourceID + ISO 15924 脚本码）
- Bundle 结构（Contents/{Info.plist, MacOS, Resources/lproj, _CodeSignature, PkgInfo}）
- Hardened runtime（`codesign --options runtime`）
- 真证书签名（本机自签 trusted，`scripts/mac/dev.sh sign-setup` 建）
- IME 自身 `--register-input-source` 子命令 + RunLoop 常驻（TIS 注册是进程级
  lifecycle，register API 调完进程退出 mode 会被清掉）
- `TISRegisterInputSource(bundleURL)` 真把 mode 持久写进 TIS DB

本机自签路径（`sign-setup` + `p1`/`pm1`）可以端到端跑起来，无需 Apple Developer
Program 公证——已多次实测。

**重装后偶发不稳定**：重新部署 `.app` 后有时切不过去 / 系统设置里状态不对。
**注销重登**（Log Out）即恢复——TIS 数据库与输入源列表是登录会话级的缓存，
`lsregister -f` 与重注册刷不动它。碰到这种情况先注销，不要往「代码坏了」方向排查。

> 早期文档曾记有一条「macOS 26 Tahoe 对非 Notarized IME 有系统设置 UI 硬墙、必须公证」
> 的结论，并附 ensan-hcl sample 对照实验。**该结论已被实测推翻**，多半是当时撞上了上面
> 这个需要注销的缓存问题。勿据此认为本机测不了。

## 变体共存（release/dev）

`.app` 无编译期变体标记，一律从**自身 bundleID 后缀**派生变体：
`Bundle.main.bundleIdentifier` 末尾为 `Dev` → dev。`BridgeEndpoints.variantSuffix`
（`""` / `"_debug"`）决定运行时目录（`…/WindInput[Dev]`、socket / SHM），与 Rust 侧
`wind-config/src/variant.rs` 对齐；`ModeStatusController` 菜单头读 `CFBundleDisplayName`
（release「清风输入法」/ dev「清风输入法开发版」）、`openSettings` 按变体启动对应设置应用
（`com.wails.wind_setting[_debug]`）。

变体化的完整对照表在 `scripts/mac/dev.sh` 头部注释「变体身份」一节。

**关键**：两变体 `.app` 可执行同名 `WindInput`，进程定位须用 `.app` 路径；
SHM / socket / config 全部变体隔离（漏一处即冲突，如曾漏 SHM → 开机后开发版候选框不显示）。

## 目录

| 路径 | 角色 |
|------|------|
| `Package.swift` | SwiftPM 清单，4 个 target（kit / smoke / app / tests） |
| `Sources/WindInputKit/IPC/ProtocolTypes.swift` | 协议常量 + payload 类型 + endpoint 路径 |
| `Sources/WindInputKit/IPC/BinaryCodec.swift` | 帧 encode/decode。`encodeFocusGainedFrame(inputScopeMask:)`: FocusGained 帧布局 `pid:u32(0占位) + inputScopeMask:u64`（bit31=IS_PASSWORD 标记密码框；旧版空帧=mask 0） |
| `Sources/WindInputKit/IPC/BridgeClient.swift` | UDS 阻塞客户端；`init(socketPath:, ioTimeoutMs:)` 可选 I/O 超时（`SO_RCVTIMEO`/`SO_SNDTIMEO`，0=不设）。request 连接设 2s——服务卡死/重启时同步 `readFrame` 超时抛错而非在 IMKit 主线程无限 hang（上层 catch → reconnect 自愈）；push 连接（PushClient）必须保持 0（长期空闲等服务端推送，否则被读超时误判断连）。**`connect()` 必设 `SO_NOSIGPIPE`**: 否则对端（Rust 服务）重启后向死连接 `write` 触发 SIGPIPE → 默认处置直接**杀死 .app 进程**（表现为服务重启后输入法彻底失灵、需强制重启前端）；设此项后 `write` 改返回 EPIPE 由 `send()` 抛错交上层重连。request/push/sendClient 都用此构造，一处覆盖 |
| `Sources/WindInputKit/IPC/BridgeResponseRouter.swift` | 把响应帧路由到 `TextInputClient` 调用。**新增 cmd 必须在此显式接一臂**——`default` 是"消费按键但不出字"，漏接的表现是按键被吃掉、屏幕上什么都没有（历史案例：`commitThenDefer` 漏接导致码表顶码上屏丢字） |
| `Sources/WindInputSmoke/main.swift` | `swift run wind-smoke` — 连 bridge + push，打印帧 |
| `Sources/WindInputApp/main.swift` | `.app` 入口：默认启 IMKServer + NSApp.run; 也支持 `--register-input-source` / `--enable-input-source` / `--select-input-source` 子命令。`--register-input-source` **总是重注册 + RunLoop 常驻**，不因「已注册」早退（重新部署 .app 的 cdhash 变，必须重注册刷新否则无法切换；install 已先杀旧守护，早退会让注册失去维持进程而失效）。变体隔离：mode-id 检查用 `Bundle.main.bundleIdentifier + "."` 前缀精确匹配（避免 `WindInput`/`WindInputDev` 子串互串） |
| `Sources/WindInputApp/Controller/InputController.swift` | `IMKInputController` 子类，同步 KeyEvent roundtrip，路由 PassThrough/Consumed/CommitText/UpdateComposition; `activateServer`/`deactivateServer` 发 FocusGained/FocusLost（驱动协调器 imeActivated → 指示器）; **密码框适配**（对齐 Win 36614ae）: `activateServer` 用 `IsSecureEventInputEnabled()` 探测，命中则在帧 payload 携带 InputScope bitmask 的 IS_PASSWORD 位（bit31），协调器据此对密码框强制英文半角直通（模式图标不变）; `deactivateServer` 失焦时若仍有 marked text 先 `setMarkedText("")` 清残留 + 清本端 composition。`menu()` 重写：点系统输入源图标弹出统一菜单（复用 UnifiedMenuBuilder）; 选中项经 IMK `doCommandBySelector` → `imkMenuCommand:` 读 NSMenuItem.tag 回发 CmdMenuAction。**自愈重连**: bridge 连接持有在实例字段，`activateServer`/`handle` 入口 `ensureConnected()` 懒重连; `handle` 的 `send`/`readFrame` 经 `sendAndApply` 执行，失败 catch → `reconnect()` 后**用新连接重试当前键一次**（服务重启后第一个键就自愈，不丢字）; 连接用 `ioTimeoutMs=2000` + `SO_NOSIGPIPE`。**智能配对光标**: `router.moveHostCursor` 闭包把 kit 层的 `CursorMove` 意图用 `KeySynthesizer` 合成 ←/→ 方向键（主线程 async，排在 insertText 后）; 需辅助功能授权，未授权静默降级 |
| `Sources/WindInputApp/Controller/KeyHandler.swift` | `NSEvent.keyCode` → Win VK 映射 + Modifier 编码 + KeyEvent 帧构造 |
| `Sources/WindInputApp/UI/CandidatePanelHost.swift` | 候选框承载层：订阅 push，收 CmdHostRenderFrame→SHM→NSPanel、CmdCandidateRects→hit-test、CmdModeStatus→ModeStatusController、CmdTooltip*/CmdStatus*→气泡、CmdToast*→ToastPanel; 命令直通车按键：CmdKeyTap/Hold/Release/Seq→`KeySynthesizer` CGEvent 合成，CmdKeyType→`activeResponder.applyPushResponse`→router `insertText` 上屏; 鼠标选词回发。导出 `unifiedMenuItems()`/`sendMenuAction(_:)` 供三处菜单（候选框右键/菜单栏指示器/系统输入菜单）复用同一 IPC 请求与回发路径 |
| `Sources/WindInputApp/UI/KeySynthesizer.swift` | 命令直通车按键合成（key.tap/seq/hold/release）: canonical 键名→CGKeyCode + 修饰键→CGEventFlags 映射，经 `CGEvent.post(tap: .cgSessionEventTap)` 向聚焦应用注入; **需「辅助功能」授权**，`ensureTrusted()` 未授权时弹一次系统请求并放弃本次（ad-hoc 签名重部署 cdhash 变会使旧授权失效，须重授）。key.type / clip.paste 文本上屏不走此处，走 `client.insertText` 免授权 |
| `Sources/WindInputApp/UI/CandidatePanel.swift` | 候选框 NSPanel（borderless 浮窗）+ 自绘 bitmap + 鼠标命中/悬停; 空白处右键经 UnifiedMenuBuilder 弹统一菜单 |
| `Sources/WindInputApp/UI/UnifiedMenuBuilder.swift` | 把服务下发的统一菜单树（MenuItemData）构建为原生 NSMenu; 三处共用。两种派发：`.inProcess`（普通 NSMenu，builder 作 target 回调）与 `.imkCommand`（系统输入菜单，target=nil + selector，IMK 经 doCommandBySelector 路由）; 菜单 id 统一经 NSMenuItem.tag 回传 |
| `Sources/WindInputApp/UI/ModeStatusController.swift` | 菜单栏模式指示器（NSStatusItem）: 收 CmdModeStatus 显示中英/全半角/标点/方案; 下拉菜单（NSMenuDelegate 动态填充）复用统一菜单树，点击回发 CmdMenuAction，服务未就绪时回退只读状态 |
| `Sources/WindInputApp/UI/TooltipPanel.swift` | 候选悬停 tooltip NSPanel。配色与拆字字根字体路径随 `CmdTooltipShow` 下发（服务侧 `manager_macos.rs` 从主题求值成 `#RRGGBBAA`）; 空串则用内置深色默认 |
| `Sources/WindInputApp/UI/StatusBubblePanel.swift` | 锚 caret 的模式状态气泡（收 CmdStatusShow） |
| `Sources/WindInputApp/UI/ToastPanel.swift` | 屏幕级 Toast 通知 NSPanel（词库就绪/错误等）: 收 CmdToastShow（标题+正文+bg/fg/accent+position+时长）渲染暗色圆角卡片 + 左侧 accent 条，按 durationMs 自动隐藏（0=5000，<0 常驻）; 点击穿透 |
| `Sources/WindInputApp/Resources/Info.plist` | IMK 元数据：ComponentInputModeDict / TISInputSourceID / LSUIElement（**不可**设 LSBackgroundOnly，否则候选 NSPanel 无法显示）/ InputMethodConnectionName = bundleID_Connection。**不可设 `tsInputModeDefaultStateKey`**: 会让 mode 注册即「已启用」却不落盘 AppleEnabledInputSources，导致「+ 添加输入法」列表过滤掉它、主列表又没有 → 中英文分组两头都看不见（Tahoe 实测） |
| `Sources/WindInputApp/Resources/AppIcon.icns` | 应用图标（Finder/安装器/关于面板），plist `CFBundleIconFile=AppIcon` 引用。与菜单栏单色 `menu_icon.pdf`（`tsInputMethodIconFileKey`）互不相干 |
| `Sources/WindInputApp/Resources/WindInput.entitlements` | App Sandbox 关闭（IMKit `.app` 与服务 UDS 共享文件路径需要） |
| `Sources/WindInputApp/Resources/{zh-Hans,en}.lproj/InfoPlist.strings` | 本地化菜单名（"清风输入法" / "WindInput"） |

## 协议同步铁律

修改 cmd id 或帧布局必须三处同步：

- `wind_input/crates/wind-ipc/src/protocol.rs` + `codec.rs`（**Rust SSOT**）
- `wind_tsf/include/BinaryProtocol.h`（Win）
- `wind_macos/Sources/WindInputKit/IPC/{ProtocolTypes,BinaryCodec}.swift`（本目录）

⚠️ **别按"名字像 Windows 的"来判断要不要接**：`CommitThenDefer`/`CommitAndHold`
的名字来自 TSF 的「吃了再吐」机制，但产出它们的是跨平台协调器逻辑
（码表顶码 direct_commit 在 `handle_candidate.rs`），macOS 一样会收到。
判断依据只看**谁产出**，不看名字。

## 本地开发

需要的工具：Xcode（含 swift 5.9+）、Rust toolchain。

```bash
cd wind_macos
swift test          # 单测
swift build         # 编 kit / smoke / app

# 另一终端：起 Rust 服务
../scripts/mac/dev.sh m2 && ../scripts/mac/dev.sh pm2

# smoke（默认监听 push 10 秒）
swift run wind-smoke
```

期望输出：

- 请求通道：`[smoke] <- recv cmd=0x0401 len=0`（Consumed）或 `cmd=0x0002 len=0`（PassThrough）
- push 通道：至少看到 `cmd=0x0206`（StatePush）一帧

## 构建 / 部署

统一入口 `scripts/mac/dev.sh`（命令面对齐 Windows 的 `scripts/dev.ps1`）：

```bash
scripts/mac/dev.sh sign-setup   # 一次性：建自签证书 "WindInput Dev"
scripts/mac/dev.sh gd           # 下载词库 + 生成 + 组装 data/ → build_mac/data
scripts/mac/dev.sh 1            # release 全构建（service + app + 数据校验）
scripts/mac/dev.sh p1           # 系统安装全部（service LaunchAgent + IME app + 设置 app）
scripts/mac/dev.sh m1           # 仅构建 .app        pm1  仅安装 .app
scripts/mac/dev.sh status       # 诊断：service pid / socket / 签名 / 进程
scripts/mac/dev.sh logs         # 跟踪 service + IME 日志
scripts/mac/dev.sh 8            # 生成 .pkg 安装包
scripts/mac/dev.sh hooks        # 激活 pre-commit（提交前 cargo fmt --check）
```

`d` 前缀为 dev 变体（`d1` / `pd1` / `dm1` / `pdm1` / `d8`）。
完整命令表与变体身份说明见 `scripts/mac/dev.sh -h`（或脚本头部注释）。

**固定用自签证书签名**：macOS 26 的 IME 必须真 Authority（纯 ad-hoc 装上能切但 IMK 不拉起
控制器 → 无法输入）；且证书签名 cdhash 稳定，重装不掉 TIS 注册，不用每次去系统设置重加。
