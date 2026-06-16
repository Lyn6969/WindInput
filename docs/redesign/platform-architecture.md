# 重设计：跨平台包架构（Windows + macOS）

> 目标：当前以 Windows 为主，但 Go 版已支持 macOS，Rust 版需对等支持。**提前在包/crate 架构上隔离平台**，
> 使 macOS 移植变成"实现一组已定义的平台 trait + 原生 host"，而非重构核心。

## 1. 现状（已核实，2026-06-16）
好消息：**核心已基本平台无关**。
- 核心逻辑 crate **平台相关行=0**：engine / dict / store / config / candidate / transform / cmdbar / theme / ipc。
- **wind-coordinator 也是 0**：靠 `KeyEventData`（输入）/`KeyAction` enum（输出）抽象，不直接碰平台 API。
- 平台耦合集中在：**wind-ui（~53 行）/ wind-bridge（~24 行）/ apps(service,rpc)**。
- `windows` crate 依赖均 **`[target.'cfg(windows)'.dependencies]` 门控**（不会污染其它平台编译）。

结论：架构方向已对，macOS 移植 = 给 bridge(传输)/ui(渲染+窗口) 补 macOS 实现 + 原生 IMK host + 把缺失平台能力（keyinject/foreground/clipboard/systemfont）做成平台 trait。

## 2. Go 的 macOS 模型（参考）
- Windows：`wind_tsf`（C++ TSF DLL，注入每个应用进程）↔ 命名管道 ↔ Go 服务进程（wind_input，引擎所在）。
- macOS：`wind_macos`（原生，非 Go——InputMethodKit 应用，独立进程）+ wind_input 内 `*_darwin.go` 平台分支。
- 即"平台抽象在服务内用 build tag，原生 host 单独一个组件"。

## 3. 部署模型（关键架构分叉）
- **Windows 必须"独立服务 + 瘦客户端"**：TSF DLL 注入到**每个应用进程**，不能各自驮一份引擎 → 引擎必须在**单独服务进程**，DLL 经 IPC 通信。
- **macOS IMK 是单一独立进程**：可以
  - (a) **嵌入**：IMK 应用通过 FFI 直接链接 Rust 核心库（无 IPC，最简单）；或
  - (b) **瘦客户端**：IMK 应用经 IPC 连 Rust 服务（与 Windows 一致）。
- **设计原则**：把 Rust 核心做成**平台无关库**，**两种部署都支持**——Windows 走服务模型，macOS 默认可走嵌入（更简单），保留瘦客户端选项。核心不假设自己以何种方式被宿主。

## 4. 目标 Crate 架构（分层）
```
核心层（平台无关，CI 守护 0 平台行）
  wind-ipc / wind-config / wind-store / wind-dict / wind-candidate /
  wind-transform / wind-engine / wind-theme / wind-cmdbar / wind-coordinator
        │ 仅依赖 ↓（trait，不依赖任何平台 API）
平台抽象层（trait，平台无关）
  wind-platform：定义 Transport / TextClient / Surface / SystemServices 等 trait
        │ 由 ↓ 实现
平台实现层（按 target 编译）
  cfg(windows)  → windows crate 实现（可放 wind-platform-windows 或各 crate 的 platform 子模块）
  cfg(macos)    → objc2/core-foundation/core-text/core-graphics/app-kit/IMK 实现（wind-platform-macos）
        │ 由 ↓ 装配
宿主/入口
  apps/service（Windows 服务进程）       wind-ffi（cdylib/staticlib，C ABI，供原生 host 嵌入）
原生 host（非 Rust workspace，同级组件）
  wind_tsf（C++ TSF DLL）                 wind_macos（Swift IMK 应用）
```

要点：
- 核心层**永不**直接 `use windows::`/`objc2::`/`cfg(target_os)`——一切平台交互经 trait。**加 CI 守护**（dev.sh 检查核心 crate 无平台符号）。
- 平台实现可选两种组织：**cfg 门控子模块**（类似 Go build tag，相关代码就近）或**独立 `wind-platform-{windows,macos}` crate**。倾向后者（隔离彻底、依赖不外溢）；wind-ui/wind-bridge 现有的平台代码逐步收敛到这层。
- `wind-ffi`：暴露 C ABI 的核心入口（init/feed-key/get-candidates/select/commit 回调等），供 macOS IMK 应用嵌入；Windows 服务可不用它（直接 Rust 装配）。

