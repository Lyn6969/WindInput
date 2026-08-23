# 回车键行为（`input.enter_behavior`）语义与实现收口

## 配置

| 值 | 语义 |
|---|---|
| `commit`（默认） | 回车上屏「已转换前缀 + 剩余原码」，然后退出组合 |
| `clear` | 回车放弃整段组合，**不上屏任何内容**，退出组合 |

## 同族的另外两键，以及「第二根轴」

`input.space_on_empty_behavior`（空格）与 `input.punct_on_empty_behavior`（标点/符号键）
与本键同族，都描述「空码时这个终结键怎么处置废码」。但**值域并不相同**，原因是这一族的
行为其实有**两根轴**，而配置值域只有一维——它是两轴合法组合的枚举：

| | 键字符照常输出 | 键字符被吞掉 |
|---|---|---|
| **废码上屏** | `commit` | （无意义，三键都不设值） |
| **废码丢弃** | `clear`（仅标点） | `clear`（回车/空格）、`clear_no_input`（仅标点） |

读法：**同一个 `clear` 字面值，在三个键上第二根轴取值相反**。

- 回车/空格的 `clear` 返回 `ClearComposition`——回车不换行、空格不插空格，键本身不产出
  任何字符。
- 标点的 `clear` 仍照常上屏那个标点。这**不是漏改**：标点是用户真正想输入的可见字符，
  吞掉它等于吞掉用户的意图；而回车/空格在这个语境下本就不产生可见内容。

`clear_no_input`（0.119）补的正是标点缺的那一格：「这串码作废，这个键也当没按过」——
把标点当成「取消废码」的键。**它只属于标点**：回车/空格的 `clear` 本就在吞键那一格，
给它们加同名值只会得到两个行为完全相同、用户无从分辨的选项。

值域清单在 `config_schema.rs` 的 `EMPTY_CODE_BEHAVIOR_VALUES`（回车/空格两态）与
`PUNCT_EMPTY_CODE_BEHAVIOR_VALUES`（标点三态）；唯一解释器是
`Coordinator::punct_empty_code_policy`，返回枚举而非 bool——bool 判据加第三态时，漏接的
出口会静默落进 `else`，而 `match` 的漏接是编译错误。

### ★ 可复用判据

**用户提的新需求「装不进现有值域」时，先画行为矩阵，再决定加值还是拆维度。** 判别很
简单：新需求与某个现有值**只在一个方面不同**、其余完全相同 ⇒ 那个方面就是一根没被命名
的轴。本例中「丢废码且吞标点」与 `clear` 只差「标点出不出」，正是这个信号。

⚠️ 认出第二根轴后，**不要**把它拆成独立的布尔开关（如 `punct_on_empty_output_punct`）：
正交布尔会与 `commit` 组合出「废码上屏、标点却丢了」这一格，没人要，而布尔无从表达
「这一格非法」。两轴不自由组合时，只能用 Enum 枚举合法格。

## 标点键有**三条**独立通路

回车的教训（下一节）在标点上重演了一遍，且多一条：

| # | 通路 | 位置 | 漏接的症状 |
|---|---|---|---|
| 1 | 普通标点出口 | `message_handler.rs` 标点臂末段 | —— |
| 2 | 智能符号 `CommitAndHoldComposition` | 同臂 `hold_info` 分支，**独立 return** | 「只在开了智能符号（`hold_composition`）的宿主上开关不生效」 |
| 3 | 以词定字 `select_char_keys` | `message_handler.rs` 分发段，在标点臂**之前** return | 「开了以词定字后，只有没配成以词定字的那几个标点还生效」 |

通路 3 尤其隐蔽：出厂 `select_char_keys` 为空 ⇒ 默认不暴露，正因如此更不容易被发现。
空码时 `handle_select_char` 拿不到字源，退到 `keys.overflow.select_char_key`（出厂
`ignore` ＝吞键并**保留**编码），`punct_on_empty_behavior` 根本够不着。修法是**放行**回
标点臂，而不是在 overflow 那边复制一份判据——放行后这几个键与其余标点走同一段代码，
日后标点臂再改也不会漏掉它们。

### ★★★ 放行判据只能问「以词定字自己成不成立」，不能问标点策略

首版修法把条件写成 `policy == Commit`（只放行 clear 系两档），**错在拿下一环的配置当本环
的判据**，把两个本该正交的东西耦合起来。后果是 `commit` 档漏网：开了以词定字得到
`Consumed`（吞键、废码留着），没开得到 `bbqq。`——同一个键、同一个状态，行为取决于一个
看上去无关的功能开没开。

正确判据是 `!state.candidates.is_empty()`，即**以词定字自身的适用条件**：它要从「当前高亮
候选词」取第 N 个字，没有候选就没有字源，此刻这个键压根不该算以词定字键。不符合条件就整
个交给下一环，三档各自表现由 `punct_on_empty_behavior` 全权决定。

