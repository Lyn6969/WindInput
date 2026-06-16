# 重设计差分：store（持久化存储）

> 阶段 A 产物。Go 侧 3 个只读 agent 提取、关键论断 grep 抽验 file:line 属实；Rust 侧本人通读。
> 体量：Go `internal/store` ≈ 3093 行生产代码（bbolt）；Rust `wind-store` ≈ 509 行。
> 本子系统是 dict 差分"接通 store_layer"的后端依赖——§9 给出 dict 所需 API 契约。

---

## 1. 核心现状分歧（最重要）

**Rust 的 redb 后端尚未落地**：`store.rs` 是桩（`// TODO: redb database handle`，`open` 不真正开库，`pause`/`resume` 是 TODO）。当前实际持久化**不是 redb，而是各类型独立文件**：
- `FreqTracker`(freq.rs)：内存 HashMap + **TSV 文件**，能用。
- `ShadowStore`(shadow.rs)：内存 HashMap + **JSON 文件**，能用，逻辑（LIFO/删除优先/pin/delete/reset）扎实。
- `user_words`/`temp_words`/`phrases`/`stats`/`migration`：**仅类型定义或空壳，无存储逻辑**。

**Go 是完整 bbolt**：单文件、事务、嵌套 bucket、异步词频 flush、临时词淘汰/晋升、统计、迁移、bulk 导入导出。bucket 层次（已核实）：
```
Meta（version/device_id） | Schemas/{schemaID}/{UserWords,TempWords,Freq,Shadow} | Phrases（全局） | Stats/{Daily,Meta}
```

### 决策：落地 redb 作为统一后端（对齐 MIGRATION_PLAN，redb 已在 Cargo.toml）
理由：
1. dict 的 store_layer 需要**前缀可扫描的用户/临时词** + **事务化的学习/晋升/淘汰**——散落的 TSV/JSON 文件撑不起（freq.tsv 没有按 code 前缀检索、temp 没有有序淘汰）。
2. 单库事务 + crash-safe（Go 强调 fsync 安全）优于 N 个文件 + N 套 pause/resume。
3. redb 提供有序 key + range 扫描（前缀检索）+ mmap，干净映射 Go 的 bucket 模型。

> 当前文件式 freq/shadow 虽能用，但三后端三套热替换路径与"统一"原则相悖；统一到 redb 后，freq/shadow 的**逻辑保留**、仅换后端。

---

## 2. redb 表设计（映射 bbolt）

redb 无嵌套 bucket（扁平 table），用 **table + 复合 key** 表达 Go 的层次：

| redb table | key（建议） | value | 对应 Go bucket |
|---|---|---|---|
| `user_words` | `"{schema}\x00{code}\x00{text}"` | 二进制记录 | Schemas/*/UserWords |
| `temp_words` | 同上 | 同上 | Schemas/*/TempWords |
| `freq` | `"{schema}\x00{code}\x00{text}"`（Go 用 `code:text`，统一改 \x00）| 定长(count u32+last_used i64+streak u8) | Schemas/*/Freq |
| `shadow` | `"{schema}\x00{code}"` | ShadowRecord | Schemas/*/Shadow |
| `phrases` | `"{code}\x00{text}"`（全局）| PhraseRecord | Phrases |
| `stats_daily` | `"YYYY-MM-DD"` | DailyStat | Stats/Daily |
| `meta` | `"version"`/`"device_id"`/`"schema_version"` | bytes | Meta |

要点：
- **复合 key 带 schema 前缀**（redb 扁平），`GetUserWords` = range 扫 `"{schema}\x00{code}\x00"`；`SearchUserWordsPrefix` = range 扫 `"{schema}\x00{prefix}"`。对齐 Go cursor.Seek 前缀语义。
- **value 改紧凑二进制**（postcard/bincode），替代 Go 的 JSON（freq 记录尤其，定长 13B vs JSON）——体积/性能双赢（Go 坏设计之一）。
- **事务**：学习/晋升/淘汰用单个 redb WriteTransaction 保证原子（修 Go EvictTempWords 的 view→update TOCTOU）。
- **异步词频 flush**：保留 Go 模式——内存累积 deltas，达 `freqFlushSize=50` 或 `30s` ticker 触发，单事务批量写（Rust Store 已留 `freq_deltas` 字段）。`ErrPaused` 时增量放回。
- **pause/resume**（Windows 热替换释放文件锁）：对齐 Go `Pause()`/`Resume(newPath)`，关库置 None / 重开 + 建表。

---

## 3. 用户词 / 临时词

