# 方案级用户数据：归属、缺口与分期

**状态**：设计草案，待拍板。触发自「英文方案要有用户词库与词频」「特殊方案也要支持候选调整、用户词库」两条需求。

调查过程中发现这不只是「缺功能」——**特殊模式当前在把用户的选择记进主方案的词频表**，越用越污染五笔的候选顺序，而它自己一条也读不到。修复这一条比新增功能更紧急。

---

## 一、核心问题：「归属方案」在 overlay 模式下取错了

用户数据（词频 / 候选调整 / 用户词库）一律按 `EngineManager::active_schema_id()` 归属。
这个值表达的是**当前主方案**，而 overlay 模式期间实际出候选的是另一个方案，`active` 根本不变。

| 输入状态 | `active_schema_id()` | 实际出候选的方案 | 一致？ |
| --- | --- | --- | --- |
| 普通输入 | `wubi86` | `wubi86` | ✅ |
| 特殊模式（快符） | `wubi86` | `qsym` | ❌ |
| 临时拼音 | `wubi86` | `primary_pinyin` | ❌ |
| 临时英文 | `wubi86` | `english` | ❌ |
| 快捷输入（mix） | `wubi86` | 按候选来源分流 | ❌ |
| 英文方案（active） | `english` | `english` | ✅ |

英文方案之所以是对的，正因为它是**换方案**而非 overlay——这也说明问题的边界在「overlay 不改 active」这一点上，与英文本身无关。

### 需要引入的概念：生效方案（effective schema）

> 这一次按键的候选，是哪个方案的引擎出的。

普通输入下它等于 active；overlay 期间等于 overlay 所引用的方案。所有用户数据的读写都应按它归属，而不是按 active。

`write_data_schema_id` 已有一个**按候选来源**分流的雏形（混输：码表→主方案、拼音→"pinyin"），但它只处理混输，且入参仍是 active。生效方案是它的推广。

---

## 二、三条通路的现状

### 词频

| | 读（`apply_freq_rerank`） | 写（`record_selection`） |
| --- | --- | --- |
| 普通码表 / 拼音 | ✅ | ✅ |
| 英文方案（active） | ✅ | ✅ |
| **特殊模式** | ❌ **根本没调** | ⚠️ **调了，记到主方案名下** |

`update_special_candidates` 只做 `convert_with` + `finalize_candidates`，没有重排步骤；而
`handle_special.rs` 的上屏路径照常调 `record_selection`。**只写不读，且写错地方**——这是当前
最该修的一条，它在持续污染主方案的词频表。

### 候选调整（shadow）

`apply_shadow` 按 `data_schema_id(active_schema_id())` 取规则，`update_special_candidates`
同样没调。写端（`shadow.addRule` RPC）也按 active。⇒ 特殊模式读写皆不通。

### 用户词库

引擎层各自挂 `StoreUserLayer` / `StoreTempLayer`：

| 引擎分支 | 用户词层 | 临时词层 | 归属 id |
| --- | --- | --- | --- |
| 码表（含特殊方案，`type = "codetable"`） | ✅ | ✅ | 方案自身 id |
| 拼音 | ✅ | ✅ | 折叠为 `"pinyin"` |
| **英文** | ❌ | ❌ | — |

★ **特殊方案的用户词库「读」其实是通的**（它们是 codetable 类型，走 else 分支挂了层），
缺的是「写」——加词流程按 active 归属，写进了主方案。读写不同源 ⇒ 写进去的词永远读不出来。

英文则是两端都没有：`build_engine` 的 english 分支在挂层之前就 `return` 了，注释写的是
「英文暂不挂用户词 / 临时词层（无造词学习），仅系统词库层」。

---

## 三、分期建议

### P1：修归属错位（纯修复，不加功能）

抽 `Coordinator::effective_data_schema(&state) -> Option<String>`，按 `state.active`
（`ModeKind`）分流；`record_selection`、`apply_shadow`、加词三处改用它。

⚠️ **这会改变现有行为**：特殊模式的选择当前记在主方案上，修完就不记了。存量数据里那些
「本该属于快符、实际记在五笔名下」的条目无法区分（记的是快符的码与词，混在五笔的表里），
**不做迁移**——它们本就是污染，随各自的调频衰减自然淡出即可（`position` 策略下会衰减；
`top`/`step` 下需用户手工清）。

判据同 [[freq-rerank-model]] §记账码：**读、写、调试三处必须同口径**，这次要一并检查
`debug_freq_count`。

### P2：英文的用户词库与词频

- `build_engine` 的 english 分支挂 `StoreUserLayer`（归属 `"english"`）；
- **不挂** `StoreTempLayer`：临时词库是「自动造词的暂存区」，英文没有造词流程，挂了永远是空表；
- 加词入口：英文方案下 `Ctrl+=` 应能把当前输入的英文词写进用户词库（专有名词、缩写、
  项目内部词汇是真实需求）。
- 词频已可用（`schema.english.frequency.*` 已落地）。

### P3：特殊方案的词频与候选调整读端

`update_special_candidates` 补 `apply_freq_rerank` + `apply_shadow`（用 P1 的生效方案）。
⚠️ 特殊方案多是小符号表，候选顺序常常是作者精心排的；调频默认应**关**，由用户按方案开启。

### P4：配置面

特殊方案的调频/候选调整是**每方案独立开关**还是**共用一套**？倾向前者——快符表和生僻字表
的诉求不同（前者要稳定顺序，后者要学习）。落点为 `[[schema.special_modes]]` 的新字段，
但那意味着这些字段也需要 GUI（当前只有引导键有）。

---

## 四、待拍板

1. **P1 的行为改变**是否接受（特殊模式不再往主方案记频）？存量污染是否需要提供「清理」入口？
2. **临拼 / 临英 / 快捷输入**的归属是否一并修正？它们同样错位，但影响面更大——临拼当前
   记在主方案名下，若改为记在 `"pinyin"` 名下，用户已积累的临拼词频会「看起来丢了」。
3. 特殊方案的用户数据是**每方案独立**还是**共用主方案**？独立更干净，但用户在快符里加的词
   在主方案下打不出来，可能反直觉。
4. P4 的每方案开关是否值得——会把 `special_modes` 的 GUI 从「一个引导键」扩成一组设置。

---

相关：[[freq-rerank-model]]（记账码口径与三处同源约束）、`docs/design/special-mode-codetable.md`
（特殊模式的引擎与配置结构）。
