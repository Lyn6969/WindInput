# 重设计：用户词频系统（权威设计）

> 阶段 A 补充。用户明确反馈：Go 的词频设计不合理，需**完全重构**。本文为词频的**单一真值源**，
> 取代 store.md / engine.md / dict.md / config-schema.md 中"词频 boost 加到 weight"的旧表述。

## 1. 背景与动机（为什么推翻 Go）
Go 的做法：记录使用次数 → 以**加权方式把 boost 加到词的 weight 上**。问题：
- 词库 weight 体系繁杂、差异巨大（拼音 weight 可达千万级，码表/inner_order 又是另一套）。
- boost 有上限（Go `BoostMax=2000`），加到动辄百万、千万的 weight 上**几乎不起作用**。
- 结果：相近权重的词偶尔生效；但常出现**用几十次仍无法超过前一个词**。用户词频表现极不统一。

结论：**词频必须与权重彻底解耦**，不再做"加到 weight"的有界 boost。

## 2. 核心原则：两个独立概念
| 概念 | 含义 | 用途 |
|---|---|---|
| **权重 weight** | 词库**自带**的权重（统一命名"权重"）| 默认排序维度 |
| **用户词频 frequency** | **完全独立**的一套，只记录**真实使用数据** | 独立排序维度 |

- 词频**只记真实数据**：`{ count: u32, last_used: i64 }`（按 schema+code+text）。选词时 `count++`、`last_used=now`。
- **不再有 streak / boost / weight 叠加**（Go 的 streak 批量累加语义混乱、boost-to-weight 无效，一并去除）。
- 排序时把 weight 与 frequency 作为**独立维度组合**，**绝不改 weight**。

## 3. 码表方案：基础排序 + 词频（两层关系）
排序是**两层**，不是互斥三选一：

**第一层 基础排序 `base_sort`**：`weight`（词库权重）或 `natural`（自然/字根序，对应 dict.md WeightMode 的 inner_order）。
- 为何可选：**有些词库无权重，只能自然排序；有些词库虽有权重，但设计上就按自然序走**。故基础排序由方案/用户指定。

**第二层 用户词频 `user_frequency`（开关）**：开启后，**以 base_sort 为基底**，把用过的词按词频上浮。
开启后规则（used-first，基底=base_sort）：
1. 有词频数据的候选**优先**，按 `freq_metric` 排序；
2. 无词频数据的候选**回退**到 base_sort（weight 或 natural），排在已用词之后。

排序键：`(has_freq?1:0 desc, freq_metric desc, <base_sort: weight desc | natural asc>)`。
- 码表 `freq_metric`：以 **count 为主**（"这个码我选了这个词 N 次，置前"）；last_used 作轻量 tiebreak。
- 效果：**用一次即可靠上浮**，不受 weight 量级压制——修复 Go 痛点。
- 不混算 weight 与 count（量纲不同），用分区/分层实现。

**词频应用策略 `freq_strategy`（两种语义，对应用户诉求"一次到顶 / 逐次提升"）**：
| 策略 | used 集合内排序键 | 行为 | 适合 |
|---|---|---|---|
| **`top`（一次到顶 / MRU）** | `last_used desc` 主，`count desc` 次 | 选一次→立即置该档之首 | "刚选的下次就在首位" |
| **`step`（逐次提升）** | `count desc` 主，`last_used desc` 次 | 累积使用才爬升，抗误选 | 稳健，默认 |
- 两者共用 used-first 基底：用过的（count>0）浮到未用之上，未用回退 base_sort（稳定排序保持其原序）。
- 真正的"逐位移动一格"需存显式 rank、收益低；用 `count desc` 表达"逐次提升"语义已是 Go/搜狗码表标准做法。
- `step` 是默认（等价旧的无配置 used-first count 序，仅多加 last_used tiebreak）。