## 5. 平台抽象面（trait 清单）
| trait | 职责 | Windows | macOS |
|---|---|---|---|
| `Transport` | IPC 收发（服务模型）| Named Pipe | Unix domain socket / Mach port |
| `TextClient` | 上屏/更新组合区/移光标/清空（coordinator 的 KeyAction 落地）| TSF | IMK `IMKInputController` |
| `Surface` | 候选窗呈现：BGRA 缓冲 → 窗口 | Layered Window / host-render 共享内存 | NSWindow + CALayer |
| `TextShaper` | 字形整形/排版（已有 backend trait）| DirectWrite | CoreText |
| `Foreground` | 前台应用识别（应用级配置）| Win32 | NSWorkspace / Accessibility |
| `SystemFonts` | 系统字体枚举 | DirectWrite | CoreText |
| `Clipboard` | 剪贴板 | Win32 | NSPasteboard |
| `CaretQuery` | 光标矩形（候选窗定位）| TSF/UIAutomation | IMK/AX |
| `Hotkey` | 全局热键注册 | Win32 | Carbon/CGEventTap |
| `KeyInject` | 上屏/数字键上屏 | SendInput/TSF | IMK insertText |

> 这些 trait 也正好覆盖差距分析里缺失的平台模块（keyinject/foreground/clipboard/systemfont）——
> **从一开始就按平台 trait 设计**，Windows 先实现、macOS 留 stub，避免后补时重构。

## 6. 渲染与文本策略（跨平台关键）
- **光栅：tiny-skia（纯 Rust、跨平台）** 把候选窗画进 BGRA 缓冲——**两平台共用**；只有"缓冲→窗口呈现"是平台特定（`Surface` trait）。这是 wind-ui 跨平台的核心抓手，**保持光栅路径平台无关**。
- **文本整形：`TextShaper` backend trait（已存在 dwrite/freetype）** 加 **CoreText backend**（macOS）。
  - ⚠️ **纯 Rust 交叉编译约束**：freetype 是 C 依赖。macOS 优先 **CoreText**（objc2-core-text，系统库，不破坏 mac 构建）；
    Windows 用 DirectWrite。长期可评估**纯 Rust 文本栈**（rustybuzz 整形 + ab_glyph/fontdue 光栅）以彻底去 C，但非当务之急。
- 候选窗：Windows 可用 layered window 或 host-render（共享内存把缓冲交宿主进程画，应对 D2D 应用）；macOS 用 IMK 候选窗机制或自绘 NSWindow。共享内存 host-render 是 **Windows 特例**，不进核心。

## 7. IPC / 传输
- `Transport` trait：Windows Named Pipe / macOS Unix domain socket。协议（wind-ipc）平台无关，已是。
- host-render 共享内存：Windows 专属优化，trait 之外的可选扩展。

## 8. 目录布局（产品仓，承接 dir 决策）
```
WindInput/
├── wind_input/     Rust 核心 + 服务 + 平台实现（cfg/平台 crate）+ wind-ffi
├── wind_tsf/       C++ TSF DLL（Windows 原生 host）
├── wind_macos/     Swift IMK 应用（macOS 原生 host，将来）
├── data/ docs/ installer/   共享
```

## 9. 近期可做（只留门，不建 macOS）
现在不实现 macOS，但**现在就把门留好**，避免日后重构：
1. **CI 守护核心 0 平台行**：dev.sh 加检查（核心 crate 不出现 `use windows::`/`objc2`/`cfg(target_os`）。
2. **收敛平台耦合到明确边界**：wind-ui/wind-bridge 的平台代码集中到 `platform` 子模块 / 平台 crate，并抽出上面的 trait。
3. **缺失平台能力按 trait 设计**：keyinject/foreground/clipboard/systemfont/caret/hotkey 一律先定义 trait（平台无关签名），Windows 实现、macOS `unimplemented!` stub。
4. **依赖门控就位**：`windows` 已 target-gated；预留 `[target.'cfg(target_os="macos")'.dependencies]`（objc2 系列）。
5. **光栅保持平台无关**：tiny-skia 路径不掺平台调用；`Surface` 只负责"缓冲→窗口"。

## 10. 不现在做（排期）
- 真正的 macOS 实现 / IMK Swift 应用 / wind-ffi C ABI / CoreText backend：等 **Windows 质量核心（阶段 B/C）稳定后**，作为**独立的"macOS 平台阶段"**（阶段 D 或之后）。
- 现在的产出是**架构与 trait 边界**，确保那一天到来时是"填实现"而非"拆重构"。

> 与各差分的关系：本架构不改变 engine/dict/store/frequency/pinyin 的逻辑设计（它们本就平台无关）；
> 影响的是 wind-ui/wind-bridge/coordinator-host 边界与新平台 trait 的引入。每步 `wind_input/scripts/dev.sh ci` 把关。
