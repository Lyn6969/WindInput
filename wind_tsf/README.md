# wind_tsf - C++ TSF 核心层

Windows TSF (Text Services Framework) 输入法核心实现，负责与 Windows 系统交互。

> 本组件已并入 **WindInput** 仓库（`wind_tsf/`，同级于 `wind_input/`）。对端服务现为
> **Rust 版**（`wind_input`，原 Go 服务的继任者）；IPC 二进制协议保持兼容（见
> `include/BinaryProtocol.h` ↔ Rust `crates/wind-ipc`）。可在本仓库 Linux 交叉编译链路里
> 直接用 MinGW 构建，亦保留 Windows/MSVC 原生构建。迁移说明见
> [`docs/redesign/tsf-migration.md`](../docs/redesign/tsf-migration.md)。

## 功能

- 实现 `ITfTextInputProcessor` 接口
- 处理按键事件 (`ITfKeyEventSink`)
- 语言栏图标显示 (`ITfLangBarItemButton`)
- 通过命名管道与输入法服务（Rust `wind_input`）通信
- 支持 Windows 10/11 现代输入框架

## 项目结构

```
wind_tsf/
├── src/
│   ├── dllmain.cpp           # DLL 入口点
│   ├── Globals.cpp           # 全局变量和 GUID 定义
│   ├── ClassFactory.cpp      # COM 类工厂
│   ├── TextService.cpp       # TSF 主服务实现
│   ├── KeyEventSink.cpp      # 按键事件处理
│   ├── HotkeyManager.cpp     # 快捷键管理器
│   ├── LangBarItemButton.cpp # 语言栏图标
│   ├── IPCClient.cpp         # 命名管道客户端
│   ├── CaretEditSession.cpp  # 编辑会话管理
│   ├── DisplayAttributeInfo.cpp # 显示属性
│   └── Register.cpp          # TSF 注册/卸载
├── include/
│   ├── BinaryProtocol.h      # 二进制协议定义
│   ├── HotkeyManager.h       # 快捷键管理器
│   ├── TextService.h
│   ├── KeyEventSink.h
│   ├── LangBarItemButton.h
│   ├── IPCClient.h
│   └── ...
├── resource/
│   └── wind_tsf.rc           # 资源文件
└── CMakeLists.txt
```

## 构建

### 方式一：MinGW 交叉编译（默认，Linux→Windows；本仓库链路）

需要 mingw-w64 工具链（`x86_64-w64-mingw32-g++` / `-windres`）。MinGW 自带 TSF 头不完整，
缺失接口/GUID/常量由 `include/mingw_tsf_compat.h` 补齐（仅 `__MINGW32__` 生效）。

```bash
make                       # → build/wind_tsf.dll
make DEV_VARIANT=1       # → build_dev/wind_tsf_dev.dll（独立 CLSID/管道/日志）
# 或经 dev.sh：
../scripts/dev.sh tsf            # release
../scripts/dev.sh tsf debug      # dev 变体
```

构建输出：`build/wind_tsf.dll`（PE32+ x64，静态链接，无 MinGW 运行时依赖）。
细节见 [`docs/redesign/tsf-migration.md`](../docs/redesign/tsf-migration.md)。

### 方式二：Windows/MSVC 原生（可选，用完整 Windows SDK）

需要 CMake 3.15+ 与 Visual Studio 2017+（含 C++ 桌面开发工具）。

```batch
mkdir build && cd build
cmake .. -A x64
cmake --build . --config Release
```

构建输出：`build/Release/wind_tsf.dll`

## 核心类说明

### CTextService

TSF 输入法主服务，实现以下接口：

| 接口 | 说明 |
|------|------|
| `ITfTextInputProcessor` | 输入法生命周期管理 |
| `ITfThreadMgrEventSink` | 线程管理器事件 |

关键方法：
- `Activate()` - 输入法激活，初始化所有组件
- `Deactivate()` - 输入法停用，释放资源
- `InsertText()` - 向应用程序插入文本
- `ToggleInputMode()` - 切换中英文模式

### CKeyEventSink

按键事件处理，实现 `ITfKeyEventSink` 接口：

- `OnTestKeyDown()` - 预判断是否处理该按键
- `OnKeyDown()` - 实际处理按键
- 自动识别 Ctrl/Alt 组合键并放行

处理的按键：
- `A-Z` - 拼音输入
- `1-9` - 选择候选词
- `Space` - 选择第一个候选词
- `Enter` - 提交原始拼音
- `Escape` - 取消输入
- `Backspace` - 删除最后一个字符
- `Shift` - 切换中英文模式

### CLangBarItemButton

语言栏图标，显示当前中/英文状态：

- 使用 `GUID_LBI_INPUTMODE` 确保在 Windows 11 输入指示器中显示
- 支持点击切换模式
- 图标自动更新

### CIPCClient

命名管道客户端：

- 管道名称: `\\.\pipe\wind_input`
- 协议: 二进制协议（BinaryProtocol.h）
- 自动重连机制
- 自动启动 Go 服务

### CHotkeyManager

快捷键管理器，负责：

- 解析 Go 端下发的热键配置
- 维护热键白名单（KeyHash 格式）
- 判断按键是否需要拦截
- 支持 KeyDown/KeyUp 不同热键列表

### BinaryProtocol.h

