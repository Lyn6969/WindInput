# 配置设计规则指引

状态：规则汇编（2026-08）。§R5（受众分级）为已定方向、未实施；其余为对现状规则的收拢与固化。
本文收拢此前散在 `wind-config/AGENTS.md`、`docs/config-key-migration.md`、
`docs/architecture/user-override.md`、`docs/redesign/schema-config-layering.md`、
`docs/archive/SETTINGS_REVAMP_PLAN.md` 等处的配置规则，作为后续功能迭代加配置项时的单一入口。
细节与本文冲突时，以被引用的专项文档为准并回改本文。

总纲（四句话）：

- **灵活性靠数据层不设限**：加键的准入门槛是低的，REGISTRY + 守门测试保证数据层不失控。
- **复杂度靠分级请出主视野**：键的增长是常态，呈现层用受众分级（§R5）消化，不靠拒绝加键。
- **易用性靠 common 精选**：主界面只呈现少数高频键，其余折叠/对话框/片段化。
- **文档靠三层结构 + 脚本守门**：入门 / 设置页对照 / 全量参考各司其职，覆盖率由脚本保证。

## R1 准入：什么值得成为配置键

一个行为差异要成为配置键，须满足其一：

1. 不同用户群体有真实相反的偏好（不是「可能有人想要」）；
2. 宿主/环境差异导致无法有单一正确值（compat 类）；
3. 取舍代价对称，替用户决定任一侧都会伤另一侧。

反例判据：若某一侧取值明显更优、另一侧只是兜底 → 不加键，直接取优值；
若差异可以由程序判定（宿主探测、能力位）→ 走自动判定 + compat 覆盖，不加用户键。
「拧了没反应的旋钮」（功能默认关且资源不随包分发之类）宁可不进 GUI 也不要进主视野
（登记 `UNCOVERED_BY_DESIGN` 并写明理由）。

## R2 落点：键放哪里

判据一句话：**实例身份从哪来，配置就落到哪**（详见 overlay-mode 配置下沉的结论）。

| 落点 | 适用 | 覆盖模型（user-override.md） |
|---|---|---|
| config.toml（全局） | 与具体方案/实例无关的用户偏好 | B：键级合并（仅 config.toml 与 compat.toml，**不得扩大**） |
| `*.schema.toml` + `schema_overrides/{id}.toml` | 身份来自方案实例的配置 | A：整文件替换 + 深合并折叠 |
| 特殊模式方案文件 `[overlay]` | 身份来自 overlay 模式实例 | A |
| redb（user_data.db） | 数据而非配置（词、词频、短语、状态） | C：无文件覆盖 |

数据三分准则（SETTINGS_REVAMP_PLAN 的核心遗产）：**配置（用户意志）/ 数据（使用积累）/
状态（运行时镜像）三者不混放**。状态镜像变更必须回灌运行时（否则「设置了不生效、重启后生效」）。
GUI 一切非 config.toml 落点的写回统一登记 `SideCommitter`，禁止独立保存按钮。

## R3 命名与形态

- 点分路径 snake_case，**≤3 级**（`domain.feature.key`）。
- 布尔一律正向命名（仓内现状 0 个 `disable_*`，保持）；功能总开关用裸 `enabled`
  （`input.xxx.enabled`），不用 `enable_xxx` 变体。
- 「开关 + 子配置」是仓内主导形态：`enabled` + 同 table 下的细粒度键。
- **枚举当开关**的判据（`input.association.kind` 先例）：当「开着但没配内容」是一个
  无意义状态时，用枚举（`off | modeA | modeB`）合并开关与模式，消灭歧义态；
  否则保持 bool + 子键。
- 三态表达只允许两式，且优先前者：
  1. `Option<T>` 且**刻意不进 REGISTRY**（如注释模板）——「未设置=跟随内置」；
  2. 空字符串哨兵（`"" = 默认`）——仅当该键必须进 REGISTRY/GUI 时。
  不得发明第三种三态写法。
- 顺序带语义的 StrList 注意：GUI `checkbox_group` 恒按声明顺序写回，用户手排的顺序会被
  静默改写——要么把声明顺序钉成语义（hint 说明），要么上列表编辑器，不得两不管。

## R4 默认值与变更纪律

- L1（`Config::default()`）与 L2（`data/config.toml`）必须一致，除非 L2 刻意覆盖
  （此类键登记于 `ABSENT_FROM_DATA_CONFIG` 之外的注释说明）。L2 同时是出厂说明书：
  全量列出、注释齐备。
- **改一个出厂默认值的落点清单**（实测曾不完整，改前先全仓 grep 键名、排除 build/target）：
  1. L1 结构体 default（部分键在引擎侧还有第三份 default，逐键确认）；
  2. L2 `data/config.toml`；
  3. 文档站四处（guides/config 代码块 + 表格、settings 用法页出厂列 + 正文）；
  4. **Android assets 的手工同步副本**（有意的平台差异，是否跟随是产品决策，要问）。
- prune 语义（`set_user_value` 等默认即删）意味着：默认值变更会静默改写「曾主动设成
  当时默认值」的用户——发布默认值变更必须写进 changelog。
- 默认值本身要有三层守门测试（取值 / L1-L2 同源 / 端到端行为），否则「只翻默认值」
  一条测试都不红。
