# 重设计差分：dict（词典系统）

> 阶段 A 产物。Go 侧由 3 个只读 agent 提取、关键论断 grep 抽验 file:line 属实；Rust 侧本人通读。
> 体量：Go `internal/dict` ≈ 1.08 万行 + `pkg/dictio` ≈ 1.8k；Rust `wind-dict` ≈ 1.3k。
> 行数差被"职责重分布"放大：Rust 把 phrase/用户词/词频/shadow/stats 放进了 **wind-store(redb)**。

---

## 1. 核心架构分歧（最重要）

| | Go | Rust 现状 |
|---|---|---|
| 查询面 | **活的多层 CompositeDict**（DictManager 编排）| 引擎持**扁平 mmap `CachedDict`**（combined.wdb）|
| 用户/临时/Shadow | 作为 store 层挂入 composite，查询时合并 | 在 wind-store，**词频 boost 由 coordinator 层另外应用** |
| 词频 | 查询时即应用（composite.searchInternal Step2，FreqBoost）| 引擎产基础权重，上层 boost |
| Rust 分层脚手架 | — | `CompositeDict/DictLayer/LayerType` **存在但完全未接线**（引擎不用）；`store_layer.rs`(3行空)、`manager.rs` DictManager(30行桩) |

**Go 查询语义**（composite.go searchInternal，已核实）：遍历层（LayerType 升序优先）→ 按 Text 去重（继承更高 weight；前缀模式保留更短 code）→ 叠加 `perLayerNOOffset=10_000_000`×层序到 NaturalOrder → **应用 freqScorer.FreqBoost** → 排序 → 截断。Shadow **不在** searchInternal 内，由引擎排序后 Phase 6 应用（composite.go:211 注释）。

### 决策：接通"活的分层 CompositeDict"作为引擎唯一查询面
理由（对齐"统一"原则 + 质量）：
1. 把 用户词/临时词/词频/shadow/phrase 全部**收拢到 dict 层一处合并**，引擎与 coordinator 不再各自重复做合并/boost——直接消除"模式特例分支"，呼应用户强调的"统一按键处理/模式融合"。
2. Rust 已有干净的 `DictLayer`/`LayerType` trait 设计，缺的是把 store 层和 DictManager 接上。
3. **保留** Rust 的 mmap combined.wdb 作为 **System 层后端**（不可变层的预合并是合理优化，契合内存目标）。

> **跨文档修正（再修正）**：词频以 [frequency.md](./frequency.md) 为准——**词频与权重彻底解耦，不在查询时改 weight**。CompositeDict 只负责**合并各层候选**（system+用户词+temp+phrase，带 weight）；词频作为**排序阶段的独立维度**由 engine 排序层应用（码表 used-first 可选模式 / 拼音衰减分）。引擎的 `dict` 字段从 `CachedDict` 改为 composite 查询接口。

---

## 2. 目标层模型与查询语义

层（数值越小优先级越高）：
| 层 | 后端 | 可变 | 说明 |
|---|---|---|---|
| Logic（命令/短语）| PhraseLayer + cmdbar hook | — | 日期/UUID/$CC 命令、自定义短语 |
| User（用户造词）| wind-store(redb) | ✓ | StoreUserLayer，权重上限 `MaxDynamicWeight=10000` |
| Temp（临时学习）| wind-store | ✓ | 学习/晋升(promote)/淘汰(evict) |
| Cell（细胞词库）| mmap | — | 可选附加库 |
| System（系统主词库）| mmap combined.wdb | — | 现 CachedDict 升级为本层 |

- **Shadow 不是层**：它是 `ShadowProvider`，引擎排序后应用（pin/delete）。
  → **修 Rust 现 bug**：`layer.rs` 把 `Shadow=1` 当成 LayerType（与 Go 同款死枚举错误），重设计删除它，Shadow 独立为 provider。
- 查询语义照搬 Go composite（去重保更高 weight/更短 code + perLayerNOOffset + 排序截断），但用栈上 `HashMap` 去重，不引入 sync.Pool。**不做查询时 FreqBoost**（词频改为排序阶段独立维度，见 [frequency.md](./frequency.md)）。
- 前缀按前缀长度限流（Go defaultPrefixSafeLimit 200/800/500/300，魔法值，Rust 按实际词库规模重定）。
- `SearchSystemOnly`（仅 System/Cell，不调频）供 ProtectTopN。

