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

## 现状（核实于 2026-06-17）

- **基本输入闭环已通**：wubi、拼音、混输的击键 → 候选 → 上屏基本流程都能用。
- **核心输入链 + 输入质量 + 按键管线已走完**（见下"已完成里程碑"），差距主要转移到**外围交互
  （cmdbar、应用级配置）、配套（rpc/设置/备份/字体/主题深度/测试设施）与跨平台**。
- **体量**：Rust ≈ 2.26 万行（12 crates + 2 apps） vs Go `wind_input` ≈ 15.3 万行（666 文件，`internal/` 22 模块）。
  行数 ≠ 完成度（Rust 更精简、Go 含大量平台/测试样板），下表 🟡/🔴 为**结构层证据**，深度仍需逐项核。

### 已完成里程碑（2026-06-16/17）
- **阶段 A**：engine/dict/store/coordinator/config-schema 五份 redesign 差分 + pinyin-smart-input + platform-architecture。
- **阶段 B**（输入质量大头）：redb 落地（user/temp/freq/shadow）、dict 分层 CompositeDict 接通、
  拼音 lattice trie + bigram + 打分器、词频与权重解耦。
- **key-pipeline S0–S4**（见 redesign/key-pipeline.md）：S0 redb 接线 → S1 模式单点决策（散装 bool→enum）→
  S2 符号单点流水线（自定义/智能符号/数字后/全半角）→ S3 URL+特殊模式+统一夺取回退 →
  S4 全码/空码策略（全码上屏/空码清空/顶码，含全局 `[input.code_commit]` tri-state 继承）+ 模式激活键融合。

## 差距全景（结构层，证据化；Rust 行更新于 2026-06-17）

| Go 模块 | Go 行 | Rust 对应 | Rust 行 | 状态 |
|---|---|---|---|---|
| coordinator | 23570 | wind-coordinator | 5184 | 🟢 核心链完整（S0–S4 单点管线/全码策略） |
| ui | 22426 | wind-ui | 3929 | 🟡 部分（候选窗/工具栏可用，深度待核） |
| engine | 19933 | wind-engine | 2989 | 🟢 拼音/码表/混输 + 全码策略；模糊音等深度待核 |
| dict | 16021 | wind-dict | 1611 | 🟢 CompositeDict 接通 + mmap/redb 层 |
| tooltip | 8743 | wind-ui/tooltip（内含） | — | 🟡 部分 |
| theme | 7224(+pkg) | wind-theme | 493 | 🟡 浅（调色板/ResolvedV3/bgimage 深度不足） |
| config | 6139 | wind-config | 1747 | 🟢 富 Schema + 三层合并 + code_commit 全局 |
| store | 5328 | wind-store | 1457 | 🟢 redb 落地 |
| bridge | 5261 | wind-bridge | 1403 | 🟡 部分（管道/共享内存通，边角待核） |
| cmdbar | 5481 | wind-cmdbar | 181 | 🔴 骨架（34 内置函数是桩；基础日期/计算走 quick_input） |
| schema | 4854 | wind-config/schema（内含） | — | 🟢 富 Schema |
| rpc | 4718 | apps/rpc | 22 | 🔴 空桩（**用户已明确延后**，设置程序通信用） |
| uicmd | 2872 | 散落 coordinator | — | 🔴 缺/散落 |
| transform | 1453 | wind-transform | 553 | 🟡 部分（标点/全半角/配对/智能符号；简繁 s2t 待核） |
| ipc | 1534 | wind-ipc | 1047 | 🟢 接近（含 ReplaceBackward 等新命令码） |
| candidate | 315 | wind-candidate | 311 | 🟢 接近 |
| keyinject | 576 | — | 0 | 🔴 缺（基础上屏走 KeyAction::InsertText，专用注入未移植） |
| backup | 1075 | — | 0 | 🔴 缺 |
| foreground | 206 | — | 0 | 🔴 缺（前台窗口检测驱动的应用级配置） |
| clipboard | 232 | 仅 UI 引用 | — | 🔴 基本缺 |
| pkg/systemfont | 1135 | — | 0 | 🔴 缺（系统字体枚举，设置面用） |
| perf / sysinfo | 317/104 | 极少 | — | 🔴 基本缺 |
| e2e | 1632 | — | 0 | 🔴 缺（测试设施） |

## 策略：先重设计，再补（用户定）

不照搬 Go 的结构与坏设计。每个子系统在补实现前，先做一轮设计差分，锁定 Rust 侧目标架构边界，
再填功能/质量。目标对齐"补全功能 / 删坏设计 / 精简重设计"。

### 要吸取的 Go 最新设计优点（跨子系统原则）
- **模式融合**：不把 拼音/码表/混输 当成硬隔离的三套模式，而是融合为统一的候选/交互模型
  （Go 最新方向）。Rust 当前 engine 仍是三个独立 trait 实现 + mixed 包装；重设计时应朝
  "一套引擎/协调逻辑按方案配置驱动"靠拢，减少模式特例分支。
