# 方案级按键功能表（`key_actions`）

> 目标：让「某个键在本方案里干什么」成为方案的属性，且动词表与既有热键系统共用一套。
> 直接诉求是「不同输入方案进不同的快符方案」，但设计要能装下「右 Shift 进英文方案、
> 再按回来」这类非模式类功能。
>
> 这是 [key-pipeline.md](../redesign/key-pipeline.md) 里预留但从未实施的 **S5「按钮自定义」**
> （见该文档 §3 挂载点表与 §4 阶段表），不是新开的机制。

## 1. 现状：三套动词表，值域重叠但互不相识

| 表 | 值域 | 触发形态 | 层级 | 落点 |
|---|---|---|---|---|
| 组合键热键 | `add_word` / `open_add_word_dialog` / `enter_temp_pinyin` / `enter_special:<id>` / `switch_schema:<id>` + `dispatch_hotkey` 的 8 个 | 组合键 | 全局 | `coordinator.rs` ~5390 起 |
| `ZKeyAction` | `none` / `temp_pinyin` / `temp_english` / `mix:<id>` / `special:<id>` | 单键（仅 z） | **方案级** | `handle_lifecycle.rs:269` |
| `trigger_keys` × 5 处 | 隐含「进入本实例」 | 单键 | 全局 | 见下表 |

五处 `trigger_keys`：`input.temp_pinyin` / `input.temp_english` / `schema.mix_modes[]` /
`schema.special_modes[]`，加上 `keys.toggle_mode_keys`。前四者的优先级是**代码顺序**
（`try_activate_mode`，`handle_lifecycle.rs:95`），不是数据；`match_special_trigger` 用
`.find()` 先到先得，两个实例配同一个键时后者**静默失效**。

**关键事实**：第一张表的值域已经覆盖了本设计需要的绝大部分动词，且 `enter_special:<id>`
与 `switch_schema:<id>` 这类带实例 id 的形态也已存在。本设计不发明动词，只做两件事：
把这套值域**扩展到单键**，并给它**加方案层**。

## 2. 统一动词表：四类，按生命周期分

分类不是为了整齐，而是因为**方案级覆盖对不同类的意义不同、插入点也不同**（见 §4）。

| 类 | 动词 | 生命周期 | 方案级绑定的价值 |
|---|---|---|---|
| **A 状态切换** | `toggle_mode` `toggle_punct` `toggle_full_width` `toggle_s2t` `toggle_toolbar` `open_settings` `take_screenshot` `add_word` `open_add_word_dialog` | 瞬时 | **低——仅作功能补全**（已拍板）。切标点在哪个方案里都一样；真实用途只有「在本方案禁用某键的全局绑定」，而那是 D 类。排在最后一期，见 §7 |
| **B 模式进入** | `temp_pinyin` `temp_english` `mix:<id>` `special:<id>` | overlay，打完退回 | **最高**——快符按方案分流即此类 |
| **C 方案切换** | `toggle_schema:<id>` | 持久，带回程 | 高，但**有锁死风险**，见 §5 |
| **D 禁用** | `none` | — | 屏蔽全局绑定的第三态 |

> `switch_schema:<id>`（单向，无回程）**不进方案级表**，只保留在全局热键（已拍板）。
> 全局层已经提供了单向切换的完整能力，方案级表的定位是「在此基础上提供更多选择」，
> 不必重复提供一个自带锁死风险的同义动词。理由见 §5。

## 3. 配置形态

放在方案文件**顶层**（与 `[engine]` 平级，已拍板），不放 `[engine.codetable]` 下——按键
功能与引擎类型无关，拼音方案同样需要它。