于是 `keys.overflow.select_char_key` 专管**真正的越界**：候选词字数不足、联想态无
`input_buffer`、高亮下标越界。**空码不是「以词定字越界」**，而是「这个键此刻不是以词定字
键」——两者的区别正是这条 bug 的全部内容。

⚠️ 回归测试必须**三档一起断言**，且每档的期望值取「没开以词定字时该键本来的行为」。只测
`clear` 一档的话，`commit` 档的漏网完全照不到——首版就是这么漏过去的。

`clear_no_input` 的短路点因此刻意放在 `hold_info` **之前**、通路 1 和 2 之上，一处覆盖两
条；也必须早于标点流水线（`convert_punct` / `record_commit` / 配对栈），否则会记一次从未
上屏的标点、并把它压进配对栈。

⚠️ 标点不上屏 ⇒ **没有可 hold 的对象**。若仍走 `CommitAndHoldComposition`，会挂一个屏幕
上并不存在的 hold 态，下一次同键 press2 会去删一个从未上屏的符号。

## 回车有五条彼此独立的处理路径

回车不是在一个地方处理的。各输入模式在分发阶段就被劫走，各自有一份 `VK_RETURN` 实现：

| 路径 | 文件 | 劫持点 |
|---|---|---|
| 主输入 | `coordinator.rs` `VK_RETURN` 分支 | 默认路径 |
| 临时拼音 | `handle_temp.rs::handle_temp_pinyin_key` | `coordinator.rs` `ModeKind::TempPinyin => return …` |
| 临时英文 | `handle_temp.rs::handle_temp_english_key` | 同上，`ModeKind::TempEnglish` |
| 混合 / 快捷输入 | `handle_mode.rs` | `ModeKind::Mix` |
| 特殊模式（快符 / 生僻字） | `handle_special.rs` | `ModeKind::Special` |

分发是 `return`，不是 fallthrough —— **主输入路径里的任何回车逻辑都不会惠及其余四条**。

## 曾经的缺陷（已修）

四个模式 handler 都把 `enter_behavior` 判断写在了 `if buffer.is_empty()` 的**内部**：

```rust
if state.xxx_buffer.is_empty() && state.committed_text.is_empty() {
    if self.rt().config.input.enter_behavior != "clear" && !prefix.is_empty() { … }
    return KeyAction::ClearComposition;
}
// 非空缓冲：上屏「已转换前缀 + 缓冲原文」（原行为不变）  ← 完全不看配置
```

成因：这段配置判断是为**另一个需求**（「空缓冲回车上屏被模式键占用的符号本身」）才引入的，只加在新增的空缓冲分支上，注释里明写「非空缓冲……原行为不变」。配置判断是**顺带**进来的，从未覆盖它本该覆盖的主路径。

**用户可见指纹**：设了「清空编码」后，什么都不打直接回车是生效的；**打了码再回车就失效**，照旧上屏原码。「时灵时不灵」正是判断位置错了一层的表现。

修复：判据收口为 `Coordinator::enter_clears_composition()`，五条路径共用，且在各 `VK_RETURN` 分支的**最外层**前置判断。收口的价值在于——漏接会从「调用了但判在错误分支」（grep 搜得到字符串、看不出位置错）退化为「没有调用点」（容易发现）。

## 上屏原码时，引导符归不归还

`commit` 上屏的「剩余原码」里**含不含引导符**，取决于引导符是字母还是符号。判据收在
`Coordinator::guide_to_return`，三个同源出口共用：

| 出口 | 位置 |
|---|---|
| 临拼回车 | `handle_temp.rs` `VK_RETURN` 非空缓冲分支 |
| mix 回车 | `handle_mode.rs` 同上 |
| 切中英文 | `coordinator.rs::take_input_on_mode_switch` |

规则：

- **符号引导符不还**（`` ` ``、`;`）。它们在码表里不产出编码，用户按下只可能是为了开模式。
  `` `nihao `` → `nihao`。
- **字母引导符要还**（`z`，经 `schema.codetable.z_key_action` 进入）。字母在码表里是**合法
  编码字符**，按下时它既可能是开关也可能是码。放弃整段的语义正是「别猜了，把我打的原样给我」，
  此时吞掉那个字母就是猜错了还不还。`zhang` → `zhang` 而非 `hang`。
- **`committed_text` 非空则一律不还**。用户已在模式内选过词，说明认可了这次进入，引导符归
  模式所有；再吐出来只会得到「z你好ma」。

z-fallback 路径尤其要还：那里的 `z` 是从 `input_buffer` 里**抢走**的真实击键（`zzha` 夺取后
`prefix="z"` + `buffer="zha"`，归还后恰好复原成 `zzha`）。

