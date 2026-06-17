# 模式 / 方案统一 —— 设计差分（评审稿）

> 目标态设计，供评审。证据取自 Rust 现状（`wind-coordinator` / `wind-engine`）与 Go decider 模型。
> 上游：[key-pipeline.md]（S0–S4 已完成的按键管线）。本篇是阶段 C 的核心：把"临时拼音 / 特殊
> 模式"等瞬态模式的**引擎来源**统一到方案（scheme）注册表，并为"临时 mix 复合"铺路。
> 决策已与用户对齐方向（对话 2026-06-17），本稿细化为可评审的边界 + 增量路径。

## 0. 为什么要统一（问题陈述）

现状各模式的"引擎/词典来源"不一致，配置能力参差：

| 模式 | 引擎/词典来源（现状） | 配置能力 | 证据 |
|---|---|---|---|
| 普通码表/拼音/混输 | EngineManager 方案注册表（按 schema id） | 完整 `*.schema.toml`（全码策略/排序/…） | `manager.rs` |
| 临时拼音 | **借**一个 pinyin 方案：`engine_mgr.convert_with(schema_id, …)` | 借方案的配置（够用） | `update_temp_pinyin_candidates` |
| 特殊模式（快符） | **自带** `CodetableDict` 塞进 `State.special_tables`，`search_prefix` 直查 | **弱**：仅 S3c 的三档自动上屏，无排序/全码策略/词频 | `coordinator.rs` 特殊模式段 |
| 快捷输入 | 无词典（日期/计算纯函数） | 无 | `quick_input.rs` |
| 临时英文 | 无词典（首版无候选） | 无 | `handle_temp_english_key` |
| URL | 无引擎（纯累积） | 无 | `handle_url_key` |

问题：
1. **特殊模式是二等公民**——自带 `CodetableDict` 绕开了方案注册表，拿不到全码/排序/词频等成套能力，配置面也和 scheme 割裂。
2. **无法组合**——想要"临时同时查 拼音反查 + 生僻字 + 快符"这种 mix overlay，现在没有统一的引擎抽象可叠加。
3. **重复**——特殊模式的码表加载/缓存（`ensure_special_table`）与 EngineManager 的方案加载/缓存是两套。

## 1. 核心模型：base 持久 / overlay 瞬态

关键洞察（已与用户确认）：**方案切换是持久态，模式触发是瞬态 overlay**，两者生命周期不同，不可混为一谈。

- **base scheme**：当前持久激活方案（`engine_mgr.active_schema_id()`），靠 cycle/热键切换，常驻。
- **overlay mode**：按键触发、上屏即**自动回到 base** 的叠加态（`State.active: Option<ModeKind>`，S1 已是此结构）。

> 所以"把特殊模式当方案"**不是**让它变成可 cycle 常驻的方案，而是：**它背后的引擎/词典 = 一个方案
> 实例（由 EngineManager 统一加载/配置），但激活方式仍是 overlay**。base/overlay 的分离保持不变。

统一只动一件事：**overlay 模式的引擎来源，从"各搞各的"收敛到"方案注册表"**。

```
            ┌──────────────── EngineManager（方案注册表，统一加载/缓存/配置）────────────────┐
 base ───▶  │  wubi86 / pinyin / wubi86_pinyin(mixed) / special:quick_symbols / …            │
            └──────────────────────────────────────────────────────────────────────────────┘
 overlay ─▶  Mode { kind, engine: Option<schema_id>, trigger, exit, … }  ← 引用上面的方案
```

## 2. Mode 描述符（Processor-lite，避免过度抽象）

不照搬 Go 的重 `Processor` trait 对象 + `Capability`（S1 已论证 Rust 无共享引擎副作用，trait+Ctx 间接层不划算）。改用**数据描述符 + 现有 enum 分派**：

```rust
/// 一个 overlay 模式的静态描述（表驱动，便于新增模式只加一行）。
struct ModeSpec {
    kind: ModeKind,
    /// 引擎来源：Some(schema_id) → 走方案注册表查询；None → 无词典（快捷/英文/URL 自处理）。
    engine: Option<String>,
    /// 触发与退出策略（沿用 keymap 的键名→VK；激活判定沿用 S4d try_activate_mode）。
    trigger: TriggerSpec,
}
```

- `ModeKind` enum **保留**（S1 决策不翻案）；`handle_*_key` 各模式的交互处理**保留**。
- 变的是：**有引擎的模式**（临拼/特殊）统一通过 `engine_mgr.convert_with(schema_id, code, n)` 取候选，
  候选再走 coordinator 既有的后处理（filter/freq/shadow/全码策略），与普通输入**同一条质量链**。
- `State.special_tables`（`HashMap<u8, CodetableDict>`）**删除**——特殊模式的码表改由 EngineManager 持有。

## 3. 各模式归类与改动量

