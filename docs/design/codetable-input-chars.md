# 码表码元字符集 `input_chars` 设计

> **范围**（2026-08-05 拍板）：只做**码表引擎**的码元字符集，字符档位为**字母子集 + ASCII 标点**
> （如 `a-x`、`a-x/`、`a-z;`）。**不含数字**——数字要动 C++ 吃键与 IPC，另立。
> 双拼符号（`;`）不在本设计内，它有自己的处理路径。驱动场景为通用能力建设，无特定方案。

---

## 1. 现状：字段就位，消费点为零

`wind-config/src/schema.rs:100-102` 已有字段，方案 `.schema.toml` 写 `[engine.codetable] input_chars = "a-x/"`
**能解析成功且不报错**：

```rust
/// 输入码字符集，如 "a-x" / "a-x/" / "a-z"。空=回退全局/默认。
#[serde(default)]
pub input_chars: String,
```

但全仓 `input_chars` 仅 3 类命中：本定义处、`docs/redesign/*` 设计稿、`docs/design/codetable-candidate-ordering.md`
的结构体引用。**无任何读取点**。三层佐证：

| 层 | 状态 |
|---|---|
| `CodetableGlobal`（全局基线） | **无对应字段**——注释里「空=回退全局」指向虚空 |
| `CodetableGlobal::resolved()`（`wind-config/src/config.rs:517`） | 折叠 9 个行为字段 + frequency 段，**不含 `input_chars`** |
| coordinator 按键分派 | 仍是硬编码 `keymap::VK_A..=keymap::VK_Z`（`coordinator.rs:5449`） |

对照组证明这是遗漏而非有意：同段的另外两个「引擎固定参数」都接了线——`max_code_length`
（`manager.rs:652/2255/2357`）、`base_sort`（`manager.rs:2378`）。

> ★ **判据**：一个配置项是否真实装，唯一可靠证据是 grep 消费点，不是结构体、不是设置页、不是文档。
> 本仓已多次出现「配置四层就位、消费点接在不可达调用点上」（见 `project_english_commit_space`）。

---

## 2. 可行性地基：为什么改动面比预想小一个数量级

三处调研结论，共同把改动面收敛到 **coordinator 分派层 + 配置折叠层**：

### 2.1 引擎层对码元字符零假设 ⇒ 零改动

`wind-engine/src/codetable/engine.rs`：

```rust
fn convert(&self, input: &str, max_candidates: usize) -> anyhow::Result<ConvertResult>
```

纯字符串键；码长一律 `input.chars().count()`（`:121`、`:223`、`:266`）。
整个 `wind-engine/src/codetable/` 目录中 `is_ascii_lowercase` / `b'a'` / `'a'..=` **零命中**。

### 2.2 词典层已非纯 26 字母 ⇒ 零改动

`wind-dict/src/codetable.rs:76` 的 `is_code_shape` 已接受 `a-z`、数字、空格、`'`、`;`。
（注意它是**列序猜测**的启发式，不是码元校验器——见 §6「明确不做」。）

### 2.3 C++ 中文模式已吃下全部字母与标点 ⇒ 零改动、零 IPC

`wind_tsf/src/HotkeyManager.cpp:106-161` + `KeyEventSink.cpp:611-710` 的吃键真相表：

| 键类型 | 中文模式吃键条件 |
|---|---|
| `Letter`（A-Z） | **无条件吃**（仅 CapsLock 透传例外） |
| `Punctuation`（`IsPunctuationKey` + Shift+数字） | **无条件吃**（仅 CapsLock 例外） |
| `Number`（主键盘 0-9 无 Shift、小键盘） | **仅 `hasInputSession` 时吃**，空缓冲透传（全角例外） |
| Space / Enter / Backspace / Esc / 方向 | 仅 `hasInputSession` |

⇒ 字母与 ASCII 标点在中文模式下**本来就已送达 core**，只是 core 把标点分流进了标点流水线
而非输入缓冲。本设计做的是**在 Rust 侧改分流**，不是扩 C++ 吃键集。

