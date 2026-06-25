# 设置系统改造计划（Schema 注册表 + CLI + Native 替代 webview）

## 背景与目标

把"设置"从"多份手写真相源、整块拉取、webview 重依赖"改造为：**一份声明式 Config Schema 注册表作单一真相源**，CLI / Native GUI / 校验 / 文档全部从它派生。

已决策（用户拍板）：
- Schema 形式：**独立声明式注册表**（手写集中声明，struct 反向对照校验）
- CLI 形态：**`wind_input config` 子命令**（不另造 wind_cli.exe）
- key 重命名（6 域规范化）：**延后**，先做地基；保持现有 key 路径，重命名留到 alias 迁移机制就绪后单独一批

关键事实（探查结论，避免返工）：
- 线协议 `config.setItems` **已是字段级**（`[{key, value}]` 点分路径），`Config::set_user_value` 只改单叶子原子写回。无需改协议。
- 真正缺口：① 单字段 get（`config.get` 只能返回整份）；② 描述所有字段的 schema；③ core 端对未知/越界 key 无校验（静默丢弃）。
- TOML 与 struct 已漂移 ~13 个字段（`config.rs` 无对应字段，写了不生效）。

---

## Stage 1: Schema 注册表地基 + 修复 TOML/struct 漂移
**Goal**: 在 wind-config 新增声明式字段注册表（key/类型/默认/域/合法值/needs_restart/label/desc），并修齐 TOML 与 struct 的 13 处漂移。
**Success Criteria**:
- `wind-config/src/schema.rs` 提供 `fn registry() -> &'static [ConfigField]`，覆盖全部现有 key。
- 编译期测试：遍历注册表，断言每个 key 在 `Config` 默认值里可解析、类型与声明一致（struct ↔ 注册表零漂移）。
- 13 个孤立字段：补齐 struct 字段 **或** 从 `data/config.toml` 删除（按"是否真生效"逐个裁决，删除项写明理由）。
**Tests**:
- `schema_registry_covers_all_keys`：注册表 key 集合 == Config 序列化后的叶子 key 集合。
- `schema_types_match_defaults`：每条声明的 ty/default 与 Config::default() 实际值一致。
- `no_orphan_toml_keys`：加载 data/config.toml，每个叶子 key 都能在注册表中找到。
**Status**: Complete
- 新增 `wind-config/src/config_schema.rs`：`FieldType`/`ConfigField`/`registry()`（127 字段声明）+ `config_leaf_keys()` 展开助手。
- 4 个 TDD 测试全绿：`leaf_keys_drills_tables_and_keeps_arrays_maps_as_leaves` / `registry_covers_every_config_key`（127）/ `data_config_toml_has_no_orphan_keys` / `registry_types_match_default_values`。
- 修复漂移：从 `data/config.toml` 删除 **33 个**孤立键（实测比预估的 13 多；全部经 grep 确认无 .rs 消费——含主题层同名键 always_show_pager/show_page_number/vertical_max_width/border_radius 属 theme.yaml，非 config）。
- 范围说明：Stage 1 仅声明 key+type（含 ~12 处 Enum 合法值）；domain/label/desc/needs_restart 留待消费方（Stage 3 CLI / Stage 4 GUI）按需补。
- wind-config 全套 29 测试绿，cargo fmt 已跑（格式与逻辑应分提交）。

