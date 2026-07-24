# AGENTS.md — WindInput 协作约定

> 给在本仓工作的 AI/人类协作者。Rust 核心在 `wind_input/`（cargo workspace）。

## 仓库地图

本仓含三大组件，另有若干独立仓库与本仓协同（相对路径以本仓根为基准）：

| 位置 | 内容 | 文档 |
|---|---|---|
| `wind_input/` | Rust 核心服务（cargo workspace：19 个 crate + `apps/service`） | 本文档 + crate 级 AGENTS.md |
| `wind_tsf/` | C++17 TSF DLL：Windows 输入法接口层，经 Named Pipe 与 Rust 服务通信 | [AGENTS.md](wind_tsf/AGENTS.md) |
| `wind_macos/` | macOS IMKit `.app`（与 `wind_tsf/` 对位，开发中） | [AGENTS.md](wind_macos/AGENTS.md) |
| `../wind-setting` | 设置 UI（**独立仓库**，经 JSON-RPC 与核心通信）；改设置界面去那边 | — |
| `../wind-portable` | 绿色版启动器（独立仓库，不存在时构建脚本自动跳过） | — |
| `../WindInput-Go` | Go 旧版源码（只读参考；docs 旧文档里的 `../WindInput` 指的是它） | — |

## Crate 索引

> workspace 共 19 个 crate（均在 `wind_input/crates/`）。复杂 crate 已配 crate 级 `AGENTS.md`，改动前先读对应文档；新增/重构 crate 时参照同结构补文档。

| Crate | 职责 | crate 文档 |
|---|---|---|
| `wind-coordinator` | 输入法“大脑”：按键路由、状态机、候选与模式切换的中央协调器 | [AGENTS.md](wind_input/crates/wind-coordinator/AGENTS.md) |
| `wind-engine` | Schema 驱动的引擎工厂：拼音/码表/混输/英文四类引擎的构建、切换与候选分发 | [AGENTS.md](wind_input/crates/wind-engine/AGENTS.md) |
| `wind-ui` | 所有浮层窗口（候选窗/工具栏/菜单/状态泡/Toast/Tooltip）的渲染与鼠标交互 | [AGENTS.md](wind_input/crates/wind-ui/AGENTS.md) |
| `wind-cmdbar` | 命令直通车：短语解析 → AST 求值 → 动作执行（纯逻辑） | [AGENTS.md](wind_input/crates/wind-cmdbar/AGENTS.md) |
| `wind-dict` | 多层复合词典引擎：DictLayer/CompositeDict 查询 + wdat mmap 二进制词库 | [AGENTS.md](wind_input/crates/wind-dict/AGENTS.md) |
| `wind-store` | 基于 redb 的用户数据持久化：按方案隔离用户词/词频/Shadow，全局存短语 | [AGENTS.md](wind_input/crates/wind-store/AGENTS.md) |
| `wind-rpc` | core ↔ 设置端 JSON-RPC IPC 双通道（ctrl 请求-响应 + events 广播） | [AGENTS.md](wind_input/crates/wind-rpc/AGENTS.md) |
| `wind-config` | 配置系统：TOML 三层合并、字段注册表 SSOT、热键编译、变体探测、运行时状态 | [AGENTS.md](wind_input/crates/wind-config/AGENTS.md) |
| `wind-theme` | 加载并求值 v3 主题，输出调色板 + 盒模型树供 wind-ui 渲染 | [AGENTS.md](wind_input/crates/wind-theme/AGENTS.md) |
| `wind-bridge` | Named Pipe 服务器 + Push 管道，桥接 Rust 服务与 C++ TSF DLL | [AGENTS.md](wind_input/crates/wind-bridge/AGENTS.md) |
| `wind-ipc` | IPC 协议定义与编解码（TSF 二进制协议 + JSON-RPC） | — |
| `wind-keys` | 键名/VK 映射、导航键分类（纯逻辑）+ 按键注入（平台层）；**VK 常量 SSOT** | — |
| `wind-candidate` | 候选词数据类型、排序与过滤 | — |
| `wind-phrase` | 短语系统：静态/动态模板展开 + cmdbar 双路径 | — |
| `wind-transfer` | 导入导出/备份还原底座：Bundle（manifest + zip）聚合打包与 Merge 合并策略（编解码在 wind-store） | — |
| `wind-quick-input` | 快捷输入提供器：日期 / 计算器（纯逻辑） | — |
| `wind-reverse` | 候选反查：五笔编码/拆字/拼音读音（悬停 tooltip） | — |
| `wind-punct` | 标点转换纯逻辑（中英标点/全半角/数字后智能） | — |
| `wind-transform` | 文本变换：标点、全角、自动配对、简繁 | — |

