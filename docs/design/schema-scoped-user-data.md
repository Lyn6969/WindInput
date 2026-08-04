# 方案级用户数据：归属、缺口与分期

**状态**：设计草案，待拍板。触发自「英文方案要有用户词库与词频」「特殊方案也要支持候选调整、用户词库」两条需求。

> **本文档 v1 有一处结论性错误，已订正。** v1 断言「特殊模式在把用户的选择记进主方案的词频表」，
> 被用户实测推翻（打快符并未污染五笔词频）。复查后确认：`commit_special_candidate`
> 只调 `record_commit`（统计），**不调 `record_selection`**（词频）——特殊模式的词频读写
> **两端都没有**。v1 把 `commit_and_enter_special_mode` 里那个 `record_selection` 当成了模式内
> 的上屏，实际它是「进入特殊模式**之前**把已有候选上屏」，记的是主方案自己的候选，归属正确。
>
> 教训记在这里：**读一处调用点不足以断言运行时行为**，尤其当函数名（`commit_and_enter_*`）
> 同时描述了两个动作时——它属于「进入」那一侧，不属于「模式内」。

---

## 一、现状：特殊模式在用户数据体系里是一片空白

不是归属错位，是**根本没接**：

| 通路 | 特殊模式的现状 |
| --- | --- |
| 词频 | 读端 `update_special_candidates` 不调 `apply_freq_rerank`；写端 `commit_special_candidate` 不调 `record_selection`。**两端皆无** |
| 候选调整 | 读端不调 `apply_shadow`；写端按 active 归属（用户在特殊模式下够不到该入口） |
| 用户词库 | 引擎层**已挂** `StoreUserLayer`（按方案自身 id），即「读」是通的；写端（加词）按 active，写不到自己名下 |

⇒ 起点是干净的：**加功能是纯新增，没有存量污染要迁移**。

### 已经正确的部分（不要动）

`write_data_schema_id` 对**混输 active** 按候选来源分流，这条已在正确工作：

| active | 候选来源 | 归属 | 实测 |
| --- | --- | --- | --- |
| `wubi86_pinyin`（混输） | `Pinyin`（含临时拼音） | `"pinyin"` | ✅ 用户实测确认，全拼/双拼共享 |
| `wubi86_pinyin`（混输） | `CodeTable` | 主码表方案 `wubi86` | ✅ |

`data_schema_id` 把拼音族折叠成 `"pinyin"`，是**刻意设计**（全拼/双拼共享一份用户词与词频），
不是缺陷。

### 仍需引入的概念：生效方案（effective schema）

> 这一次按键的候选，是哪个方案的引擎出的。

普通输入下等于 active；特殊模式期间等于该模式引用的方案。要让特殊方案有自己的用户数据，
写端必须按它归属，而不是按 active。

`write_data_schema_id` 的按来源分流是这个概念在**混输**场景下的特例；特殊模式需要的是按
**模式状态**（`ModeKind`）分流，两者互补而非替代。

---

## 二、三条通路的现状

### 词频

| | 读（`apply_freq_rerank`） | 写（`record_selection`） |
| --- | --- | --- |
| 普通码表 / 拼音 | ✅ | ✅ |
| 英文方案（active） | ✅ | ✅ |
| **特殊模式** | ❌ **不调** | ❌ **不调**（`commit_special_candidate` 只 `record_commit` 统计） |

### 候选调整（shadow）

`apply_shadow` 按 `data_schema_id(active_schema_id())` 取规则，`update_special_candidates`
不调。⇒ 特殊模式读端没有。

### 用户词库

引擎层各自挂 `StoreUserLayer` / `StoreTempLayer`：

| 引擎分支 | 用户词层 | 临时词层 | 归属 id |
| --- | --- | --- | --- |
| 码表（含特殊方案，`type = "codetable"`） | ✅ | ✅ | 方案自身 id |
| 拼音 | ✅ | ✅ | 折叠为 `"pinyin"` |
| **英文** | ❌ | ❌ | — |

★ **特殊方案的用户词库「读」已经是通的**（codetable 类型，走 else 分支挂了层，且归属就是
方案自身 id）。缺的只是写端。

英文两端都没有：`build_engine` 的 english 分支在挂层之前就 `return` 了，注释写的是
「英文暂不挂用户词 / 临时词层（无造词学习），仅系统词库层」。

---

## 三、已定方向（用户 2026-08-04 拍板）

- **特殊方案的用户数据每方案独立**——「和五笔一个层级的东西，只是使用特殊按键进入」。
  归属用方案自身 id，引擎层已经是这样，写端对齐即可。
- **特殊方案默认不继承全局码表配置**（`schema.codetable`）。见 P0。

## 四、分期

### P0：特殊方案不再继承全局码表配置

