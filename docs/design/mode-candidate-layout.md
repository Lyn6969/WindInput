# 模式级候选布局（强制竖排 / 横排）统一设计

状态：✅ 已并入 main（core + 设置页 UI 均完成，两仓测试全绿），**待真机手测**
起因：快捷输入已有「强制竖排」，用户要求临时拼音 / 临时英文 / 特殊模式（快符、生僻字）等同样支持。

> 实施中对设计做了三处修正，已就地更新到下文并标注：**不引入 `serde(flatten)`**（§4.2）、
> **收口点是两个而非一个**（§5.3）、**热重载之外还须接命令栏切换**（§5.4）。

## 背景与目标

目标不是「再抄四份 force_vertical」，而是让**模式级候选布局**成为一个有统一表达、统一执行路径的概念：

- **配置上统一**：所有模式用同一个字段名、同一套取值，用户在任何模式的设置里看到的都是同一件事。
- **逻辑上统一**：布局的「覆盖 → 恢复」只有一处实现，新增模式只加一行映射，不再各写一份保存/恢复。
- **不牺牲灵活性**：每个特殊模式 / 融合实例可以各自设定（快符竖排、生僻字横排互不影响），并且要能表达「强制横排」——现有布尔表达不了。

---

## 一、现状盘点（实施前的事实基线）

### 配置面：只有一个键，且挂错了地方

| 项 | 位置 |
|---|---|
| `schema.quick_input.force_vertical`（bool） | `wind-config/src/config.rs:604-606`、manifest `config_schema.rs:122`、预置 `data/config.toml:156` |

判定在 `handle_mode.rs:163`：

```rust
if self.mix_has_quick_input(idx) && self.rt().config.schema.quick_input.force_vertical
```

**语义错位**：配置挂在 `schema.quick_input` 全局段，判定条件却是「**这个 mix 实例**的 members 里有没有 quick 成员」。它实际上是某个 mix 实例的显示属性，却存在一个与实例无关的段里。后续所有扩展困难都由此而来——想给第二个 mix 实例单独设一个方向，现有形态根本表达不了。

### 执行面：已经有两套并行实现

| 模式 | 状态字段 | 保存点 | 恢复点 |
|---|---|---|---|
| mix（快捷） | `state.quick_saved_vertical`（`coordinator.rs:463`） | `handle_mode.rs:164-172` | `handle_mode.rs:255-258` + `handle_lifecycle.rs:266-269` |
| 快捷加词 | `state.add_word_saved_vertical` | `handle_addword.rs:556-562`（**无条件硬编码**，没有开关） | `handle_addword.rs:589-591` + `handle_lifecycle.rs:276-278` |

两套逻辑形状完全相同，各写一遍。

### 待接入的模式与它们的出口数量

`ModeKind::{TempPinyin, TempEnglish, Url, Special(idx), Mix(idx)}`，加上不在 `state.active` 里的 `add_word_active`。

`state.active` 在 `wind-coordinator/src` 内共 **8 处写 `Some(..)`、8 处写 `None`**，分布在 6 个文件（`handle_lifecycle.rs` / `handle_mode.rs` / `handle_special.rs` / `handle_temp.rs` / `handle_url.rs` / `handle_candidate.rs`）。

### 布局的运行时真相源

- `Coordinator::candidate_vertical: Mutex<bool>`（`coordinator.rs:765`）是**运行时镜像**，由命令栏 `ime.toggle("layout")`（`cmd_toggle_layout`，`coordinator.rs:2318`）切换并持久化。
- `apply_runtime_config`（`coordinator.rs:1887-1893`）在配置热重载时**无条件**按 `config.ui.candidate.layout` 覆写镜像并下发 `SetCandidateLayout`。
- 现有强制竖排的「保存」读的却是 `rt().config.ui.candidate.layout`（`handle_mode.rs:164-170`、`handle_addword.rs:556-560`），**不是镜像**。

---

## 二、诊断：照抄现有做法会崩在哪

按现有形状给 4 个模式各加一份，会得到这些确定性的问题：

