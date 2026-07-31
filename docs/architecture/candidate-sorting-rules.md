# 候选排序规则（权威）

> **本文件是候选排序的唯一真相（source of truth）。** 凡改动任何与「候选先后顺序」有关的
> 代码——引擎内排序、`candidate_display_order`、`freq_rerank`、混输加权、短语注入、shadow——
> **必须先读本文件、改完后回来对齐本文件**。三套排序系统各改一处极易漂移（历史上反复发生），
> 本文件的存在就是为了把它们钉在同一口径上。
>
> 涉及文件：
> - `wind_input/crates/wind-candidate/src/candidate.rs`（字段定义 + `cmp_match_layers`/`cmp_exact_first`/`better`/`by_natural`）
> - `wind_input/crates/wind-engine/src/codetable/engine.rs`、`pinyin/mod.rs`、`mixed/engine.rs`（引擎内排序）
> - `wind_input/crates/wind-engine/src/freq_rerank.rs`（词频重排两条路径）
> - `wind_input/crates/wind-coordinator/src/handle_candidate.rs`（`candidate_display_order` + 装配流水线）

---

## 0. 一句话心智模型

一条候选从产生到显示，要穿过**三套彼此独立的排序系统**，后一套可以整体推翻前一套：

```
引擎内排序  ──►  协调器 candidate_display_order  ──►  词频重排 freq_rerank  ──►  shadow
（各引擎自排）    （无条件重排全部候选，匹配层为主）      （开自动调频且有记录时，          （置顶/删除，
                                                        首要键整体压过上一步）           最高优先级）
```

**最容易踩的坑：** 你以为改 `candidate_display_order` 就能决定最终顺序——**错**。只要开了自动调频
且有词频记录，`freq_rerank` 的档位/锚定是**首要键**，会整体压过 `candidate_display_order`
（`freq_rerank.rs` 头部注释、`candidate.rs` `cmp_exact_first` 文档都有警告）。**验证「匹配层/精确
优先」类改动必须先关自动调频**，否则会误判「改动没生效」。

---

## 1. 完整流水线（`Coordinator::build_candidates`，`handle_candidate.rs`）

按执行顺序，一次候选刷新经过以下步骤：

| # | 步骤 | 位置 | 作用 |
|---|---|---|---|
| 1 | **引擎 `convert`** | `engine_mgr.convert*` | 各引擎产出候选并**引擎内排序**（见 §4） |
| 2 | **`finalize_candidates`** | `handle_candidate.rs` | 词库值内嵌 `$CC/$AA/$SS` 展开（打 `is_command`/`is_group`，**不打 `is_phrase`**） |
| 3 | **短语注入** | 同上，`phrases.lookup` + `lookup_prefix` | 全局短语进候选（打 `is_phrase`，见 §5） |
| 4 | **空码补全收口** | 同上 | 精确模式无候选时补一条（`completion_hint`/`completion_pool`，统一判空取一条） |
| 4.5 | **`mark_common`（常用字判定）** | `handle_candidate.rs` | 无条件填 `is_common`（**不看 `filter_mode`**）；混输拼音精确档拿它当提档准入，见 §6 ③ |
| 5 | **`candidate_display_order` 排序** | 同上 | **无条件重排全部候选**（七级链，见 §6） |
| 6 | **按 `text` 去重** | `retain(seen.insert)` | 保留排序后第一条 |
| 7 | **`apply_filter`** | `handle_candidate.rs` | 检索范围过滤（常用字/GB18030）；**短语恒保留**（`is_common` 已由第 4.5 步填好） |
| 8 | **`apply_freq_rerank`** | 同上 | 开自动调频**且有词频记录**时重排（见 §7）——**首要键压过第 5 步** |
| 9 | **`apply_shadow`** | 同上 | 用户 shadow 规则：删除 + 置顶到指定位（**最高优先级**，见 §8） |
| 10 | **自动上屏复核** | 同上 | 满码/顶码唯一自动上屏判定（不改顺序，只决定要不要上屏） |
| 11 | **`expand_s2t_variants`** | 同上 | 简繁 1对多变体紧跟原字插入（**必须在去重/重排/shadow 之后**，见 §9） |

