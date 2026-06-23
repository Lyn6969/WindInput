# AGENTS.md — WindInput 协作约定

> 给在本仓工作的 AI/人类协作者。Rust 核心在 `wind_input/`（cargo workspace）。

## 虚拟键码（VK）—— 用常量，禁止裸十六进制

所有 Windows 虚拟键码统一在 `wind_input/crates/wind-coordinator/src/keymap.rs` 定义为
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

## 构建 / 测试

- 交叉编译检查（Windows 目标，纯 Rust 无 C 依赖）：
  `cargo check --target x86_64-pc-windows-gnu -p <crate>`（`wind_input/` 下，或经 `scripts/dev.sh`）。
- host 单测：仅 `wind-engine` / `wind-dict` / `wind-config` / `wind-transform` 等无 Windows 依赖的 crate 可在本机
  `cargo test -p <crate>` 运行；**`wind-coordinator` 传递依赖 `windows` crate，不能在 host 跑测试**
  （其纯逻辑单测如 `keymap` 仅交叉编译期编译，靠设备/CI 验证）。
- 部署调试版到 Windows：`scripts/dev.sh push debug`（配置见 `scripts/deploy.local`）。