## 4. 拼音方案：次数 + 最近时间 + 衰减
拼音候选多、有整句组合，**不宜硬 used-first**（会破坏长句质量）。词频按**衰减分**参与排序：
```
age_hours   = (now - last_used) / 3600
decay       = exp(-ln2 * age_hours / half_life)     // 半衰期衰减，最近用过≈1，久未用→0
freq_score  = (base_scale * log2(count + 1) + recency_peak) * decay
```
- `recency_peak`（默认 0）：与使用次数无关的"刚用过"峰值加成，随半衰期同步衰减；=0 时完全退化为旧公式，向后兼容。
- 最近+高频 → freq_score 高，候选上浮；久未用 → 衰减回落，自然让位。
- **关键差异（相对 Go）**：freq_score 在**归一化分数层**与引擎基础质量分结合，**不是加到原始大 weight 上**。引擎 RimeScorer 的基础质量分本就是受控小范围（见 engine.md §1.2），freq_score 按可比量级叠加 → **真正能重排**。
- 组合方式：`final = base_quality + freq_score`（base_quality 来自 RimeScorer 的词库质量分，与 LM/initialQuality 同源、同量级）。具体系数实现期调参，目标是"高频近用词稳定靠前、但不压垮整句最优解"。

**实现细化（关键，避免重蹈 Go 覆辙）**：当前 Rust 拼音引擎候选用的是**原始大 weight**（整句 `SENTENCE_WEIGHT_BASE=30M`、词百万级），而 `pinyin_score`（`base_scale=100`）量级极小。**绝不能 `weight += freq_score`**——会被大 weight 淹没（=Go bug 重演）。拼音侧改为**带衰减的软置前 + 整句豁免**：
1. **整句豁免**：`SENTENCE_WEIGHT_BASE` 的整句候选（Viterbi 最优解）恒不被词频挤下。
2. **非整句候选**：用过的按 `pinyin_score` 软置前于未用候选。
3. **阈值褪色**：当 `pinyin_score < ε`（久未用、衰减回落）→ 该候选**失去 used-first 资格**，落回 weight 序。
- 本质分野：**码表的"用过"是永久的（count 不衰减），拼音的"用过"是会褪色的（半衰期衰减）。**

## 5. 数据与排序的位置（修正 dict/engine 旧设计）
- **存储**（store）：freq table `{count, last_used}`，按 (schema, code, text)。异步批量写保留。提供 `freq_lookup(schema, code, text) -> Option<{count,last_used}>` 与 `record_selection`。
- **排序阶段应用**（engine 排序层，**非 dict 查询时**）：
  - dict 的 CompositeDict 只负责**合并各层候选**（system + 用户词 + temp + phrase），产出带 weight 的候选集——**取消"查询时把 FreqBoost 加到 dictWeight"**（dict.md 旧设计作废）。
  - engine 排序层（持有 store 的 freq 只读访问）按**方案类型**应用词频维度：码表按 §3，拼音按 §4。然后截断。
- 即词频从 Go 的"查询时改权重"彻底变为"**排序时独立维度**"。

> 区分两个易混概念：**用户词**（用户造的词，是 dict 的一个 layer，有自己的 weight）≠ **用户词频**（对任意候选的使用统计，是排序维度）。本文只讲后者；前者仍按 dict.md 的 store_layer。

## 6. 配置（修正 config-schema 旧 FreqSpec）
- 码表排序（两层，对齐 §3）：`base_sort: weight | natural` + `user_frequency: bool` + `freq_strategy: top | step`。**取代** Go 的单一 `candidate_sort_mode`。
- 主开关：`[learning.freq] enabled`（词频维度总闸，关闭则完全不重排）。shipped schema 默认 `true`。
- 拼音衰减参数（FreqSpec 仅保留衰减相关）：`half_life`（默认 72h）、`base_scale`、可选 `recency_peak`。
- **删除**旧的 boost-to-weight 语义字段（BoostMax / StreakScale / StreakCap 等"加权上限/连击"参数——属旧模型）。
- 词频默认值改为**单一真值源**（store 提供默认 + schema 可覆盖），消除 store.md 指出的"两套默认源不一致"。

## 7. 落地（并入阶段 B store/engine）
1. store：freq table 改为 `{count, last_used}`，去 streak/boost；提供 lookup + record。
2. engine 排序层：新增**词频重排步骤**（码表 used-first 分区；拼音衰减分叠加），持 store freq 只读访问。
3. dict：composite 去掉查询时 FreqBoost，只合并层。
4. config：CodeTableSpec 排序改两层 `base_sort(weight|natural) + user_frequency:bool + freq_strategy(top|step)`；FreqSpec 改衰减参数。