> ⚠️ 第 5 步与第 8 步的先后是 `freq_rerank` 正确性的前提：`freq_rerank` 的「维持权重序」分支
> 靠**稳定排序**保住第 5 步喂进来的顺序，它自己**从不比 weight**。调换两步、或在中间插重排，
> `freq_rerank` 的输出即失去权重语义且不报错（`freq_rerank.rs` 头部有详述）。

---

## 2. 候选的排序相关字段（`Candidate`，`candidate.rs`）

| 字段 | 类型 | 语义 | 谁置位 |
|---|---|---|---|
| `is_fuzzy` | bool | 模糊音变体命中（非原拼音精确） | 拼音引擎 |
| `is_prefix` | bool | **前缀补全**（候选码比输入长，如 `si`→思考`sikao`）；**又被全局短语借作「非精确层」标记** | 拼音引擎；协调器前缀短语（`lookup_prefix`，`is_prefix=!codetable_mode`） |
| `is_partial` | bool | **子短语**（候选码是输入的真前缀、比输入短，如 `baoan`→报`bao`） | 拼音引擎 |
| `is_exact_code` | bool | **精确匹配档**（候选 `code`==输入的完全匹配；或引导键导航候选的既定置顶） | 码表引擎（`code==input`）；协调器精确码短语（`lookup`） |
| `is_sentence` | bool | 引擎合成的整句解（Viterbi 多词/超长整词） | 拼音引擎 |
| `is_sentence_demoted` | bool | 整句已让位于精确整词（降级，不参与锚定） | 拼音引擎 |
| `is_phrase` | bool | 全局短语（`self.phrases` 注入的，系统/用户皆然） | 协调器短语注入 |
| `is_command`/`is_group` | bool | `$CC` 命令 / `$SS·$AA` 组——**决定选中行为，不参与排序** | 短语注入 / `finalize_candidates` |
| `weight` | i32 | 权重（**一物多用**：真实词频 + 隐式类别加成，见 §3 红线①） | 引擎 + 各套加成 |
| `base_order` | i32 | 词库**层级基序档**（`[[dictionaries]].base_order`，默认 0，小整数即分档） | 码表词库配置 |
| `natural_order` | i32 | **每库局部**出现序（各库从 0 数，仅同 `base_order` 档内可比） | 词库解析 |
| `consumed_length` | usize | 该候选消费的输入长度（分段上屏用；0=未标注/整串） | 拼音引擎 |
| `source` | enum | None/CodeTable/Pinyin/English/Phrase | 引擎 |

---

## 3. 三条贯穿全文的红线

**① 权重量纲不可比——跨类别比 `weight`，比的其实是类别。**
词组权重取自词频、单字取自字频，两套量纲不可比（`codetable/engine.rs` 自认）。更有甚者，多套系统往
`weight` 上叠**巨大常量**来表达「类别」：协调器 `PHRASE_WEIGHT_BASE=40M`、混输 `PHRASE_WEIGHT_BOOST=1M`
/`PARTIAL_MATCH_BOOST=500K`、拼音 `BARE_INITIAL_SINGLE_CHAR_BOOST=10M`。这些数字**没有物理意义**，
只是排序占位符。**任何「跨来源比权重」的想法都要先想到这一点。**

**② 匹配层/精确档先于权重——胜负常在比权重前就被结构标志定死。**
`candidate_display_order` 与 `freq_tier` 都把「层级/档位」作为**首要键**，`weight` 只在同层同档内才起作用。
所以「靠调权重把某候选提上去」经常无效——它可能在更低的匹配层，权重再高也上不来。

