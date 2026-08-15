# 引入语言模型（n-gram）的可行性分析

**状态**：**P0~P3 全部完成**（2026-08-11）。功能已实现并接入配置
（`[schema.pinyin.grammar]`），`pinyin_eval` 在默认配置下全程与基线逐位相同。
**但默认 `weight = 0`（关闭）**。

⚠️ **不要因为「做完了」就默认打开**。同环境实测：收益是 50 条真实整句上 **+1/50**
（只有一条样本被修好，统计上不显著），代价是**解码慢 2.54 倍**、且 `pinyin_eval`
的 D 类（简拼混合长句）top-1 掉 2.10 个百分点。详见 §7 的 P3。
要开启：`model = zh-hans-bgw.gram`（词级，比字级 bgc 稳健得多）、`weight = 0.2~0.3`。

**关闭时零开销**：`weight = 0` 走独立的单状态 DP 路径，与接模型之前逐字节同构（§7.5）。

**下一步**（按价值排序）：① 用更大规模的真实输入语料复测，才谈得上默认开启；
② 开启路径的性能优化；③ `preceding_text`（§4.4，句首上下文，独立工作线）。

工具：`scripts/lm/gram.py`（`.gram` 解析）、`scripts/lm/bigram_coverage.py`（命中率评估）、
`tests/grammar_sentence_eval.rs`（整句标定，支持 `WIND_GRAM_WEIGHT` / `WIND_GRAM_MODEL`）。
**目标读者**：准备给拼音引擎接上下文模型的人。
**前置阅读**：`docs/design/sentence-weight-same-axis.md` §7（与参考实现的架构对照）。

## 1. 为什么要做

我们的整句解码只有 **unigram（词频）**，没有上下文。以下问题反复出现、且都**无法在现有框架内根治**：

| 现场 | 现状 | 根因 |
|---|---|---|
| `sixiang` → 「是想」压过「思想」 | 靠 step 6.5 结构性让位硬压 | 无上下文，分不出哪个解读合理 |
| `nih` → 「你会」压过「你好」 | 已诊断、归因词库语料是书面语，**搁置不修** | 同上；选 1 次即自愈，故容忍 |
| `sishi` → 模糊「事实/试试」淹没精确「四十」 | 模糊惩罚 `0.5^k`（对齐参考实现）后仍如此 | 单字场景没有上下文可依 |
| `lianzhengtixing` / `liandaoyan` 二选一 | `AMBIGUOUS_PENALTY` 卡在 0.35 这个刀刃值 | 见下 |
| 整句质量整体 | 靠各类惩罚常数手工标定 | — |

`AMBIGUOUS_PENALTY` 那条尤其值得点名：`lattice.rs` 的注释里已经写死了结论——
0.30~0.35 之间**聚合指标完全不变**，只有那两个定点在翻转，因为它们是同一个词、
同一个 `li|an` 拆分、同一个歧义接缝，**切分层不存在可区分二者的信息**。
这是一笔明确记在账上的技术债，接上 bigram 才还得掉。

⇒ **这是我们与 librime / libime 最后的实质差距。** 前几轮把量纲、饱和、模糊惩罚逐个对齐之后，
剩下的差距集中在这里。

## 2. 参考实现怎么做的

### 2.1 librime：Grammar 接口只有一个虚函数

`ref/weasel/librime/src/rime/gear/grammar.h` 全文核心：

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

### 2.2 ★ librime-octagram 实读（2026-08-11，源码已入 `ref/librime-octagram`）

插件全部代码只有 ~15 KB（7 个源文件），BSD-3。以下是逐行读完后的事实。

#### 2.2.1 它不是「词 bigram」，是「字串搭配表」

这是**最重要的一条，且推翻了本文旧版的隐含假设**。`octagram.cc:107-162` 的 `Query`：

```cpp
int n = min(kMaxEncodedUnicode /*8*/, collocation_max_length - 1 /*3*/);
string context_query = encode(last_n_unicode(context, n, context_len), str_end(context));
string word_query    = encode(str_begin(word), first_n_unicode(word, n, word_query_len));

for (const char* context_ptr = str_begin(context_query);
     context_len > 0;
     --context_len, context_ptr = next_unicode(context_ptr)) {
  int num_results = db_->Lookup(context_ptr, word_query, matches);
  ...
}
```

- context 取的是**前文最后 ≤3 个 Unicode 字**，word 取的是**当前词开头 ≤3 个字**；
- 外层循环把 context **从最长逐字缩短**，每轮查一次，`update_result` 取**最大分**；
- 查表是 `traverse(context)` 定位节点后，对 word 做 `commonPrefixSearch`，
  一次最多返回 `kMaxResults = 8` 个不同长度的匹配（`gram_db.cc:105-116`）。

⇒ 键空间是「≤3 字前文 + ≤3 字后词」的**字级搭配**，不是词到词的转移概率。
这解释了数据仓两个变体的命名：`bgc` = bigram character，`bgw` = bigram word。

#### 2.2.2 存储：单棵 double-array trie，值是定点对数

`gram_db.h` / `gram_db.cc`：

```cpp
static constexpr int kMaxResults = 8;
static constexpr double kValueScale = 10000;
// Build:
values.push_back(max(0, int(log(kv.second) * kValueScale)));
// Query 侧:
inline static double scale_value(int value) {
  return value >= 0 ? double(value) / GramDb::kValueScale : -1;
}
```

- 整个模型就是**一棵 Darts double-array trie**，键是编码后的 `context+word` 拼接串，
  值是 `log(v) × 10000` 取整；
