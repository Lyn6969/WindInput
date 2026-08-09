# Overlay 模式配置下沉 —— `schema.special_modes` 解散

> [schema-config-layering.md](schema-config-layering.md) 的续篇。那一轮把**引擎配置**从
> `[[schema.special_modes]]` 迁进被引用方案的 `.schema.toml`；这一轮把剩下的**呈现配置**
> 也迁下去，并解散 `schema.special_modes` 这个数组本身。
>
> 上游：[mode-scheme-unify.md](mode-scheme-unify.md)（base/overlay 模型）、
> [schema-key-actions.md](../design/schema-key-actions.md)（五c 引导键收编）。
>
> 本项目未发布，**不做存量迁移**（口径沿用 schema-config-layering.md）。

## 1. 问题：实例集合没有稳定 key

`schema.special_modes` 是 `Vec<SpecialModeConfig>`，条目身份靠**数组下标**。这一个事实
同时制造了三个症状：

1. **`data/config.toml` 写不进去**——写出即冻结快照（该文件 §29-38 的豁免注释），于是
   "本文件同时是全部可配置项的说明书"这条契约在这一项上是破的。
2. **设置页套不上**——`wind-setting/src/capabilities.rs` 的豁免注释已诊断到位：
   *"没有固定 id、条目随已安装方案增删，manifest 的『一个 key 一个控件』模型套不上"*。
   于是只能"按 `schema` 字段定位条目、没有就新建"，且**必须保存原始 `Value` 而非结构体**，
   否则重建会把手写字段静默抹掉。
3. **`ModeKind::Special(u8)` 的下标语义绑在用户配置数组上**。

而五c 收编引导键之后，条目里的 `trigger_keys` 已被清空废弃，条目**唯一的存在理由**
退化成"`special:<id>` 需要指向一个真实存在的实例定义"——用户在设置页配一个引导键，
副作用是 config.toml 里凭空长出一段空壳 `[[schema.special_modes]]`。

★ 判据：**配置该住哪里，看的不是"它是不是方案的属性"，而是"它的实例身份从哪来"。**
`effective_id` 已回落 `schema`、`name` 已回落 `schema_name`、`short_name` 已回落
`schema_icon_label`——身份早就全部来自被引用方案，数组只剩一个壳。

## 2. 目标态：实例即方案

方案文件新增 `[overlay]` 段（与 `[engine]`、`[key_actions]` 平级）。**段的存在本身即声明
"我可以被当作 overlay 激活"**，同时是实例集合的枚举依据。

```toml
# kf.schema.toml
[schema]
id = "kf"
name = "快符"
icon_label = "符"
hidden = true

[overlay]
kind = "special"            # overlay 类别；当前只有 special
show_all_on_enter = true
candidate_layout = "vertical"
# comment_template_vertical / _horizontal 同 tri-state 语义（不写=跟随全局，写空串=不显示）

[engine.codetable]          # 上一轮已下沉，不动
max_code_length = 1
single_code_input = true
auto_commit_at_full = true
```

`schema.special_modes` 整键删除（`SpecialModeConfig` 一并删）。

### 2.1 字段归属判据

判据是**「这条配置被读的时候，需不需要跟别的实例比较」**：

| 字段 | 去向 | 理由 |
|---|---|---|
| `id` / `name` / `short_name` / `schema` | **删除** | 已在派生；条目即方案自己，`schema` 字段变成自指 |
| `show_all_on_enter`<br>`candidate_layout`<br>`comment_template_*` | `[overlay]` 段 | 只在该模式激活期间读，无跨实例比较 → 纯下沉 |
| `trigger_keys` | **已在** `keys.key_actions`（五c） | — |
| `hotkey` | `keys.key_actions` | 它是组合键，正是那张表的形态；见 §4 |

★ 这三个下沉字段的共同点，恰好解释了段名为什么叫 `overlay`：它们描述的都不是
"这张码表是什么"（那是 `[engine.codetable]`），而是"**这张码表被叠加使用时怎么表现**"。
`show_all_on_enter` 只在存在"进入这一刻"时才有意义；`candidate_layout` 的语义是
"本模式期间覆盖全局、退出自动恢复"。没有 overlay 生命周期，这三个字段都不成立。

> ⚠️ `overlay` 在本仓有两个粒度：**运行时状态**（临拼/临英/mix/URL 也都是 overlay，
> 但它们没有宿主方案，配置只能待在 `input.*` / `schema.mix_modes`）与**方案文件的一个段**
> （仅有宿主方案者）。`kind` 字段用于消歧：段说"我可以被当 overlay 用"，`kind` 说"哪一类"。

### 2.2 ⛔ 不设 `[overlay].trigger_keys`

引导键已由五c 收编进 `keys.key_actions`。在方案文件里再开一个 `trigger_keys` 就是
**第三个真相源**，正是本轮要消除的东西。按键的落点只有两个，且都已存在：

