# 工具栏自定义：条目显隐排序 + 自定义快捷按钮

状态：📝 **设计定稿，未实施**。
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

### 6.2 本期处置：导入确认框警示

不阻断写入，但**片段预览必须对这个键标红**，文案回答「这个包会往你的工具栏放一个能启动
外部程序的按钮」。落点是 `package-format.md:182` 的「确认对话框最低内容」清单——
那份清单现在只有来源 / id 版本 / 文件数 / 冲突数 / config_patch 逐键 diff，**要新加这一项**。

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