| 模式 | 引擎来源（目标） | 改动 |
|---|---|---|
| 临时拼音 | `convert_with("pinyin")` | **几乎不动**（已是此模型，仅纳入 ModeSpec 表） |
| 特殊模式 | `convert_with("special:<id>")` | **主要改动**：表注册为方案；删 `special_tables`/`ensure_special_table`/`search_prefix` 直查；候选走质量链 |
| 快捷输入 | None（纯函数） | 不动 |
| 临时英文 | None（首版无候选；将来可挂英文词库方案） | 不动；留挂载点 |
| URL | None | 不动 |

### 特殊模式表 → 方案的映射

`features.special_modes[]{id, name, trigger_keys, table, auto_commit, fixed_length}` 保持为**激活面**配置
（触发键、显示名属于 overlay 行为，不是引擎属性）。新增：把其 `table` 在 EngineManager 注册为一个
**合成方案** `special:<id>`，引擎类型=码表，`CodeTableSpec` 由 special_modes 的 `auto_commit`/`fixed_length`
映射 + 全局 `[input.code_commit]` 继承（复用 S4 的 tri-state 解析）。

> 收益：特殊模式**白嫖**全码自动上屏/顶码/排序/用户词频/shadow 等成套能力；S3c 里手写的
> `decide_special_auto_commit` 可由码表引擎的 `should_auto_commit` 接管（三档 prefix_free/fixed_length/manual
> 对齐到 CodeTableSpec），删重复。

## 4. 临时 mix 复合引擎（融合临拼 / 快符 / 生僻字）

一旦 overlay 引擎都来自方案注册表且接口统一（`convert_with`），"临时 mix" = **`MixedEngine` 的推广**：

```
TempMix overlay: 触发键 → 同时查 [pinyin 反查, special:rare_char, special:quick_symbols]
                 → 复用 MixedEngine 的分层加权合并 → 一个候选列表
                 → 上屏即回 base
```

- 复合定义放 config（如 `features.mix_modes[]{id, trigger_keys, members:[schema_id…]}`）。
- 引擎层加一个 `CompositeOverlayEngine`（或直接复用/推广 `MixedEngine` 支持 N 路）。
- **排在统一之后**，不阻塞；统一落地后这是增量。

## 5. 不照搬 Go

- Go 的 `decider` 持 `Processor` trait 对象 + `applyEngineDiff(Capability)` 挂卸**共享引擎**词典层；
  Rust 各方案按 schema id 独立查询，无共享引擎副作用 → **不引入 Capability/trait 对象**（S1 已定）。
- Go 把激活/退出/回退散在多个 `handle_*` + decider；Rust 已用 S4d `try_activate_mode` 单点 + 统一夺取回退（S3b），
  本阶段**复用**，不重写。

## 6. 增量阶段（每步：编译 + 测试 + 交叉编译 + 提交）

- **M1 引擎来源抽象**：EngineManager 支持注册"合成方案"（给定 id + 码表文件 → 方案实例）；
  暴露按 id 懒加载/查询的稳定 API。host 可测（内存词典）。**不改 coordinator 行为**。
- **M2 特殊模式迁移**：特殊模式候选改走 `convert_with("special:<id>")` + 质量链；删 `State.special_tables`
  与 `decide_special_auto_commit`（由码表引擎全码策略接管）。设备回归：快符 `\` 仍正常。
- **M3 ModeSpec 表驱动**：把临拼/特殊/快捷/英文/URL 的触发与引擎来源收进 `ModeSpec` 表，`try_activate_mode`
  与 active 分派读表。纯重构，行为不变。
- **M4 临时 mix**（可选，按需）：`CompositeOverlayEngine` + `features.mix_modes`，融合临拼/快符/生僻字。

## 7. 待确认决策（评审点）

1. **合成方案的 id 命名与配置面**：用 `special:<id>` 隐式生成，还是让 special_modes 直接写一个 `schema` 字段
   指向真实方案？（倾向隐式生成，special_modes 配置不变，向后兼容。）
2. **特殊模式三档自动上屏**与 CodeTableSpec 的映射：`prefix_free→?`、`fixed_length→auto_commit_min_len+max_code_length`、
   `manual→关`。是否完全用 CodeTableSpec 取代 S3c 的 `decide_special_auto_commit`？（倾向取代，删重复。）
3. **临时 mix 是否本轮做**：还是先 M1–M3 统一、mix 留到确有需求（如生僻字方案就绪）时再上 M4？
4. **临时英文**要不要顺带挂英文词库方案出候选（目前无候选），还是保持纯累积？

---

### 决策小结（供评审）
1. base 持久 / overlay 瞬态**分离不变**；统一只收敛 overlay 的**引擎来源**到方案注册表。
2. 保留 `ModeKind` enum 分派与 `handle_*_key`；删特殊模式的 `CodetableDict`-in-State 旁路。
3. 特殊模式白嫖码表方案的成套能力（全码/排序/词频/shadow），删手写重复。
4. 临时 mix = `MixedEngine` 推广，统一后增量。
5. 不引入 Go 的 Capability/Processor trait 重抽象。