Go（已核实）：
- key=`code\x00text`，code 统一小写；record=`{Text,Weight,Count,CreatedAt}` JSON。
- 用户词：`AddUserWord`(重复取 max weight) / `GetUserWords`(前缀 seek `code\x00`) / `SearchUserWordsPrefix`(seek `prefix`，跨 code) / `RemoveUserWord` / `UpdateUserWordWeight` / `OnWordSelected`(Count++，每 countThreshold 次 Weight+=boostDelta)。**用户词无权重上限**。
- 临时词：`tempWordMaxWeight=10000`；`LearnTempWord`(新词 Weight=min(addWeight,1e4)/Count=1；旧词 Weight=min(old+delta,1e4)/Count++)；`EvictTempWords(maxKeep)`(**按 weight 升序淘汰最低的**)；`PromoteTempWord`(读 temp→merge 写 user→删 temp，merge 时 Weight=min(temp+user,1e4))。

Rust 现状：`user_words.rs` 仅 `UserWordRecord` 结构（字段名 created_at:String，Go 是 int64）；`temp_words.rs` 空壳。
**目标边界**：在 redb 实现上述全部 ops；记录字段对齐（CreatedAt 用 i64，**统一单位**——修 Go user=秒/temp=毫秒不一致）；淘汰用单事务（修 TOCTOU）；晋升条件由调用方（dict StoreTempLayer）判断 Count≥promoteCount（对齐 dict 差分 §3）。

---

## 4. 词频模型（质量相关）

> ⚠️ **本节已被 [frequency.md](./frequency.md) 取代**（用户反馈：Go 的 boost-to-weight 完全重构）。
> 新模型：词频与权重**彻底解耦**，freq 只存 `{count, last_used}`（去 streak/boost），作为**排序独立维度**
> （码表 used-first 可选模式；拼音衰减分），**不再加到 weight**。下方 Go 现状仅作"被推翻的旧设计"留档。

Go `CalcFreqBoostWithProfile`（已核实公式，**旧模型，已弃**）：
```
base    = log2(Count+1) * BaseScale(50)
lambda  = ln2 / DecayHalfLife(72h)
recency = MaxRecency(100) * exp(-lambda * ageHours)
streak  = min(Streak * StreakScale(30), StreakCap(150))
boost   = min(base + recency + streak, BoostMax(2000))   // Count==0 →0
```
Streak 上限 255（u8）；异步 flush 时 `Streak += delta`（坏设计：批量累加使"连续"语义退化）。

Rust 现状（**两套且不一致**）：
- `FreqProfile::calc_boost`（**疑未接线**）：base_scale=100 / max_recency=50 / lambda=0.1(固定，非半衰期) / streak_scale=10 / streak_cap=200 / boost_max=**500**。
- `FreqTracker::get_boost`（**实际用**）：极简公式，**只用 count**（`log2(count+1)*base_scale*0.1`），忽略 recency/streak。

**目标边界**：
1. 统一为 Go 的 profile 模型（半衰期衰减 + recency + streak），**删极简 get_boost**。
2. 参数默认值对齐 Go（BaseScale 50 / MaxRecency 100 / HalfLife 72h / StreakScale 30 / StreakCap 150 / BoostMax 2000），lambda 由半衰期导出（非固定 0.1）。
3. **boost 量级必须与 engine RimeScorer 联动**：Go 中 freq boost 加到 dictWeight 后再 `NormalizeWeight([0,10000]→[-15,0])`。boost_max 取 2000 还是别的，须与引擎归一化区间一致（见 engine.md §1.2）——这是跨子系统约束，落地时一并定。
4. streak 语义：异步路径避免简单 +delta（考虑"连续"应是单调步进），或明确接受计数语义并改名。

---

## 5. Shadow

Go：bucket `Shadow`，key=`code`(小写)，record={Pinned[],Deleted[]}，pin/delete 互斥、CandID 优先匹配（空则按 Word）、LIFO（prepend）、空记录删 key。
Rust 现状：`shadow.rs` 逻辑**已对齐且扎实**（pin LIFO、delete 优先、reset、has_rule、get_rules，JSON 文件 + 原子写），仅缺 **CandID**（Go R2 的稳定 ID 匹配，动态短语需要）与 redb 后端。
**目标边界**：保留现有逻辑，补 `CandID` 字段与匹配优先级（对齐 dict 差分 Shadow），后端迁 redb（`shadow` table）。应用点仍在引擎排序后（dict 差分已定 Shadow 是 Provider 非层）。

---

## 6. 统计 / bulk / 迁移（阶段归属）

