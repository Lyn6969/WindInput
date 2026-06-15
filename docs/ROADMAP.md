# WindInput — 现状差距与重设计路线

> 本文是**当前现状 + 下一步路线**，基于 2026-06-15 对两侧仓库的真实命令核查。
> 项目初始的整体构建蓝图见 [MIGRATION_PLAN.md](./MIGRATION_PLAN.md)（技术选型/协议兼容仍可参考，
> 但其 Phase 0–6 复选框已过时，不代表当前进度）。

## 仓库目录结构（2026-06-15 定）

本仓库从"纯 Rust 工程"转为"多语言产品仓"，最终完全替代 Go 版 `WindInput`。
采用**按组件分子目录 + 共享资产在根**的布局（镜像 Go），Rust 核心服务放在 `wind_input/`：

```
WindInput/              产品仓根
├── wind_input/            Rust 核心服务 (Cargo workspace: Cargo.toml + crates/ + apps/ + scripts/)
├── docs/                  产品级文档 (ROADMAP / MIGRATION_PLAN / VERSION)
├── .gitignore
└── (将来) wind_tsf/        C++ TSF DLL
         wind_macos/        macOS 组件
         data/ installer/   共享数据与安装包
```

- Cargo workspace 根 = `wind_input/`（不在 git 根，cargo 不受影响）。
- 构建脚本 `wind_input/scripts/dev.{sh,ps1,bat}` 内部用 `PROJECT_ROOT`(wind_input) /
  `PRODUCT_ROOT`(产品仓根) 两级定位；`VERSION` 读自产品仓 `docs/VERSION`，Go 仓库在产品仓同级。

## 现状（已核实）

- **基本输入闭环已通**：wubi、拼音、混输的击键 → 候选 → 上屏基本流程都能用。
- **差距在深度与质量**，不在"能不能跑"：功能完整度与候选质量不如 Go 版。
- **体量**：Rust ≈ 1.9 万行（12 crates + 2 apps） vs Go `wind_input` ≈ 15.3 万行（666 文件，`internal/` 22 模块）。
  行数 ≠ 完成度（Rust 更精简、Go 含大量平台/测试样板），下表的"部分/骨架"为**结构层证据**，
  各子系统的功能深度需在阶段 A 的设计差分中逐一核实。

## 差距全景（结构层，证据化）

| Go 模块 | Go 行 | Rust 对应 | Rust 行 | 状态 |
|---|---|---|---|---|
| coordinator | 23570 | wind-coordinator | 3977 | 🟡 部分（handle_* 框架在） |
| ui | 22426 | wind-ui | 3905 | 🟡 部分 |
| engine | 19933 | wind-engine | 2523 | 🟡 部分（拼音/码表/混输框架在，深度待核） |
| dict | 16021 | wind-dict | 1319 | 🟡 部分 |
| tooltip | 8743 | wind-ui/tooltip（内含） | — | 🟡 部分 |
| theme | 7224(+pkg) | wind-theme | 493 | 🟡 部分 |
| config | 6139 | wind-config | 1260 | 🟡 部分 |
| store | 5328 | wind-store | 509 | 🟡 部分 |
| bridge | 5261 | wind-bridge | 1370 | 🟡 部分 |
| cmdbar | 5481 | wind-cmdbar | ~313 | 🔴 骨架（每文件 10~30 行桩） |
| schema | 4854 | wind-config/schema（内含） | — | 🟡 部分 |
| rpc | 4718 | apps/rpc | ~32 | 🔴 空桩（services 每个 1 行） |
| uicmd | 2872 | 散落 coordinator? | — | 🔴 缺/散落 |
| transform | 1453 | wind-transform | 441 | 🟡 部分 |
| ipc | 1534 | wind-ipc | 1008 | 🟢 接近 |
| candidate | 315 | wind-candidate | 311 | 🟢 接近 |
| keyinject | 576 | — | 0 | 🔴 缺（上屏/数字键上屏） |
| backup | 1075 | — | 0 | 🔴 缺 |
| foreground | 206 | — | 0 | 🔴 缺（应用级配置） |
| clipboard | 232 | 4 处引用，无独立 | — | 🔴 基本缺 |
| pkg/systemfont | 1135 | — | 0 | 🔴 缺 |
| perf / sysinfo | 317/104 | 极少 | — | 🔴 基本缺 |
| e2e | 1632 | — | 0 | 🔴 缺（测试设施） |

## 策略：先重设计，再补（用户定）

不照搬 Go 的结构与坏设计。每个子系统在补实现前，先做一轮设计差分，锁定 Rust 侧目标架构边界，
再填功能/质量。目标对齐"补全功能 / 删坏设计 / 精简重设计"。

## 阶段计划

### 阶段 A — 重设计基线（设计先行，read-only）
对核心链逐子系统产出一份"设计差分"：**Go 设计要点 / Go 坏设计（不照搬）/ Rust 现状 / Rust 目标边界**。
参考 Go 仓库 `wind_input/docs/design/`（含归档设计稿）。
- 优先子系统：**engine、dict、store**（候选质量短板集中处），其次 coordinator、schema/config。
- 产出：每子系统一份 redesign spec（落 `docs/redesign/<模块>.md`），用户逐份确认后再进入补实现。

### 阶段 B — 输入质量核心补齐（engine + dict + store）
按阶段 A 锁定的架构补候选质量：拼音 LM/打分/模糊音、码表深度、混输策略、用户词频学习、临时词、影子层。
这是"质量不如 Go"最直接的部分。

### 阶段 C — 交互与配置完善
方案切换重载、hotkey、cmdbar（当前骨架）、tooltip/状态提示、foreground 应用级配置、transform 补齐。

### 阶段 D — 配套与设置程序

### 贯穿原则
- 每个改动用 `wind_input/scripts/dev.sh ci`（fmt-check + clippy + test）把关。
- 删坏设计：发现 Go 的冗余/坏抽象，记录在对应 redesign spec 的"不照搬"小节，明确不移植。
- 增量提交，单次聚焦单一职责。

## 推荐的第一步
阶段 A 从 **engine + dict** 的设计差分开始（候选质量短板的根因），产出 `docs/redesign/engine.md`、
`docs/redesign/dict.md`，用户确认目标边界后再进入阶段 B 的补实现。