定义二进制 IPC 协议：

- 协议头（8 字节）：版本 + 命令 + 长度
- KeyPayload（16 字节）：按键事件数据
- 状态标志位定义
- 命令常量（CMD_KEY_EVENT 等）

## TSF 注册

### 必需的分类 GUID

```cpp
GUID_TFCAT_TIP_KEYBOARD           // 键盘类输入法
GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT // UWP 应用支持 (Windows 8+)
GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT   // 系统托盘支持 (Windows 8+)
GUID_TFCAT_TIPCAP_UIELEMENTENABLED // UI 元素支持 (Windows 8+)
```

### 注册命令

```batch
# 注册
regsvr32 wind_tsf.dll

# 卸载
regsvr32 /u wind_tsf.dll
```

需要管理员权限。

## GUID 说明

项目中定义的 GUID：

| GUID | 用途 |
|------|------|
| `c_clsidTextService` | COM 类 ID |
| `c_guidProfile` | 语言配置文件 ID |
| `GUID_LBI_INPUTMODE` | 语言栏图标 ID |

如需部署，应生成新的唯一 GUID：
```powershell
[guid]::NewGuid()
```

## 调试

### 构建调试版本

```batch
cmake --build . --config Debug
```

### 日志系统

TSF 层使用分级日志系统（定义在 `Globals.h`）：

| 级别 | 宏 | 说明 |
|------|-----|------|
| ERROR | `WIND_LOG_ERROR` | 严重错误（始终启用） |
| WARN | `WIND_LOG_WARN` | 警告信息 |
| INFO | `WIND_LOG_INFO` | 重要信息（默认） |
| DEBUG | `WIND_LOG_DEBUG` | 调试信息 |
| TRACE | `WIND_LOG_TRACE` | 追踪信息 |

**使用示例**:

```cpp
WIND_LOG_INFO(L"Operation completed");
WIND_LOG_DEBUG_FMT(L"Key: 0x%X, Mods: 0x%X", keyCode, modifiers);
WIND_LOG_ERROR_FMT(L"Failed with error: %d", GetLastError());
```

**启用详细日志**:

在 `Globals.h` 中取消注释：
```cpp
#define WIND_DEBUG_LOG
```

### 查看日志

1. 使用 DebugView (Sysinternals)
2. Visual Studio 输出窗口
3. 附加到 TSF 应用进程 (如 notepad.exe)

日志前缀格式：`[WindInput][LEVEL] message`

## 双管道架构

TSF 层使用两个命名管道与 Go 服务通信：

| 管道 | 名称 | 方向 | 用途 |
|------|------|------|------|
| 主管道 | `\\.\pipe\wind_input` | TSF → Go | 同步请求/响应 |
| 推送管道 | `\\.\pipe\wind_input_push` | Go → TSF | 异步状态推送 |

**主管道**: 用于按键事件、模式切换等需要同步响应的操作。

**推送管道**:
- 独立线程 (`AsyncReader`) 监听
- 接收状态变更、热键更新等通知
- 不阻塞主输入流程

## IPC 消息格式（二进制协议）

### 协议头格式（8 字节）

```cpp
struct IpcHeader {
    uint16_t version;   // 协议版本 (0x1001)
    uint16_t command;   // 命令类型
    uint32_t length;    // Payload 长度
};
```

### 上行命令 (C++ → Service)

| 命令 | 代码 | 说明 |
|------|------|------|
| CMD_KEY_EVENT | 0x0101 | 按键事件 |
| CMD_FOCUS_GAINED | 0x0201 | 获得焦点 |
| CMD_FOCUS_LOST | 0x0202 | 焦点丢失（载荷 8 字节 clientToken，见下） |
| CMD_IME_ACTIVATED | 0x0203 | 输入法激活 |
| CMD_IME_DEACTIVATED | 0x0204 | 输入法停用（载荷 8 字节 clientToken，见下） |
| CMD_CARET_UPDATE | 0x0301 | 光标更新 |

失焦类命令（0x0202 / 0x0204）自 v0.111.4 起携带发送方的 clientToken。服务端据此丢弃
**陈旧失焦**：`OnKillThreadFocus` 比 DocMgr 级失焦晚约 100ms 才发出，跨宿主切换时它必然
晚于新宿主的 focus_gained 到达，无校验会清掉新宿主刚建立的激活态（工具栏闪一下即隐藏）。
载荷为 0 字节时服务端按 token=0 处理并保持旧行为，故新旧两侧可任意组合。

### 下行命令 (Service → C++)

| 命令 | 代码 | 说明 |
|------|------|------|
| CMD_ACK | 0x0001 | 确认 |
| CMD_COMMIT_TEXT | 0x0101 | 提交文字 |
| CMD_UPDATE_COMPOSITION | 0x0102 | 更新组字 |
| CMD_MODE_CHANGED | 0x0201 | 模式变更 |
| CMD_STATUS_UPDATE | 0x0202 | 状态更新 |
| CMD_SYNC_HOTKEYS | 0x0301 | 同步热键 |

## 注意事项

- 修改 DLL 后需要卸载再重新注册
- 卸载前确保所有应用已切换到其他输入法
- 调试时可能需要结束 `ctfmon.exe` 进程
- Windows 11 要求使用 `ITfInputProcessorProfileMgr` 注册
