# 特殊模式 / 临拼 热键直达入口 设计

状态：✅ core 逻辑已实现（config 字段 + 热键编译 + 协调器分发 + 单测绿），待真机手测
对应缺口：Go-parity #5「热键进入模式」

## 背景与目标

Rust 版的特殊模式（自定义码表 / 快符 / 生僻字等，`schema.special_modes`）与临时拼音（`input.temp_pinyin`）目前**只能靠引导键进入**——用户须先在输入框里敲那个触发符（如 `\`、`;`、`` ` ``）。Go 版另有一条路径：为指定模式配一个**专用全局热键**（`enter_special_mode(mode_id)` / `enter_temp_pinyin`），无需敲引导符即可一键直达。

本设计为特殊模式与临拼各补一个**可选专用热键**，与现有引导键**共存**（引导键路径完全不变；热键路径进入时不把引导符写入组合区）。

### 交付边界

- **本仓（core）**：配置字段 + 热键编译 + 协调器分发，复用现有热键基座与已有的模式进入函数。
- **wind-setting（独立原生程序仓）**：设置界面暴露这两个热键字段的 UI 不在本仓范围（同「输入诊断 HUD」的边界约定）。

## 已定决策（本轮 brainstorming）

| 决策点 | 结论 |
|---|---|
| 配置形态 | **绑到模式**：`hotkey` 字段长在 `SpecialModeConfig` / `TempPinyinConfig` 上，与 `trigger_keys` 并列（非独立映射表） |
| 激活语义 | **只进入（幂等）**：已在该模式再按无效；退出统一靠 Esc / 打完（不做 toggle 退出） |
| 半成品处理 | 进入前**先把当前半成品编码上屏**（对齐现有 `commit_and_enter_temp_pinyin`） |
| 触发条件 | **CHINESE_ONLY \| GLOBAL**（与加词键一致）：英文模式放行给宿主；GLOBAL 位穿透 QQNT/Tabby 等 Chromium 宿主 |
| 引导键 | 完全保留、与热键共存；热键进入时组合区**无引导符** |

## 架构

在**已有热键编译/分发链**上加一条支路，不新增窗口、不新增 IPC 命令：

```
config.special_modes[].hotkey / temp_pinyin.hotkey
  │ Compiler::compile()
  ▼
HotkeyEntry{ tsf_hash: raw|CHINESE_ONLY|GLOBAL,
             match_hash: raw,
             action: "enter_special:<id>" | "enter_temp_pinyin" }
  │ 下发 TSF 白名单（含 GLOBAL → RegisterHotKey）
  ▼
入站按键 → match_key_down(norm_hash) → action
  │ coordinator.rs 分发点（add_word/open_add_word_dialog 特判旁）
  ▼
先提交半成品 → enter_special_mode(idx, key_code=0) / commit_and_enter_temp_pinyin
```

## A. 配置（绑到模式）

```toml
[[schema.special_modes]]
  id = "rare"
  trigger_keys = ["backslash"]   # 引导键（保留，共存）
  hotkey = "ctrl+shift+u"        # ← 新增；空串=不注册

[input.temp_pinyin]
  enabled = true
  trigger_keys = ["semicolon"]
  hotkey = "ctrl+shift+p"        # ← 新增；空串=不注册
```

- `SpecialModeConfig`（`wind-config/src/config.rs`）新增 `#[serde(default)] pub hotkey: String`。
- `TempPinyinConfig`（同文件）新增 `#[serde(default)] pub hotkey: String`。
- `config_schema.rs`：`schema.special_modes` 已是 `StructList` 不透明叶子，内部新增字段透明无需登记；**只需补一条** `f("input.temp_pinyin.hotkey", Str)`（供设置程序识别该叶子）。
- **引用完整性天然成立**：hotkey 就长在模式上，不存在悬空 id 需要校验——这是「绑模式」相对「独立映射表」的核心红利。

## B. 编译（`wind-config/src/hotkey.rs::Compiler::compile`）

紧跟现有加词键段的范式，新增两段：

- 遍历 `config.special_modes`，对非空 `hotkey`：
  `parse_hotkey(hotkey)` → `raw`；push `HotkeyEntry { tsf_hash: raw | CHINESE_ONLY | GLOBAL, match_hash: raw, action: format!("enter_special:{}", id) }`。
- `config.input.temp_pinyin.hotkey` 非空：同样策略位，`action = "enter_temp_pinyin"`。
- **id 约束**：`enter_special:<id>` 用 `SpecialModeConfig.id`；id 为空的模式跳过（无法被分发定位）。
- **冲突检测**：v1 不做显式热键冲突检测（与现有热键字段一致，last-wins），YAGNI。

## C. 分发（`wind-coordinator/src/coordinator.rs` 的 `match_key_down` 调用点，约 3315 行）