- 文件布局：`Metadata{ char format[32]; uint32 db_checksum; uint32 double_array_size;
  OffsetPtr<char> double_array; }` + double-array 镜像，`format` 串是 `"Rime::Grammar/1.0"`；
- 加载就是 `mmap` 后 `trie_->set_array(array, array_size)`（`gram_db.cc:36-45`），
  **零解析、零反序列化**。

⚠️ `max(0, ...)` 意味着 `log(v) < 0`（即 `v < 1`）的条目**全部被截成 0**。
所以训练输出的 `value` **不是概率**，必是某个 > 1 的量。具体是什么，见下节实测。

#### 2.2.3 ★ 实测 dump：`zh-hans-t-essay-bgc.gram`（2026-08-11）

`.gram` 是二进制、数据仓不含源 `.txt`，所以直接写了个解析器把成品文件拆开，
用「先 `traverse(context)` 再从该节点 `commonPrefixSearch(word)`」查真实搭配，
**8/10 命中**（「的+时候」「一+个」「没+有」等），据此确认格式、编码、查询三者都读对了。

> 解析器已入库：**`scripts/lm/gram.py`**（既是库也是自检 CLI，
> `python scripts/lm/gram.py <file.gram>` 会打印 metadata、搭配探针与全表分布）。
> 位域运算的出处是下方 (a)——**若脚本与本文档不一致，以实测为准并更新文档**。

**a. 是 darts-clone，unit 为 4 字节**（不是原版 Darts 的 8 字节 `{int base; uint check;}`）。
判据：`double_array_size × 4 = 10,397,696` 字节，与文件实际可用字节（`文件大小 − 44`）
**精确相等**。darts-clone 把 base/check 打包进一个 `uint32`，位域如下（重建解析器必需）：

```text
has_leaf(u) = (u >> 8) & 1
value(u)    = u & 0x7FFFFFFF          // 仅叶单元有意义
offset(u)   = (u >> 10) << ((u & 0x200) >> 6)
label(u)    = u & 0x800000FF
// traverse：id ^= offset(unit) ^ 当前字节；随后校验 label(array[id]) == 该字节
// 取值：  leaf = id ^ offset(unit); value(array[leaf])
```

`Metadata` 共 44 字节（`format[32]` + `db_checksum` + `double_array_size`
+ `OffsetPtr` 各 4 字节），double-array 镜像自 offset 44 起直到文件尾。

**b. `value = ln(频次) × 10000`**，自然对数（C++ `log()` 即 ln）。取值分布：

| 分位 | value | ln 域 | 还原频次 |
|---|---|---|---|
| p1 | 69,216 | 6.92 | **1,014** |
| p50 | 83,428 | 8.34 | 4,200 |
| p95 | 119,090 | 11.91 | 1.49e5 |
| p99.99 | 184,934 | 18.49 | 1.08e8 |

反证 log10 假设：那样频次上限会是 8.75e29，任何真实语料都不可能 ⇒ **确认是 ln**。

**c. 训练侧有频次下限 ≈ 1000**。p1 就已经是 1014 次，且全表 150 万条里
`value == 0` 的**只有 1 条**（0.0001%）——`max(0, ·)` 这道保险几乎从不触发，
因为低频搭配在入表前就被滤掉了。⇒ 我们自训练时也要设同量级的下限，否则表会爆炸。

**d. ★★ `bgc` 是纯 2-gram（字对），表里没有 3 字键。**
对「的时候」「我们的」「中国人」等 8 个三字串从根做 `commonPrefixSearch`，
**全部只命中到 2 字**。⇒ 用 bgc 时，octagram 默认的 `collocation_max_length = 4`
（context 取 3 字 + word 取 3 字）**完全是空转**，实际只有「1 字 + 1 字」会命中。

**e. 类型覆盖率只有约 4%**：1,498,654 个有效字对，而常用汉字约 6000 个、
理论字对 3600 万。这是一张**高频搭配表**，不是完整语言模型。

**f. ★★★ 但命中率高达 88%——不要被 (e) 的 4% 误导。**

这是 P0 实测（`scripts/lm/bigram_coverage.py`，2026-08-11，词库 630,289 条）：

| 指标 | 字对数 | 命中率 |
|---|---|---|
| 词内字对（参考项，Viterbi 在词内不做选择） | 1,528,465 | **96.28%**（按词频加权 **99.41%**） |
| **★ 跨词边界字对**（Viterbi 转移真正发生的位置） | 62,806 | **88.21%** |

命中分值 `ln ∈ [6.91, 19.67]`，中位 10.89，**跨度 12.76 nat**。

⚠️ **「4% 覆盖率」与「88% 命中率」并不矛盾**，这是 Zipf 分布的必然：
模型覆盖的那 4% 恰恰是占据绝大多数出现次数的高频组合。
**类型（type）覆盖率与词例（token）命中率是两个量，不能相互推断**——
本文旧版就是从 4% 直接推出「收益上限低、扰动风险低」，**两个结论都错了**。

正确的结论是反过来的：

- **收益潜力大**：88% 的转移都能拿到上下文分；
- **★ 扰动也大**：分值跨度 12.76 nat，是每词固定罚 `WORD_PENALTY = 3.0` 的**四倍以上**。
  一旦接入，现有整句排序会被**大幅重排**，而不是"多数 miss、吃基线、排序不变"。
  ⇒ §7 P3 的重新标定不是可选项，是**必经之路**，且工作量要按"整套重标"预估。

