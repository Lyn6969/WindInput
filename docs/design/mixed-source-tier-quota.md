# 混输：拆掉权重加成，改为「来源分组 + 配额截断」

**状态**：设计中，**未实施**。
**目标读者**：改混输候选生成/截断的人。
**前置阅读**：`docs/architecture/candidate-sorting-rules.md` 红线①②③、
`wind_candidate::source_tier` 的文档注释。

## 1. 要解决什么

混输引擎把「跨来源优先级」**编码进 weight 数值本身**：

| 常数 | 值 | 作用对象 |
|---|---|---|
| `PHRASE_WEIGHT_BOOST` | +1,000,000 | 短语 |
| `codetable_weight_boost` | +10,000,000（方案可配） | 码表精确全码 |
| `PARTIAL_MATCH_BOOST` | +500,000 | 码表前缀补全 |
| `ENGLISH_EXACT_BOOST` | +500,000 | 英文精确 |
| `ENGLISH_PREFIX_BOOST` | +0 | 英文前缀 |
| `PINYIN_TIER_SCALE` | **÷100** | 拼音全部 |

后果（`candidate-sorting-rules.md` 红线①「跨来源比 weight 无意义」正是在描述它）：

- 真实词频与类别偏置挤在同一个 `i32` 里，量程被吃掉；拼音 p50=34 被 ÷100 **整除归零**
- 同一个词在纯拼音与混输下量纲差 100 倍
- 「拼音精确档」这类补丁的成因就是「拼音被一律 ÷100 压档」，是**一处实现在修另一处实现造成的问题**

协调器侧的档位已经收敛为 [`wind_candidate::source_tier`]（已实施、已真机）。本文档处理剩下的
引擎侧。

## 2. 关键事实（已查证，决定了方案边界）

### 2.1 ✅ 自动上屏 / 清空**不依赖 weight**

`MixedEngine::convert` 的两个判定：

```rust
// should_commit
ct_should_commit && !ct_commit_text.is_empty() && !has_english
    && !self.pinyin_vetoes_commit(input, has_pinyin)
    && merged.iter().any(|c| c.text == ct_commit_text)   // ← 存活性，不是权重

// should_clear
ct_should_clear && !(auto_commit_block_on_pinyin && (has_pinyin || pinyin_may_continue(input)))
```

全部是**存在性/存活性**判定。⇒ 拆加成不会动自动上屏与清空，**只要候选还在列表里**。
风险因此收敛到唯一一处：**截断**。

### 2.2 ⚠️ `source_tier` 在引擎侧用不了

档 2 判据 `is_pinyin_exact_tier` 要求 `c.is_common`，而 `is_common` **只在协调器
`mark_common` 置位**，引擎产出的候选恒为 `false`。在引擎里调 `source_tier`，拼音精确候选会
**静默落到档 4**。

> `CommonChars` 类型本身在共享 crate `wind-candidate` 里，技术上可以下放给引擎。但**本设计
> 不这么做**——见 §3.3。

### 2.3 截断现状

`sort_dedup_truncate`：**全局按 weight 降序** → 按 text 去重（`absorb_codes_from` 并码位）
→ `truncate_with_pinyin_quota`（拼音保底 `max/5` 席）。

加成正是让这个全局排序产出正确截断优先级的原因。直接删加成，拼音「的」15,378,475 会压过
一切码表候选，截断结果整个翻掉。

⚠️ 实测量级：**52 个五笔 2 码前缀的候选数 > 300**（`kh` 663 条、`pu` 495 条），而生产
`max_candidates = 300`。截断不是边缘情况，是常态。

## 3. 方案：职责分离

> **引擎负责「谁进得来」，协调器负责「谁排前面」。**
> 这句话 `truncate_with_pinyin_quota` 的文档里已经写了（「本函数的职责只是让候选进得来，
> 不是排好序」），本设计只是让代码真正照它执行。

### 3.1 引擎：不再改 weight

删除全部六个加成/降档常数的施加点。`weight` 从此**只承载真实词频**。