**③ 三套排序系统必须口径一致，改一处要核对另两处。**
`candidate_display_order`（用 `is_prefix`/`is_exact_code`）、`freq_tier`（用 `is_phrase`/`source`/`code==input`）、
混输引擎加成（用 `is_phrase`/`code==input`）是**平行实现**，各自有一套「谁该靠前」的判据。历史上多次
「只改一套、另一套绕过」导致行为漂移。**当前的对齐口径：完全匹配（精确码）才提前，前缀匹配一律避让。**

---

## 4. 引擎内排序（第 1 步）

各引擎产出候选时**先自排一遍**。注意：协调器第 5 步会**无条件重排全部候选**，所以引擎内排序的
**匹配层/精确档必须落到 `Candidate` 字段上随候选流动**，否则会被下游按纯权重推翻（「引擎排对了、协调器
又推翻」的白工）。

### 4.1 码表引擎（`codetable/engine.rs`）

- 候选来源：`dm.search`（精确）+ `dm.search_prefix`（前缀补全，精确模式跳过）。
- **只置位 `is_exact_code = (code == input)`**，**从不置 `is_prefix`/`is_partial`**（码表的「精确 vs 补全」
  分层全靠 `is_exact_code` 承载）。
- 排序：`cmp_exact_first(a,b).then(base_cmp)`，`base_cmp` 由 `base_sort` 选：
  - `base_sort = "weight"`（默认）→ `better`：`weight 降 → base_order 升 → natural_order 升 → code → consumed_length 降 → text`
  - `base_sort = "natural"` → `by_natural`：**完全忽略权重**，`base_order 升 → natural_order 升 → code → consumed_length 降 → text`
- **空码补全**：精确模式无候选且未满码时，从更长编码取首个作 `completion_hint`（**只备货不入列**，交协调器判空）。

### 4.2 拼音引擎（`pinyin/mod.rs`）

- 置位 `is_fuzzy`/`is_prefix`/`is_partial`/`is_sentence`/`is_sentence_demoted`；**约定不置 `is_exact_code`**
  （混输下码表精确恒先于拼音，靠这个约定）。
- 排序首要键 `cmp_match_layers`（见 §6），再按权重等。
- 特例：**裸声母**（无完整音节，如 `m`/`zh`）单字 +`BARE_INITIAL_SINGLE_CHAR_BOOST=10M` 提到多字词前；
  **残码前缀补全**故意不标 `is_prefix`（否则数百单字淹掉目标词，见 `meiy`→没有 案）。

### 4.3 混输引擎（`mixed/engine.rs`）——**独立的权重分档系统**

混输把码表半边 + 拼音半边合并，用**加大常量分档 + 纯按权重排**（`merge_sort_dedup` 只按
`weight 降 → natural_order 升`，**不走匹配层**）：

| 档 | 加成 | 对象 |
|---|---|---|
| 短语 | +`PHRASE_WEIGHT_BOOST` = **1,000,000** | `is_phrase` 候选 |
| 码表精确全码 | +`codetable_weight_boost`（可配） | `code == input` |
| 码表前缀补全/拆分 | +`PARTIAL_MATCH_BOOST` = **500,000** | 其余码表候选 |
| 英文整词 | +`ENGLISH_EXACT_BOOST` = **500,000** | `code == input` 英文 |
| 英文前缀 | +`ENGLISH_PREFIX_BOOST` = **0** | 其余英文 |
| 拼音 | **÷ `PINYIN_TIER_SCALE` = 100** | 全部拼音候选 |

> 例：混输打 `da`，字 矼 的最终权重 `509000 = 矼基础字频(~9000) + PARTIAL_MATCH_BOOST(500000)`。
> 这套加成是**引擎内**的，之后协调器第 5 步会用匹配层重排、第 8 步 freq 会用档位重排——所以 509000
> 这个数只在同层同档内才决定先后。

**截断的拼音保底配额**（`truncate_with_pinyin_quota`，`convert` 与 overflow 共用）：码表带 +500K、
拼音 ÷100 ⇒ 截断时码表恒在前，而五笔 2 码前缀的候选量常常吃满整个配额——实测 **52 个 2 码前缀的
条目数 > 300**（最多 `kh` 663 条），其中 `pu`（495 条）正是「既是五笔 2 码、又是完整拼音音节」的
交集。那种输入下拼音候选**一条都进不了列表**，协调器 §6 ③ 的拼音精确档就无从下手（提不了不在场的
候选）。故截断时给拼音留 `max/PINYIN_QUOTA_DIVISOR` 席。