**实测的局限**（结论要按这个折扣看）：语料用的是本仓 `docs/` 的中文正文，
是技术文档而非用户输入，分布有偏；分词用正向最大匹配近似，与 Viterbi 实际走的
路径不同；跨边界样本 62,806 个，量级够但不算大。
⇒ 这个数适合用来判断"**值不值得做**"（结论：值得），
不适合用来预测"能提升多少准确率"。

#### 2.2.4 编码：CJK 主区压成 2 字节

`gram_encoding.cc:8-43`。UTF-8 下一个汉字 3 字节，这里把 `U+4000..U+A000`
（覆盖 CJK 统一表意文字主区）压成 **2 字节**：高位字节 `(u >> 8) + 0x40`、低位 `u & 0xFF`。
ASCII 保持 1 字节，其余走 `0xE0 | 字节数` 头的变长格式。

⇒ 纯粹是为了缩小 trie 键长度、进而缩小 double-array 体积。**它不是通用编码，
不能拿来存任何需要往返转换的东西**（`u == 0` 被映射到 `0xE0`，`(u & 0xFF) == 0`
走 `0xE1` 转义，都是单向的规避冲突处理）。

#### 2.2.5 常数与「同基准」设计

`octagram.cc:12-19`：

```cpp
int    collocation_max_length   = 4;
int    collocation_min_length   = 3;
double collocation_penalty      = -12;
double non_collocation_penalty  = -12;
double weak_collocation_penalty = -24;
double rear_penalty             = -18;
```

★ **`collocation_penalty` 与 `non_collocation_penalty` 同为 −12**，这不是巧合：

- 未命中任何搭配 → 返回 `-12`（基线）；
- 命中 → `scale_value(match.value) + (-12)`，而 `scale_value` 恒 ≥ 0。

⇒ **搭配分是纯净加成，命中与否共用同一基准**。接入方不需要写「命中/未命中」两条分支，
这也是为什么 `Grammar::Evaluate` 能写成一行加法。

`weak_collocation_penalty = -24`：搭配总长（context_len + match_len）不足
`collocation_min_length`(3)、且不是整串匹配时，额外再罚 12。即**短搭配要额外强的证据**才算数。

`rear_penalty = -18`：句尾位置额外查一次 `Lookup(word, "$")`，用 `$` 作句末标记。

★ 顺带解出一个量纲问题：`-12` 在这里扮演的角色，与我们 `DICT_TOTAL` 的
`ln(242154693) ≈ 19.3` 是同一类**标定系数**——`ln(count) − 12 = ln(count / e¹²)`，
`e¹² ≈ 162754` 就是搭配表的等效归一化总量。两边都是「对数域里的一个减法常数」，
不是什么神秘超参。

### 2.3 ★★★ librime 的解码结构：beam 的状态键是「最后一个词」

`ref/weasel/librime/src/rime/gear/poet.cc`。**这一节直接决定了 §4.2 的成本，
是本次修订的核心。**

librime 用策略模板把两种解码统一在同一份 `MakeSentenceWithStrategy` 里：

```cpp
an<Sentence> Poet::MakeSentence(...) {
  return grammar_ ? MakeSentenceWithStrategy<BeamSearch>(...)
                  : MakeSentenceWithStrategy<DynamicProgramming>(...);
}
```

- `DynamicProgramming`：`State = Line`（每个位置一条最优线）——**这正是我们现在的 `dp[i]`**；
- `BeamSearch`：`State = hash_map<string, Line>`，注释写得很直白：

```cpp
// keep the best line candidate per last phrase
using LineCandidates = hash_map<string, Line>;

static Line& BestLineToUpdate(State& state, const Line& new_line) {
  const auto& key = new_line.last_word();   // ← 状态键 = 最后一个词
  return state[key];
}

static constexpr int kMaxLineCandidates = 7;  // ← 每个位置只向前扩展 7 条
```

⇒ **状态键是「最后一个词」，不是整条前缀路径**；再叠加 `find_top_candidates<7>`
的硬剪枝，**每个位置的活跃状态数被钳死在 7，与词表大小无关**。

而 context 只回看两个词（`poet.cc:52-58`）：

```cpp
string context() const {
  // look back 2 words
  return empty() ? string()
       : !predecessor || predecessor->empty() ? last_word()
       : predecessor->last_word() + last_word();
}
```

这两个词拼出的文本，再被 octagram 截成尾部 ≤3 个字。**所以「最后一个词」这个状态键
并不足以精确表达 context**（context 还含前一个词的尾巴）——librime 接受了这个近似，
用 beam 的多状态并存兜住误差。这是工程取舍，不是疏漏，我们照抄即可。

## 3. 许可格局

本仓是 **MIT**。

| 组件 | 许可 | 对我们 |
|---|---|---|
| librime（Grammar 接口 / poet beam 结构） | BSD-3 | ✅ 自由借鉴 |
| **librime-octagram（插件代码）** | **BSD-3** | ✅ **可自由移植/参考实现**（已 clone 至 `ref/librime-octagram`） |
| rime-octagram-data（模型数据） | **LGPL-3.0** | ⚠️ 可分发，但须附许可文本、保持可替换、来源可溯 |
| **amzxyz/RIME-LMDG（万象模型数据）** | **CC-BY-4.0** | ✅ **仅需署名，无 copyleft 传染，可直接进安装包** |
| libime | LGPL-2.1+ | ⚠️ 不宜链接 |
| KenLM | LGPL-2.1 | ⚠️ Rust 静态链接麻烦，不宜 |

⇒ **代码走 BSD-3 路线自己实现，不链接任何 LGPL 库**，是唯一无负担的选择。
数据侧新增了 CC-BY-4.0 的选项，见 §5。