**1. 恢复点的组合爆炸。** 6 个 `*_saved_vertical` 字段 × 平均 3 个恢复点 ≈ 18 处手写恢复。`handle_lifecycle.rs:266-278` 里已经为两个模式各写了一遍——那正是在补第 3、第 4 个出口。漏一处的表现是「候选窗卡在竖排、用户找不到原因」，且**没有任何日志**。同构问题在 `mixed_overflow_vs_topcode` 上已经栽过两次（「否决开关必须三处都接」）。

**2. 保存的快照会过期。** 强制期间发生配置热重载，`apply_runtime_config` 会直接把强制态清掉；而 state 里的 `saved` 仍在，退出时又按旧快照改回去——两次错误叠加。同样，命令栏切换布局改的是镜像，而快照读的是 config，在写盘到热重载回灌之间存在窗口期，恢复会恢复到错误方向。这是 `runtime_mirror_state_config_sync` 记过的同一类坑。

**3. 布尔表达力不足。** 全局横排的用户想要「快符竖排」；全局竖排的用户想要「临英横排」（英文候选一行放得下，竖排反而占屏）。`force_vertical: bool` 只能表达前者。加第二个 `force_horizontal` 会出现 `true/true` 这种非法组合。

**4. 配置面分散。** 同一件事散在 `schema.quick_input.*` / `input.temp_pinyin.*` / `input.temp_english.*` / `schema.special_modes[]` 四个不同的域，用户要在设置页四个页面各找一次。

---

## 三、已定决策

| 决策点 | 结论 |
|---|---|
| 取值形态 | **三态** `follow` / `vertical` / `horizontal`，默认 `follow`（不是布尔） |
| 字段名 | 所有模式统一叫 `candidate_layout`，与 `ui.candidate.layout` 共用取值词汇 |
| 粒度 | **每实例**：`mix_modes[]` / `special_modes[]` 各条目自带；单例模式各自一个键 |
| 挂载方式 | 抽共享子结构 `ModeDisplayConfig`，`#[serde(flatten)]` 内嵌，保持 TOML 扁平 |
| 执行机制 | **声明式重算**（每次显示前按当前状态算），不再保存/恢复快照 |
| 基线来源 | `candidate_vertical` 运行时镜像，**不读 config** |
| 收口点 | `notify_ui_update`（`coordinator.rs:2366`） |
| 旧键处理 | `quick_input.force_vertical` 加载期迁移后删除（对齐 `enable_english` 先例） |

---

## 四、配置面设计

### 4.1 三态枚举

```rust
/// 模式级候选布局意图。
/// - Follow：跟随全局 ui.candidate.layout（用户改全局，本模式跟着改）
/// - Vertical / Horizontal：进入该模式期间覆盖全局方向，退出自动回到全局
///
/// 刻意与 ui.candidate.layout 共用取值词汇（"vertical"/"horizontal"），让「模式级设置」
/// 与「全局设置」在用户眼里是同一件事的两个层级，而不是两套发明出来的开关名。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum LayoutIntent {
    #[default]
    Follow,
    Vertical,
    Horizontal,
}
```

**为什么必须是三态而不是布尔**：`follow` 与 `vertical` 的区别只在**全局本身是竖排时**才显现——`follow` 表示「跟着全局走」，`vertical` 表示「这个模式永远竖排」。布尔把这两种意图压成同一个 `true`，将来想区分就得做破坏性改键。

### 4.2 共享子结构：**评估后未采用**（实施修正）

原设计要把字段包进 `ModeDisplayConfig` 并 `#[serde(flatten)]` 内嵌，为未来的第二个模式级
显示属性预留位置。实施时否决，理由是两条硬事实：

1. **仓库里没有任何 `serde(flatten)` 先例**（全 workspace grep 零命中）。
2. flatten 与 `toml` 序列化有已知摩擦（flatten 产生的 map 被当作 table，而 TOML 要求
   value 先于 table 输出）。本仓的 `Config::default()` **必须**能序列化成 `toml::Value`
   ——三层合并与 capability 生成都建立在这一步上，赌它不出问题的收益不足以匹配风险。

改为在六个结构体上各加一个同名字段 `candidate_layout: LayoutIntent`。**统一性由字段名与
类型保证，不由结构体嵌套保证**；真到了第二个属性出现时再抽结构不迟，那时也有了真实的
第二个样本来决定该抽什么。