| 落点 | 语义 | 存储 | 提交通路 |
|---|---|---|---|
| `keys.key_actions` | **全局**：按 `\` 进快符，所有方案生效 | config.toml | `FieldRegistry.writers` → `config.setItems` |
| 方案文件 `[key_actions]` | **按源方案分流**：五笔里 `\` 进五笔快符 | `schema_overrides/{id}.toml` | `SideCommitter` → `schema.saveConfig` |

> 第三方方案包想带默认引导键，将来可加 `[overlay].suggested_trigger` 作**导入期种子**
> （`scheme.importPackage` 时写进用户 `keys.key_actions`，之后与方案文件脱钩），而不是
> 运行时真相源。本轮不做。

## 3. Overlay 注册表：下标语义换源

`ModeKind::Special(u8)` 与 `State.special_id` **保留**，只把下标的语义来源从
"config 数组序"换成"**overlay 注册表序**"。

```rust
// wind-engine
pub struct OverlayEntry {
    pub schema_id: String,     // = 实例身份
    pub name: String,          // [schema] name
    pub icon_label: String,    // [schema] icon_label
    pub spec: OverlaySpec,     // [overlay] 段
}
```

- 枚举源是 **`installed_schemas()`**（已合并扫描安装目录 + 用户目录并 `sort`），
  **不是 `available`**——overlay 方案 `hidden = true`、不进 `schema.available`，
  只由 overlay 触发懒加载。
  ⚠️ 同样的理由，`all_key_action_keys()` 现在只遍历 `available`，若将来要收 overlay
  方案自己的 `[key_actions]`，枚举源也得换。
- 顺序 = `installed_schemas()` 的 id 字典序，**稳定且与用户配置无关**。
- 读方案走静态 `read_schema(id, data_dir, override_dir)`，故 `schema_overrides/{id}.toml`
  的 `[overlay]` 覆盖**自动生效**（`merge_toml` 逐键合并），无需额外接线。

### ★ 下标稳定性的取舍

按 id 排序意味着**安装一个新 overlay 方案会改变其后方案的下标**。这不比现状差
（现状是 config 数组序，热重载删条目同样错位，`layout.rs:51` 的注释已在处理越界回落），
且 overlay 是瞬态的（上屏即退出）。注册表刷新点与 `schema.invalidate` 对齐。

### ★ 纯函数的签名要跟着改

`layout.rs::intent_for` 与 `comment.rs::template_for` 现在是吃 `&Config` 的纯函数
（刻意与取值分离，便于不构造协调器就测出完整矩阵）。配置下沉后它们拿不到数据，
签名改为额外吃 `&[OverlayEntry]`。**保持纯函数形态**——测试直接造 `Vec<OverlayEntry>`，
可测性不退化。

## 4. `hotkey` 收编进 `keys.key_actions`

`hotkey.rs` 现在遍历 `special_modes` 编译 `enter_special:<id>`，且**跳过 id 为空的条目**
（分发点无法定位）——这条坑随字段删除一并消失。

收编要动两处：

1. **动词白名单**：`is_supported_hotkey_action` 目前只认 `toggle_schema:`，加 `special:`。
2. **★ 策略位必须按动词分**。该函数上方的注释已经预警过这一条：

   > ⚠ 后续接入别的动词时策略位必须按动词分，不能沿用这里的"一律不带"：进 overlay 的
   > 动词只在中文输入中途有意义，需要 `CHINESE_ONLY | GLOBAL`。

   | 动词 | 策略位 | 为什么 |
   |---|---|---|
   | `toggle_schema:<id>` | 不带 | 回程恰恰要在非中文态下按得动，带上 `CHINESE_ONLY` 就回不来 |
   | `special:<id>` | `CHINESE_ONLY \| GLOBAL` | 与原 `special_modes[].hotkey` 编译口径一致 |

3. **动词形态映射**：引导键通路用 `special:<id>`（`BoundAction`），热键分发端认的是
   `enter_special:<id>`。组合键编译时做这层映射，分发端零改动。

配置写法从

```toml
[[schema.special_modes]]
id = "kf"
hotkey = "ctrl+shift+u"
```

变成

```toml
[keys.key_actions]
"ctrl+shift+u" = "special:kf"
backslash = "special:kf"       # 引导键与直达热键从此是同一张表的两行
```

## 5. 不做存量迁移

沿用 schema-config-layering.md 的口径。**理由是收益面与风险不成比例**：

- 呈现类字段要写进 `schema_overrides/{id}.toml`，而 `normalize()` 在 wind-config、
  零 IO、幂等纯内存（五c 明确立过"在内存里做，不写盘——回退一版就能工作"），
  写盘 API 在 wind-engine，**依赖方向反不过来**。要落地就得引入本仓从未有过的
  "一次性副作用型迁移"：新执行点 + 迁移状态标记 + 幂等守卫 + 带临时目录的测试。
- 而手工重配约 5 行，且项目未发布。

**替代**：启动时检测到残留的 `schema.special_modes` 非空即 `warn!` 一条，指明它已失效
及新写法。约 10 行，不写盘、不可能出错。

## 6. 影响点清单

### core

| 文件 | 改动 |
|---|---|
| `wind-config/src/schema.rs` | 新增 `OverlaySpec`；`Schema.overlay: Option<OverlaySpec>` |
| `wind-config/src/config.rs` | 删 `SpecialModeConfig`、`SchemaConfig.special_modes`；加残留 warn |
| `wind-config/src/config_schema.rs` | 删注册表项 `schema.special_modes` 与 `ABSENT_FROM_DATA_CONFIG` 里的同名豁免 |
| `wind-config/src/hotkey.rs` | 删 `special_modes` 遍历；白名单加 `special:`；策略位按动词分 |
| `wind-engine/src/manager.rs` | 新增 `OverlayEntry` + `overlay_modes()` 注册表（枚举源 `installed_schemas`） |
| `wind-coordinator/src/handle_special.rs` | `special_mode_idx` / `special_schema` / `special_mode_show_all` 三处改查注册表 |
| `wind-coordinator/src/handle_mode.rs` | name/short_name 直接取注册表（不再"缺省回落"两段） |
| `wind-coordinator/src/layout.rs` | `intent_for` 加 `&[OverlayEntry]` 参数 |
| `wind-coordinator/src/comment.rs` | `template_for` 加 `&[OverlayEntry]` 参数 |
| `wind-coordinator/src/webdata.rs` | `schema.getConfig` 带出 `[overlay]`；`saveConfig` 照常 diff 落盘 |
| `data/config.toml` | 删 §29-38 豁免注释里的 `schema.special_modes` 一条 |

`coordinator.rs` 只需更新 `special_id` 字段的文档注释（下标语义换源），无逻辑改动。

### wind-setting

| 文件 | 改动 |
|---|---|
| `src/special_modes.rs` | **整个删除**（`ensure_special_mode_entry` 的存在理由消失） |
| `src/dialogs/schema_manager.rs` | 删 `special_modes: Rc<RefCell<Vec<Value>>>` 工作态；引导键改写 `keys.key_actions`；新增 overlay 配置区 |
| `src/capabilities.rs` | 删 `schema.special_modes` 豁免 |
| `src/mockdata/config.json` | 删 `special_modes` 段 |
| `capabilities.snapshot.json` | 重新生成 |

⚠️ **同一方案的 override 全局只允许有一个写入者**：`schema.saveConfig` 是"整份 cfg 与
方案文件 diff、结果全量重写 override"，overlay 配置与码表配置若各注册一个 `SideCommitter`，
后提交者会用自己那份 cfg 的 diff 把前者整个覆盖掉。两者必须投递进**同一个** per-schema
待提交队列（`schema_mgr.commit_pending_edits` 已按 id 累积合并）。

### 文档站（WindInputDocs）

配置参考页删 `schema.special_modes`、新增方案文件 `[overlay]` 段；用法页改写快符配置示例。

## 7. 分期与落地情况

| 期 | 内容 | commit |
|---|---|---|
| 1 | `OverlaySpec` + overlay 注册表（纯新增） | `39f09e1` ✅ |
| 2 | 6 个消费点切到注册表 | `057b28b` ✅ |
| 3 | `hotkey` 收编 + 删除 `SpecialModeConfig` + 残留 warn | `602f14b` ✅ |
| 4 | `saveConfig` 往返测试 + `schema.list` 带 overlay 标志 | `61c3e1f` / `4639b08` ✅ |
| 5 | wind-setting：清理 + 新增「特殊模式」配置节 | `d69a41e` / `85f43d1` ✅ |

**未真机验证**：引导键/直达热键实际进入、设置页新节的渲染与保存效果，均需真机确认。

### 实施中的两处修正

**① `State.overlay_spec` 快照**（设计原稿没有）。原打算让 `intent_for`/`template_for`
直接吃 `&[OverlayEntry]`，实施时发现 `template_for` 返回的是**借用 `cfg` 的 `&str`**
（刻意不分配），临时 Vec 借不出来。改为进入模式时把 `[overlay]` 段快照进 `State`：
借用有处可依、省掉候选路径上的整表 clone，且**注册表因装新方案而下标平移时，进行中的
模式不会被换成隔壁那个**。这不是 `layout.rs` 反对的「保存/回放」——快照的是只读配置，
随 `active = None` 自然失效，没有需要被执行的恢复动作。

**② 设置页判据从 `entry.hidden` 换成 `entry.overlay`**。引导键那一行原先按 `hidden`
显示，那是 overlay 的**代理**（当年只有快符/英文两个隐藏方案）。但它一直不准：`english`
也是 hidden 却没有 overlay 生命周期，给它配引导键按下去什么都不会发生。

### ⚠️ 顺带发现、**刻意未改**的一处同形问题

`EngineManager::codetable_baseline`（`manager.rs`）判定「特殊方案用内置基线而非全局
`schema.codetable`」用的也是 `s.schema.hidden`——同样是拿 hidden 当 overlay 的代理。
它不依赖已删除的数组，故本次改造没碰坏它。

**没顺手改对，是因为改了会动 `english` 的码表基线取值**（它是 hidden），而 english 走
的是 `EnglishGlobal` 还是这条路径需要单独确认，属于另一件事的风险。留作独立小项。
