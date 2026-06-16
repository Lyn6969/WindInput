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

## 3. 码表方案：词频为可选主排序维度
用户可选 `candidate_sort_mode`（方案/用户级配置）：
- `weight`：按词库权重（默认）。
- `natural`：自然/字根序（inner_order，对应 dict.md 的 WeightMode）。
- `frequency`：**用户词频优先**。

`frequency` 模式的排序规则（"used-first"）：
1. 有词频数据的候选**整体优先**，按词频度量排序；
2. 无词频数据的候选**回退**到默认 `weight`/`natural` 排序，排在已用词之后。

排序键：`(has_freq?1:0 desc, freq_metric desc, weight desc, natural asc)`。
- 码表 `freq_metric`：以 **count 为主**（确定性——"这个码我选了这个词 N 次，置前"）；可选用 last_used 作轻量 tiebreak（同次数时近用优先）。
- 效果：**用一次即可靠上浮**，不受 weight 量级压制——修复 Go 痛点。

> 注：码表"used-first"是用户明确意图（"优先使用用户词频排序，没数据时用默认"）。
> 不混算 weight 与 count（两者量纲不同），用分区/分层实现。

## 4. 拼音方案：次数 + 最近时间 + 衰减
拼音候选多、有整句组合，**不宜硬 used-first**（会破坏长句质量）。词频按**衰减分**参与排序：
```
age_hours = (now - last_used) / 3600
freq_score = base_scale * log2(count + 1) * decay
decay      = exp(-ln2 * age_hours / half_life)     // 半衰期衰减，最近用过≈1，久未用→0
```
- 最近+高频 → freq_score 高，候选上浮；久未用 → 衰减回落，自然让位。
- **关键差异（相对 Go）**：freq_score 在**归一化分数层**与引擎基础质量分结合，**不是加到原始大 weight 上**。引擎 RimeScorer 的基础质量分本就是受控小范围（见 engine.md §1.2），freq_score 按可比量级叠加 → **真正能重排**。
- 组合方式：`final = base_quality + freq_score`（base_quality 来自 RimeScorer 的词库质量分，与 LM/initialQuality 同源、同量级）。具体系数实现期调参，目标是"高频近用词稳定靠前、但不压垮整句最优解"。

## 5. 数据与排序的位置（修正 dict/engine 旧设计）
- **存储**（store）：freq table `{count, last_used}`，按 (schema, code, text)。异步批量写保留。提供 `freq_lookup(schema, code, text) -> Option<{count,last_used}>` 与 `record_selection`。
- **排序阶段应用**（engine 排序层，**非 dict 查询时**）：
  - dict 的 CompositeDict 只负责**合并各层候选**（system + 用户词 + temp + phrase），产出带 weight 的候选集——**取消"查询时把 FreqBoost 加到 dictWeight"**（dict.md 旧设计作废）。
  - engine 排序层（持有 store 的 freq 只读访问）按**方案类型**应用词频维度：码表按 §3，拼音按 §4。然后截断。
- 即词频从 Go 的"查询时改权重"彻底变为"**排序时独立维度**"。

> 区分两个易混概念：**用户词**（用户造的词，是 dict 的一个 layer，有自己的 weight）≠ **用户词频**（对任意候选的使用统计，是排序维度）。本文只讲后者；前者仍按 dict.md 的 store_layer。

## 6. 配置（修正 config-schema 旧 FreqSpec）
- 码表：`candidate_sort_mode: weight | natural | frequency`（已在 CodeTableSpec）。
- 拼音衰减参数（FreqSpec 仅保留衰减相关）：`half_life`（默认 72h）、`base_scale`、可选 `recency_peak`。
- **删除**旧的 boost-to-weight 语义字段（BoostMax / StreakScale / StreakCap 等"加权上限/连击"参数——属旧模型）。
- 词频默认值改为**单一真值源**（store 提供默认 + schema 可覆盖），消除 store.md 指出的"两套默认源不一致"。

## 7. 落地（并入阶段 B store/engine）
1. store：freq table 改为 `{count, last_used}`，去 streak/boost；提供 lookup + record。
2. engine 排序层：新增**词频重排步骤**（码表 used-first 分区；拼音衰减分叠加），持 store freq 只读访问。
3. dict：composite 去掉查询时 FreqBoost，只合并层。
4. config：CodeTableSpec.candidate_sort_mode 加 frequency；FreqSpec 改衰减参数。

> 受影响差分均已加指针指向本文。每步 `wind_input/scripts/dev.sh ci` 把关。