- **只补不挤空**：尾部确实没有拼音候选时（`kh` 这类非音节码、纯五笔溢出串）一条码表候选都不会被
  挤掉，行为与改动前完全一致（回归测试 `no_pinyin_means_no_codetable_is_evicted`）。
- ⚠️ 补进来的拼音候选**追加在尾部、不保证有序**——这依赖协调器第 5 步会无条件重排全部候选。
  本函数的职责只是「让候选进得来」，不是「排好序」。

---

## 5. 全局短语的注入（第 3 步，`handle_candidate.rs`）

全局短语（`self.phrases`，系统/用户皆然）**不与方案挂钩、是跨方案的**，故需特殊处理。**按「来源=全局短语」
统一处理，不按 `$CC`/`$SS`/静态语法类型区分**（语法只决定 `is_command`/`is_group` 的选中行为）：

| 匹配方式 | 来源 | `is_exact_code` | `is_prefix` | `weight` |
|---|---|---|---|---|
| **精确码**（`lookup`，`code==输入`，HashMap 精确键） | 完全匹配 | **true** | false | `PHRASE_WEIGHT_BASE(40M) + hit.weight` |
| **前缀枚举**（`lookup_prefix`，码严格更长） | 前缀匹配 | false | **`!codetable_mode`** | `hit.weight`（**不加 40M**） |

- **精确码短语**（打全 `date`）→ 进精确档、靠 40M 抬升，与码表精确候选同层竞争（对应「完全匹配才提前」，
  也是 `skce` 短语曾输给五笔「可能」那个 bug 的修复点）。
- **前缀短语**（打 `da`）→ 不进精确档；`is_prefix=!codetable_mode` 使其在拼音/混输降到拼音精确候选之下、
  码表下与更长编码补全同档；按 `hit.weight` 排，**不靠 40M 硬顶**（对应「前缀避让、按权重」）。
- **方案内词库 `$CC` 词条**（挂在五笔等方案里，走 `finalize_candidates`）**不是**全局短语：它 `is_phrase=false`、
  `source=CodeTable`，按方案权重排、`is_command` 只影响选中行为——**天然按方案处理，不经本节**。

---

## 6. 协调器 `candidate_display_order`（第 5 步，权威显示序）

对**全部候选无条件重排**。`candidate_display_order` 本体是**六级**比较链（`handle_candidate.rs`）：

```
① cmp_match_layers        is_fuzzy 升 → is_prefix 升 → is_partial 升 （非模糊 > 完整/子短语 > 前缀补全 > 模糊）
② cmp_exact_first         is_exact_code 降                          （同层内精确档优先）
③ cmp_pinyin_exact_first  拼音精确档 降   （**仅混输**；is_pinyin_exact_tier，先于码表前缀补全）
④ by_weight               weight 降       （base_sort=natural 时 ignore_weight 跳过本级）
⑤ base_order              升              （词库档位，跨库隔离）
⑥ natural_order           升              （每库局部出现序）
⑦ consumed_length         降              （消费整串者优先，供分段上屏；对齐引擎 better 末级）
```

**③ 拼音精确档（混输专属）**：判据 `wind_candidate::is_pinyin_exact_tier(c, input_len)` =
`source==Pinyin && is_common && !is_prefix && !is_partial && !is_abbrev && !is_fuzzy`
**且消费整串**（`consumed_length == 0 || consumed_length >= input_len`，0=未标注按整串算）。
于是三档顺序为「码表精确/精确码短语 → **拼音精确** → 码表前缀补全」。