`—` = 暂无独立 `AGENTS.md`（多为纯逻辑/工具 crate，职责单一，看 `src/lib.rs` 顶部模块注释即可）。

核心输入链路（词库 → 五类引擎 → 候选后处理）的**现状架构文档**见
[docs/architecture/engine-candidate-pipeline.md](docs/architecture/engine-candidate-pipeline.md)
（含混输拼音否决、顶码/满码一致性、各模式流程对比）；改引擎/候选逻辑时同步更新该文档。

## 虚拟键码（VK）—— 用常量，禁止裸十六进制

所有 Windows 虚拟键码统一在 `wind_input/crates/wind-keys/src/keymap.rs` 定义为
`pub const VK_*`（`VK_ESCAPE`/`VK_BACK`/`VK_SPACE`/`VK_RETURN`/`VK_PRIOR`/`VK_NEXT`/
`VK_UP`/`VK_DOWN`/`VK_A..VK_Z`/`VK_0..VK_9`/`VK_SEMICOLON` 等）。

- **禁止**在 `match data.key_code` / 比较中写裸 `0x1B`、`0x21` 之类字面量；用 `keymap::VK_*`。
- 触发键名（配置里的 `"backslash"`/`"semicolon"`）→ VK：`keymap::key_name_to_vk(_with_letters)`，
  单一真相源 `KEY_TABLE`，新增键只改一处。
- **⚠️ 触发键名跨仓一致性**：设置界面（独立仓库 `../wind-setting`，见上方仓库列表）的
  `src/assets/settings_manifest.toml` 里各触发键选项的 `value` 必须与本表 `KEY_TABLE.names`
  字符串**逐字相同**——两仓无编译期/运行期校验，写错会静默失效（UI 显示"已选中"、保存不报错，
  内核 `key_name_to_vk` 返回 `None` 后被 `filter_map` 悄悄丢弃）。曾因 `wind-setting` 把方括号
  选项写成 `open_bracket`/`close_bracket`（本表实际是 `lbracket`/`rbracket`）导致临时英文/临时
  拼音/快捷输入的方括号触发键全部失效。改本表新增键或改名时，**必须同步 grep 检查
  `wind-setting/src/assets/settings_manifest.toml` 与 `wind-setting/src/key_conflict.rs` 的
  `key_symbol()`** 有没有过时或不一致的字符串。
- **注意类型**：`KeyEventData.prev_char`、`CommitRequestData.trigger_key` 是 **u16**
  （UTF-16 码元 / 协议字段），与 VK(`u32`) 比较前需 `as u32` 转换；prev_char 是字符码点不是 VK，
  别套 VK 常量（用数值区间，如 `(0x30..=0x39).contains(&prev_char)` 判数字字符）。

## 候选导航键（翻页 / 高亮 / 二三候选）—— 配置驱动 + 统一逻辑

这些键**都可配置**，且必须走**统一**入口，禁止各 handler 各写一套硬编码判断：

- 翻页 / 高亮：`keymap::NavKeys`（从 `keys.page_keys` / `keys.highlight_keys` 组名编译）+
  `Coordinator::apply_nav_key(state, data, include_printable)`。普通模式与所有候选模式共用。
  - `include_printable=true`（码表型：普通/特殊/mix/临拼）：`-`/`=`/`[`/`]` 可作翻页；
  - `include_printable=false`（文本/表达式型：临英/快捷输入）：上述键作输入，不当导航。
  - overlay handler 用 `handle_candidate_nav`（按 `state.active` 自判 `include_printable`）。
- 二三候选键：`select_key_offset`（读 `keys.select_key_groups`，经 `hotkey::select_key_vks`）。
- 新增模式/按键时**复用**以上，不要再写 `0x21|0x22 =>` 之类分支。

## 跨组件硬约定（违反即复现历史 bug）

跨 crate / 跨语言边界的立约级不变量集中在此；crate 内部细节归 crate 级 `AGENTS.md`。

