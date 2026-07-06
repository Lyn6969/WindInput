# 迁移：C++ TSF 核心层（wind_tsf）并入 WindInput

> 目标：把原 `WindInput/wind_tsf`（Windows TSF 文本服务框架 C++ COM DLL）**完整迁移**进本仓库，
> 纳入现有 Linux→Windows 交叉编译链路（与 Rust 侧 `x86_64-pc-windows-gnu` 一致，无需 Windows/MSVC），
> 文档化、可在 `dev.sh` 一键构建；迁移以**忠实保留**为原则（历史含大量 bug 修复，不做破坏性改动）。

状态（2026-06-20）：**已完成并在 Linux 上编译+链接通过**，产出 `wind_tsf.dll`（PE32+ x64，3.4MB，
完全静态、无 MinGW 运行时依赖）。**运行期验证（regsvr32 + 实际输入）需在 Windows 实测机进行**，见 §9。

---

## 1. 迁移了什么

源 `../WindInput/wind_tsf` → 本仓库 `wind_tsf/`（同级于 `wind_input/`，对齐原 Go 仓库布局）。
不带历史记录，按当前快照拷入。

```
wind_tsf/
├── src/                       # 14 个 .cpp（TextService/KeyEventSink/IPCClient/LangBarItemButton/HostWindow…）
├── include/                   # 头文件 + BinaryProtocol.h（IPC 协议，与 Rust wind-ipc 镜像）
│   └── mingw_tsf_compat.h     # ★新增：MinGW TSF 兼容垫片（仅 __MINGW32__ 生效）
├── res/                       # 图标 + version.rc.in 模板
├── CMakeLists.txt             # 保留：Windows/MSVC 原生构建（Windows SDK 完整头）
├── Makefile                   # ★新增：MinGW 交叉编译（Linux 链路）
├── wind_tsf.def(.in)          # COM 导出定义
└── README.md / AGENTS.md      # 文档（已更新 Go→Rust、构建路径）
```

组件职责（详见 `wind_tsf/AGENTS.md`）：`CTextService` 主服务（实现一组 TSF COM 接口）、
`CKeyEventSink` 按键、`CIPCClient` 命名管道（二进制协议 + 熔断 + 异步推送）、
`CLangBarItemButton` 语言栏、`CHostWindow`（Win11 开始菜单 band 窗口候选框）、
`CHotkeyManager` 热键白名单、`CFileLogger` 文件日志。

---

## 2. 构建链路集成

**两条等价构建路径，编同一份源码：**

| 路径 | 工具 | 何时用 | 缺失符号来源 |
|------|------|--------|--------------|
| **MinGW（默认，Linux）** | `wind_tsf/Makefile` + `x86_64-w64-mingw32-g++` | 本仓库交叉编译链路 | `mingw_tsf_compat.h/.cpp`（见 §3） |
| **MSVC（Windows 原生）** | `wind_tsf/CMakeLists.txt` + VS 2022 | Windows 上用完整 SDK 构建 | Windows SDK + uuid.lib（原生齐全） |

### dev.sh

`scripts/dev.sh` 原 `copy_tsf_dll`（从同级 Go 仓库复制预编译 DLL）已替换为 **`build_tsf`**（真正 MinGW 构建）：

- `do_build` / `deploy_all` 在构建 `wind_input.exe` 后自动调 `build_tsf`，release→`wind_tsf.dll`、debug→`wind_tsf_debug.dll`。
- 独立命令：`./scripts/dev.sh tsf`（release）、`./scripts/dev.sh tsf debug`（调试变体）；菜单项 `6`。
- 未装 mingw-w64 时优雅跳过并提示（`push` 经 SSH 部署仍可复用 Windows 上已有 DLL）。
- 版本号取自 `docs/VERSION` 传入 Makefile，写进版本资源。

### Makefile 选项

```bash
make -C wind_tsf                         # build/wind_tsf.dll
make -C wind_tsf DEBUG_VARIANT=1         # build_debug/wind_tsf_debug.dll（独立 CLSID/管道/日志，可与正式版共存）
make -C wind_tsf VERSION=1.2.3 OUTDIR=/path
```

产物及 `obj/` 落在 `build/`、`build_debug/`，已被根 `.gitignore` 忽略。

---

## 3. MinGW TSF 兼容垫片（核心技术点）

**问题**：mingw-w64 自带的 `<msctf.h>`/`<ctfutb.h>` 不完整 —— 缺少一批本工程用到的 TSF COM 接口、
类别 GUID 与常量；而 Windows SDK（MSVC）这些都齐全。直接用 MinGW 编译会报数十处 "未声明" /
链接期 "undefined reference"。