- ⚠️⚠️ **「消费整串」必须直接问 `consumed_length`，不能拿 `!is_partial` 代替**（首版即栽于此）：
  真机打 `aaw`（本意 `aawt`→工作）首选变成拼音「啊啊」。它是 Viterbi 整句（词条 `啊啊 a a`），
  `code` 取 `completed`="aa"、`consumed_length=2`，只解释了 3 键中的 2 键——**可 `is_partial`
  是 false**：整句走 `insert(0)` 不经 `pinyin/mod.rs` 里算 `is_partial` 的 `push_hit` 闭包，
  同文合并时还会主动 `existing.is_partial = false`。
  ★ `is_partial` 的语义是「这不是子短语」，**不是**「消费了整串」，两者在残码场景下分叉。
  ★ 该场景比 `xu` 更严苛：五笔 `aaw` **无精确全码**（候选全是前缀补全），没有
  `is_exact_code=true` 的候选占着首位 ⇒ 拼音一旦被误提档就直接是首选。
  回归测试 `mixed_aaw_partial_sentence_does_not_preempt_codetable`（断言首选是「工作」）。

- **修的什么**：混输打 `xu`，码表精确「弱」是首选（`xu` 是二简码），但拼音「需」（`code==xu`、
  该音节最高频字 6999）**实测排第 98 位**——被 124 条 `xu*` 码表前缀补全整体压住
  （`per_page=5` ⇒ 第 20 页，与真机报告精确吻合）。根因是「精确 vs 前缀」这个维度混输此前
  只承认码表那一半：码表精确 +1e7、码表前缀补全 +500K，拼音**不分精确与补全**统一 ÷100。
  拼音侧的 `is_prefix`/`is_partial` 信息一直齐全，只是在 `normalize_pinyin` 被抹平。
- **为何是层级键而非权重加成**：拼音词库最高权重是「的」**15,378,475**，任何「不先归一就加
  boost」的写法都会让它越过码表精确档 1e7（打 `de` 首选变「的」）。层级键无量纲陷阱（红线②）。
- **为何叠 `is_common`**：单音节同音字动辄上百条（`xu` 有 **329** 条，含权重 0 的生僻字）。
  用检索范围而非固定条数上限把关，让「提多少条」跟着用户的 `filter_mode` 走。
  ⚠️ 依赖第 4.5 步 `mark_common` 无条件跑过——该判定原先写在 `apply_filter` 内部、
  `Gb18030` 时随 early-return 一起被跳过，沿用那个位置会让本档在 Gb18030 下**静默失效**。
- ⚠️ **必须按引擎语境传 `mixed`，不可恒 true**：纯拼音下全体候选同为 `Pinyin` 来源，本键会退化成
  「`is_common` 优先」，把含生僻字的多字词（`is_string_common` 要求整串每字都常用）硬降到全部
  常用单字之后。回归测试 `pure_pinyin_xu_order_is_unaffected`。
- ★ 这是「五笔优先」的一处**有意松动**：码表**精确**仍恒先于拼音（①②不变），只有码表**前缀
  补全**让位。依据是短输入下二者置信度反相关——124 条补全全都要打满 4 码才精确，而拼音 `xu`
  已是完整音节。

**关键点：**
- ①②③是**结构层级**，④才是权重——所以「靠权重反超」只能在同层同档内发生（红线②）。
- `cmp_exact_first` 置于 `cmp_match_layers` **之后**：精确优先只在同匹配层内生效，不跨层提拔
  （`is_prefix=true` 的前缀短语仍留在下层）。
- **⚠️ 本函数末级是 ⑥ `consumed_length`，没有 `text` 末级。** 主排序（第 5 步）对①~⑥全同分的候选
  靠 `sort_by` 的**稳定性**维持引擎/注入序。**凡需要「确定性取一条」的调用方（如空码补全池取首条）
  必须自己补 `.then_with(|| a.text.cmp(&b.text))`**——`lookup_prefix`/HashMap 遍历序不定，不补末级会
  退化成「随机取一条」、重启可能不同（补全池已补，见第 4 步）。

---

