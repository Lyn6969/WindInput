# 工具栏自定义：条目显隐排序 + 自定义快捷按钮

状态：✅ **三阶段已实施**（主仓 `d9dced54` / `698673f9` / `cb5468e0`，设置仓
`cf51bb3` / `188eb50`，文档站 `199d8ef` / `513f9a7`），**未真机验证**。
实施中的四处偏离与新发现已回写本文（§5.2、§6.2、§11）。
起因：工具栏当前是硬编码的 `[方案][标点][全半角]([简繁])[设置]`（`wind-ui/src/toolbar.rs:355` 的
`fn cells`），用户既不能关掉不用的格，也不能加自己的入口（如一个「符」按钮打开系统字符映射表）。

本文定两件事：**内置条目的显隐与排序**（common 档，设置页可视化编辑）、
**自定义按钮**（expert 档，动作复用 cmdbar 表达式）。

---

## 一、这是两个不同性质的功能

| | 需求 1：条目显隐排序 | 需求 2：自定义按钮 |
|---|---|---|
| 配置形态 | 有序 StrList | StructList |
| 受众档（`config-design-rules.md` §R5） | `common` | `expert` |
| 是否引入执行外部程序的能力 | 否 | **是**——见 §6 |
| 现成先例 | `ui.status.items` / `schema.mix_modes` 的成员管理 | `ui.comment_dicts` |

两者共用**同一个有序列表**表达显示与顺序（§2.1），但定义分处两个键——这是「引用」关系，
不是同一语义的两张表（对比 `key-actions-materialization.md` 里两个 writer 争同一个键的翻车）。

### 1.1 为什么顺序可以做进配置

`config-design-rules.md` §R3 有一条约束：

> 顺序带语义的 StrList 注意：GUI `checkbox_group` 恒按声明顺序写回，用户手排的顺序会被
> 静默改写——要么把声明顺序钉成语义，要么上列表编辑器，**不得两不管**。

本需求走「上列表编辑器」这一侧，且**控件是现成的**：`wind-setting/src/dialogs/field_dialogs.rs:271`
的 `build_toggle_rows_dialog`（开关决定有无 + 拖拽手柄决定顺序，`order: Signal<Vec<String>>`
是顺序真相源），当前已被快捷输入的 `schema.mix_modes` 成员管理使用（`manifest.rs:2290`）。
另有 `schema_manager.rs:2727` 的 `reordered_enabled_order` 是带单测的双区拖拽纯函数内核。

⚠️ **`ui.status.items` 不是本需求的参照物**。它是 `checkbox_group`（声明顺序即显示顺序、
不可拖拽），照抄它就落进上面那条约束的「两不管」里。

---

## 二、配置形态

### 2.1 `[ui.toolbar]`

```toml
[ui.toolbar]
# 显示哪些条目、按什么顺序。数组顺序 = 渲染顺序。
# 内置项：mode / punct / full_width / s2t / settings
# 自定义项：custom:<id>，引用下面 [[ui.toolbar.buttons]] 里同 id 的按钮
# 留空 = 全部显示（旧配置无此键时行为不变）
items = ["mode", "punct", "full_width", "s2t", "custom:sym", "settings"]

# 自定义按钮定义（expert 档）
[[ui.toolbar.buttons]]
id = "sym"                          # 稳定标识，被 items 引用
label = "符"                        # 1 个汉字或 2 个 ASCII（见 §5）
tooltip = "打开字符映射表"           # 可选，悬停提示
action = 'proc.run("charmap.exe")'  # cmdbar 表达式（见 §4）
enabled = true                      # 缺省 true
```

### 2.2 三条判据

**① 留空 = 全部显示，而不是全部隐藏。**
与 `ui.status.items` 同一取舍：「未配置」的合理默认是全显，且让无此键的旧配置行为不变。
想全部隐藏的正确表达是 `ui.toolbar.visible = false`——那才是「不要工具栏」这个意图的落点。
于是「空列表」这个取值有了唯一语义，不需要额外的自锁兜底逻辑。