### 4.3 挂载点

| 模式 | 配置键 | 粒度 | 默认 |
|---|---|---|---|
| 融合 mix | `schema.mix_modes[].candidate_layout` | 每实例 | `follow`（`quick_mix` 由迁移置为 `vertical`） |
| 特殊模式 | `schema.special_modes[].candidate_layout` | 每实例 | `follow` |
| 临时拼音 | `input.temp_pinyin.candidate_layout` | 单例 | `follow` |
| 临时英文 | `input.temp_english.candidate_layout` | 单例 | `follow` |
| 网址 | `input.url.candidate_layout` | 单例 | `follow` |
| 快捷加词 | `input.add_word.candidate_layout` | 单例（**需新建配置段**） | `vertical`（保持现状行为） |

mix / special 走每实例是灵活性的关键：快符表要竖排、生僻字表可能要横排，它们是 `special_modes` 里两个独立条目。这同时把 `quick_input.force_vertical` 的语义错位一并修正——它本来就该是 `quick_mix` 这个实例的属性。

加词模式当前是无条件硬编码强制竖排，本次顺带给它一个配置出口，默认值取 `vertical` 以保持行为不变。

### 4.4 TOML 形态

```toml
[[schema.special_modes]]
  id = "rare"
  trigger_keys = ["backslash"]
  candidate_layout = "vertical"   # ← 新增

[input.temp_pinyin]
  trigger_keys = ["semicolon"]
  candidate_layout = "follow"     # ← 新增

[input.add_word]
  candidate_layout = "vertical"   # ← 新增段
```

⚠️ **`mix_modes` 的默认值只能写在代码侧 `default_mix_modes()`**。`data/config.toml:151-157` 已明确记载：在预置文件里写 `mix_modes` 会把整份定义冻结成快照。不要为了「让用户看得见」而顺手把 `mix_modes` 整段落盘——那是 `dict_override_sparse_merge` 记过的坑的复刻。预置文件里只留注释说明去哪改。

---

## 五、逻辑面设计：从「保存/恢复」改为「声明式重算」

### 5.1 核心转变

现有实现是**命令式**的：进入时把旧值快照进 state，退出时回放。代价随退出路径数线性增长，而退出路径有 8 条。

新实现是**声明式**的：

> 任何时刻的候选方向 = f(全局基线, 当前模式意图)

不存快照，只在每次要显示候选前重算一次。「恢复」不再是一个需要被执行的动作，而是模式退出后重算的自然结果。

### 5.2 两个纯函数（新建 `wind-coordinator/src/layout.rs`）

```rust
/// 当前生效的布局意图。**唯一一处**把「模式 → 布局」映射写死的地方——
/// 新增模式只在这里加一行。优先级：加词 > 独占模式 > 全局。
///
/// add_word 不在 state.active 里（是独立的 add_word_active 标志），
/// 所以「当前模式」的判定必须把它一起收进来，这正是需要一个集中函数的理由。
fn layout_intent(&self, state: &State) -> LayoutIntent {
    let cfg = &self.rt().config;
    if state.add_word_active {
        return cfg.input.add_word.candidate_layout;
    }
    match state.active {
        Some(ModeKind::Mix(i))      => self.mix_cfg(i).map(|m| m.candidate_layout),
        Some(ModeKind::Special(i))  => self.special_cfg(i).map(|s| s.candidate_layout),
        Some(ModeKind::TempPinyin)  => Some(cfg.input.temp_pinyin.candidate_layout),
        Some(ModeKind::TempEnglish) => Some(cfg.input.temp_english.candidate_layout),
        Some(ModeKind::Url)         => Some(cfg.input.url.candidate_layout),
        None                        => None,
    }
    .unwrap_or_default()   // 配置缺项 / 下标越界一律回落 Follow
}

/// 期望的实际方向 = 意图叠加到基线上。
/// 基线取运行时镜像 candidate_vertical，**不读 config**——命令栏 ime.toggle("layout")
/// 之后到热重载回灌之前，config 是陈旧的，读它会恢复到错误方向。
fn desired_vertical(&self, state: &State) -> bool {
    match self.layout_intent(state) {
        LayoutIntent::Vertical   => true,
        LayoutIntent::Horizontal => false,
        LayoutIntent::Follow     => *self.candidate_vertical.lock().unwrap_or_else(|e| e.into_inner()),
    }
}
```