## 7. 词频重排 `freq_rerank`（第 8 步）——**开自动调频时的真正话事人**

### 7.1 触发前提（`apply_freq_rerank`）

**全部满足**才跑，否则直接跳过（此时 §6 的显示序即最终序）：
1. 有 `store`；2. `code` 非空且候选 ≥ 2；3. **自动调频开启**（`freq_settings().enabled`）；
4. **至少一个候选有词频记录**（`recs` 非空，`count>0`）——**全新无记录时不跑**。

### 7.2 引擎分派

- `is_pinyin()`（纯拼音）→ `rerank_pinyin_decay`（§4 衰减软置前）
- 其余（**码表 / 混输**）→ `rerank_codetable_usedfirst`（§3 永久 used-first）

### 7.3 码表/混输：`rerank_codetable_usedfirst` + `freq_tier`（**首要键**）

`freq_tier`（越小越靠前）：

| tier | 对象 | 判据 |
|---|---|---|
| **0** | 码表精确全码 | `!is_phrase && source==CodeTable && code==input` |
| **1** | **精确码短语** | `is_phrase && is_exact_code` |
| **2** | **拼音精确档** | `is_pinyin_exact_tier(c)`（与 §6 ③ **共用同一判据函数**） |
| **3** | **码表前缀补全 + 前缀短语** | `is_phrase && !is_exact_code`；或 `source==CodeTable && code!=input`；或 `_ =>` |
| **4** | 拼音其余（前缀/子短语/简拼/模糊/生僻） / 英文 | `source==Pinyin/English` |

> tier 2 **不必**像 §6 那样区分「是否混输」：`freq_tier` 只服务 `rerank_codetable_usedfirst`
> （码表/混输），纯拼音走 `rerank_pinyin_decay` 不经过此处；纯码表下没有 `Pinyin` 候选，该档天然
> 是空操作。**但两处的判据函数必须是同一个**——只改 §6 一侧，开自动调频时会被 `freq_tier`
> 整体绕过（红线③）。

- **档内**再按 used-first：`Step`（count 降、last_used 破平，抗误选）/ `Top`（last_used 降、count 破平，MRU）；
  同档无记录者返回 `Equal` → **稳定排序维持第 5 步喂进来的显示序**。
- **首选保护 `ProtectPolicy`**：重排后把「基础序前 N 位」回填锁定（呈现层保护，不动 weight）。
  N **按输入码长分级**（`schema.codetable.frequency.protect_top_n_len{1,2,3}` + `protect_top_n` 兜底）：

  | 输入码长 | 出厂 N | 理由 |
  |---|---|---|
  | 1（一简位） | 1 | 五笔一简 25 个码**每个都是二选一**（`a` → 工 9999 / 戈 9998） |
  | 2（二简位） | 1 | 同上，616 个码中 39 个有竞争者 |
  | 3（三简位） | 0 | 钦定性弱于一二简 |
  | ≥ 4（全码位） | 0 | 调频该起作用的地方 |

  ⚠️ **这是简码钦定次序的唯一防线**：词库靠权重表达简码地位（`gen_dict` 一简 9999 / 二简 9950 /
  三简 9000，普通词条压在 9000 以下），而**本节的比较链不含 weight**——权重再高也拦不住
  「被选过」这一位。设计与数据统计见 `docs/design/codetable-freq-short-code-protection.md`。

  保护名额**只在精确档内取**（`is_exact_code`）：钦定首选必在精确档，名额匀给前缀补全没有语义
  依据；精确候选不足则少保护，该码位无精确候选则不保护。
- ⚠️ 保护作用域是「**码表配置组**」而非「纯码表方案」：`freq_settings()` 按"非拼音即码表"分流，
  **混输走的是同一套值**（混输下 2 键拼音会落进"二简位"档——有意保留，此时基础序首位本就是
  码表精确候选，保护它与"五笔优先"同向）。拼音路径恒为 `ProtectPolicy::NONE`。