---

## 3. store 层与词频（user / temp / shadow / freq）

Go（store_layer.go，已核实）：
- `StoreUserLayer`（Type=User）：增删改 + `IncreaseWeight`（≤MaxDynamicWeight）+ `OnWordSelected`。
- `StoreTempLayer`（Type=Temp）：`LearnWord`（学习→按需 evict→Count≥promoteCount 返回晋升信号）、`IncrementIfExists`、`PromoteWord`（晋升到用户库）、`SetLimits` 热更新。
- `StoreShadowLayer`：实现 `ShadowProvider`，pin/delete/removeRule，空规则快返回 nil。
- 词频：`FreqHandler.Record`→`store.IncrementFreqAsync`（**异步批量写**，减 redb 写锁竞争）；`StoreFreqScorer.FreqBoost`(code,text)→`CalcFreqBoostWithProfile`，查询时加到 weight。

Rust 现状：`store_layer.rs` 空壳；用户词/词频/temp/shadow 散落 wind-store，未经 composite 统一。
**目标边界**：实现 `store_layer.rs`，把 wind-store 的 **user/temp 词包装成 `DictLayer`、shadow 包装成 `ShadowProvider` 挂进 composite**。**词频不挂 composite**——它是 engine 排序层的独立维度（[frequency.md](./frequency.md)），engine 只需 store 的 freq 只读访问（`freq_lookup`）。异步词频写保留（对齐 Go，契合 redb）。**此项牵动 wind-store**，需在 store 差分同步定义接口（GetUserWords/LearnTempWord/freq_lookup/Shadow 记录等）。

---

## 4. phrase / 命令层

Go（phrase.go，已核实）：`PhraseLayer`（Type=Logic）分 `staticPhrases`（无变量，支持前缀）与 `dynamicPhrases`（含 `$` 变量，仅精确）；模板变量（YYYY/MM/DD/uuid/ts… cmdbar_filter.go:42）；`$CC(...)` 命令直通车经 `cmdbarHook`（coordinator 注入）连 wind-cmdbar；`$AA`/`$SS` 字符组/数组组；TOML/YAML + store 三来源；候选带 `IsPhrase/IsGroup/IsCommand` 标记，默认 weight=1000。

Rust 现状：**wind-dict 无 phrase**；wind-cmdbar 是骨架（见 engine 差分外的 cmdbar）。
**目标边界**：phrase 作为 Logic 层落在 wind-dict（或 wind-cmdbar，边界待定）；命令求值走 cmdbar hook。**与 cmdbar 子系统强耦合，建议 cmdbar 差分时合并定边界**；engine/dict 阶段先留 Logic 层接口位。
**不照搬**：手写 double-checked locking 缓存 + 每次按键全量失效 `cmdCacheKey`（与按 code 缓存自相矛盾）；字符串扫描散落的类型判定（用枚举 + 统一 parse）。

---

## 5. 二进制格式（.wdb / .wdat / hotcache / topk / registry）

Rust 现状：`binformat.rs`（.wdb，V2 10B/V3 14B 含 Order，mmap 二分 + 前缀扫描，有往返测试）扎实；`unigram.rs`（WUNI mmap）扎实；`datformat.rs` **仅 open 桩**；`hotcache.rs`/`trie.rs` **存在但未接线**；前缀查找是"全收集+排序+截断"（无 top-K 堆）。

Go 对应：.wdb header 32B 含 `AbbrevOff/MetaOff`；**abbrev 简拼段**（AbbrevHeader 16B + AbbrevIndex 12B）；topk.go min-heap top-K 截断；hotcache 单字母前缀（z 子树 ~47k）；registry.go（Windows mmap 文件锁→原子替换前强制关 reader）；拼音用 **.wdat**(datformat 双数组 trie)。