当前 `resolve_codetable(schema_id, .., global, ..)` 以全局 `schema.codetable` 为基线、
方案 `[engine.codetable]` 逐字段覆盖。于是快符方案没写 `single_code_input` 就自动继承了
五笔的设置——用户改五笔的精确匹配，快符跟着变，而两者的码表性质完全不同。

改为：**hidden 方案取内置默认值作基线**，只认自己方案文件里写的字段。

判据用 `[schema].hidden` 而非「被 special_modes 引用」：后者在 `build_engine` 里拿不到
（那层没有 config.schema.special_modes），且 hidden 正是设置页据以分区的同一个标志，
两处判据一致比各自发明更不容易漂。英文方案虽也 hidden，但走 english 分支、不读码表配置，
不受影响。

### P1：写端按生效方案归属

抽 `Coordinator::effective_data_schema(&state)`，按 `state.active`（`ModeKind`）分流。
`commit_special_candidate` 补 `record_selection`（用生效方案）、加词流程对齐。

**无存量迁移问题**——特殊模式此前根本没写过词频。

判据同 [[freq-rerank-model]] §记账码：**读、写、调试三处必须同口径**，一并检查
`debug_freq_count`。

### P2：英文的用户词库

- `build_engine` 的 english 分支挂 `StoreUserLayer`（归属 `"english"`）；
- **不挂** `StoreTempLayer`：临时词库是「自动造词的暂存区」，英文没有造词流程，挂了永远是空表
  （用户已确认临时词库不需要）；
- 加词入口：英文方案下 `Ctrl+=` 把当前输入写进用户词库（专有名词、缩写、项目内部词汇）。
- 词频已可用（`schema.english.frequency.*` 已落地）。

### P3：特殊方案的词频与候选调整读端

`update_special_candidates` 补 `apply_freq_rerank` + `apply_shadow`（用 P1 的生效方案）。
⚠️ 特殊方案多是小符号表、顺序常是作者精心排的，调频默认应**关**。

### P4：每方案调频配置（落点已定：方案文件）

**落点**：方案文件的 `[engine.codetable.frequency]`（用户 2026-08-04 拍板）。理由是它已经是
「这个方案怎么工作」的落点，而 `special_modes` 条目描述的是「怎么进入」，两件事不该混。

⚠️ **这不是「把配置项挪个位置」**。「普通码表方案其实也支持、只是默认用了全局」这个印象
对**上屏行为**成立，对**调频不成立**——两者的读取路径根本不同：

| | 方案文件可写？ | 读取路径 | 按方案折叠？ |
| --- | --- | --- | --- |
| 上屏行为（9 项） | ✅ `CodeTableSpec` 的 `Option` 字段 | `resolve_codetable` | ✅ |
| **调频** | ❌ `CodeTableSpec` **没有 frequency 字段** | `freq_settings()` 直读全局镜像 `self.codetable.lock()` | ❌ |

即使今天在方案文件里写 `[engine.codetable.frequency]`，也没有任何人读它。故 P4 是三步：

1. `CodeTableSpec` 加稀疏 `frequency`（每字段 `Option`，照现有 9 项的样子）；
2. `CodetableGlobal::resolved()` 折叠该段；
3. **`freq_settings()` 改为按方案解析**——这一步是真正的风险点：
   - 它有 `freq_cache`（`HashMap<schema_id, FreqSettings>`），键已经是 schema_id，
     结构上是支持的，但失效时机要跟着方案 override 走；
   - 判据同 [[freq-rerank-model]] §记账码：**读、写、调试三处同口径**，
     `apply_freq_rerank` / `record_selection` / `debug_freq_count` 必须一起改；
   - `active_freq_profile()`（half_life 来源）同理。

顺带的收益：普通码表方案也就此获得**每方案独立调频**的能力（用户预期中本已存在的能力）。

---

## 五、已定与待办

**已定**（2026-08-04）：

- P4 落点＝方案文件 `[engine.codetable.frequency]`。
- **临拼 / 临英 / 快捷输入的归属不动**——它们走 `write_data_schema_id` 的按来源分流，
  实测行为正确（临拼记进 `"pinyin"`、全拼双拼共享），改动风险大于收益。

**待办**：

- P0 是行为变更：已手写过 special_modes 且**依赖继承**的用户，其快符行为会变（回到内置默认）。
  考虑到 `special_modes` 出厂为空、实际用户极少，倾向直接改不做兼容——**待真机确认**。
- 实施顺序建议 P1' → P2 → P3 → P4：前三步各自独立可验证，P4 触及 `freq_settings` 这条
  全局路径，放最后单独做、单独验。

---

相关：[[freq-rerank-model]]（记账码口径与三处同源约束）、`docs/design/special-mode-codetable.md`
（特殊模式的引擎与配置结构）。