**方案**：`include/mingw_tsf_compat.h`（声明 + 接口结构体）+ `src/mingw_tsf_compat.cpp`（GUID 定义），
整体 `#ifdef __MINGW32__` 包裹 —— **MSVC 构建时完全为空**，不影响原生路径。垫片通过 `-include` 在
所有 TU 前置（业务源码基本零改动）。

补齐内容：

1. **7 个 COM 接口**（vtable 顺序不可改）：`ITfTextInputProcessorEx`、`ITfDisplayAttributeProvider`、
   `ITfTextLayoutSink`、`ITfMenu`、`ITfLangBarItemButton`、`ITfCandidateListUIElement`、
   `ITfCandidateListUIElementBehavior`。基类（`ITfTextInputProcessor`/`ITfUIElement`/`ITfLangBarItem`/
   `IUnknown`）MinGW 已有，仅补派生接口。
2. **2 个枚举**：`TfLBIClick`、`TfLayoutCode`。
3. **常量**：`TF_LBI_*`、`TF_CLUIE_*`、`TF_CLIENTID_NULL`、`TF_INVALID_GUIDATOM`、
   `PACKAGE_FAMILY_NAME_MAX_LENGTH`（=64；MinGW 的 minappmodel.h include guard 写反且被 UWP 分区守卫，失效）。
4. **GUID 定义**（编译期声明缺失 + 链接期 libuuid.a 缺值，共两类）：13 个 IID/类别/区间 GUID。

### GUID/vtable 的权威来源与交叉校验（关键：避免凭记忆出错）

- **接口 IID + vtable 方法顺序**：取自本仓库依赖 `windows` crate **0.58**（微软 win32metadata 自动生成，
  官方权威），`define_interface!` 给 IID，`*_Vtbl` 结构体字段顺序即 vtable 顺序。
- **类别/区间 GUID 值**：同样取自 windows crate 0.58。
- **交叉校验**：用 4 个已知值（`GUID_TFCAT_CATEGORY_OF_TIP` / `TIPCAP_COMLESS` / `PROP_AUDIODATA` /
  `PROPSTYLE_STATIC`）核对第三方源（TSF-TypeLib），全部吻合；又用 mingw-w64 `msctf-uuid.c`
  反查 libuuid 实际提供值。**校验中抓到一处第三方源错误**：TSF-TypeLib 把 `GUID_TFCAT_PROPSTYLE_CUSTOM`
  错标成了 `GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT` 的值（`25504fb4-…`）—— 已据 windows crate 纠正。
- C++ 方法签名同时与实现类（`CTextService`/`CLangBarItemButton`/`CDisplayAttributeProvider`）的
  override 声明逐一比对一致（这些类按真实 SDK 编写，是签名的第二重权威）。

---

## 4. 唯一的行为分叉：MinGW 跳过两个 legacy 属性样式类别

`GUID_TFCAT_PROPSTYLE_CUSTOM` 与 `GUID_TFCAT_PROPSTYLE_STATICCOMPACT` 这两个 **legacy 文本属性样式类别**
在**所有可得权威源（官方 win32metadata、mingw-w64、Wine、ReactOS）均已移除**，无可信 GUID 值可用
（TSF-TypeLib 的 CUSTOM 值已证伪）。

它们出现在 `Register.cpp` 的类别注册数组中（注释称"与小狼毫 weasel 保持一致，确保 Win11/UWP 兼容"）。
但这两个属于**文本属性提供者**类别，与键盘 TIP 的 Win11/UWP 兼容性（该列表的真实目的：COMLESS /
IMMERSIVESUPPORT / SYSTRAYSUPPORT / UIELEMENTENABLED / SECUREMODE / INPUTMODECOMPARTMENT）**无关**。

**决策**：不伪造 GUID 值。`Register.cpp` 用 `#ifndef __MINGW32__` 守卫这两条注册/卸载项：
- **MSVC 构建**：经 uuid.lib，**完整保留**两个类别（与 weasel 字节一致，行为不变）。
- **MinGW 构建**：跳过这两个 legacy 类别注册。对键盘 IME 功能无影响。

> 若日后取得这两个 GUID 的权威值，在 `mingw_tsf_compat.cpp` 补 `DEFINE_GUID` 并去掉 `Register.cpp`
> 的两处 `#ifndef __MINGW32__` 即可恢复完全一致。

---

## 5. 业务源码改动清单（忠实保留）

除上面的 `#ifndef` 守卫外，仅两处可移植性修复（MSVC/MinGW 通用，行为不变）：