**目标边界（决策）**：
1. **统一为单一 .wdb，丢弃 .wdat**（Go 自己也认两套格式是坏设计）：Rust 删 datformat 桩，拼音词典也走 binformat。
2. **实现 abbrev 简拼段**：写 pinyin .wdb 时构建简拼索引、`search_abbrev` 读取——支撑 engine.md 的简拼流水线（比运行时扫描快）。Rust `DictFileHeader` 已留 `abbrev_off` 字段（现写 0）。
3. **前缀查找用 top-K min-heap**（对齐 Go topk），替换全收集+排序——单字母前缀性能关键。
4. **接通 hotcache**：单字母前缀（limit≤500）走 hotcache，避免 z 子树每次全扫。
5. **Windows 原子替换**：实现 reader registry 等价物（原子替换 .wdb 前释放 mmap），否则缓存重建在 Windows 报 Access Denied。**Windows 专属，缓存失效/重建实现时一并处理**。
**不照搬**：abbrev 词条混存主 EntryRecords 区致 offset 语义不统一；AbbrevHeader 存绝对偏移 + 8B Reserved 浪费；IsCommon 不存盘每次重算（Rust 存 IsCommon 标记位）；sync.Pool 复用 picker/map（Rust 栈上复用即可）。

---

## 6. 工具与杂项（阶段归属）

| Go | 行 | Rust | 归属 |
|---|---|---|---|
| pkg/dictio 导入导出（rime/winddict/zip/tsv/phrase）| ~1.8k | 无 | **阶段 D（设置程序）**，低优先 |
| dictcache/convert 格式转换 | 747 | 散落 codetable.rs+engine manager | 已基本覆盖；**增量 patch（dict_patch 298）缺**，记为缺口 |
| english_dict | 427 | 无 | 随 engine.md §3/§5 英文一起做 |
| common_chars（IsStringCommon 常用字表）| 157 | 无 | 码表 BFS common-first 需要，随码表交互做 |
| weight_norm（NormalizeWeight）| 135 | engine scorer | 已在 engine.md |
| template/value_expand/markers（$AA/$SS/$CC/日期变量）| ~640 | 无 | 随 phrase/cmdbar 做 |

---

## 7. Go 坏设计（不照搬）汇总
1. `Shadow` 作为 LayerType 枚举值（实为 Provider，死枚举）——**Rust layer.rs 现有同款 bug，删除**。
2. 两套并行二进制格式 .wdb/.wdat → 统一 .wdb。
3. abbrev 词条混存 + offset 含主区大小（语义不统一）；AbbrevHeader 绝对偏移 + Reserved 浪费。
4. IsCommon 每次查询重算 → 存盘。
5. sync.Pool 复用去重 map / topk picker（Go GC 妥协）→ Rust 栈上复用。
6. composite seenIdxPool 双重间接 → 普通 HashMap。
7. phrase：手写 DCL 缓存 + 每键全量失效 + 字符串扫描散落类型判定。
8. 三套 ID（schemaID/dataSchemaID/freqSchemaID）无类型封装易传错 → Rust 用 `SchemaKeys` 结构。
9. 英文 Shadow 层建后即弃（`_ = shadowLayer`，潜在 bug）→ 接通或不建。
10. `LookupAbbrev` 不应用 freqScorer 与 Search 不一致 → 统一或注明。

## 8. Rust 现状要保留的优点
- mmap combined.wdb 预合并作 System 层后端（懒加载、低内存）。
- 单一 .wdb 格式（不要引入 .wdat）。
- CachedDict 的 Mmap/Memory 回退。

---

## 9. 落地顺序（feed 阶段 B；多数为 engine 质量的前置依赖）
1. **接通 CompositeDict + store_layer + DictManager**（§1/§2/§3）：引擎改用 composite 查询面，词频移入查询时。**engine 打分器之前/同步做**（打分依赖 freq 已并入 dictWeight）。牵动 wind-store，需先定 store 接口。
2. **binformat：top-K + hotcache + abbrev 段**（§5.2-5.4）：支撑码表前缀质量与拼音简拼。
3. **删 datformat、修 Shadow 枚举**（§5.1/§7.1）：清理。
4. **common_chars / IsCommon 存盘**（§6/§5）：码表 common-first BFS 依赖。
5. phrase Logic 层、english、dictio 工具按各自子系统/阶段推进。

> 每步用 `wind_input/scripts/dev.sh ci` 把关。