## Stage 2: core 端校验 + RPC 补字段级读/描述
**Goal**: setItems 进来按 schema 校验；新增 `config.schema` 和 `config.getItem` 两个 RPC。
**Success Criteria**:
- `dispatch.rs:set_items` 对每个 item 查注册表：未知 key / enum 越界 / 类型不符 → 返回结构化错误（不再静默丢弃），合法项照常写入。
- `config.schema` 返回注册表 JSON（CLI 与 Native 共用）。
- `config.getItem(key)` 返回单字段当前值。
- `config.setItems` 行为不变（向后兼容现有 Native/webview 客户端）。
**Tests**:
- `set_items_rejects_unknown_key` / `set_items_rejects_enum_out_of_range`。
- `get_item_returns_single_value`。
- 回归：现有 Native 保存流程仍通过。
**Status**: Complete
- `wind-config/src/config_schema.rs` 新增 `validate(key, &toml::Value)` + `ValidateError`（UnknownKey/TypeMismatch/EnumOutOfRange），Float 字段宽松接受整数；5 个 TDD 校验单测。
- `wind-rpc/src/dispatch.rs`：`set_items` 改两遍式（先全量校验，全过才落盘——杜绝部分写入/静默失效）；新增 `config.schema`（`{fields:[{key,type,options?}]}`）与 `config.getItem`（`{key,value}`，含三层合并、未知键报错）。
- 5 个 dispatch 测试（3 reject + schema 列举 + getItem 已知/未知）；reject 走校验前置不写盘，已实测 `~/.config/WindInput` 无污染。
- `config.setItems` 公开行为对既有合法输入不变（向后兼容 Native/webview）；wind-config 36 + wind-rpc 15 测试绿，fmt 已跑。

## Stage 3: wind_input config CLI 子命令
**Goal**: 主程序新增 `config` 子命令，作 RPC 瘦客户端；core 不在运行时降级为直接读写 config.toml。
**Success Criteria**:
- `wind_input config get <key>` / `set <key> <value>` / `list [--domain D]` / `describe <key>` / `export` / `import <file>` 可用。
- `set` 走 `config.setItems` 单元素；`list`/`describe` 吃 `config.schema`；`import` 批量 setItems。
- core 离线时 `set/get` 直接走 `Config::set_user_value` / 读 config.toml（复用现有 API）。
**Tests**:
- CLI 集成测试：set 后 get 回读一致；set 非法 enum 报错；list 输出含全部域。
- core 离线路径冒烟测试。
**Status**: Complete
- `wind-rpc/src/client.rs`：最小同步 RPC 客户端 `client::call(suffix, method, params)`（连 ctrl 通道，连不上即 Err）。
- `wind-config/src/config_schema.rs`：新增 `leaf_entries()`（任意 TOML 表拍平为逐字段，供 import）。
- `apps/service/src/config_cli.rs`：`list [前缀]` / `describe` / `get` / `set` / `export` / `import`；list/describe/get/export 纯本地，set/import 优先 RPC 热重载、core 未运行离线直写；写入前按注册表 `validate`；`parse_value` 按类型解析 CLI 字符串。
- `main.rs`：`config` 子命令在服务启动前拦截（先于 init_logger，无日志噪音）。
- 端到端冒烟实测（新二进制）：list 前缀过滤 / describe 枚举可选值 / get / 三类 reject(exit 1) / 沙箱(XDG_CONFIG_HOME)set→get 往返均通过；真实用户配置零污染、无残留进程。
- 测试：wind-config 37 + wind-rpc 15 + wind_service 4（config_cli parse/format 单测）全绿，fmt 已跑。

**Goal**: 让当前主用的 webview 与 Stage 1-3 的严格 core 兼容，并防 cross-language 漂移。
**背景**: 实测前端 `config-keys.json`(137 键) vs registry(127) 漂移——**35 个前端键不在 registry**（=删除的 33 孤立键 + `hotkeys.enter_special_mode`/`features.quick_input.trigger_keys`），这些键本就从未生效（旧 core 也静默丢弃）。注：agent 误报「pinyin 15 键被拒」——已核实 pinyin 全在 registry，无事。
**已拍板**: ① setItems 改「应用合法项+报告跳过项」（不再原子拒绝整批）；② 前端加 CI 校验（key ⊆ registry∪允许名单）。
**Success Criteria / 已完成**:
- `dispatch.rs` set_items：未知/类型/枚举错的项**跳过并记入响应 `skipped`**（含 `applied` 计数），合法项照常写——webview 不再因一个旧字段整批保存失败；malformed item(无 key) 仍硬错误；落盘 IO 失败仍硬错误。响应加 `applied`/`skipped` 向后兼容（前端只读 needsRestart）。
- `config_cli.rs` apply_items：防御性呈现 skipped，全跳过(applied=0)视为失败 exit1。
- `wind-rpc/manifest.rs` schema_binding 新增 `frontend_config_keys_known_to_core`：前端 config-keys.json 每键须 ∈ registry 或 `FRONTEND_AHEAD_ALLOWLIST`(35 键,注释指向 deferred 文档)；新增前端键不在二者即红。
- 3 个 set_items 测试由 reject 改为 skip 断言（TDD RED→GREEN）。
**Status**: Complete（核心+CI 校验）；**剩余可选**：前端 toast 呈现 skipped(让用户知道改了已废弃字段)、GeneralPage 拼音保存只 console.error 无 toast(既有小 bug)、前端清理 35 死字段(用户选择保留,未做)。
**Tests**: wind-config 37 + wind-rpc 16 + wind_service 4 全绿。