## 4. 我们要改什么

### 4.1 打分接入：小

我们的 `pinyin/lattice.rs::score_node` **本来就是对数域**
（`ln(weight / DICT_TOTAL)` 加各类惩罚），与 librime 的 `entry_weight` 同构。
加一项上下文分即可，形态与 `Grammar::Evaluate` 一致。

⚠️ 一处必须同步重估：`WORD_PENALTY = 3.0`。它的现有取值论证（`lattice.rs:70-77`）
明确写着「librime 的 `kPenalty` 是**无语言模型时的兜底值**，有 grammar 时整个被替换掉」。
接上 grammar 后，我们这一侧的对应物也从「兜底罚」变成「非搭配基线 + 搭配加成」，
**3.0 这个数是对着旧语义标定的，不能原样留着**。

### 4.2 ★ Viterbi 状态扩展：**成本远低于本文旧版的估计**

旧版写的是「DP 状态数与词表大小相关，需要 beam search 剪枝，现有 `ViterbiDecoder`
结构要重做」。读完 `poet.cc` 后，这个判断要**下修**：

| 旧版估计 | 实读后 |
|---|---|
| 状态数与词表大小相关 | 状态键只是「最后一个词」，再取 top-7，**活跃状态恒 ≤ 7** |
| `ViterbiDecoder` 结构要重做 | 循环骨架可原样保留，见下 |
| beam search 要新写 | 就是 `find_top_candidates<7>` 那 15 行 |

**为什么循环骨架能保留**：librime 按 `start_pos` 遍历词图（`graph[start][end]`），
是因为 beam 要「从某状态向前扩展」；我们按 `nodes[end_pos]` 索引、`end_pos` 升序遍历。
两者的正确性前提是同一条——**扩展 `end_pos` 时 `dp[start]` 必须已定稿**，
而 `start < end_pos` 在升序遍历下天然成立。⇒ 现有 `for end_pos in 1..=input_len`
的结构不需要翻转，只需把内层的「单值比较」换成「对 `dp[start]` 的 top-7 逐个扩展」。

具体改动（`viterbi.rs`，全文仅 139 行）：

```
dp: Vec<DpEntry>                    →  dp: Vec<HashMap<String /*末词*/, DpEntry>>
DpEntry { log_prob, prev_pos, word, syl_mask }
                                    →  额外携带 prev_word（构造 context 用）
回溯键: pos                          →  回溯键: (pos, word)
```

★ **用 bgc 时状态键还能更小**：由 §2.2.3d，bgc 的 context 实际只有 **1 个汉字**。
⇒ 状态键不必是「最后一个词」，用「**最后一个汉字**」即可——汉字集比词表小两个数量级，
同一位置上不同末字的个数天然远少于不同末词。librime 之所以用末词，是因为它要同时
支持 bgw（context 是两个词拼出的文本再截尾）。

⚠️ 代价是**升级到 bgw 时状态键要跟着换**（末字 → 末词），不是换个数据文件就完事。

**★ 已决策（2026-08-11）：走「末词」**。即按 librime 的形态实现，只在 query 时
让具体 `Grammar` 去截取末字——用一次性的 beam 宽度成本，换掉日后升级 bgw 的返工。
P2 按此实现，`P1` 的 `context_at` 已经是「回看两个词」的语义，与该选择一致。

`ViterbiResult` 的三个字段（`words` / `log_prob` / `boundary`）语义不变，
`boundary` 的位移累加逻辑（`entry.syl_mask << entry.prev_pos`）照旧成立。

**调用点：4 处，`mod.rs:1069 / 1648 / 1760 / 1850`，结构完全一致**
（建 lattice → `decode(&lattice, len)` → 读 `words` / `log_prob`）。
只要 `decode` 签名保持 `(&nodes, input_len) -> ViterbiResult`、grammar 从
`ViterbiDecoder::new()` 注入，**这 4 处零改动**。需要 `preceding_text` 时才动它们（见 §4.4）。

⚠️ 仍然成立的警告：这一步**会改变整句路径的最优解**，前几轮所有关于整句 weight 的结论
（几何平均、`consumed_length` 兜底、6.5/6.5b 的让位、`AMBIGUOUS_PENALTY = 0.35`）
都建立在「无上下文」的前提上，**必须整套重跑 `pinyin_eval` 重新标定**。
成本重心从「写代码」转移到了「重新标定 + 回归」——这才是真正的工作量所在。

### 4.3 存储：中等，但有现成骨架

复用 `wind-dict` 的「mmap + 内容指纹缓存」骨架（`datformat.rs` / `commentdict.rs` /
`cache_fp.rs` / `reader_pool.rs` 已跑熟）。

**★ 结论：只做 bgc 的话，连 trie 都不需要。** 见 §2.2.3d——bgc 是纯 2-gram，
键恒为「1 个汉字 + 1 个汉字」。⇒ 键可以直接打成一个 `u64`（两个 char 的码点拼接），
值是 `i32`，150 万条 × 12 字节 ≈ **18 MB 的排序数组，一次二分即可查完**，
mmap 友好、零解析。octagram 那套 `traverse` + `commonPrefixSearch` 的两段式，
是为了支持变长 context/word（即 bgw）才存在的，**在 bgc 上是杀鸡用牛刀**。

若将来要上 bgw（词级、变长键），才需要真正的 trie。届时的好消息是——

**我们已经有一个成熟的 double-array trie，且它天然支持「带起始节点的搜索」。**
`datformat.rs` 的 wdat 格式就是 DAT（base/check 双数组 + CharMap 紧凑码 + MaxW 剪枝上界），
而 `search_prefix_inner`（`datformat.rs:1101`）的结构正是：