### 5.3 接入收口点：**两个**（实施修正）

原设计以为 `notify_ui_update` 是唯一必经点。实际枚举 `UiCommand::UpdateCandidates` 的发送点
得到**两处**：

| 发送点 | 服务对象 |
|---|---|
| `coordinator.rs` 的 `notify_ui_update` | 主输入与全部 `ModeKind` 模式 |
| `handle_addword.rs` 的 `show_add_word_preview` | 加词面板（**独立绘制路径，不经 notify_ui_update**） |

两处都要接。漏掉第二处的后果是加词的 `candidate_layout` 完全失效——而它默认值是
`vertical`，用户会立刻发现。

**教训**：「必经收口点」必须靠枚举命令的发送点来确认，不能靠「主流程都走这儿」的直觉。

在每个发送点的 `UpdateCandidates` **之前**插入：

```rust
let want = self.desired_vertical(state);
let mut last = self.candidate_layout_sent.lock().unwrap_or_else(|e| e.into_inner());
if *last != want {
    *last = want;
    let _ = self.ui_tx.send(UiCommand::SetCandidateLayout(want));
}
```

新增字段 `candidate_layout_sent: Mutex<bool>`（初值同 `candidate_vertical` 初值）。

**它不是优化而是必需**：没有去重就会每次按键都发一条 `SetCandidateLayout`，UI 侧 `set_vertical` 会触发重排/重绘，在首显时序敏感的路径上（`candidate_first_show_modes` 记过的那类坑）引入抖动。

**`HideCandidates` 的早返回路径不需要重算**：布局只在显示时有意义。退出模式必然伴随一次隐藏 + 下一次显示，恢复发生在「显示之前」而非「隐藏之时」。且两条 `UiCommand` 在同一个 channel 里按序处理，`SetCandidateLayout` 先于 `UpdateCandidates` 到达 UI，不会闪。

### 5.4 其余会绕过覆盖的下发点（实施修正）

除两个收口点外，还有两处**直接**发 `SetCandidateLayout`，都会绕过模式覆盖，必须一并改成
「改基线镜像 → 调 `sync_candidate_layout`」：

| 位置 | 原行为 | 漏改的后果 |
|---|---|---|
| `apply_ui_config`（函数名，非原设计写的 `apply_runtime_config`） | 热重载时无条件下发 config 值 | 模式进行中改任意一项设置都会静默取消强制竖排；无 saved 字段可查，表现为「偶尔失效」，极难复现 |
| `cmd_toggle_layout`（命令栏 `ime.toggle("layout")`） | 翻转镜像后直接下发新值 | 在强制竖排的模式里切换会绕过覆盖直接改方向，且去重缓存与真实下发值脱节，后续判断连锁出错 |

两处的共同点：它们改的都是**基线**，而基线不等于最终下发值。改完基线必须重新叠加当前
模式意图。加锁顺序统一为 `state → candidate_layout_sent`，与 `notify_ui_update` 一致，不构成环。

### 5.5 这一步删掉的东西

- `state.quick_saved_vertical`、`state.add_word_saved_vertical` 两个字段 → 删除
- `handle_mode.rs` 的 `enter_mix_mode` 保存 + `exit_mix_mode` 恢复 → 删除
- `handle_addword.rs` 的进入保存 + `exit_add_word_mode` 恢复 → 删除
- `handle_lifecycle.rs` 的 `reset_exclusive_modes` 里两段恢复 → 删除
- 未来 4 个模式本该新增的 ~12 处保存/恢复 → 不存在

**自愈性质**：即使某条退出路径什么都没做（例如失焦走 `reset_exclusive_modes`、或将来新增一条谁都没想到的退出路径），下一次候选显示时会自动算回基线。这正是 `toolbar_flash_stale_focus_lost` 总结的「给新状态接出口要找必经收口点，而不是枚举消费点」。

---

## 六、优先级与边界

**优先级链**（`layout_intent` 内固定，取第一个匹配项）：