## Stage 4-orig（暂停）: Native 补齐缺失页面

## Stage 5（进行中）: key 域重命名（**不做向后兼容**；webview 保留不降级）
**用户拍板（4 项）**：① 旧配置直接遗弃，不写迁移代码（开发期、仅作者自用）；② **ui 顶层名保留**（中文用户习惯/简洁），但 `ui.tooltip` 4 级**拍平**到 3 级；③ **keys 扁平**；④ **不强求 6 顶级**——立"正交大类"软准则，稳定域原地保留；`features` 是"杂物抽屉"反模式须拆解，模式三件套（quick_input/special_modes/mix_modes）**现在就归 schema.\***（预定位到未来"英文/快捷做成方案"的归宿，避免改两次）。

**最终顶级集合（约 9，正交大类）**：`general`/`schema`(方案+pinyin+模式)/`input`(+s2t/cmdbar,punct·symbol 分组)/`keys`/`ui`(tooltip 拍平)/`dict`(phrase)/`stats`(升顶级)/`compat`/`debug`。移除 `hotkeys`(→keys)/`pinyin`(→schema.pinyin)/`features`(拆解)。

**映射真相源**：`docs/config-key-migration.md`（127 键旧→新，全表）。webview **保留备用**（等 Native 完全对等后才去除，非本阶段）。

**子分期**（TDD，编译器引导；逐阶段 fmt 与逻辑分提交；main 不 push 不 `git add -A`）：
- **5.1** 重构 `config.rs` 结构体+Default（删 hotkeys/features/pinyin 顶级，新增 keys/dict/stats，schema 吸收 pinyin+模式，input 吸收 s2t/cmdbar+punct/symbol 组，ui.tooltip 拍平，shift_temp_english→temp_english、url_input→url、phrase→dict.phrase）。
- **5.2** 重写 `config_schema.rs` REGISTRY 127 键 + `internal_setter_paths` 守卫 + `data/config.toml`；三向绑定测试（struct↔registry↔toml）复绿。
- **5.3** 修下游 ~50–60 Rust 读取点 + 6 处 `set_user_*` 路径；workspace 全测绿。
- **5.4** `data/settings/manifest.toml` 全 key + manifest↔registry 三测复绿。
- **5.5** 前端 `config-keys.json` + ~25 文件键名替换；`frontend_config_keys_known_to_core` 收紧（清空 ALLOWLIST，旧键不兼容）。
- **5.6** 迁移表与实现核对定稿。
**Tests**: 全程靠既有三向绑定测试 + 编译器报错兜底；无 alias 迁移测试（不做向后兼容）。
**Status**: In Progress（5.1 开始）

---

## 三层真相源架构（含既有 manifest.toml）

配置元数据分三层，各管一摊，用测试**两两绑定**消除漂移（不合并成一份）：

| 层 | 位置 | 职责 | 范围 |
|---|---|---|---|
| 解析真相 | `wind-config/src/config.rs`（struct + serde） | 运行时 key→字段自动映射 | 全部 |
| 类型/校验真相 | `wind-config/src/config_schema.rs` `REGISTRY` | key+type+enum；core 校验 + CLI | 全部 127 |
| 展示真相 | `wind_input/data/settings/manifest.toml`（经 `system.manifest` RPC，`wind-rpc/src/manifest.rs`） | label/group/section/widget/options/min-max/enabled_when | UI 精选子集 |

