# 用户词频（调频）机制：开源实现调研

> **目的**：本项目拼音侧的词频重排存在「用过一次即无条件置顶」的问题（打 `d` 首选变成
> 用过一次的「的样子」）。本文调研三个开源输入法的做法，为重新设计提供依据。
>
> **调研对象**：`ref/weasel`（librime）、`ref/Yzime`、`ref/fcitx5-android`。
>
> ⚠️ **fcitx5 未能分析**：`fcitx5-android` 的 git submodule 全部未初始化
> （`git submodule status` 显示 `-` 前缀），`lib/libime/src/main/cpp/libime/` 为空目录。
> fcitx5 的词频核心（libime 的 `HistoryBigram` / `UserLanguageModel`）不在本地。
> 若需补齐，须先 `git submodule update --init --recursive`。本文结论仅基于 librime 与 Yzime。

---

## 1. 我们的现状（问题基线）

`wind-engine/src/freq_rerank.rs::rerank_pinyin_decay` 的比较链：

```
① 整句 / 精确码短语        → 锚定顶部
② cmp_match_layers        → 布尔层级（模糊/前缀/子短语），词频不得跨层提拔
③ used-first              → 布尔闸门：score >= PINYIN_FREQ_EPSILON(10.0) 即胜
④ 衰减分降序
⑤ 都没用过                → 才轮到引擎权重序
```

**缺陷在 ③ 排在 ⑤ 之前且是布尔的**：一旦「用过」，就跨越了权重轴。实测用户数据
`deyangzi 的样子 count=1` 即可压过 `的`（weight ≈ 1.18e7）。

码表侧（`rerank_codetable_usedfirst`）另有 `ProtectPolicy` 按输入码长保护首选 N 位，
拼音侧**没有任何对应机制**。

---

## 2. librime（RIME / 小狼毫）

### 2.1 一切都是对数概率

librime 没有「层级」概念，所有维度都折算成**同一量纲的对数概率**并相加：

`src/rime/dict/dictionary.cc:74`（候选比较）：
```cpp
return a.credibility + a.entries[a.cursor].weight >
       b.credibility + b.entries[b.cursor].weight;   // by weight desc
```

`src/rime/dict/dictionary.cc:154`（系统词条权重）：
```cpp
const double kS = 18.420680743952367;  // log(1e8)
entry_->weight = e.weight - kS + chunk.credibility;
```

各类「质量折扣」全是 log 空间的加性惩罚（等价于概率相乘）：

| 常量 | 值 | 语义 |
|---|---|---|
| `kAbbreviationPenalty` | `log(0.5)` | 简拼 → 概率减半 |
| `kFuzzySpellingPenalty` | `log(0.5)` | 模糊音 → 概率减半 |
| **`kCompletionPenalty`** | `log(0.5)` | **前缀补全 → 概率减半** |
| `kPenaltyForAmbiguousSyllable` | 计算得出 | 歧义音节 |
| `kCorrectionCredibility` | `log(0.01)` | 纠错候选 → 概率 1% |

> **对照本项目**：我们把模糊/简拼/补全做成 `cmp_match_layers` 的**布尔层级**，
> 等价于「惩罚 = ∞」。仓库此前已就模糊音踩过一次这个坑并改为权重惩罚
> （见 `project_fuzzy_pinyin_layer_vs_penalty` 的教训：布尔层级键只配结构性质量差异，
> 「来源」一律走 weight）。**词频的 used-first 是同一个坑的另一处未修版本。**

### 2.2 用户词频也折算进同一量纲

`src/rime/dict/user_dictionary.cc:544`：
```cpp
double weight = algo::formula_p(0, (double)v.commits / present_tick,
                                (double)present_tick, v.dee);
e->weight = log(weight > 0 ? weight : DBL_EPSILON) + credibility;
```