> ★ 铁律「C++ 吃键集 ⊆ Rust 出字集」（`project_fullwidth_eat_flip`）在此**仍然成立且方向有利**：
> C++ 目前吃得比 Rust 用得多，本设计缩小的是分歧，不扩大。
> 一旦档位升到含数字，方向反转、铁律立刻生效——那时必须照 `CONFIG_KEY_CUSTOM_EN_PUNCT`
> （`BinaryProtocol.h:627` + `_IsCustomEnglishPunctKey` 数据驱动查表）的模板走，不得新开机制。

---

## 3. 设计

### 3.1 配置模型与归属

```
方案 [engine.codetable].input_chars  (已存在，String，空=回落)
        ↓ read_schema 的 merge_toml 深合并 schema_overrides/{id}.toml
全局 schema.codetable.input_chars     (新增，String，默认 "a-z")
        ↓ CodetableGlobal::resolved()
解析为 CodeCharSet，存进 CodeTableEngine
```

**归属决定：`CodeCharSet` 挂在 `CodeTableEngine` 上，coordinator 经
`EngineManager::active_input_chars()` 取。**

理由——`active_max_code_length()`（`manager.rs:864`）已经是这个形状，照抄即可：

```rust
pub fn active_max_code_length(&self) -> usize {
    self.active_engine().map(|e| e.max_code_length()).unwrap_or(...)
}
```

码元集与 `max_code_length` 是同一性质的东西（方案级引擎固定参数、按方案解析、按键热路径每键要查），
放同一处天然一致。

> ⚠️ **不要预解析进 `ConfigBundle`**。`custom_en_punct_chars` 那样做是对的（它是**全局**的），
> 但 `input_chars` 是**方案级**的，塞进全局快照就会在方案切换时读到别的方案的码元集。
> 参见 `project_schema_switch_and_english`：三个方案切换入口的行为已经漂移过。

> ⚠️ **混输引擎必须显式定义取哪一个**。`active_engine()` 在混输下返回混输引擎（主码表 + 次拼音）。
> 建议：取**主码表子引擎**的码元集；拼音侧不受 `input_chars` 约束。
> 这是 `project_mixed_pinyin_exact_tier` / `project_mixed_overflow_vs_topcode` 反复栽过的地方——
> **混输的任何「按来源分流」都要三条通路各自确认**，不可默认继承。

### 3.2 `CodeCharSet` 解析器

格式：范围 + 字面集，如 `"a-x"`、`"a-x/"`、`"a-z;'"`。

- **范围**：`X-Y`（`X <= Y`，同为 ASCII）。
- **字面**：其余字符逐个收入。
- **`-` 作字面**：仅当位于**首位或末位**（`"-a-z"` / `"a-z-"`），与正则字符类惯例一致。
- **大小写**：一律 `to_ascii_lowercase` 归一后入集——`input_buffer` 恒存小写
  （`coordinator.rs:5450` 注释：「缓冲恒存小写，z-fallback 探针、顶码判定、引擎查询、词频记账全部只看它」），
  码元集必须同域，否则集内 `A` 永不命中。
- **存储**：`[bool; 128]` 位图。按键热路径每键查一次，别用 `BTreeSet` 做哈希/比较。
- **非法输入**：解析失败 → 记 `warn!` 并**回落默认 `a-z`**，不 panic、不静默变空集。
  空集会让方案完全打不出字，是比忽略配置严重得多的故障。

### 3.3 优先级契约（核心，也是唯一真正需要拍板的部分）

`handle_key_event` 尾段现有优先级链（`coordinator.rs:5189-5282`）：

```
退格夺取回退 → 已激活模式单点分派 → try_activate_mode(空缓冲模式激活)
  → Ctrl/Alt → URL 前缀夺取 → select_char(以词定字) → apply_nav_key(翻页/高亮)
  → numpad(小键盘 direct) → 大 match(Escape/Back/数字选词/VK_A..=VK_Z 累积/兜底标点)
```

**契约：组码中的码元优先，空缓冲一律让位。**

| 缓冲状态 | 码元集内的**符号**键 | 依据 |
|---|---|---|
| 空缓冲（`input_buffer` / `candidates` / `committed_text` 皆空） | **行为完全不变**：模式激活 / 标点 / 透传 | 零回归 |
| 组码中 | 作**码元累积**，抢在 `select_char` / `apply_nav_key` / 大 match 之前 | 正在组码，符号是码的一部分 |