**② `s2t` 是合取，不是替代。**
`ToolbarState.s2t_shown` 是运行时条件（用户开启简繁功能后才为 true）。加 `items` 后判据是
`items 含 s2t && state.s2t_shown`。⚠️ 用户没开简繁功能时，勾了也不显示——**hint 必须写明**，
否则就是一个「勾了没反应」的旋钮（`feedback_settings_hint_concise` 的判据：这条限制在用户
环境里真的会触发，所以是提示不是备注）。

**③ 隐藏 `settings` 不会锁死用户。**
设置格是主菜单的鼠标入口（`toolbar.rs:1000`），但**右键工具栏任意位置**同样弹主菜单
（`toolbar.rs:1012` 的 `WM_RBUTTONDOWN`）。两条路并存，故 `settings` 可自由隐藏。

### 2.3 非法输入的处置

| 情形 | 处置 |
|---|---|
| 未知内置项键（拼写错） | 跳过 + `warn!` |
| `custom:<id>` 引用不存在的按钮 | 跳过 + `warn!` |
| 按钮 `enabled = false` | 不渲染（`items` 里留着，设置页开关的落点就是它） |
| 解析后渲染项为空 | 回落全集 + `warn!`（判据 ① 已让这条几乎不可达，留作兜底） |

---

## 三、数据通道：新增独立 UiCommand，绝不进 `ToolbarState`

### 3.1 判据

工具栏的数据链路本来就分两侧，这是既有分界：

- **动态状态**走 `UpdateToolbar(ToolbarState)`，高频推送，靠 `PartialEq` 去重（`toolbar.rs:5` 的
  类型文档写明了这个用途）。
- **配置参数**走独立 `UiCommand`（`SetToolbarVertical` / `SetToolbarAutoHide`），
  只在 `apply_ui_config`（`coordinator.rs:3290`，启动 + 配置重载共用单点）下发一次。

按钮清单是配置。塞进 `ToolbarState` 的后果是每次按 Shift 切中英都要 clone 一遍按钮列表
并做深比较——去重反而变成了开销。

### 3.2 协议

```rust
// wind-ui-types/src/command.rs，与 SetToolbarVertical 并列
SetToolbarLayout(Vec<ToolbarItem>),

// wind-ui-types/src/toolbar.rs
pub enum ToolbarItem {
    Mode,
    Punct,
    FullWidth,
    S2t,
    Settings,
    /// `index` = 该按钮在 `ui.toolbar.buttons` 里的下标，点击时经
    /// `ToolbarAction::Custom(index)` 原样回传。
    Custom { index: u8, label: String, tooltip: String },
}
```

**字符串解析在协调器做，不在 UI 做。** UI 侧不该懂配置键的取值——它收到的已经是一份
「按顺序渲染这些东西」的声明。这同时让 `wind-ui-types` 保持纯数据（不引入配置依赖，
headless / Android 侧照常编译）。

⚠️ `ToolbarAction::Custom(u8)` 的载荷**必须是 `u8` 而不是 `String`**：`ToolbarAction` 是
`Copy`（`menu.rs:7`），`cell_at` / `hover_at` / `hits: Vec<(ToolbarAction, Rect)>` 全建立在
这个前提上，带 `String` 会让整条命中链路改签名。

索引失配（配置重载后 UI 侧 spec 与协调器配置错开一瞬）的最坏后果是执行相邻按钮的动作，
非破坏性；协调器侧 `.get(i)` 越界即忽略。

### 3.3 各端影响

| 端 | 处置 |
|---|---|
| Windows `wind-ui/src/manager.rs:864` | 加 match 分支。⚠️ **缺分支编译过但静默无效**（`wind-ui/AGENTS.md`） |
| macOS `manager_macos.rs:506` | 有 `other =>` 兜底，**无需改代码**；但 `wind_macos/AGENTS.md` 的「与 Windows 的功能差距」表要加一行 |
| Android / headless | `ToolbarItem` 是纯数据，无平台代码；无工具栏窗口，命令被忽略 |

### 3.4 渲染侧改动

`wind-ui/src/toolbar.rs`：

