# 引入语言模型（n-gram）的可行性分析

**状态**：调研完成，**未实施**。本文是开工前的事实底稿与方案建议。
**目标读者**：准备给拼音引擎接上下文模型的人。
**前置阅读**：`docs/design/sentence-weight-same-axis.md` §7（与参考实现的架构对照）。

## 1. 为什么要做

我们的整句解码只有 **unigram（词频）**，没有上下文。以下问题反复出现、且都**无法在现有框架内根治**：

| 现场 | 现状 | 根因 |
|---|---|---|
| `sixiang` → 「是想」压过「思想」 | 靠 step 6.5 结构性让位硬压 | 无上下文，分不出哪个解读合理 |
| `nih` → 「你会」压过「你好」 | 已诊断、归因词库语料是书面语，**搁置不修** | 同上；选 1 次即自愈，故容忍 |
| `sishi` → 模糊「事实/试试」淹没精确「四十」 | 模糊惩罚 `0.5^k`（对齐参考实现）后仍如此 | 单字场景没有上下文可依 |
| 整句质量整体 | 靠各类惩罚常数手工标定 | — |

⇒ **这是我们与 librime / libime 最后的实质差距。** 前几轮把量纲、饱和、模糊惩罚逐个对齐之后，
剩下的差距集中在这里。

## 2. 参考实现怎么做的（2026-08-10 实读源码）

### 2.1 librime：Grammar 接口只有一个虚函数

`src/rime/gear/grammar.h` 全文核心：

```cpp
class Grammar : public Class<Grammar, Config*> {
 public:
  virtual double Query(const string& context, const string& word, bool is_rear) = 0;

  inline static double Evaluate(const string& context,
                                const string& entry_text,
                                double entry_weight,
                                bool is_rear,
                                Grammar* grammar) {
    const double kPenalty = -18.420680743952367;  // log(1e-8)
    return entry_weight +
           (grammar ? grammar->Query(context, entry_text, is_rear) : kPenalty);
  }
};
```

两个性质值得注意：

1. **语义是 `词条对数权重 + 上下文对数分`** —— 纯加法，因为全在对数域。
2. **无 grammar 时退化成固定 `log(1e-8)`**，即「没有模型就给所有词一个统一的低分」，
   调用方无需分支。

接入点在 `gear/poet.cc` 的路径扩展：

```cpp
double weight = candidate.weight +
                Grammar::Evaluate(context, entry->text, entry->weight, is_rear, grammar_.get());
```

### 2.2 libime：直接用 KenLM

`core/languagemodel.cpp:30` `#include "lm/model.hh"`。抽象层是
`LanguageModelBase`（`core/languagemodel.h`），关键虚函数：

```cpp
virtual float score(const State &state, const WordNode &word, State &out) const = 0;
```

注意它带 **State**（`std::array<char, 20 + sizeof(void*)>`）——KenLM 的上下文状态。
这是 n-gram 解码的标准形态：**转移函数需要携带前文状态**。

`core/userlanguagemodel.cpp:139` 还有一层用户模型插值：
`max(score, sum_log_prob(score + wa, userScore + wb))`，`w=0.2`。

## 3. 许可格局（决定方案的关键）

本仓是 **MIT**。

| 组件 | 许可 | 对我们 |
|---|---|---|
| librime（Grammar 接口设计） | BSD-3 | ✅ 自由借鉴 |
| **librime-octagram（插件代码）** | **BSD-3** | ✅ **可自由移植/参考实现** |
| rime-octagram-data（模型数据） | **LGPL-3.0** | ⚠️ 可分发，但须附许可文本、保持可替换、来源可溯 |
| libime | LGPL-2.1+ | ⚠️ 不宜链接 |
| KenLM | LGPL-2.1 | ⚠️ Rust 静态链接麻烦，不宜 |

⇒ **代码走 BSD-3 路线自己实现，不链接任何 LGPL 库**，是唯一无负担的选择。
数据另议（见 §5）。

## 4. 我们要改什么

### 4.1 打分接入：小

我们的 `pinyin/lattice.rs::score_node` **本来就是对数域**
（`ln(weight / DICT_TOTAL)` 加各类惩罚），与 librime 的 `entry_weight` 同构。
加一项上下文分即可，形态与 `Grammar::Evaluate` 一致。

### 4.2 ★ Viterbi 状态扩展：大，这是真正的工作量

现在的转移只看当前节点：路径分 = 各节点 `log_prob` 之和，节点之间**无条件依赖**。
bigram 要求转移函数携带「前一个词」，于是：

- 词图节点的 DP 状态从「位置」变成「位置 × 前驱词」
- 状态数与词表大小相关，需要 **beam search 剪枝**（libime 有 `beamSize`，我们没有）
- 现有的 `ViterbiDecoder` 结构要重做

⚠️ **这一步会动整句解码的核心**，前几轮所有关于整句 weight 的结论（几何平均、
`consumed_length` 兜底、6.5/6.5b 的让位）都建立在当前解码结构上，须重新验证。

### 4.3 存储：中等，但有现成骨架

复用 `wind-dict` 的「mmap + 内容指纹缓存」骨架（`datformat.rs` / `commentdict.rs` /
`cache_fp.rs` / `reader_pool.rs` 已跑熟）。n-gram 表是「键 → 分数」的点查，
与 `.wcmt` 注释表同构（排序数组 + 二分），不需要 DAT。

内存是要害：模型大小直接变成常驻内存或 mmap 页。需实测。

## 5. 数据方案

| 方案 | 优点 | 代价 |
|---|---|---|
| **A. 用 rime-octagram-data** | 质量已被 rime 社区验证，立刻可用 | LGPL-3.0：须做成**可选下载的独立数据文件**（不进主安装包），附许可文本与来源说明，保持用户可替换 |
| **B. 自训练** | 无许可负担 | 语料许可要先确认（Wikipedia 是 CC-BY-SA）；训练管线是新工程 |

建议 **A 先行**（把接口做对、验证收益），B 作为后续。分离得干净的话，两者可共存
——接口一样，换数据文件即可。

## 6. ⚠️ 尚未查证（开工第一件事）

- **`.gram` 的实际格式**：阶数、是否 Kneser-Ney 平滑、二进制布局。WebFetch 只拿到
  仓库首页，**没读到 `librime-octagram/src/`**。这决定我们是复用格式还是自定义。
- rime-octagram-data 的**模型规模**（文件大小 / 条目数）与查询延迟。
- `collocation_max_length` / `collocation_min_length` / `contextual_suggestions`
  这几个 schema 选项的确切语义（`rime-frost/others/语言模型相关/rime-octagram-data.txt`
  里有配置样例，但只有配置没有解释）。

## 7. 建议的第一步

**不要直接开做**。先产出一份可行性评估：

1. 读 `librime-octagram/src/`，确定 `.gram` 格式与 Grammar 的实际接入点
2. 量出模型文件大小与预期内存占用
3. 评估我们 `ViterbiDecoder` 改成带状态解码的改动量

这三件事做完才谈得上排期。**§4.2 的 Viterbi 状态扩展是整个方案的成本重心**，
它没摸清之前，任何工期估计都是猜的。