绑定测试：
- registry ↔ struct：**Stage 1 已绑**（coverage/type/orphan）。
- manifest ↔ registry：**已绑**（`wind-rpc/src/manifest.rs` schema_binding 测试）——① 每个 manifest `item.key` ∈ registry；② 控件 type 与 registry 类型相容；③ select `options` 值集 ⊆ registry `Enum`。后续可据 manifest 的 options 反向把 registry 里保守标 `Str` 的键升级为 `Enum`（如 `input.enter_behavior`/`filter_mode`/`pinyin_separator`/`s2t.variant`）。

已修 bug：manifest `ui.status_tip.{offset_x,offset_y,schema_name_style}` → `ui.status_indicator.*`（原前缀错误，web 调状态气泡这几项被 serde 静默丢弃、不生效）。由 manifest↔registry 测试 RED→修→GREEN 驱动。

含义：
- label/desc 归 manifest（**不**进 config_schema.rs，确认 Q1 决策）。
- `config.schema`（Stage 2，类型/校验/全键，CLI 用）与 `system.manifest`（展示/精选，GUI 用）**并存互补**。
- Native（Stage 4）应消费 `system.manifest` 渲染页面/分组，不硬编码页面。

## 设计补充（来自第二轮讨论）

### 跨项目共享 config 定义：用运行时 schema RPC，不做编译期共享
- **进程内消费者**（core、`wind_input config` CLI 子命令）直接 link `wind-config`，引用 `registry()`/`field()`。
- 结论：`config.schema` RPC（Stage 2 新增）即跨项目契约；不让 native/webview 为拿 key 而硬依赖 core crate。

### key → Config 的映射机制：写"哑"、读"serde 自动"
- **写**（setItems）：把点分 key `split('.')` 后 `set_nested` 写进通用 `toml::Value` 树，**完全不认识结构体**——任意路径都能写进去。
- **读/重载**（Config::load）：三层 `toml::Value` 深合并后一次性 `merged.try_into::<Config>()`，由 **serde derive 自动按字段名映射**，无逐键 `match`。
- 后果：未知键被 serde **静默丢弃**（无 `deny_unknown_fields`）——这正是"写了不生效"的根因，Stage 2 用注册表在**写入时**校验拦截来根治。
- 含义：约定式映射，点分 key 必须与 serde 字段嵌套一致；注册表测试已锁死 key↔字段一致。

### 实例集合 schema 与 mode 统一（临拼/临英/快捷 → 统一 mix-mode 体系）
方向：临时拼音、临时英文、快捷输入等后续统一为 **mix_mode 式实例集合**，以 id 区分。对 schema 的影响：
- **结构体**：实例集合应是 **id 键控的 map**（`HashMap<String, ModeConfig>`，序列化为 `[features.modes.<id>]`），而非 `Vec`——因为 toml 数组是**按位置**寻址，CLI/setItems 无法用 `features.modes.quick_mix.trigger_keys` 按 id 定位；map 才能 id 寻址。当前 `features.mix_modes`/`special_modes` 是 Vec，统一时需迁移为 map。
- **注册表**：需新增"**元素 schema**"概念——集合整体声明一次元素字段集（id/name/short_name/trigger_keys/enabled + 各 kind 专属字段），校验 `<集合>.<id>.<字段>` 时跳过 id 段、按元素 schema 校验；异类 kind 用 `kind` 判别字段做受限联合（discriminated union）。
- **通用设置的归处**：跨实例共享的默认（如统一的触发/上屏策略）沿用代码里已有的「方案级 Some > 全局 > 内置默认」模式（参考 `input.code_commit`）——全局默认块作普通 flat 键登记注册表，实例仅存覆盖项。
- 现状：Stage 1 注册表把 `features.mix_modes`/`special_modes` 当**不透明 StructList 叶子**（够 Stage 1/2 用）。元素 schema 在真正做 mode 统一、且 CLI 要按 id 改实例字段时再引入（Stage 3+ 或专项）。

## 数据三分准则（config / state / data）—— Stage 1 设计前提

配置系统必须先把三类数据分清，各有归宿，避免"配置与状态混在一起、第二三套用户配置成管理盲区"：

