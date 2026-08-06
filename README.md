<p align="center">
  <img src="pic/logo.png" alt="清风输入法" width="128">
</p>

<h1 align="center">清风输入法 (WindInput)</h1>

<p align="center">
  轻量、快速、可定制的开源中文输入法
</p>

<p align="center">
  <img src="https://img.shields.io/badge/platform-Windows%2010%2F11-brightgreen" alt="Platform">
  <img src="https://img.shields.io/badge/macOS-12%2B-blue" alt="macOS">
  <img src="https://img.shields.io/badge/license-MIT-green" alt="License">
</p>

<p align="center">
  <a href="https://windinput.com"><b>官网</b></a> ·
  <a href="https://windinput.com/download"><b>下载</b></a> ·
  <a href="https://windinput.com/docs"><b>使用文档</b></a> ·
  <a href="https://windinput.com/changelog"><b>更新日志</b></a>
</p>

## 特性

- **专为五笔和码表输入方案设计** — 五笔 86、五笔拼音混输，同时提供全拼和双拼
- **方案驱动** — 通过方案文件灵活定义输入行为
- **图形设置** — 配套设置工具，配置可视化调整，修改即时生效
- **亮暗主题** — 支持亮色和暗色主题，可随系统自动切换
- **轻量运行** — Rust 实现，资源占用低，启动迅速

## 安装

前往 [windinput.com/download](https://windinput.com/download) 下载 Windows 安装包，
双击安装后按 `Win + Space` 切换到清风输入法。

macOS 目前仅支持从源码构建，暂未提供安装包。

## 文档

完整的使用说明、配置参考和常见问题都在文档站：**[windinput.com/docs](https://windinput.com/docs)**

## 仓库范围

本仓库包含清风输入法的核心部分：

| 组件 | 技术 | 职责 |
|------|------|------|
| `wind_input` | Rust | 核心服务：输入引擎、词库、候选管理、UI 渲染、IPC（跨平台） |
| `wind_tsf` | C++ | Windows TSF 输入法框架接口，键盘事件捕获 |
| `wind_macos` | Swift | macOS IMKit 输入法客户端 |

配套的设置程序、便携启动器与安装器目前未开源，完整成品请从
[下载页](https://windinput.com/download)获取。核心部分可独立构建和运行。

## 从源码构建

- **Windows**：Rust stable + Visual Studio 2022（C++ 桌面开发）+ CMake，运行 `.\scripts\dev.ps1`
- **macOS**：Rust stable + Xcode（Swift 5.9+），运行 `scripts/mac/dev.sh`
- **Linux（交叉编译 Windows 产物）**：`scripts/dev.sh`

构建脚本会自动下载第三方词库数据并生成完整的数据目录。详细说明见
[贡献指南](CONTRIBUTING.md)。

## 参与贡献

欢迎贡献代码、报告 Bug 或提出建议！请阅读 [贡献指南](CONTRIBUTING.md)。

> 首次提交 PR 需要签署 [贡献者许可协议 (CLA)](CLA.md)。

## 许可证

本项目源代码采用 [MIT 许可证](LICENSE)。词库数据来源于
[白霜拼音](https://github.com/gaboolic/rime-frost)、[极点五笔](https://github.com/KyleBing/rime-wubi86-jidian)、
[pinyin-data](https://github.com/mozillazg/pinyin-data)、[OpenCC](https://github.com/BYVoid/OpenCC)
等第三方项目，适用各自的许可证条款，完整声明详见 [NOTICE.md](NOTICE.md)。

## 关于项目名称

MIT 许可证授予您对源代码的完整权利，本项目不对此附加任何限制。

若您将本项目分支后公开发布，请更换项目名称与 logo，并注明为非官方分支，
以便用户区分软件的实际维护者。

本项目未注册商标，上述为约定与请求而非法律条款，
详见 [项目名称与标识使用约定](BRANDING.md)。

## 交流与反馈

- **QQ 交流群**：[1085293418](https://qm.qq.com/q/u2A8FfafIs)
- **GitHub Issues**：[问题反馈](../../issues)

## 相关项目

- [WindInput-Go](https://github.com/huanfeng/WindInput-Go) — 清风输入法的前身（Go 实现），本项目由其移植重写而来