```toml
# data/schemas/wubi86.schema.toml  或  schema_overrides/wubi86.toml
[key_actions]
backslash = "special:fuhao_wubi"     # B：五笔用五笔快符表
z         = "temp_pinyin"            # B：z_key_action 收编
rshift    = "toggle_schema:english"  # C：进英文方案，再按回来
grave     = "none"                   # D：本方案禁用全局的 ` 引导
```

**覆盖语义：逐键合并**（不是整段替换）。方案只声明要改的键，其余回落全局。
与 `CodeTableSpec` 的 tri-state 同源——`Option<BTreeMap<String,String>>`，`None` 与
空表都表示「整段跟随」。

★ **顺序无语义，键唯一**。这是选 Map 而非 `Vec<(key, action)>` 的理由：优先级由 §4.1 的
插入点决定，不由配置顺序决定。此处不能重蹈 `checkbox_group` 那个坑——那类 StrList 的顺序
带语义（优先级列表），而 GUI 恒按声明顺序写回，导致用户手改配置文件排出的顺序会在保存
任何一项时被静默改回去。Map 从类型上就没有这个问题：没有顺序可丢。

查表结果三态：

| 结果 | 含义 | 后续 |
|---|---|---|
| `Some(action)` | 方案显式绑定 | 执行，**跳过全局链** |
| `Some(none)` | 方案显式禁用 | 不执行，**也不落全局链**，键归普通输入 |
| `None`（未声明） | 未表态 | 落全局链（现状行为，逐字节不变） |

★ **`none` 哨兵是必需的，不是设计洁癖**。方案作者内联段与用户 override 的合并发生在
toml 层（`merge_toml`），而它**只能新增/覆盖，无法表达「删除 base 的某个键」**——base 里
有而 override 里没有的键恒保留。所以「本方案禁用某个绑定」只能靠显式值，不能靠"把这行
从 override 里删掉"。已实证，见 §10。

## 4. 与现有按键处理的兼容

### 4.1 插入点必须按类分，不能只有一个

现有顶层链（`coordinator.rs::handle_key_event`，keydown 方向，节选关键位次）：

```
 2  key_up 分支：CapsLock → handle_select_key_up → is_toggle_mode_keycode
 3  组合键热键匹配 → add_word / enter_special: / switch_schema: / dispatch_hotkey
 6  密码框抑制 → PassThrough
 8  英文模式 → PassThrough          ← 分水岭
11  已激活独占模式分派
12  try_activate_mode（引导键 / z_key_action）
15  普通输入
```

位置 8 是分水岭：**在它之后的一切，英文模式下都跑不到**。现有模式引导键在 12，
所以英文模式下按 `\` 正常出反斜杠——这是对的，不能破坏。

于是插入点由**键有没有字符**决定，而不是由动作类别决定：

| 键的形态 | 判据 | 插入点 | 理由 |
|---|---|---|---|
| 有字符（`\` `z` `;`） | `punct_char(vk).is_some()` | **位置 12 内**（`try_activate_mode` 最前） | 英文模式下必须让它出字，否则该字符永远打不出来 |
| 无字符（`rshift` `F8`） | `punct_char(vk).is_none()` | **位置 2 / 3** | 没有字符可出，英文模式下拦截无副作用；C 类还必须在英文态生效才回得来 |

这条判据也解释了既有代码里一个看似不一致的地方：`switch_schema:<id>` 在位置 3
**刻意不判 `chinese_mode`**（`coordinator.rs:5439` 的注释：「切方案在英文态下同样该生效，
否则切到英文方案后这条路径就失效了，用户回不到中文方案」）——同一个道理，同一个坑。

### 4.2 字母键的活码前缀裁决要从 z 泛化到所有字母

z 现在的三重裁决（`handle_lifecycle.rs:207`）里，只有第 ① 条是 z 专有的：

1. `z_key_repeat` 开且有上屏历史 → 让位　　**（z 专有，repeat 功能本就绑在 z 上）**
2. 该键是**活码前缀**（`has_code_prefix`）→ 让位　　**（所有字母都该有）**
3. 否则执行 action

第 ② 条必须随 `key_actions` 泛化到任何绑了 action 的字母键。否则用户在某方案把 `u`
绑成功能键，`u` 开头的编码就在该方案里彻底打不出来了，且毫无提示。

### 4.3 与 `leading_chars` 的冲突：方案级绑定优先

`code_char_takes_lead`（`handle_lifecycle.rs:78`）现在的规则是「符号已被方案声明为首码
⇒ 引导键让位给码表」。方案级 `key_actions` 绑了同一个符号时，**绑定优先**。

理由：该函数存在的目的是**跨层仲裁**——全局配的引导键不该抢方案自己的码元。而
`key_actions` 与 `leading_chars` 是同一层（都在方案里）的两条声明，显式绑定优先于
从字符集隐式推导。同层冲突不适用跨层规则。

设置页应对这种同层冲突给 `Coexist` 级提示（该分级机制见 `key_conflict.rs`），不是
`Blocking`——内核有确定裁决，两条配置都不算坏。

### 4.4 修饰键只能走 keyup 轻敲，且会再次触碰 `is_toggle_mode_keycode`

`rshift` 这类纯修饰键绑任何 action，通路只能是 **keyup 轻敲**，三条理由各自独立成立：
keydown 不能吃（吃掉会让 AutoCAD 看不到修饰键）、keydown 上判定会让 `Ctrl+A` 的第一下
Ctrl 误触发、宿主对按住的键重复发 keydown（实测 28 秒 145 次）会连续触发。

做法复用既有机制、C++ 零改动：注册进 `CompiledHotkeys.key_up`，由 `IsKeyUpHotkey` 接管。

> ⚠️ **必须重查 `is_toggle_mode_keycode` 的判定**。该函数原本按「key_up 表里有没有这个
> key_code」判定，`select_key_groups` 进表时已经踩过一次（只配了选词用的 Ctrl 会被当成
> 切换键，空闲敲 Ctrl 莫名切中英文），修法是**按 `action` 过滤**而非按键码。`key_actions`
> 的修饰键条目进表是第二次触碰这条，同样要按 action 过滤。

keyup 分支内的优先级，沿用既有裁决「有候选选词、无候选切换」：

```
handle_select_key_up（二三候选键，有候选时赢）
  → key_actions 查表
  → is_toggle_mode_keycode
