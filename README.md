<p align="center">
  <img src="pic/logo.png" alt="清风输入法" width="128">
</p>

<h1 align="center">清风输入法 (WindInput)</h1>

<p align="center">
  轻量、快速、可定制的开源中文输入法
</p>

<p align="center">
  <img src="https://img.shields.io/badge/platform-Windows%2010%2F11-brightgreen" alt="Platform">
  <img src="https://img.shields.io/badge/macOS-12%2B%20alpha-orange" alt="macOS alpha">
  <img src="https://img.shields.io/badge/license-MIT-green" alt="License">
</p>

> **⚠️ 早期开发阶段**
>
> 本项目处于 alpha 阶段，功能和配置格式可能随版本更新发生变化。

## 特性

- **专为五笔设计** — 支持五笔 86、五笔拼音混输，同时提供全拼和双拼输入
- **方案驱动** — 通过方案文件灵活定义输入行为
- **图形设置** — 配套设置工具，配置可视化调整，修改即时生效
- **亮暗主题** — 支持亮色和暗色主题，可随系统自动切换
- **高 DPI 适配** — 支持高分辨率和多显示器环境
- **轻量运行** — Rust 实现，资源占用低，启动迅速

## 安装

从 [Releases](../../releases) 页面下载：

- **Windows 安装包**：`WindInput-Setup-x.x.x.exe`，双击安装

安装完成后，按 `Win + Space` 切换到清风输入法。

> 当前版本未做数字签名，安装时可能需要在系统安全设置中手动放行。

macOS 目前仅支持[从源码构建](#从源码构建)，暂未提供安装包。

## 仓库范围

本仓库包含清风输入法的核心部分：

| 组件 | 技术 | 职责 |
|------|------|------|
| `wind_input` | Rust | 核心服务：输入引擎、词库、候选管理、UI 渲染、IPC（跨平台） |
| `wind_tsf` | C++ | Windows TSF 输入法框架接口，键盘事件捕获 |
| `wind_macos` | Swift | macOS IMKit 输入法客户端 |

配套的设置程序、便携启动器与安装器目前未开源，完整成品请从
[Releases](../../releases) 下载。核心部分可独立构建和运行。

## 从源码构建

- **Windows**：Rust stable + Visual Studio 2022（C++ 桌面开发）+ CMake，运行 `.\scripts\dev.ps1`
- **macOS**：Rust stable + Xcode（Swift 5.9+），运行 `scripts/mac/dev.sh`
- **Linux（交叉编译 Windows 产物）**：`scripts/dev.sh`

构建脚本会自动下载第三方词库数据并生成完整的数据目录。详细说明见
[贡献指南](CONTRIBUTING.md)。

## 参与贡献

欢迎贡献代码、报告 Bug 或提出建议！请阅读 [贡献指南](CONTRIBUTING.md)。

> 首次提交 PR 需要签署 [贡献者许可协议 (CLA)](CLA.md)。

## 第三方资源

| 资源 | 用途 | 许可证 |
|------|------|--------|
| [白霜拼音 (rime-frost)](https://github.com/gaboolic/rime-frost) | 拼音词库数据源 | GPL-3.0 |
| [极点五笔 for Rime](https://github.com/KyleBing/rime-wubi86-jidian) | 五笔 86 码表数据源 | Apache-2.0 |
| [pinyin-data](https://github.com/mozillazg/pinyin-data) | 汉字拼音注音数据 | MIT |
| [OpenCC](https://github.com/BYVoid/OpenCC) | 简繁转换数据 | Apache-2.0 |

完整声明（含来源不详资源的说明）请参阅 [NOTICE.md](NOTICE.md)。

## 许可证

本项目源代码采用 [MIT 许可证](LICENSE)。词库数据来源于第三方项目，
适用各自的许可证条款，详见 [NOTICE.md](NOTICE.md)。

## 交流与反馈

- **QQ 交流群**：[1085293418](https://qm.qq.com/q/u2A8FfafIs)
- **GitHub Issues**：[问题反馈](../../issues)

## 相关项目

- [WindInput-Go](https://github.com/huanfeng/WindInput-Go) — 清风输入法的前身（Go 实现），本项目由其移植重写而来