- `fn cells(state: &ToolbarState) -> Vec<Cell>` → `fn cells(&self, state)`，读 `self.layout`。
- 分隔线规则从 `i == 0 || is_settings` 扩为：**首格前 + 首个 `Custom` 前 + `Settings` 前**
  （内置状态格之间仍不画，对齐设计稿）。
- `bar_layout` / 圆角 / 高亮 / 纵向转置**一律不动**——它们已按 `cells.len()` 参数化，
  且有 `layout_tracks_cell_count` 等 7 条回归测试钉住（`toolbar.rs:1129+`）。

---

## 四、动作执行：复用 cmdbar，不新建执行路径

### 4.1 判据

本仓已有一套完整的动作 DSL（`wind-cmdbar/src/funcs/action.rs`）：
`open`（ShellExecute 语义，URL / 程序 / 文件通吃）、`proc.run`（带 `cwd` / `verb=runas` /
`show=min` 具名参数与**取值白名单校验**）、`proc.shell`、`key.tap` / `key.seq`、`wind.cli`、
`clip.copy`、`web.search`。

协调器侧执行入口是 `handle_cmdbar.rs:65` 的 `run_command_candidate(src, input)`，它已经带好了：
求值失败弹 toast（不是哑失败）、Text 与 Effect 动作的时序协调、整条链只弹第一个错误。
**短语里的 `$CC` 动作走的就是这同一个函数**（`handle_candidate.rs:3117`）。

自己实现 `{type="app", path="..."}` 等于把 ShellExecute 的参数校验、错误反馈、跨平台差异
重写一遍——而那些坑（`verb` 拼错只回一个泛化错误码，所以要收白名单）已经踩过并写进注释了。

### 4.2 接线

```rust
// handle_menu.rs mouse_toolbar，现有 match 里加一支
ToolbarAction::Custom(i) => {
    let Some(btn) = self.rt().config.ui.toolbar.buttons.get(i as usize) else { return };
    let src = btn.action.clone();
    // ⚠️ run_command_candidate 的文档要求：必须在独立线程、未持 state 锁时调用
    //    （控制器会回调自锁的 coordinator 方法）。抄 handle_candidate.rs:3111 的 spawn_command。
    self.spawn_command(src, String::new());
}
```

`Services`（`ime` / `dict` / `proc` / `open` / `clip` / `keys` / `config`）由 `init_cmdbar`
（`handle_cmdbar.rs:30`）一次性装配存进 `OnceLock`，任何调用点 `self.cmdbar_services.get()`
即得，无需重建。

⚠️ 现有 `mouse_toolbar` 末尾有一个 `ToolbarAction::ToggleS2t | OpenSettings => unreachable!()`
（`handle_menu.rs:1702`），加变体时必须一并处理——`Custom` 落进那个 `unreachable!()` 就是运行时 panic。

### 4.3 能力面自动变宽（是红利，不是范围蔓延）

复用带来的直接结果：用户不止能「链接到系统中的应用」，还能配
`web.search("baidu", ...)`、`wind.cli("schema switch wubi86")`（一键切方案）、
`key.tap("Ctrl+Shift+P")`。这些都是同一套已测试的实现，零额外代码。

---

## 五、label 宽度

### 5.1 现状与风险

`toolbar.rs:609-627` 画文字是 `measure_text` 取宽 → 居中 → `draw_text` 从 `tx.max(r.x)` 起画，
**没有任何裁剪**。格宽是主题 `button_width`（默认 30dp），字号固定 `FONT_PX = 15`。

当前内置文本恒为 1 个汉字，从不溢出；允许用户配 label 后，超长文本会画到隔壁格上。

### 5.2 处置：加载期截断 + 日志告警

按显示宽度计算（CJK 计 2、ASCII 计 1），> 2 则截断到 2 并 `warn!`。落在配置加载期
（`wind-config`），而非渲染期——「写错了要有线索」是这条的全部目的。

默认几何下 1 个汉字 ≈ 15px、2 个 ASCII ≈ 15px，都在 30px 格内有富余，截断后不会溢出。