### 3.2 引擎：排序与截断改为「来源分组 + 配额」

```text
1. 按来源分组：codetable / pinyin / phrase / english
2. 组内按 weight 降序（真实词频，同源可比 —— 这是唯一成立的比较）
3. 按配额从各组取：
     phrase   全取（量本来就极少）
     english  全取或限量
     pinyin   保底 max / PINYIN_QUOTA_DIVISOR
     codetable 取剩余
4. 合并（组间顺序无所谓 —— 协调器 `candidate_display_order` 会无条件重排全部候选）
```

关键性质：**不需要跨来源可比的 weight**。组内比较是同源的，配额是显式的。

### 3.3 为什么不把 `is_common` 下放给引擎

那样确实能让引擎直接用 `source_tier`，但：

1. **职责错位**：`is_common` 是「检索范围」这个**用户设置**的产物（`filter_mode`），属于展示层
   决策。引擎不该为了排序去理解用户的检索范围。
2. **两份加载**：协调器已持有一份 `CommonChars`，引擎再加载一份是内存浪费 + 潜在不一致；
   注入则要给 `EngineManager` 增加一条与转换无关的依赖。
3. **没必要**：引擎需要的不是「档位」而是「配额」，配额不需要 `is_common`。

⇒ 保持 `source_tier` 是**协调器侧唯一真相源**，引擎侧只做分组配额。两者职责不重叠，
因此不构成「第四份并行实现」。

### 3.4 去重的顺序要重新定

现状：全局排序 → 去重（保留排序后第一条）。加成保证了「同 text 时码表那条在前」。
分组后这个隐含保证消失，必须**显式定义跨来源同 text 的保留优先级**（建议：码表 > 短语 >
拼音 > 英文，与 `source_tier` 的档序一致）。

⚠️ `absorb_codes_from` 跨来源本就会挡掉码位并入（两套编码不同域），所以这里只是「保留哪条」
的问题，不涉及码位合并语义。

## 4. 风险与验证

| 风险 | 判据 |
|---|---|
| 截断后某来源候选整片消失 | 混输探针覆盖 `kh`/`pu`/`xu`/`da` 这类高冲突码，逐条比对各来源候选数 |
| 自动上屏/清空行为变化 | §2.1 已证不依赖 weight，但仍须跑 `input_flow` 的满码上屏/清空用例族 |
| 同 text 跨来源保留错条 | 新增用例：同一个词既在码表又在拼音词库时，保留的 `source` 与 `code` |
| 纯码表 / 纯拼音回归 | **必须零差异**——它们不走混输路径 |

**不变量**：纯码表、纯拼音方案零差异。任何差异都说明改动泄漏出了混输路径。

## 5. 分步实施

1. **先补测试**：给现有截断行为补齐用例（各来源候选数、同 text 保留谁），**在改动前锁住现状**。
   现有用例只覆盖了拼音配额，码表侧没有。
2. 引擎内部改分组 + 配额截断，**加成暂时保留**。此步应零差异（配额与加成产出同样的集合）
   —— 有差异就说明配额参数没配平。
3. 删加成。此步 weight 数值大变，但**候选集合**应与上一步一致，变的只有引擎内部顺序
   （协调器会重排，故最终显示序不变）。
4. 回收常数与 `normalize_pinyin`，更新 `candidate-sorting-rules.md` 红线①③。

⚠️ 第 2、3 步分开做是关键：第 2 步验证「配额能复现加成的集合」，第 3 步验证「删加成不影响
集合」。合并做则出问题时分不清是配额没配平还是别的。

## 6. 后续可选：来源优先级配置化

librime 的跨来源偏置是**可配置的一等公民**（`translator/initial_quality`，见
`sentence-weight-same-axis.md` §7.4）。本设计把偏置从 weight 里拆出来之后，
「各来源配额 / 档序」就具备了配置化的条件——用户可以调「码表 vs 拼音谁优先」而不必碰词频。

**不在本次范围内**，但方案不应堵死这条路：配额值与档序建议留成常量表而非散落的魔数。