| 类别 | 定义 | 例子 | 归宿 |
|---|---|---|---|
| **config（用户配置）** | 人主动改、希望可移植/同步 | config.toml、compat.toml、schema override、theme.yaml | 进 Schema 注册表，CLI/Native/校验统一管；放漫游目录 |
| **state（运行状态）** | 程序自己写、机器相关、不该同步 | 主题选择(theme.txt)、工具栏位置(toolbar_pos.txt)、输入模式记忆 | 收拢成**单个 `state.toml`**（放 `%LOCALAPPDATA%`），取代散落裸 txt；不进 Schema 注册表 |
| **data（分发数据）** | 只读、随程序发货 | system.phrases、双拼布局、码表、manifest、common_chars | 只读，不进配置系统 |

落地要点：
- Schema 注册表只描述 **config** 类；CLI/Native 的写入面 = config 类全集（含 compat.toml、schema_overrides，不止 config.toml 一隅）。
- 新建 `state.toml` 收拢 `theme.txt` + `toolbar_pos.txt` + 模式记忆；旧裸 txt 提供一次性迁移读取后弃用。
- `ui.theme.name`（config）与 theme 选择（state）的重复真相源要消歧：config 存"用户偏好默认主题"，state 存"当前实际选中"。

## 命名空间隔离（与 Go 旧项目不冲突）—— 已就绪，沿用 debug 变体

决策：**保留 `WindInput` 基础 token**（Go 仅 alpha，Rust 最终全面顶替）；过渡期用既有 `debug_variant` 作内测渠道；**仅隔离不迁移**。

经核实，debug 变体已在**所有命名空间轴**与 Go release 完全隔离，无需新增工作：

| 轴 | release（最终顶替 Go） | debug 变体（内测渠道） | 证据 |
|---|---|---|---|
| TSF CLSID / Profile / 显示属性 GUID | `99C2EE3x` 系 | `99C2DEB x` 系 | `wind_tsf/src/Globals.cpp:18-60`（`#ifdef WIND_DEBUG_VARIANT`）|
| 核心 exe | `wind_input.exe` | `wind_input_debug.exe` | `wind_tsf/src/IPCClient.cpp:398-401` |
| 管道 | `wind_input{_ctrl,_events}` | `wind_input_debug{_ctrl,_events}` | `service/main.rs:18-22` `PIPE_SUFFIX` |
| 互斥量 | `Global\WindInputIMEService` | `Global\WindInput_debugIMEService` | `service/main.rs:271` |
| 配置/状态目录 | `%APPDATA%\WindInput\`、`%LOCALAPPDATA%\WindInput\` | `…\WindInputDebug\` | `wind-config/config.rs:1282` |
| DLL / 输出目录 | `wind_tsf.dll` / `build/` | `wind_tsf_debug.dll` / `build_debug/` | `wind_tsf/CMakeLists.txt:8`、`Makefile:45-48` |

要点：
- 本计划的 config/state **格式改造均在内测期进行，只落到 `WindInputDebug` 域；Go 的 `WindInput` 数据零风险**。
- 不做旧配置迁移导入（仅隔离不迁移）。
- cutover 细节（release 顶替时）：加载器读到 Go 遗留旧格式 config.toml 时靠 serde 默认值兜底不崩、旧字段忽略；安装器可选清空旧文件。无需为迁移写代码。

## 分布准则（Stage 5 落地，先在此固化为规范）
1. 顶层域 = 设置页签 = 用户心智，固定 6 个：`schema` / `input` / `keys` / `appearance` / `dict` / `advanced`。
2. 路径最多三级 `<域>.<组>.<字段>`（现有四级如 `features.quick_input.alpha_providers.pinyin` 拍平）。
3. 命名：全 snake_case；布尔正向（`enabled`/`show_*`，禁 `disable_*`）；枚举合法值集中在 schema；"跟随主题"的样式字段从 TOML 删除而非保留。
4. 顶级域 = 设置页签 = 用户心智锚点；不卡死数量，立"正交大类"软准则（语义自洽/规模相当/能对应一页），排除"削足适履强并"与"碎片化堆放"两极。`features` 式杂物抽屉禁止。
5. 本批**不做向后兼容**（开发期自用，旧配置遗弃）；改名映射记于 `docs/config-key-migration.md`，仅作文档与 codemod 依据，不写运行时 alias。