现状：该点已对 `add_word` / `open_add_word_dialog` 做**调用点特判**（因它们需 `&mut State` 并返回 `KeyAction`），其余 action 走 `dispatch_hotkey`。进入模式同样需 state + 返回 KeyAction，故放此处：

- `action == "enter_temp_pinyin"`：
  - 幂等守卫：若当前已处于该临拼态 → 吞键返回、不重入。
  - 否则 `commit_and_enter_temp_pinyin(...)`（其内部已含「先上屏半成品」）。
- `action` 以 `"enter_special:"` 开头：
  - `strip_prefix` 取 `id` → 在 `config.special_modes` 按**配置序**找 `idx`（`iter().position(|m| m.id == id)`，与 `match_special_trigger` 的下标语义一致，最多 256 项）；找不到则安全忽略（吞键或放行，取吞键以免误触）。
  - 幂等守卫：若 `state.active == Some(ModeKind::Special(idx))` → 吞键返回、不重入。
  - 否则：**先提交当前半成品**（复用现有普通提交路径）→ `enter_special_mode(state, idx, /*key_code=*/0)`。key_code=0 无前缀映射 → `special_prefix` 为空，满足「热键进入不写引导符」。

> 决定：复用 `enter_special_mode(state, idx, key_code)` 现签名，热键路径**传 `key_code = 0`**。理由：0 在 `KEY_TABLE` 无 VK 映射 → `vk_to_prefix_char(0)` 返回 `None` → `special_prefix` 为空，正好表达「无引导符」。不改签名（最小改动面）；用一行注释说明 0 的哨兵语义即可，避免读者当成魔法值。

## D. 行为契约

| 场景 | 结果 |
|---|---|
| 中文模式普通输入按热键 | 上屏半成品 → 进入目标模式（组合区无引导符） |
| 中文模式打字中按热键 | 同上：先把已敲编码提交，再进入 |
| 已在该模式再按同热键 | 幂等吞键，无变化（退出靠 Esc / 打完） |
| 英文模式按热键 | CHINESE_ONLY → 放行给宿主，无作用 |
| Chromium 宿主（QQNT/Tabby） | GLOBAL 位 RegisterHotKey 兜住，可靠触发 |
| 引导键路径 | 完全不变（`trigger_keys` 照旧，含引导符显示） |
| 未知 id 的 `enter_special:` | 安全忽略（吞键） |

## E. 涉及文件

- `wind-config/src/config.rs`：`SpecialModeConfig` / `TempPinyinConfig` 各加 `hotkey: String`。
- `wind-config/src/config_schema.rs`：补 `f("input.temp_pinyin.hotkey", Str)`。
- `wind-config/src/hotkey.rs`：`compile()` 新增 special_modes / temp_pinyin 两段编译。
- `wind-coordinator/src/coordinator.rs`：`match_key_down` 调用点新增 `enter_special:<id>` / `enter_temp_pinyin` 两分支。
- （必要时）`wind-coordinator/src/handle_special.rs`：若采用 `Option<char>` 显式前缀参数则微调 `enter_special_mode` 签名。

## F. 测试

Rust 单测：

- `hotkey.rs`：
  - `special_modes[].hotkey` 非空 → 编出 `action == "enter_special:<id>"`，`tsf_hash` 含 `CHINESE_ONLY | GLOBAL`，`match_hash` 不含任何 policy 位。
  - `temp_pinyin.hotkey` 非空 → `action == "enter_temp_pinyin"`，同策略位断言。
  - 空 `hotkey` / 空 `id` → 不产生条目。
- coordinator 分发：
  - 喂 `enter_special:rare` 的规范化 hash → `state.active == Some(Special(idx))`、组合区无前缀、进入前的半成品已提交进上屏。
  - 幂等：已在该模式二次按 → 状态不变。
  - 未知 id → 安全忽略、无 panic。
  - `enter_temp_pinyin` → 进入临拼态、半成品已提交。

真机手测（C++ 时序，无法单测）：

- 三类宿主（普通 Win32 / WPS / QQNT/Tabby）按热键直达特殊模式与临拼。
- 英文模式按热键 → 宿主收到该键（放行验证）。
- 打字中按热键 → 已敲内容正确上屏、再进入。
- 引导键与热键并存：两条路径都能进入，引导键路径仍显示引导符、热键路径不显示。

## 非目标（YAGNI）

- 不做热键冲突检测/提示 UI（另有 quick-input 触发键冲突检测 spec，属别的功能域）。
- 不做 toggle 退出、不做英文模式强行切中文再进入。
- 设置界面归 `wind-setting` 独立仓，本设计只交付 core 逻辑 + 配置字段。
- 不新增 IPC 命令 / 窗口 / RPC。