- ⚠️ **`freq_tier` 是首要键，开自动调频时整体压过 `candidate_display_order`**，也因此**掩盖
  `is_exact_code`/`is_prefix` 的效果**——验证 §6 类改动**必须关自动调频**。
- ⚠️ tier 1 vs tier 3 对短语的区分**依赖 §5 打好的 `is_exact_code`**：精确码短语 tier 1、前缀短语 tier 3
  与码表补全同档（这是「打 `da` 时 `date` 短语不再压过码表补全」的落点）。

### 7.4 纯拼音：`rerank_pinyin_decay`

- **锚定**：`(is_sentence && !is_sentence_demoted) || (is_phrase && is_exact_code)` 的候选恒锚定顶部
  （互相维持引擎权重序）。**只锚定精确码短语**——前缀短语（`is_phrase && !is_exact_code`）不锚定，
  落到下面 `cmp_match_layers` 靠 `is_prefix` 降到精确候选之下（与 §7.3 `freq_tier` tier1/tier2、
  §6 `candidate_display_order` **同口径**：完全匹配才提前、前缀避让）。
- 其余：先 `cmp_match_layers`（不跨层提拔），再按衰减分（半衰期）软置前，褪色（< ε）落回权重序。

> ✅ **已对齐**：此前 `|| is_phrase` 一刀切锚定所有短语，导致纯拼音下 `date` 前缀短语在「自动调频开
> 且有词频记录」时被顶到首位（潜伏 bug，实测复现）。已改为 `|| (is_phrase && is_exact_code)`，与
> `freq_tier` 口径一致。回归测试 `pinyin_exact_phrase_is_anchored_like_sentence`（精确码短语仍锚定）
> + `pinyin_prefix_phrase_not_anchored`（前缀短语不锚定）。

---

## 8. Shadow（第 9 步，最高优先级）

`apply_shadow`：按用户 shadow 规则先删 `deleted`、再把 `pinned` 词移到指定位置。**在所有排序之后应用，
优先级最高**——用户手工置顶/删除的意图压过一切算法序。按 `data_schema_id` 归属（拼音族折叠共享、
码表/混输各自独立）。

## 9. `apply_filter` 与 `expand_s2t_variants`

- **`apply_filter`（第 7 步）**：按检索范围（常用字表 / GB18030）过滤；**`is_phrase` 候选恒保留**。
- **`expand_s2t_variants`（第 11 步）**：简繁 1对多变体紧跟单字原字插入。**硬约束**：必须在
  去重/排序/词频/shadow **全部完成后**（否则去重按 text 误删变体、重排拆散原字与变体）、且在
  **自动上屏判定之后**（否则变体会让「唯一候选」误判为不唯一，静默否决自动上屏）。

---

## 10. 常量速查

| 常量 | 值 | 位置 | 作用 |
|---|---|---|---|
| `PHRASE_WEIGHT_BASE` | 40,000,000 | `coordinator.rs` | 协调器**精确码短语**权重基（前缀短语已不用，改按 `hit.weight`） |
| `PHRASE_WEIGHT_BOOST` | 1,000,000 | `mixed/engine.rs` | 混输 `is_phrase` 档 |
| `PARTIAL_MATCH_BOOST` | 500,000 | `mixed/engine.rs` | 混输码表前缀补全档 |
| `ENGLISH_EXACT_BOOST` | 500,000 | `mixed/engine.rs` | 混输英文整词 |
| `ENGLISH_PREFIX_BOOST` | 0 | `mixed/engine.rs` | 混输英文前缀 |
| `PINYIN_TIER_SCALE` | 100 | `mixed/engine.rs` | 混输拼音 ÷ 降档 |
| `PINYIN_QUOTA_DIVISOR` | 5 | `mixed/engine.rs` | 截断时拼音**保底配额**分母（`max/5`，300 ⇒ 60 席） |
| `BARE_INITIAL_SINGLE_CHAR_BOOST` | 10,000,000 | `pinyin/mod.rs` | 裸声母单字提权 |
| `LEARN_ADD_WEIGHT` | 800 | `coordinator.rs` | 加词/学习临时权重 |
| `freq_tier` | 0/1/2/3 | `freq_rerank.rs` | 词频重排档位（见 §7.3） |