```

### 4.5 与全局 `trigger_keys` 的关系（一期不动全局）

一期只加方案层，五处 `trigger_keys` 原样保留。查表未命中即落原有链，**未配置
`key_actions` 的用户行为逐字节不变**——这是一期可以低风险交付的前提。

全局层的收编（把五处折算进 `keys.key_actions`、删掉硬编码优先级链、冲突检测上线）
留到三期，届时才动既有行为，需单独排期与真机验证。

## 5. `toggle_schema` 的回程语义与防锁死

### 为什么方案级不允许 `switch_schema`

方案级绑定 + 单向方案切换 = 可锁死用户：

> 五笔方案里 `rshift` → 切到英文方案 → **英文方案的 `key_actions` 里没有 `rshift`**
> → 回不来，只能去点工具栏。

两条配置各自都对，组合起来功能归零——与 `enter_behavior` 那次「`clear` 与
`space_as_input` 各自正确、叠加后临英一个上屏通路都不剩」同形。

`toggle_schema` 从结构上免疫：回程由**运行时来源**决定，不依赖目标方案的配置。

### 回程规则（已拍板：回到来源）

来源记在运行时状态，**不落配置**：

```
五笔 --按--> 英文 --按--> 五笔
拼音 --按--> 英文 --按--> 拼音
```

边界行为：

| 情形 | 行为 |
|---|---|
| 已在目标方案、无来源记录（如刚重启） | **无操作**（不切走），避免把用户送到没待过的方案 |
| 在目标方案期间用别的方式切了方案 | 来源失效——见下方「怎么失效」 |
| `toggle_schema:<自己>` | 无操作 |
| 目标方案不存在 / 加载失败 | 安全失败，不记来源（否则下次按会把用户送去一个他从未离开过的地方） |

### 来源怎么失效：代际校验，不是散点清空

★ 实施时发现的坑：切 active 方案在协调器侧有**五条路径**——循环键、直达热键、命令栏
`switch_schema`、菜单 `select_schema`、设置页 `schema.setActive` RPC，而
`finish_user_schema_switch` 这个名为「统一收尾」的函数**只覆盖前两条**（它的文档注释
本身就承认「三个入口行为各自漂移过」，它收敛的是其中两个）。

把「清来源」挂在那里只能清一半，剩下三条的漏接表现是「往返键把用户送回几步之前的
方案」——低频、难复现、且看起来像随机跳转。

改为**代际校验**：`EngineManager` 持一个 `schema_generation` 计数，来源记录连同写入时的
代际一起存，读取时比对是否仍相等。期间任何路径切过方案，代际就对不上，来源自动失效。
零散点接线。

两个实现细节：

- 代际递增与 `active_hook::notify_active_changed` **绑死在 `on_active_changed` 里**，不是
  各赋值点自己加。漏掉通知会让设置界面的方案显示不刷新（看得见、会被报），漏掉计数只在
  往返键这个低频路径上出错（看不见）——**把易漏的接线搭在不易漏的接线上**。
- 只比对方案 id 不够：「切走又切回来」与「从未变过」在 id 上完全同形，非有代际不可。

## 6. 存量迁移

| 旧 | 新 | 时机 |
|---|---|---|
| `schema.codetable.z_key_action` | **不迁移，改为回落**（见下） | 二期 |
| 五处 `trigger_keys` | `keys.key_actions` | 五期 |

### `z_key_action` 为什么不迁移而是回落

原计划把它 `migrate_*` 成 `key_actions["z"]`，实施时发现**迁不过去**：`z_key_action` 是
**全局配置**（`config.toml` 的 `schema.codetable`，方案经 `CodeTableSpec` 覆盖），而
`[key_actions]` 在**方案文件**里。全局值不知道该写给哪个方案——用户装了五个码表方案，
迁移代码没有依据挑其中一个。

改为查表时**两个来源、方案表优先**（`Coordinator::bound_action_for`）：

1. 方案的 `[key_actions]` 里显式写了 `z` → 以它为准（含显式 `"none"` 禁用）
2. 没写 → 回落 `schema.codetable.z_key_action`（其自身已含全局→方案的折叠）

存量配置因此零改动继续生效，而新写法可以逐方案覆盖它。代价是 z 在两处可配，靠文档与
设置页说明消化——比起为迁移瞎猜一个目标方案，这个代价更小。

`ZKeyAction` 类型改名 `KeyAction`（值域扩充，解析逻辑不变）。注意与 `wind-coordinator`
既有的 `KeyAction`（按键返回值）**重名**——建议叫 `KeyBinding` 或 `BoundAction`，命名在
实施时定。

## 7. 分期

| 期 | 内容 | 风险 |
|---|---|---|
| ~~一~~ ✅ | `toggle_schema:<id>` 动词 + `keys.key_actions` 全局表 | 已完成 |
| ~~二~~ ✅ | 方案级 `[key_actions]` 表（B/D 类 + 有字符键）+ 字母裁决泛化 | 已完成 |
| 三 | 设置页动态列表编辑器（见 §9.3） | 中，新控件 |
| 四 | 无字符键（修饰键 keyup 通路）+ C 类进方案级表 | 中高，触碰 `is_toggle_mode_keycode` |
| 五 | 全局层收编 + 冲突检测 + **A 类补全** | 高，改既有行为 |

设置页排在三期而非最后：二期交付后能力只能靠手改方案文件使用，中间隔太久等于没交付。
A 类反过来排到最后——它是功能完整性，不是任何人的诉求。

一期落地情况（`keys.key_actions` 全局表 + `toggle_schema:<id>`）：

- 一期就建 `keys.key_actions` 而不是加一张 `schema_toggle_hotkeys` 平行表，是为了让四期
  「全局层收编」直接扩充动词即可，不必先拆掉一个临时机制。键名走 `parse_hotkey`，
  组合键与单键同一形态，与二期方案级表的键形态天然统一。
- 动词走**白名单**（`is_supported_key_action`）。写错的动词若静默进热键表，按下时分发端
  匹配不上、什么都不发生，与「热键没注册上」完全同形，用户无从分辨是自己拼错了还是
  功能坏了。
- 无字符键（`rshift` 那类）仍不可用——它要走 keyup 轻敲通路，是四期的事。一期能配的是
  组合键，如 `"ctrl+shift+n" = "toggle_schema:english"`。

二期落地情况（方案级 `[key_actions]`）：

- 字母的「活码前缀」裁决按计划从 z 泛化到**任意绑了动作的字母键**
  （`bound_action_key_yields`）。没有这条，用户在某方案把 `u` 绑成功能键后，`u` 开头的编码
  在该方案里就彻底打不出来且毫无提示。`z_key_repeat` 那条**仍是 z 专有**——repeat 功能
  本身绑死在 z 上，不是通用概念。
- 字母键沿用「仅码表引擎」的限制（拼音/混输里字母全是有效输入，借作功能键会丢首字母）；
  **符号键不限引擎**——拼音方案里用 `\` 进快符同样合理，这是二期新增的能力。
### ★★ 二期真机翻车：进模式有两条通路，方案表只接了一条

**现象**：方案里写 `semicolon = "none"`，空码按 `;` 仍然进快捷输入。

**根因**：进同一个模式有**两条**通路——
1. `try_activate_mode`（空缓冲激活链，`handle_lifecycle.rs`）
2. `decideBufferedTrigger` 的「顶字 + 进模式」链（`coordinator.rs` 的 `_ =>` 臂）

第二条的注释写着「缓冲非空/有候选时」，但那个守卫只管到 `select_overflow`，**模式触发判定
本身不要求缓冲非空**，空码按键照样走到。于是第一条按 `none` 放行（`return None`），
第二条毫不知情地接管。

**修法**：判据抽成单点 `bound_key_decision() -> BoundKeyDecision{NotBound|Yield|Act}`，
两条通路都接；第二条另需一份「顶字版」进入函数 `commit_and_enter_bound_action`
（与 `commit_and_enter_mix_mode` 之于 `enter_mix_mode` 同构）。

★ **盘查的判据是「进这个模式有几个入口」，不是「我改的那个函数里有几个分支」**。
同源教训已记录在 `project_mixed_overflow_vs_topcode`（混输上屏三条通路，否决开关必须三处
都接，已栽四次）——本次是第五次，因为只盘查了自己改的函数内部。

同批修的第二处漏接：`try_z_fallback` 读的是 `z_key_action()` 而非 `bound_action_for()`，
方案把 z 改绑到快符表后，首键进快符、而夺取路径仍按「临拼」判定，同一个键在两条路径上
成了两个身份。

### 测试缺口已补

`Coordinator::new_headless_with_override` 允许集成测试指定**临时** override 目录。
此前只有 `new_headless`，它让 `EngineManager` 取真实用户目录的 `schema_overrides`，
测试写进去会污染用户配置——于是方案级覆盖的行为整个没法在集成测试里验证，上面那个
两条通路的 bug 正是因此漏到真机。`tests/schema_key_actions.rs` 覆盖 `none` 的否决
（含对照组）与「方案表压过全局 `z_key_action`」。

## 8. 测试要求

- **未配置 `key_actions` 时行为不变**：现有 `input_flow.rs` 全量即是这条的回归网。
- **每类动词至少一个方案级用例**，且必须先断言「确实进了该方案」——触发键未生效时
  按键会落普通输入，而普通输入的某些结局与预期结局同形，不验证进入就是假绿。
- **`none` 的用例**：配了 `none` 的键既不执行 action、也不落全局链。
- **锁死回归**：`toggle_schema` 从 A 进 B 再按回 A，且 B 的 `key_actions` 为空。
- **字母裁决**：某字母绑 action 但在本方案是活码前缀 → action 不执行、编码正常打出。
- ⚠️ 符号键测试不能按字符传 VK（`/` = 0x2F ≠ `VK_OEM_2` 0xBF，错了照样绿）。
- ⚠️ 依赖真实词库的用例需 `build_dev/data`，缺失时静默跳过且计数照常绿，判据是耗时。

## 9. 已定决策

### 9.1 落点：方案文件顶层

`Schema` 结构加与 `engine` 平级的字段。见 §3。

### 9.2 A 类只作功能补全

方案级的 A 类没有真实诉求——唯一想得到的用途「在本方案禁用某键」由 D 类的 `none`
承担。故排在最后一期，做它是为了动词表完整（用户能在设置页看到所有功能而不必记
「哪些能配方案级、哪些不能」），不是为了解决问题。

### 9.3 设置页：动态列表，可增删

一行一条绑定（键 + 动词），支持添加/删除，与 `dialog_button_mix_members` 那种列表
编辑器对齐——现有 manifest 的 `select` / `checkbox_group` 装不下动态行数。

三条约束：

- **键唯一性即时校验**：Map 的后写覆盖先写是静默的，UI 必须在添加重复键时当场拦，
  否则用户会看到自己刚配的一行凭空消失。
- **顺序不可作为优先级呈现**：列表看起来有序，用户会自然以为上面的先生效。UI 应按
  键名排序（与 `BTreeMap` 一致）而非按添加顺序，从呈现上就断掉这个误解。
- **动词选项要按当前已安装方案动态生成**：`special:<id>` / `mix:<id>` / `toggle_schema:<id>`
  的实例 id 随安装增删。参考 `z_key_action` 下拉的做法——清单列举不到的当前取值必须由
  「保留当前配置」项兜住，否则保存时静默改写成首项。

## 10. 已验证：`merge_toml` 对键集合可变的表逐键合并

原为待验证项（§3 的覆盖语义能否落地取决于它）。已实证，结论有利：

`merge_toml`（`wind-engine/src/manager.rs:199`）对 `Table` 递归逐键合并，只有非 Table
才整体替换。它**不关心键名是结构字段还是用户数据**，故 `[key_actions]` 这种键集合可变
的表与已有的固定字段表行为一致：override 未提及的键保留、同名键覆盖、新增键加入，
结果 = 两侧键集合的并集。

用例 `merge_toml_merges_tables_with_arbitrary_key_sets`（同文件 tests 模块）钉住这条，
并顺带钉住那条**能力缺口**——合并无法删除键，故 §3 的 `none` 哨兵是必需的。

> 该用例与既有的 `merge_toml_table_recurse_and_scalar_replace` 看似重复，保留两个的
> 理由：后者用的是已知字段（x/y/z），改动它的人不会意识到自己同时也在改「用户任意
> 键名」那条契约。分开写，改哪个红哪个。