| 文件 | 改动 | 原因 |
|------|------|------|
| `src/TextService.cpp` | `#include <ShellScalingApi.h>`→`<shellscalingapi.h>`、`<InputScope.h>`→`<inputscope.h>` | Linux 大小写敏感；Windows 大小写不敏感，两端皆可 |
| `src/TextService.cpp` | 裸 `max(a,b)` → 等价三元表达式 | `max` 宏 MSVC 由 `<windows.h>` 提供、MinGW 不一定有；三元表达式两端通用 |
| `src/Register.cpp` | 2 处 `#ifndef __MINGW32__` 守卫 | 见 §4 |

新增文件：`include/mingw_tsf_compat.h`、`src/mingw_tsf_compat.cpp`、`Makefile`。
**未删除任何业务逻辑**，bug 修复历史完整保留。

---

## 6. IPC 协议状态（与 Rust wind-ipc 的兼容性）

`include/BinaryProtocol.h` 是 IPC 二进制协议的 C++ 侧定义，与 Rust `wind_input/crates/wind-ipc`
（`protocol.rs`/`codec.rs`）互为镜像。命令码、Header、KeyPayload(18B)、CaretPayload(20B)、
FocusGainedPayload(36B) 等核心结构两端一致（已核对）。

> **✅ 已解决（2026-07，host-render Windows 移植）**：`SharedRenderHeader`（64B）Rust 侧
> `protocol.rs` 已按 C++ 补齐尾部字段 `rect_count / rects_offset / rendered_hover_index /
> target_instance_id`（+ `reserved[2]`），开始菜单 band 窗口候选框的鼠标交互（点选/翻页/悬停）
> 与多实例定向均已实现并真机验证。`BinaryProtocol.h` 仍是该协议的权威定义，改动须同步。
> 详见 `docs/redesign/host-render-windows-port.md`。

注释中残留的 "Go 服务" 字样为历史协议镜像说明（功能无害）；权威对端现为 Rust 服务。

---

## 7. 功能精简审查（结论：维持现状）

按要求审查了"是否有历史原因可精简"。结论：**代码成熟、bug 驱动，不做自由裁量式删除。**

- 扫描 `TODO/FIXME/DEPRECATED/legacy/废弃`：命中项多为**有意的向后兼容**（如 IPCClient 的 legacy 单字段
  镜像、Register 的 legacy profile 注册回退），删除有破坏风险，保留。
- `wind_dwrite.dll` / `WindDWriteShim.cpp` 已在迁移前移除（DirectWrite 改由对端直调系统 dwrite.dll）；
  C++ 侧 LangBarItemButton 仍用 DWrite 渲染语言栏图标，属正常依赖。
- 唯一实际精简是 §4 的两个 legacy 属性样式类别（且仅 MinGW 路径，被迫而非裁量）。

考虑到"历史处理了大量 bug、不能有影响"的约束，**进一步精简留待有 Windows 实测回归能力时再评估**。

---

## 8. 构建命令速查

```bash
# 单独构建 TSF DLL
./scripts/dev.sh tsf               # release → wind_input/build/wind_tsf.dll
./scripts/dev.sh tsf debug         # 调试变体 → wind_input/build/wind_tsf_debug.dll
make -C wind_tsf                   # 直接用 Makefile

# 完整构建（exe + TSF DLL + data）
./scripts/dev.sh release           # 或 debug

# Windows 原生（MSVC，可选）
cd wind_tsf && mkdir build && cd build && cmake .. -A x64 && cmake --build . --config Release
```

依赖：`x86_64-w64-mingw32-g++` / `-windres`（mingw-w64 工具链）。本机经 linuxbrew 安装，含较完整的
Windows 头（msctf/ctfutb/dwrite/shellscalingapi 等），缺失部分由 §3 垫片补齐。

---

## 9. 验证状态与待办

- [x] 14 个 .cpp + 资源 + compat.cpp 全部 MinGW 编译通过
- [x] 链接为 `wind_tsf.dll`（PE32+ x64），导出 4 个 COM 入口（DllCanUnloadNow/DllGetClassObject/DllRegisterServer/DllUnregisterServer）
- [x] 完全静态链接，无 libgcc/libstdc++/winpthread 运行时依赖（可 drop-in 到纯净 Windows）
- [x] release 与 debug 变体均通过 Makefile + dev.sh 构建
- [ ] **Windows 实测机运行期验证**（compile-verified，未 run-verified）：本机无 Windows，COM vtable 正确性
      仅经编译期 + 权威源校验保证；需在 Windows 上 `regsvr32 wind_tsf.dll` → 切到清风输入法 → 实际输入，
      重点核验：① 中文组字/候选上屏 ② 语言栏图标/菜单 ③ 开始菜单候选框（HostWindow band 窗口）
      ④ 与 Rust 服务的 IPC 握手。可经 `./scripts/dev.sh push` 部署到 SSH 实测机。
- [ ] Rust 侧 `SharedRenderHeader` 字段补齐（§6）