> ⚠️ 三个出口必须同进同出。只改回车会造出「回车带 z、Shift 切英文不带」的不一致，而
> `take_input_on_mode_switch` 的注释还写着「与各自回车上屏一致」——注释会当场变成谎言。

### 相关：夺取回退的落点

`Rewind.snapshot` 必须是**夺取前**的 `input_buffer`，不含触发夺取的那一键。曾在
`try_z_fallback` 里取 `buffer + ch`，那必然退到一个无候选的死状态——夺取的前提恰恰是
`has_code_prefix(buffer + ch) == false`。用户可见指纹：`zzh`（有候选）→ 打 `a` 进拼音 →
退格只让候选窗消失、编码还在，得再按一次才回到 `zzh`。

判断这类问题的通用抓手：**回退目标必须是用户实际见过的某一帧**。`zzha` 那一帧从未渲染过
（按下 `a` 的同一帧就被夺取了），退过去无论内部账目多自洽，读起来都像卡了一下。

## 已定决策

**`clear` 一并丢弃 `committed_text`**（即临拼/混合模式下已通过选词逐步上屏的那部分转换结果），与主输入路径 `coordinator.rs` 的既有行为一致。四条路径的退出都走各自的 `exit_*` 函数，它们本就会清 `committed_text` / `committed_segs`。

理由：保持五条路径行为统一，用户心智负担最小 —— 「清空编码」就是清空全部，不需要记忆「哪部分会保留」。

## 例外：临时英文只受 `clear` 管辖「空缓冲」

**临英是五条路径中唯一的例外**：`enter_behavior = "clear"` 在该模式下只对**空缓冲**生效（即只按了触发键、还没打字），非空缓冲一律照常上屏，不读该配置。

两条理由：

1. **临英缓冲装的是英文原文，不是「编码」。** 其余四条路径回车放弃的是待转换的编码（用户还能重打），临英放弃的是用户已经完整打好的文本。
2. **`space_as_input` 叠加后会形成上屏死锁。** 该配置把空格让给输入字符，上屏职责整个转交回车（见 `handle_temp.rs` 的 `VK_RETURN` 分支）；`clear` 若再把回车的上屏职责拿走，临英就**一个上屏通路都不剩** —— 打进去的英文只能靠 Esc 整段丢弃。`allow_symbols` 再开时数字键也让位于输入，连选词键都没有。

单看任一配置都合理，**叠加才致命**。这是「两个正交开关各自正确、组合后功能归零」的典型：加新的「让位类」开关（把某键的既有职责让给输入）时，须回头确认接手职责的那个键没有被别的配置同时收走。

空缓冲仍按 `clear` 放弃（不回显触发键字符）：此时本就没有内容可上屏，「至少能上屏打进去的内容」这条底线不受影响。

> 该例外**推翻了本文档早先的决策**（原为「临英纳入 enter_behavior 管辖，语义统一优先于『编码』的字面义」）。反转依据即上述第 2 点 —— 语义统一的代价是模式不可用时，统一让位。

## 待办（未实施）

用户明确表示**不排除后续需要「上屏已转换部分、只丢弃剩余原码」的需求** —— 即回车时把已选好的汉字上屏，只放弃还没转换的编码。

若要实现，注意：

- 这是**第三种模式**，不应改变现有 `clear` 的语义（会破坏已建立的用户预期）。建议取值扩展为 `commit` / `clear` / `commit_converted` 之类，而非在 `clear` 内部加分支。
- 五条路径都要接，且**必须为每条路径写「非空缓冲」的回归测试** —— 本次缺陷正是「只覆盖了空缓冲」造成的。
- 临时英文没有 `committed_text` 概念（无逐步转换），该模式下新值应退化为等同 `clear`。

## 测试

`crates/wind-coordinator/tests/input_flow.rs`，四条路径各一个 `*_nonempty_enter_clear_discards`，外加两个 `*_nonempty_enter_commit_still_outputs_code` 对照组。

两个约束，改动本节测试时务必保留：

1. **必须先断言「确实进入了该模式」**。触发键若未生效，按键会落到主输入路径，而主输入路径的 `clear` 同样返回 `ClearComposition` —— 不验证进入就是假绿。
2. **对照组不可删**。没有 `commit` 模式的对照，无法区分「配置生效」与「该模式回车本来就不上屏」。

> 注：该测试族依赖仓库根 `build_dev/data`（`has_schemas()` 为假时全部静默 `return`，测试显示 ok）。全量 `cargo test -p wind-coordinator` 真跑约 1.6s，**0.0x s 即假绿**。另该测试族会真写 `%APPDATA%\WindInput\config.toml` 的 `schema.active`，跑完须核对。