```rust
let Some(start) = self.walk(v, prefix) else { return ... };   // ← 先走到 context 节点
// 其后的分支限界搜索全部从 `start` 状态出发
```

⇒ 「先定位到 context 节点、再从该节点继续」这件事**结构上已经成立**，
把入口从「prefix 字符串」换成「起始 state」即可。

⚠️ 但要分清两种查询，它们**不是同一个东西**：

| | 语义 | 我们有吗 |
|---|---|---|
| `search_prefix`（现有） | 走到 prefix 节点后展开**整个子树**取 top-K | ✅ 有，但不是这里要的 |
| `commonPrefixSearch`（octagram 用） | 沿 word 这**一条路径**往下走，收集途中每个终止节点 | ❌ 需新写 |

前者「你」查出「你好/你们/你的…」，后者「你好」查出「你」和「你好」。
好在后者**更简单**：线性走一条路径、每步查 `terminal_leaf`，不需要堆、不需要剪枝、
不需要 arena，约 20 行，且 `base` / `check` / `walk` / `terminal_leaf` 全部可直接复用。

⇒ **P3 先按「bgc + 排序数组」做**，把 trie 那条路留给 bgw 阶段。

### 4.4 新发现的缺口：我们没有 `preceding_text`

`poet.h:43-57` 的 `ContextualWeighted` 与 `MakeSentence` 都吃一个 `preceding_text`
（光标前已上屏的文本），由 `translator->GetPrecedingText(start)` 提供。

**本仓不存在任何对应物**——grep `preceding_text` / `GetPrecedingText` / 「前文」
在 `wind_input/crates` 下零命中。后果：

- 句首的第一个词**永远没有 context**，只能吃 `non_collocation_penalty` 基线；
- `ContextualTranslation`（按上文给**普通候选**重排，不只整句）**完全做不了**。

拿到它要动 TSF 侧：从 `ITfContext` 取 `ITfRange` 反查光标前 N 个字符，走 EditSession。
⚠️ 这条路有已知陷阱——`TF_ES_SYNC` 只在按键上下文内合法（见
`project_tsf_edit_session_lock_mode` 的记录），且宿主兼容性参差。

⇒ **这是一条独立的工作线，不应与 bigram 解码绑定排期**。第一期可以先不做，
接受「句首无上下文」，反正整句内部的搭配才是收益大头。

## 5. 数据方案

| 方案 | 规模 | 许可 | 评价 |
|---|---|---|---|
| **A1. rime-octagram-data `zh-hans-t-essay-bgc`** | **10.4 MB** | LGPL-3.0 | 字级搭配，体积友好；须做成可选下载的独立数据文件，附许可文本 |
| A2. 同上 `zh-hans-t-essay-bgw` | 40.9 MB | LGPL-3.0 | 词级，质量更高、体积 ×4 |
| **B. amzxyz/RIME-LMDG `wanxiang-lts-zh-hans`** | **420 MB** | **CC-BY-4.0** | 许可最友好（仅署名），但体积是 A1 的 40 倍 |
| C. 自训练 | 可控 | 自有 | 语料许可要先确认（Wikipedia 是 CC-BY-SA）；训练管线是新工程 |

**建议**：**A1 先行**，理由是把接口做对、验证收益所需的最小代价——10.4 MB 可以走
「可选下载」而不进主安装包，LGPL-3.0 的义务（附许可文本、保持可替换、来源可溯）
在「独立数据文件」形态下是清晰可控的。

⚠️ B 的 420 MB 不是「分发大一点」这么简单：mmap 的实际驻留取决于访问局部性，
**必须实测 RSS 增量**才谈得上可行，否则会直接撞上我们前几轮刚做完的内存优化
（-53% heap / -44% RSS）。在实测数据出来之前，**不要因为许可友好就选 B**。

四个方案**接口完全一致**（都是 `.gram` / Darts trie），分离得干净的话可以共存——
换数据文件即可。这一点使得「A1 先行、后续再换」没有沉没成本。

## 6. ⚠️ 尚未查证

**已结清**（2026-08-11 第二轮）：

- ~~`.gram` 格式~~ → §2.2.2 / §2.2.3，darts-clone 4 字节 unit，已能端到端解析
- ~~模型规模~~ → §5
- ~~Viterbi 改动量~~ → §4.2
- ~~训练数据 `value` 的量纲~~ → **`ln(频次) × 10000`**，见 §2.2.3b
- ~~`wind-dict` 的 DAT 能否带起始节点搜索~~ → **能**，见 §4.3
- ~~bgc 在我们数据上的实际命中率~~ → **88.21%**，见 §2.2.3f（P0 已执行）

**仍待查**：

1. **实测**：加载后的 RSS 增量、单次 `Query` 延迟（注意它在每条边的每个词条上都要调，
   是解码的内循环）。bgc 若走 §4.3 的排序数组方案，这项风险很低，但仍需量。
2. `contextual_suggestions` 这个 schema 选项的确切语义——`poet.h:48` 显示它是
   `ContextualWeighted` 的开关，但它与 `preceding_text` 的耦合程度还没读透。
   ⚠️ 它依赖 §4.4 的 `preceding_text`，而那条我们没有，**优先级最低**。
3. **用真实输入语料复测命中率**。P0 用的是本仓技术文档，分布有偏（§2.2.3f 的局限）。
   若手上有用户实际输入的样本，值得复测一次——这会影响对收益幅度的预期，
   但**不影响「值得做」这个结论**。