用户词频先算成 `[0,1]` 概率，再 `log` 转入与系统词**完全相同**的量纲，最后同样加
credibility。**用户词与系统词直接可比，没有任何硬闸门。**

### 2.3 两个核心公式

`src/rime/algo/dynamics.h`：

```cpp
// 衰减累积：d=本次新增, t=当前 tick, da=旧的 dee, ta=上次 tick
inline double formula_d(double d, double t, double da, double ta) {
  return d + da * exp((ta - t) / 200);
}

// 概率估计：s=系统概率, u=用户频率, t=tick, d=dee
inline double formula_p(double s, double u, double t, double d) {
  const double kM = 1 / (1 - exp(-0.005));       // ≈ 200.5
  double m = s - (s - u) * pow((1 - exp(-t / 10000)), 10);
  return (d < 20) ? m + (0.5 - m) * (d / kM)
                  : m + (1 - m) * (pow(4, (d / kM)) - 1) / 3;
}
```

**`formula_p` 的关键性质**：提升幅度由 `d / kM` 驱动，而 `kM ≈ 200`。

| dee | `d/kM` | 提升 |
|---|---|---|
| 1（用过一次） | ≈ 0.005 | **≈ 0.5% 的区间，几乎不动** |
| 20 | ≈ 0.1 | 进入指数段起点 |
| 200 | ≈ 1.0 | 显著提升 |

> **这是与我们最尖锐的差异**：`commits=1` 在 librime 里几乎不改变排序，
> 在我们这里直接置顶。

`formula_d` 则给出**按 tick 距离的指数衰减**（时间尺度 200），久未使用的词自然褪色。

---

## 3. Yzime（影子输入法，AutoHotkey）

Yzime 直接改词库里的 `weight` 字段（SQLite），逻辑集中在 `Lib/srf_func.ahk:109-130`。

### 3.1 调频公式：渐进逼近而非置顶

```ahk
If (weight := Tofirst
    ? Round(selectvalue[4]+1)                                  // 一次到顶（可选）
    : Round(selectvalue[3]+1+(selectvalue[4]-selectvalue[3])/分母))   // 默认：渐进
```

其中 `selectvalue[3]` = 该候选当前权重，`selectvalue[4]` = 该编码下的最高权重，
`tt[2]` = 候选数量，分母为：

```ahk
tt[2] < 5 ? Max(tt[2]-1, 1) : (2*tt[2]-6)/(tt[2]-4)
```

| 候选数 | 分母 | 单次提升 |
|---|---|---|
| 2 | 1 | **直接到顶**（二选一，意图明确） |
| 3 | 2 | 与最高权重差距的 1/2 |
| 5 | 4 | 1/4（最慢） |
| 10 | ≈2.33 | ≈43% |
| ∞ | → 2 | ≈50% |

**默认需要多次使用才能到顶**，「一次到顶」(`Tofirst`) 是**可选项**。

### 3.2 「连续两次选同一个词」才直接到顶

```ahk
If (srf_last_input[2, 2] = selectvalue[2]){
    weight = 1 + (SELECT max(weight) ... )    // 直接置顶
}
```

把「用户刚刚才选过、现在又选」当作明确意图信号，此时才一步到位。

### 3.3 三个关键的可选约束

| 选项 | 语义 | 对我们的价值 |
|---|---|---|
| **`fixedword`（字频固定）** | 单字（`selectvalue[2]~="^.$"`）**不参与调频** | **直接对应我们的问题**：保护高频单字不被词组挤掉 |
| `decfre`（重码词降频） | 选中多字词时，同码其他词 `weight -= 5` | 主动压制竞争者，而非只抬升选中者 |
| `Tofirst`（一次到顶） | 跳过渐进，直接置顶 | 我们当前行为 ≈ 恒开此项 |

### 3.4 调频总开关默认关闭

`Wordfrequency` 是总开关，关闭时上述三个子选项全部禁用。这与你提到的
「码表那边默认关闭所以没问题」一致。

