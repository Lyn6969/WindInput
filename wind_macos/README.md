# WindInput macOS

WindInput 输入法的 macOS 端工程：IMKit `.app` 壳，经 Unix Domain Socket 与跨平台
Rust 服务（`../wind_input/`）通信。渲染、定位、上屏决策都在服务侧，`.app` 负责
IMKit 接口、原生浮层呈现与按键合成。

## 快速开始

```bash
# 1. 单测（协议帧 roundtrip + 响应路由）
swift test

# 2. 构建并安装 Rust 服务（另一终端）
../scripts/mac/dev.sh m2 pm2

# 3. Smoke：连 bridge.sock 发 KeyEvent + 订阅 push 10 秒
swift run wind-smoke
```

完整构建 / 安装 / 打包走 `../scripts/mac/dev.sh`（命令面对齐 Windows 的
`scripts/dev.ps1`），`-h` 看命令表。

## 文档

- `AGENTS.md` — 目录结构 / 协议同步铁律 / 与 Windows 的功能差距 / 变体共存
- `../AGENTS.md` — 仓库地图、crate 索引、跨组件硬约定
- `../scripts/mac/dev.sh` — 头部注释即构建/部署/变体的权威说明