| 模块 | Go | Rust | 归属/决策 |
|---|---|---|---|
| 统计 stats | DailyStat（字符/小时分布/码长分布/选重位/活跃秒/按方案/按来源 10 类 CommitSource）+ StatCollector 30s flush + streak 天数 | 仅 4 字段 struct + 空 StatCollector | **阶段 D（设置程序统计页）**，低优先；落地时对齐 CommitSource 枚举 |
| bulk 导入导出 | 5 类数据全量导出/批量追加（备份恢复）| 无 | **阶段 D**（备份/迁移工具）|
| migration | **无版本表**，仅 2 个一次性幂等迁移（短语格式）| 空 | 见下 |

**迁移决策**（2026-06-16 用户澄清）：
- Go 缺统一版本管理是坏设计 → Rust **从一开始引入 `meta/schema_version`**，迁移按版本链执行（已落地，store.rs）。
- **legacy 文件式（FreqTracker/ShadowStore）无需迁移**：它们从未实际投产，**切换到 redb 时直接删除**，不保留、不迁数据。
- **Go 用户数据导入用通用导出+导入格式**（非 bbolt→redb 原地迁移）：Go 端导出为通用交换格式（如 JSON/TSV），Rust 端导入。解耦两边实现、避免 Rust 依赖 bbolt。属 Phase D 工具项；本阶段不实现，redb 记录字段保持语义清晰即可。

---

## 7. Go 坏设计（不照搬）
1. 无 schema 版本管理 → redb `meta/schema_version` + 版本化迁移。
2. CreatedAt 单位不一致（user=秒/temp=毫秒）→ 统一 i64（建议秒或全毫秒，定一个）。
3. 记录用 JSON 序列化 → 紧凑二进制（freq 定长 13B）。
4. `EvictTempWords` view→update 两步 TOCTOU → 单写事务。
5. `OnWordSelected` 对不存在词静默建 Weight=0 记录 → 显式区分或拒绝。
6. `AllSchemaPhrases`/`BulkPutSchemaPhrases` 永远空操作的死接口 → 删除或明确 planned。
7. `RemovePhrase` O(N) 兜底扫描（legacy key 遗留）→ 统一 key 从源头消除。
8. 异步 streak `+delta` 与同步 `+1` 语义不一致 → 统一。
9. stat flush 每 30s 全量 JSON 序列化（无脏标记）→ 增量/脏字段（阶段 D）。
10. freq key `code:text`（冒号分隔，与其它 bucket 的 `\x00` 不一致）→ 统一 `\x00`。

## 8. Rust 现状要保留的优点
- shadow.rs 的 pin LIFO + 删除优先逻辑（补 CandID 即可）。
- freq 的衰减 profile **概念**（对齐 Go 参数后启用）。
- 原子写（tmp+rename）思路——redb 自带事务后由其保证。

---

## 9. dict 所需的 store API 契约（store_layer 依赖，落地第①优先）
按 dict 差分 §3，store_layer 需 store 暴露：
- 用户词：`get_user_words(schema,code)` / `search_user_words_prefix(schema,prefix,limit)` / `add_user_word` / `remove_user_word` / `update_user_word_weight` / `on_word_selected(schema,code,text,boost_delta,count_threshold)`
- 临时词：`get_temp_words(schema,code)` / `learn_temp_word(schema,code,text,add_weight,weight_delta)->promoted?` / `increment_if_exists` / `evict_temp_words(schema,max_keep)` / `promote_temp_word(schema,code,text)` / `set_limits`
- 词频：`increment_freq_async(schema,code,text)` / `get_freq(schema,code,text)` / `calc_freq_boost(rec, now, profile)` / `set_freq_profile`
- shadow：`pin_shadow` / `delete_shadow` / `remove_shadow_rule` / `get_shadow_rules(schema,code)`（含 CandID）

## 10. 落地顺序（feed 阶段 B）
1. **redb 后端骨架**：open/表定义/事务/pause-resume/version（§2）。
2. **user_words + temp_words**（§3）+ **freq 统一模型**（§4）+ **shadow 迁 redb**（§5）——满足 §9 契约，解锁 dict 的 store_layer。
3. phrases（随 dict Logic 层）、stats/bulk（阶段 D）、Go 数据导入工具（阶段 D，字段现已兼容）。

> 词频设计以 [frequency.md](./frequency.md) 为准（解耦权重、排序独立维度）。落地建议顺序：store §10.1-2 → dict §9.1 → engine 打分器 + 词频重排。
> 每步 `wind_input/scripts/dev.sh ci` 把关。