> 受影响差分均已加指针指向本文。每步 `wind_input/scripts/dev.sh ci` 把关。

## 8. 落地状态（2026-06-18）
- **F1 码表策略 + F3 配置（码表部分）**：已完成并部署。
  - `EngineManager::freq_settings()` 按活跃方案解析并缓存 `{enabled, strategy(top/step)}`（`learning.freq.enabled` + `engine.codetable.freq_strategy`），避免每键读盘。
  - shipped schema（wubi86 / wubi86_pinyin）设 `[learning.freq] enabled = true`，`engine.codetable.freq_strategy = "step"`（默认）。
- **词频重排下沉 engine 排序层（§5/§7）**：重排纯函数移入 `wind-engine::freq_rerank`（`rerank_codetable_usedfirst` / `rerank_pinyin_decay` + `freq_tier`），coordinator 的 `apply_freq_rerank` 只负责取词频记录、按 `is_pinyin()` 分流调用。两路均有原生单测（wind-engine `freq_rerank::tests`，6 例）。
- **F2 拼音衰减**：已完成。`rerank_pinyin_decay` 按 §4 实现 —— ① 整句/短语豁免（`weight ≥ PINYIN_SENTENCE_FLOOR=20M` 锚定顶部，介于词权重上限~19M 与整句基准 30M 之间）；② 非整句按 `FreqProfile::pinyin_score`（半衰期衰减）软置前；③ 阈值褪色（衰减分 `< PINYIN_FREQ_EPSILON=10.0` → 落回引擎权重序）。`now` 由 coordinator 注入便于测试。
- **recency_peak 近用峰值加成**：已完成。`FreqProfile` 新增 `recency_peak: f64`（默认 0.0），`pinyin_score` 公式改为 `(base_scale * log2(count+1) + recency_peak) * decay`；`EngineManager::pinyin_freq_profile()` 从配置 `frequency.recency_peak` 读取（非负截断）。`=0` 时完全向后兼容。
- **L 造词显现**：已完成。`PinyinEngine` 新增可选 `store_layers`（`with_store_layers`），`EngineManager::build_engine` 拼音分支在有 Store 时挂 `StoreUserLayer/StoreTempLayer`（按 schema 隔离）。convert 第 6 步按相同码（整串精确 + 各前缀子码 + 前缀补全）并入用户/临时造词，dedup by text、source=Pinyin、consumed_length 据前缀标注（部分消费分段上屏）。原生单测见 wind-engine `pinyin::tests`（3 例）。
  - 注：混输的 pinyin 子引擎按 secondary schema id 挂层；混输造词键于 mixed schema id，故混输 L 不在此生效（无回归，待混输专项）。
- **protect_top_n**：呈现层前 N 位保护，已实现。语义定稿：重排**前**记录引擎基础序（base_sort 输出）的前 N 个候选，重排**后**按原相对序回填到前 N 位；其余候选相对序不变。优先级高于词频——即使某候选词频极高，只要它不在保护集内，就不能占据前 N 位。默认 `0` = 空保护集，行为与不启用完全一致（零回归）。不修改任何候选 weight；引擎层 auto_commit 决策在词频重排之前完成，天然不受影响（无 Go 版 ProtectTopN 致顶码与 UI 不一致的隐患）。与 `top`/`step` 策略正交——两种策略均在保护回填之前完成排序。仅码表/混输路径（`rerank_codetable_usedfirst`）生效，拼音路径固定为 `ProtectPolicy::NONE`。
- **protect 按输入码长分级**（后续增强，设计见 `docs/design/codetable-freq-short-code-protection.md`）：单一标量表达不了「简码位要护、全码位要放」这对相反诉求——取 1 则全码位首选永久锁死、调频只对第 2 位以后有效；取 0 则五笔一简二简当场失守（一简 25 个码每个都是二选一，词库靠 9999/9998 表达的钦定次序在本模块**完全失效**，因为比较链不含 weight）。现改为 `ProtectPolicy { by_len: [1,1,0], fallback: 0 }`，配置键 `protect_top_n_len{1,2,3}` + `protect_top_n`（兜底）。同时保护名额**只在精确档内取**（`is_exact_code`），修掉「名额多于精确候选时把前缀补全词钉死」的旧缺陷。