```
add_word_active  >  state.active 对应模式  >  Follow（回落全局基线）
```

加词优先于底层模式：加词是一个覆盖在任意输入态之上的临时面板，它的显示需求（多字符逐字确认）与底层模式无关。

**不在本设计范围**：

- 候选**每页数量**不随布局变化。竖排显示不下时的分页由现有 `per_page` 逻辑负责，本设计只改方向。
- 不提供「按宿主程序区分布局」。那是另一个维度（compat），不与模式维度混在一个字段里。
- 不改 UI 侧 `set_vertical` 的实现，只改「什么时候用什么参数调它」。

---

## 七、迁移

沿用 `quick_input.enable_english` 已有的加载期迁移先例（`config.rs:592-594` 记录了那次）：

1. 加载期（**反序列化前**，字段已从结构体删除、之后就读不到了）按 `id == QUICK_MIX_ID` 定位实例，写入其 `candidate_layout`：
   - `force_vertical == true` → `"vertical"`
   - `force_vertical == false` → **`"follow"`**，不是 `"horizontal"`
   - **键缺失 → 不动**，让 `default_mix_modes()` 的出厂值（`Vertical`）生效
2. 迁移后从 `QuickInputConfig` 删除 `force_vertical` 字段。
3. manifest 删除该行；新增 4 个单例模式的 `candidate_layout`（`Enum(LAYOUT_INTENT_VALUES)`）。

★ 第 1 条的三分支各有理由，改任何一支都会伤到一类用户：

- **false → follow 而非 horizontal**：旧布尔的 false 语义是「不强制」（跟随全局）。迁成
  horizontal 会把从没开过这个开关、又把全局设成竖排的用户强行钉在横排上。
- **缺失 → 不动**：老版预置文件写的是 `force_vertical = true`，与新出厂值同义；新版预置
  已删该行，全新安装读不到旧键。两种情况都应落到出厂竖排，故「不动」是对的。若在这里
  写 follow，全新用户的快捷输入会变成跟随全局——出厂行为被悄悄改掉。

两条分支各有一个测试守着（`force_vertical_migrates_into_quick_mix_candidate_layout` /
`absent_force_vertical_keeps_factory_vertical`）。

⚠️ `mix_modes` / `special_modes` 已是 `StructList` manifest 项，**per-instance 字段不另立 manifest 项**（`quick_input_member_sources` 记过：一个配置键只能有一个 manifest 项）。

---

## 八、落点清单

### 本仓（core）

| 文件 | 改动 |
|---|---|
| `wind-config/src/config.rs` | 新增 `LayoutIntent`；6 处挂载 + 新建 `AddWordConfig`；删 `QuickInputConfig::force_vertical` |
| `wind-config/src/config.rs`（加载期） | `migrate_force_vertical_value`（**反序列化前**，同 `migrate_enable_english_value`） |
| `wind-config/src/lib.rs` | re-export `LayoutIntent` |
| `wind-config/src/config_schema.rs` | 删 `force_vertical`；加 4 个单例键 + `LAYOUT_INTENT_VALUES` 常量 |
| `data/config.toml` | 新键补齐（含 `[input.add_word]` 新段），删 `force_vertical`；**不写 `mix_modes`** |
| `wind-coordinator/src/layout.rs`（新建） | 纯函数 `intent_for` / `vertical_for` + `impl` 包装 + 单测 |
| `wind-coordinator/src/coordinator.rs` | `notify_ui_update` 接入；`apply_ui_config` / `cmd_toggle_layout` 改走重算（§5.4）；新增 `candidate_layout_sent`，`candidate_vertical` 提为 `pub(crate)` |
| `wind-coordinator/src/handle_addword.rs` | `show_add_word_preview` 接入（第二个收口点，§5.3）；删 saved/restore |
| `handle_mode.rs` / `handle_lifecycle.rs` | 删除全部 saved/restore 代码与两个 state 字段 |
| `tests/mode_layout.rs`（新建） | 接线端到端 + 自愈 |

净效果：删掉 2 个 state 字段与 6 处保存/恢复代码，新增 5 个模式的支持。

### wind-setting（独立仓 `D:\Develop\workspace\windinput\wind-setting`）