- **C++ 吃键集必须 ⊆ Rust 出字集**：`wind_tsf` 在 `OnTestKeyDown` 就决定是否吃键，早于 IPC
  往返；Rust 侧事后回 PassThrough 已来不及。凡 C++ 吃掉而 Rust 最终不出字的键，在严格 TSF
  宿主上直接丢失（历史案例：全角模式丢键、密码框丢键；指纹＝「有些应用打不出、有些出半角」）。
  给 C++ 侧新增吃键条件前，先确认 Rust 侧在同条件下必定产出。
- **候选排序必须落到 weight**：协调器会按 weight 统一重排候选；引擎内部只调顺序、不改
  weight 的排序会被重排冲掉。顶码上屏取首选与候选窗展示必须共用同一排序函数。
- **用户短语数据只存 `user_data.db`**（wind-store，全局不分方案）：yaml 短语文件是系统种子，
  **不是**用户覆盖入口——旧设计文档里「yaml 用户目录覆盖」的说法已过时，勿据此实现。

## 提交纪律（多会话共仓）

可能有多个 AI 会话同时在本仓工作。**提交只用显式路径**（`git add <具体文件>`），
**禁止 `git add -A` / `git add .`**——会把其它会话未提交的文件一起卷入提交。
提交前 `git status` 确认暂存区只含自己改的文件。

提交信息保持常规工程风格（`type(scope): 摘要`，中文正文）：**不要**添加
`Co-Authored-By`、`Generated with` 以及 `Constraint:` / `Confidence:` / `Tested:` 等
AI 附加 trailer。

## 格式化（强制）

**每次修改 Rust 文件后，验证通过前必须运行 `cargo fmt`**（在 `wind_input/` 目录下），
再把格式化结果作为独立提交：

```bash
cd wind_input
cargo fmt
# 确认只有格式改动，无逻辑变更
git add <修改过的 .rs 文件>
git commit -m "style(fmt): cargo fmt 统一格式化"
```

- **逻辑修改** 和 **fmt 修改** 必须分开提交，不能混在同一个 commit。
- 不要用 `git add -A`：只 stage 本次逻辑改动涉及的文件 + 对应 fmt 文件。
- `cargo fmt` 对整个 workspace 生效，若其他 crate 也被格式化，一并纳入 fmt 提交。
- 多会话协作下格式漂移容易累积（上一会话改完忘记提交 fmt 结果）：开始新一轮工作前，
  先跑一次 `git status` + `cargo fmt`，确认没有遗留的纯格式改动混入本次工作区，
  避免和自己本次的逻辑改动绞在一起难以拆分提交。

## 日志规范

### 级别策略

| 级别 | 用途 | 隐私要求 |
|---|---|---|
| `error` | 不可恢复错误，影响功能 | 无用户数据 |
| `warn` | 可恢复异常，值得关注 | 无用户数据 |
| `info` | **生产默认级别**，关键生命周期事件 | **严禁**包含用户输入、词库词条、候选词等隐私数据 |
| `debug` | 诊断细节，开发时手动开启 | 可含调试上下文，部署时不应开启 |
| `trace` | 极细粒度追踪 | 仅本地调试 |

`info` 是正式部署时的唯一文件输出级别，开发者需在 `config.toml` 手动配置才能开启更详细级别：

```toml
[debug]
log_level = "debug"   # 或 "trace"
```

### 日志文件

- 滚动策略：**每次服务启动滚动一次**（`log_rotate::rotate_on_startup`，上次运行整体搬入
  历史文件），另按大小兜底（默认 10 MB/文件）；历史文件默认保留 10 个（`debug.log_max_files`）
- 文件命名：`wind_input.log`（本次运行）、`wind_input.1.log`（上次）… `wind_input.10.log`。
  **序号在扩展名之前**，滚动后仍是 `.log`（编辑器可双击、按 `*.log` 可搜）；勿改回 `.log.N` 旧式
- 时间戳为**本地时区**，格式与 `wind_tsf` 的 FileLogger 完全一致，两份日志按时间直接对齐排查；
  勿退回 tracing 默认的 UTC SystemTime timer