这个分层**不是新发明**，是仓里既有形状的推广：
- `select_char` 闸门自带缓冲守卫（`:5250-5252`），注释明说「空缓冲且无候选时放行，让 `,`/`.` 作普通标点」；
- `try_activate_mode` 的语义本身就是「**空缓冲**模式激活」（`handle_lifecycle.rs:64-66`）。

**插入位置**：`try_activate_mode` 之后、`select_char` 之前，加一道带缓冲守卫的码元闸门。

**逃生口**：`input_chars_leading: bool`（默认 `false`）——允许码元符号打头，供「码元集含符号且该符号可作首码」
的方案使用。**默认关 = 零回归**；打开后空缓冲下该符号也进缓冲，其引导键/标点身份让位。

> ★ **判据**：本仓「加任何让位类开关」的教训（`project_enter_behavior_multipath`）——
> 必须确认**接手职责的键没被别的配置同时收走**。开 `input_chars_leading` 前要检查该符号是否同时
> 配了临拼触发 / 临英触发 / 特殊模式引导 / `select_char_keys` / `page_keys`，冲突时**在配置校验期
> 报警**，而不是让用户在真机上撞见静默失效。

### 3.4 非码元字母的处置（`a-x` 下的 `y`/`z`）

必须显式定义，否则 §3.3 只解决了符号、没解决字母子集。

| 缓冲状态 | 非码元字母 | 理由 |
|---|---|---|
| 空缓冲 | **透传**（宿主出该字母） | 与 CapsLock 字母透传同构，保留 `WM_KEYDOWN` 给宿主快捷键 |
| 组码中 | **先上屏当前高亮候选，再输出该字符**（`commit_highlight_then_char`） | 与小键盘 direct 语义同构（`:5271-5279`），不丢已打的码 |

**顺序铁律：字母触发键判定必须先于「非码元」判定。**
`z` 常同时是「非码元」（`a-x`）与「临拼触发键」（`matched_letter_temp_trigger`，`handle_temp.rs:47`）。
若先判非码元，`z` 会被上屏顶掉，临拼永远进不去。

> 顺带的设计红利（**本次不做**，记为后续机会）：现在 z-fallback 靠「加此键后是否破活码前缀」的
> 启发式判定（`handle_temp.rs:100-108`）。有了 `input_chars` 后它可以变成确定性判定
> （`z ∉ 码元集` ⇒ 必是触发键）。但这会改动一条已稳定的路径，不在本设计范围。

---

## 4. 缓冲出口兼容性盘查清单

符号进 `input_buffer` 后，所有「假设缓冲只含 a-z」的下游都要逐个确认。
**这份清单是本设计最容易翻车的部分**——本仓多次栽在「一个能力五处落点，读端接了写端没接」
（`project_special_mode_codetable`）与「上屏原码有四个同源出口」（`project_normal_input_uppercase`）。

| # | 出口 | 要确认什么 |
|---|---|---|
| 1 | `input_buffer_cased` 影子串 | 符号无大小写。两串的同步规则要显式定义，**失配即作废**的既有机制是否仍成立 |
| 2 | 顶码判定 / `handle_top_code` | 符号是否计入「满码 +1」的码长 |
| 3 | 「上屏原码」四个同源出口 | 符号能否原样还原 |
| 4 | 词频记账 `record_selection` | 记账码按来源分流，读写调试三处同口径（`project_freq_system`） |
| 5 | preedit 投影 / 编码栏显示 | 符号在组合区的呈现 |
| 6 | 退格回退 / `committed_segs` | 分步上屏后退格（`project_shuangpin_partial_backspace` 已有未修缺陷） |
| 7 | z-fallback 探针 | 探针拼接 `format!("{}{}", buffer, ch)` 对符号是否仍成立 |
| 8 | 加词取码 `dict.add` code 推导 | 该推导本就「只声明未实现」（`project_dict_add_code_derivation`） |
| 9 | 拆字反查 / tooltip「编码」段 | 按方案词库反查时的码元域 |
| 10 | URL 前缀夺取探针（`:5226-5234`） | `printable_char` 已支持符号，与码元闸门的先后顺序 |

---

## 5. 分期