状态：✅ 已完成（设置仓 `d49dd29`，321 项测试全绿）。

**五道守门测试全部通过**：

1. 本仓 `config_schema::tests::registry_covers_every_config_key`
2. 本仓 `data_config_toml_covers_registry`
3. 设置仓 `snapshot_matches_core_generated_capabilities` → `cargo test regenerate_capabilities_snapshot -- --ignored`
4. 设置仓 `rpc::tests::mock_config_matches_core_system_preset` → `cargo test regenerate_mock_config -- --ignored`
5. 设置仓 `uncovered_capability_keys_match_allowlist` → 不进 GUI 的键须具名登记 `UNCOVERED_BY_DESIGN`

⚠️ 3/4 两条的快照与 mockdata 从主仓 `wind-config` + `data` **现算**，不要手改。
⚠️ 设置仓靠 `path = "../WindInput/..."` 依赖**主工作区**，所以在 feature worktree 里
它是绿的——**合并进 main 后才会红**。同类改动要把设置仓的收尾算进「合并后」而非「分支内」。

### 实际落点

| 模式 | 清单项 | 控件 |
|---|---|---|
| 快捷输入 | `schema.mix_modes#candidate_layout`（视图 key） | `select_mix_layout`（新增类型） |
| 临时拼音 | `input.temp_pinyin.candidate_layout` | `select` |
| 临时英文 | `input.temp_english.candidate_layout` | `select` |
| 网址输入 | `input.url.candidate_layout` | `select`，但 **`hidden = true` 暂时隐藏** |
| 快捷加词 | —— | 不暴露，登记 `UNCOVERED_BY_DESIGN` |

两处不进 GUI，用的机制不同，别混：

- **网址** —— 该模式目前**不产出任何候选**，候选窗根本不出现，这一项没有可观察的效果。
  用 `hidden = true` 而非删清单项：key 仍在清单里，capability 覆盖照旧满足（**不需要**
  登记 `UNCOVERED_BY_DESIGN`），将来网址若开始出候选，删掉 `hidden` 一行即可恢复。
- **加词** —— 没有独立设置分区，且逐字确认的两行提示本就该竖排，出厂 `vertical` 即最优解，
  横排没有使用场景。属于「设计上就不打算暴露」，故走 `UNCOVERED_BY_DESIGN`。

判据：**「暂时没意义」用 `hidden`，「设计上不暴露」用 `UNCOVERED_BY_DESIGN`**。前者预期会
恢复、留着清单项省事；后者是长期决策、名单里那句理由就是它存在的凭据。

快捷输入那项**不能复用通用 `build_select`**：后者直接 `cfg.get_str(key)` / `set_path(key, String)`，
用在 structlist 上会把整个 `mix_modes` 数组换成一个字符串。故新增 `mix_layout.rs`
（纯读写逻辑，照 `mix_trigger.rs` / `mix_members.rs` 的形态）+ `select_mix_layout` 控件类型，
`control_type_compatible` 里按 `structlist` 而非 `enum` 校验。


### 仍存在的交付边界

- **`schema.special_modes` 整个不在设置页**（登记在 `UNCOVERED_BY_DESIGN`）。所以快符 /
  生僻字等特殊模式的 `candidate_layout` **只能改配置文件**。要暴露它得先做特殊模式列表的
  条目编辑器，那是独立一块工作。
- **只有内置 `quick_mix` 一个 mix 实例能在 GUI 里改布局**。`schema.mix_modes` 在设置页是
  整体读写的 structlist，三个清单项（触发键、候选来源、布局）都靠硬编码 `quick_mix` id
  打补丁。用户自建的 mix 实例同样只能改配置文件。
- **`quick_mix` 的默认值进不了 `data/config.toml`**。预置文件写 `mix_modes` 会把整份定义
  冻结成快照（见该文件头部说明），所以出厂 `vertical` 只能落在代码侧 `default_mix_modes()`，
  config.toml 里只留注释指路。其余四个单例键的默认值都已写进 config.toml。

---

## 九、测试要点

`fuzzy_pinyin_layer_vs_penalty` 与 `ci_host_target_split_path_tests` 都记过「测试会静默退化成假绿」，本设计有两个具体的假绿陷阱：