---

## 11. 不变量与红线清单（改排序前逐条自检）

1. **完全匹配才提前，前缀匹配一律避让**——精确码短语（`lookup`）可入精确档；前缀短语（`lookup_prefix`）
   一律降层/按权重。三套系统都按此口径。★ 混输的拼音精确档（§6 ③ / §7.3 tier 2）是这条口径的
   **贯彻而非例外**：它让「拼音完全匹配」也享受到此前只有码表才有的提前，代价是码表**前缀补全**
   （非完全匹配）让位。码表精确仍恒先于拼音。
2. **跨来源比 `weight` 无意义**（红线①）——量纲不可比 + 巨大类别常量。别用权重表达「类别」，用显式标志
   （`is_exact_code`/`is_prefix`/`is_phrase`）。
3. **匹配层/档位先于权重**（红线②）——想让某候选靠前，先确认它在对的匹配层/档，而不是加权重。
4. **改一套排序、核对另两套**（红线③）——`candidate_display_order` / `freq_tier` / 混输加成 / 拼音锚定
   是平行实现，漂移无编译报错。
5. **验证匹配层/精确档类改动必须关自动调频**——否则 `freq_tier`/锚定会掩盖效果，误判「没生效」。
6. **`candidate_display_order` 必须在 `freq_rerank` 之前**——后者靠稳定排序继承前者的权重序，自己不比权重。
7. **末级 `text` 兜底不可省**——破 HashMap/遍历序不定。
8. **新增排序标志字段要穷举「谁属于这一层」**——全仓 `Candidate{..}` 都以 `..Default::default()` 收尾，
   漏标默认 false、编译器抓不到。
9. **码表引擎从不置 `is_prefix`**——其精确/补全分层靠 `is_exact_code`；拼音靠 `is_prefix`/`is_partial`。
   两种引擎分层维度不同，写跨引擎逻辑时勿混。

---

## 12. 已知待办 / 存疑

- ~~拼音锚定平行漂移~~ **已修**（§7.4）：`rerank_pinyin_decay` 改为只锚定精确码短语，与 `freq_tier` 对齐。
- **混输加成系统 vs 匹配层**：混输引擎的 500K/1M 加成是「类别编码进权重」的老范式，与
  `candidate_display_order` 的匹配层是两套语言。长期可考虑统一到「匹配层 + 真实权重」，属较大重构。
  ✅ **已迈出第一步**：拼音精确档没有沿用加成，而是做成层级键（§6 ③）——正因为拼音权重上限
  1537 万让「加 boost」这条路走不通（会越过码表精确档 1e7），反过来印证了层级键是对的方向。
  引擎内 `÷100` 的降档保持不动，仅作为「档内权重」使用。
- **混输拼音的召回面收窄（未做）**：主流实现（微软、冰凌）在混输下**不给拼音做前缀补全/模糊**，
  只认精确音节。本仓已按此方向定案但**尚未实施**，落点是 `pinyin::Config` 新增 `enable_completion`
  + `schema.mix` 开关，经 `manager.rs::build_engine` 注入（与 `enable_pinyin_abbrev` 同一条路）。
  ⚠️ **实施前必须先改造清空守护**：`mixed/engine.rs` 注释与 `clear_blocked_by_candidates` 明文依赖
  「`wanl` 出前缀补全候选 → 拦住清空」，而这条兜底恰恰只在 `auto_commit_block_on_pinyin=false` 时
  是唯一防线。正确切法是区分「补全不进候选列表」与「引擎内部仍知道有更长码」——后者要保留。
  回归测试见 `input_flow.rs` 末尾的 `wanl` 用例。
- **`base_sort=natural` 与短语**：natural 模式忽略权重，短语靠 `base_order`/`natural_order` 默认 0 浮顶，
  与短语「按权重」的新方向是否自洽，待观察。