| 期 | 内容 | 行为变化 |
|---|---|---|
| **P1** | `CodeCharSet` 解析器 + 全局字段 + `resolved()` 折叠 + `CodeTableEngine` 持有 + `active_input_chars()` | **无**（默认 `a-z`，与现状逐键等价） |
| **P2** | 非码元字母处置（§3.4） | 仅对配了字母子集的方案生效 |
| **P3** | 符号码元 + 组码中优先（§3.3） | 仅对码元集含符号的方案生效 |
| **P4** | `input_chars_leading` 逃生口 + 配置期冲突校验 | 默认关 |

P1 是纯地基、可独立合入且**可证明零行为变化**——这一点很重要，它让后续每期的回归都有干净的对照基线。

---

## 6. 测试策略

- **单元**：`CodeCharSet` 解析——范围 / 字面 / `-` 首末位 / 大小写归一 / 非法输入回落 `a-z` 而非空集。
- **零回归锁（P1 最关键）**：默认 `a-z` 时，`wind-coordinator/tests/input_flow.rs` 全绿且**无一处断言修改**。
  断言需要改 = P1 没做到「无行为变化」。
- **端到端**：`a-x` 方案下 `y` 键行为（空缓冲 vs 组码中）；`a-x/` 方案下 `/` 在组码中作码元、空缓冲仍作引导键。
- **反向对照**：每条「符号作码元」的用例都要配一条「同符号在空缓冲下仍是标点/引导键」的对照。
  只测正向会漏掉「闸门吃得过宽」这一整类缺陷。

> ⚠️ **报「全量通过」前先读 `project_build_dev_data_missing`**：`build_dev/data` 缺失时，
> 依赖真实词库的测试会**静默跳过、计数照常绿**，唯一判据是耗时。

---

## 7. 明确不做

| 项 | 原因 |
|---|---|
| **数字作码元** | 需扩 C++ 吃键（空缓冲下数字不送 core）+ 新增 `CONFIG_KEY` 下发，且与数字选词键正面冲突、需要一整套让位规则。风险高一档，另立设计。 |
| **双拼符号自定义** | 双拼有自己的处理路径（`;`），与按键分派优先级无关，混入会让本设计失焦。 |
| **dict 层码元校验** | 设计稿 `config-schema.md:61` 提到「码表词条的合法码元据此校验」。但 `is_code_shape` 是**列序猜测**的启发式，把它改成校验器会牵动解析语义——**须 bump `PARSE_SEMANTICS_VERSION`**（`project_dict_column_layout_fix`）。独立议题。 |
| **z-fallback 确定性化** | 红利明确但改动一条已稳定路径，见 §3.4 末。 |
| **通用全字符集** | 冲突面最大，需要完整定义与选词/翻页/引导/标点的优先级契约。本设计的分层契约（§3.3）是它的前置。 |

---

## 8. 相关代码位置

- `wind_input/crates/wind-config/src/schema.rs:100-102`：`CodeTableSpec::input_chars`（现有字段）
- `wind_input/crates/wind-config/src/config.rs:517`：`CodetableGlobal::resolved()`（折叠点）
- `wind_input/crates/wind-engine/src/manager.rs:864`：`active_max_code_length()`（`active_input_chars()` 的模板）
- `wind_input/crates/wind-engine/src/codetable/engine.rs:175`：`convert()`（纯字符串接口，零改动）
- `wind_input/crates/wind-coordinator/src/coordinator.rs:5189-5282`：优先级链与插入位置
- `wind_input/crates/wind-coordinator/src/coordinator.rs:5449`：待替换的 `VK_A..=VK_Z` 硬编码
- `wind_input/crates/wind-coordinator/src/handle_lifecycle.rs:64-67`：`try_activate_mode`（空缓冲模式激活）
- `wind_input/crates/wind-coordinator/src/handle_temp.rs:47`：`matched_letter_temp_trigger`（顺序铁律）
- `wind_tsf/src/HotkeyManager.cpp:106-161`：`ClassifyInputKey`（吃键真相表来源）
- `wind_tsf/include/BinaryProtocol.h:627`：`CONFIG_KEY_CUSTOM_EN_PUNCT`（升档到数字时的模板）
- `docs/redesign/config-schema.md:53-70`：原始设计稿 §3b
- `docs/redesign/coordinator.md:110`：待办第 11 条（本设计即其兑现）