**1. 断言落点。** 必须断言 `desired_vertical(state)` 的返回值，**不要**断言「有没有发出 `SetCandidateLayout`」。去重缓存会让「上次已是这个值」的情况不发命令，测试拿不到信号却看起来通过了。

**2. 必测 `follow` + 全局竖排这一格。** 这是**唯一**能区分新旧语义的输入组合（`follow` 与 `vertical` 在全局横排时表现相同）。漏了它，整个三态改造等于没测。

### 已实施的用例

`wind-coordinator/src/layout.rs`（决策矩阵，纯函数，不构造协调器）：

| 用例 | 守什么 |
|---|---|
| `every_mode_maps_intent_over_baseline` | 5 种模式 × 3 种意图 × 2 种基线全矩阵，含上述第 2 条那一格 |
| `no_active_mode_follows_baseline` | 无模式时与任何模式配置无关 |
| `add_word_outranks_active_mode` | 优先级链 |
| `out_of_range_instance_falls_back_to_follow` | 实例下标越界回落 Follow，不 panic |
| `each_mode_reads_its_own_key` | 映射表没把字段抄串（改临英不影响其余四个） |
| `builtin_quick_mix_defaults_to_vertical` | 出厂值只能落在 `default_mix_modes()`，被改回 Follow 会让全局横排的用户突然变横排 |
| `add_word_defaults_to_vertical` | 加词从硬编码迁成配置项后行为不变 |

`wind-coordinator/tests/mode_layout.rs`（接线端到端）：

| 用例 | 守什么 |
|---|---|
| `temp_english_overrides_vertical_baseline_then_restores` | 正常进入/退出，且用的是「基线竖排 + 模式横排」这一格 |
| `layout_self_heals_when_mode_cleared_without_its_exit_path` | 自愈：失焦复位后无需显式恢复 |
| `follow_intent_leaves_baseline_untouched` | Follow ≠ Horizontal |

`wind-config/src/config.rs`：`force_vertical_migrates_into_quick_mix_candidate_layout`、
`absent_force_vertical_keeps_factory_vertical`（迁移三分支，见 §7）。

⚠️ 端到端刻意选**临时英文**：Shift+字母进入，不依赖任何方案/词典，因此不需要 `build_dev/data`，
不会像 `has_schemas()` 守卫的测试族那样在缺数据时静默跳过。三个用例都先断言「真的进了模式」
再断言布局——少了这一步，「压根没进模式」与「进了但覆盖没生效」在后续断言上无法区分。

### 未由测试覆盖的部分（如实记录）

- **`apply_ui_config` / `cmd_toggle_layout` 确实走了 `sync_candidate_layout` 而非直接下发**：
  这两条路径难从集成测试触达（前者需热重载、后者是私有的命令栏动作），目前靠代码审查保证。
  逻辑本身（基线变化 → 结果跟随）已由 `vertical_for` 的矩阵覆盖，缺的只是「接线」这一层。
- **真机手测**：候选窗方向切换的视觉表现、是否有闪烁，需在真实宿主里确认。

⚠️ `runtime_mirror_state_config_sync` 记过：`cargo test -p wind-coordinator` 会真写
`%APPDATA%` 的 `schema.active`。本设计新增的测试不触及配置写回，但同族测试仍有此副作用。

---

## 十、遗留与风险

| 项 | 说明 |
|---|---|
| **真机手测未做** | 候选窗方向切换的视觉表现、有无闪烁，需在真实宿主里确认（构建走 `scripts/dev.ps1` 的 `dm1`/`dm2` 再 `pdm1`/`pdm2`） |
| 特殊模式 / 自建 mix 的设置页缺口 | 见 §8，功能可用但只能改配置文件；补条目编辑器是独立一块工作 |
| `Follow` 的语义边界 | 若将来出现「模式 A 覆盖 → 模式 A 中进入模式 B」的嵌套，当前优先级链是「取最内层」，不做栈式嵌套。目前没有这种嵌套场景，如果出现需重新审视 |
| 加词默认值 | 定为 `vertical` 是为了保持现状；若认为它本就该跟随全局，改默认值即可，但属于行为变更需单独说明 |