---

## 4. 横向对比

| 维度 | 本项目（拼音） | librime | Yzime |
|---|---|---|---|
| 词频的表达 | **布尔 used-first** | log 概率，与系统词同量纲相加 | 直接改词库 weight |
| `count=1` 的效果 | **无条件置顶** | 几乎不动（`d/kM≈0.005`） | 提升与最高权重差距的 1/n |
| 一步到顶的条件 | 立即 | dee 需累积到 ~200 量级 | 连续两次选同一词，或候选数=2 |
| 补全 / 模糊 / 简拼 | **布尔层级**（惩罚=∞） | `log(0.5)` 加性惩罚 | — |
| 单字保护 | 无 | — | **`fixedword` 选项** |
| 压制竞争者 | 无 | — | `decfre`（同码 -5） |
| 时间衰减 | 有（`PinyinFreqProfile`） | `formula_d`，尺度 200 tick | 无 |
| 默认开关 | **开** | 开 | **关** |

**没有任何一个参考实现使用布尔闸门。** 两者都让词频与词库权重在**同一个可比量纲**上竞争，
区别只在于 librime 用概率模型、Yzime 用直接改权重的启发式。

---

## 5. 可借鉴的方案（按改动成本排序）

### 方案 1：单字保护（最小，立即可做）

借鉴 Yzime 的 `fixedword`。词频重排时，**单字候选不被多字候选跨越**，或更弱的版本：
输入长度 ≤ N 时保护首选。

- 成本：极小，只在比较链里加一道判据
- 覆盖：直接解决「打 `d` 出「的样子」」
- 局限：治标；`qu`→`去` 这类单字调频仍正常工作

### 方案 2：补全惩罚（对齐 librime 的 `kCompletionPenalty`）

前缀补全候选参与词频竞争时施加固定折扣，而不是靠布尔层级隔离。可与现有
`should_promote_user_completion` 的距离判据结合：距离越远，折扣越大。

- 成本：中等，需定标折扣量级
- 覆盖：解决整类「远距离补全越权」

### 方案 3：把 used-first 换成加权分（最彻底，对齐 librime）

取消 ③ 的布尔闸门，改为 `最终分 = f(引擎权重) + g(词频)`，两者同量纲。
`g` 参考 `formula_p` 的形状：`count=1` 只给微小加成，需累积才显著。

- 成本：大。需要把引擎权重（当前是 `i32`，跨度 0~1.9e7）与词频分标定到同一量纲，
  且必须有评测支撑（`pinyin_eval` 的 A/C 类可作为回归基准）
- 收益：一次性解决「词频绕过权重轴」这个根因，且与仓库既有的
  「模糊音改用 weight 惩罚」方向一致

### 方案 4：渐进提升（借鉴 Yzime）

不再让「用过」直接生效，而是每次使用把候选权重向「同码最高权重」推进一个比例，
连续两次选同一词才一步到顶。

- 成本：中等，但**需要可写的权重存储**——我们当前 `apply_freq_rerank` 刻意
  「绝不改 weight」，改这条等于改变词频的存储模型
- 注意：与我们现有的「词频是独立维度、不回写 weight」的架构冲突，需先决定是否接受

---

## 6. 待确认的问题

1. 是否接受「词频与权重同量纲加权」这个方向（方案 3）？它最彻底，但需要标定与评测。
2. 若暂时只做止血，方案 1 + 2 的组合是否足够？
3. `count` 的语义要不要引入「连续选中」信号（Yzime 的做法）？
4. 拼音侧是否也需要一个「默认关闭」或「保护首选 N 位」的开关（对齐码表侧的
   `ProtectPolicy`）？
5. 是否补齐 fcitx5 的调研（需先初始化 submodule）？libime 的 `HistoryBigram`
   是基于 n-gram 语言模型的用户历史，与上述两者路线不同，可能带来第三种思路。
