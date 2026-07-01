# AGENTS.md — WindInput 协作约定

> 给在本仓工作的 AI/人类协作者。Rust 核心在 `wind_input/`（cargo workspace）。

## Crate 索引

> workspace 共 18 个 crate（均在 `wind_input/crates/`）。复杂 crate 已配 crate 级 `AGENTS.md`，改动前先读对应文档；新增/重构 crate 时参照同结构补文档。

| Crate | 职责 | crate 文档 |
|---|---|---|
| `wind-coordinator` | 输入法“大脑”：按键路由、状态机、候选与模式切换的中央协调器 | [AGENTS.md](wind_input/crates/wind-coordinator/AGENTS.md) |
| `wind-engine` | Schema 驱动的引擎工厂：拼音/码表/混输三类引擎的构建、切换与候选分发 | [AGENTS.md](wind_input/crates/wind-engine/AGENTS.md) |
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
| `wind-quick-input` | 快捷输入提供器：日期 / 计算器（纯逻辑） | — |
| `wind-reverse` | 候选反查：五笔编码/拆字/拼音读音（悬停 tooltip） | — |
| `wind-punct` | 标点转换纯逻辑（中英标点/全半角/数字后智能） | — |
| `wind-transform` | 文本变换：标点、全角、自动配对、简繁 | — |

`—` = 暂无独立 `AGENTS.md`（多为纯逻辑/工具 crate，职责单一，看 `src/lib.rs` 顶部模块注释即可）。

## 虚拟键码（VK）—— 用常量，禁止裸十六进制

所有 Windows 虚拟键码统一在 `wind_input/crates/wind-keys/src/keymap.rs` 定义为
`pub const VK_*`（`VK_ESCAPE`/`VK_BACK`/`VK_SPACE`/`VK_RETURN`/`VK_PRIOR`/`VK_NEXT`/
`VK_UP`/`VK_DOWN`/`VK_A..VK_Z`/`VK_0..VK_9`/`VK_SEMICOLON` 等）。

- **禁止**在 `match data.key_code` / 比较中写裸 `0x1B`、`0x21` 之类字面量；用 `keymap::VK_*`。
- 触发键名（配置里的 `"backslash"`/`"semicolon"`）→ VK：`keymap::key_name_to_vk(_with_letters)`，
  单一真相源 `KEY_TABLE`，新增键只改一处。
- **注意类型**：`KeyEventData.prev_char`、`CommitRequestData.trigger_key` 是 **u16**
  （UTF-16 码元 / 协议字段），与 VK(`u32`) 比较前需 `as u32` 转换；prev_char 是字符码点不是 VK，
  别套 VK 常量（用数值区间，如 `(0x30..=0x39).contains(&prev_char)` 判数字字符）。

## 候选导航键（翻页 / 高亮 / 二三候选）—— 配置驱动 + 统一逻辑

这些键**都可配置**，且必须走**统一**入口，禁止各 handler 各写一套硬编码判断：

- 翻页 / 高亮：`keymap::NavKeys`（从 `input.page_keys` / `input.highlight_keys` 组名编译）+
  `Coordinator::apply_nav_key(state, data, include_printable)`。普通模式与所有候选模式共用。
  - `include_printable=true`（码表型：普通/特殊/mix/临拼）：`-`/`=`/`[`/`]` 可作翻页；
  - `include_printable=false`（文本/表达式型：临英/快捷输入）：上述键作输入，不当导航。
  - overlay handler 用 `handle_candidate_nav`（按 `state.active` 自判 `include_printable`）。
- 二三候选键：`select_key_offset`（读 `input.select_key_groups`，经 `hotkey::select_key_vks`）。
- 新增模式/按键时**复用**以上，不要再写 `0x21|0x22 =>` 之类分支。

## 提交纪律（多会话共仓）

可能有多个 AI 会话同时在本仓工作。**提交只用显式路径**（`git add <具体文件>`），
**禁止 `git add -A` / `git add .`**——会把其它会话未提交的文件一起卷入提交。
提交前 `git status` 确认暂存区只含自己改的文件。

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

- 滚动策略：按大小（默认 10 MB/文件），保留最近 N 个文件（默认 5 个）
- 文件命名：`wind_input.log`（当前）、`wind_input.log.1`（上一个）…
- 路径（变体感知）：
  - 正常安装 release：`%LOCALAPPDATA%\WindInput\logs\`
  - 正常安装 dev：`%LOCALAPPDATA%\WindInputDev\logs\`
  - 便携模式：`<exe目录>\userdata\logs\`（以 exe 同目录存在 `wind_portable_mode` 文件为标记）
- 可通过 `RUST_LOG` 环境变量覆盖级别（优先级最高，仅用于开发排查）

### 写日志准则

- `info!` 只记录系统事件（启动/关闭/加载/错误），**不得**记录用户键入的字符、候选词、词库内容
- `debug!` / `trace!` 可含诊断数据，但部署包中不应默认开启

## 构建 / 测试

- 交叉编译检查（Windows 目标，纯 Rust 无 C 依赖）：
  `cargo check --target x86_64-pc-windows-gnu -p <crate>`（`wind_input/` 下，或经 `scripts/dev.sh`）。
- host 单测：仅 `wind-engine` / `wind-dict` / `wind-config` / `wind-transform` 等无 Windows 依赖的 crate 可在本机
  `cargo test -p <crate>` 运行；**`wind-coordinator` 传递依赖 `windows` crate，不能在 host 跑测试**
  （其纯逻辑单测如 `keymap` 仅交叉编译期编译，靠设备/CI 验证）。
- 部署调试版到 Windows：`scripts/dev.sh push debug`（配置见 `scripts/deploy.local`）。