**需要拍板的决策**（不是查证，是选择）：

- **状态键用「末字」还是「末词」**（§4.2）：前者简单、后者免返工。必须在 P2 前定。
- **数据用 A1 还是 B**（§5）：10.4 MB / LGPL-3.0 vs 420 MB / CC-BY-4.0。可留到 P3。

## 7. 建议的实施分期

前置查证已完成，可以排期了。**分四步，每步独立可验证、可回滚**：

**P0 — 先算命中率，再决定要不要做**（不写引擎代码）✅ **已完成，2026-08-11**
`scripts/lm/bigram_coverage.py`。结果见 §2.2.3f：**跨词边界命中率 88.21%**，
分值跨度 12.76 nat。**结论：值得做**，且必须按「大扰动」预估 P3 的标定成本。
重跑命令：

```bash
python scripts/lm/bigram_coverage.py --gram <zh-hans-t-essay-bgc.gram> \
    --dict-dir build_dev/data/schemas/pinyin/cn_dicts
```

（worktree 下 `build_dev/data` 通常没有 junction，需用 `--dict-dir` 指向主仓。）

**P1 — 打分骨架（不接真模型）** ✅ **已完成，2026-08-11**

`pinyin/grammar.rs` 定义 `Grammar` trait（`query(context, word, is_rear) -> f64`）
与恒返回 `0.0` 的 `NullGrammar`；`ViterbiDecoder` 改持 `Option<Arc<dyn Grammar>>`，
在转移处按 librime `Grammar::Evaluate` 的加法形态加一项上下文分。

**验收（两条都过）**：

1. `pinyin_eval` 剔除耗时字段后与基线 **JSON 逐位相同**——四类指标、misses 明细全等。
2. 新增单测 `null_grammar_does_not_change_result`（挂 `NullGrammar` 与不挂结果逐位相同）
   与 `grammar_receives_context_and_rear_flag`（context 回看两词、`is_rear` 只在末尾为真）。
   wind-engine 全量 0 failed。

★ 两条缺一不可：只有第 1 条的话，`query` 这条路径**从未被执行过**，
证明的只是「没接模型时没坏」，而不是「接上去的通路是通的」。

⚠️ **`NullGrammar` 必须返回 `0.0`，不能照抄 librime 的 `kPenalty`（−18.42）**。
理由见 `grammar.rs` 的长注释：librime 那个常数**就是**它的词数惩罚，
而我们这一侧 `WORD_PENALTY = 3.0` 早已在扮演该角色；返回任何非零常数都等于
偷偷改了 `WORD_PENALTY`，路径分会多出 `词数 × 常数`，指标必然漂移。

⚠️ 当前 `context_at` 是**单状态 DP 下的近似**：`dp[pos]` 只留一条最优路径，
拿到的是「当前最优路径的末两词」而非全部可能前驱。P2 换 beam 后才真正准确。

**P2 — beam 解码** ✅ **已完成，2026-08-11**

`dp: Vec<DpEntry>` → `Vec<Vec<DpEntry>>`（每位置按末词区分、分数降序、至多
`BEAM_WIDTH = 7` 条），回溯键从 `pos` 变成 `(prev_pos, prev_word)`。

**验收**：`pinyin_eval` 仍与基线**逐位相同**；单测扩到 7 条，新增三条覆盖多状态行为。

★ **回溯为什么安全**：键查找要求前驱状态还在。成立的理由是——按 `end_pos` 升序遍历时，
`dp[start_pos]` 在 `end_pos == start_pos` 那轮之后就**定稿**、之后只被读取；
而扩展只从保留下来的那几条线出发，所以被扩展出的线，其前驱必然还在保留集里。
（librime 用裸指针 `const Line* predecessor` 绕开了这个问题，我们用键就必须靠这条性质。）

★ **无模型时 beam 宽度退化为 1**，即回到单状态 DP——对齐 librime 的模板分派
（`MakeSentence` 无 grammar 走 `DynamicProgramming`、有才走 `BeamSearch`）。
不这么做的话，无模型时保留 7 条线纯属做功。

**性能**：见下面 §7.5「关闭路径零开销」——beam 初版确实很慢（每位置 7 条线 +
每条候选都先 clone 再丢弃），中间做过两轮优化，最终以**双路径分派**根治。

### ★★★ 7.5 关闭路径零开销（双路径分派）+ 一条测量方法论教训

**做法**：`decode()` 按有无 grammar 分派到 `decode_dp` / `decode_beam` 两套实现，
各用自己的状态类型（`DpEntry` 不含 `prev_word`）。对齐 librime `Poet::MakeSentence`
分派 `DynamicProgramming` / `BeamSearch`。**`decode_dp` 与接模型之前的实现逐字节同构**，
只多了 `decode()` 里一次 `match`。

两份实现的一致性由单测 `null_grammar_does_not_change_result` 守着——
它对比「不挂 grammar」与「挂恒 0 的 `NullGrammar`」，那恰好就是这两条路径。

**⚠️⚠️ 一条比性能数字本身更重要的教训：跨时段的基线不可比。**

改造中途曾得出「beam 让解码慢 1.25 倍」的结论并写进文档，**那个结论是错的**。
它拿的是几小时前测的基线（3280 ms），而同一份分支起点代码在当天晚些时候重测是
**4235 ms**——**环境漂移本身就有 1.29 倍**（这台机器同时在被人使用，CPU 占用不稳定）。

同环境重测后：

同一时段连测三份代码：