⚠️ **已知缺口**：若某主题把 `button_width` 配到 < 20dp，2 个 ASCII 仍可能溢到隔壁格。
渲染期自适应缩字号是可行的（`TextRenderer` 已有 per-call 字号的 `measure_text_sized` /
`draw_text_sized`，`text/dwrite.rs:419,550`），但本期不做。若日后要补，注意
**测量与渲染必须用同一个字号值**，否则居中偏移算错（候选窗宽度那次的教训）。

### 5.3 为什么不复用 `icon_label_trunc`（实施时的判断）

`schema::icon_label_trunc` 是本仓已有的「统一截断口径」，其文档还专门警告过各写一份
的后果。但它的口径是**字符数 ≤ 2**（语言栏 16×16 图标的既有尺度），这里要的是
**显示宽度 ≤ 2**——对「符号」两者判断相反：前者放行（2 个字符），后者截成「符」。

分野在于两处要保证的东西不同：语言栏图标是独立的一张位图，画 2 个 CJK 只是挤；
工具栏格是**等宽方格排成的一条**，一格画得比别格宽就破了整条的节奏，而用户要的正是
「和其它格长宽比一致」。改 `icon_label_trunc` 去迁就这里，会连带改掉方案标签与语言栏
图标的既有行为。

⚠️ 故 `toolbar_label_trunc` **不是**漏掉的复用。若日后要合并两条口径，得先确认语言栏
那三个调用点接受「符号」被截成「符」。

---

## 六、安全：`config.toml` 首次持有可执行内容

### 6.1 这是一个被打破的不变量

`docs/architecture/package-format.md` §5 的安全原则是「能力越强的内容走越窄的门」，
而当前的门是这么分的：

- **配置片段 / 配置包**：键域 = `config_schema::REGISTRY` ∪ `ALLOWED_UNREGISTERED_KEYS`
  （`patch.rs:53`），是**最开放**的一档——因为 `config.toml` 里**没有任何可执行内容**。
- **能执行外部程序的短语**（`$CC(..., proc.run(...))`）属于**用户数据**，
  只进备份包、**永不进分发包**（`package-format.md:21`）。风险从格式层面就被挡住了。

`[[ui.toolbar.buttons]]` 会第一次让 `config.toml` 持有可执行内容。而 `patch.rs` 对
`StructList` 键是**整值覆盖**（`ui.comment_dicts` / `schema.mix_modes` 是先例，见
`config_schema.rs:811-823`），于是一份配置片段可以整表写入按钮定义：

> 导入配置片段 → 工具栏多了个按钮 → 用户点一下 → 任意程序执行，全程无提示。

### 6.2 本期处置：导入预览警示（已实施）

不阻断写入，`PatchEntry` 加 `warning` 字段（与 `error` 正交、不影响 `ok`），
危险键在片段预览里单起一行、Warning 色、带 ⚠ 前缀。

三处实施要点：

- **提示在 `preview` 单点按键补**，不在 `flatten` 的三个 `PatchEntry` 构造处各填一次
  ——那样加第四个构造点时必然漏，而漏的表现是「提示没出现」，无人会发现。
- **放在 `error` 判断之前**：写错了的危险键照样提示。用户看到「这条有错」多半会改对
  再导入一次，那时提示就该已经说过。
- **用 Warning 而不是 Danger**：Danger 在预览列表里已表示「这条有错、不会应用」，
  混色会让「合法但有风险」看着像「导入失败」。

`RISKY_KEYS`（`wind-config/src/patch.rs`）是这类键的登记处。⚠️ **日后再加能执行程序 /
改写启动项一类的配置键时必须登记**，判据不是「这个键危不危险」，而是「一份陌生片段
写了它之后，用户的某次寻常操作会不会变成执行对方给的代码」。守门测试保证名单里的键
真在 REGISTRY 里（否则那条提示永远不会触发）。

### 6.3 被记下但本期不做的两条

- **`patch.rs` 加不可片段写入的键黑名单**（`NON_PATCHABLE_KEYS`）。技术上最干净：
  一个常量数组 + 一条判断 + 一条守门测试，设置页直写 `config.toml` 不受影响。
  代价是引入一个新机制，且以后每个可执行配置键都得记得登记。