- **统一的按键处理**：按键事件走单一优先级链路，不为各模式各写一套；模式差异由配置/策略表达，
  而非散落的 if-mode 分支。
- 以上主要落在 **coordinator / 按键链**，将在 coordinator 差分中深挖 Go 最新实现并吸取；
  engine/dict 差分中凡涉及"模式特例"的边界，优先选可被统一模型复用的设计。

## 阶段计划

### 阶段 A — 重设计基线（设计先行，read-only）
对核心链逐子系统产出一份"设计差分"：**Go 设计要点 / Go 坏设计（不照搬）/ Rust 现状 / Rust 目标边界**。
参考 Go 仓库 `wind_input/docs/design/`（含归档设计稿）。
- 优先子系统：**engine、dict、store**（候选质量短板集中处），其次 coordinator、schema/config。
- 产出：每子系统一份 redesign spec（落 `docs/redesign/<模块>.md`），用户逐份确认后再进入补实现。
- **已完成（2026-06-16）**：engine / dict / store / coordinator / config-schema 五份差分，证据化（Go 侧 agent 提取 + grep 抽验 file:line）。
  关键交叉结论：① **词频已完全重构**（见 redesign/frequency.md）：词频与权重解耦，只存 {count,last_used}，作排序独立维度（码表 used-first 可选模式 / 拼音衰减分），不再加到 weight；engine 打分器只出基础质量分；
  ② dict 分层 CompositeDict 脚手架存在但未接线，决策接通；③ store redb 未落地，决策统一后端；
  ④ coordinator 吸取 Go 统一管线/模式融合**目标态**（避开半迁移双轨）；⑤ schema 是质量特性配置面，统一为一套富 Schema。
  ⑥ **智能拼音单列权威设计**（见 redesign/pinyin-smart-input.md）：lattice 用 trie common-prefix-search（拼音系统词库=只读 mmap trie，crawdad/yada/fst 评估），bigram 必做；
  **纠正**前 dict.md"弃 wdat 统一 wdb"——码表用 wdb、拼音用 trie（按访问模式分，Go 分法是对的）。
  ⑦ **跨平台包架构**（见 redesign/platform-architecture.md）：核心 crate 已 0 平台行（含 coordinator），耦合仅 wind-ui/wind-bridge/apps；
  目标=核心无关层 + 平台 trait 层(Transport/TextClient/Surface/SystemServices) + 平台实现(cfg/平台crate) + 原生 host(wind_tsf C++/wind_macos Swift) + wind-ffi；
  光栅用 tiny-skia(跨平台)、文本走 backend trait(DirectWrite/CoreText)；macOS 实现排期阶段 D 后，现在只留门(trait 边界 + CI 守护核心 0 平台行 + 缺失能力按平台 trait 设计)。
  落地顺序：config 合并修复 + 富 Schema → store redb(user/temp/freq/shadow) → dict 接通 composite → engine 打分器/拼音 lattice trie + bigram → coordinator pipeline。

### 阶段 B — 输入质量核心补齐（engine + dict + store）✅ 大头已落
拼音 LM/打分、码表深度、混输策略、用户词频学习、临时词、影子层、redb 落地均已实现。
余：模糊音深度、码表更多边角，随用随补。

### 阶段 C — 交互与配置完善（🚧 进行中）
- ✅ 按键管线统一（key-pipeline S1–S4：单点决策/符号流水线/URL/特殊模式/全码策略/激活融合）。
- 🔜 **模式/方案统一**（下一阶段，已拍板，先出设计 `redesign/mode-scheme-unify.md` 评审）：
  临拼/特殊模式引擎统一为 scheme 实例（base 持久 / overlay 瞬态分离的 Processor-lite），
  支持临时 mix 复合引擎融合临拼/快符/生僻字。
- ⬜ **S5 按钮自定义**（工具栏/候选操作键映射）。
- ⬜ **cmdbar**（命令栏 DSL + 内置函数，当前骨架）。
- ⬜ **foreground 应用级配置**（前台窗口检测 → 按 app 自动切模式/方案）。
- ⬜ 方案切换重载质量、tooltip/状态提示深度、transform 简繁补齐。

### 阶段 D — 配套与设置程序

### 贯穿原则
- 每个改动用 `wind_input/scripts/dev.sh ci`（fmt-check + clippy + test）把关。
- 删坏设计：发现 Go 的冗余/坏抽象，记录在对应 redesign spec 的"不照搬"小节，明确不移植。
- 增量提交，单次聚焦单一职责。

## 下一步（2026-06-17）
阶段 A/B 已完成，按键管线 S0–S4 已落。下一阶段为**模式/方案统一**（阶段 C 核心）：
先出设计差分 `docs/redesign/mode-scheme-unify.md`，用户评审通过后再动手；之后接 S5 按钮自定义 / cmdbar / foreground。