| 对象 | 合计耗时 | vs 同时段基线 |
|---|---|---|
| 分支起点代码（真基线） | 4235 ms | — |
| **双路径**（本节做法） | 4479 ms | 1.06x |
| 单一路径 + `width` 随模型切换（优化前） | 4538 ms | 1.07x |

其中单类波动可达 20%（同一份代码两次跑，C 类 1309 vs 1577 ms），且**若干类目下
被测版本反而比基线更快**。稍晚再测同一份双路径代码得 5355 ms（1.26x）——
机器负载一直在变（这台机同时在被人使用）。

⇒ **在这台机器上，小于 20% 的差异根本测不出来，再测多少次也一样。**

**⇒ 做法上的结论**：
1. **基线必须与被测对象在同一时段测**，否则测的是机器状态而不是代码；
2. 本机噪音水平约 **20%**，小于该幅度的差异一律不可判；
3. 结构性改动**优先用代码论证**——「这条路径与原实现同构吗？」比任何跑分都硬。
   跑分只用来否证「有没有**数量级**的退化」，比如 beam 初版那次 2.3x 就是真的。

**本节的论证就是这么立的**：把分支起点的 `decode` 与现在的 `decode_dp` 各自取出函数体、
去掉注释与空行后逐行比对，**53 行完全一致**。加上 `decode()` 里那一次 `match`，
「关闭时零开销」是结构事实，不依赖任何一次跑分。往后改动 `decode_dp` 时，
这个比对随时可以重做一遍。

**P3 — 接真模型** ✅ **已完成，2026-08-11**（但**结论是保守的**，见下）

`wind-dict/src/gramdb.rs`（mmap + darts-clone 只读查询 + `encode`）
与 `pinyin/octagram.rs`（`OctagramGrammar`）；配置 `[schema.pinyin.grammar]`
的 `weight` / `model`，`weight = 0`（默认）时**根本不读文件**。

**验收**：Rust 实现与 Python 独立实现在真实 `.gram` 上**逐位吻合**
（`tests/octagram_gram.rs`，见 §2.2.3 的对账值）；`weight=0` 时 `pinyin_eval`
与基线逐位相同。

### ★★★ P3 的真正结论：收益微弱且未经充分验证

**① `pinyin_eval` 测不出 bigram 的收益。** A/B/C 三类都是**单个词**的测试，
不存在跨词转移；实测 `weight=1.0` 时三类变化为 +0.10 / 0 / 0，纯噪音。
唯一涉及多词的 D 类是「随机拼两个词」造的，不是自然语言。
⇒ 新建了 `tests/grammar_sentence_eval.rs`（50 条真实整句）作为 bigram 的专用评测。

**② 标定扫描（50 条整句，基线 46/50）**：

| 模型 | w ≤ 0.2 | 0.3 | 0.5 | 0.7~1.0 | 1.5 |
|---|---|---|---|---|---|
| bgc（字级） | +1 | +0（1 弄坏） | +0 | −1 | **−4** |
| **bgw（词级）** | +1 | **+1（零弄坏）** | +0 | +1 | −1 |

⇒ 安全区间 `weight ≤ 0.3`，**bgw 明显比 bgc 稳健**。

**③ 但收益只有 +1/50，且只有一条样本被修好**（`xiawuliangdiankaishi`：
「下午亮点开始」→「下午两点开始」）。**统计上不显著，不能据此声称有效**。
仍未修好的典型：「生活中的小事」→ 小时、「这个事件很重要」→ 时间、
「他说得很对」→ 说的。

**④ 字级模型（bgc）有系统性缺陷**：它只看字对频次、不理解词边界，会把完整词组
打散成高频字对的拼接。实测把「明天再见」改成「明天在建」（因为「天+在」的字对
频次高于「天+再」）、把「气候特征有哪些」改成「前后他在有那些」。

⇒ **默认 `weight = 0`（关闭）**。要开启，建议 `model = zh-hans-bgw.gram`、
`weight = 0.2~0.3`。**在有更大规模的真实输入语料评测集之前，不建议默认打开。**

### 性能（数字均为**同环境**实测，见 §7.5 关于跨时段基线不可比的教训）

`weight = 0`（默认）时不读模型文件、走 `decode_dp`，**与接模型之前逐字节同构**，
开销测不出（§7.5）。

开启后（`zh-hans-bgw.gram`、`weight = 0.3`）：

| 类别 | 关闭 | 开启 | 倍数 |
|---|---|---|---|
| A 普通词 | 1506 ms | 3664 ms | 2.43x |
| B 缩合音短词 | 94 ms | 103 ms | 1.10x |
| C 多音节含缩合音 | 1247 ms | 3060 ms | 2.45x |
| D 简拼混合整句 | 1388 ms | 3939 ms | 2.84x |
| **合计** | **4235 ms** | **10766 ms** | **2.54x** |

引擎加载 38 → 48 ms（mmap 41 MB 模型，这一项很轻）。

⚠️ **而且 `weight = 0.3` 时 D 类 top-1 仍从 12.20% 掉到 10.10%（−2.10）**——
50 条整句测试集上测出的「w ≤ 0.3 零劣化」**不覆盖简拼混合长句这个场景**。
A/B/C 三类无变化（它们是单词测试，本就测不出 bigram）。

⇒ 开启的代价是「**解码慢 2.5 倍 + D 类 −2.1%**」，换来的是 50 条整句上 +1。
**默认开启前必须先做性能优化**，并拿更大的真实语料把收益测清楚。

（性能优化已于 2026-08-15 做过一轮，见下节。**当时对瓶颈的归因是错的**，
写在这里的「`context_of` 的 String 分配」并非主因。）