- 路径（变体感知）：
  - 正常安装 release：`%LOCALAPPDATA%\WindInput\logs\`
  - 正常安装 dev：`%LOCALAPPDATA%\WindInputDev\logs\`
  - 便携模式：`<exe目录>\userdata\logs\`（以 exe 同目录存在 `wind_portable_mode` 文件为标记）
- 可通过 `RUST_LOG` 环境变量覆盖级别（优先级最高，仅用于开发排查）

### 写日志准则

- `info!` 只记录系统事件（启动/关闭/加载/错误），**不得**记录用户键入的字符、候选词、词库内容
- `debug!` / `trace!` 可含诊断数据，但部署包中不应默认开启

## 构建 / 测试

两套开发脚本命令菜单对齐，按主机选择：

### Windows 本机（MSVC，`scripts\dev.ps1` / `dev.bat`）

- host 即 Windows 目标，无交叉编译限制：`cargo check` / `cargo test` 可直接跑全 workspace
  （含 `wind-coordinator`）。脚本快捷键：`k`=check、`l`=clippy、`t`=test、`f`=fmt、
  `ci`=fmt+clippy+test。
- 全构建：`1`（release → `build/`）/ `d1`（dev → `build_dev/`）；单模块 `m1..m4`（tsf/核心/
  setting/portable，前缀 `d` 为 dev）。系统安装：`p1`/`pd1`；安装包：`8`/`d8` → `dist\*-Setup.exe`。
- 部署目标默认 `C:\Program Files\WindInput[Dev]`，可在 `scripts\deploy.local.ps1` 覆盖。

### Linux 交叉（MinGW，`scripts/dev.sh`）

- 编译检查：`cargo check --target x86_64-pc-windows-gnu -p <crate>`（`wind_input/` 下）。
- host 单测仅限 `wind-engine` / `wind-dict` / `wind-config` / `wind-transform` 等无 Windows
  依赖的 crate；**`wind-coordinator` 传递依赖 `windows` crate，不能在 Linux host 跑测试**。
- 部署调试版到 Windows：`scripts/dev.sh push debug`（配置见 `scripts/deploy.local`）。

### 注意

- 部分集成测试依赖 `build_dev/` 下的数据（junction/词库）；**数据缺失时测试族静默跳过，
  0.0x 秒全绿＝假绿**，在 worktree 里跑测试尤其要核对耗时是否合理。

## 版本 / 发布

- 产品版本**唯一真源 = `docs/VERSION`**。构建脚本读取后分发到 5 类产物
  （`wind_input.exe` / `wind_tsf.dll` / `wind_setting.exe` / `wind_portable.exe` / 安装包），
  跨仓经环境变量 `WIND_APP_VERSION` 注入（不经脚本独立构建时各仓自行回退）。
  发版只改 `docs/VERSION` 一处，**不要**手改各仓 `Cargo.toml` 的 `version`。
- CI（release.yml）为 tag-first：以 tag 覆盖 `docs/VERSION` 再构建；仓库里的 `docs/VERSION`
  是开发占位。**切勿添加 `tag == docs/VERSION` 一致性校验**——会破坏手动触发的 `-dev` 占位流程。
- 草稿 Release 的正文由 `scripts/gen-release-notes.sh` 生成（模板在 `docs/release-notes/`）：
  基础信息 + 人工填写区 + 折叠的提交记录。人工填写区由 `<!-- user-facing:start/end -->`
  圈定，**两个下游按此标记取内容**——文档仓 `scripts/sync_release_notes.py`（官网更新记录）
  与 wind-setting `src/update/notes.rs`（应用内升级提示）。占位文本必须恰好是 `暂未填写`
  （Rust 侧按全等判定），前面加 `>` 之类修饰会让占位符被当成正文弹给用户。

## Agent skills

> 供 Matt Pocock 系列工程技能（`to-tickets` / `triage` / `to-spec` / `qa` / `wayfinder` 等）读取的仓级约定。改动这些约定改对应 `docs/agents/*.md`，无需重跑安装技能。

### Issue tracker

本地 markdown：issue 与 spec 存于 `.scratch/<feature>/`（一 feature 一目录，`issues/NN-<slug>.md` 一票一文件）。详见 `docs/agents/issue-tracker.md`。

### Triage labels

五个规范角色标签，标签名与角色名一致（`needs-triage` / `needs-info` / `ready-for-agent` / `ready-for-human` / `wontfix`），本地追踪器下写作 issue 文件顶部的 `Status:` 行。详见 `docs/agents/triage-labels.md`。

### Domain docs

单上下文：根 `CONTEXT.md` + `docs/adr/`（均按需惰性生成，缺失时静默跳过）；本仓另有 `AGENTS.md` 与 `docs/architecture/` 作为现状架构文档。详见 `docs/agents/domain.md`。