- **`action` 落 redb 而非 config.toml**。按 §R2「实例身份从哪来配置就落到哪」，
  自定义按钮的动作确实更像短语（数据），落 redb 可自动继承「只进备份包」的现成边界、
  零新机制。⛔ 否决理由：按钮定义会分裂成两处（label / 顺序在 config、action 在 redb），
  引入「还原了配置没还原数据 → label 在但 action 没了」的新错误面。

⚠️ 若分发包场景日后变得活跃，优先重估 §6.3 第一条。

---

## 七、被否决的备选

| 备选 | 否决理由 |
|---|---|
| ⛔ `items` 只管显隐、顺序固定 | 用户明确要排序；且 `build_toggle_rows_dialog` 已是现成控件，成本本来就不高 |
| ⛔ 自定义按钮固定排在末尾、不参与排序 | 有了列表编辑器之后，把 custom 排除在外是没有理由的半吊子 |
| ⛔ 按钮定义写死 `{path, args}` 两字段 | 要重写 ShellExecute 的校验与错误反馈；且「一键切方案」这类动作没有出路（§4.1） |
| ⛔ `ToolbarAction::Custom(String)` | `ToolbarAction` 失去 `Copy`，整条命中链路改签名（§3.2） |
| ⛔ 按钮清单进 `ToolbarState` | 每次切中英都 clone + 深比较，把去重变成开销（§3.1） |
| ⛔ 照抄 `ui.status.items` 的 `checkbox_group` | 落进 §R3 的「顺序两不管」（§1.1） |

---

## 八、落点清单

`config-design-rules.md` 附录 checklist 的实例化。

### 8.1 主仓

| 文件 | 改动 |
|---|---|
| `wind-config/src/config.rs:3155` | `ToolbarConfig` 加 `items: Vec<String>` + `buttons: Vec<ToolbarButtonSpec>`；`TOOLBAR_ITEM_KEYS` 常量；label 截断 |
| `wind-config/src/config_schema.rs:430` | REGISTRY 补 `ui.toolbar.items`（`StrList`）、`ui.toolbar.buttons`（`StructList`）。⚠️ `registry_covers_every_config_key` 强制全键覆盖，漏登记必红 |
| `data/config.toml` | L2 补键（`data_config_toml_covers_registry` 会拦）。⚠️ `buttons` 参照 `ui.comment_dicts` 的先例，大概率登记进「不写进预置文件」的豁免表（`config_schema.rs:823`） |
| `wind-ui-types/src/toolbar.rs` | 加 `ToolbarItem` |
| `wind-ui-types/src/command.rs:96` | 加 `SetToolbarLayout` |
| `wind-ui-types/src/menu.rs:8` | `ToolbarAction` 加 `Custom(u8)` |
| `wind-coordinator/src/coordinator.rs:3297` | `apply_ui_config` 解析 items + 下发 |
| `wind-coordinator/src/handle_menu.rs:1697` | `mouse_toolbar` 加 `Custom` 分支（含那个 `unreachable!()`） |
| `wind-ui/src/manager.rs:864` | 新命令的 match 分支 |
| `wind-ui/src/toolbar.rs:355,513` | `cells()` 改实例方法；分隔线规则 |
| `wind_macos/AGENTS.md` | 功能差距表加一行 |

### 8.2 设置仓（wind-setting）

- `settings_manifest.toml:2341`（section「工具栏」）加 `ui.toolbar.items`，形态 =
  宿主行 + `opens_dialog`，对话框复用 `build_toggle_rows_dialog`。
- `ui.toolbar.buttons` 是 StructList，本期登记 `UNCOVERED_BY_DESIGN`（理由：expert 档、
  面向配置文件），P3 再做编辑器。
- ⚠️ 五道守门闸门依次拦（`config-design-rules.md` §R6），照报错提示修。
- ⚠️ `manifest.rs:3413` 的 `toolbar_items_hidden_on_macos` 断言了工具栏各键的平台可见性，
  新键要决定 `platform` 并同步这条测试。