### ★ 查询侧缓存：ctx 半与 word 半的重复劳动（2026-08-15）

**先归因，再动手。** 四个变体同环境连续测，把 bigram 净开销拆开：

| 分量 | 占 bigram 净开销 |
|---|---|
| beam 结构 + `context_of` 的 String 分配 | 25% |
| ctx / word 的编码 | 28% |
| trie `traverse` + `commonPrefixSearch` | **47%** |

⚠️ **本文旧版把「`context_of` 的 String 分配」列为主因之一，是错的**——它连同
整个 beam 结构才占四分之一。一次堆分配是几十纳秒，而一次 `best_collocation`
要在 41 MB 的 mmap 上做约 30 次随机访问，差着两个数量级。照旧版的归因去优化
String，最多摸到四分之一的盘子。

**做法**：`OctagramGrammar` 内部加 thread-local 查询缓存。`best_collocation`
天然分成互不相干的两半，而 `decode_beam` 的循环是 `for node { for src } }`：

- **ctx 半**（取尾部 n 字 → 编码 → 逐级 traverse）只依赖 context，却在该起点下的
  每个 node 上重算一遍 ⇒ 8 槽环形缓存，实测**命中率 99.3%**；
- **word 半**（取首部 n 字 → 编码）只依赖 `node.word`，却在 beam 的每条线上重算
  一遍 ⇒ 单槽即可，内层循环里 word 恒定。

两者都是纯函数结果，缓存**不改变任何打分**。

★ **没有改 `Grammar` trait**：把 context 改成预编码句柄能再省下 `context_of`
那次分配（约 8%），但那会让 `grammar_receives_context_and_rear_flag` 失去可断言的
context 字符串。为 8% 折掉一层守门测试不划算。

**效果（确定性计数，15 条真实整句）**：

| 指标 | 优化前 | 优化后 | |
|---|---|---|---|
| `traverse` 调用 | 571,805 | 24,361 | −95.7% |
| `traverse` 字节 | 2,022,256 | 70,294 | −96.5% |
| `encode_chars` 调用 | 471,590 | 35,359 | −92.5% |
| `commonPrefixSearch` 调用 | 459,515 | 459,515 | 0 |
| **trie 随机访问字节合计** | 3,284,811 | 1,332,849 | **−59.4%** |

**验收**：`pinyin_eval` 三组（`w=0.3` 两轮、`w=0`）剔除耗时字段后 **JSON 逐位相同**。

### ★★★ 测量教训：wall-clock 在有负载的开发机上根本不可用

§7.5 记的是「跨时段基线不可比」。这次更进一步——**同一时段内交替测量也不可比**：

| | bigram 净开销 |
|---|---|
| A1（优化前） | 7337 ms |
| A2（优化前，**同一份代码**） | 3698 ms |

同代码两次测量差 **2 倍**。在这种数据上宣称任何加速比都是自欺欺人。

⇒ **判定「工作量是否减少」必须用确定性计数**（调用次数、访问字节数）：同样的输入
必然给出同样的数字，与机器忙闲无关。wall-clock 只能在安静环境下作参考。

还有一个陷阱：**别拿倍数（ON/OFF）当优化指标**。倍数的分母里装着与 bigram 无关的
基础解码时间，会把效果严重稀释——本次优化按倍数看只从 2.68x 降到 2.43x（像是失败），
按 bigram 净开销看是降低约 37%（与计数口径一致）。

### ★ 还能再优化多少：这条路基本到头了

`commonPrefixSearch` 一次都没减少，而它现在占 trie 访问字节的 **94.7%**。

这不是遗漏，是算法固有的：它的键是 `(某一级 ctx 落点, word)` 的组合——ctx 每缩短
一级就要拿 word 重查一次，而 word 每个 node 都不同，**没有任何两次调用的参数是重复的，
所以没有任何东西可缓存**。

要再往下压只能改算法本身（例如减少 ctx 级数、或对明显劣势的线剪枝跳过查询），
那会**改变打分结果**，属于另一个性质的决策，不在「纯性能优化」范围内。

### 标定路上踩的两个坑（都已写进 `octagram.rs` 的类型注释并有测试守门）

1. **照搬 octagram 的 `−12`**：它是「每转移固定罚」，而 librime 的 `entry_weight`
   是正值能抵消、我们的 `log_prob` 是负值抵消不了 ⇒ 按词数惩罚长句，
   D 类 top-1 从 12.20% 崩到 3.90%。
2. **改成「未命中 = 0（中性）」**：于是**无搭配记录的碎片（0）反而优于有记录的
   正确词组（负）** ⇒「建议修改」输给「见一修改」、「他的意思就是」输给「他的一死就是」。

两条约束（未命中不得优于命中 / 不得每转移重罚）并不真冲突，**冲突的是量级**——
保持符号语义、把 `weight` 压到 0.2~0.3 即可。

`preceding_text`（§4.4）不进这三期，作为独立工作线另排。

**★ 排期的真正风险不在编码，在 P3 的重新标定。** 前几轮标定出来的每一个常数
（`WORD_PENALTY` 3.0、`AMBIGUOUS_PENALTY` 0.35、`FUZZY_SYLLABLE_LOG_PENALTY` ln2、
`ABBREV_NODE_PENALTY` 1.2）都是在「无上下文」前提下用 `pinyin_eval` 定点标出来的。
上下文分一进来，这些常数的最优值会整体漂移，**且它们彼此耦合**——
不要指望逐个微调，要准备一轮系统性的重标定。