- **改已发布键的类型**必须在 `Config::load` 加 Value 层迁移（`migrate_*_value` 族，
  反序列化之前），否则用户全盘配置静默回落出厂值。详见 `docs/config-key-migration.md`。

## R5 受众分级（level）——已定方向，未实施

问题：REGISTRY 记录了「类型」但从不记录「受众」，244 键（2026-08 统计）呈现上人人平等，
设置页被迫在「54 行平铺」与「五层对话框」之间二选一。

方案：`ConfigField` 增加 `level` 元数据，单一维度同时驱动 UI 折叠、文档分层、hint 详略：

| 档 | 判据 | 呈现 |
|---|---|---|
| `common` | 不看文档、使用第一周就可能想改的（目标 40~60 键） | 设置页卡片内联控件 |
| `advanced` | 需要理解功能机制才会调 | 卡片「高级…」按钮 → 段级对话框（渲染由 level 自动推导） |
| `expert` | 面向文件/片段的键（Map、StructList、调参类）；`UNCOVERED_BY_DESIGN` 的形式化归宿 | 不做专属控件；出口 = 文档片段模板 + 剪贴板导入（见 config-import-unification.md §3） |

- level 落在主仓 REGISTRY，经既有 capabilities 通道流向设置仓（快照重生成即同步）。
- 不做预设 profile（「新手/高级模式」整体切换）——会重新引入第三层裁决的雷区。
- 方案对话框内部不做二级分级（对话框已是 advanced 语境）。

## R6 设置 UI 规则（wind-setting）

- 折叠机制收敛为两种，其余视为语法糖或冻结项：
  1. **宿主行 + 附属对话框**：宿主控件任意（toggle / select / number），可选置灰条件
     （复用 `enabled_when` 语法）。现有 `gate_key` / `gate_soft` / `opens_dialog` 都是
     它的特例；支持「枚举某取值 → 子配置」（如 position_mode==custom → 坐标）。
  2. **整段对话框**：段内 ≥6 个从属项时整段收进对话框。
  - `dialog_extra_for` 冻结：不再新增使用（其宿主必须 gate_soft 的隐式依赖是错误形态信号）。
- 对话框深度 ≤2（页面 → 对话框 → 子对话框到此为止）。
- 搜索（待建）必须深链：命中折叠项时自动打开对话框链并高亮该行。
- hint 纪律：一到两句，只答「是什么 + 怎么选」；`enabled_when` 已表达的前置条件不重复；
  设计理由写代码注释 / commit message / 文档站。括号补语仅当承载取舍代价（「较慢」「可能抖动」）。
- 一个配置键只能有一个 manifest 项；无 subsection 的通用项必须声明在该 section 所有
  subsection 之前（渲染器不终止分组）。
- **新增配置项的五道闸门**（两仓守门测试会依次拦，照报错提示修）：
  1. 主仓 `registry_covers_every_config_key` → REGISTRY 补行；
  2. 主仓 `data_config_toml_covers_registry` → L2 补键；
  3. 设置仓 capabilities 快照重生成（前提：主仓改动已在设置仓 path 依赖指向的那份工作区）；
  4. 设置仓 mock config 重生成；
  5. 设置仓 `uncovered_capability_keys_match_allowlist` → 接进 manifest 或登记
     `UNCOVERED_BY_DESIGN` 并写明理由（判据：写得出理由才登记，写不出就留红）。

## R7 文档规则（WindInputDocs）

- 三层结构，各司其职，不互相替代：
  1. start（入门）：面向 common 档，任务导向；
  2. settings（设置页对照）：与设置页同构，讲「这一页每项是什么」；
  3. guides/config（全量参考）：按 TOML 段组织，固定五列表（键/类型/可选值/默认/说明），
     覆盖含 expert 档在内的全部键。
- **REGISTRY → 文档覆盖校验脚本待补**（现状 97% 覆盖、7 键缺口的根因就是没有脚本守门）。
  脚本以 REGISTRY 为基准比对 guides/config 各表，缺键即 CI 红。
- cookbook 层（待建）：任务导向的片段合集，每篇 = 说明 + 可复制 TOML 片段
  （+ 官方包的一键导入按钮）。片段格式与导入通路见 `docs/architecture/package-format.md`。
- 一个键在文档站有多个落点（参考页代码块 + 表格、用法页），改键必须同步全部落点（见 R4 清单）。

## 附：新增一个配置项的完整 checklist

1. 过 R1 准入判据；定 R2 落点（非 config.toml 落点则本清单只有文档项适用）。
2. 按 R3 命名定形态；定 R5 level（实施后）。
3. 主仓：Config 结构体 + default、REGISTRY、L2 `data/config.toml`。
4. 消费点接线并确认可达（「配置四层就位、消费点在不可达调用点上」= 开关毫无反应）。
5. 运行时镜像回灌（若该键影响运行时状态）。
6. 设置仓：五道闸门（R6）；manifest 项 + hint（按纪律）。
7. 文档站：guides/config 参考页 + settings 用法页；如有 cookbook 关联片段一并更新。
8. 若替代旧键：`RETIRED_KEYS` 登记 + 迁移（R4）。