### 8.3 文档站（WindInputDocs）

`guides/config` 参考页 + `settings` 用法页两处，缺一不可（§R7）。

---

## 九、实施阶段

| 阶段 | 内容 | 可独立验证 |
|---|---|---|
| **P1** | `items` 显隐排序 + 渲染重构 + `SetToolbarLayout` + 设置页对话框 | 是：不含任何执行能力，纯呈现 |
| **P2** | `buttons` + cmdbar 接线 + label 截断 | 是：`debug_run_command` 可单测动作链 |
| **P3** | 设置页按钮编辑器 + 导入确认框警示（§6.2） | 是 |

P1 的设置页对话框只列内置 5 项；custom 项进对话框要等 P2 落地。

---

## 十、测试要点

- **纯函数优先**：items 字符串 → `Vec<ToolbarItem>` 的解析要抽成纯函数（含未知键 / 悬空
  引用 / 空列表三条），在 `wind-config` 或协调器里单测——`toolbar.rs` 的渲染部分在非 Windows
  上是 mock，覆盖不到（`bar_layout` 被抽成纯函数就是这个理由，见其文档）。
- **`s2t` 合取**要有一条测试：`items` 含 `s2t` 但 `s2t_shown = false` 时不出现。
- **label 截断**按显示宽度而非 `chars().count()`：`"AB"` 保留、`"符号"` 截成 `"符"`。
- ⚠️ **P1 的回归基线**：`items` 留空时渲染出的格序列必须与改动前**逐格相同**，
  否则就是给所有老用户改了外观。

---

## 十一、实施记录：设计外的四处

### 11.1 🔴 空展开：兜底装错了层（P1 审查抓出）

`parse_toolbar_items` 保证**项序列**非空，但 `S2t` 是运行时合取——配
`items = ["s2t"]` 而简繁没开时项序列非空、展开结果却是空的，工具栏渲染成一条
只剩 12dp 拖动柄的窄条。设置页里只勾「简繁」一项就能走到，不必手写 TOML。

判据：**兜底要装在产出最终结果的那一环**。装在解析层只能保证「配置里写了东西」，
而决定画几格的是 `expand_cells`。修法是把展开逻辑抽成自由函数（`expand_cells`
+ 无兜底内核 `expand_cells_raw`），兜底放在外层——分两层是为了让兜底自身可测，
否则「回落」与「本来就该有格」两种结果长得一样。

### 11.2 齿轮排首位时没有分隔线（P1 审查抓出）

原规则「首格前 + 设置格前」在齿轮居首时两条线重合，齿轮反而失去边界。
改为：齿轮在首位时画**下一格**的起始边（= 齿轮的结束边）。

### 11.3 tooltip 已从方案中移除

§2.1 原本给 `[[ui.toolbar.buttons]]` 设计了 `tooltip` 字段，实施时发现
**工具栏根本没有悬停提示机制**（`wind-ui` 的 tooltip 窗口绑在候选窗上，是编码
反查用的）。加一个渲染端消费不了的字段等于给用户一个配了永远没反应的旋钮，
故整个字段删除。要做提示得先给工具栏做悬停窗口，那是独立的一件事。

### 11.4 设置页控件会**静默降级**成占位符

§8.2 只写了「manifest 加项 + 复用 `build_toggle_rows_dialog`」，漏了一处：
`wind-setting` 的控件渲染有**两处分派**——`build_dialog_body`（对话框主体）与
`build_control`（行内控件）。只加前者时，capability 校验照样过、741 条测试全绿，
界面上那一行却是 `[类型名?]` 灰字占位符。

**这是 `--screenshot` 才看出来的**，没有任何测试会红。已补 `RENDERABLE_CONTROL_TYPES`
名单 + 两条方向相反的守门（清单⊆名单、名单⊆实现），后者靠 `build_control` 兜底
分支里的 `debug_assert`。

⚠️ 教训一般化：**改设置页 UI 后要截图自查**（`cargo run -- --page ui --screenshot <path>`，
配 `--click X Y` 可展开对话框）。这个仓的测试覆盖的是数据契约，不是"画出来长什么样"。
